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

use std::fs;
use std::path::{Component, Path, PathBuf};

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

/// The system prompt section: how to keep memories, then the index.
#[must_use]
pub fn render_memory_section(dir: &Path) -> String {
    let index = fs::read_to_string(dir.join(MEMORY_INDEX)).unwrap_or_default();
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
---

<the fact. For feedback and project memories add a **Why:** line and a **How to apply:** line.>
```

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
