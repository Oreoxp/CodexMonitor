// Phase 7 S1 / S6-1 — `chats` read-DB storage layer.
//
// The UI render source for team conversations (full rationale in `store`).
// S6-1 ships the storage layer only — schema + write/read API — with NO
// production caller yet. Producers land in S6-2 (the tap observer that
// persists structured agent messages) and S6-3 (user-input capture + the
// read-only Tauri command). Callers reach the API via `crate::chats::store::*`.

pub(crate) mod commands;
pub(crate) mod store;

#[cfg(test)]
mod tests;

// Glob re-export (matches `tasks::mod`): brings both `list_thread_chats` and
// the `#[tauri::command]`-generated `__cmd__list_thread_chats` helper that
// `generate_handler!` needs in `lib.rs`.
pub(crate) use commands::*;
