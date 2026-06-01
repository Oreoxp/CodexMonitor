use super::BackendError;
use rusqlite::Connection;

/// Schema version this binary writes. The DB file records its current
/// schema in `PRAGMA user_version`; `migrate` advances it one step at a
/// time up to this value.
pub(crate) const SCHEMA_VERSION: i64 = 9;

/// Bring `conn`'s schema up to [`SCHEMA_VERSION`].
///
/// v0 baseline (every `IF NOT EXISTS`) runs unconditionally so a fresh DB
/// gets the initial objects; against an already-initialised DB it is a
/// no-op. After that we walk from `user_version + 1` up to
/// `SCHEMA_VERSION`, applying one version step per IMMEDIATE transaction.
/// A future binary opening a higher-versioned DB iterates over an empty
/// range and exits clean.
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), BackendError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS log (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            ts      INTEGER NOT NULL,
            summary TEXT    NOT NULL,
            detail  TEXT
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS log_fts USING fts5(
            detail,
            content='log',
            content_rowid='id'
        );
        CREATE TRIGGER IF NOT EXISTS log_ai AFTER INSERT ON log BEGIN
            INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
        END;
        CREATE TRIGGER IF NOT EXISTS log_ad AFTER DELETE ON log BEGIN
            INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
        END;
        CREATE TRIGGER IF NOT EXISTS log_au AFTER UPDATE ON log BEGIN
            INSERT INTO log_fts(log_fts, rowid, detail) VALUES('delete', old.id, old.detail);
            INSERT INTO log_fts(rowid, detail) VALUES (new.id, new.detail);
        END;",
    )?;

    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for target in (current + 1)..=SCHEMA_VERSION {
        // Each step's `ALTER TABLE ADD COLUMN` (and any other non-idempotent
        // DDL) would crash the next open() on a `duplicate column name`-style
        // error if it ran twice. Wrap the whole step in one IMMEDIATE
        // transaction with the `user_version` bump as the last statement so
        // any mid-batch failure rolls back cleanly and the next open()
        // retries from the same starting version.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Re-read `user_version` under the write lock: guards the rare case
        // of two first-opens racing on the upgrade.
        let locked: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if locked < target {
            apply_migration_step(&tx, target)?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// One version step. Each branch ends with `PRAGMA user_version = N` so
/// the bump is the last statement in the batch — a mid-step failure rolls
/// back the version pin along with the DDL.
pub(crate) fn apply_migration_step(tx: &rusqlite::Transaction<'_>, target: i64) -> Result<(), BackendError> {
    match target {
        1 => tx.execute_batch(
            "ALTER TABLE log ADD COLUMN origin TEXT NOT NULL DEFAULT 'self';
             ALTER TABLE log ADD COLUMN project_hash TEXT;
             CREATE INDEX IF NOT EXISTS idx_log_ts ON log(ts);
             PRAGMA user_version = 1;",
        )?,
        2 => tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS raw_thread (
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
             CREATE TABLE IF NOT EXISTS raw_event (
                 id          INTEGER PRIMARY KEY,
                 thread_id   TEXT NOT NULL REFERENCES raw_thread(thread_id),
                 line_no     INTEGER NOT NULL,
                 payload     TEXT NOT NULL,
                 ingested_at INTEGER NOT NULL,
                 UNIQUE(thread_id, line_no)
             );
             CREATE INDEX IF NOT EXISTS idx_raw_event_thread ON raw_event(thread_id);
             PRAGMA user_version = 2;",
        )?,
        3 => tx.execute_batch(
            // S3-schema: distill cursor + growth timestamp.
            //
            // `last_distilled_line_no` (NOT NULL DEFAULT 0): how far the S3
            // distiller has consumed this thread. Sibling to `last_line_no`
            // (ingester's cursor); both move monotonically.
            //
            // `last_growth_ts` (nullable): timestamp of the last ingest pass
            // that actually inserted new raw_event rows. Distinct from
            // `last_ingest_ts`, which is bumped every pass (even no-op ones).
            // The distiller uses this to decide "anything new since I last
            // ran?" without having to re-read raw_event.
            "ALTER TABLE raw_thread ADD COLUMN last_distilled_line_no INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE raw_thread ADD COLUMN last_growth_ts INTEGER;
             PRAGMA user_version = 3;",
        )?,
        4 => tx.execute_batch(
            // S3-distill: classify distilled `log` rows by `kind`
            // (decision/failure/pattern/fact). Nullable on purpose:
            //   * existing user-written `log_progress` rows have no kind
            //     and we won't backfill;
            //   * `log_progress` signature stays unchanged — it still
            //     writes `kind = NULL`. Only the distiller writes a kind.
            // FTS triggers reference only `detail`, so adding `kind`
            // doesn't require an FTS rebuild.
            "ALTER TABLE log ADD COLUMN kind TEXT;
             PRAGMA user_version = 4;",
        )?,
        5 => tx.execute_batch(
            // S4-schema: per-row embeddings into a vec0 virtual table.
            // Dim 4096 is fixed at table-create time — picked from S4-R
            // (SiliconFlow Qwen3-Embedding-8B native dim). Rowid is
            // expected to equal `log.id` so queries can plain-JOIN.
            //
            // We never write to log_vec in this step — backfill +
            // per-row insert come in S4-impl. Empty virtual table is
            // a no-op for KNN (returns 0 rows) and for the existing
            // FTS-only search path.
            "CREATE VIRTUAL TABLE IF NOT EXISTS log_vec USING vec0(embedding float[4096]);
             PRAGMA user_version = 5;",
        )?,
        6 => tx.execute_batch(
            // v6: re-dimension log_vec from 4096 (Qwen3-Embedding-8B) to 1024
            // (BAAI/bge-m3). A vec0 table's dim is fixed at create time, so we
            // DROP + re-CREATE. DROP+CREATE of a vec0 virtual table runs inside
            // this IMMEDIATE migration tx exactly like the v5 CREATE did.
            // Lossless in practice: production has 0 memory.db files
            // (greenfield) and test DBs are temp; embed_pending_once re-embeds
            // every log row on the next pass (empty log_vec → all rows pending).
            "DROP TABLE IF EXISTS log_vec;
             CREATE VIRTUAL TABLE log_vec USING vec0(embedding float[1024]);
             PRAGMA user_version = 6;",
        )?,
        7 => tx.execute_batch(
            // v7: rebuild log_fts with the FTS5 `trigram` tokenizer so CJK
            // substrings become lexically matchable (the default unicode61
            // doesn't segment CJK → Chinese queries got 0 FTS hits and the
            // lexical half of hybrid search was dead for Chinese). Same
            // external-content config (content='log', content_rowid='id',
            // column `detail`); ONLY the tokenizer changes. The sync triggers
            // (log_ai/ad/au) reference log_fts by name and keep working after
            // the recreate — sync logic untouched. DROP+CREATE of an FTS5 table
            // runs inside this IMMEDIATE tx just like the v6 log_vec rebuild;
            // `'rebuild'` re-indexes every existing row from the content table.
            "DROP TABLE IF EXISTS log_fts;
             CREATE VIRTUAL TABLE log_fts USING fts5(
                 detail,
                 content='log',
                 content_rowid='id',
                 tokenize='trigram'
             );
             INSERT INTO log_fts(log_fts) VALUES('rebuild');
             PRAGMA user_version = 7;",
        )?,
        8 => tx.execute_batch(
            // v8 (consolidation): schema substrate for semantic consolidation
            // of `log` rows (dedup / supersede / contradiction flagging).
            // SCHEMA-ONLY — nothing reads or writes these objects yet (the
            // search-side `superseded_by` filter and the apply path land
            // later), so this step is behavior-inert against an existing v7
            // DB: no current writer names the column, no current reader
            // selects it.
            //
            // NOTE on the name: the code's schema-chunk series already labels
            // v7 (trigram) as "S5schema". This consolidation work is the
            // *feature-ladder* "S5" but is deliberately NOT tagged with an
            // S-number in code — that would collide with the v7 label and the
            // two series are off by one (trigram took a schema-chunk slot but
            // not a feature-ladder rung). Anchor on the version (v8) + the
            // feature name, nothing else.
            //
            // `log.superseded_by INTEGER NULL REFERENCES log(id)`: NULL = a
            // live row; a non-NULL value points at the row that absorbs this
            // one (both dedup and supersede write it; same column, different
            // reason — the relation kind lives in the audit row). The FK is
            // declarative only: `PRAGMA foreign_keys` is never enabled on this
            // connection, so it documents intent without runtime enforcement
            // (the apply path owns the "points at a real live id" invariant).
            // ADD COLUMN with a NULL default runs inside this IMMEDIATE tx
            // exactly like the v1/v3/v4 ALTERs.
            //
            // `log_contradiction`: contradiction is many-to-many (one row can
            // contradict several others), so it can't be a column on `log`.
            // Pairs are normalised id_a < id_b by the writer so each unordered
            // pair has one canonical row; the composite PK dedupes. `audit_id`
            // links the edge back to the audit row that judged it.
            //
            // `log_consolidation_audit`: append-only trail of every judged
            // pair — the inspection surface (and the only durable record of
            // the judge's `relation` + chosen `action` + `rationale` +
            // `superseded_id`, plus whether it was a `dry_run` and whether it
            // was `applied`). It's what keeps dedup-merge vs supersede-merge
            // and complement-noop vs false-neighbor-noop distinguishable after
            // the fact, since the schema collapses each of those pairs to the
            // same action. `superseded_id` persists the judge's supersede
            // DIRECTION (which KP it called older) — apply (S5-C) re-derives
            // direction from ts, so the two can be reconciled and a divergence
            // (judge read the direction wrong, or ts injection is off) flagged.
            "ALTER TABLE log ADD COLUMN superseded_by INTEGER NULL REFERENCES log(id);
             CREATE TABLE IF NOT EXISTS log_contradiction (
                 id_a     INTEGER NOT NULL,
                 id_b     INTEGER NOT NULL,
                 audit_id INTEGER,
                 PRIMARY KEY (id_a, id_b)
             );
             CREATE TABLE IF NOT EXISTS log_consolidation_audit (
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
             );
             PRAGMA user_version = 8;",
        )?,
        9 => tx.execute_batch(
            // v9 (consolidation loop): a single-row cursor table for the
            // low-frequency background consolidation pass. Pure-add — nothing in
            // v8 is touched. The recon confirmed the store had NO consolidation-
            // dimension marker (all existing cursors live on `raw_thread` and are
            // per-thread ingest/distill cursors); consolidation is a whole-store,
            // cross-thread, live-KP-counted pass, so it needs its own global cursor.
            //
            // `consolidation_state` is a singleton (`CHECK (id = 1)`): one row
            // holds the high-water mark `last_consolidation_max_log_id` (the
            // `MAX(log.id)` snapshot the last completed run advanced to) plus
            // `last_consolidation_ts`. The loop fires only once
            // `COUNT(live KP with id > last_consolidation_max_log_id) >= M`.
            // `INSERT OR IGNORE` seeds the row at 0 so a fresh DB consolidates
            // once enough KPs accrue, and a re-run is a no-op (row already there).
            "CREATE TABLE IF NOT EXISTS consolidation_state (
                 id                            INTEGER PRIMARY KEY CHECK (id = 1),
                 last_consolidation_max_log_id INTEGER NOT NULL DEFAULT 0,
                 last_consolidation_ts         INTEGER
             );
             INSERT OR IGNORE INTO consolidation_state (id, last_consolidation_max_log_id) VALUES (1, 0);
             PRAGMA user_version = 9;",
        )?,
        _ => unreachable!("no migration step defined for v{target} — add an arm"),
    }
    Ok(())
}
