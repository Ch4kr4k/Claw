//! Finds claw sessions on disk (the terminal REPL's and the app's alike) and
//! reads them back for display.
//!
//! claw saves a session as `session-<ms>-<n>.jsonl` under
//! `<workspace>/.claw/sessions/<fingerprint>/`, and sessions started in the
//! home folder under `~/.claw/sessions/<fingerprint>/`. The first line is a
//! `session_meta` record with the workspace; then come `message` records.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

/// One session as listed in the sidebar.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub workspace: PathBuf,
    pub title: String,
    pub model: Option<String>,
    /// Last change, in milliseconds since the epoch.
    pub updated_ms: u64,
}

/// A piece of a conversation, as shown in the chat view.
#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool {
        name: String,
        input: String,
        output: Option<String>,
        is_error: bool,
    },
    Notice(String),
}

/// `~/.claw` (or `$CLAW_CONFIG_HOME`).
pub fn config_home() -> PathBuf {
    std::env::var_os("CLAW_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claw")))
        .unwrap_or_else(|| PathBuf::from(".claw"))
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// Session folders to look in: the global one plus `<dir>/.claw/sessions`
/// for every known workspace.
fn session_roots(workspaces: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut roots = vec![config_home().join("sessions")];
    roots.extend(
        workspaces
            .iter()
            .map(|workspace| workspace.join(".claw").join("sessions")),
    );
    roots.sort();
    roots.dedup();
    roots
}

/// Every non-empty session in the known places, newest first.
pub fn list_sessions(workspaces: &BTreeSet<PathBuf>) -> Vec<SessionSummary> {
    let mut seen = BTreeSet::new();
    let mut sessions = Vec::new();
    for root in session_roots(workspaces) {
        let Ok(fingerprints) = fs::read_dir(&root) else {
            continue;
        };
        for fingerprint in fingerprints.flatten() {
            let Ok(files) = fs::read_dir(fingerprint.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let is_session = path.extension().is_some_and(|ext| ext == "jsonl")
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("session-"));
                if !is_session {
                    continue;
                }
                let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if !seen.insert(canonical) {
                    continue;
                }
                if let Some(summary) = summarize(&path) {
                    sessions.push(summary);
                }
            }
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
    sessions
}

/// Reads the head of a session file: its workspace and first prompt. A
/// session without any prompt is left out.
fn summarize(path: &Path) -> Option<SessionSummary> {
    let file = fs::File::open(path).ok()?;
    let updated_ms = file
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let mut workspace = None;
    let mut model = None;
    let mut title = None;
    // A title is near the top; stop early on long sessions.
    for line in BufReader::new(file).lines().take(400) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                workspace = value
                    .get("workspace_root")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                model = value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("prompt_history") if title.is_none() => {
                title = value
                    .get("text")
                    .and_then(Value::as_str)
                    .map(clean_prompt)
                    .filter(|text| !text.is_empty());
            }
            Some("message") if title.is_none() => {
                let message = value.get("message")?;
                if message.get("role").and_then(Value::as_str) == Some("user") {
                    title = message
                        .get("blocks")
                        .and_then(Value::as_array)
                        .and_then(|blocks| {
                            blocks.iter().find_map(|block| {
                                (block.get("type").and_then(Value::as_str) == Some("text"))
                                    .then(|| block.get("text").and_then(Value::as_str))
                                    .flatten()
                            })
                        })
                        .map(clean_prompt)
                        .filter(|text| !text.is_empty());
                }
            }
            _ => {}
        }
        if title.is_some() && workspace.is_some() {
            break;
        }
    }
    let title = title?;
    let workspace = workspace.unwrap_or_else(|| workspace_from_path(path));
    Some(SessionSummary {
        path: path.to_path_buf(),
        workspace,
        title: one_line(&title, 80),
        model,
        updated_ms,
    })
}

/// `<ws>/.claw/sessions/<fp>/<file>` -> `<ws>`, for old files without meta.
fn workspace_from_path(path: &Path) -> PathBuf {
    path.ancestors()
        .nth(4)
        .map_or_else(home_dir, Path::to_path_buf)
}

/// A prompt without the `<system-reminder>` blocks claw adds to it.
pub fn clean_prompt(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<system-reminder>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</system-reminder>") {
            Some(end) => rest = &rest[start + end + "</system-reminder>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line
    }
}

/// The conversation in a session file, for display.
pub fn load_entries(path: &Path) -> Vec<Entry> {
    let Ok(file) = fs::File::open(path) else {
        return vec![Entry::Notice(format!("Could not open {}", path.display()))];
    };
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let blocks = message
            .get("blocks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for block in blocks {
            let field = |key: &str| {
                block
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            match (role, block.get("type").and_then(Value::as_str)) {
                ("user", Some("text")) => {
                    let text = clean_prompt(&field("text"));
                    if text == "[Request interrupted by user]" {
                        entries.push(Entry::Notice("Stopped".to_string()));
                    } else if !text.is_empty() {
                        entries.push(Entry::User(text));
                    }
                }
                ("assistant", Some("text")) => {
                    let text = field("text");
                    if !text.trim().is_empty() {
                        entries.push(Entry::Assistant(text));
                    }
                }
                ("assistant", Some("thinking")) => {
                    let text = field("thinking");
                    if !text.trim().is_empty() {
                        entries.push(Entry::Thinking(text));
                    }
                }
                ("assistant", Some("tool_use")) => entries.push(Entry::Tool {
                    name: field("name"),
                    input: field("input"),
                    output: None,
                    is_error: false,
                }),
                (_, Some("tool_result")) => {
                    let name = field("tool_name");
                    let output = display_tool_output(&name, &field("output"));
                    let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                    // Attach to the oldest call of that tool still waiting.
                    let slot = entries.iter_mut().find_map(|entry| match entry {
                        Entry::Tool {
                            name: call,
                            output: slot @ None,
                            is_error: error,
                            ..
                        } if *call == name => Some((slot, error)),
                        _ => None,
                    });
                    if let Some((slot, error)) = slot {
                        *slot = Some(output);
                        *error = is_error;
                    }
                }
                ("system", Some("text")) => {
                    entries.push(Entry::Notice(one_line(&field("text"), 200)));
                }
                _ => {}
            }
        }
    }
    entries
}

/// Bash results are saved as a JSON envelope; show stdout/stderr as text.
pub fn display_tool_output(name: &str, output: &str) -> String {
    if matches!(name, "bash" | "Bash") {
        if let Ok(value) = serde_json::from_str::<Value>(output) {
            let mut parts = Vec::new();
            for key in ["stdout", "stderr"] {
                if let Some(text) = value.get(key).and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        parts.push(text.trim_end().to_string());
                    }
                }
            }
            if let Some(code) = value
                .get("returnCodeInterpretation")
                .and_then(Value::as_str)
            {
                parts.push(format!("[{code}]"));
            }
            return parts.join("\n");
        }
    }
    output.to_string()
}

/// A short, readable form of a tool call's input for its row header.
pub fn tool_summary(name: &str, input: &str) -> String {
    let value = serde_json::from_str::<Value>(input).unwrap_or(Value::Null);
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(str::to_string)
    };
    let detail = match name {
        "bash" | "Bash" => pick(&["command"]),
        "read_file" | "write_file" | "edit_file" | "Read" | "Write" | "Edit" => {
            pick(&["path", "file_path"])
        }
        "glob_search" | "grep_search" => pick(&["pattern"]),
        "web_fetch" | "WebFetch" | "Download" | "download" => pick(&["url"]),
        "web_search" | "WebSearch" => pick(&["query"]),
        "Agent" => pick(&["description", "prompt"]),
        "X11" | "x11" | "COM" | "com" => pick(&["action"]),
        _ => None,
    }
    .unwrap_or_else(|| input.to_string());
    one_line(&detail, 120)
}

/// Deletes a session file.
pub fn delete_session(path: &Path) -> std::io::Result<()> {
    // The undo checkpoints kept beside it would be orphaned.
    let _ = fs::remove_file(runtime::CheckpointStack::sidecar_path(path));
    fs::remove_file(path)
}

/// Folders under `root` (to `max_depth`) that hold claw sessions, for
/// finding sessions started in the terminal before the app knew the folder.
pub fn scan_workspaces(root: &Path, max_depth: usize) -> BTreeSet<PathBuf> {
    const SKIP: &[&str] = &[
        "node_modules",
        "target",
        "build",
        "dist",
        "venv",
        ".venv",
        "__pycache__",
        "proc",
        "sys",
    ];
    let mut found = BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), 0_usize)];
    while let Some((dir, depth)) = stack.pop() {
        if dir.join(".claw").join("sessions").is_dir() {
            found.insert(dir.clone());
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(children) = fs::read_dir(&dir) else {
            continue;
        };
        for child in children.flatten() {
            let Ok(kind) = child.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let name = child.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
                continue;
            }
            stack.push((child.path(), depth + 1));
        }
    }
    found
}

/// "just now", "5 min ago", "3 h ago", "yesterday", "12 days ago".
pub fn relative_time(updated_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(updated_ms) / 1000;
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        86_400..=172_799 => "yesterday".to_string(),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// `/home/me/ws/project` -> `~/ws/project`.
pub fn short_path(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_prompt, display_tool_output, relative_time};

    #[test]
    fn prompts_lose_system_reminders() {
        assert_eq!(
            clean_prompt("<system-reminder>\njob done\n</system-reminder>\n\nfix the build"),
            "fix the build"
        );
        assert_eq!(clean_prompt("plain"), "plain");
    }

    #[test]
    fn bash_output_is_shown_as_text() {
        let output = r#"{"stdout":"hi\n","stderr":"","returnCodeInterpretation":"exit_code:2"}"#;
        assert_eq!(display_tool_output("bash", output), "hi\n[exit_code:2]");
        assert_eq!(display_tool_output("read_file", "x"), "x");
    }

    #[test]
    fn relative_times_read_naturally() {
        assert_eq!(relative_time(0, 30_000), "just now");
        assert_eq!(relative_time(0, 5 * 60_000), "5 min ago");
        assert_eq!(relative_time(0, 3 * 3_600_000), "3 h ago");
        assert_eq!(relative_time(0, 30 * 3_600_000), "yesterday");
        assert_eq!(relative_time(0, 5 * 86_400_000), "5 days ago");
    }
}
