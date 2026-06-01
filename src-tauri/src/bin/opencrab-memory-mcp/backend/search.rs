use super::{open, BackendError, Embedder, vec_to_match_json};
use rusqlite::{params, Connection};
use std::path::Path;
use serde::Serialize;

/// Reciprocal-rank-fusion mixing constant. RRF score for one entry is
/// `Σ 1 / (RRF_K + rank)` summed over the ranked lists it appears in.
/// `60` is the value Cormack/Clarke/Buettcher recommended in the original
/// paper and is the default in most implementations.
pub const RRF_K: usize = 60;

/// Candidate pool size for the hybrid search: `limit * SEARCH_POOL_MULTIPLIER`
/// clamped to a sensible floor. The pool is what each underlying ranker
/// (FTS, KNN) returns; RRF picks the best `limit` from the union.
pub const SEARCH_POOL_MULTIPLIER: usize = 4;
pub const SEARCH_POOL_MIN: usize = 20;

/// KNN over-fetch factor. vec0 applies its `k = ?` nearest-k cut BEFORE the
/// joined `superseded_by IS NULL` filter, so asking for exactly `pool_size`
/// would under-fill whenever some of the nearest rows are superseded. We ask
/// vec0 for `pool_size * KNN_OVERFETCH` (a full-scan, so a larger k is
/// effectively free) and keep the first `pool_size` LIVE ids.
pub const KNN_OVERFETCH: usize = 2;

/// Hard wall-clock budget for embedding the user's query at search
/// time. If the embedder takes longer than this, we abort and fall
/// back to pure FTS — `memory_search` MUST stay snappy even if the
/// provider is slow.
pub const QUERY_EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Hybrid search: FTS bm25 + (optional) vector KNN, merged via RRF.
///
/// Returns the most relevant rows in the merged ranking (best first).
/// Degrades gracefully to pure-FTS in every failure mode of the vector
/// path: no `embedder`, embed error, embed timeout, dim mismatch, or
/// empty `log_vec`. The `MemoryHit` shape is unchanged from the
/// pre-S4 FTS-only API.
///
/// **Signature note:** we take `memory_db: &Path` rather than the
/// `&Connection` originally drafted because `rusqlite::Connection`
/// is `!Sync`, so holding `&Connection` across the embed `.await`
/// would make this function's future non-`Send` — and rmcp's
/// `ServerHandler::call_tool` requires `Send + 'static`. Opening once
/// per phase is cheap (WAL DB, no schema work on already-migrated
/// files; `ensure_vec_extension` is OnceLock-guarded).
pub async fn search(
    memory_db: &Path,
    embedder: Option<&dyn Embedder>,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryHit>, BackendError> {
    search_with_timeout(memory_db, embedder, query, limit, QUERY_EMBED_TIMEOUT).await
}

/// Same as [`search`] but with an injectable embed timeout for tests.
///
/// Three phases keep the !Sync `Connection` out of the future's
/// captured state across `.await`:
///   1. sync — open conn, FTS bm25 ranking, drop conn
///   2. async, no conn — embed the query under a timeout
///   3. sync — re-open conn, KNN over log_vec, RRF merge, fetch
pub(crate) async fn search_with_timeout(
    memory_db: &Path,
    embedder: Option<&dyn Embedder>,
    query: &str,
    limit: usize,
    embed_timeout: std::time::Duration,
) -> Result<Vec<MemoryHit>, BackendError> {
    if query.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let pool_size = (limit * SEARCH_POOL_MULTIPLIER).max(SEARCH_POOL_MIN);

    // ---- Phase 1 (sync): FTS bm25; conn scoped to this block.
    let fts_ranked: Vec<i64> = {
        let conn = open(memory_db)?;
        match build_fts_match(query) {
            Some(match_expr) => fts_ranked_ids(&conn, &match_expr, pool_size)?,
            None => Vec::new(), // query had no FTS-usable tokens — vector may still hit
        }
    }; // conn dropped here — does NOT live across the embed await below

    // ---- Phase 2 (async, no DB access): embed the query ----
    let query_vec: Option<Vec<f32>> = match embedder {
        Some(emb) => match embed_query_with_timeout(emb, query, embed_timeout).await {
            Ok(v) => Some(v),
            Err(reason) => {
                eprintln!("[search] vector path skipped: {reason}");
                None
            }
        },
        None => None,
    };

    // ---- Phase 3 (sync): re-open, KNN, merge, fetch.
    let conn = open(memory_db)?;
    let vec_ranked: Vec<i64> = match query_vec {
        Some(v) => match knn_ranked_ids(&conn, &v, pool_size) {
            Ok(ids) => ids,
            Err(reason) => {
                eprintln!("[search] vector path skipped: {reason}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    let merged = rrf_merge(&[&fts_ranked, &vec_ranked], limit);
    fetch_hits_in_order(&conn, &merged)
}

/// FTS bm25 ranking only, returns ordered ids (best first).
pub(crate) fn fts_ranked_ids(
    conn: &Connection,
    match_expr: &str,
    pool_size: usize,
) -> Result<Vec<i64>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT log.id FROM log_fts JOIN log ON log.id = log_fts.rowid \
         WHERE log_fts MATCH ?1 AND log.superseded_by IS NULL \
         ORDER BY bm25(log_fts) \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![match_expr, pool_size as i64], |row| {
        row.get::<_, i64>(0)
    })?;
    let mut out = Vec::with_capacity(pool_size);
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Async, no DB access: embed `query` under a wall-clock budget and
/// validate the returned vector's dim matches the embedder's
/// `dimensions()`. Returns `Err(reason)` for every failure mode the
/// caller should silently degrade to FTS on (timeout, embed error,
/// dim mismatch). Crucially this function does NOT take `&Connection`,
/// so the caller's `&Connection` borrow doesn't have to survive the
/// await (rusqlite's `Connection` is !Sync; that would break the
/// `Send`-ness of any future holding it across `.await`).
async fn embed_query_with_timeout(
    embedder: &dyn Embedder,
    query: &str,
    timeout: std::time::Duration,
) -> Result<Vec<f32>, String> {
    let texts = vec![query.to_string()];
    let vectors = match tokio::time::timeout(timeout, embedder.embed(&texts)).await {
        Ok(Ok(v)) => v,
        Ok(Err(err)) => return Err(format!("embed error: {err}")),
        Err(_) => return Err(format!("embed timed out after {timeout:?}")),
    };
    let q_vec = vectors
        .into_iter()
        .next()
        .ok_or_else(|| "embedder returned 0 vectors for 1 input".to_string())?;
    if q_vec.len() != embedder.dimensions() {
        return Err(format!(
            "query embedding dim {} != embedder.dimensions() {}",
            q_vec.len(),
            embedder.dimensions()
        ));
    }
    Ok(q_vec)
}

/// Sync: KNN over log_vec given an already-computed query embedding, JOINed
/// back to `log` so superseded rows are filtered out before they ever reach
/// RRF. Returns up to `pool_size` LIVE rowids in distance-ascending order.
///
/// vec0 applies its `k = ?` nearest-k constraint BEFORE the joined
/// `superseded_by IS NULL` predicate, so a plain `k = pool_size` would
/// under-fill when some of the k nearest are superseded. We over-fetch
/// `k = pool_size * KNN_OVERFETCH` (a vec0 full-scan, so the larger k is
/// effectively free) and keep the first `pool_size` live ids. `k = ?` (not
/// `LIMIT`) is what composes with the JOIN here — exactly what the original
/// `k = ?` choice was reserved for.
fn knn_ranked_ids(
    conn: &Connection,
    q_vec: &[f32],
    pool_size: usize,
) -> Result<Vec<i64>, String> {
    let q_json = vec_to_match_json(q_vec);
    let k = pool_size.saturating_mul(KNN_OVERFETCH);
    let mut stmt = conn
        .prepare(
            "SELECT log_vec.rowid FROM log_vec \
             JOIN log ON log.id = log_vec.rowid \
             WHERE log_vec.embedding MATCH ?1 AND k = ?2 \
               AND log.superseded_by IS NULL \
             ORDER BY distance",
        )
        .map_err(|e| format!("prepare KNN: {e}"))?;
    let rows = stmt
        .query_map(params![q_json, k as i64], |row| row.get::<_, i64>(0))
        .map_err(|e| format!("KNN query: {e}"))?;
    let mut out: Vec<i64> = Vec::with_capacity(pool_size);
    for r in rows {
        out.push(r.map_err(|e| format!("KNN row: {e}"))?);
        if out.len() >= pool_size {
            break;
        }
    }
    Ok(out)
}

/// Pure: Reciprocal Rank Fusion across N ranked lists.
///
/// Score for an id is `Σ 1 / (RRF_K + (rank + 1))` summed over the
/// lists it appears in; `rank` starts at 0 inside each list. Ties on
/// score are broken by id ascending so the order is deterministic.
pub(crate) fn rrf_merge(ranked_lists: &[&[i64]], limit: usize) -> Vec<i64> {
    use std::collections::HashMap;
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in ranked_lists {
        for (rank, &id) in list.iter().enumerate() {
            *scores.entry(id).or_insert(0.0) += 1.0 / (RRF_K as f64 + (rank + 1) as f64);
        }
    }
    let mut pairs: Vec<(i64, f64)> = scores.into_iter().collect();
    pairs.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    pairs.into_iter().take(limit).map(|(id, _)| id).collect()
}

/// JOIN final ids back to `log` and project into `MemoryHit`. Order
/// preserved as given (i.e. the RRF order).
fn fetch_hits_in_order(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<MemoryHit>, BackendError> {
    let mut stmt =
        conn.prepare("SELECT ts, summary, detail FROM log WHERE id = ?1")?;
    let mut hits = Vec::with_capacity(ids.len());
    for id in ids {
        let row = stmt
            .query_row(params![id], |r| {
                let ts: i64 = r.get(0)?;
                let summary: String = r.get(1)?;
                let detail: Option<String> = r.get(2)?;
                Ok(MemoryHit {
                    id: *id,
                    ts,
                    summary,
                    snippet: truncate_snippet(detail.as_deref().unwrap_or("")),
                })
            })
            .ok();
        if let Some(hit) = row {
            hits.push(hit);
        }
        // A merged id that no longer matches a log row (race vs delete?
        // future GC?) is silently dropped — caller just gets a shorter
        // list. Cleaner than surfacing an "id 42 vanished" error.
    }
    Ok(hits)
}

/// Fetch one log row by id. A missing id is a normal `found: false` result,
/// not an error.
pub fn get(memory_db: &Path, id: i64) -> Result<MemoryEntry, BackendError> {
    let conn = open(memory_db)?;
    let mut stmt =
        conn.prepare("SELECT id, ts, summary, detail FROM log WHERE id = ?1")?;
    let row = stmt
        .query_row(params![id], |row| {
            let id: i64 = row.get(0)?;
            let ts: i64 = row.get(1)?;
            let summary: String = row.get(2)?;
            let detail: Option<String> = row.get(3)?;
            Ok(MemoryEntry {
                id,
                found: true,
                ts,
                summary,
                detail,
            })
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(row.unwrap_or(MemoryEntry {
        id,
        found: false,
        ts: 0,
        summary: String::new(),
        detail: None,
    }))
}

/// Per-snippet character cap. The `detail` column is free-form, so a
/// pathological entry should not flood one search response.
const MAX_SNIPPET_CHARS: usize = 800;

/// One search hit: a row of `log`, with `detail` truncated to a snippet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryHit {
    pub id: i64,
    pub ts: i64,
    pub summary: String,
    pub snippet: String,
}

/// One full log entry, as returned by `get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryEntry {
    pub id: i64,
    pub found: bool,
    pub ts: i64,
    pub summary: String,
    pub detail: Option<String>,
}

/// FTS5 `trigram` tokenizer (set in the v7 migration) indexes/matches only
/// substrings of **>= 3 chars**. Terms shorter than this are unmatchable and
/// are dropped — those queries fall back to the vector path.
const FTS_TRIGRAM_MIN: usize = 3;

/// Non-overlapping width used to slice a long CJK run into bounded substring
/// terms. Deliberately NOT a per-character sliding 3-gram sweep: a 4-char
/// chunk like `会话状态` is specific, whereas OR-ing every sliding 3-gram
/// (`会话状`,`话状态`,`状态用`,…) would recall any row sharing a common 3-gram
/// and flood RRF with noise.
const FTS_CJK_CHUNK: usize = 4;

/// CJK Unified Ideographs (incl. Ext-A) — enough to detect Chinese runs in a
/// query; non-CJK alphanumerics (latin / digits) are matched whole.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9FFF}')
}

fn push_fts_term(terms: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, raw: &str) {
    if !raw.is_empty() && seen.insert(raw.to_string()) {
        // Quote as an FTS5 string literal so query operators inside the term
        // are content, not syntax; the only in-literal escape is `"` → `""`.
        terms.push(format!("\"{}\"", raw.replace('"', "\"\"")));
    }
}

/// Turn a free-text query into an FTS5 `trigram` MATCH expression.
///
/// The index is trigram (substring match, >= 3 chars — see [`FTS_TRIGRAM_MIN`]),
/// so:
///   * non-CJK runs (English words >= 3 chars) are kept whole and substring-
///     match (e.g. `test` matches `testing`);
///   * CJK runs are sliced into NON-overlapping [`FTS_CJK_CHUNK`]-char blocks
///     (bounded segmentation, not a sliding 3-gram sweep) so each block is a
///     specific substring term;
///   * runs/blocks shorter than [`FTS_TRIGRAM_MIN`] are dropped (the vector
///     path is the backstop for 1–2 char CJK queries like `缓存`).
///
/// Blocks are split on any non-alphanumeric char (punctuation / whitespace; CJK
/// ideographs are alphanumeric, so a CJK run stays one block), then `"`-escaped,
/// quoted, de-duplicated, and OR-joined for recall (bm25 surfaces the best). A
/// query that yields no matchable term returns `None` (caller: "no FTS results").
pub(crate) fn build_fts_match(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for block in query.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = block.chars().collect();
        if chars.len() < FTS_TRIGRAM_MIN {
            continue;
        }
        if chars.iter().any(|&c| is_cjk(c)) {
            let mut i = 0;
            while i < chars.len() {
                let end = (i + FTS_CJK_CHUNK).min(chars.len());
                if end - i >= FTS_TRIGRAM_MIN {
                    let piece: String = chars[i..end].iter().collect();
                    push_fts_term(&mut terms, &mut seen, &piece);
                }
                i += FTS_CJK_CHUNK;
            }
        } else {
            push_fts_term(&mut terms, &mut seen, block);
        }
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
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
