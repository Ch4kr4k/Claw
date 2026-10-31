//! Instruction files in subdirectories (`AGENTS.md`, `CLAUDE.md`, …),
//! loaded when the model first works there. The system prompt holds only
//! the files from the working directory up to the repository root, so a
//! crate's own `AGENTS.md` was never seen unless claw started inside that
//! crate. The first tool call that touches a path below the working
//! directory brings that directory's files (and those of the directories
//! between) into its result, once a session. The system prompt does not
//! change, so the prompt cache keeps working.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

const FILE_NAMES: [&str; 5] = [
    "AGENTS.md",
    "CLAUDE.md",
    "CLAW.md",
    ".cursorrules",
    ".clinerules",
];
const MAX_FILE_CHARS: usize = 8_000;
const MAX_HINT_CHARS: usize = 16_000;
/// Input keys that name a path.
const PATH_KEYS: [&str; 4] = ["path", "file_path", "notebook_path", "cwd"];

#[derive(Debug)]
pub struct ContextHints {
    root: PathBuf,
    /// Directories already checked (or covered by the system prompt).
    seen: BTreeSet<PathBuf>,
}

impl ContextHints {
    /// Hints for work under `cwd`, whose own files are already in the
    /// system prompt.
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        let root = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let seen = root.ancestors().map(Path::to_path_buf).collect();
        Self { root, seen }
    }

    /// Instructions to add to the result of `tool` called with `input`:
    /// the files of directories it touches for the first time.
    pub fn for_call(&mut self, tool: &str, input: &str) -> Option<String> {
        let value = serde_json::from_str::<serde_json::Value>(input).ok()?;
        let mut paths = PATH_KEYS
            .iter()
            .filter_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if tool.eq_ignore_ascii_case("bash") {
            if let Some(command) = value.get("command").and_then(serde_json::Value::as_str) {
                paths.extend(cd_targets(command));
            }
        }
        let mut sections = Vec::new();
        let mut used = 0;
        for path in paths {
            for dir in self.new_dirs(&path) {
                for name in FILE_NAMES {
                    let file = dir.join(name);
                    let Ok(text) = fs::read_to_string(&file) else {
                        continue;
                    };
                    let text = text.trim();
                    if text.is_empty() || used >= MAX_HINT_CHARS {
                        continue;
                    }
                    let room = MAX_FILE_CHARS.min(MAX_HINT_CHARS - used);
                    let shown = cut(text, room);
                    used += shown.len();
                    let shown_path = file.strip_prefix(&self.root).unwrap_or(&file);
                    let mut section = format!("## {}\n{shown}", shown_path.display());
                    let found = crate::threats::scan(text, crate::threats::Scope::Context);
                    if !found.is_empty() {
                        section.push_str("\n\n");
                        section.push_str(&crate::threats::warning(
                            &format!("{}", shown_path.display()),
                            &found,
                        ));
                    }
                    sections.push(section);
                }
            }
        }
        (!sections.is_empty()).then(|| {
            format!(
                "<system-reminder>\nInstructions for the part of the project you are now working \
                 in (they apply to files there, alongside the ones you already have):\n\n{}\n\
                 </system-reminder>",
                sections.join("\n\n")
            )
        })
    }

    /// The directories from `path`'s directory up to the root that have
    /// not been checked yet, outermost first; marks them checked.
    fn new_dirs(&mut self, path: &str) -> Vec<PathBuf> {
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let path = normalize(&path);
        let dir = if path.is_dir() {
            path
        } else {
            match path.parent() {
                Some(parent) => parent.to_path_buf(),
                None => return Vec::new(),
            }
        };
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        if !dir.starts_with(&self.root) {
            return Vec::new();
        }
        let mut dirs = dir
            .ancestors()
            .take_while(|ancestor| !self.seen.contains(*ancestor))
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        dirs.reverse();
        // Generated and dependency folders hold no instructions for us.
        dirs.retain(|dir| {
            !dir.components().any(|part| {
                matches!(part, Component::Normal(name)
                    if matches!(name.to_str(), Some("node_modules" | "target" | ".git" | "vendor" | "dist" | "build")))
            })
        });
        self.seen.extend(dirs.iter().cloned());
        dirs
    }
}

/// The directories a shell command `cd`s into.
fn cd_targets(command: &str) -> Vec<String> {
    command
        .split(['&', ';', '|', '\n', '(', ')'])
        .filter_map(|part| {
            let mut words = part.split_whitespace();
            (words.next()? == "cd").then(|| words.next())?
        })
        .map(|dir| dir.trim_matches(['"', '\'']).to_string())
        .filter(|dir| !dir.is_empty() && !dir.starts_with('-') && !dir.contains('$'))
        .collect()
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[… cut; read the file for the rest]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::{cd_targets, ContextHints};

    #[test]
    fn a_subdirectorys_instructions_come_once_with_the_first_call_there() {
        let root = tempfile::tempdir().expect("tempdir");
        let crate_dir = root.path().join("crates/app/src");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(root.path().join("crates/app/AGENTS.md"), "app rules").unwrap();
        std::fs::write(crate_dir.join("main.rs"), "fn main() {}").unwrap();
        let mut hints = ContextHints::new(root.path());

        let hint = hints
            .for_call("read_file", r#"{"path":"crates/app/src/main.rs"}"#)
            .expect("hint");
        assert!(hint.contains("app rules"));
        assert!(!hint.contains("root rules"), "already in the system prompt");
        assert!(hints
            .for_call("edit_file", r#"{"path":"crates/app/src/main.rs"}"#)
            .is_none());
        assert!(hints
            .for_call("read_file", r#"{"path":"/etc/hostname"}"#)
            .is_none());
    }

    #[test]
    fn cd_targets_are_found_in_commands() {
        assert_eq!(
            cd_targets("cd crates/app && cargo test; (cd ../x) | cat"),
            vec!["crates/app", "../x"]
        );
        assert!(cd_targets("echo cd here; cd $DIR").is_empty());
    }
}
