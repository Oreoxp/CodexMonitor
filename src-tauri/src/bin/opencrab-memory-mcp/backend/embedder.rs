use super::BackendError;
use rusqlite::{params, Connection};

// ---------------------------------------------------------------------------
// Phase 6 Step 4-embed — embedding client + embed_pending_once
// ---------------------------------------------------------------------------

/// Default embedding dimensionality when the config file / env vars
/// don't say otherwise. Must match the `log_vec` table's declared
/// dimension (v6 migration: `float[1024]`).
pub const DEFAULT_EMBED_DIMENSIONS: usize = 1024;

/// SiliconFlow's documented batch cap. We never embed more than this
/// many texts per HTTP request.
pub const EMBED_BATCH_SIZE: usize = 32;

/// OpenAI-compatible `/embeddings` HTTP client. Shares `base_url` +
/// `api_key` with the distiller's `HttpExtractor`; `model` and
/// `dimensions` are embedding-specific.
pub struct HttpEmbedder {
    pub(crate) client: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
    pub(crate) dimensions: usize,
}

impl HttpEmbedder {
    /// Env path: shared `OPENCRAB_DISTILLER_BASE_URL` + `OPENCRAB_DISTILLER_API_KEY`
    /// + embed-specific `OPENCRAB_EMBED_MODEL` + optional `OPENCRAB_EMBED_DIMENSIONS`
    /// (defaults to [`DEFAULT_EMBED_DIMENSIONS`]). Returns `None` if any
    /// required var is missing or empty.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("OPENCRAB_DISTILLER_BASE_URL").ok()?;
        let api_key = std::env::var("OPENCRAB_DISTILLER_API_KEY").ok()?;
        let model = std::env::var("OPENCRAB_EMBED_MODEL").ok()?;
        let dimensions = std::env::var("OPENCRAB_EMBED_DIMENSIONS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_EMBED_DIMENSIONS);
        if base_url.trim().is_empty()
            || api_key.trim().is_empty()
            || model.trim().is_empty()
        {
            return None;
        }
        Some(Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
            model,
            dimensions,
        })
    }

    /// Env first; otherwise read `<user_root>/distiller.json` and pick
    /// up `embed_model` (required) + `embed_dimensions` (default 1024).
    /// `base_url` + `api_key` are shared with the distiller config
    /// already in that same file.
    pub fn load() -> Option<Self> {
        if let Some(emb) = Self::from_env() {
            return Some(emb);
        }
        Self::from_file()
    }

    fn from_file() -> Option<Self> {
        let root = crate::paths::user_root()?;
        Self::from_file_at(&root.join("distiller.json"))
    }

    fn from_file_at(path: &std::path::Path) -> Option<Self> {
        let body = std::fs::read_to_string(path).ok()?;
        let json: serde_json::Value = serde_json::from_str(&body).ok()?;
        let base_url = json
            .get("base_url")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let api_key = json
            .get("api_key")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let model = json
            .get("embed_model")
            .and_then(|v| v.as_str())?
            .trim()
            .to_string();
        let dimensions = json
            .get("embed_dimensions")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_EMBED_DIMENSIONS);
        if base_url.is_empty() || api_key.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
            model,
            dimensions,
        })
    }

    /// Final POST URL. Like the distiller, `base_url` must already
    /// include the provider's `/v1` segment — we only append `/embeddings`.
    pub(crate) fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    pub(crate) fn build_request_body(&self, texts: &[String]) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "input": texts,
            "dimensions": self.dimensions,
        })
    }
}

#[async_trait::async_trait]
impl Embedder for HttpEmbedder {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = self.embeddings_url();
        let body = self.build_request_body(texts);
        // Hard reqwest-agnostic wall around the whole HTTP op — same rationale
        // and timer dependency as `post_chat` (reqwest's own timeout is not a
        // guarantee through the proxy).
        let op = async {
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| EmbedError::HttpTransient(format!("send {url}: {e}")))?;

            let status = resp.status();
            if !status.is_success() {
                let body_text = resp.text().await.unwrap_or_default();
                let is_transient = status.is_server_error()
                    || status.as_u16() == 408
                    || status.as_u16() == 429;
                return if is_transient {
                    Err(EmbedError::HttpTransient(format!(
                        "HTTP {status}: {body_text}"
                    )))
                } else {
                    Err(EmbedError::HttpClient(format!(
                        "HTTP {status}: {body_text}"
                    )))
                };
            }

            let resp_body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| EmbedError::Parse(format!("decode embed response body: {e}")))?;

            parse_embeddings_response(&resp_body, texts.len(), self.dimensions)
        };
        match tokio::time::timeout(LLM_HTTP_TIMEOUT, op).await {
            Ok(r) => r,
            Err(_elapsed) => Err(EmbedError::HttpTransient(format!(
                "embed call timed out after {}s (hard wall)",
                LLM_HTTP_TIMEOUT.as_secs()
            ))),
        }
    }
}

/// Pure: pull the per-input vectors out of a `/v1/embeddings` reply,
/// re-order them by `data[i].index`, and verify each vector has
/// length == `expected_dim`. Split out so unit tests can hammer it
/// without going through HTTP.
pub(crate) fn parse_embeddings_response(
    body: &serde_json::Value,
    expected_count: usize,
    expected_dim: usize,
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let arr = body
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| EmbedError::Parse(format!("missing data[] in: {body}")))?;
    if arr.len() != expected_count {
        return Err(EmbedError::Parse(format!(
            "data length {} != expected_count {}",
            arr.len(),
            expected_count
        )));
    }

    // Pull (index, embedding) out of each element; the API doesn't
    // guarantee `data[]` is sorted by index, so we sort ourselves.
    let mut pairs: Vec<(usize, Vec<f32>)> = Vec::with_capacity(arr.len());
    for item in arr {
        let idx = item
            .get("index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| EmbedError::Parse(format!("missing index in: {item}")))?
            as usize;
        let emb_arr = item
            .get("embedding")
            .and_then(|v| v.as_array())
            .ok_or_else(|| EmbedError::Parse(format!("missing embedding in: {item}")))?;
        if emb_arr.len() != expected_dim {
            return Err(EmbedError::Parse(format!(
                "vector length {} != expected_dim {} (index={idx})",
                emb_arr.len(),
                expected_dim
            )));
        }
        let v: Vec<f32> = emb_arr
            .iter()
            .map(|n| {
                n.as_f64()
                    .map(|x| x as f32)
                    .ok_or_else(|| {
                        EmbedError::Parse(format!("non-numeric embedding element: {n}"))
                    })
            })
            .collect::<Result<Vec<f32>, EmbedError>>()?;
        pairs.push((idx, v));
    }
    pairs.sort_by_key(|(i, _)| *i);
    // After sort + length match, each index 0..N must appear exactly once.
    for (i, (idx, _)) in pairs.iter().enumerate() {
        if *idx != i {
            return Err(EmbedError::Parse(format!(
                "non-contiguous indices: position {i} has data.index = {idx}"
            )));
        }
    }
    Ok(pairs.into_iter().map(|(_, v)| v).collect())
}

/// Serialise a `[f32]` to the JSON-array form vec0's MATCH expects.
/// Pure helper; also used by tests.
pub(crate) fn vec_to_match_json(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

/// Embedding-input text for one log row. The S4-R recon recommended
/// `summary + "\n\n" + detail`: the headline plus body together gives
/// the embedder maximum signal, and rows with `detail IS NULL` still
/// produce a usable text (just `summary` + empty trailer).
pub(crate) fn build_embed_text(summary: &str, detail: Option<&str>) -> String {
    format!("{}\n\n{}", summary, detail.unwrap_or(""))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmbedStats {
    pub rows_pending_seen: u64,
    pub rows_embedded: u64,
    pub batches: u64,
    pub errors: u64,
}

/// One embedding pass.
///
/// Loops over batches of ≤ [`EMBED_BATCH_SIZE`] `log` rows that lack a
/// matching `log_vec` row, embeds each batch through `embedder`, and
/// writes the result into `log_vec(rowid = log.id)` inside one IMMEDIATE
/// transaction per batch. On the first embedder error this pass stops
/// (rows stay pending for the next pass); no partial writes.
///
/// We track an in-pass `min_id` watermark so empty-content rows (which
/// can't happen via `log_progress` / distiller writes today but might
/// arrive via direct INSERT in some future path) don't cause an
/// infinite loop — every batch advances the watermark even if all its
/// rows got skipped.
pub async fn embed_pending_once(
    conn: &Connection,
    embedder: &dyn Embedder,
) -> Result<EmbedStats, BackendError> {
    let mut stats = EmbedStats::default();
    let mut min_id: i64 = 0;

    loop {
        let batch = fetch_pending_batch_after(conn, min_id, EMBED_BATCH_SIZE)?;
        if batch.is_empty() {
            return Ok(stats);
        }
        let new_max = batch.last().expect("non-empty").0;
        stats.rows_pending_seen += batch.len() as u64;

        // Partition: rows with usable text vs rows we silently skip.
        let mut ids: Vec<i64> = Vec::with_capacity(batch.len());
        let mut texts: Vec<String> = Vec::with_capacity(batch.len());
        for (id, summary, detail) in &batch {
            let text = build_embed_text(summary, detail.as_deref());
            if text.trim().is_empty() {
                continue;
            }
            ids.push(*id);
            texts.push(text);
        }

        if !texts.is_empty() {
            let vectors = match embedder.embed(&texts).await {
                Ok(v) => v,
                Err(err) => {
                    eprintln!(
                        "[embed] batch (rows {}..={}) failed: {err} — stopping pass, rows stay pending",
                        ids.first().copied().unwrap_or(0),
                        ids.last().copied().unwrap_or(0)
                    );
                    stats.errors += 1;
                    return Ok(stats);
                }
            };
            if vectors.len() != texts.len() {
                eprintln!(
                    "[embed] embedder returned {} vectors for {} texts — stopping pass",
                    vectors.len(),
                    texts.len()
                );
                stats.errors += 1;
                return Ok(stats);
            }
            write_embedding_batch(conn, &ids, &vectors)?;
            stats.rows_embedded += vectors.len() as u64;
            stats.batches += 1;
        }

        min_id = new_max;
    }
}

fn fetch_pending_batch_after(
    conn: &Connection,
    after_id: i64,
    limit: usize,
) -> Result<Vec<(i64, String, Option<String>)>, BackendError> {
    let mut stmt = conn.prepare(
        "SELECT id, summary, detail FROM log \
         WHERE id NOT IN (SELECT rowid FROM log_vec) AND id > ?1 \
         ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after_id, limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn write_embedding_batch(
    conn: &Connection,
    ids: &[i64],
    vectors: &[Vec<f32>],
) -> Result<(), BackendError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let inner = (|| -> Result<(), BackendError> {
        for (id, v) in ids.iter().zip(vectors.iter()) {
            let json = vec_to_match_json(v);
            conn.execute(
                "INSERT INTO log_vec(rowid, embedding) VALUES (?1, ?2)",
                params![id, json],
            )?;
        }
        Ok(())
    })();
    match inner {
        Ok(_) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}


/// Errors from one `Embedder::embed` call. Same retry semantics as
/// `ExtractError`: only `HttpTransient` is retryable.
#[derive(Debug, Clone)]
pub enum EmbedError {
    /// Configuration is missing or invalid (env / file). Not retryable.
    Config(String),
    /// Network failure or 5xx / 408 / 429 — almost always transient. **Retryable.**
    HttpTransient(String),
    /// 4xx other than 408/429 — same input will fail again. Not retryable.
    HttpClient(String),
    /// 2xx but the body was malformed, or a returned embedding had the
    /// wrong dimensionality. Not retryable from our side.
    Parse(String),
}

impl EmbedError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, EmbedError::HttpTransient(_))
    }
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Config(m) => write!(f, "embed config error: {m}"),
            EmbedError::HttpTransient(m) => write!(f, "embed http transient: {m}"),
            EmbedError::HttpClient(m) => write!(f, "embed http client error: {m}"),
            EmbedError::Parse(m) => write!(f, "embed parse error: {m}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Embedding contract. Returns one vector per input text, in the same
/// order. Vectors must all be the same length and match `dimensions()`.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    fn dimensions(&self) -> usize;
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Hard upper bound on ANY single LLM HTTP call — distill, judge, AND embed.
/// The AUTHORITATIVE wall is `tokio::time::timeout` at the call sites
/// (`post_chat`, `HttpEmbedder::embed`): reqwest's own client timeout did NOT
/// abort a hung proxied call in practice (Layer D hung 11 min ≫ 120s on a
/// distill call). Same value is mirrored onto the reqwest client as
/// defense-in-depth. BOTH are timer-driven → the runtime needs `enable_all` /
/// `enable_time`, or neither fires.
pub(crate) const LLM_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
