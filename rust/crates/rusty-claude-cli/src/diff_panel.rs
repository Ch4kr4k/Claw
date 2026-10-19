//! The diff panel, like Claude Code's: what changed in the working tree
//! (uncommitted changes against `HEAD`, new files included), shown right of
//! the conversation in full-screen mode. A header with the totals, one row
//! per file, then each file's diff. It is worked out again in the background
//! after edits and turns, so the window never waits for git.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use runtime::turn_keys::screen;

use crate::diff_view::{self, visible_width, Hunk};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[38;5;245m";
const GREEN: &str = "\x1b[38;5;70m";
const RED: &str = "\x1b[38;5;167m";
const BOLD: &str = "\x1b[1m";
/// The tree of an empty repository, to diff against before the first commit.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// New files bigger than this are listed without their content.
const MAX_NEW_FILE_BYTES: u64 = 512 * 1024;
/// At most this many new (untracked) files are read.
const MAX_NEW_FILES: usize = 200;
/// Rows of one file's diff shown at most.
const MAX_FILE_ROWS: usize = 2_000;
/// Rows of the whole panel drawn at most.
const MAX_PANEL_ROWS: usize = 20_000;

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub hunks: Vec<Hunk>,
    /// "new", "deleted", "binary" or "renamed", when it is one of those.
    pub note: Option<&'static str>,
}

/// Whether the panel is wanted: not turned off with `CLAW_DIFF_PANEL=0`
/// or `"diffPanel": "off"` in the settings.
pub(crate) fn enabled() -> bool {
    match std::env::var("CLAW_DIFF_PANEL") {
        Ok(value) => !matches!(value.trim(), "0" | "off" | "false" | "no"),
        Err(_) => runtime::read_user_setting("diffPanel").as_deref() != Some("off"),
    }
}

/// Columns the conversation will have once the panel shows: an edit is
/// about to open it, and output sized for the whole window would be
/// re-wrapped when it does.
pub(crate) fn output_cols_with_panel() -> Option<usize> {
    let cols = crate::render::output_cols()?;
    if !enabled() {
        return Some(cols);
    }
    match (screen::conversation_cols(), screen::side_panel_width()) {
        // No panel yet, but one would fit: leave its room.
        (Some(conversation), Some(panel)) if conversation == cols && cols > panel + 1 => {
            Some(cols - panel - 1)
        }
        _ => Some(cols),
    }
}

static RUNNING: AtomicBool = AtomicBool::new(false);
static AGAIN: AtomicBool = AtomicBool::new(false);

/// Works out the working tree's changes again in the background and shows
/// them in the panel (or removes it when nothing changed). Calls made while
/// one runs are folded into one more run.
pub(crate) fn refresh() {
    if !screen::active() || !enabled() {
        return;
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        AGAIN.store(true, Ordering::SeqCst);
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("claw-diff-panel".to_string())
        .spawn(|| {
            loop {
                AGAIN.store(false, Ordering::SeqCst);
                let files = std::env::current_dir()
                    .ok()
                    .and_then(|cwd| collect(&cwd))
                    .unwrap_or_default();
                show(files);
                if !AGAIN.load(Ordering::SeqCst) {
                    break;
                }
            }
            RUNNING.store(false, Ordering::SeqCst);
            // A call between the last check and now found RUNNING still set.
            if AGAIN.swap(false, Ordering::SeqCst) {
                refresh();
            }
        });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

fn show(files: Vec<FileDiff>) {
    if files.is_empty() {
        screen::set_side_panel("diff", None);
        return;
    }
    // Drawing a large diff takes a while: draw it here, in the background,
    // for the panel's current width, so the window only looks it up.
    let drawn: Mutex<Option<(usize, Vec<String>)>> = Mutex::new(None);
    let draw = move |width: usize| {
        let mut drawn = drawn.lock().unwrap_or_else(PoisonError::into_inner);
        match drawn.as_ref() {
            Some((drawn_for, rows)) if *drawn_for == width => rows.clone(),
            _ => {
                let rows = render(&files, width);
                *drawn = Some((width, rows.clone()));
                rows
            }
        }
    };
    if let Some(width) = screen::side_panel_width() {
        draw(width);
    }
    screen::set_side_panel("diff", Some(Arc::new(draw)));
}

fn git(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// The working tree's changes against `HEAD`, new files included; `None`
/// outside a git repository.
pub(crate) fn collect(cwd: &Path) -> Option<Vec<FileDiff>> {
    git(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    let base = if git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_some() {
        "HEAD"
    } else {
        EMPTY_TREE
    };
    let diff = git(
        cwd,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-U3",
            "-M",
            base,
            "--",
        ],
    )?;
    let mut files = parse_unified(&String::from_utf8_lossy(&diff));
    let untracked =
        git(cwd, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .unwrap_or_default();
    let prefix = git(cwd, &["rev-parse", "--show-prefix"])
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .unwrap_or_default();
    for path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        // claw's own sessions and settings are not the user's changes.
        .filter(|path| !path.starts_with(b".claw/") && !path.windows(7).any(|w| w == b"/.claw/"))
        .take(MAX_NEW_FILES)
    {
        let relative = String::from_utf8_lossy(path).to_string();
        files.push(new_file(
            &Path::new(&top).join(&prefix).join(&relative),
            format!("{prefix}{relative}"),
        ));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Some(files)
}

/// An untracked file: all of it added.
fn new_file(full_path: &Path, path: String) -> FileDiff {
    let readable = std::fs::metadata(full_path)
        .ok()
        .filter(|meta| meta.is_file() && meta.len() <= MAX_NEW_FILE_BYTES)
        .and_then(|_| std::fs::read(full_path).ok())
        .filter(|bytes| !bytes.contains(&0))
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let Some(content) = readable else {
        return FileDiff {
            path,
            added: 0,
            removed: 0,
            hunks: Vec::new(),
            note: Some("new"),
        };
    };
    let lines: Vec<String> = content.lines().map(|line| format!("+{line}")).collect();
    FileDiff {
        path,
        added: lines.len(),
        removed: 0,
        hunks: if lines.is_empty() {
            Vec::new()
        } else {
            vec![Hunk {
                old_start: 0,
                new_start: 1,
                lines,
            }]
        },
        note: Some("new"),
    }
}

/// Files and hunks from `git diff` output.
pub(crate) fn parse_unified(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(FileDiff {
                path: path_from_header(rest),
                added: 0,
                removed: 0,
                hunks: Vec::new(),
                note: None,
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(path) = line.strip_prefix("+++ ") {
            if path != "/dev/null" {
                file.path = strip_side(path);
            }
        } else if line.starts_with("--- ") {
            // The new path (or the header's) names the file.
        } else if line.starts_with("new file mode") {
            file.note = Some("new");
        } else if line.starts_with("deleted file mode") {
            file.note = Some("deleted");
        } else if line.starts_with("rename to ") {
            file.note = Some("renamed");
        } else if line.starts_with("Binary files ") {
            file.note = Some("binary");
        } else if let Some(header) = line.strip_prefix("@@ ") {
            let (old_start, new_start) = hunk_starts(header);
            file.hunks.push(Hunk {
                old_start,
                new_start,
                lines: Vec::new(),
            });
        } else if let Some(hunk) = file.hunks.last_mut() {
            match line.chars().next() {
                Some('+') => {
                    file.added += 1;
                    hunk.lines.push(line.to_string());
                }
                Some('-') => {
                    file.removed += 1;
                    hunk.lines.push(line.to_string());
                }
                Some(' ') => hunk.lines.push(line.to_string()),
                Some(_) => {} // "\ No newline at end of file"
                None => hunk.lines.push(" ".to_string()),
            }
        }
    }
    files
}

/// The new path from `a/old b/new` (quoted when it has odd characters).
fn path_from_header(rest: &str) -> String {
    let rest = rest.trim();
    let new = match rest.rfind(" b/") {
        Some(index) => &rest[index + 1..],
        None => rest,
    };
    strip_side(new)
}

fn strip_side(path: &str) -> String {
    let path = path.trim().trim_matches('"');
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

/// `-12,5 +14,7 @@ …` → (12, 14).
fn hunk_starts(header: &str) -> (usize, usize) {
    let mut parts = header.split_whitespace();
    let start = |part: Option<&str>, sign: char| {
        part.and_then(|part| part.strip_prefix(sign))
            .and_then(|part| part.split(',').next())
            .and_then(|number| number.parse().ok())
            .unwrap_or(1)
    };
    let old = start(parts.next(), '-');
    let new = start(parts.next(), '+');
    (old, new)
}

/// `left` and `right` on one row `width` columns wide, `left` cut short
/// (from its start, with `…`) when they do not fit.
fn spread(left: &str, right: &str, width: usize) -> String {
    let right_width = visible_width(right);
    let room = width.saturating_sub(right_width + 1);
    let left = if visible_width(left) > room {
        let chars: Vec<char> = left.chars().collect();
        let keep = room.saturating_sub(1);
        format!(
            "…{}",
            chars[chars.len().saturating_sub(keep)..]
                .iter()
                .collect::<String>()
        )
    } else {
        left.to_string()
    };
    let gap = width.saturating_sub(visible_width(&left) + right_width);
    format!("{left}{}{right}{RESET}", " ".repeat(gap))
}

fn counts(added: usize, removed: usize) -> String {
    match (added, removed) {
        (0, 0) => String::new(),
        (added, 0) => format!("{GREEN}+{added}{RESET}"),
        (0, removed) => format!("{RED}-{removed}{RESET}"),
        (added, removed) => format!("{GREEN}+{added}{RESET} {RED}-{removed}{RESET}"),
    }
}

/// The panel's rows, each `width` columns wide.
pub(crate) fn render(files: &[FileDiff], width: usize) -> Vec<String> {
    // One column of room on each side.
    let inner = width.saturating_sub(2);
    let row = |text: String| format!(" {text} ");
    let blank = " ".repeat(width);
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), file| (a + file.added, r + file.removed));
    let changed = if files.len() == 1 {
        "1 file changed".to_string()
    } else {
        format!("{} files changed", files.len())
    };
    let mut rows = vec![
        row(spread(
            &format!("{changed} {}", counts(added, removed)),
            &format!("{DIM}source: Current{RESET}"),
            inner,
        )),
        blank.clone(),
    ];
    for file in files {
        let note = file
            .note
            .filter(|note| *note != "new" || file.hunks.is_empty())
            .map(|note| format!(" {DIM}({note}){RESET}"))
            .unwrap_or_default();
        rows.push(row(spread(
            &format!("{}{note}", file.path),
            &counts(file.added, file.removed),
            inner,
        )));
    }
    for file in files {
        rows.push(blank.clone());
        rows.push(format!("{DIM}{}{RESET}", "─".repeat(width)));
        rows.push(row(spread(
            &format!("{BOLD}{}{RESET}", file.path),
            &counts(file.added, file.removed),
            inner,
        )));
        rows.push(format!("{DIM}{}{RESET}", "─".repeat(width)));
        if file.hunks.is_empty() {
            let what = match file.note {
                Some("binary") => "Binary file",
                Some("deleted") => "Deleted",
                Some("renamed") => "Renamed, content unchanged",
                _ => "No content to show",
            };
            rows.push(row(spread(&format!("{DIM}{what}{RESET}"), "", inner)));
            continue;
        }
        if rows.len() >= MAX_PANEL_ROWS {
            rows.push(row(spread(
                &format!("{DIM}… more files not shown{RESET}"),
                "",
                inner,
            )));
            break;
        }
        rows.extend(diff_view::render(
            &file.hunks,
            diff_view::Layout {
                width,
                indent: 1,
                path: &file.path,
                max_rows: Some(MAX_FILE_ROWS),
            },
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{parse_unified, render, visible_width};

    const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,4 +10,5 @@ fn main() {
 keep
-old
+new
+more
 tail
\\ No newline at end of file
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
";

    fn plain(row: &str) -> String {
        let mut out = String::new();
        let mut chars = row.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    #[test]
    fn parses_files_hunks_and_counts() {
        let files = parse_unified(DIFF);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!((files[0].added, files[0].removed), (2, 1));
        assert_eq!(files[0].hunks[0].old_start, 10);
        assert_eq!(files[0].hunks[0].lines.len(), 5);
        assert_eq!(files[1].path, "gone.txt");
        assert_eq!(files[1].note, Some("deleted"));
        assert_eq!(files[1].removed, 2);
        assert_eq!(files[2].note, Some("binary"));
    }

    #[test]
    fn renders_totals_file_list_and_diffs_at_the_panel_width() {
        let files = parse_unified(DIFF);
        let rows = render(&files, 60);
        let text: Vec<String> = rows.iter().map(|row| plain(row)).collect();
        assert!(text[0].starts_with(" 3 files changed +2 -3"));
        assert!(text[0].trim_end().ends_with("source: Current"));
        assert!(text[2].starts_with(" src/lib.rs") && text[2].trim_end().ends_with("+2 -1"));
        assert!(text.iter().any(|row| row.contains("11 -old")));
        assert!(text.iter().any(|row| row.contains("Binary file")));
        assert!(rows.iter().all(|row| visible_width(row) == 60), "{text:#?}");
    }
}
