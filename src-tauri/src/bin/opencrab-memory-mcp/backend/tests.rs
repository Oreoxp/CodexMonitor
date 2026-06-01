    use super::*;

    fn db_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nested/memory.db");
        (tmp, path)
    }

    #[test]
    fn schema_creation_is_idempotent() {
        let (_tmp, db) = db_path();
        // First open: creates parent dir, file, schema.
        drop(open(&db).unwrap());
        assert!(db.exists(), "open must create the db file");
        // Second open against the same file: must not error and must not
        // recreate or duplicate any object.
        let conn = open(&db).unwrap();
        let table_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='log'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 1);
        let trigger_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'log_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger_count, 3);
    }

    #[test]
    fn open_sets_wal_journal_mode() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn log_progress_inserts_summary_only() {
        let (_tmp, db) = db_path();
        let id = log_progress(&db, "wrote a kickoff plan", None).unwrap();
        assert!(id > 0);
        let entry = get(&db, id).unwrap();
        assert!(entry.found);
        assert_eq!(entry.id, id);
        assert_eq!(entry.summary, "wrote a kickoff plan");
        assert_eq!(entry.detail, None);
        assert!(entry.ts > 0);
    }

    #[test]
    fn log_progress_inserts_summary_and_detail() {
        let (_tmp, db) = db_path();
        let id = log_progress(
            &db,
            "shipped auth refactor",
            Some("Replaced the legacy session middleware with the JWT/OAuth flow."),
        )
        .unwrap();
        let entry = get(&db, id).unwrap();
        assert!(entry.found);
        assert_eq!(entry.summary, "shipped auth refactor");
        assert_eq!(
            entry.detail.as_deref(),
            Some("Replaced the legacy session middleware with the JWT/OAuth flow.")
        );
    }

    #[test]
    fn log_progress_trims_summary_and_rejects_empty() {
        let (_tmp, db) = db_path();
        assert!(matches!(
            log_progress(&db, "", None),
            Err(BackendError::EmptySummary)
        ));
        assert!(matches!(
            log_progress(&db, "   \n\t  ", None),
            Err(BackendError::EmptySummary)
        ));
        // Trim is applied so leading/trailing whitespace does not survive.
        let id = log_progress(&db, "  trimmed ok  ", None).unwrap();
        assert_eq!(get(&db, id).unwrap().summary, "trimmed ok");
    }

    #[test]
    fn fts_round_trip_search_then_get() {
        let (_tmp, db) = db_path();
        let id_a = log_progress(
            &db,
            "auth refactor day 1",
            Some("Refactored the OAuth middleware to use the new token model."),
        )
        .unwrap();
        let _id_b = log_progress(
            &db,
            "lunch break notes",
            Some("Unrelated standup notes about lunch and the cafeteria."),
        )
        .unwrap();

        let hits = block_on(search(&db, None, "OAuth", 6)).unwrap();
        assert_eq!(hits.len(), 1, "hits = {hits:?}");
        assert_eq!(hits[0].id, id_a);
        assert_eq!(hits[0].summary, "auth refactor day 1");
        assert!(
            hits[0].snippet.contains("OAuth"),
            "snippet = {}",
            hits[0].snippet
        );
        assert!(hits[0].ts > 0);

        let entry = get(&db, id_a).unwrap();
        assert!(entry.found);
        assert_eq!(
            entry.detail.as_deref(),
            Some("Refactored the OAuth middleware to use the new token model.")
        );
    }

    #[test]
    fn search_ranks_more_relevant_first_via_bm25() {
        let (_tmp, db) = db_path();
        let id_two = log_progress(
            &db,
            "review alpha vs beta",
            Some("We chose the alpha approach over beta after the review."),
        )
        .unwrap();
        let id_one =
            log_progress(&db, "tiny note", Some("Logged a few alpha notes.")).unwrap();

        let hits = block_on(search(&db, None, "alpha beta", 6)).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, id_two, "two-term match must rank first");
        assert_eq!(hits[1].id, id_one);
    }

    #[test]
    fn search_returns_empty_for_query_with_no_searchable_terms() {
        let (_tmp, db) = db_path();
        log_progress(&db, "real entry", Some("Real content here.")).unwrap();
        assert_eq!(block_on(search(&db, None, "  !!!  ??? ", 6)).unwrap(), Vec::new());
    }

    #[test]
    fn search_returns_empty_on_fresh_db() {
        let (_tmp, db) = db_path();
        assert_eq!(block_on(search(&db, None, "anything", 6)).unwrap(), Vec::new());
    }

    #[test]
    fn search_respects_the_limit() {
        let (_tmp, db) = db_path();
        for n in 0..10 {
            log_progress(
                &db,
                &format!("entry {n}"),
                Some("shared keyword line one keyword line"),
            )
            .unwrap();
        }
        let hits = block_on(search(&db, None, "keyword", 3)).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn search_skips_rows_with_null_detail() {
        let (_tmp, db) = db_path();
        // summary-only rows are intentionally not full-text searchable.
        log_progress(&db, "keyword in summary only", None).unwrap();
        assert_eq!(block_on(search(&db, None, "keyword", 6)).unwrap(), Vec::new());
    }

    #[test]
    fn get_reports_not_found_for_unknown_id() {
        let (_tmp, db) = db_path();
        let entry = get(&db, 999).unwrap();
        assert!(!entry.found);
        assert_eq!(entry.id, 999);
        assert_eq!(entry.summary, "");
        assert_eq!(entry.detail, None);
    }

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_SEARCH_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(1)), 1);
        assert_eq!(clamp_limit(Some(9999)), MAX_SEARCH_LIMIT);
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 1 — schema versioning + provenance columns
    // -----------------------------------------------------------------

    /// Frozen copy of the v0 DDL — the schema as shipped at Phase 6 Step 0.
    /// Inlined here (not reused from `migrate`) so the upgrade-path tests
    /// reflect a *real* old-shaped DB even if `migrate` changes later.
    const V0_DDL_FROZEN: &str = "CREATE TABLE log (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        ts      INTEGER NOT NULL,
        summary TEXT    NOT NULL,
        detail  TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(
        detail,
        content='log',
        content_rowid='id'
    );
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;";

    fn read_user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn log_columns(conn: &Connection) -> std::collections::BTreeSet<String> {
        let mut stmt = conn.prepare("PRAGMA table_info(log)").unwrap();
        let names = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        names.map(|n| n.unwrap()).collect()
    }

    /// Helper for "log table's full column set at the **current** schema
    /// version". Name is historical — at S1 this was the v1 shape (6
    /// columns). S3-distill added a v4 step that appends `kind` (7), and
    /// the v8 consolidation step appends `superseded_by` (8), so this now
    /// returns 8 columns. All call sites still want "the set of cols
    /// fresh-opened code produces", so they keep working unchanged.
    fn expected_log_columns() -> std::collections::BTreeSet<String> {
        [
            "id",
            "ts",
            "summary",
            "detail",
            "origin",
            "project_hash",
            // S3-distill (v4) addition:
            "kind",
            // consolidation (v8) addition:
            "superseded_by",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }

    fn schema_object_exists(conn: &Connection, ty: &str, name: &str) -> bool {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                params![ty, name],
                |r| r.get(0),
            )
            .unwrap();
        n == 1
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct V0Row {
        id: i64,
        ts: i64,
        summary: String,
        detail: Option<String>,
    }

    /// Lay down a v0-shape DB at `db_path` and seed three rows: monotonic
    /// `ts`, distinct summaries, one `detail`-null + two distinct non-null
    /// details. Returns the seeded rows so the upgrade-path tests can
    /// byte-compare them.
    fn build_v0_db(db_path: &std::path::Path) -> Vec<V0Row> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V0_DDL_FROZEN).unwrap();
        // Explicit: this DB is at v0. (Default user_version is 0, but
        // setting it documents the intent so a future reader can grep.)
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();

        let seeds: [(i64, &str, Option<&str>); 3] = [
            (1001, "early note", None),
            (
                1002,
                "mid note about apricot",
                Some("Wrote thoughts about apricot harvests and bananas."),
            ),
            (
                1003,
                "late note about cherry",
                Some("A separate cherry-themed planning chunk."),
            ),
        ];
        let mut rows = Vec::new();
        for (ts, summary, detail) in seeds {
            conn.execute(
                "INSERT INTO log(ts, summary, detail) VALUES (?1, ?2, ?3)",
                params![ts, summary, detail],
            )
            .unwrap();
            rows.push(V0Row {
                id: conn.last_insert_rowid(),
                ts,
                summary: summary.to_string(),
                detail: detail.map(|s| s.to_string()),
            });
        }
        drop(conn);
        rows
    }

    // S1.A.1 — fresh DB has v1 products (origin/project_hash + idx_log_ts).
    // user_version lands on `SCHEMA_VERSION` (post-S2 = 2) — the v1 step
    // still runs as part of the full v0→v1→…→latest walk.
    #[test]
    fn s1_a1_fresh_db_lands_on_current_schema() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "table", "log_fts"));
        assert!(schema_object_exists(&conn, "trigger", "log_ai"));
        assert!(schema_object_exists(&conn, "trigger", "log_ad"));
        assert!(schema_object_exists(&conn, "trigger", "log_au"));
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
    }

    // S1.A.2 — fresh DB log_progress defaults origin='self', project_hash NULL.
    #[test]
    fn s1_a2_fresh_db_log_progress_sets_default_provenance() {
        let (_tmp, db) = db_path();
        let id = log_progress(&db, "first entry", Some("body")).unwrap();
        let conn = open(&db).unwrap();
        let (ts, summary, detail, origin, project_hash): (
            i64,
            String,
            Option<String>,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT ts, summary, detail, origin, project_hash \
                 FROM log WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert!(ts > 0);
        assert_eq!(summary, "first entry");
        assert_eq!(detail.as_deref(), Some("body"));
        assert_eq!(origin, "self");
        assert!(project_hash.is_none());
    }

    // S1.B.1 — open() three times is idempotent; user_version stays at
    // SCHEMA_VERSION across every reopen.
    #[test]
    fn s1_b1_open_thrice_is_idempotent_and_user_version_stays_at_target() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        }
    }

    // S1.B.2 — re-open a persisted DB preserves log rows and the version pin.
    #[test]
    fn s1_b2_reopen_existing_db_preserves_rows_and_version() {
        let (_tmp, db) = db_path();
        let id_a = log_progress(&db, "kept summary", Some("kept body")).unwrap();
        let id_b = log_progress(&db, "second kept", None).unwrap();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        let a = get(&db, id_a).unwrap();
        assert!(a.found);
        assert_eq!(a.summary, "kept summary");
        assert_eq!(a.detail.as_deref(), Some("kept body"));
        let b = get(&db, id_b).unwrap();
        assert!(b.found);
        assert_eq!(b.summary, "second kept");
        assert_eq!(b.detail, None);
    }

    // S1.C.1 — old v0 DB upgrades in place; columns, index, user_version,
    // and every old row's id/ts/summary/detail are preserved byte-for-byte;
    // origin defaults to 'self' and project_hash to NULL. Endpoint is
    // SCHEMA_VERSION (the full v0→v1→latest walk runs), but the v1
    // products (columns + idx_log_ts) are what this test verifies.
    #[test]
    fn s1_c1_upgrades_v0_db_to_v1_preserving_old_rows() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));

        let mut stmt = conn
            .prepare(
                "SELECT id, ts, summary, detail, origin, project_hash \
                 FROM log ORDER BY id",
            )
            .unwrap();
        let upgraded: Vec<(i64, i64, String, Option<String>, String, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(upgraded.len(), seeded.len());
        for (after, before) in upgraded.iter().zip(seeded.iter()) {
            assert_eq!(after.0, before.id);
            assert_eq!(after.1, before.ts);
            assert_eq!(after.2, before.summary);
            assert_eq!(after.3, before.detail);
            assert_eq!(after.4, "self");
            assert!(after.5.is_none());
        }
    }

    // S1.C.2 — after the v0→v1 upgrade, FTS still finds an old row's
    // detail. Guards against ADD COLUMN breaking external-content FTS5.
    #[test]
    fn s1_c2_fts_still_finds_old_rows_after_upgrade() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);
        drop(open(&db).unwrap());

        let hits = block_on(search(&db, None, "apricot", 6)).unwrap();
        assert_eq!(hits.len(), 1, "hits = {hits:?}");
        let expected = &seeded[1];
        assert_eq!(hits[0].id, expected.id);
        assert_eq!(hits[0].ts, expected.ts);
        assert_eq!(hits[0].summary, expected.summary);
        assert!(
            hits[0].snippet.contains("apricot"),
            "snippet = {}",
            hits[0].snippet
        );
    }

    // S1.C.3 — after the upgrade, log_progress writes a new row with
    // origin='self', project_hash NULL, and FTS surfaces both the old
    // and new row for a shared keyword (proves the INSERT trigger still
    // populates log_fts after ALTER TABLE ADD COLUMN).
    #[test]
    fn s1_c3_log_progress_after_upgrade_writes_provenance_and_fts() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);
        let new_id = log_progress(
            &db,
            "post-upgrade apricot follow-up",
            Some("Another apricot dive after the schema migration."),
        )
        .unwrap();

        let conn = open(&db).unwrap();
        let (origin, project_hash): (String, Option<String>) = conn
            .query_row(
                "SELECT origin, project_hash FROM log WHERE id = ?1",
                params![new_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, "self");
        assert!(project_hash.is_none());

        let hits = block_on(search(&db, None, "apricot", 6)).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        let old_apricot_id = seeded[1].id;
        assert!(
            ids.contains(&old_apricot_id),
            "expected old apricot row {old_apricot_id} in hits {ids:?}"
        );
        assert!(
            ids.contains(&new_id),
            "expected new apricot row {new_id} in hits {ids:?}"
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 2-schema — raw_thread + raw_event tables (v2)
    // -----------------------------------------------------------------

    /// Frozen copy of the v1 final form (v0 + provenance columns + ts index).
    /// Inlined here so the v1→v2 upgrade test reflects a real v1-shaped DB
    /// independently of whatever `migrate` does now.
    const V1_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(
        detail,
        content='log',
        content_rowid='id'
    );
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);";

    fn expected_raw_thread_columns() -> std::collections::BTreeSet<String> {
        [
            "thread_id",
            "agent_id",
            "team_id",
            "project_hash",
            "source",
            "parent_thread_id",
            "cwd",
            "source_path",
            "first_seen_ts",
            "last_ingest_ts",
            "last_offset",
            "last_line_no",
            // S3-schema additions:
            "last_distilled_line_no",
            "last_growth_ts",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }

    fn expected_raw_event_columns() -> std::collections::BTreeSet<String> {
        ["id", "thread_id", "line_no", "payload", "ingested_at"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn table_columns(conn: &Connection, table: &str) -> std::collections::BTreeSet<String> {
        let sql = format!("PRAGMA table_info({table})");
        let mut stmt = conn.prepare(&sql).unwrap();
        let names = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        names.map(|n| n.unwrap()).collect()
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct V1Row {
        id: i64,
        ts: i64,
        summary: String,
        detail: Option<String>,
        origin: String,
        project_hash: Option<String>,
    }

    /// Lay down a v1-shape DB at `db_path` (per `V1_DDL_FROZEN`) and seed
    /// three rows that mix default and non-default provenance values, so
    /// the v1→v2 upgrade test can verify every field survives byte-for-byte.
    fn build_v1_db(db_path: &std::path::Path) -> Vec<V1Row> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V1_DDL_FROZEN).unwrap();
        conn.execute_batch("PRAGMA user_version = 1;").unwrap();

        let seeds: [(i64, &str, Option<&str>, &str, Option<&str>); 3] = [
            (2001, "v1 self default", None, "self", None),
            (
                2002,
                "v1 inbox flagged",
                Some("Sourced from inbox tag."),
                "inbox",
                Some("abc123def456"),
            ),
            (
                2003,
                "v1 self with detail",
                Some("Plain self entry."),
                "self",
                None,
            ),
        ];
        let mut rows = Vec::new();
        for (ts, summary, detail, origin, project_hash) in seeds {
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin, project_hash) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, summary, detail, origin, project_hash],
            )
            .unwrap();
            rows.push(V1Row {
                id: conn.last_insert_rowid(),
                ts,
                summary: summary.to_string(),
                detail: detail.map(|s| s.to_string()),
                origin: origin.to_string(),
                project_hash: project_hash.map(|s| s.to_string()),
            });
        }
        drop(conn);
        rows
    }

    // S2s.A.1 — fresh DB lands on full v2 schema; v1 products still present.
    #[test]
    fn s2s_a1_fresh_db_lands_on_v2_schema_with_raw_tables() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        // v2 products
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v1 products still survive a fresh open()
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
    }

    // S2s.A.2 — raw_* column sets match spec; (thread_id, line_no) UNIQUE
    // is enforced — a duplicate (t, 1) insert must fail with a constraint
    // violation.
    #[test]
    fn s2s_a2_raw_tables_have_expected_columns_and_unique_constraint() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(table_columns(&conn, "raw_thread"), expected_raw_thread_columns());
        assert_eq!(table_columns(&conn, "raw_event"), expected_raw_event_columns());

        // Seed a parent thread row so the UNIQUE check has a valid FK.
        conn.execute(
            "INSERT INTO raw_thread(thread_id, source_path, first_seen_ts, last_ingest_ts) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["thr-1", "/tmp/rollout.jsonl", 0_i64, 0_i64],
        )
        .unwrap();

        // First (thr-1, 1) wins; second must fail.
        conn.execute(
            "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["thr-1", 1_i64, "{}", 0_i64],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params!["thr-1", 1_i64, "{}", 0_i64],
            )
            .unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unique") || msg.contains("constraint"),
            "expected UNIQUE violation, got: {err}"
        );
    }

    // S2s.B.1 — open() three times is idempotent on the v2 schema.
    #[test]
    fn s2s_b1_open_thrice_is_idempotent_at_v2() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(schema_object_exists(&conn, "table", "raw_thread"));
            assert!(schema_object_exists(&conn, "table", "raw_event"));
        }
    }

    // S2s.B.2 — seed 1 raw_thread + 2 raw_event on a v2 DB, close, reopen;
    // version + rows survive untouched.
    #[test]
    fn s2s_b2_reopen_v2_db_preserves_raw_rows_and_version() {
        let (_tmp, db) = db_path();
        {
            let conn = open(&db).unwrap();
            conn.execute(
                "INSERT INTO raw_thread(thread_id, agent_id, source_path, first_seen_ts, last_ingest_ts) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["thr-keep", "agent-a", "/tmp/keep.jsonl", 100_i64, 200_i64],
            )
            .unwrap();
            for line_no in 1..=2_i64 {
                conn.execute(
                    "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params!["thr-keep", line_no, format!("{{\"n\":{line_no}}}"), 200_i64],
                )
                .unwrap();
            }
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let (agent_id, source_path, first_seen_ts, last_ingest_ts): (
            Option<String>,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT agent_id, source_path, first_seen_ts, last_ingest_ts \
                 FROM raw_thread WHERE thread_id = ?1",
                params!["thr-keep"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(agent_id.as_deref(), Some("agent-a"));
        assert_eq!(source_path, "/tmp/keep.jsonl");
        assert_eq!(first_seen_ts, 100);
        assert_eq!(last_ingest_ts, 200);

        let mut stmt = conn
            .prepare(
                "SELECT line_no, payload FROM raw_event \
                 WHERE thread_id = ?1 ORDER BY line_no",
            )
            .unwrap();
        let events: Vec<(i64, String)> = stmt
            .query_map(params!["thr-keep"], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            events,
            vec![(1_i64, "{\"n\":1}".to_string()), (2_i64, "{\"n\":2}".to_string())]
        );
    }

    // S2s.C.1 — v1 DB upgrades to v2: every old log row survives byte-for-byte
    // (id/ts/summary/detail/origin/project_hash) and the v2 raw_* tables now
    // exist.
    #[test]
    fn s2s_c1_upgrades_v1_db_to_v2_preserving_log_rows_and_provenance() {
        let (_tmp, db) = db_path();
        let seeded = build_v1_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));

        let mut stmt = conn
            .prepare(
                "SELECT id, ts, summary, detail, origin, project_hash \
                 FROM log ORDER BY id",
            )
            .unwrap();
        let upgraded: Vec<V1Row> = stmt
            .query_map([], |r| {
                Ok(V1Row {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    summary: r.get(2)?,
                    detail: r.get(3)?,
                    origin: r.get(4)?,
                    project_hash: r.get(5)?,
                })
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(upgraded, seeded);
    }

    // S2s.C.2 — v0 DB walks the full v0→v1→v2 chain in one open(): every
    // old log row survives, origin defaults to 'self' / project_hash to
    // NULL (v1 effect), idx_log_ts exists (v1), raw_thread/raw_event exist
    // (v2).
    #[test]
    fn s2s_c2_upgrades_v0_db_through_v1_to_v2_in_one_open() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));

        let mut stmt = conn
            .prepare(
                "SELECT id, ts, summary, detail, origin, project_hash \
                 FROM log ORDER BY id",
            )
            .unwrap();
        let upgraded: Vec<(i64, i64, String, Option<String>, String, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(upgraded.len(), seeded.len());
        for (after, before) in upgraded.iter().zip(seeded.iter()) {
            assert_eq!(after.0, before.id);
            assert_eq!(after.1, before.ts);
            assert_eq!(after.2, before.summary);
            assert_eq!(after.3, before.detail);
            assert_eq!(after.4, "self");
            assert!(after.5.is_none());
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 2-ingest — rollout → raw_thread/raw_event scanner
    // -----------------------------------------------------------------

    const SAMPLE_THREAD_UUID: &str = "019e6725-269e-7bc1-b118-a1e757ee324c";
    const SAMPLE_TEAM_ID: &str = "team_07e1d699-e0b4-453f-8d29-ca6281b36f30";
    const SAMPLE_PROJECT_HASH: &str = "b854e7722178";
    const SAMPLE_ISO_TS: &str = "2026-05-27T09-55-48";

    fn ingest_test_env() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let memory_db = tmp.path().join("memory.db");
        let scan_root = tmp.path().join("team_sessions");
        std::fs::create_dir_all(&scan_root).unwrap();
        (tmp, memory_db, scan_root)
    }

    fn write_rollout(
        scan_root: &std::path::Path,
        team_id: &str,
        project_hash: &str,
        thread_uuid: &str,
        iso_ts: &str,
        content: &str,
    ) -> PathBuf {
        let dir = scan_root.join(team_id).join(project_hash);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-{iso_ts}-{thread_uuid}.jsonl"));
        std::fs::write(&path, content).unwrap();
        path
    }

    fn count_rows(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn fetch_event_payloads(conn: &Connection, thread_id: &str) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT line_no, payload FROM raw_event \
                 WHERE thread_id = ?1 ORDER BY line_no",
            )
            .unwrap();
        stmt.query_map(params![thread_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    // S2i.A.1 — parse_rollout_attribution: good sample + bad stems.
    #[test]
    fn s2i_a1_parse_rollout_attribution_real_sample_and_bad_stem() {
        let good = std::path::PathBuf::from(format!(
            "/scan/team_sessions/{SAMPLE_TEAM_ID}/{SAMPLE_PROJECT_HASH}/\
             rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.jsonl"
        ));
        let attr = parse_rollout_attribution(&good).expect("real sample must parse");
        assert_eq!(attr.thread_id, SAMPLE_THREAD_UUID);
        assert_eq!(attr.team_id, SAMPLE_TEAM_ID);
        assert_eq!(attr.project_hash, SAMPLE_PROJECT_HASH);

        // Bad: stem ends in a 36-char tail that is NOT UUID-shaped (no
        // dashes at positions 8/13/18/23).
        let bad_uuid_tail = "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX";
        assert_eq!(bad_uuid_tail.len(), 36);
        let bad_uuid = std::path::PathBuf::from(format!(
            "/scan/team_sessions/t/p/rollout-{SAMPLE_ISO_TS}-{bad_uuid_tail}.jsonl"
        ));
        assert_eq!(parse_rollout_attribution(&bad_uuid), None);

        // Bad: stem too short to even contain a 36-char tail.
        let too_short = std::path::PathBuf::from("/scan/team_sessions/t/p/rollout-foo.jsonl");
        assert_eq!(parse_rollout_attribution(&too_short), None);

        // Bad: no `team_sessions` in the path → no attribution.
        let no_team = std::path::PathBuf::from(format!(
            "/sessions/2026/05/21/rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.jsonl"
        ));
        assert_eq!(parse_rollout_attribution(&no_team), None);

        // Bad: extension is not .jsonl.
        let not_jsonl = std::path::PathBuf::from(format!(
            "/scan/team_sessions/t/p/rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.txt"
        ));
        assert_eq!(parse_rollout_attribution(&not_jsonl), None);
    }

    // S2i.A.2 — fresh DB + 1 file with N>=3 lines: 1 raw_thread with all
    // attribution fields correct, last_line_no=N, last_offset=filesize;
    // N raw_event rows with line_no 1..N and payloads byte-equal to source
    // lines.
    #[test]
    fn s2i_a2_first_pass_writes_thread_and_events_byte_for_byte() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_a2";
        let content = "line1\nline2\nline3\n"; // 18 bytes, 3 lines
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            content,
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();

        assert_eq!(stats.files_seen, 1);
        assert_eq!(stats.threads_touched, 1);
        assert_eq!(stats.events_inserted, 3);

        let (tid, aid, tmid, ph, sp, lln, loff): (
            String,
            String,
            Option<String>,
            Option<String>,
            String,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT thread_id, agent_id, team_id, project_hash, source_path, \
                        last_line_no, last_offset \
                 FROM raw_thread",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(tid, SAMPLE_THREAD_UUID);
        assert_eq!(aid, agent_id);
        assert_eq!(tmid.as_deref(), Some(SAMPLE_TEAM_ID));
        assert_eq!(ph.as_deref(), Some(SAMPLE_PROJECT_HASH));
        assert_eq!(sp, rollout_path.to_string_lossy());
        assert_eq!(lln, 3);
        assert_eq!(loff, content.len() as i64);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![
                (1, "line1".to_string()),
                (2, "line2".to_string()),
                (3, "line3".to_string()),
            ]
        );
    }

    // S2i.B.1 — second ingest with no file change is a complete no-op
    // (events_inserted == 0, cursor frozen, raw_event count unchanged).
    #[test]
    fn s2i_b1_second_pass_unchanged_file_is_a_noop() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_b1";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc\n",
        );
        let conn = open(&memory_db).unwrap();
        let first = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(first.events_inserted, 3);

        let (off1, ln1): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.files_seen, 1);
        let (off2, ln2): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(off1, off2);
        assert_eq!(ln1, ln2);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_event"), 3);
    }

    // S2i.B.2 — append M lines, re-ingest: exactly M new raw_event rows
    // (line_no N+1..N+M), old payloads untouched, cursor advanced to new EOF.
    #[test]
    fn s2i_b2_append_then_ingest_picks_up_only_new_lines() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_b2";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "first\nsecond\n",
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();

        // Append 2 more lines.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .unwrap();
        f.write_all(b"third\nfourth\n").unwrap();
        drop(f);

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 2);
        assert_eq!(second.threads_touched, 1);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![
                (1, "first".to_string()),
                (2, "second".to_string()),
                (3, "third".to_string()),
                (4, "fourth".to_string()),
            ]
        );
        let (off, ln): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let new_size = std::fs::metadata(&rollout_path).unwrap().len() as i64;
        assert_eq!(off, new_size);
        assert_eq!(ln, 4);
    }

    // S2i.C.1 — two files with distinct team_id/project_hash/thread_uuid:
    // 2 raw_thread rows, each with its own event set, no cross-contamination.
    #[test]
    fn s2i_c1_two_distinct_threads_each_get_their_own_rows() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_c1";
        let thread_a = "11111111-2222-3333-4444-555555555555";
        let team_a = "team_aaaaaaaa-1111-1111-1111-111111111111";
        let hash_a = "aaaaaaaaaaaa";
        let thread_b = "66666666-7777-8888-9999-aaaaaaaaaaaa";
        let team_b = "team_bbbbbbbb-2222-2222-2222-222222222222";
        let hash_b = "bbbbbbbbbbbb";
        write_rollout(&scan_root, team_a, hash_a, thread_a, "2026-05-27T01-00-00", "A1\nA2\nA3\n");
        write_rollout(&scan_root, team_b, hash_b, thread_b, "2026-05-27T02-00-00", "B1\nB2\n");

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.threads_touched, 2);
        assert_eq!(stats.events_inserted, 5);

        let threads: Vec<(String, Option<String>, Option<String>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT thread_id, team_id, project_hash FROM raw_thread ORDER BY thread_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            threads,
            vec![
                (thread_a.to_string(), Some(team_a.to_string()), Some(hash_a.to_string())),
                (thread_b.to_string(), Some(team_b.to_string()), Some(hash_b.to_string())),
            ]
        );

        let a_payloads: Vec<String> = fetch_event_payloads(&conn, thread_a)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(a_payloads, vec!["A1", "A2", "A3"]);
        let b_payloads: Vec<String> = fetch_event_payloads(&conn, thread_b)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(b_payloads, vec!["B1", "B2"]);
    }

    // S2i.C.2 — a "sub-agent" rollout (same agent dir, same filename shape,
    // different thread uuid) is ingested like any other; `source` stays NULL
    // because we don't parse payload at this stage.
    #[test]
    fn s2i_c2_subagent_rollout_ingests_with_null_source_column() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_c2";
        // Parent thread
        let parent_uuid = SAMPLE_THREAD_UUID;
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            parent_uuid,
            "2026-05-27T09-55-48",
            "parent line 1\nparent line 2\n",
        );
        // Sub-agent thread (same team/project_hash dir, different uuid + ts)
        let sub_uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            sub_uuid,
            "2026-05-27T09-56-00",
            "sub line 1\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.threads_touched, 2);
        assert_eq!(stats.events_inserted, 3);

        // `source` is NULL for both — we never wrote it.
        let null_source_count: i64 = count_rows(
            &conn,
            "SELECT count(*) FROM raw_thread WHERE source IS NULL",
        );
        assert_eq!(null_source_count, 2);

        // Sub-agent's event made it in.
        assert_eq!(
            fetch_event_payloads(&conn, sub_uuid),
            vec![(1, "sub line 1".to_string())]
        );
    }

    // S2if.C.1 — shrink: truncate to < last_offset, re-ingest →
    // DELETE all raw_event rows for this thread (in the same IMMEDIATE
    // tx) then re-scan from offset 0. Post-shrink, raw_event matches
    // the file's current content exactly; no orphan rows survive.
    //
    // Replaces the pre-S2if-fix s2i_d1 test, which (correctly for the
    // earlier UNIQUE+IGNORE-only semantics) accepted orphans for line_nos
    // 4 and 5. S2if-fix tightened shrink semantics to "no orphans" — see
    // the shrink branch in `ingest_one_file_in_tx`.
    #[test]
    fn s2if_c1_shrink_purges_orphans_and_re_reads_from_zero() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_shrink";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "x1\nx2\nx3\nx4\nx5\n", // 5 lines, 15 bytes
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_event"), 5);

        // Truncate to first 3 lines (9 bytes < 15 bytes).
        std::fs::write(&rollout_path, "x1\nx2\nx3\n").unwrap();

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        // Shrink path DELETEs all rows then re-inserts 1..3, so 3 inserts.
        assert_eq!(second.events_inserted, 3);

        // raw_event for this thread now matches the file exactly:
        // 3 rows, no orphans for line_nos 4 or 5.
        let payloads = fetch_event_payloads(&conn, SAMPLE_THREAD_UUID);
        assert_eq!(
            payloads,
            vec![
                (1, "x1".to_string()),
                (2, "x2".to_string()),
                (3, "x3".to_string()),
            ]
        );
        assert_eq!(
            count_rows(&conn, "SELECT count(*) FROM raw_event"),
            3,
            "orphan rows for line_no 4 and 5 must be deleted by the shrink path"
        );

        // Cursor lands on the shrunk EOF.
        let (off, ln): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(off, 9);
        assert_eq!(ln, 3);
    }

    // S2i.D.2 — bad filename matching the glob: skipped, ingest pass still
    // succeeds for sibling well-formed files.
    #[test]
    fn s2i_d2_bad_filename_is_skipped_without_failing_the_pass() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_test_d2";
        let team_dir = scan_root.join(SAMPLE_TEAM_ID).join(SAMPLE_PROJECT_HASH);
        std::fs::create_dir_all(&team_dir).unwrap();
        // Bad: 36-char tail of stem is not UUID-shaped.
        std::fs::write(
            team_dir.join("rollout-2026-05-27T09-55-48-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX.jsonl"),
            "bogus\nbogus\n",
        )
        .unwrap();
        // Good neighbour.
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "ok\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        // Only the well-formed file got attributed → 1 thread, 1 event.
        assert_eq!(stats.threads_touched, 1);
        assert_eq!(stats.events_inserted, 1);
        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "ok".to_string())]
        );
    }

    // S2i.D.3 — per-agent isolation: a well-formed rollout placed in a
    // *different* agent's directory tree (outside this agent's scan_root)
    // is invisible to this agent's ingester.
    #[test]
    fn s2i_d3_other_agents_rollouts_outside_scan_root_are_invisible() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_db = tmp.path().join("memory.db");
        // Our agent's scan_root.
        let my_scan_root = tmp.path().join("agents/agent_me/team_sessions");
        std::fs::create_dir_all(&my_scan_root).unwrap();
        // Another agent's identical layout, NOT under my scan_root.
        let other_scan_root = tmp.path().join("agents/agent_other/team_sessions");
        std::fs::create_dir_all(&other_scan_root).unwrap();
        write_rollout(
            &other_scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "other agent line\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &my_scan_root, "agent_me").unwrap();
        assert_eq!(stats.files_seen, 0);
        assert_eq!(stats.threads_touched, 0);
        assert_eq!(stats.events_inserted, 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_thread"), 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_event"), 0);
    }

    // S2i.D.4 — legacy `sessions/YYYY/MM/DD/` layout is excluded by
    // construction (it's not under scan_root, and even if you point
    // scan_root at it, the path has no `team_sessions` segment so
    // parse_rollout_attribution returns None).
    #[test]
    fn s2i_d4_legacy_sessions_layout_is_not_ingested() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_db = tmp.path().join("memory.db");
        let my_scan_root = tmp.path().join("agents/agent_me/team_sessions");
        std::fs::create_dir_all(&my_scan_root).unwrap();
        // Legacy date-bucket layout, sitting outside any team_sessions tree.
        let legacy_dir = tmp.path().join("sessions/2026/05/21");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join(format!("rollout-{SAMPLE_ISO_TS}-{SAMPLE_THREAD_UUID}.jsonl")),
            "legacy content\n",
        )
        .unwrap();

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &my_scan_root, "agent_me").unwrap();
        assert_eq!(stats.files_seen, 0);
        assert_eq!(stats.events_inserted, 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM raw_thread"), 0);
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 2-ingest-fix — torn line / UTF-8 panic / shrink orphan
    // -----------------------------------------------------------------

    // S2if.A.1 — torn line at EOF: the trailing line "c" (no `\n`) must
    // NOT be inserted and the cursor must NOT advance past the last
    // committed `\n`.
    #[test]
    fn s2if_a1_torn_line_at_eof_is_not_committed() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc", // 5 bytes; last line has no trailing \n
        );
        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, "agent_a1").unwrap();
        assert_eq!(stats.events_inserted, 2);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "a".to_string()), (2, "b".to_string())]
        );

        let (off, ln): (i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // 4 bytes = "a\nb\n" — the position right after the last \n we saw.
        // "c" lives at bytes 4..5 and is NOT committed.
        assert_eq!(off, 4);
        assert_eq!(ln, 2);
    }

    // S2if.A.2 — the torn-line writer finishes: the previously-uncommitted
    // "c" gets completed to "cX\n" and a fresh "d\n" is appended. After
    // a second ingest the table has exactly four rows, AND `line_no=3`
    // holds the full "cX" — proving pass 1 did not split the half-line
    // across two raw_event rows.
    #[test]
    fn s2if_a2_completed_torn_line_ingests_as_one_row() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc",
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, "agent_a2").unwrap();

        // Writer completes the torn last line and adds one more.
        std::fs::write(&rollout_path, "a\nb\ncX\nd\n").unwrap();

        let second = ingest_once(&conn, &scan_root, "agent_a2").unwrap();
        assert_eq!(second.events_inserted, 2);

        let payloads = fetch_event_payloads(&conn, SAMPLE_THREAD_UUID);
        assert_eq!(
            payloads,
            vec![
                (1, "a".to_string()),
                (2, "b".to_string()),
                (3, "cX".to_string()),
                (4, "d".to_string()),
            ],
            "line 3 must be the FULL completed 'cX', not the half-line 'c'"
        );
    }

    // S2if.B.1 — parse_rollout_attribution must never panic on a stem
    // where the `len - 36` byte position falls inside a multi-byte UTF-8
    // codepoint. Construct a stem with a 4-byte emoji (🦀) straddling
    // that exact boundary and assert `None` is returned.
    #[test]
    fn s2if_b1_parse_rollout_attribution_does_not_panic_on_utf8_boundary() {
        let prefix = "rollout-2026-05-27T09-55-48-";
        assert_eq!(prefix.len(), 28);
        // Total stem byte length = 28 (prefix) + 4 (🦀) + 35 ('x') = 67.
        // tail_start = 67 - 36 = 31 — the FOURTH byte of 🦀, a UTF-8
        // continuation byte. `&stem[31..]` would panic without the
        // `is_char_boundary` guard.
        let suffix = "x".repeat(35);
        let bad_name = format!("{prefix}🦀{suffix}.jsonl");
        let stem_len = bad_name.len() - ".jsonl".len();
        assert_eq!(stem_len, 67);
        // Sanity: confirm the boundary check is the one that fires.
        let stem = &bad_name[..stem_len];
        assert!(!stem.is_char_boundary(stem_len - 36));

        let path = std::path::PathBuf::from(format!(
            "/scan/team_sessions/t/p/{bad_name}"
        ));
        assert_eq!(parse_rollout_attribution(&path), None);
    }

    // S2if.B.2 — placing the same UTF-8-boundary-bad file in scan_root
    // alongside a well-formed neighbour: ingest_once must not panic, the
    // bad file is skipped, the neighbour is ingested.
    #[test]
    fn s2if_b2_ingest_skips_utf8_named_file_and_keeps_going() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_b2";

        // UTF-8-boundary-bad file in its own team/hash directory.
        let bad_dir = scan_root.join("team_emoji").join("hash_emoji");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let prefix = "rollout-2026-05-27T09-55-48-";
        let suffix = "x".repeat(35);
        let bad_name = format!("{prefix}🦀{suffix}.jsonl");
        std::fs::write(bad_dir.join(&bad_name), "bogus content\n").unwrap();

        // Well-formed neighbour.
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "ok1\nok2\n",
        );

        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.threads_touched, 1);
        assert_eq!(stats.events_inserted, 2);

        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "ok1".to_string()), (2, "ok2".to_string())]
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-schema — distill cursor + growth_ts (v3 migration)
    // -----------------------------------------------------------------

    /// Frozen copy of the v2 final form: log v1 (provenance cols + idx_log_ts)
    /// + raw_thread (old 12 cols, pre-S3) + raw_event + idx_raw_event_thread.
    /// Used by the v2→v3 upgrade test so it doesn't depend on a live migrate.
    const V2_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(
        detail,
        content='log',
        content_rowid='id'
    );
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);";

    /// Lay down a v2-shape DB at `db_path`. Caller seeds rows via direct SQL.
    fn build_v2_db(db_path: &std::path::Path) {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V2_DDL_FROZEN).unwrap();
        conn.execute_batch("PRAGMA user_version = 2;").unwrap();
    }

    // S3s.A.1 — fresh DB lands on v3 with the two new distill-cursor columns,
    // and every v1/v2 product is still present.
    #[test]
    fn s3s_a1_fresh_db_lands_on_v3_with_distill_cursor_columns() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let cols = table_columns(&conn, "raw_thread");
        assert!(cols.contains("last_distilled_line_no"));
        assert!(cols.contains("last_growth_ts"));
        assert_eq!(cols.len(), 14, "raw_thread should be 14 columns at v3");

        // v2 products still present.
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v1 products still present.
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
    }

    // S3s.A.2 — column set equality + defaults check: a minimal insert
    // leaves `last_distilled_line_no = 0` and `last_growth_ts = NULL`.
    #[test]
    fn s3s_a2_minimal_insert_uses_v3_column_defaults() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        assert_eq!(table_columns(&conn, "raw_thread"), expected_raw_thread_columns());

        conn.execute(
            "INSERT INTO raw_thread(thread_id, source_path, first_seen_ts, last_ingest_ts) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["minimal-thr", "/tmp/r.jsonl", 0_i64, 0_i64],
        )
        .unwrap();

        let (last_distilled, last_growth): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_distilled_line_no, last_growth_ts FROM raw_thread \
                 WHERE thread_id = 'minimal-thr'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_distilled, 0);
        assert!(last_growth.is_none());
    }

    // S3s.B.1 — open() three times on a fresh DB: each open lands at v3
    // and migration arm 3 is idempotent (no duplicate-column crash).
    #[test]
    fn s3s_b1_open_thrice_is_idempotent_at_v3() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(schema_object_exists(&conn, "table", "raw_thread"));
        }
    }

    // S3s.B.2 — v2 DB upgrades to v3: a pre-existing raw_thread row with
    // only the old 12 columns populated survives the ALTERs; its v3
    // columns pick up the schema defaults (0 / NULL).
    #[test]
    fn s3s_b2_upgrades_v2_db_to_v3_preserving_raw_thread_rows() {
        let (_tmp, db) = db_path();
        build_v2_db(&db);

        // Seed an old-shape (12-col) raw_thread row before the upgrade.
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO raw_thread \
                 (thread_id, agent_id, team_id, project_hash, source, \
                  parent_thread_id, cwd, source_path, first_seen_ts, \
                  last_ingest_ts, last_offset, last_line_no) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    "thr-v2-old",
                    "agent-vintage",
                    "team-vintage",
                    "proj-hash",
                    "vscode",
                    None::<&str>,
                    "/tmp/cwd",
                    "/tmp/rollout.jsonl",
                    100_i64,
                    200_i64,
                    50_i64,
                    5_i64,
                ],
            )
            .unwrap();
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let (last_distilled, last_growth): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_distilled_line_no, last_growth_ts FROM raw_thread \
                 WHERE thread_id = 'thr-v2-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_distilled, 0);
        assert!(last_growth.is_none());

        // 12 original columns byte-for-byte preserved.
        let (
            agent_id,
            team_id,
            project_hash,
            source,
            parent_thread_id,
            cwd,
            source_path,
            first_seen_ts,
            last_ingest_ts,
            last_offset,
            last_line_no,
        ): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            i64,
            i64,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT agent_id, team_id, project_hash, source, parent_thread_id, \
                        cwd, source_path, first_seen_ts, last_ingest_ts, \
                        last_offset, last_line_no \
                 FROM raw_thread WHERE thread_id = 'thr-v2-old'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(agent_id, "agent-vintage");
        assert_eq!(team_id.as_deref(), Some("team-vintage"));
        assert_eq!(project_hash.as_deref(), Some("proj-hash"));
        assert_eq!(source.as_deref(), Some("vscode"));
        assert!(parent_thread_id.is_none());
        assert_eq!(cwd.as_deref(), Some("/tmp/cwd"));
        assert_eq!(source_path, "/tmp/rollout.jsonl");
        assert_eq!(first_seen_ts, 100);
        assert_eq!(last_ingest_ts, 200);
        assert_eq!(last_offset, 50);
        assert_eq!(last_line_no, 5);
    }

    // S3s.B.3 — full v0 → v1 → v2 → v3 chain in one open(): every step's
    // products land, the seeded v0 log rows survive.
    #[test]
    fn s3s_b3_upgrades_v0_db_through_full_chain_to_v3() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        // v1 products
        assert_eq!(log_columns(&conn), expected_log_columns());
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
        // v2 products
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v3 products: raw_thread is the 14-column v3 shape
        assert_eq!(table_columns(&conn, "raw_thread"), expected_raw_thread_columns());

        // Old log rows preserved.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, seeded.len());
    }

    // ---- S4schema (v6) — log_vec re-dimension 4096 → 1024 (BAAI/bge-m3) ----

    // Cumulative v5 schema snapshot (log through v4 `kind`, raw_thread through
    // v3, raw_event, log_fts + triggers, log_vec float[4096]). Frozen so the
    // v5→v6 step can be exercised against the exact shape a real v5 DB had.
    const V5_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT,
        kind         TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(detail, content='log', content_rowid='id');
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts   INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);
    CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[4096]);";

    fn log_vec_ddl(conn: &Connection) -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='log_vec'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    // S4schema.D.1 — v5→v6 drops+recreates log_vec at 1024; v0→…→v6 full chain
    // also lands log_vec at 1024. Confirms DROP+CREATE vec0 runs inside the
    // IMMEDIATE migration tx, and the rebuilt table is functional.
    #[test]
    fn s4schema_d1_v6_redimensions_log_vec_to_1024() {
        // Pins the s4 log_vec (1024-dim) schema at the *current* SCHEMA_VERSION
        // — not a literal version number (those rot on every bump; the real
        // invariant is read_user_version == SCHEMA_VERSION, asserted after
        // migrate below). v7 only touches log_fts, so log_vec stays 1024 here.
        // vec0 is registered process-globally inside backend::open(); this test
        // lays a frozen v5 schema (which has a vec0 table) via a RAW Connection
        // before any open(), so register the module explicitly to avoid a
        // test-ordering flake ("no such module: vec0").
        ensure_vec_extension();

        // (a) v5 → v6.
        {
            let (_tmp, db) = db_path();
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            {
                let conn = Connection::open(&db).unwrap();
                let _: String = conn
                    .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                    .unwrap();
                conn.execute_batch(V5_DDL_FROZEN).unwrap();
                conn.execute_batch("PRAGMA user_version = 5;").unwrap();
                assert!(log_vec_ddl(&conn).contains("float[4096]"), "v5 precondition");
                assert_eq!(read_user_version(&conn), 5); // laid-down v5 DB, pre-migration
            }
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            let ddl = log_vec_ddl(&conn);
            assert!(
                ddl.contains("float[1024]") && !ddl.contains("4096"),
                "post-v6 log_vec must be 1024: {ddl}"
            );
            // Functional: the rebuilt 1024-dim table accepts a row.
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (1, 's', 'd', 'self')",
                [],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            let v = vec![0.0_f32; 1024];
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_to_match_json(&v)],
            )
            .unwrap();
            // Pre-v5 products survived the in-place migration.
            assert!(schema_object_exists(&conn, "table", "raw_event"));
        }

        // (b) v0 → … → v6 full chain ends with log_vec at 1024.
        {
            let (_tmp, db) = db_path();
            build_v0_db(&db);
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(log_vec_ddl(&conn).contains("float[1024]"), "v0→v6 chain log_vec dim");
        }
    }

    // ---- S5schema (v7) — log_fts re-tokenized to FTS5 `trigram` (CJK lexical) ----

    // Cumulative v6 schema snapshot: v5 shape but log_vec at float[1024], and
    // log_fts still on the DEFAULT (unicode61) tokenizer (trigram arrives in v7).
    const V6_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT,
        kind         TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(detail, content='log', content_rowid='id');
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts   INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);
    CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[1024]);";

    fn log_fts_ddl(conn: &Connection) -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='log_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    // T1 — v6→v7: log_fts re-tokenized to trigram, and pre-existing rows are
    // re-searchable (the 'rebuild' actually repopulated the new index).
    #[test]
    fn v6_to_v7_migrate() {
        let (_tmp, db) = db_path();
        ensure_vec_extension(); // V6_DDL_FROZEN has a vec0 table (raw open)
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
            conn.execute_batch(V6_DDL_FROZEN).unwrap();
            conn.execute_batch("PRAGMA user_version = 6;").unwrap();
            // Seed rows under the OLD unicode61 log_fts (one zh, one en).
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (1, 's1', ?1, 'self')",
                params!["我们的会话状态最终选 Postgres 存储方案"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (2, 's2', ?1, 'self')",
                params!["an english oauth note about tokens"],
            )
            .unwrap();
            assert!(!log_fts_ddl(&conn).contains("trigram"), "v6 precondition: unicode61");
            assert_eq!(read_user_version(&conn), 6);
        }
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(log_fts_ddl(&conn).contains("trigram"), "post-v7 log_fts must be trigram");
        // Rebuild evidence: the pre-existing English row is FTS-retrievable.
        let en = fts_ranked_ids(&conn, &build_fts_match("oauth").unwrap(), 50).unwrap();
        assert!(!en.is_empty(), "rebuild must re-index pre-existing rows; en hits = {en:?}");
        // And the Chinese row is now CJK-substring searchable (0 under unicode61).
        let zh = fts_ranked_ids(&conn, &build_fts_match("会话状态用什么存").unwrap(), 50).unwrap();
        eprintln!("[T1] post-v7 zh FTS hits = {zh:?} (Chinese row recalled via detail 子串)");
        assert!(!zh.is_empty(), "trigram + rebuild: Chinese detail must be FTS-recallable");
    }

    // T2 — v0 → … → v9 full chain ends at v9 with all core products + trigram
    // + 1024 + the consolidation (v8) objects + the v9 consolidation cursor.
    #[test]
    fn v0_to_v9_full_chain() {
        let (_tmp, db) = db_path();
        build_v0_db(&db);
        let conn = open(&db).unwrap();
        // read_user_version == SCHEMA_VERSION is the invariant (currently 9).
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "log_vec"));
        assert!(log_fts_ddl(&conn).contains("trigram"), "v0→v9 chain: log_fts trigram");
        assert!(log_vec_ddl(&conn).contains("float[1024]"), "v0→v9 chain: log_vec 1024");
        // consolidation (v8) products:
        assert!(
            log_columns(&conn).contains("superseded_by"),
            "v0→v9 chain: log.superseded_by"
        );
        assert!(
            schema_object_exists(&conn, "table", "log_contradiction"),
            "v0→v9 chain: log_contradiction"
        );
        assert!(
            schema_object_exists(&conn, "table", "log_consolidation_audit"),
            "v0→v9 chain: log_consolidation_audit"
        );
        // audit carries the judge's supersede direction (superseded_id).
        let audit_cols: std::collections::BTreeSet<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(log_consolidation_audit)")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(
            audit_cols.contains("superseded_id"),
            "v0→v9 chain: audit must carry superseded_id; cols={audit_cols:?}"
        );
        // consolidation loop cursor (v9): the singleton row exists, seeded at 0.
        assert!(
            schema_object_exists(&conn, "table", "consolidation_state"),
            "v0→v9 chain: consolidation_state"
        );
        let (cnt, max_id): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(MIN(last_consolidation_max_log_id), -1) FROM consolidation_state",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((cnt, max_id), (1, 0), "v0→v9 chain: one consolidation_state row seeded at 0");
    }

    // T3 — re-open a v7 DB: migrate is a no-op, log_fts stays trigram, version stable.
    #[test]
    fn v7_migrate_idempotent() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(log_fts_ddl(&conn).contains("trigram"), "log_fts stays trigram across reopens");
        }
    }

    // T4 — Design B gate: build_fts_match makes a CJK query lexically matchable
    // via DETAIL substrings (summary is NOT FTS-indexed), AND bounded
    // segmentation does not over-recall.
    #[test]
    fn fts_match_cjk_substring() {
        let (_tmp, db) = db_path();
        // NOTE: log_fts indexes only `detail` → the Chinese to be matched MUST
        // live in `detail` (summary中文 is invisible to FTS).
        let target = log_progress(
            &db,
            "会话存储选型",
            Some("我们的会话状态要持久化，最终选 Postgres 存储"),
        )
        .unwrap();
        // Precision noise #1 (semantic neighbor, no query chunk): 状态机, not 会话状态.
        let noise_word = log_progress(
            &db,
            "状态机",
            Some("状态机设计采用事件驱动模式"),
        )
        .unwrap();
        // Precision noise #2 (THE Design-B discriminator): contains the query
        // 3-gram "状态用" but NOT the bounded chunk "会话状态"/"用什么存". A brute
        // per-char 3-gram sweep WOULD recall this; bounded segmentation must NOT.
        let noise_trigram = log_progress(
            &db,
            "监控看板",
            Some("性能状态用量监控看板，与本主题无关"),
        )
        .unwrap();

        let conn = open(&db).unwrap();
        let expr = build_fts_match("会话状态用什么存").expect("CJK query → MATCH expr");
        eprintln!("[T4] build_fts_match(\"会话状态用什么存\") = {expr}");
        let hits = fts_ranked_ids(&conn, &expr, 50).unwrap();
        eprintln!(
            "[T4] FTS hits = {hits:?}  (target={target}, noise_word={noise_word}, noise_trigram={noise_trigram})"
        );
        assert!(hits.contains(&target), "RECALL: target row matched via detail 中文子串");
        assert!(!hits.contains(&noise_word), "PRECISION: 状态机设计 row must NOT match");
        assert!(
            !hits.contains(&noise_trigram),
            "PRECISION (Design-B gate): row sharing only the 3-gram 状态用 must NOT match — bounded segmentation, not a per-char 3-gram sweep"
        );
    }

    // T5 — English under trigram becomes SUBSTRING matching (behavior change
    // from unicode61 word-matching; recorded intentionally).
    #[test]
    fn fts_match_english_substring() {
        let (_tmp, db) = db_path();
        let a = log_progress(&db, "a", Some("we are testing the parser")).unwrap(); // testing ⊃ test
        let b = log_progress(&db, "b", Some("won the contest yesterday")).unwrap(); // contest ⊃ test
        let c = log_progress(&db, "c", Some("completely unrelated body")).unwrap(); // no "test"
        let conn = open(&db).unwrap();
        let expr = build_fts_match("test").expect("english query");
        eprintln!("[T5] build_fts_match(\"test\") = {expr}");
        let hits = fts_ranked_ids(&conn, &expr, 50).unwrap();
        eprintln!("[T5] FTS hits = {hits:?}  (testing={a}, contest={b}, unrelated={c})");
        assert!(hits.contains(&a) && hits.contains(&b), "trigram: 'test' substring-matches testing & contest");
        assert!(!hits.contains(&c), "unrelated row not matched");
    }

    // T6 — observe (not a pass/fail gate): trigram's >=3-char floor means a
    // 2-char CJK query is unmatchable → no FTS term → vector path is the backstop.
    #[test]
    fn fts_match_short_cjk_observe() {
        let two = build_fts_match("缓存");
        eprintln!("[T6] build_fts_match(\"缓存\") = {two:?}  (None ⇒ no FTS term; vector backstop)");
        assert!(two.is_none(), "2-char CJK query has no trigram-matchable term (>=3 floor)");
        let three = build_fts_match("缓存层");
        eprintln!("[T6] build_fts_match(\"缓存层\") = {three:?}  (3 chars ⇒ matchable)");
        assert!(three.is_some(), "3-char CJK query is trigram-matchable");
    }

    // ---- consolidation (v8) — superseded_by + contradiction/audit tables ----
    //
    // Pure-schema step (see the v8 arm in `apply_migration_step`). These tests
    // prove the three new objects appear on upgrade and that NOTHING about the
    // existing write/search behaviour changes. Naming: the "S5schema" label
    // above is the v7 (trigram) schema-chunk; this v8 work is the feature-ladder
    // "S5" but is anchored on the version + "consolidation", never an S-number
    // (the two series are off by one — see the v8 migration-arm comment).

    // Cumulative v7 schema snapshot — V6_DDL_FROZEN copied verbatim with the
    // ONE delta the v7 migration applies: log_fts gains `tokenize='trigram'`
    // (log_vec was already float[1024] from the v6 bge-m3 rebuild). This is the
    // from-state for the v7→v8 migrate test; keeping it a one-line diff from V6
    // minimises the chance of a hand-typed frozen constant drifting off the real
    // shipped v7 shape (which would let v7→v8 start from a wrong v7 and falsely pass).
    const V7_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT,
        kind         TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(detail, content='log', content_rowid='id', tokenize='trigram');
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts   INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);
    CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[1024]);";

    // C1 — v7→v8: the three consolidation objects appear, the new column
    // defaults NULL on pre-existing rows, and retrieval survives the ADD COLUMN
    // (adding a column to an external-content FTS5 base table must not break the
    // index — the seeded row stays FTS-recallable).
    #[test]
    fn v7_to_v8_migrate() {
        let (_tmp, db) = db_path();
        ensure_vec_extension(); // V7_DDL_FROZEN has a vec0 table (raw open)
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let seeded_id: i64;
        {
            let conn = Connection::open(&db).unwrap();
            let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
            conn.execute_batch(V7_DDL_FROZEN).unwrap();
            conn.execute_batch("PRAGMA user_version = 7;").unwrap();
            // Seed one row under the v7 shape (FTS-indexed via the log_ai trigger).
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (1, 's1', ?1, 'self')",
                params!["an english oauth note about tokens"],
            )
            .unwrap();
            seeded_id = conn.last_insert_rowid();
            assert_eq!(read_user_version(&conn), 7);
            // Precondition: no v8 objects yet.
            assert!(!log_columns(&conn).contains("superseded_by"));
            assert!(!schema_object_exists(&conn, "table", "log_contradiction"));
            assert!(!schema_object_exists(&conn, "table", "log_consolidation_audit"));
        }
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        // All three v8 objects exist post-migrate.
        assert!(log_columns(&conn).contains("superseded_by"), "v8 adds log.superseded_by");
        assert!(schema_object_exists(&conn, "table", "log_contradiction"));
        assert!(schema_object_exists(&conn, "table", "log_consolidation_audit"));
        // ADD COLUMN is inert for the existing row: superseded_by defaults NULL.
        let sb: Option<i64> = conn
            .query_row(
                "SELECT superseded_by FROM log WHERE id = ?1",
                params![seeded_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sb.is_none(), "pre-existing row's superseded_by must default NULL");
        // Retrieval survives the column add: the seeded English row is still FTS-hit.
        let hits = fts_ranked_ids(&conn, &build_fts_match("oauth").unwrap(), 50).unwrap();
        assert!(
            hits.contains(&seeded_id),
            "schema add must not break FTS retrieval; hits = {hits:?}"
        );
    }

    // C2 — re-open a v8 DB repeatedly: migrate is a no-op, version + all v8
    // objects stay put (idempotent — `CREATE TABLE IF NOT EXISTS` doesn't
    // re-create, the version gate skips the ALTER).
    #[test]
    fn v8_migrate_idempotent() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(log_columns(&conn).contains("superseded_by"));
            assert!(schema_object_exists(&conn, "table", "log_contradiction"));
            assert!(schema_object_exists(&conn, "table", "log_consolidation_audit"));
        }
    }

    // C3 — schema-additions-are-inert: at v8 the normal write + search
    // round-trip behaves exactly as at v7. Mirrors `fts_round_trip_search_then_get`
    // and adds the proof that nobody reads/writes the new column: log_progress
    // leaves superseded_by NULL and neither search() nor get() surfaces it.
    #[test]
    fn v8_schema_additions_are_inert() {
        let (_tmp, db) = db_path();
        {
            let conn = open(&db).unwrap(); // fresh open lands at v8
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        }
        // Same write path as every prior version (explicit columns; the new
        // column is never named, so it stays NULL).
        let id_a = log_progress(
            &db,
            "auth refactor day 1",
            Some("Refactored the OAuth middleware to use the new token model."),
        )
        .unwrap();
        let _id_b = log_progress(
            &db,
            "lunch break notes",
            Some("Unrelated standup notes about lunch and the cafeteria."),
        )
        .unwrap();
        // Search behaves identically to the v7 FTS round-trip.
        let hits = block_on(search(&db, None, "OAuth", 6)).unwrap();
        assert_eq!(hits.len(), 1, "hits = {hits:?}");
        assert_eq!(hits[0].id, id_a);
        assert_eq!(hits[0].summary, "auth refactor day 1");
        // The new column is present but inert: log_progress left it NULL, and
        // the public read API (get) returns the unchanged entry shape.
        let conn = open(&db).unwrap();
        let sb: Option<i64> = conn
            .query_row(
                "SELECT superseded_by FROM log WHERE id = ?1",
                params![id_a],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sb.is_none(), "log_progress must not write superseded_by");
        let entry = get(&db, id_a).unwrap();
        assert!(entry.found);
        assert_eq!(entry.summary, "auth refactor day 1");
        assert_eq!(
            entry.detail.as_deref(),
            Some("Refactored the OAuth middleware to use the new token model.")
        );
    }

    // ---- consolidation loop cursor (v9) — consolidation_state ----
    //
    // Pure-add step (see the v9 arm in `apply_migration_step`): a single-row
    // global cursor for the low-frequency background consolidation pass. The
    // from-state for v8→v9 is V8_DDL_FROZEN — V7_DDL_FROZEN copied verbatim with
    // the THREE deltas the v8 migration applies (log.superseded_by column +
    // log_contradiction + log_consolidation_audit), so the frozen constant can't
    // drift off the real shipped v8 shape and falsely pass.
    const V8_DDL_FROZEN: &str = "CREATE TABLE log (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        ts            INTEGER NOT NULL,
        summary       TEXT    NOT NULL,
        detail        TEXT,
        origin        TEXT    NOT NULL DEFAULT 'self',
        project_hash  TEXT,
        kind          TEXT,
        superseded_by INTEGER REFERENCES log(id)
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(detail, content='log', content_rowid='id', tokenize='trigram');
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id        TEXT PRIMARY KEY,
        agent_id         TEXT,
        team_id          TEXT,
        project_hash     TEXT,
        source           TEXT,
        parent_thread_id TEXT,
        cwd              TEXT,
        source_path      TEXT NOT NULL,
        first_seen_ts    INTEGER NOT NULL,
        last_ingest_ts   INTEGER NOT NULL,
        last_offset      INTEGER NOT NULL DEFAULT 0,
        last_line_no     INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts   INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);
    CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[1024]);
    CREATE TABLE log_contradiction (
        id_a     INTEGER NOT NULL,
        id_b     INTEGER NOT NULL,
        audit_id INTEGER,
        PRIMARY KEY (id_a, id_b)
    );
    CREATE TABLE log_consolidation_audit (
        id            INTEGER PRIMARY KEY,
        run_ts        INTEGER NOT NULL,
        kp_a          INTEGER NOT NULL,
        kp_b          INTEGER NOT NULL,
        distance      REAL,
        relation      TEXT    NOT NULL,
        action        TEXT    NOT NULL,
        rationale     TEXT,
        superseded_id INTEGER,
        dry_run       INTEGER NOT NULL,
        applied       INTEGER NOT NULL
    );";

    // V9.1 — v8→v9: consolidation_state appears, seeded with ONE row (id=1) at
    // the initial high-water 0 / NULL ts. Existing v8 objects are untouched.
    #[test]
    fn v8_to_v9_migrate() {
        let (_tmp, db) = db_path();
        ensure_vec_extension(); // V8_DDL_FROZEN has a vec0 table (raw open)
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&db).unwrap();
            let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
            conn.execute_batch(V8_DDL_FROZEN).unwrap();
            conn.execute_batch("PRAGMA user_version = 8;").unwrap();
            assert_eq!(read_user_version(&conn), 8);
            assert!(
                !schema_object_exists(&conn, "table", "consolidation_state"),
                "v8 precondition: no consolidation_state yet"
            );
        }
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION); // 9
        assert!(
            schema_object_exists(&conn, "table", "consolidation_state"),
            "v9 adds consolidation_state"
        );
        let cnt: i64 = conn
            .query_row("SELECT COUNT(*) FROM consolidation_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 1, "exactly one consolidation_state row");
        let (id, max_id, ts): (i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT id, last_consolidation_max_log_id, last_consolidation_ts FROM consolidation_state",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(id, 1, "the singleton id is 1");
        assert_eq!(max_id, 0, "initial high-water is 0");
        assert!(ts.is_none(), "initial last_consolidation_ts is NULL");
    }

    // V9.2 — re-open a v9 DB repeatedly: migrate is a no-op (INSERT OR IGNORE
    // keeps the single seed row; version stable; no duplicate row).
    #[test]
    fn v9_migrate_idempotent() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            let cnt: i64 = conn
                .query_row("SELECT COUNT(*) FROM consolidation_state", [], |r| r.get(0))
                .unwrap();
            assert_eq!(cnt, 1, "INSERT OR IGNORE keeps exactly one row across reopens");
        }
    }

    // ---- S5-C-2 loop trigger: count_new_live / advance marker / kill switch ----

    // S5C2.A — count_new_live counts ONLY live KPs with id > marker; advancing
    // the marker (to a start-snapshot max_id) moves it + stamps ts.
    #[test]
    fn consolidation_trigger_counts_new_live_past_marker() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        // 5 KPs (ids 1..=5); supersede #2 and #4 → live = {1,3,5}.
        let ids: Vec<i64> = (1..=5)
            .map(|i| seed_kp_with_ts(&conn, 1_000 + i, &format!("kp{i}")))
            .collect();
        conn.execute("UPDATE log SET superseded_by=?1 WHERE id=?2", params![ids[0], ids[1]]).unwrap();
        conn.execute("UPDATE log SET superseded_by=?1 WHERE id=?2", params![ids[2], ids[3]]).unwrap();

        // marker 0 (fresh): the 3 live KPs.
        assert_eq!(count_new_live(&conn).unwrap(), 3, "marker 0 counts the 3 live KPs");

        // advance to id=3: live KPs with id>3 → only id 5 (id 4 is dead → excluded).
        advance_consolidation_marker(&conn, ids[2], 12_345).unwrap();
        assert_eq!(consolidation_marker(&conn).unwrap(), ids[2], "marker == passed max_id");
        assert_eq!(count_new_live(&conn).unwrap(), 1, "only live id>3 counts; dead id 4 excluded");
        let ts: Option<i64> = conn
            .query_row("SELECT last_consolidation_ts FROM consolidation_state WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ts, Some(12_345), "advance stamps last_consolidation_ts");

        // advance to the top → nothing new.
        advance_consolidation_marker(&conn, ids[4], 12_346).unwrap();
        assert_eq!(count_new_live(&conn).unwrap(), 0, "marker at top: nothing new");
    }

    // S5C2.B — the trigger predicate (new_live >= threshold) flips exactly at the
    // default threshold. Uses the const directly (no env), so it's deterministic.
    #[test]
    fn consolidation_trigger_threshold_gate() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        for i in 0..(CONSOLIDATION_MIN_NEW_KPS - 1) {
            seed_kp_with_ts(&conn, 1_000 + i, &format!("kp{i}"));
        }
        let n = count_new_live(&conn).unwrap();
        assert_eq!(n, CONSOLIDATION_MIN_NEW_KPS - 1);
        assert!(n < CONSOLIDATION_MIN_NEW_KPS, "below threshold → loop would NOT fire");
        seed_kp_with_ts(&conn, 9_999, "kp_last");
        let n2 = count_new_live(&conn).unwrap();
        assert_eq!(n2, CONSOLIDATION_MIN_NEW_KPS);
        assert!(n2 >= CONSOLIDATION_MIN_NEW_KPS, "at threshold → loop WOULD fire");
    }

    // S5C2.C — kill-switch + threshold env truth tables (pure parsers, no env
    // mutation → no parallel-test flakiness).
    #[test]
    fn consolidation_env_parsers_truth_table() {
        // kill switch: ONLY "1"/"true" (trimmed, case-insensitive) enable.
        assert!(parse_enabled_flag(Some("1")));
        assert!(parse_enabled_flag(Some("true")));
        assert!(parse_enabled_flag(Some("TRUE")));
        assert!(parse_enabled_flag(Some("  true  ")));
        assert!(!parse_enabled_flag(None), "unset → OFF (default kill switch)");
        assert!(!parse_enabled_flag(Some("0")));
        assert!(!parse_enabled_flag(Some("false")));
        assert!(!parse_enabled_flag(Some("yes")));
        assert!(!parse_enabled_flag(Some("")));
        // threshold: a positive int overrides; else fall back to the default const.
        assert_eq!(parse_min_new_kps(Some("5")), 5);
        assert_eq!(parse_min_new_kps(Some("  42 ")), 42);
        assert_eq!(parse_min_new_kps(None), CONSOLIDATION_MIN_NEW_KPS);
        assert_eq!(parse_min_new_kps(Some("0")), CONSOLIDATION_MIN_NEW_KPS, "non-positive → default");
        assert_eq!(parse_min_new_kps(Some("-3")), CONSOLIDATION_MIN_NEW_KPS);
        assert_eq!(parse_min_new_kps(Some("abc")), CONSOLIDATION_MIN_NEW_KPS);
    }

    // ---- consolidation (v8) judge parser + candidate finding (deterministic) ----

    // J1 — clean object parses to the right relation; null superseded_id.
    #[test]
    fn judge_verdict_parses_clean_object() {
        let v = parse_judge_verdict(
            r#"{"relation":"duplicate","rationale":"same lesson","superseded_id":null}"#,
        )
        .unwrap();
        assert_eq!(v.relation, Relation::Duplicate);
        assert_eq!(v.rationale, "same lesson");
        assert_eq!(v.superseded_id, None);
    }

    // J2 — the OBJECT parser survives the same noise the array parser does:
    // a `<think>` preamble (whose braces would mislead a naive slice) + a
    // ```json fence. superseded_id is read for supersede.
    #[test]
    fn judge_verdict_strips_think_and_fence() {
        let s = "<think>distance is 0.7 {not json}; but content shows a replacement</think>\n```json\n{\"relation\":\"supersede\",\"rationale\":\"B replaces A\",\"superseded_id\":4}\n```";
        let v = parse_judge_verdict(s).unwrap();
        assert_eq!(v.relation, Relation::Supersede);
        assert_eq!(v.superseded_id, Some(4));
    }

    // J3 — object embedded in prose is sliced out (first '{' … last '}').
    #[test]
    fn judge_verdict_slices_object_from_prose() {
        let s = r#"Sure! My verdict: {"relation":"no_action","rationale":"shared entity, different concern"} — hope that helps."#;
        let v = parse_judge_verdict(s).unwrap();
        assert_eq!(v.relation, Relation::NoAction);
        assert_eq!(v.superseded_id, None);
    }

    // J4 — a numeric-string superseded_id is accepted (model JSON sloppiness).
    #[test]
    fn judge_verdict_accepts_numeric_string_superseded_id() {
        let v = parse_judge_verdict(
            r#"{"relation":"supersede","rationale":"x","superseded_id":"9"}"#,
        )
        .unwrap();
        assert_eq!(v.superseded_id, Some(9));
    }

    // J5 — refuse to guess: unknown / missing relation and non-object are errors.
    #[test]
    fn judge_verdict_rejects_unparseable() {
        assert!(parse_judge_verdict(r#"{"relation":"merge","rationale":"x"}"#).is_err());
        assert!(parse_judge_verdict(r#"{"rationale":"x","superseded_id":null}"#).is_err());
        assert!(parse_judge_verdict("no json object here").is_err());
    }

    // J6 — relation → audit action mapping: dedup+supersede collapse to one
    // action; complement+no_action both leave live.
    #[test]
    fn relation_intent_action_mapping() {
        assert_eq!(Relation::Duplicate.intent_action(), "would_merge");
        assert_eq!(Relation::Supersede.intent_action(), "would_merge");
        assert_eq!(Relation::Contradiction.intent_action(), "would_contradict");
        assert_eq!(Relation::Complement.intent_action(), "leave");
        assert_eq!(Relation::NoAction.intent_action(), "leave");
    }

    // J7 — find_candidates: pairs strictly within T, and the three exclusions
    // (far neighbour, superseded member, already-judged pair). Also exercises
    // read_stored_vector's blob round-trip (the riskiest new bit — if the
    // sqlite-vec blob decode were wrong, candidates would be silently empty).
    #[test]
    fn find_candidates_within_t_and_exclusions() {
        let (_tmp, db) = db_path();
        ensure_vec_extension();
        let a = log_progress(&db, "kp a", Some("alpha")).unwrap();
        let b = log_progress(&db, "kp b", Some("beta")).unwrap();
        let c = log_progress(&db, "kp c", Some("gamma")).unwrap();
        let conn = open(&db).unwrap();
        // a,b near (axis 0, L2≈0.05); c orthogonal (axis 1, L2≈1.41 from a/b).
        insert_log_vec(&conn, a, &vec_with_axes(&[(0, 1.0)]));
        insert_log_vec(&conn, b, &vec_with_axes(&[(0, 1.0), (1, 0.05)]));
        insert_log_vec(&conn, c, &vec_with_axes(&[(1, 1.0)]));
        let t = 1.10;
        let lo = a.min(b);
        let hi = a.max(b);

        let cands = find_candidates(&conn, t).unwrap();
        assert!(
            cands.iter().any(|(x, y, _)| *x == lo && *y == hi),
            "a,b within T must be a candidate: {cands:?}"
        );
        assert!(
            !cands.iter().any(|(x, y, _)| *x == c || *y == c),
            "c is beyond T — no pair may include it: {cands:?}"
        );

        // superseded member removes the pair.
        conn.execute("UPDATE log SET superseded_by = ?1 WHERE id = ?2", params![a, b])
            .unwrap();
        assert!(
            find_candidates(&conn, t).unwrap().is_empty(),
            "a superseded member excludes the pair"
        );

        // restore live; an existing audit row makes the pair idempotent-skip.
        conn.execute("UPDATE log SET superseded_by = NULL WHERE id = ?1", params![b])
            .unwrap();
        conn.execute(
            "INSERT INTO log_consolidation_audit\
             (run_ts, kp_a, kp_b, distance, relation, action, rationale, dry_run, applied)\
             VALUES (0, ?1, ?2, 0.05, 'duplicate', 'would_merge', 'seeded', 1, 0)",
            params![lo, hi],
        )
        .unwrap();
        assert!(
            find_candidates(&conn, t).unwrap().is_empty(),
            "an already-judged pair is skipped (idempotency)"
        );
    }

    // ---- consolidate_once parallel judging + audit dump (deterministic) ----

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock judge: counts calls, tracks peak concurrency, optionally errors on
    /// one canonical pair. No network — drives `consolidate_once` offline.
    struct MockJudge {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        calls: AtomicUsize,
        error_on: Option<(i64, i64)>,
        pending_on: Option<(i64, i64)>,
        // `flaky_on`: the first `flaky_pending` calls to this pair hang (→ timeout),
        // then it returns a normal verdict — exercises timeout-only retry recovery.
        flaky_on: Option<(i64, i64)>,
        flaky_pending: usize,
        flaky_calls: AtomicUsize,
    }
    impl MockJudge {
        fn new(error_on: Option<(i64, i64)>) -> Self {
            Self {
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                error_on,
                pending_on: None,
                flaky_on: None,
                flaky_pending: 0,
                flaky_calls: AtomicUsize::new(0),
            }
        }
        /// A judge that NEVER returns for `pending_on` — exercises the per-call
        /// hard timeout (the call that hung Layer D for 30+ min).
        fn pending(pending_on: (i64, i64)) -> Self {
            Self { pending_on: Some(pending_on), ..Self::new(None) }
        }
        /// A judge whose `flaky_on` pair hangs (times out) the first `pending`
        /// attempts, then returns a normal verdict — exercises timeout retry.
        fn flaky(flaky_on: (i64, i64), pending: usize) -> Self {
            Self { flaky_on: Some(flaky_on), flaky_pending: pending, ..Self::new(None) }
        }
    }
    #[async_trait::async_trait]
    impl ConsolidationJudge for MockJudge {
        async fn judge_pair(
            &self,
            a: &KpRef,
            b: &KpRef,
            _distance: f64,
        ) -> Result<JudgeVerdict, ExtractError> {
            let cur = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(cur, Ordering::SeqCst);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let pair = if a.id <= b.id { (a.id, b.id) } else { (b.id, a.id) };
            if Some(pair) == self.flaky_on {
                // Hang the first `flaky_pending` attempts (→ caller times out →
                // retries), then recover with a real verdict.
                let n = self.flaky_calls.fetch_add(1, Ordering::SeqCst);
                if n < self.flaky_pending {
                    return std::future::pending().await;
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                return Ok(JudgeVerdict {
                    relation: Relation::NoAction,
                    rationale: "mock flaky recovered".into(),
                    superseded_id: None,
                });
            }
            if Some(pair) == self.pending_on {
                // Never resolves; the caller's per-call tokio::time::timeout must
                // abort it. (No decrement — this future is cancelled mid-await.)
                return std::future::pending().await;
            }
            // Yield twice so co-scheduled buffered futures actually overlap.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            let r = if Some(pair) == self.error_on {
                Err(ExtractError::HttpTransient("mock judge boom".into()))
            } else {
                Ok(JudgeVerdict {
                    relation: Relation::NoAction,
                    rationale: format!("mock {}<->{}", a.id, b.id),
                    superseded_id: None,
                })
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            r
        }
    }

    /// Seed a v8 DB with `n` mutually-near KPs (all within T) → n*(n-1)/2
    /// candidate pairs. Returns (tmp, db_path, sorted live ids).
    fn seed_dense_db(n: usize) -> (tempfile::TempDir, std::path::PathBuf, Vec<i64>) {
        let (tmp, db) = db_path();
        ensure_vec_extension();
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(log_progress(&db, &format!("kp {i}"), Some(&format!("body {i}"))).unwrap());
        }
        let conn = open(&db).unwrap();
        for (i, &id) in ids.iter().enumerate() {
            // axis-0 dominant + a unique tiny perturbation → all pairwise L2 ≈ 0.014 < T.
            insert_log_vec(&conn, id, &vec_with_axes(&[(0, 1.0), (i + 1, 0.01)]));
        }
        ids.sort();
        (tmp, db, ids)
    }

    // T-par1 — bounded-concurrency judging: ≤8 in flight, every candidate judged
    // once, audit rows == candidates in deterministic (id_a,id_b) order, and an
    // erroring judge yields a `relation="error"` verdict without panicking the batch.
    #[test]
    fn dry_run_parallel_bounded() {
        let (_tmp, db, ids) = seed_dense_db(6); // 6 KPs → 15 candidate pairs
        let conn = open(&db).unwrap();
        let n_cand = find_candidates(&conn, 1.10).unwrap().len();
        assert!(n_cand >= 8, "need >=8 candidates to exercise the cap; got {n_cand}");

        let err_pair = (ids[0], ids[1]); // canonical (sorted ids)
        let judge = MockJudge::new(Some(err_pair));
        let written = block_on(consolidate_once(
            &conn,
            &judge,
            1.10,
            std::time::Duration::from_secs(120),
            true,
        ))
        .unwrap();

        assert_eq!(written, n_cand, "one audit row per candidate (errors included)");
        assert_eq!(judge.calls.load(Ordering::SeqCst), n_cand, "every candidate judged once");
        let maxc = judge.max_in_flight.load(Ordering::SeqCst);
        assert!(maxc <= 8, "concurrency cap exceeded: {maxc}");
        assert!(maxc >= 2, "expected real overlap (bounded parallel), got {maxc}");

        let rows: Vec<(i64, i64, String)> = {
            let mut s = conn
                .prepare("SELECT kp_a, kp_b, relation FROM log_consolidation_audit ORDER BY id")
                .unwrap();
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(rows.len(), n_cand);
        let mut sorted = rows.clone();
        sorted.sort_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)));
        assert_eq!(rows, sorted, "audit rows written in deterministic (id_a,id_b) order");

        let err_row = rows.iter().find(|(a, b, _)| (*a, *b) == err_pair).unwrap();
        assert_eq!(err_row.2, "error", "errored pair records an error verdict");
        assert_eq!(
            rows.iter().filter(|(_, _, rel)| rel == "error").count(),
            1,
            "exactly one error row"
        );
        assert!(
            rows.iter().filter(|(a, b, _)| (*a, *b) != err_pair).all(|(_, _, rel)| rel == "no_action"),
            "the rest judged normally"
        );
        let sb: i64 = conn
            .query_row("SELECT count(*) FROM log WHERE superseded_by IS NOT NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sb, 0, "dry-run must not set superseded_by");
    }

    // T-par2 — the audit dump is always written, one line per candidate, with the
    // canonical header.
    #[test]
    fn audit_dump_always_written() {
        let (_tmp, db, _ids) = seed_dense_db(5); // 5 KPs → 10 candidate pairs
        let conn = open(&db).unwrap();
        let n_cand = find_candidates(&conn, 1.10).unwrap().len();
        let judge = MockJudge::new(None);
        block_on(consolidate_once(
            &conn,
            &judge,
            1.10,
            std::time::Duration::from_secs(120),
            true,
        ))
        .unwrap();

        let dump = db.parent().unwrap().join("audit-dump.tsv");
        let n = dump_audit_tsv(&conn, &dump).unwrap();
        assert_eq!(n, n_cand, "dump row count == candidate count");
        let body = std::fs::read_to_string(&dump).unwrap();
        let mut lines = body.lines();
        assert_eq!(
            lines.next().unwrap(),
            "run_ts\tkp_a\tkp_b\tdistance\trelation\taction\tsuperseded_id\tdry_run\tapplied\trationale",
            "header columns"
        );
        assert_eq!(lines.count(), n_cand, "one data line per candidate");
    }

    // T-timeout — a judge call that NEVER returns is force-aborted by the
    // per-call `tokio::time::timeout` (reqwest's own timeout did not catch this
    // in Layer D). Regression for the 30+ min hang: the pending pair → an
    // `error` verdict naming the timeout, the OTHER calls complete, audit rows
    // == candidates, no panic, and crucially the test returns fast (no hang).
    #[test]
    fn judge_call_has_hard_timeout() {
        let (_tmp, db, ids) = seed_dense_db(6); // 15 candidate pairs
        let conn = open(&db).unwrap();
        let n_cand = find_candidates(&conn, 1.10).unwrap().len();
        let hang_pair = (ids[0], ids[1]); // canonical (sorted ids)
        let judge = MockJudge::pending(hang_pair);

        // Tiny timeout so the hung pair aborts fast — the whole test must NOT hang.
        let written = block_on(consolidate_once(
            &conn,
            &judge,
            1.10,
            std::time::Duration::from_millis(100),
            true,
        ))
        .unwrap();
        assert_eq!(written, n_cand, "every candidate written, incl. the timed-out pair");

        let rows: Vec<(i64, i64, String, String)> = {
            let mut s = conn
                .prepare("SELECT kp_a, kp_b, relation, rationale FROM log_consolidation_audit ORDER BY id")
                .unwrap();
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(rows.len(), n_cand);
        let hung = rows.iter().find(|(a, b, _, _)| (*a, *b) == hang_pair).unwrap();
        assert_eq!(hung.2, "error", "the hung pair records an error verdict");
        assert!(hung.3.contains("timeout"), "rationale names the timeout: {:?}", hung.3);
        assert_eq!(
            rows.iter().filter(|(_, _, rel, _)| rel == "error").count(),
            1,
            "only the hung pair errors"
        );
        assert!(
            rows.iter()
                .filter(|(a, b, _, _)| (*a, *b) != hang_pair)
                .all(|(_, _, rel, _)| rel == "no_action"),
            "the other calls complete normally"
        );
    }

    // T-retry — a judge call that times out the first MAX_JUDGE_RETRIES attempts
    // then succeeds is RECOVERED (real verdict, not an error) — de-flakes the gate
    // read against a wedging proxy WITHOUT changing any verdict classification.
    #[test]
    fn judge_timeout_is_retried_until_success() {
        let (_tmp, db, ids) = seed_dense_db(6);
        let conn = open(&db).unwrap();
        let n_cand = find_candidates(&conn, 1.10).unwrap().len();
        let flaky_pair = (ids[0], ids[1]); // canonical (sorted)
        // hang the first 2 attempts (= MAX_JUDGE_RETRIES), recover on the 3rd.
        let judge = MockJudge::flaky(flaky_pair, 2);
        let written = block_on(consolidate_once(
            &conn,
            &judge,
            1.10,
            std::time::Duration::from_millis(50),
            true,
        ))
        .unwrap();
        assert_eq!(written, n_cand, "one audit row per candidate, incl. the recovered pair");
        // recovered via retry → a REAL verdict, not an error row.
        let rel: String = conn
            .query_row(
                "SELECT relation FROM log_consolidation_audit WHERE kp_a=?1 AND kp_b=?2",
                params![flaky_pair.0, flaky_pair.1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rel, "no_action", "timed-out pair recovered via retry, NOT 'error'");
        assert_eq!(
            judge.flaky_calls.load(Ordering::SeqCst),
            3,
            "2 timeouts + 1 success = 3 attempts"
        );
        let errs: i64 = conn
            .query_row("SELECT count(*) FROM log_consolidation_audit WHERE relation='error'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(errs, 0, "no error rows — every pair resolved");
    }

    // ---- S5-C apply: direction + referential integrity + live mutation ----

    /// Seed a `log` row with an explicit `ts` — apply direction is ts-driven, so
    /// the apply tests must control it. Returns the new id.
    fn seed_kp_with_ts(conn: &Connection, ts: i64, summary: &str) -> i64 {
        conn.execute(
            "INSERT INTO log(ts, summary, detail, origin) VALUES (?1, ?2, ?3, 'self')",
            params![ts, summary, format!("{summary} detail")],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    // S5C.A — decide_merge: the pure direction matrix (no DB). The load-bearing
    // logic the architect flagged: dup retires older, supersede demands the judge
    // agree with ts (else skip), co-temporal falls back to the judge's pick.
    #[test]
    fn apply_decide_merge_direction_matrix() {
        // duplicate → retire the older (by ts); survivor is the newer.
        assert_eq!(
            decide_merge(Relation::Duplicate, 10, 100, 20, 200, None),
            MergeDecision::Supersede { superseded: 10, survivor: 20 }
        );
        // duplicate tie on ts → the smaller id is the (retired) older one.
        assert_eq!(
            decide_merge(Relation::Duplicate, 20, 100, 10, 100, None),
            MergeDecision::Supersede { superseded: 10, survivor: 20 }
        );
        // supersede, judge agrees the older (10) is replaced → apply.
        assert_eq!(
            decide_merge(Relation::Supersede, 10, 100, 20, 200, Some(10)),
            MergeDecision::Supersede { superseded: 10, survivor: 20 }
        );
        // supersede, judge points at the NEWER (20) → direction mismatch → skip.
        assert_eq!(
            decide_merge(Relation::Supersede, 10, 100, 20, 200, Some(20)),
            MergeDecision::Skip("[apply skipped: judge/ts direction mismatch]")
        );
        // supersede co-temporal → trust the judge's named member.
        assert_eq!(
            decide_merge(Relation::Supersede, 10, 100, 20, 100, Some(20)),
            MergeDecision::Supersede { superseded: 20, survivor: 10 }
        );
        // supersede co-temporal, judge named neither → undeterminable → skip.
        assert_eq!(
            decide_merge(Relation::Supersede, 10, 100, 20, 100, None),
            MergeDecision::Skip("[apply skipped: supersede direction undeterminable]")
        );
        // non-merge relations.
        assert_eq!(decide_merge(Relation::Contradiction, 1, 0, 2, 0, None), MergeDecision::Contradict);
        assert_eq!(decide_merge(Relation::Complement, 1, 0, 2, 0, None), MergeDecision::Leave);
        assert_eq!(decide_merge(Relation::NoAction, 1, 0, 2, 0, None), MergeDecision::Leave);
    }

    // S5C.B — consolidate_pair(dry_run=false): real mutation + audit bookkeeping
    // + the referential-integrity guard. Drives apply with scripted verdicts —
    // no vectors / find_candidates needed (per the architect's escape hatch).
    #[test]
    fn apply_verdict_mutates_live_state() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let run_ts = current_time_ms();
        let v = |rel: Relation, sid: Option<i64>| -> Result<JudgeVerdict, String> {
            Ok(JudgeVerdict { relation: rel, rationale: "r".into(), superseded_id: sid })
        };
        let sb = |id: i64| -> Option<i64> {
            conn.query_row("SELECT superseded_by FROM log WHERE id=?1", params![id], |r| r.get(0)).unwrap()
        };
        let audit = |a: i64, b: i64| -> (i64, i64, String) {
            conn.query_row(
                "SELECT dry_run, applied, rationale FROM log_consolidation_audit WHERE kp_a=?1 AND kp_b=?2",
                params![a, b],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default())),
            )
            .unwrap()
        };

        // duplicate → older(ts) superseded by newer; newer live; applied, dry=0.
        let d_old = seed_kp_with_ts(&conn, 1_000, "dup old");
        let d_new = seed_kp_with_ts(&conn, 2_000, "dup new");
        consolidate_pair(&conn, run_ts, d_old.min(d_new), d_old.max(d_new), 0.05, v(Relation::Duplicate, None), false).unwrap();
        assert_eq!(sb(d_old), Some(d_new), "dup: older superseded by newer");
        assert_eq!(sb(d_new), None, "dup: newer stays live");
        assert_eq!(audit(d_old.min(d_new), d_old.max(d_new)), (0, 1, "r".into()), "dup audit dry=0 applied=1");

        // supersede correct direction → older superseded.
        let s_old = seed_kp_with_ts(&conn, 1_000, "sup old");
        let s_new = seed_kp_with_ts(&conn, 3_000, "sup new");
        consolidate_pair(&conn, run_ts, s_old.min(s_new), s_old.max(s_new), 0.4, v(Relation::Supersede, Some(s_old)), false).unwrap();
        assert_eq!(sb(s_old), Some(s_new), "supersede: older replaced");
        assert_eq!(sb(s_new), None);
        assert_eq!(audit(s_old.min(s_new), s_old.max(s_new)).1, 1, "supersede applied");

        // supersede WRONG direction (judge names the newer) → skip, both live, marked.
        let w_old = seed_kp_with_ts(&conn, 1_000, "wrong old");
        let w_new = seed_kp_with_ts(&conn, 4_000, "wrong new");
        consolidate_pair(&conn, run_ts, w_old.min(w_new), w_old.max(w_new), 0.4, v(Relation::Supersede, Some(w_new)), false).unwrap();
        assert_eq!(sb(w_old), None, "mismatch: both stay live");
        assert_eq!(sb(w_new), None, "mismatch: both stay live");
        let (_d, applied, rationale) = audit(w_old.min(w_new), w_old.max(w_new));
        assert_eq!(applied, 0, "mismatch not applied");
        assert!(rationale.contains("[apply skipped: judge/ts direction mismatch]"), "rationale marks mismatch: {rationale:?}");

        // contradiction → log_contradiction edge linked to its audit row; no superseded_by.
        let c1 = seed_kp_with_ts(&conn, 1_000, "contra a");
        let c2 = seed_kp_with_ts(&conn, 1_000, "contra b");
        consolidate_pair(&conn, run_ts, c1.min(c2), c1.max(c2), 0.6, v(Relation::Contradiction, None), false).unwrap();
        let (cnt, linked): (i64, i64) = conn
            .query_row(
                "SELECT count(*), coalesce(max(lc.audit_id = a.id), 0) \
                 FROM log_contradiction lc \
                 JOIN log_consolidation_audit a ON a.kp_a=lc.id_a AND a.kp_b=lc.id_b \
                 WHERE lc.id_a=?1 AND lc.id_b=?2",
                params![c1.min(c2), c1.max(c2)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cnt, 1, "contradiction edge inserted");
        assert_eq!(linked, 1, "log_contradiction.audit_id links to its audit row");
        assert_eq!(sb(c1), None, "contradiction leaves superseded_by untouched");
        assert_eq!(sb(c2), None);
        assert_eq!(audit(c1.min(c2), c1.max(c2)).1, 1, "contradiction applied");

        // referential integrity: survivor already superseded → skip, no chain.
        let dead = seed_kp_with_ts(&conn, 5_000, "dead survivor");
        conn.execute("UPDATE log SET superseded_by=?1 WHERE id=?2", params![d_new, dead]).unwrap();
        let live_old = seed_kp_with_ts(&conn, 1_500, "older vs dead");
        // duplicate(live_old, dead): newer=dead would be survivor, but it's not live.
        consolidate_pair(&conn, run_ts, live_old.min(dead), live_old.max(dead), 0.05, v(Relation::Duplicate, None), false).unwrap();
        assert_eq!(sb(live_old), None, "survivor-not-live: older stays live (no chain)");
        let (_d2, ap2, rat2) = audit(live_old.min(dead), live_old.max(dead));
        assert_eq!(ap2, 0, "not applied when survivor not live");
        assert!(rat2.contains("[apply skipped: survivor no longer live]"), "marks survivor-not-live: {rat2:?}");

        // referential integrity: loser already superseded → skip, first-survivor-wins.
        // d_old was retired by d_new (the dup case). A later pair that would retire
        // d_old AGAIN must skip WITHOUT overwriting its survivor pointer to z.
        let z = seed_kp_with_ts(&conn, 6_000, "z newer");
        consolidate_pair(&conn, run_ts, d_old.min(z), d_old.max(z), 0.05, v(Relation::Duplicate, None), false).unwrap();
        assert_eq!(sb(d_old), Some(d_new), "first-survivor-wins: d_old still points at d_new (not overwritten)");
        assert_eq!(sb(z), None, "loser-already-superseded: z untouched");
        let (_d3, ap3, rat3) = audit(d_old.min(z), d_old.max(z));
        assert_eq!(ap3, 0, "not applied when loser already superseded");
        assert!(rat3.contains("[apply skipped: loser already superseded]"), "marks loser-already-superseded: {rat3:?}");
    }

    // S3s.C.1 — first ingest with new rows sets `last_growth_ts` to the
    // same `now` as `last_ingest_ts` (proving the inserts-branch UPDATE).
    #[test]
    fn s3s_c1_first_ingest_with_new_rows_sets_growth_ts() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_c1";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc\n",
        );

        let before = current_time_ms();
        let conn = open(&memory_db).unwrap();
        let stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert!(stats.events_inserted > 0);

        let (last_ingest_ts, last_growth_ts): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_ingest_ts, last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert!(
            last_growth_ts.is_some(),
            "first ingest with new rows must set growth_ts"
        );
        let growth = last_growth_ts.unwrap();
        assert!(growth >= before, "growth_ts {growth} should be >= {before}");
        assert_eq!(
            Some(last_ingest_ts),
            last_growth_ts,
            "first-ingest pass writes both timestamps to the same `now`"
        );
    }

    // S3s.C.2 — KEY semantic test: no-op pass (0 new rows) advances
    // `last_ingest_ts` but leaves `last_growth_ts` unchanged. Pins down
    // that the two timestamps have different meanings.
    #[test]
    fn s3s_c2_noop_pass_advances_ingest_ts_but_not_growth_ts() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_c2";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "x\ny\n",
        );

        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();

        let (ingest1, growth1): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_ingest_ts, last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(growth1.is_some(), "first ingest must have set growth_ts");

        // 10 ms sleep so the second pass's `now` is strictly greater
        // (current_time_ms has 1 ms resolution).
        std::thread::sleep(std::time::Duration::from_millis(10));

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 0, "file unchanged, no inserts expected");

        let (ingest2, growth2): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_ingest_ts, last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            ingest2 > ingest1,
            "last_ingest_ts must advance every pass: {ingest1} -> {ingest2}"
        );
        assert_eq!(
            growth2, growth1,
            "last_growth_ts must NOT advance when 0 new rows were inserted"
        );
    }

    // S3s.C.3 — appending then re-ingesting advances `last_growth_ts`
    // (proving the inserts-branch UPDATE actually fires on the resume
    // path, not only on the first-touch path).
    #[test]
    fn s3s_c3_append_then_ingest_advances_growth_ts() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_c3";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "first\n",
        );

        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();
        let growth1: Option<i64> = conn
            .query_row(
                "SELECT last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| r.get(0),
            )
            .unwrap();
        assert!(growth1.is_some());

        std::thread::sleep(std::time::Duration::from_millis(10));

        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .unwrap();
        f.write_all(b"second\n").unwrap();
        drop(f);

        let second = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert_eq!(second.events_inserted, 1);

        let growth2: Option<i64> = conn
            .query_row(
                "SELECT last_growth_ts FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            growth2.unwrap() > growth1.unwrap(),
            "growth_ts must advance when new content arrives: {:?} -> {:?}",
            growth1,
            growth2
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3a — transcript render + segment_thread (pure)
    // -----------------------------------------------------------------

    use serde_json::json;

    /// Helper: turn a JSON value into a compact JSONL-style payload string.
    fn payload(v: serde_json::Value) -> String {
        v.to_string()
    }

    // ---- S3a.A.1 — classification ----

    #[test]
    fn s3a_a1_classification_user_message_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello world"}]
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::Message { role, text }) => {
                assert_eq!(role, "user");
                assert_eq!(text, "hello world");
            }
            other => panic!("expected Kept(Message), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_assistant_message_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Alice here."}]
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::Message { role, text }) => {
                assert_eq!(role, "assistant");
                assert_eq!(text, "Alice here.");
            }
            other => panic!("expected Kept(Message assistant), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_developer_message_is_dropped() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "system-side instructions"}]
            }
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    #[test]
    fn s3a_a1_classification_reasoning_is_dropped() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "thinking..."}]
            }
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    #[test]
    fn s3a_a1_classification_function_call_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "do_thing",
                "arguments": "{\"x\":1}",
                "call_id": "call_abc"
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolCall {
                name,
                arguments,
                call_id,
            }) => {
                assert_eq!(name, "do_thing");
                assert_eq!(arguments, "{\"x\":1}");
                assert_eq!(call_id, "call_abc");
            }
            other => panic!("expected Kept(ToolCall), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_function_call_output_is_kept() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_abc",
                "output": {"content": "ok"}
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolResult { output, .. }) => {
                assert_eq!(output, "ok");
            }
            other => panic!("expected Kept(ToolResult), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_inner_compaction_is_dropped() {
        // Inner `response_item/compaction` is the encrypted-internal kind,
        // different from the top-level `compacted` boundary below.
        let line = payload(json!({
            "type": "response_item",
            "payload": {"type": "compaction", "encrypted_content": "…opaque…"}
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    #[test]
    fn s3a_a1_classification_top_level_compacted_is_boundary() {
        let line = payload(json!({
            "type": "compacted",
            "payload": {"message": "<segment summary text>"}
        }));
        match parse_line(&line) {
            ParsedLine::Boundary { summary } => {
                assert_eq!(summary, "<segment summary text>");
            }
            other => panic!("expected Boundary, got {other:?}"),
        }
    }

    #[test]
    fn s3a_a1_classification_top_level_compacted_missing_message_defaults_empty() {
        let line = payload(json!({"type": "compacted", "payload": {}}));
        assert_eq!(
            parse_line(&line),
            ParsedLine::Boundary {
                summary: String::new()
            }
        );
    }

    #[test]
    fn s3a_a1_classification_event_msg_turn_context_session_meta_dropped() {
        for top in ["event_msg", "turn_context", "session_meta"] {
            let line = payload(json!({"type": top, "payload": {"type": "x"}}));
            assert_eq!(
                parse_line(&line),
                ParsedLine::Dropped,
                "top type {top} must be Dropped"
            );
        }
    }

    #[test]
    fn s3a_a1_classification_unknown_top_type_dropped() {
        let line = payload(json!({"type": "something_new", "payload": {"type": "x"}}));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    // ---- S3a.A.2 — extraction ----

    #[test]
    fn s3a_a2_message_multi_content_blocks_concat_text_only() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "first "},
                    // Non-text block — must be skipped, not panic.
                    {"type": "input_image", "image_url": "data:..."},
                    {"type": "output_text", "text": "second."},
                    // Junk block with no `text` field.
                    {"type": "input_text"},
                    {"type": "input_text", "text": " end"}
                ]
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::Message { role, text }) => {
                assert_eq!(role, "user");
                assert_eq!(text, "first second. end");
            }
            other => panic!("expected Kept(Message), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a2_function_call_output_via_content_items() {
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "c-2",
                "output": {
                    "content_items": [
                        {"type": "text", "text": "part1 "},
                        {"type": "text", "text": "part2"}
                    ]
                }
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolResult {
                call_id,
                output,
            }) => {
                assert_eq!(call_id.as_deref(), Some("c-2"));
                assert_eq!(output, "part1 part2");
            }
            other => panic!("expected Kept(ToolResult), got {other:?}"),
        }
    }

    #[test]
    fn s3a_a2_function_call_arguments_passed_through_verbatim() {
        // Arguments is a raw JSON-encoded string per protocol — we don't
        // re-parse it; the distiller sees exactly what codex stored.
        let line = payload(json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "log_progress",
                "arguments": "{\"summary\":\"hi\",\"detail\":\"world\"}",
                "call_id": "call_xyz"
            }
        }));
        match parse_line(&line) {
            ParsedLine::Kept(RenderedItem::ToolCall {
                name, arguments, ..
            }) => {
                assert_eq!(name, "log_progress");
                assert_eq!(arguments, "{\"summary\":\"hi\",\"detail\":\"world\"}");
            }
            other => panic!("expected Kept(ToolCall), got {other:?}"),
        }
    }

    // ---- S3a.A.3 — robustness (no panics) ----

    #[test]
    fn s3a_a3_invalid_json_is_dropped() {
        assert_eq!(parse_line("{not json"), ParsedLine::Dropped);
        assert_eq!(parse_line(""), ParsedLine::Dropped);
        assert_eq!(parse_line("null"), ParsedLine::Dropped);
        // Valid JSON but no `type` field.
        assert_eq!(parse_line("{\"foo\":1}"), ParsedLine::Dropped);
        // `type` of wrong shape (number).
        assert_eq!(parse_line("{\"type\":42}"), ParsedLine::Dropped);
        // response_item with no payload.
        assert_eq!(parse_line("{\"type\":\"response_item\"}"), ParsedLine::Dropped);
        // response_item with empty payload.
        assert_eq!(
            parse_line("{\"type\":\"response_item\",\"payload\":{}}"),
            ParsedLine::Dropped
        );
        // message with no role.
        let no_role = payload(json!({
            "type": "response_item",
            "payload": {"type": "message", "content": []}
        }));
        assert_eq!(parse_line(&no_role), ParsedLine::Dropped);
    }

    // ---- S3a.B.1 — rendering with role labels ----

    #[test]
    fn s3a_b1_renders_kept_items_with_role_labels() {
        let lines = vec![
            (
                1_i64,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "do the thing"}]
                    }
                })),
            ),
            (
                2,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "on it"}]
                    }
                })),
            ),
            (
                3,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "do_thing",
                        "arguments": "{\"x\":1}",
                        "call_id": "c1"
                    }
                })),
            ),
            (
                4,
                payload(json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": "c1",
                        "output": {"content": "done"}
                    }
                })),
            ),
        ];
        let lines_ref: Vec<(i64, &str)> = lines.iter().map(|(n, s)| (*n, s.as_str())).collect();
        let segs = segment_thread(&lines_ref);
        assert_eq!(segs.len(), 1);
        assert_eq!(
            segs[0].transcript,
            "USER: do the thing\n\
             ASSISTANT: on it\n\
             TOOL CALL do_thing: {\"x\":1}\n\
             TOOL RESULT: done"
        );
    }

    // ---- S3a.C — segmentation ----

    fn user_msg(line_no: i64, text: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }
            })),
        )
    }

    fn assistant_msg(line_no: i64, text: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]
                }
            })),
        )
    }

    fn compacted(line_no: i64, summary: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "compacted",
                "payload": {"message": summary}
            })),
        )
    }

    fn reasoning(line_no: i64, text: &str) -> (i64, String) {
        (
            line_no,
            payload(json!({
                "type": "response_item",
                "payload": {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": text}]
                }
            })),
        )
    }

    fn event_msg(line_no: i64) -> (i64, String) {
        (
            line_no,
            payload(json!({"type": "event_msg", "payload": {"type": "token_count"}})),
        )
    }

    fn as_refs(v: &[(i64, String)]) -> Vec<(i64, &str)> {
        v.iter().map(|(n, s)| (*n, s.as_str())).collect()
    }

    // S3a.C.1 — no compacted markers: one segment, prior_summary=None,
    // start/end equal the first/last input line_no.
    #[test]
    fn s3a_c1_no_boundary_single_segment() {
        let lines = vec![
            user_msg(5, "u1"),
            assistant_msg(6, "a1"),
            user_msg(7, "u2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start_line_no, 5);
        assert_eq!(segs[0].end_line_no, 7);
        assert!(segs[0].prior_summary.is_none());
        assert_eq!(segs[0].transcript, "USER: u1\nASSISTANT: a1\nUSER: u2");
    }

    // S3a.C.2 — one mid-stream boundary: 2 segments, seg2.prior_summary
    // == marker.message, marker is in neither transcript, seg1.end==C,
    // seg2.start==C+1.
    #[test]
    fn s3a_c2_single_boundary_in_middle_splits_to_two_segments() {
        let lines = vec![
            user_msg(1, "u1"),
            assistant_msg(2, "a1"),
            compacted(3, "rollup-of-1-and-2"),
            user_msg(4, "u2"),
            assistant_msg(5, "a2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 2);

        assert_eq!(segs[0].start_line_no, 1);
        assert_eq!(segs[0].end_line_no, 3);
        assert!(segs[0].prior_summary.is_none());
        assert_eq!(segs[0].transcript, "USER: u1\nASSISTANT: a1");
        assert!(
            !segs[0].transcript.contains("rollup-of-1-and-2"),
            "marker must not leak into seg1 transcript"
        );

        assert_eq!(segs[1].start_line_no, 4);
        assert_eq!(segs[1].end_line_no, 5);
        assert_eq!(segs[1].prior_summary.as_deref(), Some("rollup-of-1-and-2"));
        assert_eq!(segs[1].transcript, "USER: u2\nASSISTANT: a2");
        assert!(
            !segs[1].transcript.contains("rollup-of-1-and-2"),
            "marker must not leak into seg2 transcript either"
        );
    }

    // S3a.C.3 — multiple boundaries: ranges are contiguous and non-overlapping;
    // each segment's prior_summary is its preceding marker's message.
    #[test]
    fn s3a_c3_multiple_boundaries_produce_contiguous_ranges() {
        let lines = vec![
            user_msg(1, "u1"),
            compacted(2, "sum-A"),
            assistant_msg(3, "a1"),
            compacted(4, "sum-B"),
            user_msg(5, "u2"),
            assistant_msg(6, "a2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 3);

        assert_eq!((segs[0].start_line_no, segs[0].end_line_no), (1, 2));
        assert!(segs[0].prior_summary.is_none());
        assert_eq!(segs[0].transcript, "USER: u1");

        assert_eq!((segs[1].start_line_no, segs[1].end_line_no), (3, 4));
        assert_eq!(segs[1].prior_summary.as_deref(), Some("sum-A"));
        assert_eq!(segs[1].transcript, "ASSISTANT: a1");

        assert_eq!((segs[2].start_line_no, segs[2].end_line_no), (5, 6));
        assert_eq!(segs[2].prior_summary.as_deref(), Some("sum-B"));
        assert_eq!(segs[2].transcript, "USER: u2\nASSISTANT: a2");

        // Contiguity: end_i + 1 == start_{i+1}.
        for win in segs.windows(2) {
            assert_eq!(win[0].end_line_no + 1, win[1].start_line_no);
        }
    }

    // S3a.C.4 — trailing boundary: no empty tail segment; the marker's
    // line_no is the previous segment's end.
    #[test]
    fn s3a_c4_trailing_boundary_does_not_produce_empty_tail_segment() {
        let lines = vec![
            user_msg(1, "u1"),
            assistant_msg(2, "a1"),
            compacted(3, "rollup"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 1, "no trailing empty segment expected");
        assert_eq!(segs[0].start_line_no, 1);
        assert_eq!(segs[0].end_line_no, 3, "boundary's line_no closes the segment");
        assert_eq!(segs[0].transcript, "USER: u1\nASSISTANT: a1");
    }

    // S3a.C.5 — Dropped lines (reasoning / event_msg) sit inside the
    // segment's line range but contribute no transcript.
    #[test]
    fn s3a_c5_dropped_lines_keep_range_but_skip_transcript() {
        let lines = vec![
            user_msg(10, "u1"),
            reasoning(11, "mid-thought"),
            event_msg(12),
            assistant_msg(13, "a1"),
            compacted(14, "sum"),
            event_msg(15),
            user_msg(16, "u2"),
        ];
        let segs = segment_thread(&as_refs(&lines));
        assert_eq!(segs.len(), 2);

        assert_eq!((segs[0].start_line_no, segs[0].end_line_no), (10, 14));
        assert_eq!(
            segs[0].transcript, "USER: u1\nASSISTANT: a1",
            "reasoning + event_msg must not appear in the transcript"
        );

        assert_eq!((segs[1].start_line_no, segs[1].end_line_no), (15, 16));
        assert_eq!(segs[1].prior_summary.as_deref(), Some("sum"));
        assert_eq!(segs[1].transcript, "USER: u2");
    }

    // S3a.E.1 — module-internal: confirm the parser uses serde_json::Value
    // navigation. No codex-cli ResponseItem / RolloutItem types are imported.
    // (Grep verifies; this test just exists as the documented contract.)
    #[test]
    fn s3a_e1_render_module_uses_no_codex_protocol_types() {
        // Sentinel: a deeply unfamiliar tool kind still classifies cleanly
        // (no enum-exhaustiveness coupling to a codex-cli upstream version).
        let line = payload(json!({
            "type": "response_item",
            "payload": {"type": "future_unknown_tool", "name": "x"}
        }));
        assert_eq!(parse_line(&line), ParsedLine::Dropped);
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3b — extractor parse + request body + env config
    // -----------------------------------------------------------------

    /// Build an HttpExtractor for tests without going through env. Lets us
    /// inspect `build_request_body` deterministically.
    fn test_http_extractor(base_url: &str, model: &str, api_key: &str) -> HttpExtractor {
        HttpExtractor {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
        }
    }

    // ---- S3be.A — tolerant JSON parse ----

    #[test]
    fn s3be_a1_clean_json_array_parses() {
        let s = r#"[
            {"summary":"x","detail":"d","kind":"decision"},
            {"summary":"y","detail":null,"kind":"fact"}
        ]"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 2);
        assert_eq!(kps[0].summary, "x");
        assert_eq!(kps[0].detail.as_deref(), Some("d"));
        assert_eq!(kps[0].kind, "decision");
        assert_eq!(kps[1].summary, "y");
        assert!(kps[1].detail.is_none());
        assert_eq!(kps[1].kind, "fact");
    }

    #[test]
    fn s3be_a2_strips_json_code_fence() {
        let s = "```json\n[{\"summary\":\"in-a-fence\",\"detail\":null,\"kind\":\"pattern\"}]\n```";
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "in-a-fence");
        assert_eq!(kps[0].kind, "pattern");

        // Unlabeled ``` fence also works.
        let unlabeled =
            "```\n[{\"summary\":\"plain-fence\",\"detail\":null,\"kind\":\"fact\"}]\n```";
        let kps2 = parse_knowledge_points(unlabeled).unwrap();
        assert_eq!(kps2.len(), 1);
        assert_eq!(kps2[0].summary, "plain-fence");
    }

    #[test]
    fn s3be_a3_extracts_array_from_prose_wrapper() {
        let s = r#"Here is the JSON you asked for: [{"summary":"sliced","detail":null,"kind":"fact"}]. Hope this helps!"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "sliced");
    }

    #[test]
    fn s3be_a4_defaults_and_kind_normalization() {
        // Missing kind → "fact"; missing detail → None; invalid kind → "fact";
        // valid kind in different case → normalised lowercase.
        let s = r#"[
            {"summary":"missing-kind","detail":"d1"},
            {"summary":"missing-detail","kind":"failure"},
            {"summary":"bad-kind","detail":null,"kind":"banana"},
            {"summary":"upper-kind","detail":null,"kind":"DECISION"}
        ]"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 4);

        assert_eq!(kps[0].kind, "fact");
        assert_eq!(kps[0].detail.as_deref(), Some("d1"));

        assert_eq!(kps[1].kind, "failure");
        assert!(kps[1].detail.is_none());

        assert_eq!(
            kps[2].kind, "fact",
            "unknown kind values fall back to 'fact'"
        );

        assert_eq!(kps[3].kind, "decision", "kind is normalised to lowercase");
    }

    #[test]
    fn s3be_a5_empty_array_and_empty_summary_skip() {
        // "[]" → Ok(empty).
        assert_eq!(parse_knowledge_points("[]").unwrap(), Vec::new());
        // Element with empty / missing summary is skipped silently.
        let s = r#"[
            {"summary":"","detail":"ignored","kind":"failure"},
            {"summary":"   ","kind":"fact"},
            {"summary":"survivor","kind":"pattern"}
        ]"#;
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1, "empty/whitespace summaries are silently dropped");
        assert_eq!(kps[0].summary, "survivor");
        assert_eq!(kps[0].kind, "pattern");
    }

    #[test]
    fn s3be_a6_total_garbage_returns_err_no_panic() {
        assert!(parse_knowledge_points("absolutely no JSON here").is_err());
        assert!(parse_knowledge_points("").is_err());
        // Top-level non-array between brackets.
        assert!(
            parse_knowledge_points("{\"summary\":\"x\"}").is_err(),
            "no '[' → Err"
        );
        // Brackets but the body between them is gibberish.
        assert!(parse_knowledge_points("[this is not JSON]").is_err());
    }

    // ---- S3be.B — request body assembly (no network) ----

    #[test]
    fn s3be_b1_body_with_prior_summary_includes_prior_block() {
        let extractor = test_http_extractor("https://api.example/v1", "qwen-test", "sk-test");
        let body = extractor.build_request_body("USER: hi", Some("earlier we agreed X."));

        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("qwen-test"));
        let temperature = body.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!(
            (0.0..=0.5).contains(&temperature),
            "temperature stays low: got {temperature}"
        );

        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        assert_eq!(messages.len(), 2);

        assert_eq!(messages[0].get("role").and_then(|v| v.as_str()), Some("system"));
        assert_eq!(
            messages[0].get("content").and_then(|v| v.as_str()),
            Some(EXTRACTION_PROMPT),
            "system message must be EXTRACTION_PROMPT verbatim"
        );

        assert_eq!(messages[1].get("role").and_then(|v| v.as_str()), Some("user"));
        let user_content = messages[1].get("content").and_then(|v| v.as_str()).unwrap();
        assert!(user_content.contains("[PRIOR CONTEXT SUMMARY]"));
        assert!(user_content.contains("earlier we agreed X."));
        assert!(user_content.contains("[TRANSCRIPT]"));
        assert!(user_content.contains("USER: hi"));
        // Prior block must come before the transcript block.
        let prior_idx = user_content.find("[PRIOR CONTEXT SUMMARY]").unwrap();
        let transcript_idx = user_content.find("[TRANSCRIPT]").unwrap();
        assert!(
            prior_idx < transcript_idx,
            "prior must precede transcript"
        );
    }

    #[test]
    fn s3be_b1_body_without_prior_summary_omits_prior_block() {
        let extractor = test_http_extractor("https://api.example/v1", "model-x", "k");
        let body = extractor.build_request_body("USER: hi", None);

        let user_content = body.get("messages").and_then(|v| v.as_array()).unwrap()[1]
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(
            !user_content.contains("[PRIOR CONTEXT SUMMARY]"),
            "no PRIOR block when prior_summary is None: got {user_content:?}"
        );
        assert!(user_content.contains("[TRANSCRIPT]"));
        assert!(user_content.contains("USER: hi"));

        // model field propagates from the extractor.
        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("model-x"));
    }

    // ---- S3be.C — from_env ----

    // env is process-global; serialise these tests against EVERY other
    // env-mutating test in the same binary. `paths::ENV_LOCK` is the
    // shared mutex used by paths::tests for HOME / USERPROFILE mutation;
    // reusing it here means our HOME-touching tests below (S3cfg.A.*)
    // can't race with the path tests.
    fn with_env<F>(values: &[(&str, Option<&str>)], f: F)
    where
        F: FnOnce(),
    {
        let _g = crate::paths::ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Snapshot + override + run + restore. set_var / remove_var are
        // unsafe in Rust 2024+; we hold ENV_LOCK so this is the only env
        // writer for the duration of `f`.
        let prior: Vec<(&str, Option<String>)> = values
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in values {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        f();
        for (k, prev) in prior {
            match prev {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    #[test]
    fn s3be_c1_from_env_all_set_returns_some() {
        with_env(
            &[
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://test.example/v1")),
                ("OPENCRAB_DISTILLER_MODEL", Some("qwen-test")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("sk-test")),
            ],
            || {
                let extractor = HttpExtractor::from_env().expect("all three vars set → Some");
                assert_eq!(extractor.base_url, "https://test.example/v1");
                assert_eq!(extractor.model, "qwen-test");
                assert_eq!(extractor.api_key, "sk-test");
            },
        );
    }

    #[test]
    fn s3be_c1_from_env_missing_any_returns_none() {
        for missing in [
            "OPENCRAB_DISTILLER_BASE_URL",
            "OPENCRAB_DISTILLER_MODEL",
            "OPENCRAB_DISTILLER_API_KEY",
        ] {
            let mut env: Vec<(&str, Option<&str>)> = vec![
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://x/v1")),
                ("OPENCRAB_DISTILLER_MODEL", Some("m")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("k")),
            ];
            for slot in env.iter_mut() {
                if slot.0 == missing {
                    slot.1 = None;
                }
            }
            with_env(&env, || {
                assert!(
                    HttpExtractor::from_env().is_none(),
                    "missing {missing} → None"
                );
            });
        }
    }

    #[test]
    fn s3be_c1_from_env_empty_string_treated_as_missing() {
        // Empty / whitespace-only values are treated the same as missing
        // (avoid surfacing "I see a base_url but it's blank, what now?").
        with_env(
            &[
                ("OPENCRAB_DISTILLER_BASE_URL", Some("   ")),
                ("OPENCRAB_DISTILLER_MODEL", Some("m")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("k")),
            ],
            || {
                assert!(HttpExtractor::from_env().is_none());
            },
        );
    }

    // ---- S3be.E.2 — #[ignore]'d live test (manual `cargo test -- --ignored`) ----

    /// Hits the real endpoint configured in `OPENCRAB_DISTILLER_*`. Not run
    /// in CI; run manually with:
    ///   `cargo test --bin opencrab-memory-mcp -- --ignored s3be_e2`
    /// after exporting the env vars. `OPENCRAB_DISTILLER_BASE_URL` must
    /// include the provider's API version segment (e.g.
    /// `https://api.openai.com/v1`, `https://dashscope.aliyuncs.com/compatible-mode/v1`);
    /// the extractor appends `/chat/completions` and nothing else.
    #[test]
    #[ignore]
    fn s3be_e2_live_extract_hits_real_endpoint() {
        let extractor = HttpExtractor::load()
            .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(extractor.extract(
            "USER: I keep crashing with 'duplicate column name: origin' when re-running cargo test.\n\
             ASSISTANT: ALTER TABLE ADD COLUMN is not idempotent — gate the step on PRAGMA user_version inside an IMMEDIATE transaction so a re-run is a no-op.",
            None,
        ));
        let kps = result.expect("live extract should succeed against the configured provider");
        eprintln!("[live] extracted {} knowledge point(s)", kps.len());
        for kp in &kps {
            eprintln!("[live]   kind={} summary={}", kp.kind, kp.summary);
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3b-distill — distill_once + URL/prompt fixes (S3d)
    // -----------------------------------------------------------------

    /// Block on a future on a temporary current_thread runtime — same flavor
    /// as the production bin so async behaviour matches.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    // ---- FakeExtractor — async trait impl, no network ----

    #[derive(Clone)]
    enum FakeResult {
        Ok(Vec<KnowledgePoint>),
        Transient(String),
        Client(String),
        Parse(String),
    }

    struct FakeExtractor {
        queue: std::sync::Mutex<std::collections::VecDeque<FakeResult>>,
        fallback: std::sync::Mutex<FakeResult>,
        calls: std::sync::Mutex<Vec<(String, Option<String>)>>,
    }

    impl FakeExtractor {
        fn always(result: FakeResult) -> Self {
            Self {
                queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
                fallback: std::sync::Mutex::new(result),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_sequence(seq: Vec<FakeResult>, fallback: FakeResult) -> Self {
            Self {
                queue: std::sync::Mutex::new(seq.into_iter().collect()),
                fallback: std::sync::Mutex::new(fallback),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn calls_snapshot(&self) -> Vec<(String, Option<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Extractor for FakeExtractor {
        async fn extract(
            &self,
            transcript: &str,
            prior_summary: Option<&str>,
        ) -> Result<Vec<KnowledgePoint>, ExtractError> {
            self.calls.lock().unwrap().push((
                transcript.to_string(),
                prior_summary.map(|s| s.to_string()),
            ));
            let next = self.queue.lock().unwrap().pop_front();
            let result = next.unwrap_or_else(|| self.fallback.lock().unwrap().clone());
            match result {
                FakeResult::Ok(kps) => Ok(kps),
                FakeResult::Transient(m) => Err(ExtractError::HttpTransient(m)),
                FakeResult::Client(m) => Err(ExtractError::HttpClient(m)),
                FakeResult::Parse(m) => Err(ExtractError::Parse(m)),
            }
        }
    }

    // ---- Seeding helpers (build raw_thread + raw_event rows) ----

    fn user_msg_str(text: &str) -> String {
        payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}]
            }
        }))
    }

    fn assistant_msg_str(text: &str) -> String {
        payload(json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }
        }))
    }

    fn compacted_str(summary: &str) -> String {
        payload(json!({
            "type": "compacted",
            "payload": {"message": summary}
        }))
    }

    fn reasoning_str(text: &str) -> String {
        payload(json!({
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": text}]
            }
        }))
    }

    fn event_msg_str() -> String {
        payload(json!({"type": "event_msg", "payload": {"type": "token_count"}}))
    }

    fn seed_distill_thread(
        conn: &Connection,
        thread_id: &str,
        last_line_no: i64,
        last_distilled_line_no: i64,
        last_growth_ts: Option<i64>,
        project_hash: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO raw_thread \
             (thread_id, agent_id, project_hash, source_path, \
              first_seen_ts, last_ingest_ts, last_offset, last_line_no, \
              last_distilled_line_no, last_growth_ts) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                thread_id,
                "agent-test",
                project_hash,
                "/tmp/r.jsonl",
                0_i64,
                0_i64,
                0_i64,
                last_line_no,
                last_distilled_line_no,
                last_growth_ts,
            ],
        )
        .unwrap();
    }

    fn seed_raw_event(conn: &Connection, thread_id: &str, line_no: i64, payload_str: &str) {
        conn.execute(
            "INSERT INTO raw_event(thread_id, line_no, payload, ingested_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![thread_id, line_no, payload_str, 0_i64],
        )
        .unwrap();
    }

    fn read_distilled_cursor(conn: &Connection, thread_id: &str) -> i64 {
        conn.query_row(
            "SELECT last_distilled_line_no FROM raw_thread WHERE thread_id = ?1",
            params![thread_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    // ==== A — trigger gate ====

    // S3d.A.1 — not idle AND pending < MAX → no trigger.
    #[test]
    fn s3d_a1_not_idle_low_pending_does_not_trigger() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        // last_growth_ts very recent → not idle.
        seed_distill_thread(&conn, "thr1", 3, 0, Some(now_ms - 1_000), None);
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("hello"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("hi"));
        seed_raw_event(&conn, "thr1", 3, &user_msg_str("ok"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_seen, 1);
        assert_eq!(stats.threads_triggered, 0);
        assert_eq!(stats.points_written, 0);
        assert_eq!(fake.call_count(), 0, "must not call LLM when not triggered");
        assert_eq!(read_distilled_cursor(&conn, "thr1"), 0, "cursor must not move");
    }

    // S3d.A.2 — idle (now_ms - growth > DISTILL_IDLE_MS) → trigger.
    #[test]
    fn s3d_a2_idle_triggers_distill() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            3,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("hello"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("hi"));
        seed_raw_event(&conn, "thr1", 3, &user_msg_str("ok"));

        let kp = KnowledgePoint {
            summary: "x".into(),
            detail: None,
            kind: "fact".into(),
        };
        let fake = FakeExtractor::always(FakeResult::Ok(vec![kp]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_triggered, 1);
        assert!(fake.call_count() >= 1, "idle thread should call LLM");
    }

    // S3d.A.3 — not idle BUT pending > MAX → trigger (safety valve).
    #[test]
    fn s3d_a3_pending_overflow_triggers_even_when_not_idle() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        let last_line = DISTILL_MAX_PENDING + 1;
        // last_growth_ts very recent → not idle, but pending > MAX.
        seed_distill_thread(
            &conn,
            "thr1",
            last_line,
            0,
            Some(now_ms - 1_000),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("flooding the buffer"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_triggered, 1);
    }

    // ==== B — write path ====

    // S3d.B.1 — extractor returns 2 KPs → both land in `log` with
    // origin='distill', project_hash propagated, kind preserved; cursor
    // advances to segment end; detail is FTS-searchable.
    #[test]
    fn s3d_b1_writes_kps_with_distill_origin_kind_and_project_hash() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            3,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            Some("proj-abc123"),
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("how do I do X?"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("by Y because Z"));
        seed_raw_event(&conn, "thr1", 3, &assistant_msg_str("done"));

        let kps = vec![
            KnowledgePoint {
                summary: "Use Y for X".into(),
                detail: Some("Because Z applies in this repo's setup".into()),
                kind: "decision".into(),
            },
            KnowledgePoint {
                summary: "Cache invalidation note".into(),
                detail: Some("a fenestration sentinel for FTS".into()),
                kind: "fact".into(),
            },
        ];
        let fake = FakeExtractor::always(FakeResult::Ok(kps));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.points_written, 2);

        let mut stmt = conn
            .prepare("SELECT summary, detail, origin, project_hash, kind FROM log ORDER BY id")
            .unwrap();
        let rows: Vec<(String, Option<String>, String, Option<String>, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for (_, _, origin, project_hash, kind) in &rows {
            assert_eq!(origin, "distill");
            assert_eq!(project_hash.as_deref(), Some("proj-abc123"));
            assert!(kind.is_some());
        }
        assert_eq!(rows[0].0, "Use Y for X");
        assert_eq!(rows[0].4.as_deref(), Some("decision"));
        assert_eq!(rows[1].0, "Cache invalidation note");
        assert_eq!(rows[1].4.as_deref(), Some("fact"));

        // FTS round-trip on a unique token in `detail`.
        let hits = block_on(search(&db, None, "fenestration", 6)).unwrap();
        assert!(!hits.is_empty(), "distilled `detail` must be FTS-searchable");

        assert_eq!(read_distilled_cursor(&conn, "thr1"), 3);
    }

    // ==== C — segments + prior_summary ====

    // S3d.C.1 — thread with a mid-stream compacted boundary: extractor
    // gets two calls; the second carries the marker's `message` as
    // `prior_summary`; cursor lands on the last line.
    #[test]
    fn s3d_c1_compacted_thread_passes_prior_summary_to_extractor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            5,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u1"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a1"));
        seed_raw_event(&conn, "thr1", 3, &compacted_str("rollup-summary"));
        seed_raw_event(&conn, "thr1", 4, &user_msg_str("u2"));
        seed_raw_event(&conn, "thr1", 5, &assistant_msg_str("a2"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let _ = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        let calls = fake.calls_snapshot();
        assert_eq!(calls.len(), 2, "two segments → two extract calls");
        assert!(
            calls[0].1.is_none(),
            "first segment has no prior_summary (no boundary before it)"
        );
        assert_eq!(
            calls[1].1.as_deref(),
            Some("rollup-summary"),
            "second segment carries the marker's message as prior_summary"
        );
        assert_eq!(read_distilled_cursor(&conn, "thr1"), 5);
    }

    // S3d.C.2 — segment whose lines are all Dropped (reasoning + event_msg)
    // → LLM is NOT called, but the cursor still advances past the
    // segment so we don't look at these lines next pass.
    #[test]
    fn s3d_c2_empty_transcript_segment_skips_llm_but_advances_cursor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            3,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &reasoning_str("internal thought"));
        seed_raw_event(&conn, "thr1", 2, &event_msg_str());
        seed_raw_event(&conn, "thr1", 3, &reasoning_str("more thinking"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(
            fake.call_count(),
            0,
            "all-Dropped segment must not invoke the LLM"
        );
        assert_eq!(stats.segments_processed, 1, "the segment still counts");
        assert_eq!(
            read_distilled_cursor(&conn, "thr1"),
            3,
            "cursor advances past the empty segment so we don't see it again"
        );
        let log_count: i64 = count_rows(&conn, "SELECT count(*) FROM log");
        assert_eq!(log_count, 0);
    }

    // ==== D — error strategy ====

    // S3d.D.1 — transient error: 0 log rows, cursor stays put (retry on
    // the next pass).
    #[test]
    fn s3d_d1_transient_error_does_not_advance_cursor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            2,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a"));

        let fake = FakeExtractor::always(FakeResult::Transient("503 backend".into()));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.transient_errors, 1);
        assert_eq!(stats.points_written, 0);
        assert_eq!(
            read_distilled_cursor(&conn, "thr1"),
            0,
            "cursor must stay put on transient — next pass retries"
        );
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM log"), 0);
    }

    // S3d.D.2 — client error: same handling as transient (cursor stays,
    // log empty), but counted separately for visibility.
    #[test]
    fn s3d_d2_client_error_does_not_advance_cursor() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            2,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a"));

        let fake = FakeExtractor::always(FakeResult::Client("401 unauthorized".into()));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.client_errors, 1);
        assert_eq!(stats.points_written, 0);
        assert_eq!(read_distilled_cursor(&conn, "thr1"), 0);
        assert_eq!(count_rows(&conn, "SELECT count(*) FROM log"), 0);
    }

    // S3d.D.3 — parse error: 0 log rows for THAT segment, cursor
    // ADVANCES past it (so we don't loop on garbage); a subsequent
    // segment in the same thread is still processed normally.
    #[test]
    fn s3d_d3_parse_error_advances_cursor_past_segment_continues_to_next() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            5,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        seed_raw_event(&conn, "thr1", 1, &user_msg_str("u1"));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("a1"));
        seed_raw_event(&conn, "thr1", 3, &compacted_str("rollup"));
        seed_raw_event(&conn, "thr1", 4, &user_msg_str("u2"));
        seed_raw_event(&conn, "thr1", 5, &assistant_msg_str("a2"));

        let kp = KnowledgePoint {
            summary: "from seg2".into(),
            detail: None,
            kind: "fact".into(),
        };
        let fake = FakeExtractor::with_sequence(
            vec![
                FakeResult::Parse("LLM emitted prose-only".into()),
                FakeResult::Ok(vec![kp]),
            ],
            FakeResult::Ok(vec![]),
        );
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.parse_skips, 1, "exactly one parse skip");
        assert_eq!(stats.points_written, 1, "second segment's KP still landed");

        assert_eq!(
            read_distilled_cursor(&conn, "thr1"),
            5,
            "cursor advanced past BOTH the parse-skipped segment AND the good one"
        );

        let log_count: i64 = count_rows(&conn, "SELECT count(*) FROM log");
        assert_eq!(log_count, 1);
        let (summary, kind): (String, Option<String>) = conn
            .query_row(
                "SELECT summary, kind FROM log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(summary, "from seg2");
        assert_eq!(kind.as_deref(), Some("fact"));
    }

    // ==== E — idempotency ====

    // S3d.E.1 — a fully-distilled thread (last_distilled == last_line) is
    // not even surfaced by the candidate query, so the LLM is never called.
    #[test]
    fn s3d_e1_fully_distilled_thread_does_not_re_trigger() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            5,
            5,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        for i in 1..=5_i64 {
            seed_raw_event(&conn, "thr1", i, &user_msg_str(&format!("u{i}")));
        }
        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let stats = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        assert_eq!(stats.threads_seen, 0);
        assert_eq!(stats.threads_triggered, 0);
        assert_eq!(fake.call_count(), 0);
    }

    // ==== F — truncation ====

    // S3d.F.1 — a segment whose transcript exceeds DISTILL_MAX_TRANSCRIPT_CHARS
    // is delivered to the extractor with the head dropped and the marker
    // prepended; total chars never exceed the cap.
    #[test]
    fn s3d_f1_long_transcript_is_truncated_with_marker() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let now_ms = 1_000_000_i64;
        seed_distill_thread(
            &conn,
            "thr1",
            2,
            0,
            Some(now_ms - DISTILL_IDLE_MS - 1),
            None,
        );
        let huge = "X".repeat(DISTILL_MAX_TRANSCRIPT_CHARS + 1_000);
        seed_raw_event(&conn, "thr1", 1, &user_msg_str(&huge));
        seed_raw_event(&conn, "thr1", 2, &assistant_msg_str("tail message"));

        let fake = FakeExtractor::always(FakeResult::Ok(vec![]));
        let _ = block_on(distill_once(&conn, &fake, now_ms)).unwrap();

        let mut calls = fake.calls_snapshot();
        let last = calls.pop().expect("LLM should have been called once");
        let transcript = last.0;
        let char_count = transcript.chars().count();
        assert!(
            char_count <= DISTILL_MAX_TRANSCRIPT_CHARS,
            "got {char_count} chars, max {DISTILL_MAX_TRANSCRIPT_CHARS}"
        );
        assert!(
            transcript.starts_with("[...earlier content truncated...]"),
            "must start with truncation marker; head was: {:?}",
            transcript.chars().take(60).collect::<String>()
        );
        // Tail content survives.
        assert!(transcript.ends_with("tail message"));
    }

    // ==== G — migration v3→v4 + full v0→v4 + log_progress untouched ====

    // S3d.G.1 — v3 DB upgrades to v4: `log.kind` (nullable) gets added;
    // pre-existing log rows have NULL kind.
    #[test]
    fn s3d_g1_upgrades_v3_db_to_v4_adds_log_kind_nullable() {
        let (_tmp, db) = db_path();
        build_v2_db(&db);
        {
            // Hand-roll the v3 step on top of the v2-shape DB.
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "ALTER TABLE raw_thread ADD COLUMN last_distilled_line_no INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE raw_thread ADD COLUMN last_growth_ts INTEGER;
                 PRAGMA user_version = 3;",
            )
            .unwrap();
            // Pre-v4 log row: no kind column to set.
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) VALUES (?1, ?2, ?3, ?4)",
                params![100_i64, "old summary", "old detail", "self"],
            )
            .unwrap();
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        let cols = log_columns(&conn);
        assert!(cols.contains("kind"), "v4 must add the `kind` column to log");

        let (summary, detail, origin, kind): (String, Option<String>, String, Option<String>) =
            conn.query_row(
                "SELECT summary, detail, origin, kind FROM log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(summary, "old summary");
        assert_eq!(detail.as_deref(), Some("old detail"));
        assert_eq!(origin, "self");
        assert!(
            kind.is_none(),
            "pre-v4 row picks up kind = NULL on upgrade"
        );
    }

    // S3d.G.2 — full v0 → v1 → v2 → v3 → v4 chain in one open().
    #[test]
    fn s3d_g2_upgrades_v0_db_through_full_chain_to_v4() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        // log: v1 cols + v4 `kind`.
        assert!(log_columns(&conn).contains("kind"));
        // raw_thread: v2 + v3 cols.
        assert_eq!(
            table_columns(&conn, "raw_thread"),
            expected_raw_thread_columns()
        );
        // v0 log rows preserved.
        let count: i64 = conn
            .query_row("SELECT count(*) FROM log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, seeded.len());
    }

    // S3d.G.3 — log_progress (user-side) keeps `kind = NULL`. The
    // signature is unchanged; only the distiller writes a kind.
    #[test]
    fn s3d_g3_log_progress_writes_with_null_kind_unchanged() {
        let (_tmp, db) = db_path();
        let id = log_progress(&db, "user note", Some("by hand")).unwrap();
        let conn = open(&db).unwrap();
        let (origin, kind): (String, Option<String>) = conn
            .query_row(
                "SELECT origin, kind FROM log WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(origin, "self");
        assert!(
            kind.is_none(),
            "log_progress writes don't set kind (signature unchanged)"
        );
    }

    // ==== H — S3b-extract fix regressions ====

    // S3d.H.1 — chat_completions_url does NOT inject `/v1`; it appends
    // exactly `/chat/completions` (handling a trailing slash on base).
    #[test]
    fn s3d_h1_chat_completions_url_does_not_inject_v1() {
        let e1 = test_http_extractor("https://api.openai.com/v1", "m", "k");
        assert_eq!(
            e1.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        let e2 = test_http_extractor("https://api.openai.com/v1/", "m", "k");
        assert_eq!(
            e2.chat_completions_url(),
            "https://api.openai.com/v1/chat/completions"
        );
        // Anti-regression: no double `/v1/v1` even if the base ended with /.
        assert!(!e1.chat_completions_url().contains("/v1/v1"));
        assert!(!e2.chat_completions_url().contains("/v1/v1"));
        // A non-v1-style base (e.g. dashscope) keeps its own version prefix.
        let e3 = test_http_extractor(
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "qwen-max",
            "sk-test",
        );
        assert_eq!(
            e3.chat_completions_url(),
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
    }

    // S3d.H.2 — PROMPT has ≥2 few-shot examples plus an empty-output demo,
    // and the OUTPUT FORMAT section mentions the 0–5 soft cap.
    #[test]
    fn s3d_h2_prompt_has_multiple_examples_and_empty_result_demo() {
        // The failure example (existing).
        assert!(
            EXTRACTION_PROMPT.contains("\"kind\":\"failure\""),
            "Example 1 (failure) is missing"
        );
        // The decision example (S3-distill addition).
        assert!(
            EXTRACTION_PROMPT.contains("\"kind\":\"decision\""),
            "Example 2 (decision) is missing"
        );
        // The empty-result demo: contains the literal user/assistant
        // pair AND shows `[]` as the expected output.
        assert!(
            EXTRACTION_PROMPT.contains("thanks, that worked!"),
            "Example 3 (empty result) user line is missing"
        );
        assert!(
            EXTRACTION_PROMPT.contains("Glad it helped!"),
            "Example 3 (empty result) assistant line is missing"
        );
        // Soft cap line (the en-dash variant the spec uses).
        assert!(
            EXTRACTION_PROMPT.contains("0–5"),
            "Soft cap '0–5 points' is missing from OUTPUT FORMAT"
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-wire — shrink-resets-distill + end-to-end ignored
    // -----------------------------------------------------------------

    // S3w.A.1 — when the ingester detects a shrunk file and DELETEs the
    // raw_event rows, it must also reset `last_distilled_line_no` to 0.
    // Otherwise the distiller would skip re-distilling the new file
    // content under the assumption "I've already consumed lines 1..N",
    // silently going out of sync.
    #[test]
    fn s3w_a1_shrink_resets_distill_cursor_and_re_reads_file() {
        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_shrink_distill";
        let rollout_path = write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            "a\nb\nc\nd\ne\n", // 5 lines, 10 bytes
        );
        let conn = open(&memory_db).unwrap();
        let _first = ingest_once(&conn, &scan_root, agent_id).unwrap();

        // Simulate the distiller having consumed every line.
        conn.execute(
            "UPDATE raw_thread SET last_distilled_line_no = ?1 WHERE thread_id = ?2",
            params![5_i64, SAMPLE_THREAD_UUID],
        )
        .unwrap();

        // Truncate the file to fewer (and DIFFERENT) lines.
        std::fs::write(&rollout_path, "x\ny\n").unwrap();

        let _second = ingest_once(&conn, &scan_root, agent_id).unwrap();

        // raw_event now matches the file exactly (orphans gone, no dups).
        assert_eq!(
            fetch_event_payloads(&conn, SAMPLE_THREAD_UUID),
            vec![(1, "x".to_string()), (2, "y".to_string())]
        );

        // Ingester cursor + distill cursor:
        let (last_offset, last_line_no, last_distilled): (i64, i64, i64) = conn
            .query_row(
                "SELECT last_offset, last_line_no, last_distilled_line_no \
                 FROM raw_thread WHERE thread_id = ?1",
                params![SAMPLE_THREAD_UUID],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(last_offset, 4, "ingester cursor lands on the shrunk EOF");
        assert_eq!(last_line_no, 2);
        assert_eq!(
            last_distilled, 0,
            "shrink MUST reset last_distilled_line_no so the new content gets re-distilled"
        );
    }

    // S3w.C.1 — end-to-end ingest→distill against the real configured
    // provider. Not in CI; run manually with:
    //   `cargo test --bin opencrab-memory-mcp -- --ignored s3w_c1`
    // with `OPENCRAB_DISTILLER_{BASE_URL,MODEL,API_KEY}` exported.
    #[test]
    #[ignore]
    fn s3w_c1_e2e_ingest_then_distill_writes_distill_origin_log_row() {
        let extractor = HttpExtractor::load()
            .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");

        let (_tmp, memory_db, scan_root) = ingest_test_env();
        let agent_id = "agent_s3w_c1";

        // A small but rich transcript: a real failure-with-fix the LLM
        // can almost certainly extract.
        let transcript_lines = vec![
            user_msg_str("My cargo test keeps panicking — what should I check?"),
            assistant_msg_str("I'll look at the failure."),
            assistant_msg_str(
                "The panic is 'SqliteFailure: duplicate column name: origin' — \
                 ALTER TABLE ADD COLUMN ran twice. The fix is to wrap the step in \
                 PRAGMA user_version gating inside an IMMEDIATE transaction so a \
                 re-run is a no-op.",
            ),
        ];
        let content = transcript_lines.join("\n") + "\n";
        write_rollout(
            &scan_root,
            SAMPLE_TEAM_ID,
            SAMPLE_PROJECT_HASH,
            SAMPLE_THREAD_UUID,
            SAMPLE_ISO_TS,
            &content,
        );

        let conn = open(&memory_db).unwrap();
        let ingest_stats = ingest_once(&conn, &scan_root, agent_id).unwrap();
        assert!(ingest_stats.events_inserted >= 3);

        // Force the idle trigger by handing distill_once a now_ms far
        // ahead of the just-set last_growth_ts.
        let now_ms = current_time_ms() + DISTILL_IDLE_MS + 60_000;
        let stats = block_on(distill_once(&conn, &extractor, now_ms)).unwrap();

        assert_eq!(stats.threads_seen, 1);
        assert_eq!(stats.threads_triggered, 1);
        eprintln!("[live] distill stats: {stats:?}");

        let distill_rows: i64 = count_rows(
            &conn,
            "SELECT count(*) FROM log WHERE origin = 'distill'",
        );
        assert!(
            distill_rows >= 1,
            "the live extractor should have produced at least one distilled row"
        );
        let mut stmt = conn
            .prepare(
                "SELECT summary, detail, kind FROM log \
                 WHERE origin = 'distill' ORDER BY id",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap()
        {
            let (summary, detail, kind) = row.unwrap();
            eprintln!(
                "[live] distilled: kind={:?} summary={} detail={:?}",
                kind, summary, detail
            );
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-cfg — env / file credential loading + think-strip
    // -----------------------------------------------------------------

    /// Write a `distiller.json` under a temp HOME's `.opencrab/` dir so
    /// the file fallback in `HttpExtractor::load()` finds it.
    fn write_distiller_json(tmp: &std::path::Path, base_url: &str, model: &str, api_key: &str) {
        let opencrab_dir = tmp.join(".opencrab");
        std::fs::create_dir_all(&opencrab_dir).unwrap();
        let body = serde_json::json!({
            "base_url": base_url,
            "model": model,
            "api_key": api_key,
        })
        .to_string();
        std::fs::write(opencrab_dir.join("distiller.json"), body).unwrap();
    }

    // S3cfg.A.1 — env vars win over a (different) on-disk file.
    #[test]
    fn s3cfg_a1_env_present_takes_priority_over_file() {
        let tmp = tempfile::tempdir().unwrap();
        // File holds values we MUST NOT see in the loaded extractor.
        write_distiller_json(
            tmp.path(),
            "https://this-must-not-win.example/v1",
            "wrong-model",
            "sk-wrong",
        );

        let home_str = tmp.path().to_str().unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://env.example/v1")),
                ("OPENCRAB_DISTILLER_MODEL", Some("env-model")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("sk-env")),
            ],
            || {
                let extractor = HttpExtractor::load().expect("env should produce Some");
                assert_eq!(extractor.base_url, "https://env.example/v1");
                assert_eq!(extractor.model, "env-model");
                assert_eq!(extractor.api_key, "sk-env");
            },
        );
    }

    // S3cfg.A.2 — env absent → file fallback wins.
    #[test]
    fn s3cfg_a2_env_absent_falls_back_to_distiller_json() {
        let tmp = tempfile::tempdir().unwrap();
        write_distiller_json(
            tmp.path(),
            "https://file-fallback.example/v1",
            "file-model",
            "sk-file",
        );

        let home_str = tmp.path().to_str().unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_MODEL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
            ],
            || {
                let extractor =
                    HttpExtractor::load().expect("file fallback should produce Some");
                assert_eq!(extractor.base_url, "https://file-fallback.example/v1");
                assert_eq!(extractor.model, "file-model");
                assert_eq!(extractor.api_key, "sk-file");
            },
        );
    }

    // S3cfg.A.3 — neither env nor file → None.
    #[test]
    fn s3cfg_a3_neither_env_nor_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        // Don't create distiller.json — even the dir is absent.
        let home_str = tmp.path().to_str().unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_MODEL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
            ],
            || {
                assert!(
                    HttpExtractor::load().is_none(),
                    "no env + no file → distillation disabled"
                );
            },
        );
    }

    // S3cfg.B.1 — `<think>…</think>` blocks are stripped before the JSON
    // array is sliced out. The think text contains `[` that would
    // otherwise hijack `extract_json_array_slice`.
    #[test]
    fn s3cfg_b1_strips_paired_think_blocks_before_parsing() {
        let s = "<think>let me output [stuff] and reason</think>\n\
                 [{\"summary\":\"s\",\"detail\":null,\"kind\":\"fact\"}]";
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1, "exactly one KP after think-strip");
        assert_eq!(kps[0].summary, "s");
        assert_eq!(kps[0].kind, "fact");
    }

    // S3cfg.B.1 (extra) — multiple paired blocks; all paired, all stripped.
    #[test]
    fn s3cfg_b1_strips_multiple_paired_think_blocks() {
        let s = "<think>first thinking with [</think>some text\n\
                 <think>second [thinking step</think>\n\
                 [{\"summary\":\"sharp\",\"kind\":\"decision\"}]";
        let kps = parse_knowledge_points(s).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "sharp");
        assert_eq!(kps[0].kind, "decision");
    }

    // S3cfg.B.2 — no think blocks → identical to pre-S3cfg parse behaviour.
    #[test]
    fn s3cfg_b2_no_think_blocks_unchanged_behavior() {
        let clean = r#"[{"summary":"plain","detail":"d","kind":"pattern"}]"#;
        let kps = parse_knowledge_points(clean).unwrap();
        assert_eq!(kps.len(), 1);
        assert_eq!(kps[0].summary, "plain");
        assert_eq!(kps[0].detail.as_deref(), Some("d"));
        assert_eq!(kps[0].kind, "pattern");

        // Fenced (S3be regression coverage): the think-strip pass should
        // be a no-op when there are no think tags, so the fence path
        // continues to work.
        let fenced = "```json\n[{\"summary\":\"in-fence\",\"kind\":\"fact\"}]\n```";
        let kps2 = parse_knowledge_points(fenced).unwrap();
        assert_eq!(kps2.len(), 1);
        assert_eq!(kps2[0].summary, "in-fence");
    }

    // S3cfg.B.1 (defensive) — unclosed `<think>` left untouched; the
    // parse fails naturally (no JSON afterwards). No panic.
    #[test]
    fn s3cfg_b1_unclosed_think_block_left_untouched_and_parse_errs() {
        let s = "<think>this never closes, and there is no array";
        assert!(parse_knowledge_points(s).is_err());
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 3-val — one-shot quality look at a REAL rollout
    // -----------------------------------------------------------------

    /// Drive the full S2 ingest → S3 distill chain against a *real*
    /// agent's rollout files. Not a CI test — runs only with:
    ///   `cargo test --bin opencrab-memory-mcp -- --ignored \
    ///        s3val_real_rollout_distill --nocapture`
    /// (and the distiller must be configured via env or
    /// `~/.opencrab/distiller.json`).
    ///
    /// The agent below was selected because its `team_sessions/`
    /// holds rollouts with substantive content (a real DB-choice
    /// decision the PM was asked to record), not just kickoff stubs.
    /// Re-point at a different agent if this one is no longer present.
    #[test]
    #[ignore]
    fn s3val_real_rollout_distill() {
        let extractor = HttpExtractor::load()
            .expect("OPENCRAB_DISTILLER_* env OR ~/.opencrab/distiller.json required");

        let home = std::env::var("HOME").expect("HOME");
        let scan_root: std::path::PathBuf = std::path::PathBuf::from(home).join(
            ".opencrab/agents/agent_1e17677e-4a5b-40db-8113-3aeba71635d4/team_sessions",
        );
        assert!(
            scan_root.exists(),
            "no scan_root at {} — re-point at a real agent dir or skip",
            scan_root.display()
        );

        let tmp = tempfile::tempdir().unwrap();
        let memory_db = tmp.path().join("memory.db");

        let conn = open(&memory_db).unwrap();
        let ingest = ingest_once(&conn, &scan_root, "agent_s3val").unwrap();
        eprintln!("[s3val] ingest stats: {ingest:?}");

        // Thread-scale glimpse: how big is each thread?
        let mut stmt = conn
            .prepare(
                "SELECT thread_id, last_line_no FROM raw_thread ORDER BY thread_id",
            )
            .unwrap();
        for row in stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
        {
            let (tid, lines) = row.unwrap();
            eprintln!("[s3val] thread {} = {} lines", tid, lines);
        }

        // Force the idle trigger so every thread distills this pass.
        let far_future = current_time_ms() + DISTILL_IDLE_MS + 60_000;
        let stats = block_on(distill_once(&conn, &extractor, far_future)).unwrap();
        eprintln!("[s3val] distill stats: {stats:?}");

        // Dump every distilled row verbatim.
        let mut stmt = conn
            .prepare(
                "SELECT id, summary, detail, kind FROM log \
                 WHERE origin = 'distill' ORDER BY id",
            )
            .unwrap();
        let rows: Vec<(i64, String, Option<String>, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("[s3val] total distilled rows = {}", rows.len());
        for (id, summary, detail, kind) in &rows {
            eprintln!("[s3val] -------- log id={} kind={:?}", id, kind);
            eprintln!("[s3val]   summary: {}", summary);
            eprintln!("[s3val]   detail:  {:?}", detail);
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 4-schema — sqlite-vec registration + log_vec (v5)
    // -----------------------------------------------------------------

    /// Frozen v4 final form: log v1 + provenance + idx_log_ts + `kind`,
    /// raw_thread v3 (14 cols), raw_event, idx_raw_event_thread.
    /// Used by the v4→v5 upgrade test so it doesn't depend on live
    /// migrate code.
    const V4_DDL_FROZEN: &str = "CREATE TABLE log (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        ts           INTEGER NOT NULL,
        summary      TEXT    NOT NULL,
        detail       TEXT,
        origin       TEXT    NOT NULL DEFAULT 'self',
        project_hash TEXT,
        kind         TEXT
    );
    CREATE VIRTUAL TABLE log_fts USING fts5(
        detail,
        content='log',
        content_rowid='id'
    );
    CREATE TRIGGER log_ai AFTER INSERT ON log BEGIN
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE TRIGGER log_ad AFTER DELETE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
    END;
    CREATE TRIGGER log_au AFTER UPDATE ON log BEGIN
        INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
    END;
    CREATE INDEX idx_log_ts ON log(ts);
    CREATE TABLE raw_thread (
        thread_id              TEXT PRIMARY KEY,
        agent_id               TEXT,
        team_id                TEXT,
        project_hash           TEXT,
        source                 TEXT,
        parent_thread_id       TEXT,
        cwd                    TEXT,
        source_path            TEXT NOT NULL,
        first_seen_ts          INTEGER NOT NULL,
        last_ingest_ts         INTEGER NOT NULL,
        last_offset            INTEGER NOT NULL DEFAULT 0,
        last_line_no           INTEGER NOT NULL DEFAULT 0,
        last_distilled_line_no INTEGER NOT NULL DEFAULT 0,
        last_growth_ts         INTEGER
    );
    CREATE TABLE raw_event (
        id          INTEGER PRIMARY KEY,
        thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
        line_no     INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        ingested_at INTEGER NOT NULL,
        UNIQUE(thread_id, line_no)
    );
    CREATE INDEX idx_raw_event_thread ON raw_event(thread_id);";

    /// Lay down a v4-shape DB and stamp `PRAGMA user_version = 4`.
    fn build_v4_db(db_path: &std::path::Path) {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(db_path).unwrap();
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        conn.execute_batch(V4_DDL_FROZEN).unwrap();
        conn.execute_batch("PRAGMA user_version = 4;").unwrap();
    }

    /// Serialise an `[f32; 1024]` to the JSON-array form that vec0's
    /// MATCH operator accepts. Test-only.
    fn vec_to_json(v: &[f32]) -> String {
        let mut s = String::with_capacity(v.len() * 6 + 2);
        s.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("{x}"));
        }
        s.push(']');
        s
    }

    /// Build a 1024-dim vector that's mostly zeros except for one
    /// component set to 1.0 at `axis` — gives us unambiguous KNN
    /// ordering: querying near `axis=i` returns rowid `i+1` first.
    fn axis_unit_vec(axis: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 1024];
        v[axis] = 1.0;
        v
    }

    // S4s.A.1 — fresh DB lands on v5 with log_vec present, and vec0
    // is actually usable (vec_version() returns something).
    #[test]
    fn s4s_a1_fresh_db_lands_on_v5_with_log_vec_table() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        assert!(schema_object_exists(&conn, "table", "log_vec"));

        // vec_version() proves the extension actually registered.
        let vec_version: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .unwrap();
        assert!(
            !vec_version.is_empty(),
            "vec_version() must return something, got {vec_version:?}"
        );
    }

    // S4s.A.2 — log_vec accepts a 1024-dim insert tied to a parent log row.
    #[test]
    fn s4s_a2_log_vec_accepts_1024_dim_insert() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        // Need a real log row so the rowid is meaningful for join tests.
        conn.execute(
            "INSERT INTO log(ts, summary, detail, origin) VALUES (?1, ?2, ?3, ?4)",
            params![1_i64, "subject", "body", "self"],
        )
        .unwrap();
        let log_id = conn.last_insert_rowid();

        let vec = axis_unit_vec(0);
        let json = vec_to_json(&vec);
        conn.execute(
            "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
            params![log_id, &json],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT count(*) FROM log_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // S4s.B.1 — three axis-unit vectors → KNN against a vector close
    // to one axis returns that rowid first; distances are non-decreasing.
    #[test]
    fn s4s_b1_knn_ranks_nearest_rowid_first() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        for (rowid, axis) in [(1_i64, 0_usize), (2, 1), (3, 2)] {
            let v = axis_unit_vec(axis);
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![rowid, vec_to_json(&v)],
            )
            .unwrap();
        }

        // Query vector very close to axis-0 (rowid 1).
        let mut q = vec![0.0_f32; 1024];
        q[0] = 0.95;
        q[1] = 0.05;
        let q_json = vec_to_json(&q);

        let mut stmt = conn
            .prepare(
                "SELECT rowid, distance FROM log_vec \
                 WHERE embedding MATCH ?1 ORDER BY distance LIMIT 3",
            )
            .unwrap();
        let rows: Vec<(i64, f64)> = stmt
            .query_map(params![&q_json], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(rows.len(), 3, "should get 3 KNN hits");
        assert_eq!(rows[0].0, 1, "rowid 1 (axis-0) ranks first");
        // Non-decreasing distance.
        for w in rows.windows(2) {
            assert!(w[0].1 <= w[1].1, "distances must be sorted: {rows:?}");
        }
    }

    // S4s.B.2 — JOIN log_vec back to log via rowid = log.id, surfacing
    // summary + distance in one query (the shape memory_search will use).
    #[test]
    fn s4s_b2_join_log_with_log_vec_returns_summary_and_distance() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        // Three log rows, three matching vectors.
        let labels = ["first", "second", "third"];
        for (axis, label) in labels.iter().enumerate() {
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![(axis as i64) + 1, *label, "x", "self"],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            let v = axis_unit_vec(axis);
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_to_json(&v)],
            )
            .unwrap();
        }

        // Query near axis-1 ("second") and confirm the JOIN shape.
        let mut q = vec![0.0_f32; 1024];
        q[1] = 1.0;
        let q_json = vec_to_json(&q);

        // Note the `k = ?2` constraint: vec0 needs LIMIT or `k = ?` to
        // appear directly on its scan. With a JOIN, an outer `LIMIT 3`
        // wouldn't reach vec0's xBestIndex, so we pass `k` instead.
        let mut stmt = conn
            .prepare(
                "SELECT log.id, log.summary, log_vec.distance \
                 FROM log_vec JOIN log ON log.id = log_vec.rowid \
                 WHERE log_vec.embedding MATCH ?1 AND log_vec.k = ?2 \
                 ORDER BY log_vec.distance",
            )
            .unwrap();
        let rows: Vec<(i64, String, f64)> = stmt
            .query_map(params![&q_json, 3_i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, "second", "nearest hit's log.summary == 'second'");
    }

    // S4s.C.1 — v4 DB upgrades to v5: existing log rows survive; log_vec
    // appears empty.
    #[test]
    fn s4s_c1_upgrades_v4_db_to_v5_preserving_log_rows() {
        let (_tmp, db) = db_path();
        build_v4_db(&db);
        // Seed a log row pre-v5 (no log_vec yet).
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO log(ts, summary, detail, origin, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![42_i64, "pre-v5", "body", "self", None::<&str>],
            )
            .unwrap();
        }

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
        assert!(schema_object_exists(&conn, "table", "log_vec"));

        let (summary, kind): (String, Option<String>) = conn
            .query_row(
                "SELECT summary, kind FROM log",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(summary, "pre-v5");
        assert!(kind.is_none());

        let count: i64 = conn
            .query_row("SELECT count(*) FROM log_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "fresh v5 log_vec starts empty");
    }

    // S4s.C.2 — full v0 → v1 → v2 → v3 → v4 → v5 chain in one open();
    // v0 log rows preserved; every per-version product present.
    #[test]
    fn s4s_c2_upgrades_v0_db_through_full_chain_to_v5() {
        let (_tmp, db) = db_path();
        let seeded = build_v0_db(&db);

        let conn = open(&db).unwrap();
        assert_eq!(read_user_version(&conn), SCHEMA_VERSION);

        // v1 product
        assert!(log_columns(&conn).contains("kind"));
        assert!(schema_object_exists(&conn, "index", "idx_log_ts"));
        // v2 product
        assert!(schema_object_exists(&conn, "table", "raw_thread"));
        assert!(schema_object_exists(&conn, "table", "raw_event"));
        assert!(schema_object_exists(&conn, "index", "idx_raw_event_thread"));
        // v3 product
        assert_eq!(
            table_columns(&conn, "raw_thread"),
            expected_raw_thread_columns()
        );
        // v4 product (kind on log)
        assert!(log_columns(&conn).contains("kind"));
        // v5 product
        assert!(schema_object_exists(&conn, "table", "log_vec"));

        let count: i64 = conn
            .query_row("SELECT count(*) FROM log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count as usize, seeded.len(), "v0 log rows preserved");
    }

    // S4s.C.3 — open() three times in a row is idempotent on v5;
    // `CREATE VIRTUAL TABLE IF NOT EXISTS` doesn't re-create log_vec.
    #[test]
    fn s4s_c3_open_thrice_is_idempotent_at_v5() {
        let (_tmp, db) = db_path();
        for _ in 0..3 {
            let conn = open(&db).unwrap();
            assert_eq!(read_user_version(&conn), SCHEMA_VERSION);
            assert!(schema_object_exists(&conn, "table", "log_vec"));
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 4-embed — Embedder + embed_pending_once
    // -----------------------------------------------------------------

    /// In-memory embedder for tests. No network. Records every call's
    /// `texts` slice + supports three behaviours: per-text axis-units
    /// (good for KNN assertions), a fixed constant vector for everyone,
    /// or always-error.
    struct FakeEmbedder {
        dimensions: usize,
        behavior: std::sync::Mutex<FakeEmbedBehavior>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }

    #[derive(Clone)]
    enum FakeEmbedBehavior {
        /// `texts[i]` → unit vector along axis `i`. Distinguishable in KNN.
        AxisUnits,
        /// Every text → the same vector.
        Constant(Vec<f32>),
        /// Always return this error.
        Err(EmbedError),
    }

    impl FakeEmbedder {
        fn axis_units(dimensions: usize) -> Self {
            Self {
                dimensions,
                behavior: std::sync::Mutex::new(FakeEmbedBehavior::AxisUnits),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn constant(dimensions: usize, v: Vec<f32>) -> Self {
            assert_eq!(v.len(), dimensions);
            Self {
                dimensions,
                behavior: std::sync::Mutex::new(FakeEmbedBehavior::Constant(v)),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn errors_with(dimensions: usize, err: EmbedError) -> Self {
            Self {
                dimensions,
                behavior: std::sync::Mutex::new(FakeEmbedBehavior::Err(err)),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        fn batches(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Embedder for FakeEmbedder {
        fn dimensions(&self) -> usize {
            self.dimensions
        }

        async fn embed(
            &self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.calls.lock().unwrap().push(texts.to_vec());
            let beh = self.behavior.lock().unwrap().clone();
            match beh {
                FakeEmbedBehavior::AxisUnits => {
                    let mut out = Vec::with_capacity(texts.len());
                    for i in 0..texts.len() {
                        let mut v = vec![0.0_f32; self.dimensions];
                        if i < self.dimensions {
                            v[i] = 1.0;
                        }
                        out.push(v);
                    }
                    Ok(out)
                }
                FakeEmbedBehavior::Constant(v) => {
                    Ok(texts.iter().map(|_| v.clone()).collect())
                }
                FakeEmbedBehavior::Err(e) => Err(e),
            }
        }
    }

    fn test_http_embedder(
        base_url: &str,
        api_key: &str,
        model: &str,
        dimensions: usize,
    ) -> HttpEmbedder {
        HttpEmbedder {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            dimensions,
        }
    }

    fn seed_log_row(conn: &Connection, summary: &str, detail: Option<&str>) -> i64 {
        conn.execute(
            "INSERT INTO log(ts, summary, detail, origin) VALUES (?1, ?2, ?3, ?4)",
            params![current_time_ms(), summary, detail, "self"],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn log_vec_rowids(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn.prepare("SELECT rowid FROM log_vec ORDER BY rowid").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    // ==== A — request build + response parse + load() ====

    // S4e.A.1 — request body has model + input array + dimensions.
    #[test]
    fn s4e_a1_build_request_body_shape() {
        let e = test_http_embedder("https://api.example/v1", "sk-x", "model-y", 1024);
        let texts = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let body = e.build_request_body(&texts);
        assert_eq!(body.get("model").and_then(|v| v.as_str()), Some("model-y"));
        assert_eq!(
            body.get("dimensions").and_then(|v| v.as_u64()),
            Some(1024)
        );
        let input = body.get("input").and_then(|v| v.as_array()).unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0].as_str(), Some("one"));
        assert_eq!(input[1].as_str(), Some("two"));
        assert_eq!(input[2].as_str(), Some("three"));

        // URL: never re-inject `/v1`.
        assert_eq!(
            e.embeddings_url(),
            "https://api.example/v1/embeddings",
            "embeddings_url must append /embeddings, not /v1/embeddings"
        );
    }

    // S4e.A.2 — out-of-order data[] is re-sorted by `index`.
    #[test]
    fn s4e_a2_parse_response_reorders_by_index() {
        let body = json!({
            "object": "list",
            "model": "m",
            "data": [
                {"index": 2, "object": "embedding", "embedding": [0.0, 0.0, 1.0]},
                {"index": 0, "object": "embedding", "embedding": [1.0, 0.0, 0.0]},
                {"index": 1, "object": "embedding", "embedding": [0.0, 1.0, 0.0]},
            ],
            "usage": {"prompt_tokens": 9, "total_tokens": 9}
        });
        let vectors = parse_embeddings_response(&body, 3, 3).unwrap();
        assert_eq!(vectors.len(), 3);
        // index=0 must be first regardless of where it appeared in `data`.
        assert_eq!(vectors[0], vec![1.0, 0.0, 0.0]);
        assert_eq!(vectors[1], vec![0.0, 1.0, 0.0]);
        assert_eq!(vectors[2], vec![0.0, 0.0, 1.0]);
    }

    // S4e.A.3 — wrong-dimension vectors → Parse error (we don't put bad
    // vectors into log_vec).
    #[test]
    fn s4e_a3_wrong_dimensionality_returns_parse_error() {
        let body = json!({
            "data": [
                {"index": 0, "embedding": [1.0, 2.0]},   // dim=2
                {"index": 1, "embedding": [3.0, 4.0]},
            ]
        });
        let err = parse_embeddings_response(&body, 2, 4).unwrap_err();
        match err {
            EmbedError::Parse(msg) => assert!(
                msg.contains("vector length 2") && msg.contains("expected_dim 4"),
                "expected dim mismatch message, got: {msg}"
            ),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    // S4e.A.4 — load(): env beats file; file fallback; missing
    // embed_model → None.
    #[test]
    fn s4e_a4_load_env_first_then_file_else_none() {
        let tmp = tempfile::tempdir().unwrap();
        let opencrab_dir = tmp.path().join(".opencrab");
        std::fs::create_dir_all(&opencrab_dir).unwrap();
        std::fs::write(
            opencrab_dir.join("distiller.json"),
            json!({
                "base_url": "https://file.example/v1",
                "api_key": "sk-file",
                "model": "chat-file",
                "embed_model": "embed-file",
                "embed_dimensions": 1024
            })
            .to_string(),
        )
        .unwrap();
        let home_str = tmp.path().to_str().unwrap();

        // (a) env wins
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", Some("https://env.example/v1")),
                ("OPENCRAB_DISTILLER_API_KEY", Some("sk-env")),
                ("OPENCRAB_EMBED_MODEL", Some("env-embed-model")),
                ("OPENCRAB_EMBED_DIMENSIONS", None),
            ],
            || {
                let e = HttpEmbedder::load().expect("env wins → Some");
                assert_eq!(e.base_url, "https://env.example/v1");
                assert_eq!(e.api_key, "sk-env");
                assert_eq!(e.model, "env-embed-model");
                assert_eq!(e.dimensions, 1024, "default when env dim absent");
            },
        );

        // (b) env unset → file fallback
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
                ("OPENCRAB_EMBED_MODEL", None),
                ("OPENCRAB_EMBED_DIMENSIONS", None),
            ],
            || {
                let e = HttpEmbedder::load().expect("file fallback → Some");
                assert_eq!(e.base_url, "https://file.example/v1");
                assert_eq!(e.api_key, "sk-file");
                assert_eq!(e.model, "embed-file");
                assert_eq!(e.dimensions, 1024);
            },
        );

        // (c) file lacks embed_model → None even though base_url/api_key are set
        std::fs::write(
            opencrab_dir.join("distiller.json"),
            json!({
                "base_url": "https://file.example/v1",
                "api_key": "sk-file",
                "model": "chat-only"
            })
            .to_string(),
        )
        .unwrap();
        with_env(
            &[
                ("HOME", Some(home_str)),
                ("USERPROFILE", None),
                ("OPENCRAB_DISTILLER_BASE_URL", None),
                ("OPENCRAB_DISTILLER_API_KEY", None),
                ("OPENCRAB_EMBED_MODEL", None),
                ("OPENCRAB_EMBED_DIMENSIONS", None),
            ],
            || {
                assert!(
                    HttpEmbedder::load().is_none(),
                    "missing embed_model → None"
                );
            },
        );
    }

    // ==== B — embed_pending_once orchestration ====

    // S4e.B.1 — greenfield (0 log rows) is a complete no-op.
    #[test]
    fn s4e_b1_greenfield_is_noop() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let fake = FakeEmbedder::axis_units(1024);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_pending_seen, 0);
        assert_eq!(stats.rows_embedded, 0);
        assert_eq!(stats.batches, 0);
        assert_eq!(fake.call_count(), 0);
        assert!(log_vec_rowids(&conn).is_empty());
    }

    // S4e.B.2 — 3 pending rows → fake returns 3 vectors → 3 log_vec
    // rows (rowid == log.id) + KNN over the result orders correctly.
    #[test]
    fn s4e_b2_embeds_pending_rows_and_writes_to_log_vec() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id1 = seed_log_row(&conn, "alpha topic", Some("alpha body"));
        let id2 = seed_log_row(&conn, "beta topic", Some("beta body"));
        let id3 = seed_log_row(&conn, "gamma topic", Some("gamma body"));

        let fake = FakeEmbedder::axis_units(1024);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_pending_seen, 3);
        assert_eq!(stats.rows_embedded, 3);
        assert_eq!(stats.batches, 1);
        assert_eq!(fake.call_count(), 1);

        assert_eq!(log_vec_rowids(&conn), vec![id1, id2, id3]);

        // KNN: pick the axis-1 unit (== second row's text) → row 2 first.
        let mut q = vec![0.0_f32; 1024];
        q[1] = 1.0;
        let q_json = vec_to_match_json(&q);
        let mut stmt = conn
            .prepare(
                "SELECT log.id FROM log_vec JOIN log ON log.id = log_vec.rowid \
                 WHERE log_vec.embedding MATCH ?1 AND log_vec.k = ?2 \
                 ORDER BY log_vec.distance",
            )
            .unwrap();
        let ids: Vec<i64> = stmt
            .query_map(params![&q_json, 3_i64], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ids.first().copied(), Some(id2));
    }

    // S4e.B.3 — idempotent: a second pass with no new rows is a no-op.
    #[test]
    fn s4e_b3_second_pass_with_no_new_rows_is_noop() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        seed_log_row(&conn, "a", Some("b"));
        let fake = FakeEmbedder::axis_units(1024);
        let _ = block_on(embed_pending_once(&conn, &fake)).unwrap();

        let stats2 = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats2.rows_pending_seen, 0);
        assert_eq!(stats2.rows_embedded, 0);
        assert_eq!(stats2.batches, 0);
        // call_count = 1 from the first pass, not bumped by the second.
        assert_eq!(fake.call_count(), 1);
    }

    // S4e.B.4 — partial backfill: rows that already have a log_vec
    // entry are skipped; only the missing one is sent to the embedder.
    #[test]
    fn s4e_b4_skips_rows_that_already_have_an_embedding() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id1 = seed_log_row(&conn, "already embedded 1", Some("b1"));
        let id2 = seed_log_row(&conn, "already embedded 2", Some("b2"));
        let id3 = seed_log_row(&conn, "still pending", Some("b3"));

        // Pre-seed log_vec for ids 1 and 2.
        for id in [id1, id2] {
            let mut v = vec![0.0_f32; 1024];
            v[0] = 1.0;
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, vec_to_match_json(&v)],
            )
            .unwrap();
        }

        let fake = FakeEmbedder::axis_units(1024);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_pending_seen, 1, "only id3 is pending");
        assert_eq!(stats.rows_embedded, 1);

        let calls = fake.batches();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        // The only text sent should be id3's text.
        assert!(
            calls[0][0].starts_with("still pending"),
            "embed sent the pending row's text, got: {:?}",
            calls[0][0]
        );

        let rowids = log_vec_rowids(&conn);
        assert!(rowids.contains(&id3));
        assert_eq!(rowids.len(), 3);
    }

    // S4e.B.5 — text shape: `summary + "\n\n" + detail`, with NULL detail
    // producing `summary + "\n\n" + ""`. Empty-content rows skipped.
    #[test]
    fn s4e_b5_embed_text_shape_matches_spec() {
        // Direct build_embed_text check.
        assert_eq!(build_embed_text("hi", Some("body")), "hi\n\nbody");
        assert_eq!(build_embed_text("hi", None), "hi\n\n");
        assert_eq!(build_embed_text("", None), "\n\n");

        // Via embed_pending_once: detail = NULL row produces "summary\n\n".
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        seed_log_row(&conn, "headline", None);
        let fake = FakeEmbedder::axis_units(1024);
        let _ = block_on(embed_pending_once(&conn, &fake)).unwrap();
        let calls = fake.batches();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        assert_eq!(calls[0][0], "headline\n\n");
    }

    // S4e.B.6 — more than EMBED_BATCH_SIZE pending rows: multiple
    // batches, each ≤ batch size, all rows embedded.
    #[test]
    fn s4e_b6_chunks_more_than_batch_size_rows_into_multiple_batches() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let total = (EMBED_BATCH_SIZE * 2 + 5) as i64;
        for i in 1..=total {
            seed_log_row(&conn, &format!("row {i}"), Some(&format!("body {i}")));
        }

        let fake = FakeEmbedder::constant(1024, vec![0.0_f32; 1024]);
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.rows_embedded, total as u64);
        let batches = fake.batches();
        assert_eq!(batches.len(), 3, "ceil({} / {})", total, EMBED_BATCH_SIZE);
        for b in &batches {
            assert!(
                b.len() <= EMBED_BATCH_SIZE,
                "batch size {} exceeds cap {}",
                b.len(),
                EMBED_BATCH_SIZE
            );
        }
        assert_eq!(log_vec_rowids(&conn).len(), total as usize);
    }

    // S4e.B.7 — embedder error → no log_vec inserts, all rows stay pending.
    #[test]
    fn s4e_b7_embedder_error_leaves_rows_pending() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        for i in 1..=5 {
            seed_log_row(&conn, &format!("row {i}"), Some("body"));
        }

        let fake =
            FakeEmbedder::errors_with(1024, EmbedError::HttpTransient("503".into()));
        let stats = block_on(embed_pending_once(&conn, &fake)).unwrap();
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.rows_embedded, 0);
        assert!(log_vec_rowids(&conn).is_empty(), "no half-written batch");

        // Pending count via SELECT: still 5.
        let pending: i64 = conn
            .query_row(
                "SELECT count(*) FROM log WHERE id NOT IN (SELECT rowid FROM log_vec)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 5);
    }

    // ==== D — #[ignore] live: real SiliconFlow /v1/embeddings ====

    /// Manual: `cargo test --bin opencrab-memory-mcp -- --ignored \
    ///   s4e_d1_live_embed_against_siliconflow --nocapture`
    #[test]
    #[ignore]
    fn s4e_d1_live_embed_against_siliconflow() {
        let embedder = HttpEmbedder::load()
            .expect("OPENCRAB_EMBED_MODEL env OR distiller.json embed_model required");

        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        // Two log rows with semantically different content + a third
        // closer to row A. KNN-against-A's embedding should rank
        // (A, A-related) ahead of (B).
        let id_migration = seed_log_row(
            &conn,
            "cargo test failed: duplicate column name origin",
            Some("ALTER TABLE ADD COLUMN ran twice; PRAGMA user_version gating fixes it"),
        );
        let id_lunch = seed_log_row(
            &conn,
            "lunch menu discussion",
            Some("we agreed on dumplings for the team lunch"),
        );
        let id_other_migration = seed_log_row(
            &conn,
            "SQLite ALTER TABLE migration",
            Some("idempotent schema upgrade via user_version PRAGMA"),
        );

        let stats = block_on(embed_pending_once(&conn, &embedder)).unwrap();
        eprintln!("[live-embed] stats: {stats:?}");
        eprintln!("[live-embed] dim: {}", embedder.dimensions());
        assert_eq!(stats.rows_embedded, 3);

        // Sanity: each log_vec row must hold a 1024-d vector.
        // sqlite-vec stores vectors as opaque; we infer from the
        // distance column behaving sensibly.
        let q_text = "ALTER TABLE migration not idempotent";
        let q_vec = block_on(embedder.embed(&[q_text.to_string()]))
            .expect("live embed of query");
        eprintln!("[live-embed] query vector length: {}", q_vec[0].len());
        assert_eq!(
            q_vec[0].len(),
            1024,
            "embedding dimension must match log_vec's float[1024]"
        );

        let q_json = vec_to_match_json(&q_vec[0]);
        let mut stmt = conn
            .prepare(
                "SELECT log.id, log.summary, log_vec.distance \
                 FROM log_vec JOIN log ON log.id = log_vec.rowid \
                 WHERE log_vec.embedding MATCH ?1 AND log_vec.k = ?2 \
                 ORDER BY log_vec.distance",
            )
            .unwrap();
        let rows: Vec<(i64, String, f64)> = stmt
            .query_map(params![&q_json, 3_i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        eprintln!("[live-embed] KNN rows (id, summary, distance):");
        for (id, summary, dist) in &rows {
            eprintln!("  id={id} dist={dist:.4} summary={summary}");
        }
        assert_eq!(rows.len(), 3);

        // The two migration-related rows should outrank the lunch row.
        let lunch_pos = rows
            .iter()
            .position(|(id, _, _)| *id == id_lunch)
            .expect("lunch row in results");
        let migration_positions: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, (id, _, _))| *id == id_migration || *id == id_other_migration)
            .map(|(i, _)| i)
            .collect();
        assert!(
            migration_positions.iter().all(|&pos| pos < lunch_pos),
            "both migration rows must rank above the lunch row; got order: {:?}",
            rows.iter().map(|r| r.0).collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------
    // Phase 6 Step 4-search — hybrid (FTS bm25 ⊕ vector KNN, RRF-merged)
    // -----------------------------------------------------------------

    /// FakeEmbedder variant with a configurable artificial delay before
    /// returning the result. Used to exercise the search-time embed timeout.
    struct SlowFakeEmbedder {
        dimensions: usize,
        delay: std::time::Duration,
        inner: FakeEmbedBehavior,
    }

    impl SlowFakeEmbedder {
        fn new(dimensions: usize, delay: std::time::Duration, inner: FakeEmbedBehavior) -> Self {
            Self { dimensions, delay, inner }
        }
    }

    #[async_trait::async_trait]
    impl Embedder for SlowFakeEmbedder {
        fn dimensions(&self) -> usize {
            self.dimensions
        }

        async fn embed(
            &self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            tokio::time::sleep(self.delay).await;
            let beh = self.inner.clone();
            match beh {
                FakeEmbedBehavior::AxisUnits => {
                    let mut out = Vec::with_capacity(texts.len());
                    for i in 0..texts.len() {
                        let mut v = vec![0.0_f32; self.dimensions];
                        if i < self.dimensions {
                            v[i] = 1.0;
                        }
                        out.push(v);
                    }
                    Ok(out)
                }
                FakeEmbedBehavior::Constant(v) => {
                    Ok(texts.iter().map(|_| v.clone()).collect())
                }
                FakeEmbedBehavior::Err(e) => Err(e),
            }
        }
    }

    /// Build a 1024-d vector with values set at specific axes — used to
    /// craft scenarios where FTS and vector orderings differ.
    fn vec_with_axes(values: &[(usize, f32)]) -> Vec<f32> {
        let mut v = vec![0.0_f32; 1024];
        for &(i, x) in values {
            v[i] = x;
        }
        v
    }

    fn insert_log_vec(conn: &Connection, rowid: i64, v: &[f32]) {
        conn.execute(
            "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
            params![rowid, vec_to_match_json(v)],
        )
        .unwrap();
    }

    // ==== A — hybrid sort + RRF math ====

    // S4w.A.1 — FTS and vector lists disagree; RRF merges them into the
    // expected order (an entry appearing in BOTH lists outranks an
    // entry that's strong in only one).
    //
    // Construction:
    //   - "keyword" appears twice in A's detail → FTS rank 1 (best)
    //   - "keyword" appears once in C's detail → FTS rank 2
    //   - B has no "keyword" → not in FTS
    //   Vectors:
    //   - A: (0.5, 0.5, …) — partly aligned with query axis-1
    //   - B: (0, 1.0, …) — exactly query axis-1, KNN rank 1
    //   - C: (1.0, 0, …) — far from query, KNN rank 3
    //   Embedder returns axis-1 unit for the query.
    //
    // RRF (K=60, rank starts at 1):
    //   A: 1/(60+1) + 1/(60+2) ≈ 0.03253
    //   C: 1/(60+2) + 1/(60+3) ≈ 0.03200
    //   B: 0          + 1/(60+1) ≈ 0.01639
    // Expected: [A, C, B].
    #[test]
    fn s4w_a1_rrf_merges_fts_and_vector_orderings_correctly() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();

        let id_a = seed_log_row(&conn, "alpha", Some("alpha keyword keyword body"));
        let id_b = seed_log_row(&conn, "beta", Some("nothing matching here"));
        let id_c = seed_log_row(&conn, "gamma", Some("keyword tail"));

        insert_log_vec(&conn, id_a, &vec_with_axes(&[(0, 0.5), (1, 0.5)]));
        insert_log_vec(&conn, id_b, &vec_with_axes(&[(1, 1.0)]));
        insert_log_vec(&conn, id_c, &vec_with_axes(&[(0, 1.0)]));

        // Embed-the-query returns axis-1 unit (close to id_b).
        let fake = FakeEmbedder::constant(1024, vec_with_axes(&[(1, 1.0)]));
        drop(conn);

        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(
            ids,
            vec![id_a, id_c, id_b],
            "RRF must rank id_a (top of both lists indirectly) > id_c (both lists) > id_b (vector only)"
        );
    }

    // S4w.A.2 — RRF math on hand-picked lists: verify scores by hand.
    #[test]
    fn s4w_a2_rrf_merge_math_is_correct_on_known_inputs() {
        // Two lists, three docs, disagreement.
        let fts = vec![1_i64, 2, 3];
        let vec_list = vec![3_i64, 1, 2];
        // Hand-computed RRF (K=60, rank 1-based):
        //   id 1: 1/61 + 1/62 = 0.032526
        //   id 2: 1/62 + 1/63 = 0.031883
        //   id 3: 1/63 + 1/61 = 0.032265
        // Order (desc): 1, 3, 2.
        let merged = rrf_merge(&[&fts, &vec_list], 10);
        assert_eq!(merged, vec![1_i64, 3, 2]);

        // Ties on score → broken by id ASC.
        let merged2 = rrf_merge(&[&vec![1_i64, 2], &vec![2_i64, 1]], 10);
        // id 1: 1/61 + 1/62 = 0.032526
        // id 2: 1/62 + 1/61 = 0.032526 (same)
        // tie → smaller id wins
        assert_eq!(merged2, vec![1_i64, 2]);
    }

    // ==== B — graceful degradation paths ====

    // S4w.B.1 — embedder=None → pure FTS path; behavior equals the
    // pre-S4 FTS-only search.
    #[test]
    fn s4w_b1_no_embedder_returns_pure_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("alpha beta keyword"));
        let _id_b = seed_log_row(&conn, "b", Some("nothing relevant"));
        drop(conn);

        let hits = block_on(search(&db, None, "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.2 — embedder returns Err → vector path dropped, pure-FTS
    // result returned without error.
    #[test]
    fn s4w_b2_embedder_error_falls_back_to_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword in detail"));
        let id_b = seed_log_row(&conn, "b", Some("not relevant"));
        // Both have log_vec entries so a working vector path would
        // re-shuffle the ranking — but the embedder errors, so it shouldn't.
        insert_log_vec(&conn, id_a, &vec_with_axes(&[(0, 1.0)]));
        insert_log_vec(&conn, id_b, &vec_with_axes(&[(1, 1.0)]));
        drop(conn);

        let fake = FakeEmbedder::errors_with(
            1024,
            EmbedError::HttpTransient("503 backend".into()),
        );
        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1, "FTS hits only — id_b has no 'keyword'");
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.3 — `log` rows exist but `log_vec` is empty (greenfield):
    // vector path returns 0 ids → result equals pure FTS.
    #[test]
    fn s4w_b3_empty_log_vec_returns_pure_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword present"));
        let _id_b = seed_log_row(&conn, "b", Some("other content"));
        // No log_vec inserts → greenfield.
        drop(conn);

        let fake = FakeEmbedder::constant(1024, vec_with_axes(&[(1, 1.0)]));
        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.4 — embedder claims dim=1024 but returns a 100-length vector:
    // search drops the vector path and falls back to FTS.
    #[test]
    fn s4w_b4_query_embedding_dim_mismatch_falls_back_to_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword body"));
        let _id_b = seed_log_row(&conn, "b", Some("unrelated"));
        insert_log_vec(&conn, id_a, &vec_with_axes(&[(0, 1.0)]));
        drop(conn);

        // Direct construction lets us bypass FakeEmbedder::constant's
        // dim-assertion and emit a vector that lies about its size.
        let fake = FakeEmbedder {
            dimensions: 1024,
            behavior: std::sync::Mutex::new(FakeEmbedBehavior::Constant(vec![0.0_f32; 100])),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        assert_eq!(hits.len(), 1, "vector path dropped, FTS only");
        assert_eq!(hits[0].id, id_a);
    }

    // S4w.B.5 — embed takes longer than QUERY_EMBED_TIMEOUT: the
    // timeout fires, vector path drops, FTS-only result returned.
    // Uses an injected (tiny) timeout via the test-only
    // `search_with_timeout` so the test stays sub-second.
    #[test]
    fn s4w_b5_embed_timeout_falls_back_to_fts() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id_a = seed_log_row(&conn, "a", Some("keyword here"));
        drop(conn);

        let slow = SlowFakeEmbedder::new(
            1024,
            std::time::Duration::from_millis(500),
            FakeEmbedBehavior::Constant(vec![0.0_f32; 1024]),
        );
        let hits = block_on(search_with_timeout(
            &db,
            Some(&slow),
            "keyword",
            10,
            std::time::Duration::from_millis(50), // budget shorter than the embedder's 500ms delay
        ))
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id_a);
    }

    // ==== D — superseded filtering ====

    // S4w.D.1 — superseded rows are filtered from BOTH halves (FTS + KNN) before
    // RRF: a row marked `superseded_by` never surfaces, the live survivors do,
    // and the result is not emptied (over-fetch kept the live rows).
    #[test]
    fn s4w_d1_search_filters_superseded_rows() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        // All three FTS-match "keyword"; vectors put A nearest the query, then B, then C.
        let id_a = seed_log_row(&conn, "a", Some("keyword alpha"));
        let id_b = seed_log_row(&conn, "b", Some("keyword beta"));
        let id_c = seed_log_row(&conn, "c", Some("keyword gamma"));
        insert_log_vec(&conn, id_a, &vec_with_axes(&[(0, 1.0)]));
        insert_log_vec(&conn, id_b, &vec_with_axes(&[(0, 0.9), (1, 0.1)]));
        insert_log_vec(&conn, id_c, &vec_with_axes(&[(1, 1.0)]));
        // Supersede A — the top hit on BOTH halves. It must vanish from search.
        conn.execute("UPDATE log SET superseded_by=?1 WHERE id=?2", params![id_b, id_a]).unwrap();
        drop(conn);

        // Query embeds onto A's axis (A would be KNN rank 1 if not filtered).
        let fake = FakeEmbedder::constant(1024, vec_with_axes(&[(0, 1.0)]));
        let hits = block_on(search(&db, Some(&fake), "keyword", 10)).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        assert!(!ids.contains(&id_a), "superseded row must not surface: {ids:?}");
        assert!(
            ids.contains(&id_b) && ids.contains(&id_c),
            "live survivors present: {ids:?}"
        );
        assert_eq!(hits.len(), 2, "result not emptied — over-fetch kept the live rows: {ids:?}");
    }

    // ==== C — MemoryHit shape contract ====

    // S4w.C.1 — MemoryHit fields are unchanged (id, ts, summary, snippet);
    // snippet is the truncated `detail` body — proving the MCP-facing
    // shape of memory_search hasn't drifted.
    #[test]
    fn s4w_c1_memory_hit_shape_unchanged() {
        let (_tmp, db) = db_path();
        let conn = open(&db).unwrap();
        let id = seed_log_row(
            &conn,
            "headline",
            Some("body containing the keyword sentinel for FTS"),
        );
        drop(conn);

        let hits = block_on(search(&db, None, "sentinel", 10)).unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.id, id);
        assert!(h.ts > 0);
        assert_eq!(h.summary, "headline");
        assert_eq!(h.snippet, "body containing the keyword sentinel for FTS");

        // memory_get is unchanged; spot-check it still returns the
        // full row by id.
        let entry = get(&db, id).unwrap();
        assert!(entry.found);
        assert_eq!(entry.summary, "headline");
        assert_eq!(
            entry.detail.as_deref(),
            Some("body containing the keyword sentinel for FTS")
        );
    }
