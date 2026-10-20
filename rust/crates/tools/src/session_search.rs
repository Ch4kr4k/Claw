//! `SessionSearch`: full-text search over past claw sessions (as hermes'
//! session search), so the agent can recall what was done or decided in
//! earlier conversations, in this folder or any other claw was used in.
//!
//! Sessions are the JSONL files under each workspace's `.claw/sessions/`
//! (workspaces listed in `~/.claw/workspaces.txt`, plus the current folder)
//! and under `~/.claw/sessions/`. Every word of the query must appear in a
//! session; sessions are ranked by how often the words occur, newest first
//! on a tie, and each match comes with the text around it.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use runtime::PermissionMode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ToolSpec;

const DEFAULT_LIMIT: usize = 5;
const SNIPPETS_PER_SESSION: usize = 3;
const SNIPPET_RADIUS: usize = 160;
/// Larger session files are read only up to this many bytes.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "SessionSearch",
        description: "Search the user's past claw sessions (every folder claw was used in) for words, to recall what was done, decided or found earlier: a fix, a command that worked, a design choice, an error seen before. Every word of `query` must occur in a session; results are ranked by matches, each with its folder, date, first request and the text around the matches. Use it when the user refers to earlier work (\"like last time\", \"the bug we fixed yesterday\") or before redoing an investigation. `workspace` limits it to one folder (\".\" for the current one).",
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Words to find (all must occur, any case)." },
                "workspace": { "type": "string", "description": "Only sessions of this folder (\".\" = current)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::ReadOnly,
    }]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    (name == "SessionSearch").then(|| {
        serde_json::from_value::<SearchInput>(input.clone())
            .map_err(|error| error.to_string())
            .and_then(|input| search(&input))
    })
}

#[derive(Deserialize)]
struct SearchInput {
    query: String,
    workspace: Option<String>,
    limit: Option<usize>,
}

struct Hit {
    path: PathBuf,
    workspace: String,
    modified: std::time::SystemTime,
    score: usize,
    first_prompt: String,
    snippets: Vec<String>,
}

fn search(input: &SearchInput) -> Result<String, String> {
    let words = input
        .query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if words.is_empty() {
        return Err("query is empty".to_string());
    }
    let only = input.workspace.as_deref().map(|workspace| {
        let path = if workspace == "." {
            std::env::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(workspace)
        };
        std::fs::canonicalize(&path).unwrap_or(path)
    });
    let mut hits = session_files(only.as_deref())
        .into_iter()
        .filter_map(|(workspace, path)| match_session(&path, &workspace, &words))
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.modified.cmp(&a.modified)));
    hits.truncate(input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 20));
    if hits.is_empty() {
        return Ok(format!(
            "No past session contains all of: {}",
            words.join(", ")
        ));
    }
    let mut out = format!(
        "{} session(s) match \"{}\":\n",
        hits.len(),
        input.query.trim()
    );
    for hit in hits {
        let age = hit
            .modified
            .elapsed()
            .map_or_else(|_| "unknown".to_string(), |age| format_age(age.as_secs()));
        out.push_str(&format!(
            "\n## {} ({age} ago, {} matches)\nsession: {}\nfirst request: {}\n",
            hit.workspace,
            hit.score,
            hit.path.display(),
            hit.first_prompt
        ));
        for snippet in hit.snippets {
            out.push_str(&format!("  … {snippet} …\n"));
        }
    }
    out.push_str("\nResume one with /resume <session file> to see it whole.");
    Ok(out)
}

fn format_age(secs: u64) -> String {
    match secs {
        s if s < 3_600 => format!("{} min", s / 60),
        s if s < 86_400 => format!("{} h", s / 3_600),
        s => format!("{} days", s / 86_400),
    }
}

/// Every session file claw knows of, with its workspace folder; only
/// `only`'s when given.
fn session_files(only: Option<&Path>) -> Vec<(String, PathBuf)> {
    let home = runtime::default_config_home();
    let mut workspaces = BTreeSet::new();
    if let Ok(cwd) = std::env::current_dir() {
        workspaces.insert(std::fs::canonicalize(&cwd).unwrap_or(cwd));
    }
    if let Ok(listed) = std::fs::read_to_string(home.join("workspaces.txt")) {
        workspaces.extend(
            listed
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(PathBuf::from),
        );
    }
    let mut roots = workspaces
        .into_iter()
        .filter(|workspace| only.is_none_or(|only| workspace == only))
        .map(|workspace| {
            let sessions = workspace.join(".claw").join("sessions");
            (workspace.display().to_string(), sessions)
        })
        .collect::<Vec<_>>();
    if only.is_none() {
        roots.push(("~/.claw".to_string(), home.join("sessions")));
    }
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for (workspace, root) in roots {
        // Sessions sit one level down, in a folder per workspace fingerprint.
        let Ok(dirs) = std::fs::read_dir(&root) else {
            continue;
        };
        for dir in dirs.flatten() {
            let Ok(entries) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("session-")
                    && name.ends_with(".jsonl")
                    && seen.insert(path.clone())
                {
                    files.push((workspace.clone(), path));
                }
            }
        }
    }
    files
}

/// The session's searchable text (what the user and the model wrote, and
/// tool output), its first request, and when it last changed.
fn session_text(path: &Path) -> Option<(String, String)> {
    let mut raw = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_FILE_BYTES)
        .read_to_string(&mut raw)
        .ok()?;
    let mut text = String::new();
    let mut first_prompt = String::new();
    for line in raw.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(message) = record.get("message") else {
            continue;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let blocks = message.get("blocks").and_then(Value::as_array);
        for block in blocks.into_iter().flatten() {
            let piece = ["text", "output", "input"]
                .iter()
                .find_map(|key| block.get(*key).and_then(Value::as_str))
                .unwrap_or("");
            if piece.is_empty() || piece.starts_with("<system-reminder>") {
                continue;
            }
            if role == "user" && first_prompt.is_empty() {
                first_prompt = piece
                    .chars()
                    .take(160)
                    .collect::<String>()
                    .replace('\n', " ");
            }
            text.push_str(piece);
            text.push('\n');
        }
    }
    (!text.is_empty()).then_some((text, first_prompt))
}

fn match_session(path: &Path, workspace: &str, words: &[String]) -> Option<Hit> {
    let (text, first_prompt) = session_text(path)?;
    let lower = text.to_lowercase();
    if !words.iter().all(|word| lower.contains(word.as_str())) {
        return None;
    }
    let score = words
        .iter()
        .map(|word| lower.matches(word.as_str()).count())
        .sum();
    let mut snippets = Vec::new();
    let mut last_end = 0;
    for (at, _) in lower.match_indices(words[0].as_str()) {
        if snippets.len() == SNIPPETS_PER_SESSION {
            break;
        }
        if at < last_end {
            continue;
        }
        let start = floor_boundary(&lower, at.saturating_sub(SNIPPET_RADIUS));
        let end = floor_boundary(&lower, (at + SNIPPET_RADIUS).min(lower.len()));
        last_end = end;
        // `lower` and `text` have the same boundaries for ASCII; fall back
        // to the lowered text when they differ.
        let piece = text.get(start..end).unwrap_or(&lower[start..end]);
        snippets.push(piece.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(Hit {
        path: path.to_path_buf(),
        workspace: workspace.to_string(),
        modified,
        score,
        first_prompt,
        snippets,
    })
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::match_session;

    #[test]
    fn finds_sessions_containing_every_word_with_context() {
        let dir = std::env::temp_dir().join(format!("claw-session-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("session-1-0.jsonl");
        let lines = [
            r#"{"type":"session_meta","session_id":"s"}"#,
            r#"{"type":"message","message":{"role":"user","blocks":[{"type":"text","text":"add a blur option to lightcomp"}]}}"#,
            r#"{"type":"message","message":{"role":"assistant","blocks":[{"type":"text","text":"Added blur-rule parsing to config.cpp using a box blur."}]}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).expect("write");

        let hit = match_session(&path, "/w/lightcomp", &["blur".into(), "config.cpp".into()])
            .expect("match");
        assert_eq!(hit.first_prompt, "add a blur option to lightcomp");
        assert!(hit.score >= 3);
        assert!(hit.snippets[0].contains("blur"));
        assert!(match_session(&path, "/w", &["blur".into(), "vulkan".into()]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
