    use super::{
        current_time_ms, strip_code_fence, strip_think_blocks, vec_to_match_json, BackendError,
        ExtractError, HttpExtractor,
    };
    use futures_util::stream::StreamExt;
    use rusqlite::{params, Connection, OptionalExtension};
    use std::path::Path;

    /// The five-way relation a judge can assign to a candidate KP pair.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Relation {
        /// Same knowledge, redundant wording → soft-merge (keep one).
        Duplicate,
        /// One KP replaces the other (explicit replacement content + newer ts)
        /// → soft-merge, older points at newer.
        Supersede,
        /// Same scope, incompatible answers, no replacement → keep both + flag.
        Contradiction,
        /// Same topic, different facets, useful together → keep both, related.
        Complement,
        /// Incidental neighbour (shared surface, different concern) → leave both.
        NoAction,
    }

    impl Relation {
        /// Canonical wire / audit string.
        pub fn as_str(&self) -> &'static str {
            match self {
                Relation::Duplicate => "duplicate",
                Relation::Supersede => "supersede",
                Relation::Contradiction => "contradiction",
                Relation::Complement => "complement",
                Relation::NoAction => "no_action",
            }
        }

        /// Parse the judge's `relation` field (case-insensitive). None for any
        /// token outside the five-way vocabulary.
        pub fn parse(s: &str) -> Option<Relation> {
            match s.trim().to_lowercase().as_str() {
                "duplicate" => Some(Relation::Duplicate),
                "supersede" => Some(Relation::Supersede),
                "contradiction" => Some(Relation::Contradiction),
                "complement" => Some(Relation::Complement),
                "no_action" | "no-action" | "noaction" => Some(Relation::NoAction),
                _ => None,
            }
        }

        /// The intent action recorded in the audit `action` column. dedup +
        /// supersede collapse to the same soft-merge action (the `relation`
        /// column keeps them distinguishable); complement + no_action both
        /// leave the rows live.
        pub fn intent_action(&self) -> &'static str {
            match self {
                Relation::Duplicate | Relation::Supersede => "would_merge",
                Relation::Contradiction => "would_contradict",
                Relation::Complement | Relation::NoAction => "leave",
            }
        }
    }

    /// One KP as the judge sees it.
    #[derive(Debug, Clone)]
    pub struct KpRef {
        pub id: i64,
        pub summary: String,
        pub detail: Option<String>,
        pub ts: i64,
    }

    /// The judge's verdict on one pair.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct JudgeVerdict {
        pub relation: Relation,
        pub rationale: String,
        /// Set ONLY for `Supersede` — the id of the older (replaced) KP.
        pub superseded_id: Option<i64>,
    }

    /// Render a candidate pair for the judge: both KPs (id/ts/summary/detail)
    /// plus the vector distance, explicitly labelled reference-only.
    pub fn build_judge_pair_block(a: &KpRef, b: &KpRef, distance: f64) -> String {
        let fmt = |k: &KpRef| {
            format!(
                "id: {}\nts: {}\nsummary: {}\ndetail: {}",
                k.id,
                k.ts,
                k.summary,
                k.detail.as_deref().unwrap_or("<none>")
            )
        };
        format!(
            "[VECTOR_DISTANCE] {distance:.4}   (reference only — does NOT determine the relation; larger ts = more recent)\n\n[KP A]\n{}\n\n[KP B]\n{}",
            fmt(a),
            fmt(b)
        )
    }

    /// Slice the outermost `{...}` object out of a model reply.
    fn extract_json_object_slice(s: &str) -> Result<&str, String> {
        let start = s
            .find('{')
            .ok_or_else(|| "no '{' found in judge reply".to_string())?;
        let end = s
            .rfind('}')
            .ok_or_else(|| "no '}' found in judge reply".to_string())?;
        if end < start {
            return Err("object braces out of order in judge reply".to_string());
        }
        Ok(&s[start..=end])
    }

    /// Tolerant parser for the judge's reply — the OBJECT counterpart of
    /// `parse_knowledge_points`: strip `<think>`, strip a ```json fence, slice
    /// the outermost `{…}`, then read {relation, rationale, superseded_id}. A
    /// reply whose `relation` is missing / unknown is an error (we refuse to
    /// guess a relation). Deliberately NOT the array slicer.
    pub fn parse_judge_verdict(s: &str) -> Result<JudgeVerdict, String> {
        let unthought = strip_think_blocks(s);
        let unfenced = strip_code_fence(&unthought);
        let sliced = extract_json_object_slice(unfenced)?;
        let value: serde_json::Value = serde_json::from_str(sliced)
            .map_err(|e| format!("invalid JSON object: {e} (after fence-strip + brace-slice)"))?;
        let rel_raw = value
            .get("relation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing string field `relation`".to_string())?;
        let relation = Relation::parse(rel_raw)
            .ok_or_else(|| format!("unknown relation {rel_raw:?} (expected one of the five)"))?;
        let rationale = value
            .get("rationale")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        // Accept a JSON number or a numeric string; null / absent → None.
        let superseded_id = value.get("superseded_id").and_then(|v| match v {
            serde_json::Value::Number(n) => n.as_i64(),
            serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        });
        Ok(JudgeVerdict {
            relation,
            rationale,
            superseded_id,
        })
    }

    impl HttpExtractor {
        /// Judge the relation between two KPs. Reuses the distiller's wire
        /// layer (`post_chat`) with `CONSOLIDATION_PROMPT` and the object
        /// parser — same model + creds, different prompt, no new HTTP code.
        pub async fn judge(
            &self,
            a: &KpRef,
            b: &KpRef,
            distance: f64,
        ) -> Result<JudgeVerdict, ExtractError> {
            let content = self
                .post_chat(CONSOLIDATION_PROMPT, &build_judge_pair_block(a, b, distance))
                .await?;
            parse_judge_verdict(&content).map_err(ExtractError::Parse)
        }
    }

    /// The judging seam behind `consolidate_once`. A trait (not the concrete
    /// `HttpExtractor::judge`) so the dry-run is unit-testable with a mock judge
    /// — mirrors how `Extractor` abstracts distillation. `Sync` so a shared
    /// `&judge` can drive bounded-concurrency judging.
    #[async_trait::async_trait]
    pub trait ConsolidationJudge: Sync {
        async fn judge_pair(
            &self,
            a: &KpRef,
            b: &KpRef,
            distance: f64,
        ) -> Result<JudgeVerdict, ExtractError>;
    }

    #[async_trait::async_trait]
    impl ConsolidationJudge for HttpExtractor {
        async fn judge_pair(
            &self,
            a: &KpRef,
            b: &KpRef,
            distance: f64,
        ) -> Result<JudgeVerdict, ExtractError> {
            self.judge(a, b, distance).await
        }
    }

    /// Hard cap on in-flight judge calls during a dry-run. Bounded so the LLM
    /// provider's chat endpoint isn't flooded — NEVER unbounded `join_all` over
    /// the (in a dense store, ~all-pairs) candidate set.
    const MAX_JUDGE_CONCURRENCY: usize = 8;

    /// Bounded retries for a judge call that TIMED OUT (only). A flaky proxy can
    /// wedge a single connection; each retry is a fresh `judge_pair` → new HTTP
    /// request → new connection. Retries ONLY on `Elapsed` — a non-timeout judge
    /// error (HTTP 4xx/5xx) is NOT retried here (that needs backoff, a separate
    /// concern). 2 retries = up to 3 attempts × the per-call timeout, all inside
    /// the one future (never drags the other lanes). De-flakes the dry-run / gate
    /// read without touching any verdict CLASSIFICATION.
    const MAX_JUDGE_RETRIES: usize = 2;

    /// Candidate-distance threshold T. Floor = Phase-2's measured max
    /// true-relation distance (0.9777) + margin; a candidate pair must measure
    /// strictly below T. Conservative-high: better to over-admit into the
    /// judge than to miss a true relation at the candidate gate. Override with
    /// `OPENCRAB_CONSOLIDATION_T` (tune on real data via dry-run).
    pub const DEFAULT_CONSOLIDATION_T: f64 = 1.10;

    pub fn consolidation_distance_t() -> f64 {
        std::env::var("OPENCRAB_CONSOLIDATION_T")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|t| t.is_finite() && *t > 0.0)
            .unwrap_or(DEFAULT_CONSOLIDATION_T)
    }

    // ----- S5-C-2: background consolidation loop (kill switch + threshold) -----

    /// Per-judge-call hard timeout for the background loop's apply pass — the
    /// same 120s the Layer-D / unit tests use, named for reuse.
    pub const JUDGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    /// Default # of new LIVE KPs that must accrue since the last consolidation
    /// run before the low-frequency loop fires again. Override with
    /// `OPENCRAB_CONSOLIDATION_MIN_NEW_KPS`.
    pub const CONSOLIDATION_MIN_NEW_KPS: i64 = 25;

    /// Kill switch for the background consolidation step. Default OFF — the loop
    /// only consolidates when `OPENCRAB_CONSOLIDATION_ENABLED` is "1"/"true"
    /// (case-insensitive). Anything else (incl. unset / "0" / "false") → OFF.
    /// Same env-reading shape as [`consolidation_distance_t`].
    pub fn consolidation_enabled() -> bool {
        parse_enabled_flag(std::env::var("OPENCRAB_CONSOLIDATION_ENABLED").ok().as_deref())
    }

    /// Pure truth table for the kill switch (split out so it's testable without
    /// mutating process env): "1"/"true" (case-insensitive, trimmed) → true;
    /// `None` / anything else (incl. "0"/"false"/garbage) → false.
    pub fn parse_enabled_flag(v: Option<&str>) -> bool {
        matches!(
            v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
            Some("1") | Some("true")
        )
    }

    /// Effective trigger threshold (env override, else [`CONSOLIDATION_MIN_NEW_KPS`]).
    pub fn consolidation_min_new_kps() -> i64 {
        parse_min_new_kps(std::env::var("OPENCRAB_CONSOLIDATION_MIN_NEW_KPS").ok().as_deref())
    }

    /// Pure parse for the threshold env (testable without env mutation): a
    /// positive integer overrides; `None` / non-positive / unparseable falls back
    /// to [`CONSOLIDATION_MIN_NEW_KPS`].
    pub fn parse_min_new_kps(v: Option<&str>) -> i64 {
        v.and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(CONSOLIDATION_MIN_NEW_KPS)
    }

    /// Read the consolidation marker (the `MAX(log.id)` the last completed run
    /// advanced to). A missing row (shouldn't happen post-v9) reads as 0.
    pub fn consolidation_marker(conn: &Connection) -> Result<i64, BackendError> {
        Ok(conn
            .query_row(
                "SELECT last_consolidation_max_log_id FROM consolidation_state WHERE id = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Count LIVE KPs (superseded_by IS NULL) newer than the consolidation
    /// marker — the "new since last run" signal that gates the low-frequency
    /// loop. Reads the marker via the same subquery the loop uses.
    pub fn count_new_live(conn: &Connection) -> Result<i64, BackendError> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM log \
             WHERE superseded_by IS NULL \
               AND id > (SELECT last_consolidation_max_log_id FROM consolidation_state WHERE id = 1)",
            [],
            |r| r.get::<_, i64>(0),
        )?)
    }

    /// Advance the consolidation marker to `max_id` (the `MAX(log.id)` snapshot
    /// taken at the START of the run) and stamp `now_ms`. Using the START
    /// snapshot is deliberate: KPs the MCP side writes via `log_progress` DURING
    /// the (possibly minutes-long) judge pass get a fresh `id > max_id`, so they
    /// are counted in the NEXT round rather than silently skipped.
    pub fn advance_consolidation_marker(
        conn: &Connection,
        max_id: i64,
        now_ms: i64,
    ) -> Result<(), BackendError> {
        conn.execute(
            "UPDATE consolidation_state \
             SET last_consolidation_max_log_id = ?1, last_consolidation_ts = ?2 \
             WHERE id = 1",
            params![max_id, now_ms],
        )?;
        Ok(())
    }

    /// One consolidation gate-check + (threshold met) a real apply pass. Called
    /// ONLY when [`consolidation_enabled`] is true (the loop gates on that first,
    /// so a disabled loop never touches this). Snapshots `MAX(log.id)` at the
    /// START (see [`advance_consolidation_marker`]); no-op when fewer than
    /// [`consolidation_min_new_kps`] new LIVE KPs have accrued. With no embedder
    /// the store has no `log_vec` rows → `find_candidates` yields nothing →
    /// `consolidate_once` writes nothing (safe). NEVER writes a TSV (that is the
    /// dry-run inspection surface only). `consolidate_once` is `!Send` and needs
    /// the time driver — fine on the distill thread's current_thread+enable_all rt.
    pub async fn run_consolidation_step(
        conn: &Connection,
        extractor: &HttpExtractor,
    ) -> Result<(), BackendError> {
        let max_id: i64 =
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM log", [], |r| r.get(0))?;
        let new_live = count_new_live(conn)?;
        let threshold = consolidation_min_new_kps();
        if new_live < threshold {
            return Ok(()); // not enough new memory yet — stay quiet.
        }

        let count = |sql: &str| -> Result<i64, BackendError> {
            Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))?)
        };
        let before_superseded = count("SELECT COUNT(*) FROM log WHERE superseded_by IS NOT NULL")?;
        let before_contra = count("SELECT COUNT(*) FROM log_contradiction")?;

        let t = consolidation_distance_t();
        let candidates = consolidate_once(conn, extractor, t, JUDGE_TIMEOUT, false).await?;

        let merged = count("SELECT COUNT(*) FROM log WHERE superseded_by IS NOT NULL")? - before_superseded;
        let contradictions = count("SELECT COUNT(*) FROM log_contradiction")? - before_contra;

        // Advance with the START snapshot, AFTER the pass completed.
        advance_consolidation_marker(conn, max_id, current_time_ms())?;

        eprintln!(
            "[consolidate] ran: {candidates} candidate(s) judged, {merged} merged, \
             {contradictions} contradiction(s) (new_live={new_live} >= {threshold}, marker→{max_id})"
        );
        Ok(())
    }

    /// Neighbours fetched per live KP when sweeping for candidates. In a dense
    /// store the within-T set per row is small; 64 matches Layer C's pool.
    const CANDIDATE_KNN_K: i64 = 64;

    /// Read one row's stored embedding back out of log_vec as `f32`s.
    /// sqlite-vec stores float32 vectors as raw little-endian bytes; a row with
    /// no embedding yet (or a malformed blob) yields None and is skipped.
    fn read_stored_vector(conn: &Connection, rowid: i64) -> Option<Vec<f32>> {
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM log_vec WHERE rowid = ?1",
                params![rowid],
                |r| r.get(0),
            )
            .ok()?;
        if blob.is_empty() || blob.len() % 4 != 0 {
            return None;
        }
        Some(
            blob.chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        )
    }

    /// Find candidate pairs for consolidation: every LIVE KP (superseded_by IS
    /// NULL) KNN'd against log_vec, neighbours strictly under `t`, formed into
    /// canonical (a<b) pairs. Excludes: a pair with a superseded member, a pair
    /// already in `log_consolidation_audit` (idempotency — a re-run is a no-op
    /// until content changes), and self-pairs. Pure read; no mutation.
    pub fn find_candidates(conn: &Connection, t: f64) -> Result<Vec<(i64, i64, f64)>, BackendError> {
        let live: Vec<i64> = {
            let mut stmt =
                conn.prepare("SELECT id FROM log WHERE superseded_by IS NULL ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            v
        };
        let live_set: std::collections::HashSet<i64> = live.iter().copied().collect();

        // Already-judged pairs (canonical) — skip so a re-run doesn't re-judge.
        let judged: std::collections::HashSet<(i64, i64)> = {
            let mut stmt = conn.prepare("SELECT kp_a, kp_b FROM log_consolidation_audit")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            let mut s = std::collections::HashSet::new();
            for r in rows {
                let (a, b) = r?;
                s.insert(if a <= b { (a, b) } else { (b, a) });
            }
            s
        };

        let mut seen: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
        let mut out: Vec<(i64, i64, f64)> = Vec::new();
        let mut knn = conn.prepare(
            "SELECT rowid, distance FROM log_vec WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
        )?;
        for &id in &live {
            let Some(vec) = read_stored_vector(conn, id) else {
                continue;
            };
            let qjson = vec_to_match_json(&vec);
            let rows = knn.query_map(params![qjson, CANDIDATE_KNN_K], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })?;
            for r in rows {
                let (nid, dist) = r?;
                if nid == id || dist >= t || !live_set.contains(&nid) {
                    continue;
                }
                let pair = if id < nid { (id, nid) } else { (nid, id) };
                if judged.contains(&pair) || !seen.insert(pair) {
                    continue;
                }
                out.push((pair.0, pair.1, dist));
            }
        }
        Ok(out)
    }

    /// Load one KP for the judge.
    fn load_kpref(conn: &Connection, id: i64) -> Result<KpRef, BackendError> {
        let kp = conn.query_row(
            "SELECT id, summary, detail, ts FROM log WHERE id = ?1",
            params![id],
            |r| {
                Ok(KpRef {
                    id: r.get(0)?,
                    summary: r.get(1)?,
                    detail: r.get(2)?,
                    ts: r.get(3)?,
                })
            },
        )?;
        Ok(kp)
    }

    /// Pure: decide the soft-merge direction for one judged pair from the two KP
    /// ids, their `ts`, and (for supersede) the judge's claimed `superseded_id`.
    /// NO DB — the referential-integrity guards live in `apply_verdict`. This is
    /// the load-bearing direction logic:
    ///   * duplicate  → retire the OLDER (by ts; tie → smaller id), keep newer.
    ///   * supersede  → ts decides direction; the judge must AGREE the older one
    ///     is the replaced one, else we refuse to guess (`Skip`). Co-temporal →
    ///     fall back to the judge's named member; if it named neither, `Skip`.
    ///   * contradiction → `Contradict`; complement / no_action → `Leave`.
    #[derive(Debug, PartialEq, Eq)]
    pub enum MergeDecision {
        /// Set `log[superseded].superseded_by = survivor`.
        Supersede { superseded: i64, survivor: i64 },
        /// Record a contradiction edge; leave `superseded_by` untouched.
        Contradict,
        /// No mutation (complement / no_action).
        Leave,
        /// No mutation; append this marker to the audit rationale.
        Skip(&'static str),
    }

    pub fn decide_merge(
        relation: Relation,
        a: i64,
        ts_a: i64,
        b: i64,
        ts_b: i64,
        judge_superseded_id: Option<i64>,
    ) -> MergeDecision {
        use std::cmp::Ordering;
        match relation {
            Relation::Complement | Relation::NoAction => MergeDecision::Leave,
            Relation::Contradiction => MergeDecision::Contradict,
            Relation::Duplicate => {
                // Keep newer, retire older; co-temporal → the smaller id is older.
                let superseded = match ts_a.cmp(&ts_b) {
                    Ordering::Less => a,
                    Ordering::Greater => b,
                    Ordering::Equal => a.min(b),
                };
                let survivor = if superseded == a { b } else { a };
                MergeDecision::Supersede { superseded, survivor }
            }
            Relation::Supersede => match ts_a.cmp(&ts_b) {
                // Co-temporal: ts can't direct it → trust the judge's named member.
                Ordering::Equal => match judge_superseded_id {
                    Some(x) if x == a => MergeDecision::Supersede { superseded: a, survivor: b },
                    Some(x) if x == b => MergeDecision::Supersede { superseded: b, survivor: a },
                    _ => MergeDecision::Skip("[apply skipped: supersede direction undeterminable]"),
                },
                ord => {
                    let (older, newer) = if ord == Ordering::Less { (a, b) } else { (b, a) };
                    // Require the judge to agree the OLDER one is the replaced one.
                    if judge_superseded_id == Some(older) {
                        MergeDecision::Supersede { superseded: older, survivor: newer }
                    } else {
                        MergeDecision::Skip("[apply skipped: judge/ts direction mismatch]")
                    }
                }
            },
        }
    }

    /// Apply one real verdict's mutation inside the caller's transaction `tx`.
    /// Returns `(applied, rationale)`: `applied=true` ONLY when a live row was
    /// actually mutated; on any guard miss `applied=false` and a
    /// `[apply skipped: …]` marker is appended to the rationale. Enforces the
    /// referential integrity SQLite's declarative FK does not — both KPs must
    /// exist, and BOTH endpoints must still be live: the survivor (never point a
    /// row at a non-live survivor) AND the loser (first-survivor-wins — a KP is
    /// superseded at most once per round; a later pair retiring it again audits as
    /// a skip). This is NOT chain elimination: a survivor itself later superseded
    /// still forms a chain, which the chain-agnostic search filter hides at any depth.
    fn apply_verdict(
        tx: &Connection,
        a: i64,
        b: i64,
        audit_id: i64,
        relation: Relation,
        judge_superseded_id: Option<i64>,
        base_rationale: &str,
    ) -> Result<(bool, String), BackendError> {
        let skip = |marker: &str| (false, format!("{base_rationale} {marker}"));

        if matches!(relation, Relation::Contradiction) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            let changed = tx.execute(
                "INSERT INTO log_contradiction (id_a, id_b, audit_id) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(id_a, id_b) DO NOTHING",
                params![lo, hi, audit_id],
            )?;
            return Ok((changed > 0, base_rationale.to_string()));
        }

        // dup / supersede → maybe a `superseded_by` update; complement/no_action → Leave.
        let ts_of = |id: i64| -> Result<Option<i64>, BackendError> {
            Ok(tx
                .query_row("SELECT ts FROM log WHERE id = ?1", params![id], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?)
        };
        let (Some(ts_a), Some(ts_b)) = (ts_of(a)?, ts_of(b)?) else {
            return Ok(skip("[apply skipped: KP missing]"));
        };

        match decide_merge(relation, a, ts_a, b, ts_b, judge_superseded_id) {
            MergeDecision::Leave => Ok((false, base_rationale.to_string())),
            MergeDecision::Contradict => unreachable!("contradiction handled above"),
            MergeDecision::Skip(marker) => Ok(skip(marker)),
            MergeDecision::Supersede { superseded, survivor } => {
                if superseded == survivor {
                    return Ok(skip("[apply skipped: self-merge]"));
                }
                // Read one row's state. Outer Option = row present? (both exist —
                // `ts_of` guarded above); inner Option = its superseded_by NULL?
                let state_of = |id: i64| -> Result<Option<Option<i64>>, BackendError> {
                    Ok(tx
                        .query_row(
                            "SELECT superseded_by FROM log WHERE id = ?1",
                            params![id],
                            |r| r.get::<_, Option<i64>>(0),
                        )
                        .optional()?)
                };
                // The loser must still be live — first-survivor-wins: a KP is
                // superseded at most once per round (a later pair retiring it again
                // audits as a skip, never overwriting its survivor pointer). NOT
                // chain elimination: a survivor later superseded still forms a chain,
                // which the chain-agnostic search filter hides at any depth.
                match state_of(superseded)? {
                    None => return Ok(skip("[apply skipped: superseded KP missing]")),
                    Some(Some(_)) => return Ok(skip("[apply skipped: loser already superseded]")),
                    Some(None) => {}
                }
                // The survivor must still be live — never point at a non-live row.
                match state_of(survivor)? {
                    None => Ok(skip("[apply skipped: survivor missing]")),
                    Some(Some(_)) => Ok(skip("[apply skipped: survivor no longer live]")),
                    Some(None) => {
                        tx.execute(
                            "UPDATE log SET superseded_by = ?1 WHERE id = ?2",
                            params![survivor, superseded],
                        )?;
                        Ok((true, base_rationale.to_string()))
                    }
                }
            }
        }
    }

    /// One pair's phase-3 work in a single transaction: INSERT the audit row,
    /// then — APPLY mode (`dry_run=false`) on a real verdict only — mutate live
    /// state via `apply_verdict` and stamp `applied` / the augmented rationale.
    /// The audit row and its mutation commit together or roll back together
    /// (`unchecked_transaction` because `consolidate_once` holds `&Connection`).
    /// An error verdict (`relation="error"`) is audited but never applied.
    pub fn consolidate_pair(
        conn: &Connection,
        run_ts: i64,
        a: i64,
        b: i64,
        dist: f64,
        verdict: Result<JudgeVerdict, String>,
        dry_run: bool,
    ) -> Result<(), BackendError> {
        let (relation_str, action, base_rationale, superseded_id, rel_enum): (
            &str,
            &str,
            String,
            Option<i64>,
            Option<Relation>,
        ) = match verdict {
            Ok(v) => (
                v.relation.as_str(),
                v.relation.intent_action(),
                v.rationale,
                v.superseded_id,
                Some(v.relation),
            ),
            // `relation="error"` is NOT one of the five — audited, never applied.
            Err(reason) => ("error", "error", reason, None, None),
        };

        let tx = conn.unchecked_transaction()?;
        let dry_flag: i64 = if dry_run { 1 } else { 0 };
        tx.execute(
            "INSERT INTO log_consolidation_audit\
             (run_ts, kp_a, kp_b, distance, relation, action, rationale, superseded_id, dry_run, applied)\
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)",
            params![run_ts, a, b, dist, relation_str, action, base_rationale, superseded_id, dry_flag],
        )?;
        let audit_id = tx.last_insert_rowid();

        // Apply only in apply mode AND only for a real (non-error) verdict.
        let (applied, rationale) = match (dry_run, rel_enum) {
            (false, Some(rel)) => {
                apply_verdict(&tx, a, b, audit_id, rel, superseded_id, &base_rationale)?
            }
            _ => (false, base_rationale.clone()),
        };
        if applied || rationale != base_rationale {
            tx.execute(
                "UPDATE log_consolidation_audit SET applied = ?1, rationale = ?2 WHERE id = ?3",
                params![applied as i64, rationale, audit_id],
            )?;
        }
        tx.commit()?;

        eprintln!(
            "[consolidate {}] {a}<->{b} L2={dist:.4} → {relation_str} (superseded_id={superseded_id:?}) applied={applied} :: {rationale}",
            if dry_run { "dry-run" } else { "apply" }
        );
        Ok(())
    }

    /// Consolidate candidate pairs. `dry_run=true` preserves the original
    /// preview behaviour — judge each pair and append an audit row
    /// (`dry_run=1, applied=0`), ZERO mutation of live state. `dry_run=false` is
    /// judge-and-apply in one pass: each audit row is written `dry_run=0`, then
    /// (for a real verdict) the live mutation runs in the SAME transaction and
    /// `applied` is set to 1 iff a row was actually mutated (see
    /// `consolidate_pair` / `apply_verdict`). Returns the number of audit rows
    /// written (== candidate count, errors included).
    ///
    /// Three phases keep the `!Sync` `Connection` out of the concurrent judging:
    ///   1. (conn) `find_candidates` + load every `KpRef`.
    ///   2. (no conn) judge all pairs with BOUNDED concurrency
    ///      (`MAX_JUDGE_CONCURRENCY`, never an unbounded `join_all`); EACH judge
    ///      call is wrapped in a hard `tokio::time::timeout(judge_timeout)`, so a
    ///      hung upstream call (which reqwest's own request timeout did NOT
    ///      reliably catch through the local proxy) is force-aborted →
    ///      `relation="error"` verdict, never a panic, other in-flight calls
    ///      untouched.
    ///   3. (conn) sort by `(id_a,id_b)`, then one `consolidate_pair` per
    ///      candidate (each its own transaction) — deterministic order.
    ///
    /// REQUIRES a runtime with the TIME DRIVER on (`enable_all` / `enable_time`):
    /// both the per-call `tokio::time::timeout` AND reqwest's own timeout are
    /// timer-driven — on a runtime without a timer NEITHER fires and a hung call
    /// hangs forever. `block_on` (tests) uses `enable_all`; S5-C-2's loop MUST too.
    ///
    /// !Send BY DESIGN: `conn` (rusqlite Connection, `!Sync`) is alive across the
    /// phase-2 `.await`, so this future is `!Send`. Run it ONLY on a current-thread
    /// runtime / LocalSet / `block_on` — tests, and S5-C-2's background
    /// low-frequency consolidation loop. NEVER call it from rmcp `call_tool` (which
    /// requires a `Send` future): that is the exact `!Sync`-across-await wall
    /// `search` dodged by staging its connection behind a `&Path` open-per-phase.
    pub async fn consolidate_once<J: ConsolidationJudge>(
        conn: &Connection,
        judge: &J,
        t: f64,
        judge_timeout: std::time::Duration,
        dry_run: bool,
    ) -> Result<usize, BackendError> {
        // ---- Phase 1 (conn): candidates + KpRefs ----
        let candidates = find_candidates(conn, t)?;
        let mut prepared: Vec<(KpRef, KpRef, f64)> = Vec::with_capacity(candidates.len());
        for (a, b, dist) in candidates {
            prepared.push((load_kpref(conn, a)?, load_kpref(conn, b)?, dist));
        }

        // ---- Phase 2 (no conn): bounded-concurrency judging ----
        // buffer_unordered keeps at most MAX judge calls in flight; the judge
        // path never touches `conn`. Each call gets a reqwest-AGNOSTIC hard wall:
        // tokio::time::timeout aborts the WHOLE judge future at `judge_timeout`
        // no matter where it hangs (connect/TLS/read/proxy). Per-future, so a
        // timeout never drags the other in-flight calls. Any failure (error or
        // timeout) is captured as a verdict string, never a panic.
        let mut results: Vec<(i64, i64, f64, Result<JudgeVerdict, String>)> =
            futures_util::stream::iter(prepared)
                .map(move |(ka, kb, dist)| async move {
                    // Bounded retry ONLY on timeout: a wedged proxy connection is
                    // abandoned and the next attempt is a fresh judge_pair → new
                    // HTTP request → new connection. A non-timeout judge error is
                    // NOT retried (it needs backoff — separate concern). All inside
                    // this one future → never drags the other in-flight lanes, and
                    // it changes no verdict CLASSIFICATION (only timeout→error gaps).
                    let mut r: Result<JudgeVerdict, String> = Err(format!(
                        "judge timeout after {} attempts ({}s each)",
                        MAX_JUDGE_RETRIES + 1,
                        judge_timeout.as_secs()
                    ));
                    for _ in 0..=MAX_JUDGE_RETRIES {
                        match tokio::time::timeout(judge_timeout, judge.judge_pair(&ka, &kb, dist))
                            .await
                        {
                            Ok(Ok(v)) => {
                                r = Ok(v);
                                break;
                            }
                            Ok(Err(e)) => {
                                r = Err(format!("judge error: {e}"));
                                break;
                            }
                            // timeout → retry with a fresh request; r keeps the
                            // "retries exhausted" message if every attempt times out.
                            Err(_elapsed) => continue,
                        }
                    }
                    (ka.id, kb.id, dist, r)
                })
                .buffer_unordered(MAX_JUDGE_CONCURRENCY)
                .collect()
                .await;

        // ---- Phase 3 (conn): per-pair atomic audit + (apply mode) mutation ----
        // Each pair is its own transaction (audit row + any mutation commit
        // together). Sequential + committed → a later pair sees an earlier
        // pair's superseded_by, so the survivor-still-live guard works within
        // one run. `relation="error"` rows are audited but never applied.
        results.sort_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)));
        let run_ts = current_time_ms();
        let mut written = 0usize;
        for (a, b, dist, r) in results {
            consolidate_pair(conn, run_ts, a, b, dist, r, dry_run)?;
            written += 1;
        }
        Ok(written)
    }

    /// Dump the whole `log_consolidation_audit` table to a TSV at `path` — the
    /// §12 dry-run inspection surface. Always callable after a dry-run; ordered
    /// by `(kp_a,kp_b)` for stable diffs. Returns the row count written.
    pub fn dump_audit_tsv(conn: &Connection, path: &Path) -> Result<usize, BackendError> {
        use std::fmt::Write as _;
        let mut stmt = conn.prepare(
            "SELECT run_ts, kp_a, kp_b, distance, relation, action, superseded_id, dry_run, applied, rationale \
             FROM log_consolidation_audit ORDER BY kp_a, kp_b, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, Option<String>>(9)?.unwrap_or_default(),
            ))
        })?;
        let mut tsv = String::from(
            "run_ts\tkp_a\tkp_b\tdistance\trelation\taction\tsuperseded_id\tdry_run\tapplied\trationale\n",
        );
        let mut n = 0usize;
        for row in rows {
            let (run_ts, a, b, dist, rel, action, sid, dry, applied, rat) = row?;
            let rat1 = rat.replace(['\t', '\n'], " ");
            let _ = writeln!(
                tsv,
                "{run_ts}\t{a}\t{b}\t{dist:.4}\t{rel}\t{action}\t{}\t{dry}\t{applied}\t{rat1}",
                sid.map(|x| x.to_string()).unwrap_or_else(|| "null".into())
            );
            n += 1;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, tsv)?;
        Ok(n)
    }

    /// System prompt for the consolidation judge. Spec: S5-B §3. Drafted to the
    /// five-relation criteria; few-shot anchors are grounded in v2-corpus
    /// scenarios (retry-jitter dup, RabbitMQ→Kafka supersede, blue-green↔rolling
    /// contradiction, PG-store↔PG-roles complement, Kafka-new↔REST-old recency
    /// trap). The FN anchor is a SYNTHETIC Nginx near-FN ON PURPOSE: the corpus
    /// Redis cache↔lock pair (C027↔C028, the hardest near-FN at 0.79) is held
    /// back so Layer D tests gate ① on it unseen. Keep all prompt edits here.
    pub const CONSOLIDATION_PROMPT: &str = r#"# ROLE

You are a Memory Consolidation Judge for an AI software-engineering agent's long-term memory. You are given TWO knowledge points (KPs) already in the store, plus the vector distance between them. Decide their semantic relationship — EXACTLY ONE of five — so the system can decide whether to merge, flag, or leave them.

# THE FIVE RELATIONS

- duplicate — same knowledge, redundant. Different wording, SAME claim/lesson. (System keeps one.)
- supersede — one KP makes the other OBSOLETE because the world CHANGED: there is explicit REPLACEMENT content ("switched from X to Y", "migrated to", "now use Z instead of W") AND a time order (the replacer is newer). (System soft-merges; the older points at the newer.)
- contradiction — same scope/question, INCOMPATIBLE answers, NO replacement intent, roughly co-temporal (neither obsoletes the other; they simply disagree). (System keeps BOTH and flags them.)
- complement — same topic, DIFFERENT facets, both true and USEFUL TOGETHER (recalling one, the other adds value). (System keeps both, related.)
- no_action — incidental neighbours: they share surface words (same library/entity) but address DIFFERENT concerns; recalling one, the other is NOISE. (System leaves both untouched.)

# DECISION RULES — a decision tree, applied IN ORDER

vector_distance is REFERENCE ONLY and never decides the relation (a false neighbour measured 0.79 — closer than a true contradiction at 0.80). Judge from CONTENT. Walk the steps top to bottom and take the FIRST that fits:

1. SAME QUESTION? Do the two KPs answer the SAME specific question / decision / fact? Sharing an entity, library, or broad topic is NOT enough — they must address the same concrete question. If they answer DIFFERENT questions (or you are unsure they are even about the same thing) → no_action. This is the false-neighbour guard: e.g. "use Redis as a cache" and "use Redis as a distributed lock" share the entity Redis but answer different questions → no_action.

2. SAME CLAIM (restated)? They are on the same question — do both assert the SAME claim / lesson / decision, merely reworded or re-derived (different phrasing still counts as the same)? → duplicate. A restated same claim is NOT complement; complement is for DIFFERENT facets (step 4).

3. INCOMPATIBLE answers? They give incompatible answers to that one question — adopting one PRECLUDES the other (you can pick only one)? Then:
   - one carries explicit REPLACEMENT content (X→Y) and is the NEWER KP → supersede (the older is replaced). The replacement must target THE OTHER KP ITSELF — a KP whose replacement aims at some THIRD thing (e.g. "switched from REST to gRPC") does NOT supersede an unrelated neighbour (e.g. a RabbitMQ KP); that is no_action.
   - otherwise (roughly co-temporal, no replacement language) → contradiction (keep both, flag).
   Mutual exclusion is the test: two answers that CANNOT both be adopted are contradiction (or supersede), NEVER complement. "Different emphasis / different facet" is complement ONLY if both can hold AT ONCE.

4. COMPATIBLE facets (co-recall)? Same question/topic, DIFFERENT non-conflicting facets that COEXIST and are USEFUL TOGETHER — recalling one, the other genuinely adds value? → complement. Complement requires the two to be simultaneously true and jointly useful; it is NOT a catch-all for "same topic, not sure".

5. ELSE — related but not a clean duplicate / contradiction / complement, or still unsure → no_action (keep both untouched).

CONSERVATIVE BIAS: the safe defaults are step 1 and step 5 — both no_action. A wrong MERGE soft-deletes useful memory (the costliest error), so never reach for duplicate/supersede unless step 2 or 3 clearly fits. BUT the conservative fallback is no_action, NOT complement: do not hide a clear duplicate (step 2) or a clear mutual-exclusion (step 3) behind "complement". Complement is a specific verdict (step 4: coexisting facets), never a soft landing for uncertainty.

# OUTPUT (strict)

Reply with ONE JSON object and NOTHING else — no prose, no markdown fence:
{"relation":"duplicate|supersede|contradiction|complement|no_action","rationale":"<one sentence>","superseded_id":<id|null>}
- superseded_id: ONLY for supersede — the id of the OLDER KP (the one being replaced). null for every other relation.

# EXAMPLES

[VECTOR_DISTANCE] 0.0400   (reference only)

[KP A]
id: 7
ts: 1000
summary: 重试加随机 jitter 防 thundering herd
detail: 给重试间隔加随机抖动,避免多个客户端同步重试、同时打爆下游。

[KP B]
id: 12
ts: 1200
summary: 重试要带随机抖动错开
detail: 失败重试时在退避基础上叠加随机扰动,错开各实例的重试时刻,防止同步重试压垮下游服务。

{"relation":"duplicate","rationale":"同一问题(重试如何防 thundering herd)上的同一主张(加随机抖动错开),只是措辞不同——是同一条、不是不同 facet → duplicate(非 complement)。","superseded_id":null}

---

[VECTOR_DISTANCE] 0.7100   (reference only)

[KP A]
id: 4
ts: 1000
summary: 消息队列用 RabbitMQ
detail: 异步消息队列选用 RabbitMQ。

[KP B]
id: 9
ts: 5320
summary: 消息队列从 RabbitMQ 换到 Kafka
detail: 因吞吐、持久化重放与分区需求,把消息队列从 RabbitMQ 换到 Kafka。

{"relation":"supersede","rationale":"B 以明确替换内容(吞吐/持久化重放/分区)从 RabbitMQ 换到 Kafka 且 ts 更晚 → A 被取代。","superseded_id":4}

---

[VECTOR_DISTANCE] 0.7700   (reference only)

[KP A]
id: 3
ts: 1000
summary: 发布用蓝绿部署
detail: 采用蓝绿部署,新旧环境并存、可秒级回滚。

[KP B]
id: 8
ts: 1000
summary: 发布用滚动部署
detail: 采用滚动发布,逐批替换实例、更省资源。

{"relation":"contradiction","rationale":"同一决策(发布策略)的互斥答案:采纳蓝绿即排除滚动(只能选一种),co-temporal、无替换语言 → contradiction;互斥就不是 complement,双方都留并互标。","superseded_id":null}

---

[VECTOR_DISTANCE] 0.9200   (reference only)

[KP A]
id: 5
ts: 1000
summary: 主数据库用 Postgres
detail: 选 Postgres 作主库(事务一致性 + 生态)。

[KP B]
id: 11
ts: 1100
summary: Postgres 应用账号 app_rw 只授 DML,DDL 走 migrator 角色
detail: PG 权限约定:应用账号 app_rw 只给 DML,DDL 由独立 migrator 角色执行。

{"relation":"complement","rationale":"同主题(用 Postgres)的不同 facet:选型决策 + 权限角色约定,一起构成完整画面、都有用 → 互补,双方都留。","superseded_id":null}

---

[VECTOR_DISTANCE] 0.8100   (reference only)

[KP A]
id: 6
ts: 1000
summary: Nginx 作反向代理 + 上游负载均衡
detail: 用 Nginx 当流量入口,反向代理到后端服务、按 upstream 轮询做负载均衡。

[KP B]
id: 15
ts: 1040
summary: Nginx 直接托管前端静态资源并开 gzip
detail: 用 Nginx 托管前端静态文件,开 gzip 压缩 + 缓存头以降低带宽。

{"relation":"no_action","rationale":"同实体 Nginx 但两种不同用途(流量入口的反代/负载均衡 vs 静态文件托管);排查负载均衡时召回静态资源配置是噪声、不构成同一主题互补 → no_action,非 complement。","superseded_id":null}

---

[VECTOR_DISTANCE] 0.8300   (reference only)

[KP A]
id: 9
ts: 5320
summary: 消息队列从 RabbitMQ 换到 Kafka
detail: 把消息队列换到 Kafka(吞吐/重放/分区)。

[KP B]
id: 2
ts: 200
summary: 内部服务间通信用 REST
detail: 内部服务之间采用 REST 接口。

{"relation":"no_action","rationale":"A 较新,但它替换的是 RabbitMQ 而非 REST;Kafka(消息队列)与 REST(服务间传输)是不同主题、互不替换 → 仅凭更新不判 supersede,no_action。","superseded_id":null}
"#;
