// Sidecar session status enum.
//
// Phase 1 Spike A scope: defined for future use; not currently emitted to the
// frontend. Future phases will mirror `codex_session::status::*` and emit a
// `sidecar/sessionStatus` event via the existing `EventSink` infrastructure.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub(crate) enum SidecarStatus {
    Starting,
    Running,
    Stopped,
    Crashed,
}
