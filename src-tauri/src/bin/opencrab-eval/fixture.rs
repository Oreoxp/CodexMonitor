// Fixture definitions — static array, embedded via `include_str!`.
//
// Each fixture is a single-agent team + one user turn + one assertion
// function. Three starter fixtures cover the P6 hot zones: PM emitting a
// well-formed `<propose_plan>` (Phase 3 regression), PM resolving
// ambiguity before decomposing, QA reporting findings without modifying
// code.

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
    /// Per-scenario assertion. Returns an outcome whose `passed` field
    /// decides pass/fail; the `notes` field surfaces a one-line summary.
    pub assert: fn(&assertions::TurnResult) -> assertions::AssertionOutcome,
}

pub static FIXTURES: &[Fixture] = &[
    Fixture {
        id: "01-pm-clear",
        description: "PM receives a clear request → must emit a well-formed <propose_plan>",
        team_json: include_str!("fixtures/01-pm-clear/team.json"),
        user_correspondent_id: "agent_pm",
        // Step 12: previous draft (audit-logging with implicit scope choices)
        // gave the agent five legitimate clarification hooks — it correctly
        // applied the charter's "resolve ambiguity before you decompose" rule
        // and deferred the plan, so the fixture FAILed without the prompt
        // system being at fault. Re-cut to nail every plan-shaping decision
        // up front so the correct charter behaviour is unambiguously "propose
        // now", not "clarify".
        user_turn: "We need to add audit logging to the Django admin panel. \
                    The specifics are all decided: log every create / edit / \
                    delete on all models registered in the admin; store \
                    field-level diffs (not full before/after snapshots); use \
                    the django-auditlog package (do not build a custom \
                    solution); surface the logs as a new 'Audit Log' page \
                    inside the Django admin; no special retention or \
                    performance requirements. Outline the implementation plan.",
        assert: assertions::pm_calls_propose_plan_tool,
    },
    Fixture {
        id: "02-pm-ambiguous",
        description: "PM receives an ambiguous request → must clarify before proposing",
        team_json: include_str!("fixtures/02-pm-ambiguous/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "Improve the API.",
        assert: assertions::pm_does_not_propose_when_ambiguous_tool,
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
    },
    // ───────────────────────────────────────────────────────────────────────
    // P6 Step 5 — b1 probe fixtures for memory tool visibility / behavior.
    // These are PROBES, not the formal b2 fixtures (which step 6 will land
    // with proper seed plumbing). Their job: end-to-end confirm that qwen
    // sees the memory MCP tools and uses them on a "remember this for
    // tomorrow" cue, plus the negative control on a trivial exchange.
    // ───────────────────────────────────────────────────────────────────────
    Fixture {
        id: "b1-probe-write",
        description: "PM decides + is asked to record it for tomorrow — should call log_progress",
        team_json: include_str!("fixtures/01-pm-clear/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "Quick one before I step away: we're going with PostgreSQL for the new \
                    notes module — it's read-heavy, ~10k users projected, and we already run \
                    a PostgreSQL cluster for unrelated services so the operational cost is \
                    near-zero. Acknowledge the decision and record it so when I check back \
                    tomorrow you remember exactly what we landed on and why.",
        assert: assertions::pm_logs_progress_after_decision,
    },
    Fixture {
        id: "b1-probe-trivial",
        description: "PM gets a trivial ack — should NOT call log_progress",
        team_json: include_str!("fixtures/01-pm-clear/team.json"),
        user_correspondent_id: "agent_pm",
        user_turn: "thanks — talking later",
        assert: assertions::pm_does_not_log_on_trivial,
    },
];
