// Phase 4 Step 6 — event types + (from, to) → variant mapping.
//
// `TeamEvent` is the wire shape persisted in `events.jsonl`. Eight
// task-lifecycle bodies are defined; only some fire in Phase 3 today
// (see mapping below + the §Audit section of the Step 6 implementation
// notes). All eight ship at v1 so P5/P7/P8 consumers can decode every
// future emission without a schema bump.
//
// Serialization decisions:
//   * `#[serde(tag = "eventType", rename_all = "snake_case")]` on the
//     body flattens the variant tag to the top level, alongside the
//     event envelope. Combined with `#[serde(flatten)]` on the
//     `TeamEvent.body` field, every JSON line ends up with all fields at
//     the top level — easy `jq` / `grep` / log-tail consumption.
//   * `schemaVersion: u32` (integer). Phase 3 chose the same shape for
//     `tasks-proposed`; Step 6 stays consistent.
//   * `event_id` is a `uuid::Uuid::new_v4()` string. The crate is already
//     a dependency.
//   * `task_id: Option<String>` — required for Phase 4 (every variant is
//     task-scoped) but kept Option-shaped so future non-task events
//     (message_sent, supervisor_observation, …) fit without a v2 bump.
//   * Timestamps are ISO-8601 UTC with millisecond precision and a `Z`
//     suffix (e.g. `2026-05-18T10:30:45.123Z`). Lex-sortable so a
//     downstream consumer can order events by string comparison without
//     parsing.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::tasks::TaskStatus;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "eventType", rename_all = "snake_case")]
// `Task*` prefix is intentional — the variants ARE the event-type names
// that downstream consumers grep for in events.jsonl (`task_proposed`,
// `task_approved`, …). Stripping the prefix would force consumers to
// reconstruct it from the JSON, which defeats the point.
#[allow(clippy::enum_variant_names)]
pub(crate) enum TeamEventBody {
    TaskProposed {
        agent_id: String,
        title: String,
        description: String,
    },
    TaskApproved {
        approved_by: String,
    },
    TaskRejected {
        rejected_by: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    TaskStarted {
        agent_id: String,
    },
    TaskBlocked {
        agent_id: String,
        reason: String,
    },
    TaskUnblocked {
        agent_id: String,
    },
    TaskDone {
        agent_id: String,
    },
    TaskArchived {
        archived_by: String,
    },
    /// Phase 5 Step 3 — a pre-compaction memory flush completed for an agent.
    /// Not task-scoped: `TeamEvent.task_id` stays `None`. This is the durable,
    /// auditable record that the flush turn-pair was housekeeping (a later
    /// phase's UI re-build reads it to suppress the flush from the chat).
    MemoryFlush {
        agent_id: String,
        /// The codex turn id of the flush turn (the system-issued user-role
        /// message that asked for `<daily_log>`). Carried so a later phase
        /// can correlate this audit row with the chat event stream — codex
        /// will emit `item/started` + `turn/completed` for the flush turn,
        /// and the UI re-build needs the turn id to suppress that pair from
        /// the chat.
        flush_turn_id: String,
        chars_written: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TeamEvent {
    #[serde(rename = "schemaVersion")]
    pub(crate) schema_version: u32,
    pub(crate) event_id: String,
    pub(crate) timestamp: String,
    pub(crate) team_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(crate) task_id: Option<String>,
    #[serde(flatten)]
    pub(crate) body: TeamEventBody,
}

impl TeamEvent {
    /// `schemaVersion` value for every event written today. Bump the
    /// constant + branch the consumer when the wire shape needs a
    /// breaking change; additive fields are safe at v1.
    pub(crate) const SCHEMA_VERSION: u32 = 1;

    pub(crate) fn new(team_id: &str, task_id: Option<&str>, body: TeamEventBody) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            event_id: uuid_v4_string(),
            timestamp: iso8601_now_ms(),
            team_id: team_id.to_string(),
            task_id: task_id.map(str::to_string),
            body,
        }
    }
}

// ---------------------------------------------------------------------------
// (from, to) → TeamEventBody mapping
// ---------------------------------------------------------------------------

/// Translate a successful state-machine transition into the matching
/// `TeamEventBody`. Returns `None` only for combinations the state
/// machine considers illegal — defensive guard, the caller should not
/// reach this path because `validate_transition` runs upstream.
///
/// `actor` populates `approved_by` / `archived_by` / `agent_id` fields
/// uniformly. `feedback` carries:
///   * the rejection reason on `Proposed → Archived` (→ TaskRejected),
///   * the block reason on `Running → Blocked` (→ TaskBlocked).
///
/// Calling code MUST capture the prior status before running the
/// transition — the same status is no longer available after the row
/// has been written. `transition_task_at_path` callers do this with a
/// single `store::get` before the transition.
pub(crate) fn event_for_transition(
    from: TaskStatus,
    to: TaskStatus,
    actor: &str,
    feedback: Option<&str>,
) -> Option<TeamEventBody> {
    use TaskStatus::*;
    match (from, to) {
        // Approval gate (P3 production).
        (Proposed, Ready) => Some(TeamEventBody::TaskApproved {
            approved_by: actor.to_string(),
        }),
        // Reject is its own variant (NOT TaskArchived) even though the
        // state-machine destination is the same — distinguishing them in
        // the event stream matters for P5 memory + P7 audit UI.
        (Proposed, Archived) => Some(TeamEventBody::TaskRejected {
            rejected_by: actor.to_string(),
            reason: feedback.map(str::to_string),
        }),
        // Dispatch (P5+ wires production callers).
        (Ready, Running) => Some(TeamEventBody::TaskStarted {
            agent_id: actor.to_string(),
        }),
        // Cancel before run.
        (Ready, Archived) => Some(TeamEventBody::TaskArchived {
            archived_by: actor.to_string(),
        }),
        // Block / unblock / complete (P8 supervisor + Dev block flow).
        (Running, Blocked) => Some(TeamEventBody::TaskBlocked {
            agent_id: actor.to_string(),
            reason: feedback.unwrap_or_default().to_string(),
        }),
        (Running, Done) => Some(TeamEventBody::TaskDone {
            agent_id: actor.to_string(),
        }),
        (Blocked, Running) => Some(TeamEventBody::TaskUnblocked {
            agent_id: actor.to_string(),
        }),
        (Blocked, Archived) | (Done, Archived) => Some(TeamEventBody::TaskArchived {
            archived_by: actor.to_string(),
        }),
        // Anything else is rejected by `validate_transition`; we never
        // reach here in practice. Returning `None` keeps the caller's
        // emit graceful (skip + log) instead of panicking.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Timestamp + uuid helpers
// ---------------------------------------------------------------------------

/// Current UTC instant formatted as ISO-8601 with millisecond precision
/// and a `Z` suffix. Chrono's `to_rfc3339_opts(SecondsFormat::Millis, true)`
/// pins the format across versions and platforms — the alternative
/// `to_rfc3339()` includes nanoseconds, which makes round-tripping
/// brittle (we'd lose digits on re-parse and produce a different string).
pub(crate) fn iso8601_now_ms() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn uuid_v4_string() -> String {
    Uuid::new_v4().to_string()
}
