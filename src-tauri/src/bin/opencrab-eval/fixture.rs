// Fixture definitions — static array, embedded via `include_str!`.
//
// Each fixture is a single-agent team + one user turn + one assertion
// function. F01-F03 cover the P3-era hot zones (PM emits well-formed
// `<propose_plan>`; PM resolves ambiguity before decomposing; QA reports
// findings without modifying code). F-mem-write / F-mem-trivial-control /
// F-mem-recall (P6 Step 6) cover the memory loop — `log_progress` writes
// land on disk; trivial turns don't trigger noisy writes; the agent
// consults its log to recall facts older than the daily-memory prelude
// window.

use std::path::Path;

use crate::assertions;

/// The text the harness sends as the user's first real turn (after the
/// kickoff). Drives the agent into the scenario.
pub struct Fixture {
    pub id: &'static str,
    pub description: &'static str,
    /// `team.json` body — embedded so fixtures live with the harness.
    pub team_json: &'static str,
    /// Which agent the user message is delivered to (matches `id` in team_json).
    pub user_correspondent_id: &'static str,
    pub user_turn: &'static str,
    /// Per-scenario assertion. Receives both the captured turn and the
    /// per-fixture filesystem artifacts (the latter is what lets the
    /// F-mem-write assertion independently open memory.db post-run).
    pub assert: fn(&assertions::TurnResult, &assertions::RunArtifacts) -> assertions::AssertionOutcome,
    /// Optional pre-turn hook to seed the agent's `memory.db` BEFORE
    /// codex-app-server boots. Called from `setup_fixture_environment`
    /// once the user-layer paths exist (bootstrap done), so the seeded
    /// rows are in place when the agent's first turn runs. Used by
    /// F-mem-recall to make sure the key historical entry sits OUTSIDE
    /// the `buildDailyMemoryPrelude` LIMIT-10 window — only `memory_search`
    /// / `memory_get` can surface it.
    pub seed_memory_db: Option<fn(&Path)>,
}

pub static FIXTURES: &[Fixture] = &[
    Fixture {
        id: "01-pm-clear",
        description: "PM receives a clear request → must emit a well-formed <propose_plan>",
        team_json: include_str!("fixtures/01-pm-clear/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "We need to add audit logging to the Django admin panel. \
                    The specifics are all decided: log every create / edit / \
                    delete on all models registered in the admin; store \
                    field-level diffs (not full before/after snapshots); use \
                    the django-auditlog package (do not build a custom \
                    solution); surface the logs as a new 'Audit Log' page \
                    inside the Django admin; no special retention or \
                    performance requirements. Outline the implementation plan.",
        assert: assertions::pm_calls_propose_plan_tool,
        seed_memory_db: None,
    },
    Fixture {
        id: "02-pm-ambiguous",
        description: "PM receives an ambiguous request → must clarify before proposing",
        team_json: include_str!("fixtures/02-pm-ambiguous/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "Improve the API.",
        assert: assertions::pm_does_not_propose_when_ambiguous_tool,
        seed_memory_db: None,
    },
    Fixture {
        id: "03-qa-verification",
        description: "QA receives a verification ask → must report findings, not modify code",
        team_json: include_str!("fixtures/03-qa-verification/team.json"),
        user_correspondent_id: "agent_qa",
        user_turn: "Please verify the new auth middleware at apps/api/src/middleware/auth.ts. \
                    Acceptance criteria: rejects requests without a token (401), rejects \
                    expired tokens (401), accepts valid tokens. Don't fix anything you find — \
                    just report what you observe.",
        assert: assertions::qa_calls_send_message_no_modifying_tool,
        seed_memory_db: None,
    },
    // ───────────────────────────────────────────────────────────────────────
    // P6 Step 6 — formal memory fixtures (promoted from b1-probe-*).
    // ───────────────────────────────────────────────────────────────────────
    Fixture {
        id: "F-mem-write",
        description: "PM decides + is asked to record it for tomorrow — should call log_progress \
                      AND the summary must land in memory.db on disk",
        team_json: include_str!("fixtures/01-pm-clear/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "Quick one before I step away: we're going with PostgreSQL for the new \
                    notes module — it's read-heavy, ~10k users projected, and we already run \
                    a PostgreSQL cluster for unrelated services so the operational cost is \
                    near-zero. Acknowledge the decision and record it so when I check back \
                    tomorrow you remember exactly what we landed on and why.",
        assert: assertions::pm_logs_progress_after_decision,
        seed_memory_db: None,
    },
    Fixture {
        id: "F-mem-trivial-control",
        description: "PM gets a trivial ack — should NOT call log_progress",
        team_json: include_str!("fixtures/01-pm-clear/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "thanks — talking later",
        assert: assertions::pm_does_not_log_on_trivial,
        seed_memory_db: None,
    },
    Fixture {
        id: "F-mem-recall",
        description: "PM is asked about an older decision that sits OUTSIDE the daily-memory \
                      prelude's 10-row window — must call memory_search / memory_get and \
                      surface a seeded distinctive fact in the reply",
        team_json: include_str!("fixtures/01-pm-clear/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "Picking up the notes-module work again — what database engine did we \
                    actually land on for it, and why? I remember we settled this a while \
                    back but I don't have my notes handy. Can you check what you logged?",
        assert: assertions::pm_recalls_seeded_fact,
        seed_memory_db: Some(seed_f_mem_recall),
    },
];

/// F-mem-recall seed — 12 rows total. Row #1 (the OLDEST by `ts`) holds
/// the verifiable Postgres decision; the agent's natural search query
/// ("notes module database") will hit it via the FTS5 index on `detail`.
/// Rows #2..#12 fill the most-recent-10 prelude window with unrelated
/// progress so the key row falls OUTSIDE
/// `buildDailyMemoryPrelude`'s `LIMIT 10` (newest-first) cut — only an
/// explicit `memory_search` / `memory_get` call can reach it.
///
/// We write with monotonically increasing `ts` (epoch ms): row #1 gets the
/// smallest ts; row #12 gets the largest. The daily-memory prelude orders
/// by `ts DESC LIMIT 10`, so it picks rows #12..#3 — the key row #1 (and
/// #2 buffer) stay invisible to the prelude.
///
/// Schema mirrors `opencrab-memory-mcp::backend.rs` exactly. Trigger /
/// FTS shadow table created here is identical to what the live mcp
/// server creates on first open — see `backend.rs::migrate`.
pub fn seed_f_mem_recall(memory_db: &Path) {
    if let Some(parent) = memory_db.parent() {
        std::fs::create_dir_all(parent).expect("seed: mkdir parent");
    }
    let conn = rusqlite::Connection::open(memory_db).expect("seed: open memory.db");
    let _: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("seed: PRAGMA WAL");
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS log (
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
        END;
        "#,
    )
    .expect("seed: migrate");

    // Anchor the oldest seed at a deterministic point in the past so that
    // it provably falls below every other row's ts. We pick 2026-04-12
    // (matches the seeded narrative) as the oldest, and step forward by
    // one day per row.
    const ROW_COUNT: i64 = 12;
    let base_ts_ms: i64 = 1_744_416_000_000; // 2026-04-12T00:00:00Z in ms
    let day_ms: i64 = 24 * 60 * 60 * 1000;

    let rows: [(&str, &str); 12] = [
        // Row 1 — OLDEST, the key fact. Detail packed with FTS5-hittable
        // tokens so a natural "notes module database" search lands here.
        (
            "Picked PostgreSQL for notes-module DB",
            "We chose PostgreSQL for the notes module on 2026-04-12. Rationale: \
             read-heavy workload (~10k users projected), and we already operate a \
             PostgreSQL cluster at notes-pg-primary.internal.example.com:5432 for \
             unrelated services so operational cost is near-zero. The choice was \
             specifically against MySQL and SQLite — Postgres was picked for its \
             read-replica story. Schema design left to a follow-up RFC. \
             Keywords: notes module, database, Postgres, PostgreSQL.",
        ),
        // Row 2 — older buffer, unrelated.
        (
            "Frontend bundler migration to Vite",
            "Migrated the marketing site from webpack to Vite. Build time dropped \
             from 42s to 7s on a clean install. No runtime behavior change.",
        ),
        // Rows 3..12 — the most-recent-10 prelude window. All unrelated to
        // the notes-module DB question so the prelude alone cannot answer.
        (
            "Standup notes on auth refresh-token rotation",
            "Decided to rotate refresh tokens server-side on every use. JWT short-\
             lived 5m, refresh 30d, sliding window.",
        ),
        (
            "Closed Linear ticket ENG-411",
            "Fixed the trailing slash in the OAuth redirect URI. Stripe webhooks \
             now deliver cleanly in staging.",
        ),
        (
            "Spike on background job queue",
            "Compared Sidekiq vs. RabbitMQ vs. Postgres-backed worker. Deferred the \
             decision; current scale doesn't justify the dependency yet.",
        ),
        (
            "Deployment rollback runbook draft",
            "Drafted the rollback runbook for prod deploys: tag rollback, smoke, \
             page on regression. Pairs with the existing canary script.",
        ),
        (
            "Pairing notes — Maya on the search refactor",
            "Walked Maya through the search facet pipeline. Indexed her open \
             questions; we'll resume the refactor Monday.",
        ),
        (
            "Customer feedback synthesis — Q2 wave",
            "Read the 23 customer-feedback transcripts from the Q2 cohort. Major \
             theme: onboarding clarity. Filed three follow-up tickets.",
        ),
        (
            "Privacy review for analytics events",
            "Reviewed the analytics-event schema with legal. Two events removed; \
             one PII-redaction rule added on the client side.",
        ),
        (
            "RFC review — mobile push notification grouping",
            "Reviewed the push-notification grouping RFC. Approved as written. \
             Mobile rollout planned for next release.",
        ),
        (
            "Hiring loop — staff platform engineer onsite",
            "Sat in on the platform-engineer onsite. Strong yes from the panel. \
             Offer to be drafted by the recruiter.",
        ),
        // Row 12 — NEWEST. Still unrelated to the notes-module DB question.
        (
            "Weekly status compilation",
            "Compiled the weekly status report from the per-team channels. \
             Distribution list updated; report posted to #leadership.",
        ),
    ];
    assert_eq!(rows.len() as i64, ROW_COUNT);

    let mut stmt = conn
        .prepare("INSERT INTO log(ts, summary, detail) VALUES (?1, ?2, ?3)")
        .expect("seed: prepare insert");
    for (i, (summary, detail)) in rows.iter().enumerate() {
        let ts = base_ts_ms + (i as i64) * day_ms;
        stmt.execute(rusqlite::params![ts, summary, detail])
            .expect("seed: insert row");
    }
}
