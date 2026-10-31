//! Checks the turn loop makes on what the model does, each small and
//! deterministic:
//!
//! - **Repetition**: a reply stuck repeating one passage (local models do
//!   this) is not sent back with "continue", which only fed the loop.
//! - **Failing calls**: a call that keeps failing the same way, or a tool
//!   that keeps failing however it is called, gets a note to change
//!   approach, then is no longer run this turn. (Calls that keep *succeeding*
//!   with the same result are handled in `conversation.rs`.)
//! - **Verify before finishing**: a turn that edited code and is about to
//!   end without having run anything since gets one reminder to check it.

use std::collections::BTreeMap;

// ---------------------------------------------------------------- repetition

/// Below this the check does not run: short replies repeat words
/// legitimately.
const MIN_REPETITION_CHARS: usize = 400;
/// Exact-repeat window, far longer than ordinary reuse of a phrase.
const REPEAT_WINDOW: usize = 60;
const MIN_REPEAT_COUNT: usize = 5;
/// Repeats must cover at least this share of the text.
const DOMINANCE: f64 = 0.5;
/// A finished (not cut-off) reply is only treated as a loop at this size:
/// asked-for repetition ("print it 20 times") stays small.
pub const RUNAWAY_MIN_CHARS: usize = 16_000;
/// Only the latest this-many characters are judged, so the check stays
/// cheap however long the reply.
const JUDGED_TAIL_CHARS: usize = 64_000;

/// Said in place of a reply that degenerated into a loop: sending the loop
/// back would start it again.
pub const REPETITION_NOTE: &str =
    "[This reply degenerated into a repetition loop and was stopped; its text is not kept.]";

/// Whether `text` is dominated by a run of exact repeats (the shape of a
/// model stuck in a loop): one line or one 60+ character window repeated
/// over at least half of it, at least five times.
#[must_use]
pub fn is_repetition_dominated(text: &str) -> bool {
    let start = text.len().saturating_sub(JUDGED_TAIL_CHARS);
    let start = (start..=text.len())
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or(text.len());
    let text = &text[start..];
    let n = text.len();
    if n < MIN_REPETITION_CHARS {
        return false;
    }
    line_dominated(text) || window_dominated(text.as_bytes()) || periodic_dominated(text.as_bytes())
}

/// Stricter, for a reply that ended on its own: large, repetition
/// dominated, and (when it has lines) mostly copies of a few lines, so
/// distinct but similar rows (a table, SQL inserts) do not count.
#[must_use]
pub fn is_runaway_repetition(text: &str) -> bool {
    if text.len() < RUNAWAY_MIN_CHARS || !is_repetition_dominated(text) {
        return false;
    }
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() < 4 {
        return true;
    }
    let distinct = lines
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    (distinct as f64) <= lines.len() as f64 * 0.5
}

fn line_dominated(text: &str) -> bool {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for line in text.lines().map(str::trim).filter(|line| line.len() >= 8) {
        *counts.entry(line).or_default() += 1;
    }
    counts.iter().any(|(line, count)| {
        *count >= MIN_REPEAT_COUNT
            && (line.len() + 1) * count >= (text.len() as f64 * DOMINANCE) as usize
    })
}

fn window_dominated(bytes: &[u8]) -> bool {
    let n = bytes.len();
    if n < REPEAT_WINDOW {
        return false;
    }
    let needed =
        MIN_REPEAT_COUNT.max((n as f64 * DOMINANCE / REPEAT_WINDOW as f64).ceil() as usize);
    let mut counts: std::collections::HashMap<&[u8], usize> = std::collections::HashMap::new();
    for window in bytes.windows(REPEAT_WINDOW) {
        let count = counts.entry(window).or_default();
        *count += 1;
        if *count >= needed {
            return true;
        }
    }
    false
}

/// A long repeating unit whose 60-byte windows each recur too rarely for
/// [`window_dominated`]: from evenly spaced anchors, find a later copy,
/// take the distance as the period and grow the run both ways.
fn periodic_dominated(bytes: &[u8]) -> bool {
    const SAMPLES: usize = 32;
    const MATCHES_PER_ANCHOR: usize = 8;
    let n = bytes.len();
    let Some(max_start) = n.checked_sub(REPEAT_WINDOW).filter(|max| *max > 0) else {
        return false;
    };
    let step = (max_start / (SAMPLES - 1)).max(1);
    let mut starts = (0..=max_start).step_by(step).collect::<Vec<_>>();
    if starts.last() != Some(&max_start) {
        starts.push(max_start);
    }
    for start in starts {
        let anchor = &bytes[start..start + REPEAT_WINDOW];
        let mut from = start + 1;
        for _ in 0..MATCHES_PER_ANCHOR {
            let Some(found) = find(&bytes[from..], anchor).map(|at| at + from) else {
                break;
            };
            let period = found - start;
            from = found + 1;
            let mut left = start;
            while left > 0 && bytes[left - 1] == bytes[left - 1 + period] {
                left -= 1;
            }
            let mut right = start + REPEAT_WINDOW;
            while right + period < n && bytes[right] == bytes[right + period] {
                right += 1;
            }
            let run = right + period - left;
            if run >= MIN_REPEAT_COUNT * period && run as f64 >= n as f64 * DOMINANCE {
                return true;
            }
        }
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ------------------------------------------------------------- failing calls

/// The same call failing with the same input this many times gets a note.
const EXACT_FAILURE_WARN: u32 = 2;
/// ...and this many times is no longer run.
const EXACT_FAILURE_BLOCK: u32 = 4;
/// One tool failing this many times in a row (any input) gets a note.
const TOOL_FAILURE_WARN: u32 = 3;
/// ...and this many times is no longer run this turn.
const TOOL_FAILURE_BLOCK: u32 = 8;

/// What the failure guard says about a call about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallVerdict {
    Run,
    /// Not run; the reason is the tool result.
    Block(String),
}

/// Failures seen this turn.
#[derive(Debug, Default)]
pub struct FailureGuard {
    exact: BTreeMap<(String, String), u32>,
    /// Tool -> failures in a row.
    streak: BTreeMap<String, u32>,
}

impl FailureGuard {
    /// Whether the call should run.
    #[must_use]
    pub fn check(&self, tool: &str, input: &str) -> CallVerdict {
        let exact = self
            .exact
            .get(&(tool.to_string(), input.to_string()))
            .copied()
            .unwrap_or(0);
        if exact >= EXACT_FAILURE_BLOCK {
            return CallVerdict::Block(format!(
                "Not run: this exact {tool} call has already failed {exact} times this turn with \
                 the same input. Running it unchanged will fail again. Read the error, change the \
                 input or the approach, or tell the user what is blocking you."
            ));
        }
        let streak = self.streak.get(tool).copied().unwrap_or(0);
        if streak >= TOOL_FAILURE_BLOCK {
            return CallVerdict::Block(format!(
                "Not run: {tool} has failed {streak} times in a row this turn. Stop using it for \
                 this; use a different tool or approach, or tell the user what is failing."
            ));
        }
        CallVerdict::Run
    }

    /// Records a finished call; returns a note to add to its result when
    /// it is one failure too many.
    pub fn record(&mut self, tool: &str, input: &str, failed: bool) -> Option<String> {
        if !failed {
            self.streak.remove(tool);
            self.exact.remove(&(tool.to_string(), input.to_string()));
            return None;
        }
        let exact = self
            .exact
            .entry((tool.to_string(), input.to_string()))
            .or_default();
        *exact += 1;
        let exact = *exact;
        let streak = self.streak.entry(tool.to_string()).or_default();
        *streak += 1;
        let streak = *streak;
        if exact >= EXACT_FAILURE_WARN {
            Some(format!(
                "[claw: this exact call has now failed {exact} times. Do not repeat it unchanged: \
                 read the error above and change the input, or try another way.]"
            ))
        } else if streak >= TOOL_FAILURE_WARN {
            Some(format!(
                "[claw: {tool} has failed {streak} times in a row. {}]",
                failure_hint(tool)
            ))
        } else {
            None
        }
    }
}

fn failure_hint(tool: &str) -> &'static str {
    match tool {
        "edit_file" | "Edit" | "MultiEdit" => {
            "Read the current text of the file (read_file on the lines around the change) and \
             copy old_string from it exactly, or rewrite the section with write_file."
        }
        "read_file" | "Read" | "glob_search" | "grep_search" => {
            "Check the path: list the folder with glob_search, or search for the name, instead \
             of guessing."
        }
        "bash" => {
            "Look at what the errors have in common; check the command exists and the \
                   working directory is the one you expect."
        }
        "WebFetch" | "WebSearch" => {
            "The site may block automated reads; try another source or search for the facts \
             elsewhere."
        }
        _ => "Change approach instead of retrying the same way.",
    }
}

// -------------------------------------------------------- broad searches

/// The wide target a shell command searches recursively (the home
/// directory or the whole filesystem), if it does. Such a search ran 12
/// minutes in a test and brought back lines of other projects' chat
/// transcripts; it is almost never what a task needs.
#[must_use]
pub fn broad_search_target(command: &str) -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let home = home.trim_end_matches('/');
    let wide = |word: &str| {
        let word = word.trim_matches(['"', '\'']);
        let bare = word.trim_end_matches('/');
        matches!(
            word,
            "/" | "~" | "~/" | "$HOME" | "${HOME}" | "$HOME/" | "${HOME}/" | "/home" | "/home/"
        ) || (!home.is_empty() && bare == home)
    };
    command
        .split(['|', ';', '&', '\n', '(', ')'])
        .find_map(|part| {
            let words = part.split_whitespace().collect::<Vec<_>>();
            let program = words.first()?.rsplit('/').next()?;
            let recursive = match program {
                "grep" | "egrep" | "fgrep" => words.iter().any(|word| {
                    word.starts_with("--recursive")
                        || word.starts_with("--dereference-recursive")
                        || (word.starts_with('-')
                            && !word.starts_with("--")
                            && (word.contains('r') || word.contains('R')))
                }),
                "rg" | "ag" | "ack" | "find" | "fd" | "du" | "tree" => true,
                "ls" => words
                    .iter()
                    .any(|word| word.starts_with('-') && word.contains('R')),
                _ => false,
            };
            if !recursive {
                return None;
            }
            words[1..]
                .iter()
                .find(|word| wide(word))
                .map(|word| (*word).to_string())
        })
}

/// What the model is told instead of running a broad search the first
/// time it asks for one.
#[must_use]
pub fn broad_search_note(target: &str) -> String {
    format!(
        "Not run: this searches recursively through `{target}`, the whole home directory or \
         filesystem. That takes many minutes and reads unrelated private files (other projects, \
         chat transcripts, keys). Search the project, or the specific folders the task names. If \
         a search that wide is really needed, make the same call again and it will run."
    )
}

// ------------------------------------------------------- verify before finish

/// Edit tools whose input names the file they change.
const EDIT_TOOLS: [&str; 7] = [
    "edit_file",
    "write_file",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "apply_patch",
];
/// Tools that count as checking the work.
const CHECK_TOOLS: [&str; 6] = [
    "bash",
    "Diagnostics",
    "LSP",
    "REPL",
    "PowerShell",
    "ExecuteCode",
];
/// Files with nothing to run (documentation, notes, data).
const PROSE_EXTENSIONS: [&str; 12] = [
    "md", "markdown", "mdx", "rst", "txt", "adoc", "org", "log", "csv", "tsv", "json", "toml",
];

/// Tracks code edits and checks within a turn.
#[derive(Debug, Default)]
pub struct VerifyGuard {
    edited: Vec<String>,
    reminded: bool,
}

impl VerifyGuard {
    /// Records a call that ran successfully.
    pub fn record(&mut self, tool: &str, input: &str) {
        if CHECK_TOOLS.contains(&tool) {
            self.edited.clear();
        } else if EDIT_TOOLS.contains(&tool) {
            for path in edited_paths(input) {
                if is_code_path(&path) && !self.edited.contains(&path) {
                    self.edited.push(path);
                }
            }
        }
    }

    /// The reminder to send when the turn is about to end with code edited
    /// and not checked since; at most once a turn.
    pub fn reminder(&mut self) -> Option<String> {
        if self.reminded || self.edited.is_empty() {
            return None;
        }
        self.reminded = true;
        let files = self
            .edited
            .iter()
            .take(5)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "<system-reminder>\nYou changed code ({files}) and have not run anything since. \
             Before finishing, check the change if that can be done quickly: build or type-check \
             it, run the relevant tests, or run it. If it cannot be checked here, finish and say \
             plainly that it is not verified. Do not repeat your previous answer.\n\
             </system-reminder>"
        ))
    }
}

/// The files an edit call changes: its path field, or the files a Codex
/// patch adds or updates.
fn edited_paths(input: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return Vec::new();
    };
    if let Some(patch) = value.get("patch").and_then(serde_json::Value::as_str) {
        return patch
            .lines()
            .filter_map(|line| {
                line.strip_prefix("*** Update File: ")
                    .or_else(|| line.strip_prefix("*** Add File: "))
                    .or_else(|| line.strip_prefix("*** Move to: "))
            })
            .map(|path| path.trim().to_string())
            .collect();
    }
    ["path", "file_path", "notebook_path"]
        .iter()
        .find_map(|key| value.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .into_iter()
        .collect()
}

fn is_code_path(path: &str) -> bool {
    let path = std::path::Path::new(path);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.is_empty() || crate::memory::is_memory_path(&path.to_string_lossy()) {
        return false;
    }
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => !PROSE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
        None => !matches!(
            name.as_str(),
            "license" | "licence" | "notice" | "authors" | "changelog" | "codeowners"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_repetition_dominated, is_runaway_repetition, CallVerdict, FailureGuard, VerifyGuard,
    };

    #[test]
    fn loops_are_told_from_ordinary_text() {
        let looped = "I will now check the file again to be sure. ".repeat(40);
        assert!(is_repetition_dominated(&looped));
        let long_unit = format!(
            "{}\n",
            (0..30).map(|i| format!("step {i} ")).collect::<String>()
        )
        .repeat(8);
        assert!(is_repetition_dominated(&long_unit));
        let prose = (0..200)
            .map(|i| format!("Sentence number {i} says something different. "))
            .collect::<String>();
        assert!(!is_repetition_dominated(&prose));
        assert!(!is_repetition_dominated("short short short short short"));
        // Distinct rows that share a long prefix are not a runaway.
        let rows = (0..600)
            .map(|i| format!("INSERT INTO items (id, name, price) VALUES ({i}, 'item', 1);\n"))
            .collect::<String>();
        assert!(!is_runaway_repetition(&rows));
        assert!(is_runaway_repetition(&looped.repeat(10)));
    }

    #[test]
    fn repeated_failures_get_a_note_then_are_not_run() {
        let mut guard = FailureGuard::default();
        assert_eq!(guard.record("bash", "{\"command\":\"x\"}", true), None);
        assert!(guard.record("bash", "{\"command\":\"x\"}", true).is_some());
        guard.record("bash", "{\"command\":\"x\"}", true);
        assert_eq!(guard.check("bash", "{\"command\":\"x\"}"), CallVerdict::Run);
        guard.record("bash", "{\"command\":\"x\"}", true);
        assert!(matches!(
            guard.check("bash", "{\"command\":\"x\"}"),
            CallVerdict::Block(_)
        ));
        // Another input still runs; a success resets the tool's streak.
        assert_eq!(guard.check("bash", "{\"command\":\"y\"}"), CallVerdict::Run);
        guard.record("bash", "{\"command\":\"y\"}", false);
        assert_eq!(guard.record("bash", "{\"command\":\"z\"}", true), None);
    }

    #[test]
    fn searches_of_the_whole_home_or_filesystem_are_recognised() {
        use super::broad_search_target;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/u".to_string());
        assert_eq!(
            broad_search_target("grep -rniE 'registry' ~ /usr/local 2>/dev/null | head").as_deref(),
            Some("~")
        );
        assert_eq!(
            broad_search_target(&format!("find {home}/ -name '*.key'")).as_deref(),
            Some(format!("{home}/").as_str())
        );
        assert!(broad_search_target("rg token / --hidden").is_some());
        assert!(broad_search_target("cd /tmp && du -sh $HOME").is_some());
        // Searches inside the project, non-recursive greps, and home subfolders run.
        assert_eq!(broad_search_target("grep -rn fn src/"), None);
        assert_eq!(broad_search_target("grep -n x ~/notes.txt"), None);
        assert_eq!(
            broad_search_target(&format!("rg TODO {home}/ws/project")),
            None
        );
        assert_eq!(broad_search_target("ls -la ~"), None);
    }

    #[test]
    fn a_turn_that_edited_code_is_reminded_once_unless_it_ran_something() {
        let mut guard = VerifyGuard::default();
        guard.record("edit_file", r#"{"path":"README.md"}"#);
        assert_eq!(guard.reminder(), None);
        guard.record("edit_file", r#"{"path":"src/main.rs"}"#);
        guard.record("bash", r#"{"command":"cargo test"}"#);
        assert_eq!(guard.reminder(), None);
        guard.record("write_file", r#"{"path":"src/lib.rs"}"#);
        assert!(guard
            .reminder()
            .is_some_and(|text| text.contains("src/lib.rs")));
        assert_eq!(guard.reminder(), None);
    }
}
