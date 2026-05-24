// Phase 5 Step 3 Block A — compaction-flush sensing.
//
// Block A is the SENSE half of the pre-compaction memory flush: it watches a
// team agent thread's cumulative token usage and, when usage approaches the
// point where codex would auto-compact the thread, emits a one-shot signal.
// Block A only senses — the team-router consumer logs the signal. Block B
// turns the signal into a real flush turn.
//
// Why 90% of the model context window is the ceiling: codex auto-compacts a
// thread once cumulative usage reaches its `auto_compact_token_limit`. Per
// codex's own `ModelInfo::auto_compact_token_limit()`
// (`codex-rs/protocol/src/openai_models.rs`), that limit is 90% of the
// resolved context window when `model_auto_compact_token_limit` is unset (the
// default), and any configured value is clamped to ≤90% of the window. So
// `0.9 * model_context_window` — and `model_context_window` rides every
// `thread/tokenUsage/updated` notification — is the auto-compact ceiling.
// Phase 5 Step 3 Block B verified OpenCrab never writes
// `model_auto_compact_token_limit` (no config write, no template anywhere),
// so the `unset` branch always holds and the 90% figure is EXACT. Were
// OpenCrab to start setting that config, this derivation must change —
// over-estimating the ceiling would push the flush past codex's real
// auto-compact point. No codex config or model-registry lookup is needed.

use serde_json::Value;

/// Auto-compact ceiling as a fraction of the model context window — codex's
/// own `(context_window * 9) / 10` from `openai_models.rs`.
const CEILING_NUMERATOR: i64 = 9;
const CEILING_DENOMINATOR: i64 = 10;

/// Conservative token budget for the pre-compaction flush turn itself — the
/// flush prompt plus the agent's flush response (Block B). The soft threshold
/// sits this far below the ceiling so the flush turn has room to complete
/// before codex's own auto-compaction fires. Deliberately generous; Block B
/// recalibrates this against measured flush turns.
pub(crate) const FLUSH_TURN_TOKEN_BUDGET: i64 = 8_000;

/// Extra headroom below the ceiling for estimation jitter: `tokenUsage`
/// notifications lag the true count, and the turn that pushed usage over the
/// line may still be mid-flight when the watcher fires.
pub(crate) const FLUSH_SAFETY_MARGIN_TOKENS: i64 = 4_000;

/// The auto-compact ceiling for a given model context window.
pub(crate) fn auto_compact_ceiling(model_context_window: i64) -> i64 {
    model_context_window * CEILING_NUMERATOR / CEILING_DENOMINATOR
}

/// The soft threshold: once cumulative usage reaches this, the flush turn must
/// start so it can finish before codex auto-compacts. `ceiling − flush turn
/// budget − safety margin`, floored at 0 (a context window small enough to
/// drive this negative cannot host a flush turn anyway).
pub(crate) fn flush_soft_threshold(model_context_window: i64) -> i64 {
    (auto_compact_ceiling(model_context_window)
        - FLUSH_TURN_TOKEN_BUDGET
        - FLUSH_SAFETY_MARGIN_TOKENS)
        .max(0)
}

/// One-shot signal emitted the first time a thread's usage crosses the soft
/// threshold. Block A logs it; Block B will act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlushSignal {
    /// Cumulative tokens used on the thread at the crossing observation.
    pub(crate) total_tokens: i64,
    /// The model context window reported alongside the usage.
    pub(crate) model_context_window: i64,
    /// The derived auto-compact ceiling (`0.9 * model_context_window`).
    pub(crate) ceiling: i64,
    /// The soft threshold that was crossed.
    pub(crate) soft_threshold: i64,
}

/// Per-thread token-usage watcher. One instance per thread — the team router's
/// consumer task is already per-thread, so this lives as a task-local. It
/// fires exactly once; Block B will add a re-arm after the post-flush
/// compaction so a long-lived thread can be flushed again.
#[derive(Debug, Default)]
pub(crate) struct TokenWatcher {
    triggered: bool,
}

impl TokenWatcher {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reset the watcher so the next threshold crossing fires again. Called
    /// by the team router after `thread/compacted` — once codex has compacted
    /// (our triggered compaction following a flush, or codex's own auto-
    /// compact when the flush did not make it), usage drops and we want to be
    /// able to flush again on the next approach.
    pub(crate) fn rearm(&mut self) {
        self.triggered = false;
    }

    /// Feed one token-usage observation — cumulative `total_tokens` plus the
    /// reported `model_context_window`. Returns `Some(FlushSignal)` exactly
    /// once: on the first observation at or over the soft threshold. Returns
    /// `None` while under threshold, when the context window is unknown or
    /// non-positive, or after the watcher has already fired.
    pub(crate) fn observe(
        &mut self,
        total_tokens: i64,
        model_context_window: Option<i64>,
    ) -> Option<FlushSignal> {
        if self.triggered {
            return None;
        }
        let model_context_window = model_context_window?;
        if model_context_window <= 0 {
            return None;
        }
        let soft_threshold = flush_soft_threshold(model_context_window);
        if total_tokens < soft_threshold {
            return None;
        }
        self.triggered = true;
        Some(FlushSignal {
            total_tokens,
            model_context_window,
            ceiling: auto_compact_ceiling(model_context_window),
            soft_threshold,
        })
    }
}

/// Extract `(total_tokens, model_context_window)` from a
/// `thread/tokenUsage/updated` notification: cumulative usage from
/// `params.tokenUsage.total.totalTokens`, window from
/// `params.tokenUsage.modelContextWindow` (absent / null → `None`). Returns
/// `None` when the payload is not a well-formed token-usage notification.
pub(crate) fn parse_token_usage(event: &Value) -> Option<(i64, Option<i64>)> {
    let token_usage = event.get("params")?.get("tokenUsage")?;
    let total_tokens = token_usage.get("total")?.get("totalTokens")?.as_i64()?;
    let model_context_window = token_usage
        .get("modelContextWindow")
        .and_then(Value::as_i64);
    Some((total_tokens, model_context_window))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ceiling_is_ninety_percent_of_context_window() {
        assert_eq!(auto_compact_ceiling(200_000), 180_000);
        assert_eq!(auto_compact_ceiling(100_000), 90_000);
        assert_eq!(auto_compact_ceiling(0), 0);
    }

    #[test]
    fn soft_threshold_is_ceiling_minus_budget_and_margin() {
        // 200k window → ceiling 180k → soft = 180k − 8k − 4k = 168k.
        assert_eq!(flush_soft_threshold(200_000), 168_000);
        assert_eq!(
            flush_soft_threshold(200_000),
            auto_compact_ceiling(200_000) - FLUSH_TURN_TOKEN_BUDGET - FLUSH_SAFETY_MARGIN_TOKENS,
        );
    }

    #[test]
    fn soft_threshold_floors_at_zero_for_tiny_windows() {
        // A context window too small to host a flush turn → 0, never negative.
        assert_eq!(flush_soft_threshold(1_000), 0);
    }

    #[test]
    fn parse_token_usage_reads_cumulative_total_and_window() {
        let event = json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": "thr-1",
                "turnId": "turn-7",
                "tokenUsage": {
                    "total": {
                        "totalTokens": 170_000,
                        "inputTokens": 160_000,
                        "cachedInputTokens": 0,
                        "outputTokens": 8_000,
                        "reasoningOutputTokens": 2_000
                    },
                    "last": {
                        "totalTokens": 1_200,
                        "inputTokens": 1_000,
                        "cachedInputTokens": 0,
                        "outputTokens": 150,
                        "reasoningOutputTokens": 50
                    },
                    "modelContextWindow": 200_000
                }
            }
        });
        assert_eq!(parse_token_usage(&event), Some((170_000, Some(200_000))));
    }

    #[test]
    fn parse_token_usage_handles_absent_context_window() {
        let event = json!({
            "params": { "tokenUsage": {
                "total": { "totalTokens": 5 },
                "last": { "totalTokens": 5 },
                "modelContextWindow": Value::Null
            }}
        });
        assert_eq!(parse_token_usage(&event), Some((5, None)));
    }

    #[test]
    fn parse_token_usage_rejects_non_token_usage_payloads() {
        assert_eq!(
            parse_token_usage(&json!({ "method": "turn/completed", "params": {} })),
            None,
        );
        assert_eq!(parse_token_usage(&json!({ "params": { "tokenUsage": {} } })), None);
        assert_eq!(parse_token_usage(&json!({})), None);
    }

    #[test]
    fn watcher_does_not_fire_below_the_soft_threshold() {
        let mut watcher = TokenWatcher::new();
        // 200k window → soft 168k; 100k usage is well under.
        assert_eq!(watcher.observe(100_000, Some(200_000)), None);
        // Right under the line still does not fire.
        assert_eq!(watcher.observe(167_999, Some(200_000)), None);
    }

    #[test]
    fn watcher_fires_once_when_the_threshold_is_crossed() {
        let mut watcher = TokenWatcher::new();
        assert_eq!(watcher.observe(167_999, Some(200_000)), None);
        // Reaching the soft threshold fires the one-shot signal.
        let signal = watcher
            .observe(168_000, Some(200_000))
            .expect("crossing the soft threshold fires");
        assert_eq!(
            signal,
            FlushSignal {
                total_tokens: 168_000,
                model_context_window: 200_000,
                ceiling: 180_000,
                soft_threshold: 168_000,
            },
        );
        // Already fired — later observations do not re-fire.
        assert_eq!(watcher.observe(175_000, Some(200_000)), None);
        assert_eq!(watcher.observe(179_000, Some(200_000)), None);
    }

    #[test]
    fn watcher_ignores_unknown_or_invalid_context_window() {
        // An unknown window cannot yield a ceiling — never fires.
        let mut unknown = TokenWatcher::new();
        assert_eq!(unknown.observe(10_000_000, None), None);
        // A non-positive window is treated the same way.
        let mut zero = TokenWatcher::new();
        assert_eq!(zero.observe(10_000_000, Some(0)), None);
    }

    #[test]
    fn watcher_rearm_lets_a_subsequent_crossing_fire_again() {
        let mut watcher = TokenWatcher::new();
        // First crossing fires.
        assert!(watcher.observe(170_000, Some(200_000)).is_some());
        // While not re-armed, subsequent crossings do nothing.
        assert!(watcher.observe(175_000, Some(200_000)).is_none());
        // Re-arm (Block C: called after `thread/compacted`).
        watcher.rearm();
        // Usage may have dropped after compaction; the next crossing fires.
        assert!(watcher.observe(168_000, Some(200_000)).is_some());
    }

    #[test]
    fn watchers_are_independent_per_thread() {
        // Each thread owns its watcher; one firing must not affect another.
        let mut thread_a = TokenWatcher::new();
        let mut thread_b = TokenWatcher::new();
        assert!(thread_a.observe(170_000, Some(200_000)).is_some()); // A crosses
        assert_eq!(thread_a.observe(175_000, Some(200_000)), None); // A already fired
        // B, with the same window but low usage, has NOT fired.
        assert_eq!(thread_b.observe(50_000, Some(200_000)), None);
        assert!(thread_b.observe(170_000, Some(200_000)).is_some()); // B fires on its own crossing
    }
}
