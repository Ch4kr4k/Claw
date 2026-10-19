//! Auto memory: notes the model keeps for itself across sessions, so it
//! does not have to be told the same things again (how the user likes to
//! work, corrections, project decisions, where things are).
//!
//! Each project has a folder `projects/<project>/memory/` with one markdown
//! file per memory and an index, `MEMORY.md`, with one line per memory. It
//! is the same folder and format Claude Code uses (under `~/.claude`), so the
//! two share what they learn; without `~/.claude` it lives under `~/.claw`. The index and the instructions for keeping memories are added to
//! the system prompt; the model reads a memory file when it is relevant and
//! writes memories with the ordinary file tools. Nothing is created until
//! the model saves its first memory.
//!
//! On by default; `"autoMemoryEnabled": false` in settings or
//! `CLAW_AUTO_MEMORY=0` turns it off.
//!
//! Beyond Claude Code's format, two optional additions (Claude Code ignores
//! them, so the folder stays shared):
//! - **Anchors**: a memory about specific code lists
//!   `anchors: path#symbol, path` in its frontmatter (paths relative to the
//!   project root). claw adds a fingerprint (`@hash`) of that code the first
//!   time it sees an anchor; when the code changes or disappears, the
//!   memory is marked stale in the index the model sees, and ranked last,
//!   so outdated notes are not cited with confidence.
//! - **Activity**: `.claw-activity.json` counts sessions in the project
//!   and when each memory was last read or written. Memories unused for
//!   many sessions are shown as dormant and ranked after the active ones;
//!   time away from the project does not age them. `MEMORY.md` itself is
//!   never rewritten.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::config::{claude_config_home, default_config_home, RuntimeConfig};

/// The index file inside the memory folder.
pub const MEMORY_INDEX: &str = "MEMORY.md";
/// The index is cut after this many lines or characters, so a long one
/// cannot crowd out the rest of the prompt.
const MAX_INDEX_LINES: usize = 200;
const MAX_INDEX_CHARS: usize = 25_000;

/// Whether auto memory is on for this configuration.
#[must_use]
pub fn memory_enabled(config: &RuntimeConfig) -> bool {
    if let Ok(value) = std::env::var("CLAW_AUTO_MEMORY") {
        if matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ) {
            return false;
        }
    }
    config
        .get("autoMemoryEnabled")
        .and_then(crate::json::JsonValue::as_bool)
        .unwrap_or(true)
}

/// The memory folder for the project containing `cwd` (its git root, or
/// `cwd` itself outside a repository).
#[must_use]
pub fn memory_dir(cwd: &Path) -> PathBuf {
    let root = project_root(cwd);
    let home = claude_config_home()
        .filter(|claude| claude.is_dir())
        .unwrap_or_else(default_config_home);
    home.join("projects")
        .join(project_slug(&root))
        .join("memory")
}

fn project_root(cwd: &Path) -> PathBuf {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map_or_else(|| cwd.clone(), Path::to_path_buf)
}

/// `/home/me/src/my_app` → `-home-me-src-my-app`, as Claude Code names
/// its project folders.
fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

/// Whether `path` (absolute, `~/…`, or relative to the current directory)
/// is inside the current project's memory folder. Paths with `..` never
/// are, so this cannot be used to reach anything else.
#[must_use]
pub fn is_memory_path(path: &str) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let expanded = crate::expand_home(path);
    if expanded
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return false;
    }
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let dir = memory_dir(&cwd);
    if absolute.starts_with(&dir) {
        return true;
    }
    // The config home may be reached through a symlink.
    let canonical_dir = dir
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .map(|parent| parent.join("memory"));
    canonical_dir.is_some_and(|canonical| absolute.starts_with(canonical))
}

/// The memory section for the project containing `cwd`, with anchors
/// checked and the index ranked (stale and dormant memories last).
#[must_use]
pub fn render_memory_section_for(cwd: &Path) -> String {
    let dir = memory_dir(cwd);
    let root = project_root(cwd);
    render_with(&dir, Some(&root))
}

/// The system prompt section: how to keep memories, then the index.
#[must_use]
pub fn render_memory_section(dir: &Path) -> String {
    render_with(dir, None)
}

fn render_with(dir: &Path, root: Option<&Path>) -> String {
    let index = fs::read_to_string(dir.join(MEMORY_INDEX)).unwrap_or_default();
    let index = match root {
        Some(root) => rank_index(&index, dir, root),
        None => index,
    };
    let index = truncate_index(&index);
    let dir_display = dir.display();
    let mut section = format!(
        "# Memory

You have a persistent, file-based memory at `{dir_display}/`. It carries what you learn about this user and project from one session to the next. The folder may not exist yet; writing a file creates it.

Each memory is one markdown file holding one fact, with frontmatter:

```markdown
---
name: <short-kebab-case-slug>
description: <one-line summary, used to decide relevance later>
metadata:
  type: user | feedback | project | reference
  anchors: <optional: path#symbol, path — the code this memory is about>
---

<the fact. For feedback and project memories add a **Why:** line and a **How to apply:** line.>
```

Anchors: when a memory describes specific code (where something lives, how a function behaves, an architecture decision tied to a module), list it under `anchors:` as comma-separated `path#symbol` (path relative to the project root; just `path` for a whole file). claw adds a fingerprint (`@…`) to each; when that code changes or disappears, the memory shows as stale in the index below. A stale memory may be outdated: check the code, then rewrite the memory (anchors without the `@…` part, so claw takes a new fingerprint) or delete it. Memories marked dormant have not been used for many sessions; prefer fresher ones when they conflict.

Types:
- user: who the user is, their role, expertise and preferences.
- feedback: how the user wants you to work: corrections (\"don't do X\") and approaches they confirmed. Include why.
- project: goals, decisions and constraints that the code and git history do not show. Write dates as absolute dates.
- reference: where to find things (URLs, dashboards, tickets, docs).

After writing a memory file, add one line for it to `{dir_display}/{MEMORY_INDEX}`: `- [Title](file.md) — short hook`. The index is loaded into every session; keep it to one line per memory and never put memory content in it.

When to save: when the user corrects you or states a preference, when you learn something about the project that will matter later, and when a task failed for a reason worth avoiding next time. Save it right away, with write_file or edit_file.
What not to save: what the code, git history or instruction files already record; details that only matter to the current task; secrets such as API keys, passwords or tokens.
Before saving, check whether an existing memory covers it, and update that file instead of adding a duplicate. Delete memories that turn out to be wrong.
Memories are what was true when they were written. If one names a file, function or flag, check that it still exists before relying on it.
If the user asks you to remember something, save it; if they ask you to forget something, delete it."
    );
    section.push_str("\n\n## MEMORY.md\n\n");
    if index.trim().is_empty() {
        section.push_str("(No memories saved yet.)");
    } else {
        section.push_str(index.trim_end());
    }
    section
}

/// Computes a fingerprint of an anchor's code: `(project root, path,
/// symbol)` → a short hash, `None` when the file or symbol is gone. The CLI
/// sets it (the symbol parser lives in the tools crate); without one,
/// anchors are kept but not checked.
pub type AnchorHasher = fn(&Path, &str, Option<&str>) -> Option<String>;

static HASHER: OnceLock<AnchorHasher> = OnceLock::new();

pub fn set_anchor_hasher(hasher: AnchorHasher) {
    let _ = HASHER.set(hasher);
}

/// Hidden file in the memory folder with the activity counts.
const ACTIVITY_FILE: &str = ".claw-activity.json";
/// Sessions without use after which a memory counts as dormant.
const DORMANT_AFTER: u64 = 40;

#[derive(Serialize, Deserialize, Default)]
struct Activity {
    /// Sessions started in this project.
    tick: u64,
    /// The session in which each memory file was last read or written.
    used: BTreeMap<String, u64>,
}

fn load_activity(dir: &Path) -> Activity {
    fs::read_to_string(dir.join(ACTIVITY_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_activity(dir: &Path, activity: &Activity) {
    if dir.is_dir() {
        if let Ok(text) = serde_json::to_string_pretty(activity) {
            let _ = fs::write(dir.join(ACTIVITY_FILE), text);
        }
    }
}

/// Counts a session started in the project containing `cwd`.
pub fn record_session(cwd: &Path) {
    let dir = memory_dir(cwd);
    if !dir.is_dir() {
        return;
    }
    let mut activity = load_activity(&dir);
    activity.tick += 1;
    save_activity(&dir, &activity);
}

/// Notes that the memory file at `path` was read or written now.
pub fn touch_memory(path: &str) {
    if !is_memory_path(path) {
        return;
    }
    let path = crate::expand_home(path);
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let name = name.to_string_lossy().into_owned();
    if name == MEMORY_INDEX || !name.ends_with(".md") {
        return;
    }
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(dir)
    };
    let mut activity = load_activity(&dir);
    activity.used.insert(name, activity.tick);
    save_activity(&dir, &activity);
}

/// One `anchors:` entry: `path#symbol@hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Anchor {
    path: String,
    symbol: Option<String>,
    hash: Option<String>,
}

fn parse_anchors(value: &str) -> Vec<Anchor> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            let (target, hash) = match item.rsplit_once('@') {
                Some((target, hash)) if !hash.is_empty() => (target, Some(hash.to_string())),
                _ => (item, None),
            };
            let (path, symbol) = match target.split_once('#') {
                Some((path, symbol)) if !symbol.is_empty() => (path, Some(symbol.to_string())),
                _ => (target, None),
            };
            Anchor {
                path: path.trim().to_string(),
                symbol,
                hash,
            }
        })
        .collect()
}

fn format_anchor(anchor: &Anchor) -> String {
    let mut text = anchor.path.clone();
    if let Some(symbol) = &anchor.symbol {
        text.push('#');
        text.push_str(symbol);
    }
    if let Some(hash) = &anchor.hash {
        text.push('@');
        text.push_str(hash);
    }
    text
}

/// What the anchors of one memory say.
#[derive(Debug, PartialEq, Eq)]
enum Freshness {
    Fresh,
    /// The anchored code changed or is gone: what to tell the model.
    Stale(String),
}

/// Checks the anchors in a memory file, recording fingerprints for new
/// anchors (the file's `anchors:` line is rewritten for that).
fn check_anchors(file: &Path, root: &Path, hasher: AnchorHasher) -> Freshness {
    let Ok(content) = fs::read_to_string(file) else {
        return Freshness::Fresh;
    };
    let Some(value) = frontmatter_field(&content, "anchors") else {
        return Freshness::Fresh;
    };
    let mut anchors = parse_anchors(&value);
    let mut problems = Vec::new();
    let mut recorded = false;
    for anchor in &mut anchors {
        let target = anchor.symbol.as_ref().map_or_else(
            || anchor.path.clone(),
            |symbol| format!("{}#{symbol}", anchor.path),
        );
        match (
            hasher(root, &anchor.path, anchor.symbol.as_deref()),
            &anchor.hash,
        ) {
            (None, _) => problems.push(format!("{target} no longer exists")),
            (Some(now), None) => {
                anchor.hash = Some(now);
                recorded = true;
            }
            (Some(now), Some(then)) if now != *then => {
                problems.push(format!("{target} changed since this was saved"));
            }
            _ => {}
        }
    }
    if recorded {
        let line = format!(
            "anchors: {}",
            anchors
                .iter()
                .map(format_anchor)
                .collect::<Vec<_>>()
                .join(", ")
        );
        let updated: Vec<String> = content
            .lines()
            .map(|existing| {
                if existing.trim_start().starts_with("anchors:") {
                    let indent = &existing[..existing.len() - existing.trim_start().len()];
                    format!("{indent}{line}")
                } else {
                    existing.to_string()
                }
            })
            .collect();
        let mut text = updated.join("\n");
        if content.ends_with('\n') {
            text.push('\n');
        }
        let _ = fs::write(file, text);
    }
    if problems.is_empty() {
        Freshness::Fresh
    } else {
        Freshness::Stale(problems.join("; "))
    }
}

/// The index with each line's memory checked: active memories first (in
/// their order), then stale ones (anchored code changed), then dormant ones
/// (unused for many sessions), each marked for the model.
fn rank_index(index: &str, dir: &Path, root: &Path) -> String {
    let hasher = HASHER.get().copied();
    let mut activity = load_activity(dir);
    let mut seen_new = false;
    let (mut active, mut stale, mut dormant) = (Vec::new(), Vec::new(), Vec::new());
    for line in index.lines() {
        let Some(file) = linked_file(line) else {
            active.push(line.to_string());
            continue;
        };
        let path = dir.join(&file);
        if let Some(Freshness::Stale(why)) = hasher.map(|hasher| check_anchors(&path, root, hasher))
        {
            stale.push(format!(
                "{line} ⚠ stale: {why}; verify before relying on it"
            ));
            continue;
        }
        let last = *activity.used.entry(file).or_insert_with(|| {
            seen_new = true;
            activity.tick
        });
        let idle = activity.tick.saturating_sub(last);
        if idle > DORMANT_AFTER {
            dormant.push(format!("{line} (dormant: not used in {idle} sessions)"));
        } else {
            active.push(line.to_string());
        }
    }
    if seen_new {
        save_activity(dir, &activity);
    }
    let mut out = active;
    out.extend(stale);
    out.extend(dormant);
    let mut text = out.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// The memory file an index line links to: `- [Title](file.md) — hook`.
fn linked_file(line: &str) -> Option<String> {
    let start = line.find("](")? + 2;
    let end = start + line[start..].find(')')?;
    let file = &line[start..end];
    (file.ends_with(".md") && !file.contains('/')).then(|| file.to_string())
}

fn truncate_index(index: &str) -> String {
    let mut out = String::new();
    for (count, line) in index.lines().enumerate() {
        if count == MAX_INDEX_LINES || out.len() + line.len() + 1 > MAX_INDEX_CHARS {
            out.push_str("… (index cut short; keep it to one line per memory)\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// One memory file, as `/memory` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub path: PathBuf,
    pub name: String,
    pub description: String,
    pub kind: String,
}

/// The memory files in `dir` (everything but the index), sorted by name.
#[must_use]
pub fn list_memories(dir: &Path) -> Vec<MemoryEntry> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut memories = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "md")
                && path.file_name().is_some_and(|name| name != MEMORY_INDEX)
        })
        .map(|path| {
            let content = fs::read_to_string(&path).unwrap_or_default();
            let field = |key: &str| frontmatter_field(&content, key).unwrap_or_default();
            let mut name = field("name");
            if name.is_empty() {
                name = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
            MemoryEntry {
                name,
                description: field("description"),
                kind: field("type"),
                path,
            }
        })
        .collect::<Vec<_>>();
    memories.sort_by(|a, b| a.name.cmp(&b.name));
    memories
}

/// A `key: value` line from the frontmatter (also inside a `metadata:`
/// block, as Claude Code writes `type`).
fn frontmatter_field(content: &str, key: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    lines
        .take_while(|line| line.trim() != "---")
        .find_map(|line| {
            let (found, value) = line.trim().split_once(':')?;
            (found.trim() == key).then(|| value.trim().trim_matches('"').to_string())
        })
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        frontmatter_field, is_memory_path, list_memories, memory_dir, project_slug,
        render_memory_section, truncate_index, MAX_INDEX_LINES, MEMORY_INDEX,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("claw-memory-{label}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn slug_replaces_separators() {
        assert_eq!(
            project_slug(Path::new("/home/me/src/my_app.v2")),
            "-home-me-src-my-app-v2"
        );
    }

    #[test]
    fn section_has_instructions_and_index() {
        let dir = temp_dir("section");
        let empty = render_memory_section(&dir);
        assert!(empty.contains("# Memory"));
        assert!(empty.contains("(No memories saved yet.)"));

        fs::write(
            dir.join(MEMORY_INDEX),
            "- [Formatting](fmt.md) — use scripts/fmt.sh\n",
        )
        .expect("write index");
        let section = render_memory_section(&dir);
        assert!(section.ends_with("- [Formatting](fmt.md) — use scripts/fmt.sh"));
        assert!(section.contains(&format!("`{}/`", dir.display())));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn long_index_is_cut() {
        let index = "- line\n".repeat(MAX_INDEX_LINES + 50);
        let cut = truncate_index(&index);
        assert_eq!(cut.lines().count(), MAX_INDEX_LINES + 1);
        assert!(cut.contains("index cut short"));
    }

    #[test]
    fn lists_memories_from_frontmatter() {
        let dir = temp_dir("list");
        fs::write(dir.join(MEMORY_INDEX), "- [x](x.md)\n").expect("write index");
        fs::write(
            dir.join("fmt.md"),
            "---\nname: use-fmt-script\ndescription: run scripts/fmt.sh\nmetadata:\n  type: feedback\n---\n\nbody\n",
        )
        .expect("write memory");
        fs::write(dir.join("bare.md"), "no frontmatter\n").expect("write memory");
        let memories = list_memories(&dir);
        assert_eq!(memories.len(), 2);
        assert_eq!(memories[0].name, "bare");
        assert_eq!(memories[1].name, "use-fmt-script");
        assert_eq!(memories[1].description, "run scripts/fmt.sh");
        assert_eq!(memories[1].kind, "feedback");
        assert_eq!(frontmatter_field("body only", "name"), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn anchors_turn_stale_when_their_code_changes() {
        use super::{check_anchors, parse_anchors, Freshness};
        fn hasher(root: &Path, path: &str, symbol: Option<&str>) -> Option<String> {
            let text = fs::read_to_string(root.join(path)).ok()?;
            let wanted = symbol.map_or_else(
                || text.clone(),
                |s| {
                    text.lines()
                        .find(|l| l.contains(s))
                        .unwrap_or("")
                        .to_string()
                },
            );
            (!wanted.is_empty()).then(|| format!("{:x}", wanted.len()))
        }
        let root = temp_dir("anchor-root");
        let dir = temp_dir("anchor-mem");
        fs::write(root.join("lib.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        let memory = dir.join("m.md");
        fs::write(&memory, "---\nname: m\nmetadata:\n  type: project\n  anchors: lib.rs#alpha, lib.rs\n---\n\nbody\n").unwrap();
        // First sight: fingerprints are recorded in the file.
        assert_eq!(check_anchors(&memory, &root, hasher), Freshness::Fresh);
        let saved = fs::read_to_string(&memory).unwrap();
        let anchors = parse_anchors(&frontmatter_field(&saved, "anchors").unwrap());
        assert!(anchors.iter().all(|a| a.hash.is_some()), "{saved}");
        assert!(saved.contains("  anchors: lib.rs#alpha@"), "{saved}");
        // The code changes: stale.
        fs::write(root.join("lib.rs"), "fn alpha(x: u8) {}\nfn beta() {}\n").unwrap();
        assert!(
            matches!(check_anchors(&memory, &root, hasher), Freshness::Stale(why) if why.contains("lib.rs#alpha changed"))
        );
        // The symbol goes away.
        fs::write(root.join("lib.rs"), "fn beta() {}\n").unwrap();
        assert!(
            matches!(check_anchors(&memory, &root, hasher), Freshness::Stale(why) if why.contains("no longer exists"))
        );
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn index_ranks_dormant_memories_last() {
        use super::{rank_index, Activity, ACTIVITY_FILE};
        let dir = temp_dir("rank");
        let root = temp_dir("rank-root");
        let mut activity = Activity {
            tick: 100,
            ..Activity::default()
        };
        activity.used.insert("old.md".to_string(), 10);
        activity.used.insert("new.md".to_string(), 99);
        fs::write(
            dir.join(ACTIVITY_FILE),
            serde_json::to_string(&activity).unwrap(),
        )
        .unwrap();
        let index =
            "- [Old](old.md) — old\n- [New](new.md) — new\n- [Fresh](fresh.md) — never seen\n";
        let ranked = rank_index(index, &dir, &root);
        let lines: Vec<&str> = ranked.lines().collect();
        assert_eq!(lines[0], "- [New](new.md) — new");
        assert_eq!(lines[1], "- [Fresh](fresh.md) — never seen");
        assert!(
            lines[2].starts_with("- [Old](old.md) — old (dormant: not used in 90 sessions)"),
            "{ranked}"
        );
        fs::remove_dir_all(dir).ok();
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn memory_paths_are_recognized_without_escapes() {
        let cwd = std::env::current_dir().expect("cwd");
        let dir = memory_dir(&cwd);
        let inside = dir.join("note.md").display().to_string();
        assert!(is_memory_path(&inside));
        let escape = dir.join("..").join("..").join("x.md").display().to_string();
        assert!(!is_memory_path(&escape));
        assert!(!is_memory_path("src/lib.rs"));
    }
}
