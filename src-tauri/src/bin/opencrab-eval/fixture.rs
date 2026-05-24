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
];
