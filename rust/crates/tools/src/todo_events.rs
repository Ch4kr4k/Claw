//! Typed events for the `TodoWrite` checklist. `TodoWrite` replaces the
//! whole list on every call; comparing the old list with the new one gives
//! what happened to each task (`task.created`, `task.started`,
//! `task.completed`, `task.reopened`, `task.removed`). The events are
//! returned with the tool result and appended as JSON lines next to the
//! todo store (`.clawd-todos.events.jsonl`), so orchestrators and status
//! tools can follow a session's progress without parsing the transcript.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{TodoItem, TodoStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum TodoEventKind {
    #[serde(rename = "task.created")]
    Created,
    #[serde(rename = "task.started")]
    Started,
    #[serde(rename = "task.completed")]
    Completed,
    /// A completed or in-progress task set back to pending.
    #[serde(rename = "task.reopened")]
    Reopened,
    #[serde(rename = "task.removed")]
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TodoEvent {
    pub event: TodoEventKind,
    /// The task's `content`, which identifies it across calls.
    pub task: String,
    pub status: TodoStatus,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
}

/// What changed from `old` to `new`, in the order of `new` (then removals).
/// A task is matched by its content; one that is new and already completed
/// gives `task.created` then `task.completed`.
pub(crate) fn diff(old: &[TodoItem], new: &[TodoItem], at_ms: u64) -> Vec<TodoEvent> {
    let event = |event, item: &TodoItem| TodoEvent {
        event,
        task: item.content.clone(),
        status: item.status.clone(),
        at_ms,
    };
    let mut events = Vec::new();
    for item in new {
        let before = old.iter().find(|previous| previous.content == item.content);
        let previous_status = before.map(|previous| &previous.status);
        if before.is_none() {
            events.push(event(TodoEventKind::Created, item));
        }
        match (previous_status, &item.status) {
            (Some(TodoStatus::InProgress), TodoStatus::InProgress)
            | (Some(TodoStatus::Completed), TodoStatus::Completed)
            | (Some(TodoStatus::Pending) | None, TodoStatus::Pending) => {}
            (_, TodoStatus::InProgress) => events.push(event(TodoEventKind::Started, item)),
            (_, TodoStatus::Completed) => events.push(event(TodoEventKind::Completed, item)),
            (Some(_), TodoStatus::Pending) => events.push(event(TodoEventKind::Reopened, item)),
        }
    }
    for item in old {
        if !new.iter().any(|current| current.content == item.content) {
            events.push(event(TodoEventKind::Removed, item));
        }
    }
    events
}

/// The events log beside the todo store: `x.json` → `x.events.jsonl`.
pub(crate) fn log_path(store: &Path) -> PathBuf {
    let stem = store.file_stem().map_or_else(
        || "todos".into(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    store.with_file_name(format!("{stem}.events.jsonl"))
}

/// Appends `events` to the log; failures are ignored (the checklist itself
/// was saved, and the log is only for observers).
pub(crate) fn append(store: &Path, events: &[TodoEvent]) {
    if events.is_empty() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(store))
    else {
        return;
    };
    let mut lines = String::new();
    for event in events {
        if let Ok(json) = serde_json::to_string(event) {
            lines.push_str(&json);
            lines.push('\n');
        }
    }
    let _ = file.write_all(lines.as_bytes());
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            active_form: format!("Doing {content}"),
            status,
        }
    }

    fn kinds(events: &[TodoEvent]) -> Vec<(TodoEventKind, &str)> {
        events.iter().map(|e| (e.event, e.task.as_str())).collect()
    }

    #[test]
    fn diff_reports_each_transition_once() {
        use TodoEventKind::{Completed, Created, Removed, Reopened, Started};
        let first = vec![
            item("a", TodoStatus::Pending),
            item("b", TodoStatus::InProgress),
        ];
        assert_eq!(
            kinds(&diff(&[], &first, 1)),
            [(Created, "a"), (Created, "b"), (Started, "b")]
        );
        // Unchanged list: nothing happened.
        assert!(diff(&first, &first, 2).is_empty());

        let second = vec![
            item("a", TodoStatus::InProgress),
            item("b", TodoStatus::Completed),
            item("c", TodoStatus::Completed),
        ];
        assert_eq!(
            kinds(&diff(&first, &second, 3)),
            [
                (Started, "a"),
                (Completed, "b"),
                (Created, "c"),
                (Completed, "c")
            ]
        );

        let third = vec![item("b", TodoStatus::Pending)];
        assert_eq!(
            kinds(&diff(&second, &third, 4)),
            [(Reopened, "b"), (Removed, "a"), (Removed, "c")]
        );
    }

    #[test]
    fn events_append_as_json_lines_beside_the_store() {
        let dir = std::env::temp_dir().join(format!("todo-events-{}", now_ms()));
        std::fs::create_dir_all(&dir).expect("dir");
        let store = dir.join(".clawd-todos.json");
        assert_eq!(log_path(&store), dir.join(".clawd-todos.events.jsonl"));
        append(&store, &diff(&[], &[item("a", TodoStatus::Pending)], 7));
        append(&store, &diff(&[], &[item("b", TodoStatus::Completed)], 8));
        let log = std::fs::read_to_string(log_path(&store)).expect("log");
        let lines: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["event"], "task.created");
        assert_eq!(lines[0]["task"], "a");
        assert_eq!(lines[0]["status"], "pending");
        assert_eq!(lines[2]["event"], "task.completed");
        assert_eq!(lines[2]["at_ms"], 8);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
