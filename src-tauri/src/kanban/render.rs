// Phase 4 Step 7 — pure render function (Tasks → Markdown).
//
// Format-locked. Editing this file changes what every agent sees in
// their stable system-prompt prefix, which invalidates the cached
// prefix everywhere. Treat changes here like a wire-protocol change.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::tasks::{Task, TaskStatus};

const EMPTY_PLACEHOLDER: &str = "_(empty)_";

/// Render the markdown body of one agent's KANBAN.md.
///
/// `tasks` is the **already-filtered** slice — caller passes only tasks
/// whose `assignee_agent_id == agent_id` and whose status is not
/// `archived`. This function does no further filtering; it just
/// groups + renders.
///
/// `now` is the timestamp stamped into the `Last update:` comment.
/// Caller-injected so tests can pin a stable value.
///
/// Pure function: no I/O, no global state, output depends only on
/// inputs. Output ends with a single trailing newline.
pub(crate) fn render_kanban_markdown(
    agent_display_name: &str,
    tasks: &[Task],
    now: DateTime<Utc>,
) -> String {
    let (proposed, in_progress, done) = group_tasks(tasks);

    let mut out = String::new();
    out.push_str(&format!("# Kanban — {agent_display_name}\n\n"));
    out.push_str("<!-- Auto-generated from tasks table. Do not edit. -->\n");
    out.push_str(&format!(
        "<!-- Last update: {ts} -->\n\n",
        ts = now.to_rfc3339_opts(SecondsFormat::Millis, true)
    ));

    render_section(&mut out, "Proposed", &proposed, |t| {
        format!("- [{id}] {title}\n", id = t.id, title = t.title)
    });
    out.push('\n');
    render_section(&mut out, "In Progress", &in_progress, |t| {
        format!(
            "- [{id}] {title} {status}\n",
            id = t.id,
            title = t.title,
            status = in_progress_status_suffix(t),
        )
    });
    out.push('\n');
    render_section(&mut out, "Done", &done, |t| {
        format!("- [{id}] {title}\n", id = t.id, title = t.title)
    });

    out
}

/// Render `## Heading\n\n<rows or placeholder>\n`. Caller is responsible
/// for the inter-section newline (so the final section doesn't get a
/// trailing blank line).
fn render_section<F>(out: &mut String, heading: &str, rows: &[&Task], render_row: F)
where
    F: Fn(&Task) -> String,
{
    out.push_str(&format!("## {heading}\n\n"));
    if rows.is_empty() {
        out.push_str(EMPTY_PLACEHOLDER);
        out.push('\n');
    } else {
        for task in rows {
            out.push_str(&render_row(task));
        }
    }
}

/// Stable-by-created_at grouping. created_at is RFC3339; lexicographic
/// sort matches chronological order at second resolution (the precision
/// the tasks table stores).
fn group_tasks(tasks: &[Task]) -> (Vec<&Task>, Vec<&Task>, Vec<&Task>) {
    let mut proposed: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Proposed)
        .collect();
    let mut in_progress: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Ready | TaskStatus::Running | TaskStatus::Blocked
            )
        })
        .collect();
    let mut done: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .collect();

    let sort_by_created_at = |a: &&Task, b: &&Task| a.created_at.cmp(&b.created_at);
    proposed.sort_by(sort_by_created_at);
    in_progress.sort_by(sort_by_created_at);
    done.sort_by(sort_by_created_at);
    (proposed, in_progress, done)
}

/// `(ready)` / `(running)` / `(blocked)` or `(blocked: <reason>)`.
/// Caller has already filtered to the in-progress set.
fn in_progress_status_suffix(task: &Task) -> String {
    match task.status {
        TaskStatus::Ready => "(ready)".to_string(),
        TaskStatus::Running => "(running)".to_string(),
        TaskStatus::Blocked => match task.feedback.as_deref() {
            Some(reason) if !reason.trim().is_empty() => format!("(blocked: {reason})"),
            _ => "(blocked)".to_string(),
        },
        // Caller-side filter guarantees we never see Proposed/Done/Archived
        // here. Defensive fallback: don't suppress, surface the bug.
        other => format!("({other})"),
    }
}
