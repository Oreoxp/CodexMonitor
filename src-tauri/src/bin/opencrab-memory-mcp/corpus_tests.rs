// V2-corpus end-to-end runner harness.
//
// Drives the REAL Rust pipeline over `tests/v2-corpus/cases/` and machine-checks
// each case's `ground_truth.yaml`. Three layers:
//
//   * Layer A — mechanical (deterministic, plain `#[test]`, always runs in CI).
//     Real ingest + real `parse_line` classification vs `expected_ingest`.
//     This is the port↔Rust reconciliation: the yaml's kept/dropped/boundary
//     line numbers were produced by the Python `parse_line` port; Layer A
//     confirms the real Rust `parse_line` agrees, line for line.
//   * Layer B — distillation (`#[ignore]`, hits the real configured LLM).
//   * Layer C — retrieval (`#[ignore]`, real embedder + hybrid search).
//
// S5 (consolidation) is parsed from ground_truth (`ConsolidationRelation`) but
// NOT verified here — that layer is added once S5 lands.
//
// Run:
//   cargo test --bin opencrab-memory-mcp corpus_layer_a
//   cargo test --bin opencrab-memory-mcp -- --ignored corpus_layer_b --nocapture
//   cargo test --bin opencrab-memory-mcp -- --ignored corpus_layer_c --nocapture

use super::*; // backend items (incl. crate-private: parse_line, search_with_timeout, …)
use serde::Deserialize;

// ---------------------------------------------------------------------------
// ground_truth.yaml schema mirror. Every field is `#[serde(default)]` so a case
// that omits a block (or carries extra keys like `description`/`tags`) parses
// fine.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)] // `title` / `consolidation_relations` parsed for completeness (S5 verifies later)
struct GroundTruth {
    #[serde(default)]
    case_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    team_id: String,
    #[serde(default)]
    project_hash: String,
    #[serde(default)]
    expected_ingest: Option<ExpectedIngest>,
    #[serde(default)]
    expected_distill: Option<ExpectedDistill>,
    #[serde(default)]
    expected_distill_segments: Option<Vec<ExpectedSegment>>,
    /// Case-level content assertion: each inner Vec is a `must_appear_any` set
    /// that must have ≥1 token somewhere across ALL the case's distilled rows
    /// (kind-agnostic, KP-agnostic) — for knowledge the LLM legitimately splits
    /// across multiple KPs or frames under a variable `kind`.
    #[serde(default)]
    expected_distill_case_level: Vec<Vec<String>>,
    /// Case-level forbidden tokens: none may appear in ANY distilled row's
    /// summary+detail (anti-hallucination; proves a relation member's wording is
    /// clean of the partner's distinctive term, e.g. C015/JWT must not say
    /// "session"). Checked in the positive branch alongside `case_level`.
    #[serde(default)]
    expected_distill_case_level_forbidden: Vec<String>,
    #[serde(default)]
    expected_search: Vec<ExpectedSearch>,
    #[serde(default)]
    consolidation_relations: Vec<ConsolidationRelation>,
    /// Scenario group: cases sharing a `group` are co-located in ONE DB for
    /// S5 / Layer C (S5 merges neighbors within a DB). Empty ⇒ ungrouped
    /// (Layer-B-only, no retrieval DB).
    #[serde(default)]
    group: String,
    /// Supersede/contradiction temporal annotation (hours; negative = earlier).
    /// The future S5 runner injects `log.ts` from this (§10.8 — supersede order
    /// comes from ts + content, never `parent_thread_id`).
    #[serde(default)]
    created_at_offset_hours: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct ExpectedIngest {
    #[serde(default)]
    raw_event_count: i64,
    #[serde(default)]
    kept_line_nos: Vec<i64>,
    #[serde(default)]
    dropped_line_nos: Vec<i64>,
    #[serde(default)]
    boundary_line_nos: Vec<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct ExpectedKp {
    /// Advisory only. The distiller runs at temperature 0.2, so the same
    /// durable lesson is validly framed as decision|failure|pattern|fact run
    /// to run. If `kind` is present and the content-matched row's kind differs,
    /// the harness logs a note but does NOT fail. Absent → never compared.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    summary_must_contain_any: Vec<String>,
    #[serde(default)]
    summary_must_not_contain: Vec<String>,
    /// AND of OR-groups: every inner Vec must have ≥1 substring present.
    #[serde(default)]
    detail_must_mention_all: Vec<Vec<String>>,
    #[serde(default)]
    detail_must_not_mention: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ExpectedDistill {
    #[serde(default)]
    expected_kp_count: i64,
    #[serde(default)]
    knowledge_points: Vec<ExpectedKp>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)] // `segment_index` / `prior_summary_note` are documentation fields
struct ExpectedSegment {
    #[serde(default)]
    segment_index: i64,
    #[serde(default)]
    expected_kp_count: i64,
    #[serde(default)]
    knowledge_points: Vec<ExpectedKp>,
    #[serde(default)]
    prior_summary_note: String,
}

#[derive(Debug, Default, Deserialize)]
struct ExpectedSearch {
    #[serde(default)]
    query: String,
    #[serde(default)]
    limit: u64,
    #[serde(default)]
    expected_top1_summary_must_match_any: Vec<String>,
    #[serde(default)]
    expected_top1_summary_must_not_match: Vec<String>,
    /// Membership assertion: each inner Vec is a `must_match_any` group, and
    /// EVERY group must be satisfied by some hit within the top-K (different
    /// hits allowed). For contradiction-pair queries where top1 is ambiguous.
    #[serde(default)]
    expected_topk_members: Vec<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)] // parsed but not verified until S5 lands
struct ConsolidationRelation {
    #[serde(default)]
    with_case: String,
    /// The S5 relation KIND — {duplicate|supersede|contradiction|complement|
    /// false_neighbor}. Distinct from `expected_action`: duplicate+supersede
    /// share `superseded_by`, complement+false_neighbor both "keep both", so
    /// the action alone can't encode S5's verdict (S5 audit records the kind).
    #[serde(default)]
    relation: String,
    #[serde(default)]
    expected_action: String,
    #[serde(default)]
    reason: String,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

struct Case {
    id: String,
    gt: GroundTruth,
    rollout: PathBuf,
}

fn corpus_cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/v2-corpus/cases")
}

fn load_cases() -> Vec<Case> {
    let dir = corpus_cases_dir();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read corpus dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();

    let mut cases = Vec::new();
    for case_dir in entries {
        let gt_path = case_dir.join("ground_truth.yaml");
        if !gt_path.exists() {
            continue; // not a case dir
        }
        let body = std::fs::read_to_string(&gt_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", gt_path.display()));
        let gt: GroundTruth = serde_yaml::from_str(&body)
            .unwrap_or_else(|e| panic!("parse yaml {}: {e}", gt_path.display()));

        let rollout = std::fs::read_dir(&case_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("no rollout-*.jsonl in {}", case_dir.display()));

        let id = if gt.case_id.is_empty() {
            case_dir.file_name().unwrap().to_string_lossy().to_string()
        } else {
            gt.case_id.clone()
        };
        cases.push(Case { id, gt, rollout });
    }
    cases
}

/// Copy a case's rollout into a scan tree the ingester can attribute:
/// `<scan_root>/<team_id>/<project_hash>/<original-filename>`. The
/// `<scan_root>` itself ends in `team_sessions` (set by the caller) so
/// `parse_rollout_attribution` finds the marker component.
fn stage_rollout(scan_root: &Path, c: &Case) {
    let dir = scan_root.join(&c.gt.team_id).join(&c.gt.project_hash);
    std::fs::create_dir_all(&dir).unwrap();
    let fname = c.rollout.file_name().unwrap();
    std::fs::copy(&c.rollout, dir.join(fname)).unwrap();
}

/// Fresh temp opencrab root + memory.db; stage + ingest ONE case. Per-case DB
/// keeps each case's `log` rows isolated (no thread_id column on `log`).
fn ingest_one_case(c: &Case) -> (tempfile::TempDir, PathBuf, Connection) {
    let tmp = tempfile::tempdir().unwrap();
    let scan_root = tmp
        .path()
        .join("agents")
        .join(&c.gt.agent_id)
        .join("team_sessions");
    stage_rollout(&scan_root, c);
    let db = tmp.path().join("memory.db");
    let conn = open(&db).unwrap();
    let stats = ingest_once(&conn, &scan_root, &c.gt.agent_id).unwrap();
    assert!(
        stats.events_inserted > 0 || c.gt.expected_ingest.as_ref().map(|i| i.raw_event_count).unwrap_or(0) == 0,
        "{}: ingest inserted 0 events — attribution likely failed (check team_sessions path / uuid)",
        c.id
    );
    (tmp, db, conn)
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

// ---------------------------------------------------------------------------
// Matching helpers (case-insensitive substring / set containment — never
// LLM-judged, per spec §3).
// ---------------------------------------------------------------------------

fn ci_contains(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}
fn any_ci(haystack: &str, needles: &[String]) -> bool {
    needles.iter().any(|n| ci_contains(haystack, n))
}
/// Every OR-group must have at least one member present (empty groups pass).
fn all_groups(detail: &str, groups: &[Vec<String>]) -> bool {
    groups
        .iter()
        .all(|g| g.is_empty() || g.iter().any(|n| ci_contains(detail, n)))
}
fn kind_eq(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn classify(payload: &str) -> &'static str {
    match parse_line(payload) {
        ParsedLine::Kept(_) => "kept",
        ParsedLine::Boundary { .. } => "boundary",
        ParsedLine::Dropped => "dropped",
    }
}

/// Locator token-groups for mapping a case to its distilled KP (Layer D).
/// PRIMARY: `expected_distill_case_level`. FALLBACK (when that's absent — e.g.
/// C001/C002 carry only per-KP assertions): the FIRST
/// `expected_distill.knowledge_points` entry's `summary_must_contain_any` (an
/// OR-group) plus its `detail_must_mention_all` (AND-of-OR-groups). Reads
/// ground_truth only — touches no corpus yaml. Lets the contradiction pairs that
/// lack `case_level` get located so gate ③ is testable.
fn locator_groups(gt: &GroundTruth) -> Vec<Vec<String>> {
    if !gt.expected_distill_case_level.is_empty() {
        return gt.expected_distill_case_level.clone();
    }
    if let Some(ed) = &gt.expected_distill {
        if let Some(kp) = ed.knowledge_points.first() {
            let mut groups: Vec<Vec<String>> = Vec::new();
            if !kp.summary_must_contain_any.is_empty() {
                groups.push(kp.summary_must_contain_any.clone());
            }
            for g in &kp.detail_must_mention_all {
                if !g.is_empty() {
                    groups.push(g.clone());
                }
            }
            return groups;
        }
    }
    Vec::new()
}

// Deterministic CI gate for the fallback locator (the end-to-end "really
// locates C001/C002's distilled KP" is still verified live by Layer D).
#[test]
fn locator_falls_back_to_knowledge_points() {
    // No case_level, but a knowledge_point with Postgres tokens (the C001 shape).
    let gt = GroundTruth {
        expected_distill: Some(ExpectedDistill {
            expected_kp_count: 1,
            knowledge_points: vec![ExpectedKp {
                summary_must_contain_any: vec!["Postgres".into(), "PG".into()],
                detail_must_mention_all: vec![vec!["事务".into(), "ACID".into()]],
                ..Default::default()
            }],
        }),
        ..Default::default()
    };
    let groups = locator_groups(&gt);
    assert!(!groups.is_empty(), "fallback must build groups from knowledge_points");
    assert!(
        all_groups("选择 PostgreSQL 作为会话存储，因 ACID 事务", &groups),
        "fallback locates the matching distilled row: {groups:?}"
    );
    assert!(!all_groups("用 Redis 做缓存", &groups), "non-matching row must NOT be located");
    // When case_level IS present it is used directly; the fallback isn't consulted.
    let gt2 = GroundTruth {
        expected_distill_case_level: vec![vec!["Kafka".to_string()]],
        ..Default::default()
    };
    assert_eq!(locator_groups(&gt2), vec![vec!["Kafka".to_string()]]);
}

// ===========================================================================
// LAYER A — mechanical, deterministic. HARD assert; collect all failures first.
// ===========================================================================

fn check_layer_a(c: &Case) -> Result<(), String> {
    let exp = c
        .gt
        .expected_ingest
        .as_ref()
        .ok_or_else(|| format!("{}: missing expected_ingest", c.id))?;

    let (_tmp, _db, conn) = ingest_one_case(c);

    // Real ingest's raw_event rows for this (single) thread, in line order.
    let mut stmt = conn
        .prepare("SELECT line_no, payload FROM raw_event ORDER BY line_no")
        .unwrap();
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    let mut errs: Vec<String> = Vec::new();

    // (1) raw_event_count.
    if rows.len() as i64 != exp.raw_event_count {
        errs.push(format!(
            "raw_event_count: ingester={} yaml={}",
            rows.len(),
            exp.raw_event_count
        ));
    }
    // (2) line_no set == 1..=count (contiguous, 1-based).
    let line_nos: Vec<i64> = rows.iter().map(|(n, _)| *n).collect();
    let expected_contig: Vec<i64> = (1..=rows.len() as i64).collect();
    if line_nos != expected_contig {
        errs.push(format!(
            "line_no set not contiguous 1..={}: got {:?}",
            rows.len(),
            line_nos
        ));
    }

    // (3) parse_line classification per line vs expected three sets — the
    // port↔Rust reconciliation. Report per-line disagreements explicitly.
    let mut expected_class: std::collections::HashMap<i64, &str> = std::collections::HashMap::new();
    for n in &exp.kept_line_nos {
        expected_class.insert(*n, "kept");
    }
    for n in &exp.dropped_line_nos {
        expected_class.insert(*n, "dropped");
    }
    for n in &exp.boundary_line_nos {
        expected_class.insert(*n, "boundary");
    }
    for (n, payload) in &rows {
        let rust = classify(payload);
        match expected_class.get(n) {
            Some(yaml) if *yaml == rust => {}
            Some(yaml) => errs.push(format!(
                "L{n}: parse_line(Rust)={rust} but yaml={yaml}  ::  {}",
                payload.chars().take(90).collect::<String>()
            )),
            None => errs.push(format!("L{n}: Rust={rust} but yaml lists no class for this line")),
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(format!("[{}] {}", c.id, errs.join("\n         ")))
    }
}

#[test]
fn corpus_layer_a_mechanical() {
    let cases = load_cases();
    assert!(
        !cases.is_empty(),
        "no corpus cases under {}",
        corpus_cases_dir().display()
    );
    let mut failures: Vec<String> = Vec::new();
    for c in &cases {
        if let Err(e) = check_layer_a(c) {
            failures.push(e);
        }
    }
    if !failures.is_empty() {
        panic!(
            "Layer A: {}/{} cases FAILED (port↔Rust / ingest mismatch):\n  {}",
            failures.len(),
            cases.len(),
            failures.join("\n  ")
        );
    }
    eprintln!("[corpus A] {} cases PASS (ingest + parse_line == expected_ingest)", cases.len());
}

// ===========================================================================
// LAYER B — distillation, live. #[ignore]. Collect all mismatches + dump the
// real LLM output before asserting.
// ===========================================================================

fn distilled_rows(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    let mut stmt = conn
        .prepare("SELECT kind, summary, detail FROM log WHERE origin = 'distill' ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn dump_kps(rows: &[(String, String, Option<String>)]) -> String {
    let mut s = String::new();
    for (i, (k, sum, det)) in rows.iter().enumerate() {
        s.push_str(&format!(
            "\n    [{i}] kind={k} summary={sum}\n        detail={}",
            det.as_deref().unwrap_or("<none>")
        ));
    }
    if s.is_empty() {
        s.push_str(" <none>");
    }
    s
}

fn check_layer_b(c: &Case, extractor: &HttpExtractor) -> Result<String, String> {
    let (_tmp, _db, conn) = ingest_one_case(c);

    // Force the idle gate open so this thread always distills this pass.
    let now_ms = current_time_ms() + DISTILL_IDLE_MS + 60_000;
    block_on(distill_once(&conn, extractor, now_ms)).map_err(|e| format!("[{}] distill_once: {e}", c.id))?;

    let rows = distilled_rows(&conn);

    // Flatten expected KPs (single-segment or multi-segment).
    let (expected_total, expected_kps): (i64, Vec<&ExpectedKp>) =
        if let Some(segs) = &c.gt.expected_distill_segments {
            (
                segs.iter().map(|s| s.expected_kp_count).sum(),
                segs.iter().flat_map(|s| s.knowledge_points.iter()).collect(),
            )
        } else if let Some(d) = &c.gt.expected_distill {
            (d.expected_kp_count, d.knowledge_points.iter().collect())
        } else {
            (0, Vec::new())
        };

    let mut errs: Vec<String> = Vec::new();

    if expected_total == 0 {
        // Reverse case: exact-zero IS the point (No-Echo / Skip-trivial). A
        // single leaked KP is a failure.
        if !rows.is_empty() {
            errs.push(format!(
                "expected [] (no durable KP) but got {} row(s)",
                rows.len()
            ));
        }
    } else {
        // Positive case: every expected KP must be PRESENT (kind +
        // summary_must_contain_any + respects summary_must_not_contain +
        // detail OR-groups). We do NOT assert an exact row count — the prompt
        // allows 0–5 KPs, so additional valid KPs are fine. The success report
        // still prints the actual count so over-production stays visible.
        for kp in &expected_kps {
            // Content match is the HARD gate; kind is advisory (see ExpectedKp).
            let hit = rows.iter().find(|(_k, sum, det)| {
                (kp.summary_must_contain_any.is_empty()
                    || any_ci(sum, &kp.summary_must_contain_any))
                    && !any_ci(sum, &kp.summary_must_not_contain)
                    && all_groups(det.as_deref().unwrap_or(""), &kp.detail_must_mention_all)
            });
            match hit {
                None => errs.push(format!(
                    "no row matches expected KP content: summary_any={:?} detail_all={:?} (expected kind={:?})",
                    kp.summary_must_contain_any, kp.detail_must_mention_all, kp.kind
                )),
                Some((actual_kind, _, _)) => {
                    if let Some(exp) = kp.kind.as_deref() {
                        if !kind_eq(actual_kind, exp) {
                            eprintln!(
                                "[corpus B] {} KIND ADVISORY: content matched but kind differs — expected {exp:?}, got {actual_kind:?} (summary_any={:?})",
                                c.id, kp.summary_must_contain_any
                            );
                        }
                    }
                }
            }
        }

        // "禁词不犯": no forbidden token anywhere (per-case DB → "no row
        // contains X" is exactly the leak check; catches a hallucinated
        // alternative or a mined prior_summary decoy like "BlueFin" in C012).
        let forbid_summary: Vec<&String> =
            expected_kps.iter().flat_map(|k| k.summary_must_not_contain.iter()).collect();
        let forbid_detail: Vec<&String> =
            expected_kps.iter().flat_map(|k| k.detail_must_not_mention.iter()).collect();
        for (_k, sum, det) in &rows {
            for f in &forbid_summary {
                if ci_contains(sum, f) {
                    errs.push(format!("forbidden token {f:?} appeared in a summary: {sum}"));
                }
            }
            let dd = det.as_deref().unwrap_or("");
            for f in &forbid_detail {
                if ci_contains(dd, f) {
                    errs.push(format!("forbidden token {f:?} appeared in a detail"));
                }
            }
        }

        // Case-level content assertions (kind-agnostic, KP-agnostic): each set
        // must have a token somewhere across ALL distilled rows concatenated.
        // Used where the LLM splits one lesson across KPs (C003) or kinds it
        // variably (C006).
        if !c.gt.expected_distill_case_level.is_empty()
            || !c.gt.expected_distill_case_level_forbidden.is_empty()
        {
            let haystack = rows
                .iter()
                .map(|(k, s, d)| format!("{k}\n{s}\n{}", d.as_deref().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("\n");
            for set in &c.gt.expected_distill_case_level {
                if !any_ci(&haystack, set) {
                    errs.push(format!("case-level: no distilled row text matches any of {set:?}"));
                }
            }
            // Case-level forbidden: anti-hallucination / clean-separation guard.
            for f in &c.gt.expected_distill_case_level_forbidden {
                if ci_contains(&haystack, f) {
                    errs.push(format!("case-level forbidden token {f:?} appeared in a distilled row"));
                }
            }
        }
    }

    if errs.is_empty() {
        Ok(format!("{} distilled row(s) match", rows.len()))
    } else {
        Err(format!(
            "[{}] {}\n  ── actual LLM KPs ({} rows):{}",
            c.id,
            errs.join("\n         "),
            rows.len(),
            dump_kps(&rows)
        ))
    }
}

#[test]
#[ignore]
fn corpus_layer_b_distill() {
    let Some(extractor) = HttpExtractor::load() else {
        eprintln!("[corpus B] no distiller config (OPENCRAB_DISTILLER_* or ~/.opencrab/distiller.json) — SKIPPING");
        return;
    };
    let cases = load_cases();
    let mut failures: Vec<String> = Vec::new();
    for c in &cases {
        match check_layer_b(c, &extractor) {
            Ok(report) => eprintln!("[corpus B] {} PASS — {report}", c.id),
            Err(e) => {
                eprintln!("[corpus B] {} FAIL — {e}", c.id);
                failures.push(e);
            }
        }
    }
    if !failures.is_empty() {
        panic!("Layer B: {}/{} cases FAILED (see dumps above)", failures.len(), cases.len());
    }
    eprintln!("[corpus B] all {} cases PASS", cases.len());
}

// ===========================================================================
// LAYER C — retrieval, live. #[ignore]. SCENARIO-SCOPED: ONE DB per `group`
// (S5 merges neighbors within a DB; a 40-case mega-DB makes top-K noisy and S5
// should be tested on controlled neighborhoods). Per group: ingest → distill →
// embed → run that group's expected_search at the production 8s budget (with
// query-embed wall time + FTS-only contribution), and measure the bge-m3
// distance between annotated relation pairs (the distribution S5-design uses to
// pick the candidate threshold T). Ungrouped cases are Layer-B-only.
// ===========================================================================

/// First distilled log row whose summary+detail contains any of `tokens`.
fn find_distilled_id(conn: &Connection, tokens: &[String]) -> Option<i64> {
    let mut stmt = conn
        .prepare("SELECT id, summary, detail FROM log WHERE origin='distill' ORDER BY id")
        .ok()?;
    let rows: Vec<(i64, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    rows.into_iter()
        .find(|(_, s, d)| any_ci(&format!("{s}\n{}", d.as_deref().unwrap_or("")), tokens))
        .map(|(id, _, _)| id)
}

fn summary_of(conn: &Connection, id: i64) -> String {
    conn.query_row("SELECT summary FROM log WHERE id=?1", params![id], |r| r.get(0))
        .unwrap_or_default()
}

/// bge-m3 distance between two distilled KPs (A re-embedded as the query, KNN
/// against B's stored log_vec vector) — what S5 candidate selection would see.
fn kp_distance(conn: &Connection, emb: &dyn Embedder, id_a: i64, id_b: i64) -> Option<f64> {
    let (sa, da): (String, Option<String>) = conn
        .query_row("SELECT summary, detail FROM log WHERE id=?1", params![id_a], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok()?;
    let text_a = format!("{}\n\n{}", sa, da.unwrap_or_default());
    let qv = block_on(emb.embed(&[text_a])).ok()?;
    let qjson = vec_to_match_json(qv.first()?);
    let mut stmt = conn
        .prepare("SELECT rowid, distance FROM log_vec WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance")
        .ok()?;
    let rows: Vec<(i64, f64)> = stmt
        .query_map(params![qjson, 64_i64], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    rows.iter().find(|(rid, _)| *rid == id_b).map(|(_, d)| *d)
}

#[test]
#[ignore]
fn corpus_layer_c_search() {
    let Some(extractor) = HttpExtractor::load() else {
        eprintln!("[corpus C] no distiller config — SKIPPING");
        return;
    };
    let Some(embedder) = HttpEmbedder::load() else {
        eprintln!("[corpus C] no embedder config — SKIPPING");
        return;
    };
    let emb: &dyn Embedder = &embedder;
    let cases = load_cases();
    let by_id: std::collections::HashMap<String, &Case> =
        cases.iter().map(|c| (c.id.clone(), c)).collect();
    let mut groups: std::collections::BTreeMap<String, Vec<&Case>> = std::collections::BTreeMap::new();
    for c in &cases {
        if !c.gt.group.is_empty() {
            groups.entry(c.gt.group.clone()).or_default().push(c);
        }
    }

    let mut failures: Vec<String> = Vec::new();
    let mut search_count = 0u32;
    let mut dist_rows: Vec<(String, f64)> = Vec::new(); // (relation, distance)

    for (gname, gcases) in &groups {
        let agent = gcases[0].gt.agent_id.clone();
        let tmp = tempfile::tempdir().unwrap();
        let scan_root = tmp.path().join("agents").join(&agent).join("team_sessions");
        for c in gcases.iter() {
            stage_rollout(&scan_root, c);
        }
        let db = tmp.path().join("memory.db");
        let conn = open(&db).unwrap();
        let ing = ingest_once(&conn, &scan_root, &agent).unwrap();
        let now_ms = current_time_ms() + DISTILL_IDLE_MS + 60_000;
        let dstats = block_on(distill_once(&conn, &extractor, now_ms)).expect("distill_once");
        let estats = block_on(embed_pending_once(&conn, &embedder)).expect("embed_pending_once");
        eprintln!(
            "[corpus C] === group '{gname}' ({} cases) ingest={} distilled={} embedded={} ===",
            gcases.len(), ing.events_inserted, dstats.points_written, estats.rows_embedded
        );

        // ---- (a) expected_search within this group's DB ----
        for c in gcases.iter() {
            for es in &c.gt.expected_search {
                search_count += 1;
                let limit = if es.limit == 0 { 3 } else { es.limit as usize };
                let t0 = std::time::Instant::now();
                let qemb = block_on(emb.embed(&[es.query.clone()]));
                let embed_secs = t0.elapsed().as_secs_f64();
                let qdim = qemb.as_ref().ok().and_then(|v| v.first()).map(|v| v.len()).unwrap_or(0);
                eprintln!("[corpus C] {} query-embed wall={:.2}s dim={} query={:?}", c.id, embed_secs, qdim, es.query);
                match build_fts_match(&es.query) {
                    Some(expr) => {
                        let fts = fts_ranked_ids(&conn, &expr, limit.max(10)).unwrap_or_default();
                        eprintln!("[corpus C] {} FTS-only(trigram) expr={expr} -> {} ids={fts:?}", c.id, fts.len());
                    }
                    None => eprintln!("[corpus C] {} FTS-only(trigram): None", c.id),
                }
                let hits = block_on(search(&db, Some(emb), &es.query, limit)).expect("search");
                let mut topk = String::new();
                for (i, h) in hits.iter().enumerate() {
                    topk.push_str(&format!("\n      #{i} id={} {}", h.id, h.summary));
                }
                eprintln!("[corpus C] {} query={:?} -> {} hits:{}", c.id, es.query, hits.len(), if topk.is_empty() { " <none>".into() } else { topk });
                let mut why: Vec<String> = Vec::new();
                match hits.first() {
                    None => why.push("no hits".into()),
                    Some(t1) => {
                        if !es.expected_top1_summary_must_match_any.is_empty()
                            && !any_ci(&t1.summary, &es.expected_top1_summary_must_match_any) {
                            why.push(format!("top1 {:?} matches none of {:?}", t1.summary, es.expected_top1_summary_must_match_any));
                        }
                        if !es.expected_top1_summary_must_not_match.is_empty()
                            && any_ci(&t1.summary, &es.expected_top1_summary_must_not_match) {
                            why.push(format!("top1 {:?} hit must_not {:?}", t1.summary, es.expected_top1_summary_must_not_match));
                        }
                    }
                }
                for ms in &es.expected_topk_members {
                    if !hits.iter().any(|h| any_ci(&h.summary, ms)) {
                        why.push(format!("top-{limit} missing member-set {ms:?}"));
                    }
                }
                if !why.is_empty() {
                    failures.push(format!("[{}] query={:?}: {}", c.id, es.query, why.join("; ")));
                }
            }
        }

        // ---- (b) relation-pair bge-m3 distances (distribution for S5 threshold T) ----
        let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        for c in gcases.iter() {
            let a_tokens = c.gt.expected_distill_case_level.first().cloned().unwrap_or_default();
            for r in &c.gt.consolidation_relations {
                let key = if c.id < r.with_case {
                    (c.id.clone(), r.with_case.clone())
                } else {
                    (r.with_case.clone(), c.id.clone())
                };
                if !seen.insert(key) {
                    continue;
                }
                let Some(b) = by_id.get(&r.with_case) else { continue };
                let b_tokens = b.gt.expected_distill_case_level.first().cloned().unwrap_or_default();
                if a_tokens.is_empty() || b_tokens.is_empty() {
                    continue;
                }
                match (find_distilled_id(&conn, &a_tokens), find_distilled_id(&conn, &b_tokens)) {
                    (Some(ida), Some(idb)) => {
                        if let Some(d) = kp_distance(&conn, emb, ida, idb) {
                            eprintln!(
                                "[dist] {gname} {}<->{} relation={} L2={:.4}\n         {}: {:?}\n         {}: {:?}",
                                c.id, r.with_case, r.relation, d,
                                c.id, summary_of(&conn, ida), r.with_case, summary_of(&conn, idb)
                            );
                            dist_rows.push((r.relation.clone(), d));
                        }
                    }
                    _ => eprintln!("[dist] {gname} {}<->{} relation={}: KP(s) not located, skip", c.id, r.with_case, r.relation),
                }
            }
        }
    }

    eprintln!("[corpus C] ==== bge-m3 KP-distance distribution (S5 threshold T input) ====");
    let mut by_rel: std::collections::BTreeMap<String, Vec<f64>> = std::collections::BTreeMap::new();
    for (rel, d) in &dist_rows {
        by_rel.entry(rel.clone()).or_default().push(*d);
    }
    for (rel, ds) in &by_rel {
        let min = ds.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = ds.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let avg = ds.iter().sum::<f64>() / ds.len() as f64;
        eprintln!("[corpus C]   relation={rel:14} n={} min={min:.4} avg={avg:.4} max={max:.4}", ds.len());
    }

    if !failures.is_empty() {
        panic!("Layer C: {}/{} searches FAILED:\n  {}", failures.len(), search_count, failures.join("\n  "));
    }
    eprintln!("[corpus C] all {search_count} search assertions PASS across {} groups", groups.len());
}

// ===========================================================================
// LAYER D — consolidation (v8 / S5), live. #[ignore]. The eval for the judge.
//
// Loads EVERY case carrying `consolidation_relations` into ONE DENSE DB (no
// grouping — consolidation must be stress-tested on dense neighbours; this is
// the density Layer C's per-group split deliberately does NOT exercise) →
// distill → embed → inject `log.ts` from `created_at_offset_hours` (so the
// supersede direction is judgeable) → `find_candidates(T)` (report candidates
// vs the annotated expected pairs) → `consolidate_once` (dry-run) and machine-check
// the audit:
//
//   HARD gates (precision — the ones to watch):
//     1. NO contradiction / false_neighbor pair is judged duplicate|supersede
//        (never soft-merge a contradiction or a false neighbour — the
//        "don't soft-delete useful memory" lifeline).
//     2. a clear duplicate pair is judged duplicate|supersede (must merge).
//     3. a contradiction pair is judged contradiction.
//     4. the recency trap C020<->C023 is NOT judged supersede.
//   ADVISORY (logged, never fail): duplicate-vs-supersede label;
//     complement-vs-no_action label.
//   WATCHED (logged): a supersede pair judged supersede (superseded_id appears
//     in the dry-run log); a supersede mislabelled duplicate (still merges,
//     loses direction) is flagged — a prompt-fix signal, not a failure.
//
// temp-0.2 jitters: the hard gates should be stable (clear cases), advisory
// labels will wobble. 1+/4 hard-gate breakage = a real problem → tune the
// prompt. The dry-run MUTATES no live state, and the test asserts that.
//
// Mapping case-id <-> log.id in the dense DB is greedy-most-specific-first:
// a single token ("Kafka") collides across 5 cases, so each case claims the
// lowest-id distilled row matching ALL of its `expected_distill_case_level`
// groups, processing cases with more groups first so a broad locator can't
// steal a narrower case's row. Unmappable cases are logged and their gates
// skipped (best-effort — the audit dump is still the human inspection surface).
// ===========================================================================

fn canon_ids(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

#[test]
#[ignore]
fn corpus_layer_d_consolidate() {
    let Some(extractor) = HttpExtractor::load() else {
        eprintln!("[corpus D] no distiller config (OPENCRAB_DISTILLER_* / distiller.json) — SKIPPING");
        return;
    };
    let Some(embedder) = HttpEmbedder::load() else {
        eprintln!("[corpus D] no embedder config — SKIPPING");
        return;
    };
    let cases = load_cases();
    let rel_cases: Vec<&Case> = cases
        .iter()
        .filter(|c| !c.gt.consolidation_relations.is_empty())
        .collect();
    assert!(!rel_cases.is_empty(), "no cases carry consolidation_relations");

    // ---- one dense DB: ingest ALL relation cases, distill, embed ----
    let agent = rel_cases[0].gt.agent_id.clone();
    let tmp = tempfile::tempdir().unwrap();
    let scan_root = tmp.path().join("agents").join(&agent).join("team_sessions");
    for c in &rel_cases {
        stage_rollout(&scan_root, c);
    }
    let db = tmp.path().join("memory.db");
    let conn = open(&db).unwrap();
    let ing = ingest_once(&conn, &scan_root, &agent).unwrap();
    let now_ms = current_time_ms() + DISTILL_IDLE_MS + 60_000;
    let dstats = block_on(distill_once(&conn, &extractor, now_ms)).expect("distill_once");
    let estats = block_on(embed_pending_once(&conn, &embedder)).expect("embed_pending_once");
    eprintln!(
        "[corpus D] dense DB: {} relation-cases ingest={} distilled={} embedded={}",
        rel_cases.len(),
        ing.events_inserted,
        dstats.points_written,
        estats.rows_embedded
    );

    // Diagnostic: every distilled KP verbatim — so unmapped cases (C001/C002
    // have no locator tokens) can still be inspected for distill drift.
    {
        let mut stmt = conn
            .prepare("SELECT id, summary, detail FROM log WHERE origin='distill' ORDER BY id")
            .unwrap();
        let kps: Vec<(i64, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("[corpus D] --- {} distilled KPs (verbatim) ---", kps.len());
        for (id, s, d) in &kps {
            eprintln!(
                "[corpus D]   KP id={id}\n              summary={s:?}\n              detail={:?}",
                d.as_deref().unwrap_or("")
            );
        }
    }

    // ---- greedy case-id <-> log.id mapping (most groups first, claim rows) ----
    let all_rows: Vec<(i64, String, Option<String>)> = {
        let mut stmt = conn
            .prepare("SELECT id, summary, detail FROM log WHERE origin='distill' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let mut id_of: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut case_of: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let mut claimed: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut order: Vec<&Case> = rel_cases.clone();
    order.sort_by_key(|c| std::cmp::Reverse(locator_groups(&c.gt).len()));
    for c in &order {
        let groups = locator_groups(&c.gt);
        if groups.is_empty() {
            eprintln!("[corpus D] map: {} has no locator tokens — unmapped", c.id);
            continue;
        }
        let hit = all_rows.iter().find(|(id, s, d)| {
            !claimed.contains(id)
                && all_groups(&format!("{s}\n{}", d.as_deref().unwrap_or("")), &groups)
        });
        match hit {
            Some((id, _, _)) => {
                claimed.insert(*id);
                id_of.insert(c.id.clone(), *id);
                case_of.insert(*id, c.id.clone());
            }
            None => eprintln!("[corpus D] map: {} matched no distilled row — unmapped", c.id),
        }
    }

    // ---- inject ts from created_at_offset_hours (supersede direction) ----
    let base = current_time_ms();
    for c in &rel_cases {
        if let (Some(off), Some(&id)) = (c.gt.created_at_offset_hours, id_of.get(&c.id)) {
            let ts = base + off * 3_600_000;
            conn.execute("UPDATE log SET ts = ?1 WHERE id = ?2", params![ts, id])
                .unwrap();
        }
    }

    // ---- expected pair map (canonical case-ids) from annotations ----
    let mut expected: std::collections::BTreeMap<(String, String), String> =
        std::collections::BTreeMap::new();
    for c in &rel_cases {
        for r in &c.gt.consolidation_relations {
            expected.insert(canon_ids(&c.id, &r.with_case), r.relation.clone());
        }
    }

    // ---- candidates vs expected ----
    let t = consolidation_distance_t();
    let candidates = find_candidates(&conn, t).expect("find_candidates");
    eprintln!("[corpus D] T={t:.4}; {} candidate pair(s):", candidates.len());
    let label = |id: &i64| case_of.get(id).cloned().unwrap_or_else(|| format!("id{id}"));
    let mut cand_pairs: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for (a, b, d) in &candidates {
        let (ca, cb) = (label(a), label(b));
        let key = canon_ids(&ca, &cb);
        let exp = expected
            .get(&key)
            .cloned()
            .unwrap_or_else(|| "—(unannotated near pair)".into());
        eprintln!("[corpus D]   cand {ca}<->{cb} L2={d:.4} expected={exp}");
        cand_pairs.insert(key);
    }
    // annotated pairs that did NOT surface as candidates (T-robustness / floor check)
    for ((ca, cb), rel) in &expected {
        if !cand_pairs.contains(&(ca.clone(), cb.clone())) {
            let located = id_of.contains_key(ca) && id_of.contains_key(cb);
            eprintln!(
                "[corpus D]   MISS expected {ca}<->{cb} ({rel}) — {}",
                if located {
                    "distance >= T (jitter out / floor check)"
                } else {
                    "KP not located in DB"
                }
            );
        }
    }

    // ---- dry-run → audit → gate ----
    let n = block_on(consolidate_once(
        &conn,
        &extractor,
        t,
        std::time::Duration::from_secs(120),
        true, // dry-run: this Layer-D gate previews verdicts, never mutates.
    ))
    .expect("consolidate_once");
    eprintln!("[corpus D] dry-run judged {n} pair(s); audit:");
    let audit: Vec<(i64, i64, f64, String, String, Option<i64>, String, i64, i64)> = {
        let mut stmt = conn
            .prepare("SELECT kp_a, kp_b, distance, relation, action, superseded_id, rationale, dry_run, applied FROM log_consolidation_audit ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                r.get(7)?,
                r.get(8)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    };

    let mut failures: Vec<String> = Vec::new();

    // zero-mutation invariant: dry-run touches no live state.
    let superseded_cnt: i64 = conn
        .query_row("SELECT count(*) FROM log WHERE superseded_by IS NOT NULL", [], |r| r.get(0))
        .unwrap();
    let contra_cnt: i64 = conn
        .query_row("SELECT count(*) FROM log_contradiction", [], |r| r.get(0))
        .unwrap();
    if superseded_cnt != 0 {
        failures.push(format!("dry-run set superseded_by on {superseded_cnt} row(s) — must be 0"));
    }
    if contra_cnt != 0 {
        failures.push(format!("dry-run wrote {contra_cnt} log_contradiction row(s) — must be 0"));
    }

    // Few-shot anchors the judge SAW. Hard gates judge ONLY held-out pairs; a
    // seen pair is logged as sanity (it was taught → not an independent test).
    // C027↔C028 is intentionally NOT here: its few-shot slot was swapped for a
    // synthetic Nginx near-FN, so the hardest near-FN (0.79) stays held-out for
    // gate ①.
    let seen_pairs: std::collections::HashSet<(String, String)> = [
        canon_ids("C031", "C032"), // duplicate
        canon_ids("C019", "C020"), // supersede
        canon_ids("C017", "C018"), // contradiction
        canon_ids("C035", "C036"), // complement
        canon_ids("C020", "C023"), // recency-trap FN
    ]
    .into_iter()
    .collect();
    // Held-out replacement-target-mismatch traps (gate ④ emphasis): a newer KP,
    // some even carrying replacement language, but the replacement / topic
    // points at a THIRD thing, not the partner → must NOT be supersede.
    let target_traps: std::collections::HashSet<(String, String)> = [
        canon_ids("C019", "C024"),
        canon_ids("C020", "C024"),
        canon_ids("C019", "C023"),
    ]
    .into_iter()
    .collect();

    // ---- audit dump to a file (§12 dry-run inspection surface) ----
    // Unconditional, post-INSERT: the canonical backend dumper (kp-id columns).
    // Case-id labels for human reading are in the gate-loop stdout below.
    let dump_path = std::env::var("OPENCRAB_LAYERD_AUDIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/layer-d-audit.tsv"));
    match dump_audit_tsv(&conn, &dump_path) {
        Ok(n) => eprintln!("[corpus D] audit dumped ({n} rows) → {}", dump_path.display()),
        Err(e) => eprintln!("[corpus D] audit dump failed ({e}) — table still in DB"),
    }

    // ---- gate the audit (HELD-OUT = hard; seen = sanity) ----
    let merges = |rel: &str| matches!(rel, "duplicate" | "supersede");
    // Revision 1: a held-out GATE pair whose verdict came back relation="error"
    // is NOT a pass — it's missing data ("38/38 but C027↔C028 errored" is not a
    // precision result). Tracked separately and reported by name.
    let mut not_evaluated: Vec<String> = Vec::new();
    for (a, b, d, rel, action, sid, _rat, dry, applied) in &audit {
        let (ca, cb) = (label(a), label(b));
        let rel = rel.as_str();
        eprintln!(
            "[corpus D]   audit {ca}<->{cb} L2={d:.4} relation={rel} action={action} superseded_id={sid:?} dry={dry} applied={applied}\n             A={:?}\n             B={:?}",
            summary_of(&conn, *a),
            summary_of(&conn, *b)
        );
        if *dry != 1 || *applied != 0 {
            failures.push(format!("audit {ca}<->{cb}: dry_run/applied must be 1/0, got {dry}/{applied}"));
        }
        let key = canon_ids(&ca, &cb);
        let Some(exp) = expected.get(&key) else {
            continue; // unannotated near pair — logged above, no gate
        };
        let exp = exp.as_str();
        let seen = seen_pairs.contains(&key);
        let tag = if seen { "seen" } else { "HELD-OUT" };

        // Revision 1 — a judge error on a GATE pair is NOT a pass; record it as
        // "not evaluated" (held-out) and move on. Noise pairs (unannotated, no
        // `exp`) already `continue`d above, so this only fires on gate pairs.
        if rel == "error" {
            if seen {
                eprintln!("[corpus D]   [seen/sanity] {ca}<->{cb} ({exp}): judge ERROR — ignored");
            } else {
                eprintln!("[corpus D]   [HELD-OUT] {ca}<->{cb} ({exp}): judge ERROR → gate NOT evaluated");
                not_evaluated.push(format!("{ca}<->{cb} (expected {exp}) — judge error"));
            }
            continue;
        }

        // Gate violations ①②③ (④ = ① specialised to the held-out
        // replacement-target-mismatch traps; named for visibility).
        let viol: Option<String> = if matches!(exp, "contradiction" | "false_neighbor") && merges(rel) {
            let g = if target_traps.contains(&key) { "④/①" } else { "①" };
            Some(format!("gate {g}: expected {exp} but judged {rel} (soft-deletes useful memory)"))
        } else if exp == "duplicate" && !merges(rel) {
            Some(format!("gate ②: clear duplicate judged {rel} (must merge)"))
        } else if exp == "contradiction" && rel != "contradiction" {
            Some(format!("gate ③: contradiction judged {rel}"))
        } else {
            None
        };

        match (viol, seen) {
            (Some(v), false) => failures.push(format!("HELD-OUT {ca}<->{cb}: {v} [L2={d:.4}]")),
            (Some(v), true) => eprintln!(
                "[corpus D]   [seen/sanity] {ca}<->{cb}: {v} — taught in few-shot, NOT a gate [L2={d:.4}]"
            ),
            (None, _) => eprintln!("[corpus D]   [{tag}] {ca}<->{cb}: expected {exp}, judged {rel} ✓"),
        }

        // ADVISORY / WATCHED (logged, never fail). supersede→duplicate mislabel
        // is WATCHED (prompt-fix signal) and matched before the generic advisory.
        match (exp, rel) {
            ("supersede", "duplicate") => eprintln!(
                "[corpus D]   WATCHED {ca}<->{cb}: supersede mislabelled duplicate (still merges, loses direction — prompt-fix signal)"
            ),
            ("supersede", "supersede") => eprintln!(
                "[corpus D]   watched {ca}<->{cb}: supersede ✓ superseded_id={sid:?} (reconcile vs older-by-ts)"
            ),
            ("duplicate", "supersede") => eprintln!(
                "[corpus D]   advisory {ca}<->{cb}: expected {exp}, judged {rel} (both merge — label only)"
            ),
            ("complement", "no_action") | ("no_action", "complement") => eprintln!(
                "[corpus D]   advisory {ca}<->{cb}: expected {exp}, judged {rel} (both leave-live — label only)"
            ),
            _ => {}
        }
    }

    if !failures.is_empty() {
        panic!(
            "Layer D: {} HELD-OUT hard-gate / invariant failure(s):\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
    if !not_evaluated.is_empty() {
        // Not a panic (no precision violation), but NOT a clean pass either:
        // these held-out gates lack data this run (judge errors).
        eprintln!(
            "[corpus D] ⚠ {} HELD-OUT gate pair(s) NOT evaluated (judge error) — NOT counted as pass:\n  {}",
            not_evaluated.len(),
            not_evaluated.join("\n  ")
        );
    }
    eprintln!(
        "[corpus D] HELD-OUT hard gates: {} PASS, 0 fail, {} NOT-evaluated (judge error) over {} audit row(s); mapped {}/{} relation-cases; seen anchors = sanity",
        audit.len() - not_evaluated.len(),
        not_evaluated.len(),
        audit.len(),
        id_of.len(),
        rel_cases.len()
    );
}

// ===========================================================================
// Layer D-apply (`#[ignore]`) — the APPLY counterpart of Layer D. Runs the real
// pipeline, then `consolidate_once(dry_run=false)` to MUTATE live state:
// dedup/supersede set `superseded_by`, contradictions write `log_contradiction`.
// Asserts that every KP the apply path superseded then DISAPPEARS from a hybrid
// search seeded with its own text — proving apply + the search-side superseded
// filter compose. Known corpus targets the judge should merge: dup C031<->C032,
// supersede C021<->C022. Needs the real LLM + embedder; user runs from a terminal:
//   cargo test --bin opencrab-memory-mcp -- --ignored corpus_layer_d_apply --nocapture
// ===========================================================================
#[test]
#[ignore]
fn corpus_layer_d_apply() {
    let Some(extractor) = HttpExtractor::load() else {
        eprintln!("[corpus D-apply] no distiller config — SKIPPING");
        return;
    };
    let Some(embedder) = HttpEmbedder::load() else {
        eprintln!("[corpus D-apply] no embedder config — SKIPPING");
        return;
    };
    let cases = load_cases();
    let rel_cases: Vec<&Case> = cases
        .iter()
        .filter(|c| !c.gt.consolidation_relations.is_empty())
        .collect();
    assert!(!rel_cases.is_empty(), "no cases carry consolidation_relations");

    // ---- one dense DB: ingest ALL relation cases, distill, embed ----
    let agent = rel_cases[0].gt.agent_id.clone();
    let tmp = tempfile::tempdir().unwrap();
    let scan_root = tmp.path().join("agents").join(&agent).join("team_sessions");
    for c in &rel_cases {
        stage_rollout(&scan_root, c);
    }
    let db = tmp.path().join("memory.db");
    let conn = open(&db).unwrap();
    ingest_once(&conn, &scan_root, &agent).unwrap();
    let now_ms = current_time_ms() + DISTILL_IDLE_MS + 60_000;
    block_on(distill_once(&conn, &extractor, now_ms)).expect("distill_once");
    block_on(embed_pending_once(&conn, &embedder)).expect("embed_pending_once");

    // ---- inject ts from created_at_offset_hours so direction is real ----
    // (greedy locator-token map, same as Layer D — supersede/dup must pick the
    // right older KP; unmapped cases keep their distilled ts.)
    let base = current_time_ms();
    let all_rows: Vec<(i64, String, Option<String>)> = {
        let mut stmt = conn
            .prepare("SELECT id, summary, detail FROM log WHERE origin='distill' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let mut claimed: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut order: Vec<&Case> = rel_cases.clone();
    order.sort_by_key(|c| std::cmp::Reverse(locator_groups(&c.gt).len()));
    for c in &order {
        let groups = locator_groups(&c.gt);
        if groups.is_empty() {
            continue;
        }
        let hit = all_rows.iter().find(|(id, s, d)| {
            !claimed.contains(id)
                && all_groups(&format!("{s}\n{}", d.as_deref().unwrap_or("")), &groups)
        });
        if let (Some(off), Some((id, _, _))) = (c.gt.created_at_offset_hours, hit) {
            claimed.insert(*id);
            conn.execute("UPDATE log SET ts=?1 WHERE id=?2", params![base + off * 3_600_000, id])
                .unwrap();
        }
    }

    // ---- APPLY: judge + mutate in one pass ----
    let t = consolidation_distance_t();
    let n = block_on(consolidate_once(
        &conn,
        &extractor,
        t,
        std::time::Duration::from_secs(120),
        false, // APPLY: mutate live state.
    ))
    .expect("consolidate_once apply");
    eprintln!("[corpus D-apply] judged+applied {n} pair(s)");

    // ---- every superseded KP must vanish from a search seeded with its own text ----
    let superseded: Vec<(i64, i64, String, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, superseded_by, summary, detail FROM log \
                 WHERE superseded_by IS NOT NULL ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let contra: i64 = conn
        .query_row("SELECT count(*) FROM log_contradiction", [], |r| r.get(0))
        .unwrap();
    eprintln!(
        "[corpus D-apply] {} KP(s) superseded, {} contradiction edge(s)",
        superseded.len(),
        contra
    );
    assert!(
        !superseded.is_empty(),
        "apply must supersede at least the known dup/supersede pairs (C031<->C032, C021<->C022)"
    );
    drop(conn);

    let mut failures: Vec<String> = Vec::new();
    for (dead, survivor, summary, detail) in &superseded {
        let query = format!("{summary} {}", detail.as_deref().unwrap_or(""));
        let hits = block_on(search(&db, Some(&embedder), &query, 10)).expect("search");
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        eprintln!("[corpus D-apply]   superseded {dead} (→{survivor}) — search(own text) = {ids:?}");
        if ids.contains(dead) {
            failures.push(format!("superseded KP {dead} still surfaces for its own text: {ids:?}"));
        }
    }
    assert!(failures.is_empty(), "Layer D-apply: {}", failures.join("; "));
}
