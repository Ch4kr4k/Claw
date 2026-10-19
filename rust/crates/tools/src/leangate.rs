//! LeanGate: a referee in the agent's loop, after the "LeanGate" pattern.
//!
//! - Before `write_file`/`edit_file` lands, the new code's definitions are
//!   compared with the repository's symbol index (the one `CodeSearch`
//!   keeps). A function or type whose name already exists in another file
//!   is held back once, with a pointer to the existing one, so the agent
//!   reuses it instead of writing a second copy; repeating the same call
//!   writes it anyway. Merely similar names get a note in the result.
//! - When a turn that edited files is about to finish, [`completion_check`]
//!   runs the project's type checker; errors in the edited files are sent
//!   back to the agent so it fixes them before stopping.
//!
//! `"leanGate": false` in settings.json, or `CLAW_LEANGATE=0`, turns both off.

use std::collections::{BTreeSet, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::code_search;
use super::dev_tools;

/// Names too generic to count as duplicates.
const GENERIC_NAMES: &[&str] = &[
    "new",
    "main",
    "default",
    "from",
    "into",
    "fmt",
    "drop",
    "run",
    "test",
    "tests",
    "setup",
    "init",
    "clone",
    "eq",
    "hash",
    "len",
    "get",
    "set",
    "build",
    "parse",
    "apply",
    "call",
    "handle",
    "update",
    "render",
    "start",
    "stop",
    "reset",
    "next",
    "load",
    "save",
    "open",
    "close",
    "read",
    "write",
    "send",
    "error",
    "config",
    "options",
    "state",
    "app",
    "data",
    "item",
    "value",
    "result",
    "helper",
    "utils",
    "util",
    "index",
    "init_test",
];
/// Files the edit tools changed since the gate last looked.
static EDITED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
/// Writes held back once; the same call again goes through.
static HELD: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// Whether LeanGate runs (on unless switched off).
#[must_use]
pub fn enabled() -> bool {
    if std::env::var("CLAW_LEANGATE")
        .is_ok_and(|value| matches!(value.as_str(), "0" | "false" | "off"))
    {
        return false;
    }
    let settings = runtime::default_config_home().join("settings.json");
    !std::fs::read_to_string(settings)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|root| root.get("leanGate").cloned())
        .is_some_and(|value| value == Value::Bool(false))
}

/// Remembers that an edit tool changed `path`.
pub(crate) fn record_edit(path: &str) {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    EDITED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path);
}

/// The files edited since the last call (and forgets them).
#[must_use]
pub fn take_edited_files() -> Vec<PathBuf> {
    std::mem::take(
        &mut *EDITED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_iter()
    .collect()
}

/// What the duplicate check found for one write.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Finding {
    /// New definitions whose name already exists elsewhere.
    pub(crate) duplicates: Vec<String>,
    /// New definitions that resemble existing ones.
    pub(crate) similar: Vec<String>,
}

fn normalized(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn countable(name: &str, kind: &str) -> bool {
    !matches!(kind, "impl" | "module" | "constant")
        && name.chars().count() >= 4
        && !GENERIC_NAMES.contains(&name.to_ascii_lowercase().as_str())
        && !name.starts_with("test")
}

/// Repository root of `path` (git work tree, else its folder).
fn repo_root(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    let dir = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

/// Compares the definitions `new_text` adds to `path` (beyond those in
/// `old_text`) with the rest of the repository.
pub(crate) fn find_duplicates(path: &str, old_text: &str, new_text: &str) -> Finding {
    let mut finding = Finding::default();
    let Some(language) = code_search::language(path) else {
        return finding;
    };
    let before: BTreeSet<String> = code_search::extract_symbols(old_text, language)
        .into_iter()
        .map(|symbol| symbol.name)
        .collect();
    let added: Vec<code_search::Symbol> = code_search::extract_symbols(new_text, language)
        .into_iter()
        .filter(|symbol| !before.contains(&symbol.name) && countable(&symbol.name, &symbol.kind))
        .collect();
    if added.is_empty() {
        return finding;
    }
    let absolute = std::fs::canonicalize(path)
        .or_else(|_| std::env::current_dir().map(|cwd| cwd.join(path)))
        .unwrap_or_else(|_| PathBuf::from(path));
    let Some(root) = repo_root(&absolute) else {
        return finding;
    };
    let relative = absolute
        .strip_prefix(&root)
        .map(|rel| rel.to_string_lossy().into_owned())
        .unwrap_or_default();
    let files = code_search::list_files(&root);
    let (index, _) = code_search::load_index(&root, &files, false);
    for symbol in &added {
        let wanted = normalized(&symbol.name);
        let parts: BTreeSet<String> = code_search::split_identifier(&symbol.name)
            .into_iter()
            .collect();
        for (file, entry) in &index.files {
            if *file == relative {
                continue;
            }
            for existing in &entry.symbols {
                if !countable(&existing.name, &existing.kind) {
                    continue;
                }
                let place = format!(
                    "`{}` ({}) at {file}:{}",
                    existing.name, existing.kind, existing.line
                );
                if normalized(&existing.name) == wanted {
                    if finding.duplicates.len() < 5 {
                        finding
                            .duplicates
                            .push(format!("`{}` already exists: {place}", symbol.name));
                    }
                } else if parts.len() >= 2 {
                    let theirs: BTreeSet<String> = code_search::split_identifier(&existing.name)
                        .into_iter()
                        .collect();
                    let shared = parts.intersection(&theirs).count();
                    let union = parts.union(&theirs).count();
                    if shared >= 2 && shared * 3 >= union * 2 && finding.similar.len() < 5 {
                        finding
                            .similar
                            .push(format!("`{}` resembles {place}", symbol.name));
                    }
                }
            }
        }
    }
    finding
}

fn call_key(tool: &str, path: &str, text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (tool, path, text).hash(&mut hasher);
    hasher.finish()
}

/// The check before a write: `Err` holds it back (once), `Ok(Some)` is a
/// note for the result, `Ok(None)` means nothing to say.
pub(crate) fn before_write(
    tool: &str,
    path: &str,
    old_text: &str,
    new_text: &str,
) -> Result<Option<String>, String> {
    if !enabled() {
        return Ok(None);
    }
    let finding = find_duplicates(path, old_text, new_text);
    if !finding.duplicates.is_empty() {
        let key = call_key(tool, path, new_text);
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let held = held.get_or_insert_with(HashSet::new);
        if held.insert(key) {
            return Err(format!(
                "[leangate] Not written yet: this adds code the repository already has.\n- {}\nRead the existing definition and reuse or extend it instead of writing a second copy. If a separate one is really needed, make the identical call again and it will be written.",
                finding.duplicates.join("\n- ")
            ));
        }
    }
    if finding.similar.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "Similar code already exists; check it can't be reused: {}",
        finding.similar.join("; ")
    )))
}

/// Adds the note to a tool's JSON result as a `leangate` field.
pub(crate) fn annotate(output: String, note: Option<String>) -> String {
    let Some(note) = note else {
        return output;
    };
    match serde_json::from_str::<Value>(&output) {
        Ok(Value::Object(mut map)) => {
            map.insert("leangate".to_string(), Value::String(note));
            serde_json::to_string_pretty(&Value::Object(map)).unwrap_or(output)
        }
        _ => format!("{output}\n[leangate] {note}"),
    }
}

/// Runs the type checkers for the projects holding `edited` and returns a
/// report of the errors that mention those files, for the agent to fix;
/// `None` when there are none (or nothing could be checked).
#[must_use]
pub fn completion_check(edited: &[PathBuf]) -> Option<String> {
    if !enabled() || edited.is_empty() {
        return None;
    }
    let timeout = Duration::from_secs(
        std::env::var("CLAW_LEANGATE_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(240),
    );
    let mut roots: Vec<(PathBuf, Vec<dev_tools::Check>)> = Vec::new();
    for file in edited {
        if code_search::language(&file.to_string_lossy()).is_none() {
            continue;
        }
        let (root, checks) = dev_tools::plan_checks(file, false);
        if roots.iter().any(|(known, _)| *known == root) {
            continue;
        }
        let types: Vec<dev_tools::Check> = checks
            .into_iter()
            .filter(|check| check.kind == "types")
            .collect();
        if !types.is_empty() {
            roots.push((root, types));
        }
    }
    let names: Vec<String> = edited
        .iter()
        .filter_map(|file| {
            file.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    let mut problems: Vec<String> = Vec::new();
    for (root, checks) in &roots {
        for check in checks {
            let result = dev_tools::run_check(check, root, timeout);
            if result["timed_out"] == true {
                continue;
            }
            // RepoSignals reads which files the failing check named.
            super::signals::record_failures(
                root,
                "LeanGate type check",
                if result["ok"] == true {
                    ""
                } else {
                    result["output"].as_str().unwrap_or_default()
                },
            );
            if result["ok"] == true {
                continue;
            }
            let command = result["command"].as_str().unwrap_or_default().to_string();
            let output = result["output"].as_str().unwrap_or_default();
            let relevant: Vec<&str> = output
                .lines()
                .filter(|line| {
                    let lower = line.to_lowercase();
                    lower.contains("error") && names.iter().any(|name| line.contains(name.as_str()))
                })
                .take(25)
                .collect();
            if !relevant.is_empty() {
                problems.push(format!("$ {command}\n{}", relevant.join("\n")));
            }
        }
    }
    if problems.is_empty() {
        return None;
    }
    Some(format!(
        "[leangate] Not done yet: the type checker reports errors in files you edited this turn. Fix them (read the code around each line first), then finish.\n\n{}",
        problems.join("\n\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::{annotate, find_duplicates};

    #[test]
    fn spots_a_function_the_repository_already_has() {
        // This crate defines `strip_ansi` in pty_tool.rs.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/leangate_probe.rs");
        let finding = find_duplicates(
            path,
            "",
            "pub fn strip_ansi(text: &str) -> String { text.into() }\n",
        );
        assert!(
            finding.duplicates.iter().any(|d| d.contains("pty_tool.rs")),
            "{finding:?}"
        );
        let unrelated = find_duplicates(path, "", "fn zzq_unlikely_name_xyz() {}\n");
        assert!(unrelated.duplicates.is_empty(), "{unrelated:?}");
        let generic = find_duplicates(path, "", "fn new() {}\nfn main() {}\n");
        assert!(generic.duplicates.is_empty(), "{generic:?}");
    }

    #[test]
    fn notes_go_into_the_json_result() {
        let out = annotate("{\"ok\": true}".to_string(), Some("look".to_string()));
        assert!(out.contains("\"leangate\": \"look\""), "{out}");
        assert_eq!(annotate("x".to_string(), None), "x");
    }

    #[test]
    fn completion_check_reports_type_errors_in_edited_files() {
        if std::process::Command::new("cargo")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("claw-leangate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"lg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .unwrap();
        let file = dir.join("src/main.rs");
        std::fs::write(
            &file,
            "fn main() { let x: u32 = \"text\"; println!(\"{x}\"); }\n",
        )
        .unwrap();
        let report =
            super::completion_check(std::slice::from_ref(&file)).expect("a type error is reported");
        assert!(report.contains("main.rs"), "{report}");
        std::fs::write(&file, "fn main() { let x: u32 = 1; println!(\"{x}\"); }\n").unwrap();
        assert_eq!(super::completion_check(&[file]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
