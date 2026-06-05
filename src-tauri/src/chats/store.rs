// `chats` sqlite store — sibling table inside `<workspace>/.opencrab/state.sqlite`.
//
// Phase 7 S1 / S6-1 — the chat read-DB. This is the UI render source for team
// conversations: structured agent messages (observed off the per-thread tap in
// S6-2) and the human's input (captured at the send command in S6-3) get
// appended here, and the frontend (S6-3) renders a conversation by querying
// this table instead of replaying the codex rollout via `thread/resume`. The
// rollout stays the *continuation* source; `chats` is purely read/render. See
// the phased-plan §"Phase 7 S1 — Direction".
//
// Coexistence with LangGraph's SqliteSaver + the `tasks` table:
//   - Same file, same WAL discipline as `tasks` (forced on every open; it
//     persists in the file header). SqliteSaver owns the `checkpoint*` /
//     `writes` tables; Rust owns `tasks` + (now) `chats`. The CLAUDE.md
//     carve-out was widened to `tasks` + `chats` for exactly this.
//   - `IF NOT EXISTS` everywhere so init order across the three writers does
//     not matter.
//
// Single-writer: only the Tauri host writes `chats` (S6-2 observer + S6-3
// user-input). No cross-process writer contends for the table.
//
// Dormant this block: S6-1 builds the storage layer only — there is NO
// production caller yet. The producers land in S6-2 (the tap observer) and
// S6-3 (user-input capture + the read-only Tauri command). The module-level
// `#![allow(dead_code)]` reflects that; drop it once those callers wire in.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, TransactionBehavior};
use serde::Serialize;

const CREATE_TABLE_SQL: &str = "
CREATE TABLE IF NOT EXISTS chats (
    id           INTEGER PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    team_id      TEXT NOT NULL,
    thread_id    TEXT,
    sender       TEXT NOT NULL,
    recipient    TEXT,
    role         TEXT,
    kind         TEXT NOT NULL,
    content      TEXT NOT NULL DEFAULT '',
    ts           TEXT NOT NULL,
    seq          INTEGER NOT NULL
)";

// Two indices, not one: the read query filters on
// `(thread_id = ? OR recipient = ?)`, and a single composite index cannot
// cover an OR across two different columns. SQLite's planner can use one
// index per OR-branch (the "OR-by-union" optimization) when each branch has
// its own index, so we give it one on each column — both workspace-scoped and
// `(ts, seq)`-ordered to match the `ORDER BY ts, seq` of the read.
const CREATE_INDEX_THREAD_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_chats_workspace_thread_ts
    ON chats (workspace_id, thread_id, ts, seq)";

const CREATE_INDEX_RECIPIENT_SQL: &str = "
CREATE INDEX IF NOT EXISTS idx_chats_workspace_recipient_ts
    ON chats (workspace_id, recipient, ts, seq)";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `chats` has no state machine — the only failure modes are the underlying
/// SQLite error and the filesystem error preparing the sqlite path. The
/// `[ERR_*]` prefixes mirror `tasks::state_machine::TaskError` so the S6-3
/// read command surfaces the same contract the frontend already matches on.
#[derive(Debug)]
pub(crate) enum ChatError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

const ERR_CODE_SQLITE: &str = "[ERR_SQLITE]";
const ERR_CODE_IO: &str = "[ERR_IO]";

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Sqlite(e) => write!(f, "{ERR_CODE_SQLITE} sqlite: {e}"),
            ChatError::Io(e) => write!(f, "{ERR_CODE_IO} io: {e}"),
        }
    }
}

impl std::error::Error for ChatError {}

impl From<rusqlite::Error> for ChatError {
    fn from(e: rusqlite::Error) -> Self {
        ChatError::Sqlite(e)
    }
}

impl From<std::io::Error> for ChatError {
    fn from(e: std::io::Error) -> Self {
        ChatError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

/// The kinds S6-1 seeds. `kind` is stored as TEXT and read back raw (see
/// `ChatRow.kind`) so a newer writer can add a kind without breaking an older
/// reader; this enum is the typed *write* surface for the canonical values.
/// Producers: S6-2 writes `SendMessage` / `AgentMessage`; S6-3 writes
/// `UserInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatKind {
    UserInput,
    AgentMessage,
    SendMessage,
}

impl ChatKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ChatKind::UserInput => "user_input",
            ChatKind::AgentMessage => "agent_message",
            ChatKind::SendMessage => "send_message",
        }
    }
}

/// Append seed. `id` is assigned by SQLite (rowid) and `seq` is computed by
/// `append_chat`; everything else is caller-supplied. `ts` is caller-stamped
/// (`Utc::now().to_rfc3339()` in production) rather than wall-clocked inside
/// the store, so the same-`ts` → `seq` ordering contract stays deterministic
/// and unit-testable.
pub(crate) struct NewChat {
    pub(crate) workspace_id: String,
    pub(crate) team_id: String,
    /// The conversation/thread this row belongs to (the sender's thread for
    /// agent messages). `None` for thread-less rows (broadcast / narration).
    pub(crate) thread_id: Option<String>,
    pub(crate) sender: String,
    /// `None` for broadcast / narration with no single addressee.
    pub(crate) recipient: Option<String>,
    /// Display-only (e.g. role label). Never branched on.
    pub(crate) role: Option<String>,
    pub(crate) kind: ChatKind,
    pub(crate) content: String,
    pub(crate) ts: String,
}

/// A persisted chat row. `kind` is the raw stored string (forward-compatible
/// reads); `role` is display-only.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatRow {
    pub(crate) id: i64,
    pub(crate) workspace_id: String,
    pub(crate) team_id: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) sender: String,
    pub(crate) recipient: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) kind: String,
    pub(crate) content: String,
    pub(crate) ts: String,
    pub(crate) seq: i64,
}

fn row_to_chat(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatRow> {
    Ok(ChatRow {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        team_id: row.get("team_id")?,
        thread_id: row.get("thread_id")?,
        sender: row.get("sender")?,
        recipient: row.get("recipient")?,
        role: row.get("role")?,
        kind: row.get("kind")?,
        content: row.get("content")?,
        ts: row.get("ts")?,
        seq: row.get("seq")?,
    })
}

// ---------------------------------------------------------------------------
// Path resolution + connection lifecycle (mirrors `tasks::store`)
// ---------------------------------------------------------------------------

/// `<workspace_root>/.opencrab/state.sqlite`, creating `.opencrab` if missing.
/// Grounded in `crate::paths` (the single source of truth) so the path is
/// byte-identical to the one `tasks::store` opens — same file, by construction.
fn state_sqlite_path(workspace_root: &Path) -> Result<PathBuf, ChatError> {
    let dir = crate::paths::project_root(workspace_root);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(crate::paths::project_state_sqlite(&dir))
}

/// Open `state.sqlite` and run idempotent migrations. WAL + `synchronous=NORMAL`
/// + `busy_timeout` match `tasks::store::open_and_init` so the two Rust writers
/// share the file on identical terms.
pub(crate) fn open_and_init(workspace_root: &Path) -> Result<Connection, ChatError> {
    let path = state_sqlite_path(workspace_root)?;
    let conn = Connection::open(&path)?;
    conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    conn.execute("PRAGMA synchronous=NORMAL", [])?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    init_schema(&conn)?;
    Ok(conn)
}

pub(crate) fn init_schema(conn: &Connection) -> Result<(), ChatError> {
    conn.execute(CREATE_TABLE_SQL, [])?;
    conn.execute(CREATE_INDEX_THREAD_SQL, [])?;
    conn.execute(CREATE_INDEX_RECIPIENT_SQL, [])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Append one chat row. `seq` is `MAX(seq) + 1` over the rows that share this
/// row's `(workspace_id, ts)` — a stable tiebreaker so rows stamped with an
/// identical `ts` keep their insertion order under `ORDER BY ts, seq`. The
/// `SELECT MAX` and the `INSERT` run inside an `Immediate` transaction so the
/// computed `seq` cannot collide with a concurrent append (single-writer in
/// practice; the transaction is the belt-and-braces).
pub(crate) fn append_chat(conn: &mut Connection, new: NewChat) -> Result<ChatRow, ChatError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM chats WHERE workspace_id = ?1 AND ts = ?2",
        params![new.workspace_id, new.ts],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO chats \
         (workspace_id, team_id, thread_id, sender, recipient, role, kind, content, ts, seq) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            new.workspace_id,
            new.team_id,
            new.thread_id,
            new.sender,
            new.recipient,
            new.role,
            new.kind.as_str(),
            new.content,
            new.ts,
            seq,
        ],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(ChatRow {
        id,
        workspace_id: new.workspace_id,
        team_id: new.team_id,
        thread_id: new.thread_id,
        sender: new.sender,
        recipient: new.recipient,
        role: new.role,
        kind: new.kind.as_str().to_string(),
        content: new.content,
        ts: new.ts,
        seq,
    })
}

/// Path-driven entry point for callers without an open connection (S6-2 /
/// S6-3). Opens + migrates the store, then appends. Run inside
/// `spawn_blocking` from async contexts — `rusqlite::Connection` is `!Send`.
pub(crate) fn append_chat_at_path(
    workspace_root: &Path,
    new: NewChat,
) -> Result<ChatRow, ChatError> {
    let mut conn = open_and_init(workspace_root)?;
    append_chat(&mut conn, new)
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

/// Recipient-inclusive read of one conversation: every row whose `thread_id`
/// matches the conversation thread, PLUS every row addressed to `agent_id`
/// via `recipient` (so inbound Dev↔Dev messages — which live under the
/// sender's own `thread_id` — are stitched into the viewer's conversation).
///
/// Param roles (both supplied by the caller in v1):
///   - `thread_id`: the conversation thread being rendered.
///   - `agent_id`: the viewing agent, matched against `recipient`.
///
/// Ordered ascending by `(ts, seq)` for the render layer. Internally the rows
/// are fetched newest-first so `limit` keeps the most-recent window and
/// `before_ts` pages backward, then reversed to ascending before return.
///
/// `before_ts`: exclusive upper bound (`ts < before_ts`) for "load older".
/// v1 cursor is `ts`-only; if two rows straddle the boundary on an identical
/// `ts`, the lower-`seq` sibling is skipped — acceptable given the sub-second
/// `ts` resolution callers stamp. A `(ts, seq)` cursor is a future refinement.
///
/// `limit`: max rows. SQLite's `LIMIT -1` semantics apply — pass a negative
/// value for "no limit".
pub(crate) fn list_thread_chats(
    conn: &Connection,
    workspace_id: &str,
    thread_id: &str,
    agent_id: &str,
    limit: i64,
    before_ts: Option<&str>,
) -> Result<Vec<ChatRow>, ChatError> {
    let mut stmt = conn.prepare(
        "SELECT id, workspace_id, team_id, thread_id, sender, recipient, role, kind, \
         content, ts, seq \
         FROM chats \
         WHERE workspace_id = ?1 \
           AND (thread_id = ?2 OR recipient = ?3) \
           AND (?4 IS NULL OR ts < ?4) \
         ORDER BY ts DESC, seq DESC \
         LIMIT ?5",
    )?;
    let rows = stmt.query_map(
        params![workspace_id, thread_id, agent_id, before_ts, limit],
        row_to_chat,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    // Fetched newest-first; flip to ascending `(ts, seq)` for rendering.
    out.reverse();
    Ok(out)
}

/// Path-driven read entry point (S6-3 Tauri command will wrap this).
pub(crate) fn list_thread_chats_at_path(
    workspace_root: &Path,
    workspace_id: &str,
    thread_id: &str,
    agent_id: &str,
    limit: i64,
    before_ts: Option<&str>,
) -> Result<Vec<ChatRow>, ChatError> {
    let conn = open_and_init(workspace_root)?;
    list_thread_chats(&conn, workspace_id, thread_id, agent_id, limit, before_ts)
}
