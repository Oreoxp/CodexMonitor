// Phase 5 Step 2 — daily-memory search backend for `opencrab-memory-mcp`.
//
// Read-only. The corpus is ONE agent's `project-memory/<YYYY-MM-DD>.md`
// journal files. `search` builds a throwaway in-memory SQLite FTS5 index on
// every call and discards it — the markdown files are the only source of
// truth, so a persistent index would only add staleness; the corpus is a
// handful of small files, so a rebuild is sub-millisecond. `get` reads one
// day's file directly.
//
// Index granularity: paragraph — a blank-line-delimited block. One FTS5 row
// per paragraph, so a search hit maps to a single self-contained paragraph
// that becomes the snippet. Blank-line splitting makes no assumption about
// the day file's internal markdown structure (that format is owned by the
// Phase 5 write layer, a later step).

use std::path::Path;

use rusqlite::{params, Connection};
use serde::Serialize;

/// Default number of search hits when the caller does not pass `limit`.
const DEFAULT_SEARCH_LIMIT: usize = 6;
/// Hard cap on `limit` so one call cannot pull the whole archive.
const MAX_SEARCH_LIMIT: usize = 25;
/// Per-snippet character cap. Paragraph-granular snippets are normally well
/// under this; the cap only guards against one pathologically long block.
const MAX_SNIPPET_CHARS: usize = 800;

/// One search hit: the day it came from + the matched paragraph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryHit {
    pub date: String,
    pub snippet: String,
}

/// A whole day's journal, as returned by `get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryDocument {
    pub date: String,
    pub found: bool,
    pub content: String,
}

#[derive(Debug)]
pub enum BackendError {
    /// `date` was not a `YYYY-MM-DD` stamp — rejected before any path join
    /// (this is the path-traversal guard for `get`).
    InvalidDate(String),
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::InvalidDate(d) => {
                write!(f, "date must be a YYYY-MM-DD stamp, got {d:?}")
            }
            BackendError::Io(e) => write!(f, "memory I/O error: {e}"),
            BackendError::Sqlite(e) => write!(f, "memory index error: {e}"),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<std::io::Error> for BackendError {
    fn from(e: std::io::Error) -> Self {
        BackendError::Io(e)
    }
}

impl From<rusqlite::Error> for BackendError {
    fn from(e: rusqlite::Error) -> Self {
        BackendError::Sqlite(e)
    }
}

/// Clamp a caller-supplied `limit` into `1..=MAX_SEARCH_LIMIT`, defaulting
/// when absent.
pub fn clamp_limit(requested: Option<u64>) -> usize {
    match requested {
        Some(n) => (n as usize).clamp(1, MAX_SEARCH_LIMIT),
        None => DEFAULT_SEARCH_LIMIT,
    }
}

/// Full-text search the agent's daily-memory archive. Returns the most
/// relevant paragraphs (bm25-ranked, best first), each tagged with its date.
/// An absent / empty `memory_dir`, or a `query` with no usable terms, yields
/// an empty result rather than an error.
pub fn search(
    memory_dir: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryHit>, BackendError> {
    let Some(match_expr) = build_fts_match(query) else {
        return Ok(Vec::new());
    };
    let corpus = load_corpus(memory_dir)?;
    if corpus.is_empty() {
        return Ok(Vec::new());
    }

    let conn = Connection::open_in_memory()?;
    conn.execute_batch("CREATE VIRTUAL TABLE mem USING fts5(date UNINDEXED, body);")?;
    {
        let mut insert = conn.prepare("INSERT INTO mem(date, body) VALUES (?1, ?2)")?;
        for (date, paragraphs) in &corpus {
            for paragraph in paragraphs {
                insert.execute(params![date, paragraph])?;
            }
        }
    }

    let mut stmt = conn
        .prepare("SELECT date, body FROM mem WHERE mem MATCH ?1 ORDER BY bm25(mem) LIMIT ?2")?;
    let rows = stmt.query_map(params![match_expr, limit as i64], |row| {
        let date: String = row.get(0)?;
        let body: String = row.get(1)?;
        Ok(MemoryHit {
            date,
            snippet: truncate_snippet(&body),
        })
    })?;

    let mut hits = Vec::new();
    for hit in rows {
        hits.push(hit?);
    }
    Ok(hits)
}

/// Return one full day of the agent's journal. `date` must be a
/// `YYYY-MM-DD` stamp — anything else is rejected up front so a crafted
/// argument cannot escape `memory_dir`. A date with no file on disk is a
/// normal `found: false` result, not an error.
pub fn get(memory_dir: &Path, date: &str) -> Result<MemoryDocument, BackendError> {
    if !is_date_stamp(date) {
        return Err(BackendError::InvalidDate(date.to_string()));
    }
    let path = memory_dir.join(format!("{date}.md"));
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(MemoryDocument {
            date: date.to_string(),
            found: true,
            content,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MemoryDocument {
            date: date.to_string(),
            found: false,
            content: String::new(),
        }),
        Err(e) => Err(BackendError::Io(e)),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read every `YYYY-MM-DD.md` file under `memory_dir` and split each into
/// non-empty paragraphs. A missing directory yields an empty corpus.
fn load_corpus(memory_dir: &Path) -> Result<Vec<(String, Vec<String>)>, BackendError> {
    let entries = match std::fs::read_dir(memory_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(BackendError::Io(e)),
    };
    let mut corpus: Vec<(String, Vec<String>)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(date) = date_from_filename(&file_name.to_string_lossy()) else {
            continue;
        };
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(content) => content,
            // A file deleted between readdir and read is simply skipped.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(BackendError::Io(e)),
        };
        let paragraphs = split_paragraphs(&content);
        if !paragraphs.is_empty() {
            corpus.push((date, paragraphs));
        }
    }
    // Deterministic (newest-first) order. bm25 decides the final ranking, but
    // a stable corpus keeps insert order — and bm25 ties — reproducible.
    corpus.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(corpus)
}

/// `true` iff `s` is exactly `dddd-dd-dd`. Used both to pick journal files
/// out of the directory and to validate the `get` argument.
fn is_date_stamp(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b'-',
            _ => c.is_ascii_digit(),
        })
}

fn date_from_filename(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".md")?;
    is_date_stamp(stem).then(|| stem.to_string())
}

/// Split text into paragraphs — maximal runs of non-blank lines, each
/// trimmed. Empty paragraphs are dropped. `str::lines` already normalizes
/// `\r\n`, so this is newline-style agnostic.
fn split_paragraphs(content: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            flush_paragraph(&mut current, &mut paragraphs);
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    flush_paragraph(&mut current, &mut paragraphs);
    paragraphs
}

fn flush_paragraph(current: &mut String, out: &mut Vec<String>) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    current.clear();
}

/// Turn a free-text query into an FTS5 MATCH expression. Each whitespace
/// token is wrapped in a double-quoted string literal (so FTS5 query
/// operators inside the token are treated as content, not syntax — the only
/// in-literal escape is `"` → `""`), and tokens are OR-joined so the search
/// is recall-oriented with bm25 surfacing the best matches. Tokens with no
/// alphanumeric character are dropped; a query that yields none returns
/// `None` (the caller treats that as "no results").
fn build_fts_match(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .filter(|token| token.chars().any(|c| c.is_alphanumeric()))
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

fn truncate_snippet(body: &str) -> String {
    if body.chars().count() <= MAX_SNIPPET_CHARS {
        body.to_string()
    } else {
        let head: String = body.chars().take(MAX_SNIPPET_CHARS).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(dir: &Path, date: &str, body: &str) {
        std::fs::write(dir.join(format!("{date}.md")), body).expect("seed day file");
    }

    #[test]
    fn search_finds_a_seeded_term() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), "2026-05-20", "We adopted the Postgres migration plan.");
        seed(tmp.path(), "2026-05-10", "Unrelated standup notes about lunch.");

        let hits = search(tmp.path(), "Postgres", 6).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].date, "2026-05-20");
        assert!(hits[0].snippet.contains("Postgres migration"));
    }

    #[test]
    fn search_ranks_the_more_relevant_paragraph_first_via_bm25() {
        let tmp = tempfile::tempdir().unwrap();
        // 2026-05-20 matches BOTH query terms; 2026-05-10 matches only one.
        seed(
            tmp.path(),
            "2026-05-20",
            "We chose the alpha approach over beta after the review.",
        );
        seed(tmp.path(), "2026-05-10", "Logged a few alpha notes.");

        let hits = search(tmp.path(), "alpha beta", 6).unwrap();
        assert_eq!(hits.len(), 2);
        // bm25 ranks the two-term match above the one-term match.
        assert_eq!(hits[0].date, "2026-05-20");
        assert_eq!(hits[1].date, "2026-05-10");
    }

    #[test]
    fn search_snippet_is_the_whole_matched_paragraph() {
        let tmp = tempfile::tempdir().unwrap();
        // Two paragraphs; only the second mentions the term.
        seed(
            tmp.path(),
            "2026-05-20",
            "Morning: triaged the inbox.\n\nAfternoon: shipped the auth refactor.",
        );

        let hits = search(tmp.path(), "auth", 6).unwrap();
        assert_eq!(hits.len(), 1);
        // The hit is the matched paragraph, self-contained — not the whole file.
        assert_eq!(hits[0].snippet, "Afternoon: shipped the auth refactor.");
    }

    #[test]
    fn search_returns_empty_when_directory_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("project-memory");
        assert_eq!(search(&missing, "anything", 6).unwrap(), Vec::new());
    }

    #[test]
    fn search_returns_empty_for_empty_and_blank_corpora() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty directory.
        assert_eq!(search(tmp.path(), "anything", 6).unwrap(), Vec::new());
        // Directory with only whitespace-only files.
        seed(tmp.path(), "2026-05-20", "   \n\n  \t\n");
        assert_eq!(search(tmp.path(), "anything", 6).unwrap(), Vec::new());
    }

    #[test]
    fn search_returns_empty_for_query_with_no_searchable_terms() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), "2026-05-20", "Real content here.");
        assert_eq!(search(tmp.path(), "  !!!  ??? ", 6).unwrap(), Vec::new());
    }

    #[test]
    fn search_ignores_non_journal_files() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), "2026-05-20", "journal mentions widgets");
        std::fs::write(tmp.path().join("notes.md"), "stray widgets file").unwrap();
        std::fs::write(tmp.path().join("2026-05-20.txt"), "wrong widgets ext").unwrap();

        let hits = search(tmp.path(), "widgets", 6).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].date, "2026-05-20");
    }

    #[test]
    fn search_respects_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        for day in 10..20 {
            seed(tmp.path(), &format!("2026-05-{day}"), "shared keyword line");
        }
        let hits = search(tmp.path(), "keyword", 3).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn get_returns_full_content_for_an_existing_day() {
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path(), "2026-05-20", "Line one.\n\nLine two.");
        let doc = get(tmp.path(), "2026-05-20").unwrap();
        assert!(doc.found);
        assert_eq!(doc.date, "2026-05-20");
        assert_eq!(doc.content, "Line one.\n\nLine two.");
    }

    #[test]
    fn get_reports_not_found_for_a_day_with_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = get(tmp.path(), "2026-01-01").unwrap();
        assert!(!doc.found);
        assert_eq!(doc.content, "");
    }

    #[test]
    fn get_rejects_non_date_arguments_as_path_traversal_guard() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["../../etc/passwd", "2026-05", "not-a-date", ""] {
            assert!(
                matches!(get(tmp.path(), bad), Err(BackendError::InvalidDate(_))),
                "expected InvalidDate for {bad:?}",
            );
        }
    }

    #[test]
    fn split_paragraphs_breaks_on_blank_lines() {
        assert_eq!(
            split_paragraphs("a\nb\n\n\nc\n"),
            vec!["a\nb".to_string(), "c".to_string()],
        );
        assert_eq!(split_paragraphs("   \n\n  "), Vec::<String>::new());
    }
}
