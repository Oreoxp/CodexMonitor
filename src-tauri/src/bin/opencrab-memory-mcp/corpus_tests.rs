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
