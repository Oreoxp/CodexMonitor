//! Phase 7 S1 — agent self-scheduling ("alarm").
//!
//! An agent paces itself: when it wants to pause and resume later (wait for a
//! build, let a teammate finish, or just take a beat) it ends its turn with a
//! `<alarm delay="…">wake message</alarm>` tag (taught in the sidecar comm
//! guide). The team-router parses it (`process_final_text`) and arms a
//! per-thread pending wake in the [`AppState`] registry; one background
//! scheduler fires due wakes — but ONLY when the thread is idle (firing into an
//! active turn would `Replaced`-abort it). This makes the S1 "normal path"
//! (continue in the turn-end idle window) self-driven, with no human poke.
//!
//! The registry is in-memory: lost on restart (pending wakes simply stop; the
//! thread waits for a human poke — acceptable for now). Cross-restart
//! persistence (à la the `tasks` table hydrate) is a follow-up.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use crate::shared::codex_core::{get_session_clone, send_user_message_core};
use crate::state::AppState;

/// Hard ceiling on a single alarm delay — guards a fat-fingered `delay="100h"`.
/// 24h is well past any reasonable self-pause.
const MAX_ALARM_DELAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Scheduler poll cadence. A due-but-busy alarm fires within one tick of the
/// thread going idle, so this also bounds the "task finished → wake" latency.
const SCHEDULER_TICK: Duration = Duration::from_secs(1);

/// A pending self-wake for one thread. Registry value; keyed by `thread_id`.
#[derive(Debug, Clone)]
pub(crate) struct Alarm {
    pub(crate) workspace_id: String,
    pub(crate) fire_at: Instant,
    pub(crate) wake_message: String,
}

/// A parsed `<alarm>` tag with its delay already validated to a `Duration`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedAlarm {
    pub(crate) delay: Duration,
    pub(crate) wake_message: String,
}

/// Parse a delay string into a `Duration`.
///
/// Accepts `30s` / `10m` / `2h` and a bare integer (seconds, e.g. `90`).
/// Rejects negatives, non-integers, the empty string, and anything over
/// [`MAX_ALARM_DELAY`]. `0` is valid (continue on the next idle tick).
pub(crate) fn parse_alarm_delay(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    // `last()` returns the last *char*; the unit suffixes are all 1-byte ASCII,
    // so the `raw.len() - 1` slice always lands on a char boundary.
    let (digits, mult) = match raw.chars().last()? {
        's' => (&raw[..raw.len() - 1], 1u64),
        'm' => (&raw[..raw.len() - 1], 60),
        'h' => (&raw[..raw.len() - 1], 3600),
        c if c.is_ascii_digit() => (raw, 1), // bare seconds
        _ => return None,
    };
    // `u64::parse` rejects negatives ("-5") and any non-digit garbage for free.
    let n: u64 = digits.trim().parse().ok()?;
    let secs = n.checked_mul(mult)?;
    let delay = Duration::from_secs(secs);
    if delay > MAX_ALARM_DELAY {
        return None;
    }
    Some(delay)
}

/// Scan `<alarm delay="…">wake message</alarm>` blocks out of a turn's final
/// text. Mirrors `parse_send_message_tags`: hand-rolled, tolerant of
/// surrounding prose. Tags with a missing/invalid `delay` are skipped with a
/// warn (never aborts the turn). Returns alarms in document order; the caller
/// keeps only the last (one pending wake per thread).
pub(crate) fn parse_alarm_tags(text: &str) -> Vec<ParsedAlarm> {
    const OPEN_TAG: &str = "<alarm";
    const CLOSE_TAG: &str = "</alarm>";

    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open_at) = rest.find(OPEN_TAG) {
        let after_name = &rest[open_at + OPEN_TAG.len()..];
        // Require whitespace/`>` after `<alarm` so we don't match `<alarms>`.
        let after_first = after_name.chars().next();
        if !matches!(after_first, Some(c) if c.is_ascii_whitespace() || c == '>') {
            rest = after_name;
            continue;
        }
        let Some(open_close_rel) = after_name.find('>') else {
            break;
        };
        let attrs_str = &after_name[..open_close_rel];
        let body_start = &after_name[open_close_rel + 1..];
        let Some(close_rel) = body_start.find(CLOSE_TAG) else {
            // Unterminated tag — advance past the open token (avoid an infinite
            // loop) and stop trying to extract this one.
            rest = body_start;
            continue;
        };
        let body = &body_start[..close_rel];
        match extract_delay_attr(attrs_str).and_then(|d| parse_alarm_delay(&d)) {
            Some(delay) => out.push(ParsedAlarm {
                delay,
                wake_message: body.trim().to_string(),
            }),
            None => {
                eprintln!("[alarm] malformed <alarm> tag (missing/invalid delay); ignoring");
            }
        }
        rest = &body_start[close_rel + CLOSE_TAG.len()..];
    }
    out
}

/// Extract the double-quoted `delay="…"` attribute value from a tag's attr run.
fn extract_delay_attr(attrs: &str) -> Option<String> {
    const KEY: &str = "delay";
    let mut from = 0;
    while let Some(rel) = attrs[from..].find(KEY) {
        let abs = from + rel;
        // Boundary check: char before `delay` must be start-of-string or
        // whitespace (avoid matching `delay` inside a larger word).
        let boundary = abs == 0
            || attrs[..abs]
                .chars()
                .last()
                .map(|c| c.is_ascii_whitespace())
                .unwrap_or(false);
        if !boundary {
            from = abs + KEY.len();
            continue;
        }
        let after = attrs[abs + KEY.len()..].trim_start();
        let Some(eq) = after.strip_prefix('=') else {
            from = abs + KEY.len();
            continue;
        };
        let after_eq = eq.trim_start();
        let Some(rest) = after_eq.strip_prefix('"') else {
            from = abs + KEY.len();
            continue;
        };
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    None
}

/// Pure scheduling decision: of `alarms` (`thread_id` → `fire_at`), which
/// threads are due (`fire_at <= now`) AND idle (`!is_active`). Due-but-active
/// alarms are intentionally left for a later tick — never `turn/start` into an
/// active turn (that would `Replaced`-abort it). Isolated + sync so it
/// unit-tests without a runtime or real sessions.
pub(crate) fn alarms_to_fire(
    alarms: &[(String, Instant)],
    now: Instant,
    is_active: impl Fn(&str) -> bool,
) -> Vec<String> {
    alarms
        .iter()
        .filter(|(thread_id, fire_at)| *fire_at <= now && !is_active(thread_id.as_str()))
        .map(|(thread_id, _)| thread_id.clone())
        .collect()
}

/// Background scheduler. Spawned once at app setup; runs for the process
/// lifetime. Each tick drops orphaned alarms (thread archived / workspace
/// gone), fires due+idle alarms, and leaves due+busy ones for the next tick.
pub(crate) async fn run_alarm_scheduler(app_handle: AppHandle) {
    let mut ticker = tokio::time::interval(SCHEDULER_TICK);
    loop {
        ticker.tick().await;
        run_alarm_tick(&app_handle).await;
    }
}

/// Per-thread liveness as seen by the scheduler.
enum ThreadProbe {
    Idle,
    Active,
    Orphaned,
}

async fn probe_thread(app_handle: &AppHandle, workspace_id: &str, thread_id: &str) -> ThreadProbe {
    let state = app_handle.state::<AppState>();
    match get_session_clone(&state.sessions, workspace_id).await {
        Ok(session) => {
            if session
                .routing
                .resolve_workspace_for_thread(thread_id)
                .await
                .is_none()
            {
                // Mapping forgotten on `thread/archived` → no live thread to wake.
                ThreadProbe::Orphaned
            } else if session.routing.is_active(thread_id).await {
                ThreadProbe::Active
            } else {
                ThreadProbe::Idle
            }
        }
        // Workspace session gone entirely.
        Err(_) => ThreadProbe::Orphaned,
    }
}

async fn run_alarm_tick(app_handle: &AppHandle) {
    // Snapshot under the (sync) lock; never hold it across an await.
    let snapshot: Vec<(String, String, Instant)> = {
        let state = app_handle.state::<AppState>();
        let reg = state.alarms.lock().expect("alarms mutex poisoned");
        if reg.is_empty() {
            return;
        }
        reg.iter()
            .map(|(tid, a)| (tid.clone(), a.workspace_id.clone(), a.fire_at))
            .collect()
    };

    let now = Instant::now();
    let mut active: HashSet<String> = HashSet::new();
    let mut orphaned: HashSet<String> = HashSet::new();
    for (thread_id, workspace_id, fire_at) in &snapshot {
        if *fire_at > now {
            continue; // not due yet — don't probe sessions
        }
        match probe_thread(app_handle, workspace_id, thread_id).await {
            ThreadProbe::Orphaned => {
                orphaned.insert(thread_id.clone());
            }
            ThreadProbe::Active => {
                active.insert(thread_id.clone());
            }
            ThreadProbe::Idle => {}
        }
    }

    // Drop orphaned (archived / gone) alarms so they never wake a dead thread.
    if !orphaned.is_empty() {
        let state = app_handle.state::<AppState>();
        let mut reg = state.alarms.lock().expect("alarms mutex poisoned");
        for thread_id in &orphaned {
            reg.remove(thread_id);
        }
    }

    let decision_input: Vec<(String, Instant)> = snapshot
        .iter()
        .filter(|(tid, _, _)| !orphaned.contains(tid))
        .map(|(tid, _, fire_at)| (tid.clone(), *fire_at))
        .collect();
    let to_fire = alarms_to_fire(&decision_input, now, |tid| active.contains(tid));

    for thread_id in to_fire {
        // Take the alarm out before dispatch (idle → fire → remove). A dispatch
        // failure (thread vanished mid-tick) simply drops it.
        let alarm = {
            let state = app_handle.state::<AppState>();
            let mut reg = state.alarms.lock().expect("alarms mutex poisoned");
            reg.remove(&thread_id)
        };
        let Some(alarm) = alarm else {
            continue;
        };

        let state = app_handle.state::<AppState>();
        let body = format!("（你设定的闹钟触发）\n{}", alarm.wake_message);
        // `full-access` → `approvalPolicy: never` so the unattended wake turn
        // never wedges on an approval prompt. `send_user_message_core` runs
        // write① (set_turn_active), so the wake turn is itself interruptible.
        if let Err(err) = send_user_message_core(
            &state.sessions,
            &state.workspaces,
            alarm.workspace_id,
            thread_id.clone(),
            body,
            None,
            None,
            None,
            Some("full-access".to_string()),
            None,
            None,
            None,
        )
        .await
        {
            eprintln!("[alarm] wake dispatch for thread {thread_id} failed: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parse_delay_suffixes_and_bare_seconds() {
        assert_eq!(parse_alarm_delay("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_alarm_delay("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_alarm_delay("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_alarm_delay("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_alarm_delay("  5m "), Some(Duration::from_secs(300)));
    }

    #[test]
    fn parse_delay_zero_is_valid() {
        assert_eq!(parse_alarm_delay("0"), Some(Duration::from_secs(0)));
        assert_eq!(parse_alarm_delay("0s"), Some(Duration::from_secs(0)));
    }

    #[test]
    fn parse_delay_rejects_negative_garbage_and_over_cap() {
        assert_eq!(parse_alarm_delay("-5"), None);
        assert_eq!(parse_alarm_delay("abc"), None);
        assert_eq!(parse_alarm_delay("1d"), None); // no day unit
        assert_eq!(parse_alarm_delay(""), None);
        assert_eq!(parse_alarm_delay("m"), None); // no number
        assert_eq!(parse_alarm_delay("25h"), None); // over the 24h cap
        assert_eq!(parse_alarm_delay("100000"), None); // > 24h in bare seconds
    }

    #[test]
    fn parse_tags_extracts_delay_and_message() {
        let alarms = parse_alarm_tags("blah <alarm delay=\"10m\">resume the build</alarm> done");
        assert_eq!(
            alarms,
            vec![ParsedAlarm {
                delay: Duration::from_secs(600),
                wake_message: "resume the build".to_string(),
            }]
        );
    }

    #[test]
    fn parse_tags_skips_invalid_delay_keeps_valid() {
        let alarms = parse_alarm_tags(
            "<alarm delay=\"-1\">bad</alarm><alarm delay=\"5s\">good</alarm>",
        );
        assert_eq!(
            alarms,
            vec![ParsedAlarm {
                delay: Duration::from_secs(5),
                wake_message: "good".to_string(),
            }]
        );
    }

    #[test]
    fn parse_tags_replacement_last_wins() {
        // Two alarms in one turn → both parsed in order; the registry keeps the
        // last (the caller inserts each under the same thread key).
        let alarms =
            parse_alarm_tags("<alarm delay=\"1m\">first</alarm> <alarm delay=\"2m\">second</alarm>");
        assert_eq!(alarms.len(), 2);
        let mut reg: HashMap<String, ParsedAlarm> = HashMap::new();
        for a in alarms {
            reg.insert("thread-1".to_string(), a);
        }
        assert_eq!(reg["thread-1"].wake_message, "second");
        assert_eq!(reg["thread-1"].delay, Duration::from_secs(120));
    }

    #[test]
    fn alarms_to_fire_due_idle_selected_busy_skipped_future_left() {
        let now = Instant::now();
        let past = now - Duration::from_secs(5);
        let future = now + Duration::from_secs(60);
        let alarms = vec![
            ("idle-due".to_string(), past),
            ("busy-due".to_string(), past),
            ("idle-future".to_string(), future),
        ];
        let active: HashSet<String> = HashSet::from(["busy-due".to_string()]);

        let fired = alarms_to_fire(&alarms, now, |tid| active.contains(tid));

        // due + idle fires; due + active is left for a later tick; not-due is left.
        assert_eq!(fired, vec!["idle-due".to_string()]);
    }
}
