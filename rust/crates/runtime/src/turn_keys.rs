//! Keyboard watcher for the time a turn is running: Esc cancels the reply,
//! a line finished with Enter is queued as the next prompt (like Claude
//! Code), and unfinished text is kept as type-ahead for the next prompt.
//!
//! While the watcher runs, a block is drawn right under the output, like
//! Claude Code: an animated activity row (what the agent is doing), then the
//! input box (a rule, the `❯` row with what is being typed, a rule) and the
//! status line (permission mode, model, context, background shells). It
//! follows the output down the screen; see the `live` module for how.
//!
//! The terminal stays in cooked output mode (only echo and line buffering are
//! turned off), so streamed output and Ctrl+C behave exactly as without the
//! watcher. Code that reads the keyboard mid-turn (approval prompts, questions
//! to the user) must hold a [`pause`] guard while it reads.
//!
//! Ported from `KeyListener` in `AzureAiHarness`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::HookAbortSignal;

#[cfg(unix)]
mod live;
#[cfg(unix)]
pub mod screen;
#[cfg(unix)]
mod vt;

/// Full-screen mode is Unix-only; elsewhere these do nothing.
#[cfg(not(unix))]
pub mod screen {
    use super::Scroll;

    pub struct ScreenGuard(());

    #[must_use]
    pub fn enter() -> Option<ScreenGuard> {
        None
    }
    #[must_use]
    pub fn active() -> bool {
        false
    }
    pub fn write_pane(bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        stdout.write_all(bytes)?;
        stdout.flush()
    }
    pub fn begin_pane() {}
    pub fn commit_pane() {}
    pub fn clear_pane() {}
    pub fn clear_conversation() {}
    pub fn scroll(_: Scroll) {}
    pub fn wheel(_: Scroll, _: usize, _: usize) {}
    pub type PanelRender = std::sync::Arc<dyn Fn(usize) -> Vec<String> + Send + Sync>;
    pub const SIDE_PANEL_MIN_COLS: usize = 110;
    pub fn set_side_panel(_: &str, _: Option<PanelRender>) {}
    pub fn set_side_panel_closed(_: bool) {}
    #[must_use]
    pub fn side_panel_closed() -> bool {
        false
    }
    #[must_use]
    pub fn conversation_cols() -> Option<usize> {
        None
    }
    #[must_use]
    pub fn side_panel_width() -> Option<usize> {
        None
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mouse {
        Press { row: usize, col: usize },
        Drag { row: usize, col: usize },
        Release { row: usize, col: usize },
    }
    pub fn mouse(_: Mouse) {}
    pub fn set_copy_handler(_: fn(&str) -> bool) {}
    pub fn focus_write(_: &[u8]) {}
    pub fn set_focus(_: bool) {}
    #[must_use]
    pub fn focus() -> bool {
        false
    }
    pub fn open_tasks_view(_: Option<&str>) -> bool {
        false
    }
    pub fn close_tasks_view() {}
    #[must_use]
    pub fn tasks_view_open() -> bool {
        false
    }
    pub fn tasks_view_step(_: isize) {}
    pub fn fold_begin() {}
    pub fn fold_full() {}
    pub fn fold_end() {}
    pub fn toggle_all_folds() {}
}

/// Moves the conversation in full-screen mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    /// Back (older) by this many rows.
    Up(usize),
    /// Forward (newer) by this many rows.
    Down(usize),
    PageUp,
    PageDown,
    Top,
    /// Back to the end; new output is followed again.
    Bottom,
}

/// Set while a watcher thread is running.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Asks the watcher to stop reading (see [`pause`]).
static PAUSED: AtomicBool = AtomicBool::new(false);
/// Set by the watcher once it has stopped reading after [`PAUSED`].
static PARKED: AtomicBool = AtomicBool::new(false);
static TYPE_AHEAD: Mutex<TypeAhead> = Mutex::new(TypeAhead::new());

/// The slash commands and their descriptions, for the `/` menu while
/// typing during a turn (see [`set_command_menu`]).
static COMMAND_MENU: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// Rows of the `/` menu shown at once.
const MENU_ROWS: usize = 8;

/// The commands the `/` menu offers while typing during a turn, as (name
/// without the slash, description), in the order to list them.
pub fn set_command_menu(commands: Vec<(String, String)>) {
    *COMMAND_MENU.lock().unwrap_or_else(|e| e.into_inner()) = commands;
}

/// The commands matching `text` (a `/` and part of a name, nothing after
/// it): exact name first, then names starting with it, then containing it.
fn menu_matches(text: &str, commands: &[(String, String)]) -> Vec<usize> {
    let Some(query) = text
        .strip_prefix('/')
        .filter(|query| !query.contains(char::is_whitespace))
    else {
        return Vec::new();
    };
    let query = query.to_ascii_lowercase();
    let mut ranked: Vec<(u8, usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, (name, _))| {
            let name = name.to_ascii_lowercase();
            let rank = if name == query {
                0
            } else if name.starts_with(&query) {
                1
            } else if name.contains(&query) {
                2
            } else {
                return None;
            };
            Some((rank, index))
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, index)| index).collect()
}

/// The `/` menu's rows under the input box, `width` columns wide.
fn menu_rows(width: usize) -> Vec<String> {
    let commands = COMMAND_MENU.lock().unwrap_or_else(|e| e.into_inner());
    let (matches, selected) = TYPE_AHEAD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .menu(&commands);
    if matches.is_empty() || width < 20 {
        return Vec::new();
    }
    let top = selected.saturating_sub(MENU_ROWS - 1);
    let visible = &matches[top..(top + MENU_ROWS).min(matches.len())];
    let name_width = visible
        .iter()
        .map(|&index| commands[index].0.chars().count())
        .max()
        .unwrap_or(0)
        .min(22)
        + 1;
    let mut rows: Vec<String> = visible
        .iter()
        .enumerate()
        .map(|(offset, &index)| {
            let (name, description) = &commands[index];
            let chosen = top + offset == selected;
            let line = format!(
                "{} /{name:<name_width$} {description}",
                if chosen { ">" } else { " " }
            );
            let line: String = line.chars().take(width).collect();
            let style = if chosen { "\x1b[36m" } else { "\x1b[38;5;245m" };
            format!("{style}{line}\x1b[0m")
        })
        .collect();
    if matches.len() > visible.len() {
        rows.push(format!(
            "\x1b[38;5;245m  ({}/{}, ↑↓ for more · tab completes)\x1b[0m",
            selected + 1,
            matches.len()
        ));
    }
    rows
}

/// Esc with the `/` menu open closes it; returns whether it was open.
fn close_command_menu() -> bool {
    let commands = COMMAND_MENU.lock().unwrap_or_else(|e| e.into_inner());
    let mut type_ahead = TYPE_AHEAD.lock().unwrap_or_else(|e| e.into_inner());
    if type_ahead.menu(&commands).0.is_empty() {
        return false;
    }
    type_ahead.menu_closed_for = Some(type_ahead.text.clone());
    true
}

/// Text typed during a turn and where its cursor is.
struct TypeAhead {
    text: String,
    /// Cursor position in chars, `0..=text.chars().count()`.
    cursor: usize,
    /// The `/` menu's selected row, and the text it was chosen for (a
    /// different text starts again at the top).
    menu_index: usize,
    menu_for: String,
    /// The text the menu was closed for with Esc.
    menu_closed_for: Option<String>,
}

impl TypeAhead {
    const fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            menu_index: 0,
            menu_for: String::new(),
            menu_closed_for: None,
        }
    }

    /// The `/` menu for what is typed: indexes into `commands`, best first,
    /// and the selected one's position. Empty while no menu shows.
    fn menu(&mut self, commands: &[(String, String)]) -> (Vec<usize>, usize) {
        if self.menu_closed_for.as_ref() == Some(&self.text) {
            return (Vec::new(), 0);
        }
        self.menu_closed_for = None;
        let matches = menu_matches(&self.text, commands);
        if self.menu_for != self.text {
            self.menu_for.clone_from(&self.text);
            self.menu_index = 0;
        }
        self.menu_index = self.menu_index.min(matches.len().saturating_sub(1));
        (matches, self.menu_index)
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Byte offset of char position `pos`.
    fn byte_at(&self, pos: usize) -> usize {
        self.text
            .char_indices()
            .nth(pos)
            .map_or(self.text.len(), |(index, _)| index)
    }

    fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}
/// Prompts submitted with Enter while a turn was running, oldest first.
static QUEUED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Set once something other than the key watcher (claw desktop's agent)
/// queues prompts for the running turn.
static EXTERNAL_QUEUE: AtomicBool = AtomicBool::new(false);
/// Answers `/btw` typed during a turn right away (set by the CLI); without
/// it the question waits in the queue until the turn ends.
static SIDE_QUESTION_HANDLER: OnceLock<fn(String)> = OnceLock::new();
/// Runs a slash command typed during a turn; see [`set_command_handler`].
static COMMAND_HANDLER: OnceLock<fn(&str) -> bool> = OnceLock::new();
/// Slash commands typed during a turn, for the watcher to run once it has
/// let go of the type-ahead.
static TYPED_COMMANDS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// `/btw` questions being answered while the turn runs.
static SIDE_PENDING: AtomicUsize = AtomicUsize::new(0);
/// Answered `/btw` questions waiting to be printed: (question, answer).
static SIDE_ANSWERS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static ACTIVITY: Mutex<Option<Activity>> = Mutex::new(None);
/// SGR sequence for the spinner and label (set from the CLI theme).
static ACTIVITY_COLOR: Mutex<String> = Mutex::new(String::new());
/// What the status line under the input box shows (set by the CLI).
static FOOTER: Mutex<Option<FooterStatus>> = Mutex::new(None);
/// Rows one mouse-wheel step scrolls the conversation.
pub const WHEEL_ROWS: usize = 3;

static WHEEL_STEP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(WHEEL_ROWS);

/// Rows one mouse-wheel step scrolls (`/scroll-speed`; default [`WHEEL_ROWS`]).
#[must_use]
pub fn wheel_rows() -> usize {
    WHEEL_STEP.load(Ordering::Relaxed)
}

/// Sets the rows one wheel step scrolls (1 to 20).
pub fn set_wheel_rows(rows: usize) {
    WHEEL_STEP.store(rows.clamp(1, 20), Ordering::Relaxed);
}
/// Shift+Tab presses during the current turn. The running turn keeps its
/// permission mode; the CLI applies the presses when the turn ends.
static MODE_PRESSES: AtomicU32 = AtomicU32::new(0);
/// The presses in MODE_PRESSES were made at the idle prompt, not during a
/// turn: they apply as soon as the prompt returns, so the footer shows the new
/// mode without "(after this reply)".
static MODE_PRESSES_IDLE: AtomicBool = AtomicBool::new(false);
/// Reads the terminal size as `(rows, cols)`; set by the CLI. Without it the
/// block is laid out for 80 columns.
static SIZE_PROBE: OnceLock<SizeProbe> = OnceLock::new();
/// Returns the terminal size as `(rows, cols)`.
pub type SizeProbe = fn() -> Option<(u16, u16)>;
/// Text counts as streaming while chunks arrived this recently.
const STREAMING_WINDOW: Duration = Duration::from_millis(1500);
/// After this much silence mid-reply, the activity row says why it may be quiet.
const SILENCE_NOTE_AFTER: Duration = Duration::from_secs(20);
/// Claude Code-style spinner, played forwards then backwards.
const SPINNER_FRAMES: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽"];
const SPINNER_FRAME_MS: u128 = 120;
/// The activity row's verb, picked once per turn (Claude Code style).
const VERBS: &[&str] = &[
    "Pondering",
    "Spelunking",
    "Tinkering",
    "Noodling",
    "Percolating",
    "Ruminating",
    "Crafting",
    "Conjuring",
    "Mulling",
    "Brewing",
    "Cogitating",
    "Wrangling",
    "Untangling",
    "Forging",
    "Scheming",
    "Deliberating",
    "Hatching",
    "Whirring",
];

/// The status line under the input box: the permission mode (coloured, e.g.
/// `⏵⏵ auto mode on`) followed by plain details (model, context use).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FooterStatus {
    pub mode: String,
    /// SGR sequence for `mode`; empty for no colour.
    pub mode_sgr: String,
    pub details: Vec<String>,
    /// The mode and its SGR after one, two, ... Shift+Tab presses, shown
    /// while presses made during a turn wait for the turn to end.
    pub cycle: Vec<(String, String)>,
}

/// Sets what the status line shows, idle and during a turn.
pub fn set_footer_status(status: FooterStatus) {
    *FOOTER.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
}

/// The status line for a terminal `width` columns wide, with the number of
/// running background shells added, cut to fit (details go first).
#[must_use]
pub fn footer_status_line(width: usize, colored: bool) -> String {
    let footer = FOOTER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    let mut details = footer.details.clone();
    let presses = MODE_PRESSES.load(Ordering::SeqCst) as usize;
    let (mode, mode_sgr) = if presses > 0 && !footer.cycle.is_empty() {
        let (mode, sgr) = &footer.cycle[(presses - 1) % footer.cycle.len()];
        if MODE_PRESSES_IDLE.load(Ordering::SeqCst) {
            (format!("{mode} (shift+tab to cycle)"), sgr.clone())
        } else {
            (format!("{mode} (after this reply)"), sgr.clone())
        }
    } else {
        (footer.mode.clone(), footer.mode_sgr.clone())
    };
    let shells = crate::global_background_jobs().running_ids().len();
    if shells > 0 {
        details.push(format!(
            "{shells} shell{}",
            if shells == 1 { "" } else { "s" }
        ));
    }
    compose_footer(&mode, &mode_sgr, &details, width, colored)
}

fn compose_footer(
    mode: &str,
    mode_sgr: &str,
    details: &[String],
    width: usize,
    colored: bool,
) -> String {
    let room = width.saturating_sub(3);
    let mode: String = mode.chars().take(room).collect();
    let mut used = mode.chars().count();
    let mut rest = String::new();
    for detail in details {
        let piece = if used == 0 {
            detail.clone()
        } else {
            format!(" · {detail}")
        };
        if used + piece.chars().count() > room {
            break;
        }
        used += piece.chars().count();
        rest.push_str(&piece);
    }
    if !colored {
        return format!("  {mode}{rest}");
    }
    let mode = if mode_sgr.is_empty() {
        mode
    } else {
        format!("{mode_sgr}{mode}\x1b[0m")
    };
    format!("  {mode}\x1b[2m{rest}\x1b[0m")
}

/// A dim horizontal rule across the terminal.
#[must_use]
pub fn rule_line(width: usize, colored: bool) -> String {
    let rule = "─".repeat(width.clamp(10, 400));
    if colored {
        format!("\x1b[{}m{rule}\x1b[0m", prompt_color_sgr())
    } else {
        rule
    }
}

/// The colors `/color` offers for the input box, as (name, 256-color SGR).
pub const PROMPT_COLORS: &[(&str, &str)] = &[
    ("red", "38;5;203"),
    ("blue", "38;5;75"),
    ("green", "38;5;114"),
    ("yellow", "38;5;221"),
    ("purple", "38;5;141"),
    ("orange", "38;5;215"),
    ("pink", "38;5;211"),
    ("cyan", "38;5;80"),
];
/// The input box color when none is chosen.
pub const DEFAULT_PROMPT_COLOR: &str = "green";

/// Index into [`PROMPT_COLORS`] of the session's input box color.
static PROMPT_COLOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(2);

/// Sets the input box color (`/color`); `false` for an unknown name.
pub fn set_prompt_color(name: &str) -> bool {
    let wanted = if name.eq_ignore_ascii_case("default") {
        DEFAULT_PROMPT_COLOR
    } else {
        name
    };
    match PROMPT_COLORS
        .iter()
        .position(|(known, _)| known.eq_ignore_ascii_case(wanted))
    {
        Some(index) => {
            PROMPT_COLOR.store(index, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// The input box color's name.
#[must_use]
pub fn prompt_color_name() -> &'static str {
    PROMPT_COLORS[PROMPT_COLOR.load(Ordering::Relaxed) % PROMPT_COLORS.len()].0
}

/// The SGR parameters of the input box color (rules and the `❯`).
#[must_use]
pub fn prompt_color_sgr() -> &'static str {
    PROMPT_COLORS[PROMPT_COLOR.load(Ordering::Relaxed) % PROMPT_COLORS.len()].1
}

fn pick_verb(now: Instant) -> &'static str {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos() as usize)
        ^ (now.elapsed().subsec_nanos() as usize);
    VERBS[seed % VERBS.len()]
}

/// What the agent is doing during the current turn.
#[derive(Debug, Clone)]
struct Activity {
    turn_started: Instant,
    /// The turn's verb, e.g. `Spelunking`.
    verb: &'static str,
    /// Set while a tool runs (or the model is streaming a tool call):
    /// the whole activity label, e.g. `Running bash · cargo test`.
    tool: Option<String>,
    /// Whether the current model request has produced visible text.
    request_output: bool,
    last_text: Option<Instant>,
    last_reasoning: Option<Instant>,
    /// Characters received from the model this turn (text and reasoning).
    chars: usize,
    /// How far a local server has read this request's prompt (done, total).
    prompt_progress: Option<(u64, u64)>,
}

impl Activity {
    fn new(now: Instant) -> Self {
        Self {
            turn_started: now,
            verb: pick_verb(now),
            tool: None,
            request_output: false,
            last_text: None,
            last_reasoning: None,
            chars: 0,
            prompt_progress: None,
        }
    }

    /// The label and an optional note, e.g. `("Working", Some("silent 45s …"))`.
    fn label(&self, now: Instant) -> (String, Option<String>) {
        if let Some(tool) = &self.tool {
            return (tool.clone(), None);
        }
        let recent = |at: Option<Instant>| at.is_some_and(|at| now - at < STREAMING_WINDOW);
        if recent(self.last_text) {
            return ("Writing".to_string(), None);
        }
        if let (false, Some((done, total))) = (self.request_output, self.prompt_progress) {
            if total > 0 && done < total {
                return (format!("Reading the prompt {}%", done * 100 / total), None);
            }
        }
        if recent(self.last_reasoning) || !self.request_output {
            return ("Thinking".to_string(), None);
        }
        let last = self.last_text.into_iter().chain(self.last_reasoning).max();
        let silent = last.map_or(Duration::ZERO, |at| now - at);
        let note = (silent >= SILENCE_NOTE_AFTER).then(|| {
            format!(
                "silent {} · the model may be writing a file",
                format_duration(silent)
            )
        });
        ("Working".to_string(), note)
    }

    /// The whole activity row, without colours: the turn's verb (or the
    /// running tool), then elapsed time, tokens received, what the model is
    /// doing and how to stop it, e.g. `Spelunking…` /
    /// `36s · ↓ 2.5k tokens · thinking · esc to interrupt`.
    fn render(&self, now: Instant) -> (String, String) {
        let (label, note) = self.label(now);
        let mut details = vec![format_duration(now - self.turn_started)];
        let tokens = self.chars / 4;
        if tokens > 0 {
            details.push(format!("↓ {} tokens", format_count(tokens)));
        }
        let headline = if self.tool.is_some() {
            label
        } else {
            details.push(label.to_lowercase());
            self.verb.to_string()
        };
        if let Some(note) = note {
            details.push(note);
        }
        details.push("esc to interrupt".to_string());
        (format!("{headline}…"), details.join(" · "))
    }
}

/// Shortens the label and details to fit `width` columns after the margin
/// and spinner: the label (cut with `…` if needed) first, then as many
/// details as fit.
fn fit_activity(label: &str, details: &str, width: usize) -> (String, String) {
    let room = width.saturating_sub(4);
    let label = if label.chars().count() > room {
        let mut cut = label
            .chars()
            .take(room.saturating_sub(1))
            .collect::<String>();
        cut.push('…');
        cut
    } else {
        label.to_string()
    };
    // " (" + ")" around the details.
    let left = room.saturating_sub(label.chars().count() + 3);
    let details = if left < 4 {
        String::new()
    } else {
        details.chars().take(left).collect()
    };
    (label, details)
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

fn format_count(count: usize) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        #[allow(clippy::cast_precision_loss)]
        let thousands = count as f64 / 1000.0;
        format!("{thousands:.1}k")
    }
}

fn spinner_frame(elapsed: Duration) -> &'static str {
    let cycle = 2 * SPINNER_FRAMES.len() - 2;
    #[allow(clippy::cast_possible_truncation)]
    let step = (elapsed.as_millis() / SPINNER_FRAME_MS) as usize % cycle;
    let index = if step < SPINNER_FRAMES.len() {
        step
    } else {
        cycle - step
    };
    SPINNER_FRAMES[index]
}

fn with_activity(update: impl FnOnce(&mut Activity)) {
    let mut activity = ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(activity) = activity.as_mut() {
        update(activity);
    }
}

/// Colours the activity row's spinner and label (an SGR escape sequence);
/// empty for no colour.
pub fn set_activity_color(sgr: &str) {
    *ACTIVITY_COLOR.lock().unwrap_or_else(|e| e.into_inner()) = sgr.to_string();
}

/// Whether the activity row is on screen, so callers can skip their own
/// inline "thinking" animation.
#[must_use]
pub fn status_rows_active() -> bool {
    #[cfg(unix)]
    {
        live::active() || screen::active()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether stdout is a terminal. During a turn stdout goes through a pipe to
/// the block under the output, but it still ends up on the terminal; check
/// this instead of `io::stdout().is_terminal()`.
#[must_use]
pub fn stdout_is_terminal() -> bool {
    status_rows_active() || std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// A new model request started: the agent is thinking until output arrives.
pub fn begin_model_request() {
    // A Ctrl+Enter during a tool step was for that step: its messages are
    // passed before this request, which is not cut off for them.
    clear_send_now();
    with_activity(|activity| {
        activity.tool = None;
        activity.request_output = false;
        activity.last_text = None;
        activity.last_reasoning = None;
        activity.prompt_progress = None;
    });
}

/// A local server has read `done` of the request's `total` prompt tokens.
pub fn note_prompt_progress(done: u64, total: u64) {
    with_activity(|activity| activity.prompt_progress = Some((done, total)));
}

/// Visible reply text arrived.
pub fn note_text(chars: usize) {
    with_activity(|activity| {
        activity.request_output = true;
        activity.last_text = Some(Instant::now());
        activity.chars += chars;
    });
}

/// Reasoning ("thinking") text arrived.
pub fn note_reasoning(chars: usize) {
    with_activity(|activity| {
        activity.request_output = true;
        activity.last_reasoning = Some(Instant::now());
        activity.chars += chars;
    });
}

/// Shows `label` (e.g. `Running bash · cargo test`) until [`end_tool`].
pub fn begin_tool(label: &str) {
    with_activity(|activity| activity.tool = Some(label.to_string()));
    draw_status_line();
}

/// The tool finished; the model is next.
pub fn end_tool() {
    with_activity(|activity| activity.tool = None);
}

/// Sets how the footer reads the terminal size, so it can follow resizes.
pub fn set_terminal_size_probe(probe: SizeProbe) {
    let _ = SIZE_PROBE.set(probe);
}

/// The terminal's size as `(rows, cols)`, when known.
fn terminal_size() -> Option<(u16, u16)> {
    SIZE_PROBE.get().and_then(|probe| probe())
}

/// The terminal's width in columns (80 when unknown).
fn terminal_cols() -> usize {
    SIZE_PROBE
        .get()
        .and_then(|probe| probe())
        .map_or(80, |(_, cols)| usize::from(cols))
}

/// Takes the Shift+Tab presses made during the last turn.
#[must_use]
pub fn take_mode_presses() -> u32 {
    MODE_PRESSES_IDLE.store(false, Ordering::SeqCst);
    MODE_PRESSES.swap(0, Ordering::SeqCst)
}

/// Shift+Tab at the idle prompt: counted like a press during a turn, so the
/// line editor only redraws its status line; the REPL applies the presses
/// when the prompt returns ([`take_mode_presses`]).
pub fn note_idle_mode_press() {
    MODE_PRESSES_IDLE.store(true, Ordering::SeqCst);
    MODE_PRESSES.fetch_add(1, Ordering::SeqCst);
}

/// Takes the unfinished text typed while the last turn was running.
#[must_use]
pub fn take_type_ahead() -> String {
    TYPE_AHEAD.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Takes the oldest prompt queued with Enter during a turn.
#[must_use]
pub fn take_queued() -> Option<String> {
    let mut queued = QUEUED.lock().unwrap_or_else(|e| e.into_inner());
    (!queued.is_empty()).then(|| queued.remove(0))
}

/// How many prompts are waiting to run.
#[must_use]
pub fn queued_count() -> usize {
    QUEUED.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Takes every queued prompt so the running turn can pass it to the model
/// before its next step. Empty unless a watcher is running (interactive
/// REPL turns only), so other callers of the conversation loop are unaffected.
#[must_use]
pub fn take_queued_for_running_turn() -> Vec<String> {
    if !ACTIVE.load(Ordering::SeqCst) && !EXTERNAL_QUEUE.load(Ordering::SeqCst) {
        return Vec::new();
    }
    #[cfg(test)]
    if !simulating_here() {
        return Vec::new();
    }
    let mut all = QUEUED.lock().unwrap_or_else(|e| e.into_inner());
    // A `/btw` side question is not for the running task: it stays queued
    // and is answered on its own when the turn ends.
    let (side, queued): (Vec<String>, Vec<String>) =
        std::mem::take(&mut *all).into_iter().partition(|line| {
            is_btw(line) || (COMMAND_HANDLER.get().is_some() && is_slash_command(line))
        });
    *all = side;
    drop(all);
    if !queued.is_empty() {
        draw_status_line();
    }
    queued
}

/// Queues `text` for the running turn from outside the terminal (claw
/// desktop): it reaches the model before its next step, like a line typed
/// with Enter during a REPL turn.
pub fn queue_for_running_turn(text: &str) {
    EXTERNAL_QUEUE.store(true, Ordering::SeqCst);
    QUEUED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(text.trim().to_string());
}

/// Lets `/btw` typed during a turn be answered at once by `handler`
/// (which gets the question and must return quickly, answering on its own
/// thread and handing the answer to [`push_side_answer`]), like Claude Code.
pub fn set_side_question_handler(handler: fn(String)) {
    let _ = SIDE_QUESTION_HANDLER.set(handler);
}

/// Lets slash commands typed during a turn run at once, like Claude Code:
/// `handler` runs the ones that can (it gets the line, on the key watcher's
/// thread, so it must be quick) and returns false for the ones that must
/// wait for the turn to end. Those stay queued, run as commands when the
/// turn ends, and are never passed to the model.
pub fn set_command_handler(handler: fn(&str) -> bool) {
    let _ = COMMAND_HANDLER.set(handler);
}

/// Whether `line` is a slash command (`/name …`), not a path or a message.
fn is_slash_command(line: &str) -> bool {
    let name = line.split_whitespace().next().unwrap_or_default();
    name.strip_prefix('/').is_some_and(|name| {
        name.starts_with(|ch: char| ch.is_ascii_alphabetic()) && !name.contains('/')
    })
}

/// Runs the slash commands typed since the last call (see
/// [`set_command_handler`]).
fn run_typed_commands() {
    let commands = std::mem::take(&mut *TYPED_COMMANDS.lock().unwrap_or_else(|e| e.into_inner()));
    let Some(handler) = COMMAND_HANDLER.get() else {
        return;
    };
    for command in commands {
        if !handler(&command) {
            let mut stdout = std::io::stdout().lock();
            let _ = write!(
                stdout,
                "\n\x1b[1;36m> \x1b[0m{command}  \x1b[2m(runs when the reply ends)\x1b[0m\n"
            );
            let _ = stdout.flush();
            drop(stdout);
            QUEUED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(command);
        }
    }
    draw_status_line();
}

/// Hands over the answer to a `/btw` asked during the turn. It is printed
/// as soon as it arrives while a turn runs or claw draws the window;
/// otherwise at the next prompt ([`print_side_answers`]).
pub fn push_side_answer(question: String, answer: String) {
    SIDE_ANSWERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((question, answer));
    if ACTIVE.load(Ordering::SeqCst) || screen::active() {
        print_side_answers();
    }
    SIDE_PENDING
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        })
        .ok();
    draw_status_line();
}

/// Prints the `/btw` answers that are ready (between the turn's steps and
/// after it, so they never cut into streamed output).
pub fn print_side_answers() {
    let answers = std::mem::take(&mut *SIDE_ANSWERS.lock().unwrap_or_else(|e| e.into_inner()));
    if answers.is_empty() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    for (question, answer) in answers {
        let _ = write!(
            stdout,
            "\n\x1b[2m/btw {question}\x1b[0m\n{answer}\n\x1b[2m(side question, not added to the conversation)\x1b[0m\n"
        );
    }
    let _ = stdout.flush();
}

/// Waits up to `limit` for `/btw` answers still being written, so a side
/// question asked near the end of a turn is shown right after it.
pub fn wait_for_side_answers(limit: Duration) {
    if SIDE_PENDING.load(Ordering::SeqCst) > 0 {
        println!("\x1b[2m  waiting for the /btw answer…\x1b[0m");
    }
    let deadline = Instant::now() + limit;
    while SIDE_PENDING.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}

/// The question of a `/btw` line (`None` for other lines and a bare `/btw`).
#[must_use]
pub fn btw_question(line: &str) -> Option<&str> {
    line.trim_start()
        .strip_prefix("/btw ")
        .map(str::trim)
        .filter(|question| !question.is_empty())
}

fn is_btw(line: &str) -> bool {
    let line = line.trim_start();
    line == "/btw" || line.starts_with("/btw ")
}

/// Test support: queues `text` as if typed during a turn with a watcher
/// running. Returns a guard that ends the simulated turn when dropped.
#[cfg(test)]
static SIMULATING_THREAD: Mutex<Option<thread::ThreadId>> = Mutex::new(None);

/// Test support: tests run in parallel and share the queue, so only the
/// thread that simulates a turn gets its prompts.
#[cfg(test)]
fn simulating_here() -> bool {
    *SIMULATING_THREAD.lock().unwrap_or_else(|e| e.into_inner()) == Some(thread::current().id())
}

#[cfg(test)]
pub(crate) fn simulate_queued_during_turn(text: &str) -> SimulatedTurn {
    let exclusive = exclusive_queue();
    *SIMULATING_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread::current().id());
    ACTIVE.store(true, Ordering::SeqCst);
    QUEUED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(text.to_string());
    SimulatedTurn(exclusive)
}

/// Test support: the queue is global, so a test running many tool steps
/// holds this to keep a simulated prompt from landing in its turn.
#[cfg(test)]
pub(crate) fn exclusive_queue() -> std::sync::MutexGuard<'static, ()> {
    static QUEUE_TESTS: Mutex<()> = Mutex::new(());
    QUEUE_TESTS.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
pub(crate) struct SimulatedTurn(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl Drop for SimulatedTurn {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::SeqCst);
        *SIMULATING_THREAD.lock().unwrap_or_else(|e| e.into_inner()) = None;
        QUEUED.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

/// Shows a queued prompt that was just passed to the running turn.
pub fn announce_sent_to_running_turn(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = write!(
        stdout,
        "\n\x1b[1;36m> \x1b[0m{text}  \x1b[2m(sent to the agent)\x1b[0m\n"
    );
    let _ = stdout.flush();
}

/// Draws the block under the output again (a no-op outside a turn).
fn draw_status_line() {
    #[cfg(unix)]
    {
        live::redraw();
        screen::mark_dirty();
    }
}

/// The block's rows for a `cols`-wide terminal: a blank row, the animated
/// activity row, the input box (a rule, what is being typed with a cursor, a
/// rule) and the status line, each cut to fit.
fn block_rows(cols: usize) -> Vec<String> {
    let cols = cols.max(20);
    let width = cols - 1;
    let color = ACTIVITY_COLOR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let colored = !color.is_empty();
    let activity_line = ACTIVITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|activity| {
            let now = Instant::now();
            let (label, details) = activity.render(now);
            let frame = spinner_frame(now - activity.turn_started);
            let (label, details) = fit_activity(&label, &details, width);
            if details.is_empty() {
                format!("{color}{frame} {label}\x1b[0m")
            } else {
                format!("{color}{frame} {label}\x1b[0m \x1b[2m({details})\x1b[0m")
            }
        })
        .unwrap_or_default();
    let (typed, cursor) = {
        let type_ahead = TYPE_AHEAD.lock().unwrap_or_else(|e| e.into_inner());
        (type_ahead.text.clone(), type_ahead.cursor)
    };
    let queued = queued_count();
    let mut queued_note = if queued > 0 {
        format!("  [{queued} queued · ↑ to edit]")
    } else {
        String::new()
    };
    match SIDE_PENDING.load(Ordering::SeqCst) {
        0 => {}
        1 => queued_note.push_str("  [answering /btw…]"),
        n => queued_note.push_str(&format!("  [answering {n} /btw…]")),
    }
    let input_line = if typed.is_empty() {
        format!(
            "\x1b[1;{}m❯\x1b[0m \x1b[7m \x1b[0m\x1b[2mtype to queue a message for the agent{queued_note}\x1b[0m",
            prompt_color_sgr()
        )
    } else {
        format!(
            "\x1b[1;{}m❯\x1b[0m {}\x1b[2m{queued_note}\x1b[0m",
            prompt_color_sgr(),
            type_ahead_field(
                &typed,
                cursor,
                width.saturating_sub(2 + queued_note.chars().count())
            )
        )
    };
    let rule = rule_line(width, colored);
    let mut rows = vec![String::new(), activity_line, rule.clone(), input_line, rule];
    // The `/` menu while one is being typed, otherwise the status line,
    // as at the prompt.
    let menu = menu_rows(width);
    if menu.is_empty() {
        rows.push(footer_status_line(width, colored));
    } else {
        rows.extend(menu);
    }
    rows
}

/// `typed` in a field `room` cells wide, with the cursor drawn in reverse
/// video. Long input scrolls like a text field: the end shows unless the
/// cursor is further left. Line breaks (between queued messages pulled back
/// with Up) show as `↵`, so the field stays one row.
fn type_ahead_field(typed: &str, cursor: usize, room: usize) -> String {
    let chars = typed
        .chars()
        .map(|ch| if ch == '\n' { '↵' } else { ch })
        .collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    // One more cell than the text, for the cursor after its last char.
    let room = room.max(1);
    let start = (chars.len() + 1).saturating_sub(room).min(cursor);
    let end = (start + room).min(chars.len() + usize::from(cursor == chars.len()));
    let mut field = String::new();
    for (pos, ch) in (start..end).map(|pos| (pos, chars.get(pos).copied().unwrap_or(' '))) {
        if pos == cursor {
            field.push_str(&format!("\x1b[7m{ch}\x1b[0m"));
        } else {
            field.push(ch);
        }
    }
    field
}

/// What was read from the keyboard during a turn.
#[derive(Debug, Default, PartialEq, Eq)]
struct Keys {
    /// Typed text (escape sequences removed).
    text: Vec<u8>,
    /// Shift+Tab presses (`ESC [ Z`).
    back_tabs: u32,
    /// PgUp/PgDn, Ctrl+Home/Ctrl+End.
    scrolls: Vec<Scroll>,
    /// Mouse wheel turns, with the (row, col) of the pointer.
    wheels: Vec<(Scroll, usize, usize)>,
    /// Left-button presses, drags and releases (text selection).
    mice: Vec<screen::Mouse>,
    /// Ctrl+O presses: expand or collapse every fold.
    fold_toggles: u32,
    /// Esc presses sent as `ESC [ 27 u` (kitty keyboard protocol).
    escapes: u32,
    /// Ctrl+C presses sent as `ESC [ 99 ; 5 u`: with the protocol on they
    /// do not raise SIGINT, so they stop the reply here.
    interrupts: u32,
}

/// The key a kitty keyboard protocol report (`ESC [ code ; mods u`) stands
/// for, as the bytes the legacy encoding would have sent; `None` to drop it.
fn kitty_key(params: &[u8], keys: &mut Keys) -> Option<u8> {
    let params = std::str::from_utf8(params).ok()?;
    let mut fields = params.split(';');
    let code: u32 = fields.next()?.split(':').next()?.parse().ok()?;
    let mods: u32 = fields
        .next()
        .and_then(|mods| mods.split(':').next()?.parse().ok())
        .unwrap_or(1);
    let modifiers = mods.saturating_sub(1);
    let (shift, alt, ctrl) = (modifiers & 1 != 0, modifiers & 2 != 0, modifiers & 4 != 0);
    match code {
        27 if modifiers == 0 => {
            keys.escapes += 1;
            None
        }
        13 if ctrl && !alt => Some(CTRL_ENTER),
        13 if !alt => Some(b'\r'),
        9 if shift && !ctrl && !alt => {
            keys.back_tabs += 1;
            None
        }
        _ if ctrl && !alt && code == u32::from(b'c') => {
            keys.interrupts += 1;
            None
        }
        // Ctrl+letter: the control byte the editing keys are read from.
        _ if ctrl && !alt && (u32::from(b'a')..=u32::from(b'z')).contains(&code) => {
            u8::try_from(code).ok().map(|letter| letter & 0x1f)
        }
        _ => None,
    }
}

/// Splits what was read from the keyboard into typed text, Shift+Tab
/// presses and scrolling. Up/Down, Left/Right, Home/End and Delete stay in
/// the text, in order, as the Ctrl+P/N/B/F/A/E/D bytes that
/// [`record_type_ahead`] reads as editing keys; other escape sequences (function keys,
/// Alt+key, modified arrows, mouse clicks) are dropped.
fn split_keys(bytes: &[u8]) -> Keys {
    let mut keys = Keys::default();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == CTRL_O {
            keys.fold_toggles += 1;
            i += 1;
            continue;
        }
        if bytes[i] != 0x1b {
            keys.text.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'[' | b'O') => {
                // CSI/SS3: parameters, then a final byte in 0x40..=0x7e.
                let mut end = i + 2;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                let params = &bytes[i + 2..end.min(bytes.len())];
                // Normal and application cursor mode (`ESC O D`), and the
                // `ESC [ n ~` forms of Home/End/Delete.
                let csi = bytes[i + 1] == b'[';
                let cursor_key = match (params, bytes.get(end)) {
                    (b"", Some(b'A')) => Some(CTRL_P),
                    (b"", Some(b'B')) => Some(CTRL_N),
                    (b"", Some(b'D')) => Some(CTRL_B),
                    (b"", Some(b'C')) => Some(CTRL_F),
                    (b"", Some(b'H')) => Some(CTRL_A),
                    (b"", Some(b'F')) => Some(CTRL_E),
                    (b"1" | b"7", Some(b'~')) if csi => Some(CTRL_A),
                    (b"4" | b"8", Some(b'~')) if csi => Some(CTRL_E),
                    (b"3", Some(b'~')) if csi => Some(CTRL_D),
                    _ => None,
                };
                if let Some(byte) = cursor_key {
                    keys.text.push(byte);
                } else if csi && bytes.get(end) == Some(&b'u') {
                    if let Some(byte) = kitty_key(params, &mut keys) {
                        if byte == CTRL_O {
                            keys.fold_toggles += 1;
                        } else {
                            keys.text.push(byte);
                        }
                    }
                } else if csi {
                    match (params, bytes.get(end)) {
                        (b"", Some(b'Z')) => keys.back_tabs += 1,
                        (b"5", Some(b'~')) => keys.scrolls.push(Scroll::PageUp),
                        (b"6", Some(b'~')) => keys.scrolls.push(Scroll::PageDown),
                        (b"1;5", Some(b'H')) => keys.scrolls.push(Scroll::Top),
                        (b"1;5", Some(b'F')) => keys.scrolls.push(Scroll::Bottom),
                        // SGR mouse report: `<button;x;y`, then M (press or
                        // motion) or m (release).
                        ([b'<', rest @ ..], Some(&kind @ (b'M' | b'm'))) => {
                            if let Some(event) = parse_sgr_mouse(rest, kind == b'm') {
                                match event {
                                    SgrMouse::Wheel(scroll, row, col) => {
                                        keys.wheels.push((scroll, row, col));
                                    }
                                    SgrMouse::Left(mouse) => keys.mice.push(mouse),
                                }
                            }
                        }
                        _ => {}
                    }
                }
                i = end + 1;
            }
            Some(_) => i += 2,
            None => i += 1,
        }
    }
    keys
}

enum SgrMouse {
    /// A wheel turn and the pointer's (row, col).
    Wheel(Scroll, usize, usize),
    Left(screen::Mouse),
}

/// Reads the `button;x;y` of an SGR mouse report (1-based cells). Events
/// with Shift, Alt or Ctrl held are left to the terminal.
fn parse_sgr_mouse(params: &[u8], release: bool) -> Option<SgrMouse> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';').map(str::parse::<usize>);
    let (button, col, row) = (
        fields.next()?.ok()?,
        fields.next()?.ok()?,
        fields.next()?.ok()?,
    );
    if button & 0b1_1100 != 0 {
        return None;
    }
    let (row, col) = (row.saturating_sub(1), col.saturating_sub(1));
    Some(match (button, release) {
        (64, false) => SgrMouse::Wheel(Scroll::Up(wheel_rows()), row, col),
        (65, false) => SgrMouse::Wheel(Scroll::Down(wheel_rows()), row, col),
        (0, false) => SgrMouse::Left(screen::Mouse::Press { row, col }),
        (32, false) => SgrMouse::Left(screen::Mouse::Drag { row, col }),
        (0, true) => SgrMouse::Left(screen::Mouse::Release { row, col }),
        _ => return None,
    })
}

/// Moves queued prompts back into the type-ahead, so they are pre-filled in
/// the input instead of running (used when the user stops a reply with Esc).
pub fn unqueue_into_type_ahead() {
    pull_queued_into(&mut TYPE_AHEAD.lock().unwrap_or_else(|e| e.into_inner()));
}

/// Watches the keyboard until dropped. Does nothing when stdin is not a
/// terminal or on non-Unix platforms.
pub struct TurnKeyWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    #[cfg(unix)]
    terminal: Option<imp::TerminalModes>,
}

impl TurnKeyWatcher {
    /// Starts watching; Esc calls `abort.abort()`.
    #[must_use]
    pub fn start(abort: HookAbortSignal) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        {
            if let Some(terminal) = imp::TerminalModes::enter_watch_mode() {
                ACTIVE.store(true, Ordering::SeqCst);
                PAUSED.store(false, Ordering::SeqCst);
                let thread_stop = Arc::clone(&stop);
                let handle = thread::Builder::new()
                    .name("claw-turn-keys".to_string())
                    .spawn(move || imp::watch(&thread_stop, &abort))
                    .ok();
                return Self {
                    stop,
                    handle,
                    terminal: Some(terminal),
                };
            }
            Self {
                stop,
                handle: None,
                terminal: None,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = abort;
            Self { stop, handle: None }
        }
    }
}

impl Drop for TurnKeyWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        ACTIVE.store(false, Ordering::SeqCst);
        PAUSED.store(false, Ordering::SeqCst);
        #[cfg(unix)]
        if let Some(terminal) = self.terminal.take() {
            terminal.restore_original();
        }
    }
}

/// Whether a turn is running with the key watcher on the terminal (so
/// [`pause`] hands the keyboard over), not the idle prompt's line editor.
#[must_use]
pub fn watching() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Suspends the watcher (and restores normal terminal input) while the guard
/// lives, so the holder can read the keyboard itself.
#[must_use]
pub fn pause() -> PauseGuard {
    if !ACTIVE.load(Ordering::SeqCst) {
        return PauseGuard { active: false };
    }
    PARKED.store(false, Ordering::SeqCst);
    PAUSED.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_millis(500);
    while !PARKED.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    #[cfg(unix)]
    {
        live::pause();
        screen::pause_turn();
        imp::TerminalModes::apply_original();
    }
    PauseGuard { active: true }
}

pub struct PauseGuard {
    active: bool,
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        if self.active && ACTIVE.load(Ordering::SeqCst) {
            #[cfg(unix)]
            {
                imp::TerminalModes::apply_watch();
                live::resume();
                screen::resume_turn();
            }
            PAUSED.store(false, Ordering::SeqCst);
        }
    }
}

/// Cursor keys in the type-ahead, as the terminal sends them for Ctrl+letter
/// (and as [`split_keys`] turns arrows, Home/End and Delete into).
const CTRL_A: u8 = 0x01;
const CTRL_B: u8 = 0x02;
const CTRL_D: u8 = 0x04;
const CTRL_E: u8 = 0x05;
const CTRL_F: u8 = 0x06;
const CTRL_K: u8 = 0x0b;
/// Expands or collapses every fold (see [`screen::toggle_all_folds`]).
const CTRL_O: u8 = 0x0f;
/// Up (as Ctrl+P): pull the queued messages back to edit them.
const CTRL_P: u8 = 0x10;
/// Down (as Ctrl+N): the next row of the `/` menu.
const CTRL_N: u8 = 0x0e;
const CTRL_U: u8 = 0x15;
/// Ctrl+Enter, as [`split_keys`] passes it on from the kitty keyboard
/// protocol's `ESC [ 13 ; 5 u` (the legacy encoding is a plain Enter).
const CTRL_ENTER: u8 = 0x1e;

/// The terminal speaks the kitty keyboard protocol (the prompt found out):
/// the watcher turns it on during a turn so Ctrl+Enter can be told from Enter.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
/// Ctrl+Enter was pressed with messages queued: the reply being streamed is
/// cut off so the agent reads them now instead of after its step.
static SEND_NOW: AtomicBool = AtomicBool::new(false);

/// Records whether the terminal supports the kitty keyboard protocol.
pub fn set_keyboard_enhancement(supported: bool) {
    KEYBOARD_ENHANCED.store(supported, Ordering::SeqCst);
}

/// Whether Ctrl+Enter asked for the streaming reply to be cut off now.
#[must_use]
pub fn send_now_requested() -> bool {
    SEND_NOW.load(Ordering::SeqCst)
}

/// Clears a Ctrl+Enter request (it is for the reply that was streaming).
pub fn clear_send_now() {
    SEND_NOW.store(false, Ordering::SeqCst);
}

/// Moves every queued message, oldest first and one per line, in front of
/// what is being typed, with the cursor after them. Returns whether any were
/// queued. Messages the running turn has already taken are gone from the
/// queue and stay sent.
fn pull_queued_into(type_ahead: &mut TypeAhead) -> bool {
    let queued = std::mem::take(&mut *QUEUED.lock().unwrap_or_else(|e| e.into_inner()));
    if queued.is_empty() {
        return false;
    }
    let mut text = queued.join("\n");
    let cursor = text.chars().count();
    if !type_ahead.text.is_empty() {
        text.push('\n');
        text.push_str(&type_ahead.text);
    }
    type_ahead.text = text;
    type_ahead.cursor = cursor;
    true
}

/// Applies typed bytes: text goes into the type-ahead at its cursor, Enter
/// queues it, Up pulls queued messages back into it, and the cursor keys
/// (Ctrl+A/E/B/F, Ctrl+D deletes forward, Ctrl+U/K delete to the start/end)
/// edit it like the prompt.
fn record_type_ahead(bytes: &[u8]) {
    let commands = COMMAND_MENU.lock().unwrap_or_else(|e| e.into_inner());
    let mut type_ahead = TYPE_AHEAD.lock().unwrap_or_else(|e| e.into_inner());
    for ch in String::from_utf8_lossy(bytes).chars() {
        let control = u8::try_from(u32::from(ch)).ok();
        // The `/` menu takes ↑/↓, Tab and Enter while it is open.
        let enter = matches!(ch, '\r' | '\n') || control == Some(CTRL_ENTER);
        let (menu, selected) = if ch == '\t'
            || enter
            || control.is_some_and(|byte| byte == CTRL_P || byte == CTRL_N)
        {
            type_ahead.menu(&commands)
        } else {
            (Vec::new(), 0)
        };
        if !menu.is_empty() {
            let name = &commands[menu[selected]].0;
            match ch {
                _ if control == Some(CTRL_P) => {
                    type_ahead.menu_index = (selected + menu.len() - 1) % menu.len();
                    continue;
                }
                _ if control == Some(CTRL_N) => {
                    type_ahead.menu_index = (selected + 1) % menu.len();
                    continue;
                }
                '\t' => {
                    type_ahead.text = format!("/{name} ");
                    type_ahead.cursor = type_ahead.len();
                    continue;
                }
                // Enter runs the selected command, as at the prompt.
                _ => {
                    type_ahead.text = format!("/{name}");
                }
            }
        }
        match ch {
            // At the start (Backspace) or end (Delete) they do nothing.
            '\u{7f}' | '\u{8}' if type_ahead.cursor > 0 => {
                type_ahead.cursor -= 1;
                let at = type_ahead.byte_at(type_ahead.cursor);
                type_ahead.text.remove(at);
            }
            _ if control == Some(CTRL_D) && type_ahead.cursor < type_ahead.len() => {
                let at = type_ahead.byte_at(type_ahead.cursor);
                type_ahead.text.remove(at);
            }
            _ if control == Some(CTRL_A) => type_ahead.cursor = 0,
            _ if control == Some(CTRL_E) => type_ahead.cursor = type_ahead.len(),
            _ if control == Some(CTRL_B) => {
                type_ahead.cursor = type_ahead.cursor.saturating_sub(1);
            }
            _ if control == Some(CTRL_F) => {
                type_ahead.cursor = (type_ahead.cursor + 1).min(type_ahead.len());
            }
            _ if control == Some(CTRL_U) => {
                let at = type_ahead.byte_at(type_ahead.cursor);
                type_ahead.text.drain(..at);
                type_ahead.cursor = 0;
            }
            _ if control == Some(CTRL_K) => {
                let at = type_ahead.byte_at(type_ahead.cursor);
                type_ahead.text.truncate(at);
            }
            _ if control == Some(CTRL_P) => {
                pull_queued_into(&mut type_ahead);
            }
            _ if enter => {
                let line = type_ahead.take();
                if let (Some(question), Some(handler)) =
                    (btw_question(&line), SIDE_QUESTION_HANDLER.get())
                {
                    SIDE_PENDING.fetch_add(1, Ordering::SeqCst);
                    handler(question.to_string());
                } else if COMMAND_HANDLER.get().is_some() && is_slash_command(&line) {
                    TYPED_COMMANDS
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(line.trim().to_string());
                } else if !line.trim().is_empty() {
                    QUEUED
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(line.trim().to_string());
                }
                // Ctrl+Enter: what is queued goes to the agent now; the
                // reply it is writing is cut off for it.
                if control == Some(CTRL_ENTER) && queued_count() > 0 {
                    SEND_NOW.store(true, Ordering::SeqCst);
                }
            }
            ch if !ch.is_control() => {
                let at = type_ahead.byte_at(type_ahead.cursor);
                type_ahead.text.insert(at, ch);
                type_ahead.cursor += 1;
            }
            _ => {}
        }
        // A menu closed with Esc opens again once the text changes, even
        // when it comes back to the same text within one read.
        if type_ahead
            .menu_closed_for
            .as_ref()
            .is_some_and(|closed| *closed != type_ahead.text)
        {
            type_ahead.menu_closed_for = None;
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use std::time::Instant;

    use super::{
        close_command_menu, draw_status_line, live, record_type_ahead, run_typed_commands, screen,
        split_keys, Activity, ACTIVITY, CTRL_B, CTRL_F, MODE_PRESSES, PARKED, PAUSED,
    };
    use crate::HookAbortSignal;

    /// `stty -g` output for the terminal's settings before the watcher.
    static ORIGINAL: Mutex<Option<String>> = Mutex::new(None);
    /// No echo, no line buffering, and reads that return after 0.1 s even
    /// without input, so the watcher can notice `stop` and `PAUSED`.
    const WATCH_SETTINGS: &[&str] = &["-icanon", "-echo", "min", "0", "time", "1"];

    pub(super) fn stty(args: &[&str]) -> Option<String> {
        let output = Command::new("stty")
            .args(args)
            .stdin(File::open("/dev/tty").ok()?)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub(super) struct TerminalModes;

    impl TerminalModes {
        pub(super) fn enter_watch_mode() -> Option<Self> {
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                return None;
            }
            let original = stty(&["-g"])?;
            stty(WATCH_SETTINGS)?;
            *ORIGINAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(original);
            set_kitty_keys(true);
            if super::stdout_is_terminal() {
                *ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(Activity::new(Instant::now()));
                if screen::active() {
                    screen::begin_turn();
                } else {
                    live::start();
                }
            }
            Some(Self)
        }

        pub(super) fn apply_original() {
            set_kitty_keys(false);
            if let Some(original) = ORIGINAL.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                let _ = stty(&[&original]);
            }
        }

        pub(super) fn apply_watch() {
            let _ = stty(WATCH_SETTINGS);
            set_kitty_keys(true);
        }

        pub(super) fn restore_original(self) {
            live::stop();
            screen::end_turn();
            *ACTIVITY.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Self::apply_original();
            *ORIGINAL.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// Whether the watcher pushed the kitty keyboard flags (popped once).
    static KITTY_PUSHED: AtomicBool = AtomicBool::new(false);

    /// Turns the kitty keyboard protocol's "disambiguate" mode on or off for
    /// the watcher, where the terminal supports it: Ctrl+Enter then arrives
    /// as `ESC [ 13 ; 5 u` instead of the same `\r` as Enter.
    fn set_kitty_keys(on: bool) {
        use std::io::Write;
        if on && !super::KEYBOARD_ENHANCED.load(Ordering::SeqCst) {
            return;
        }
        if KITTY_PUSHED.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(if on { b"\x1b[>1u" } else { b"\x1b[<u" });
            let _ = tty.flush();
        }
    }

    pub(super) fn watch(stop: &AtomicBool, abort: &HookAbortSignal) {
        // A separate, unbuffered handle: reading through `io::stdin()` would
        // leave bytes in its shared buffer.
        let Ok(mut tty) = File::open("/dev/tty") else {
            return;
        };
        let mut buffer = [0_u8; 256];
        // The start of an escape sequence cut off by the end of a read (a
        // burst of wheel reports easily fills the buffer), kept for the next
        // read so its rest is not taken for typed text.
        let mut pending = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            if PAUSED.load(Ordering::SeqCst) {
                pending.clear();
                PARKED.store(true, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            let read = match tty.read(&mut buffer) {
                Ok(0) => {
                    // The 0.1 s read timeout passed: nothing more is coming
                    // for what was held back, and the animation advances.
                    handle_keys(&std::mem::take(&mut pending), abort);
                    draw_status_line();
                    continue;
                }
                Ok(read) => read,
                // A signal (Ctrl+C, a window resize) cut the read short.
                // Returning here left the rest of the turn without Esc.
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            pending.extend_from_slice(&buffer[..read]);
            let complete = complete_len(&pending);
            let rest = pending.split_off(complete);
            handle_keys(&std::mem::replace(&mut pending, rest), abort);
        }
    }

    /// How much of `bytes` can be handled now: all of it, unless it ends
    /// inside an escape sequence, which is left for the next read.
    pub(super) fn complete_len(bytes: &[u8]) -> usize {
        let Some(start) = bytes.iter().rposition(|&b| b == 0x1b) else {
            return bytes.len();
        };
        match bytes.get(start + 1) {
            // A lone ESC as the whole read is the Esc key.
            None if start == 0 => bytes.len(),
            None => start,
            Some(b'[' | b'O') => {
                let finished = bytes[start + 2..].iter().any(|b| (0x40..=0x7e).contains(b));
                if finished {
                    bytes.len()
                } else {
                    start
                }
            }
            Some(_) => bytes.len(),
        }
    }

    fn handle_keys(bytes: &[u8], abort: &HookAbortSignal) {
        if bytes.is_empty() {
            return;
        }
        if bytes == [0x1b] {
            // A lone ESC is the Esc key; arrow and function keys send
            // ESC followed by more bytes in the same read. With the
            // background-task view open it closes the view instead.
            // The same for an open `/` menu.
            if screen::tasks_view_open() {
                screen::close_tasks_view();
            } else if !close_command_menu() {
                abort.abort();
            }
            return;
        }
        let mut keys = split_keys(bytes);
        if keys.interrupts > 0 {
            abort.abort();
        }
        for _ in 0..keys.escapes {
            if screen::tasks_view_open() {
                screen::close_tasks_view();
            } else if !close_command_menu() {
                abort.abort();
            }
        }
        if screen::tasks_view_open() {
            // ←/→ (here as Ctrl+B/F) switch the task the view shows.
            keys.text.retain(|&byte| match byte {
                CTRL_B | CTRL_F => {
                    screen::tasks_view_step(if byte == CTRL_B { -1 } else { 1 });
                    false
                }
                _ => true,
            });
        }
        if keys.back_tabs > 0 {
            MODE_PRESSES.fetch_add(keys.back_tabs, Ordering::SeqCst);
        }
        for scroll in keys.scrolls {
            screen::scroll(scroll);
        }
        for (scroll, row, col) in keys.wheels {
            screen::wheel(scroll, row, col);
        }
        for mouse in keys.mice {
            screen::mouse(mouse);
        }
        for _ in 0..keys.fold_toggles {
            screen::toggle_all_folds();
        }
        record_type_ahead(&keys.text);
        run_typed_commands();
        draw_status_line();
    }
}

/// Model output that is part of a tool call (counted, not shown as text).
pub fn note_tool_input(chars: usize) {
    with_activity(|activity| {
        activity.request_output = true;
        activity.chars += chars;
    });
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_slash_menu_ranks_commands_and_keeps_its_selection() {
        use super::{menu_matches, TypeAhead};
        let commands: Vec<(String, String)> = ["model", "compact", "cost", "mode"]
            .iter()
            .map(|name| ((*name).to_string(), String::new()))
            .collect();
        assert_eq!(menu_matches("/mode", &commands), vec![3, 0], "exact first");
        assert_eq!(menu_matches("/co", &commands), vec![1, 2]);
        assert_eq!(menu_matches("/st", &commands), vec![2], "contains");
        assert!(
            menu_matches("/model opus", &commands).is_empty(),
            "arguments"
        );
        assert!(menu_matches("plain text", &commands).is_empty());

        let mut typed = TypeAhead::new();
        typed.text = "/co".to_string();
        assert_eq!(typed.menu(&commands), (vec![1, 2], 0));
        typed.menu_index = 1;
        assert_eq!(typed.menu(&commands).1, 1, "kept while the text stays");
        typed.text = "/c".to_string();
        assert_eq!(typed.menu(&commands).1, 0, "back to the top on new text");
        // Esc closes it for this text only.
        typed.menu_closed_for = Some(typed.text.clone());
        assert!(typed.menu(&commands).0.is_empty());
        typed.text = "/com".to_string();
        assert_eq!(typed.menu(&commands).0, vec![1]);
    }

    #[test]
    fn slash_commands_are_told_from_paths_and_messages() {
        use super::is_slash_command;
        assert!(is_slash_command("/model opus"));
        assert!(is_slash_command("  /help"));
        assert!(!is_slash_command("/tmp/x.log has the error"));
        assert!(!is_slash_command("/ alone"));
        assert!(!is_slash_command("fix /help"));
        assert!(!is_slash_command(""));
    }

    #[test]
    fn btw_lines_name_their_question() {
        assert_eq!(
            super::btw_question("/btw what is the codename?"),
            Some("what is the codename?")
        );
        assert_eq!(super::btw_question("  /btw   spaced  "), Some("spaced"));
        assert_eq!(super::btw_question("/btw"), None);
        assert_eq!(super::btw_question("/btw   "), None);
        assert_eq!(super::btw_question("/btwx no"), None);
        assert_eq!(super::btw_question("fix the build"), None);
    }

    use std::time::{Duration, Instant};

    use super::{
        format_duration, queued_count, record_type_ahead, spinner_frame, take_queued,
        take_type_ahead, unqueue_into_type_ahead, Activity, SPINNER_FRAMES,
    };

    #[test]
    fn activity_label_follows_what_the_model_is_doing() {
        let start = Instant::now();
        let mut activity = Activity::new(start);
        assert_eq!(activity.label(start).0, "Thinking", "before any output");

        activity.request_output = true;
        activity.last_reasoning = Some(start);
        assert_eq!(
            activity.label(start + Duration::from_millis(500)).0,
            "Thinking"
        );

        activity.last_text = Some(start + Duration::from_secs(1));
        assert_eq!(activity.label(start + Duration::from_secs(2)).0, "Writing");

        let (label, note) = activity.label(start + Duration::from_secs(5));
        assert_eq!(label, "Working", "silent after output");
        assert!(note.is_none(), "no note for a short pause");

        let (_, note) = activity.label(start + Duration::from_secs(60));
        assert!(note.is_some_and(|note| note.contains("may be writing a file")));

        activity.tool = Some("Running bash · cargo test".to_string());
        assert_eq!(
            activity.label(start + Duration::from_secs(60)).0,
            "Running bash · cargo test"
        );
    }

    #[test]
    fn activity_row_shows_elapsed_time_and_tokens() {
        let start = Instant::now();
        let mut activity = Activity::new(start);
        activity.chars = 9_200;
        let (label, details) = activity.render(start + Duration::from_secs(75));
        assert_eq!(label, format!("{}…", activity.verb));
        assert_eq!(
            details,
            "1m 15s · ↓ 2.3k tokens · thinking · esc to interrupt"
        );
        activity.tool = Some("Running bash · ls".to_string());
        let (label, details) = activity.render(start + Duration::from_secs(5));
        assert_eq!(label, "Running bash · ls…");
        assert_eq!(details, "5s · ↓ 2.3k tokens · esc to interrupt");
        assert_eq!(format_duration(Duration::from_secs(9)), "9s");
    }

    #[test]
    fn long_activity_labels_are_cut_to_the_row() {
        let (label, details) = super::fit_activity(&"x".repeat(200), "12s", 60);
        assert_eq!(label.chars().count(), 56);
        assert!(label.ends_with('…'));
        assert!(details.is_empty());
        let (label, details) = super::fit_activity("Writing…", "12s · ~300 tokens", 60);
        assert_eq!(
            (label.as_str(), details.as_str()),
            ("Writing…", "12s · ~300 tokens")
        );
    }

    #[test]
    fn shift_tab_is_counted_and_other_escape_sequences_are_dropped() {
        let keys = super::split_keys(b"ab\x1b[Zc\x1b[B\x1bOP\x1b[1;5Cd\x1b[Z");
        // Down stays, as Ctrl+N, for the `/` menu (typed text ignores it).
        assert_eq!(keys.text, b"abc\x0ed");
        assert_eq!(keys.back_tabs, 2);
        assert!(keys.scrolls.is_empty());
    }

    #[test]
    fn kitty_keyboard_reports_become_the_legacy_keys() {
        // Ctrl+Enter, Enter, Esc, Ctrl+C, Ctrl+A, Shift+Tab, Ctrl+O, Alt+x.
        let keys = super::split_keys(
            b"a\x1b[13;5ub\x1b[13u\x1b[27u\x1b[99;5u\x1b[97;5u\x1b[9;2u\x1b[111;5u\x1b[120;3u",
        );
        assert_eq!(keys.text, b"a\x1eb\r\x01");
        assert_eq!(keys.escapes, 1);
        assert_eq!(keys.interrupts, 1);
        assert_eq!(keys.back_tabs, 1);
        assert_eq!(keys.fold_toggles, 1);
    }

    #[test]
    fn cursor_keys_stay_in_the_text_as_control_bytes() {
        let keys = super::split_keys(
            b"a\x1b[Db\x1b[C\x1bOD\x1bOC\x1b[H\x1bOH\x1b[1~\x1b[7~\x1b[F\x1bOF\x1b[4~\x1b[8~\x1b[3~\x1b[A\x1bOA",
        );
        assert_eq!(
            keys.text,
            b"a\x02b\x06\x02\x06\x01\x01\x01\x01\x05\x05\x05\x05\x04\x10\x10"
        );
    }

    #[test]
    fn wheel_and_page_keys_scroll() {
        use super::Scroll;
        let keys = super::split_keys(b"\x1b[<64;10;5M\x1b[<65;10;5M\x1b[<0;3;3M\x1b[<0;3;3m\x1b[5~\x1b[6~\x1b[1;5H\x1b[1;5Fx");
        assert_eq!(
            keys.wheels,
            vec![
                (Scroll::Up(super::wheel_rows()), 4, 9),
                (Scroll::Down(super::wheel_rows()), 4, 9),
            ],
            "wheel turns keep where the pointer is"
        );
        assert_eq!(
            keys.scrolls,
            vec![
                Scroll::PageUp,
                Scroll::PageDown,
                Scroll::Top,
                Scroll::Bottom,
            ]
        );
        assert_eq!(keys.text, b"x", "clicks are not typed text");
        assert_eq!(
            keys.mice,
            vec![
                super::screen::Mouse::Press { row: 2, col: 2 },
                super::screen::Mouse::Release { row: 2, col: 2 },
            ]
        );
    }

    #[test]
    fn drags_are_read_and_modified_clicks_left_alone() {
        use super::screen::Mouse;
        let keys =
            super::split_keys(b"\x1b[<0;1;1M\x1b[<32;9;4M\x1b[<0;9;4m\x1b[<4;2;2M\x1b[<2;2;2M");
        assert_eq!(
            keys.mice,
            vec![
                Mouse::Press { row: 0, col: 0 },
                Mouse::Drag { row: 3, col: 8 },
                Mouse::Release { row: 3, col: 8 },
            ]
        );
        assert!(keys.text.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn escape_sequences_cut_by_a_read_are_held_back() {
        use super::imp::complete_len;
        // A wheel report split across two reads: the head waits.
        assert_eq!(complete_len(b"\x1b[<64;10;5M\x1b[<6"), 11);
        assert_eq!(complete_len(b"ab\x1b"), 2);
        // Complete input, a lone Esc and Alt+key go through whole.
        assert_eq!(complete_len(b"x\x1b[<65;10;5M"), 12);
        assert_eq!(complete_len(b"\x1b"), 1);
        assert_eq!(complete_len(b"\x1bb"), 2);
        assert_eq!(complete_len(b"plain"), 5);
    }

    #[test]
    fn btw_lines_wait_for_the_end_of_the_turn() {
        let _turn = super::simulate_queued_during_turn("/btw what is trident?");
        super::QUEUED
            .lock()
            .unwrap()
            .push("also run the tests".to_string());
        assert_eq!(
            super::take_queued_for_running_turn(),
            vec!["also run the tests".to_string()]
        );
        assert_eq!(
            super::take_queued(),
            Some("/btw what is trident?".to_string())
        );
        assert!(!super::is_btw("/btwx"));
    }

    #[test]
    fn spinner_plays_forwards_then_backwards() {
        let frames = (0..10)
            .map(|step| spinner_frame(Duration::from_millis(step * 120)))
            .collect::<Vec<_>>();
        assert_eq!(frames[0], SPINNER_FRAMES[0]);
        assert_eq!(frames[5], SPINNER_FRAMES[5]);
        assert_eq!(frames[6], SPINNER_FRAMES[4]);
        assert_eq!(frames[9], SPINNER_FRAMES[1]);
    }

    #[test]
    fn footer_keeps_the_mode_and_drops_details_that_do_not_fit() {
        let details = vec![
            "local/qwen3.6".to_string(),
            "ctx 6.3k/16k (39%)".to_string(),
        ];
        let line = super::compose_footer("⏵⏵ auto mode on", "", &details, 80, false);
        assert_eq!(
            line,
            "  ⏵⏵ auto mode on · local/qwen3.6 · ctx 6.3k/16k (39%)"
        );
        let line = super::compose_footer("⏵⏵ auto mode on", "", &details, 40, false);
        assert_eq!(line, "  ⏵⏵ auto mode on · local/qwen3.6");
    }

    /// The buffers are process-wide, so the steps run in one test.
    #[test]
    fn typing_during_a_turn_queues_lines_and_keeps_unfinished_text() {
        while take_queued().is_some() {}
        let _ = take_type_ahead();

        record_type_ahead(b"helo\x7flo\rsecond one\r  \rdraft\x01");
        assert_eq!(queued_count(), 2);
        assert_eq!(take_queued().as_deref(), Some("hello"));
        assert_eq!(take_queued().as_deref(), Some("second one"));
        assert_eq!(take_queued(), None, "blank lines are not queued");
        assert_eq!(take_type_ahead(), "draft");

        // Ctrl+Enter queues like Enter and asks for the reply to be cut off,
        // only when something is queued; a new model request clears it.
        super::clear_send_now();
        record_type_ahead(b"\x1e");
        assert!(!super::send_now_requested(), "nothing queued");
        record_type_ahead(b"stop, use the other file\x1e");
        assert!(super::send_now_requested());
        assert_eq!(take_queued().as_deref(), Some("stop, use the other file"));
        super::begin_model_request();
        assert!(!super::send_now_requested());

        record_type_ahead(b"a\rb\rtail");
        unqueue_into_type_ahead();
        assert_eq!(queued_count(), 0);
        assert_eq!(take_type_ahead(), "a\nb\ntail");

        // Arrows move the cursor: typing and Backspace act there, in the
        // order the keys arrived.
        record_type_ahead(
            &super::split_keys(b"fix teh bug\x1b[D\x1b[D\x1b[D\x1b[D\x7f\x7fhe").text,
        );
        record_type_ahead(&super::split_keys(b"\x1bOH!\x1b[F?").text);
        assert_eq!(take_type_ahead(), "!fix the bug?");
        record_type_ahead(&super::split_keys(b"h\xc3\xa9llo\x1b[1~\x1b[3~\x1b[C\x1b[C\x15").text);
        record_type_ahead(&super::split_keys(b"\x1b[4~\x1b[D\x0b").text);
        assert_eq!(
            take_type_ahead(),
            "l",
            "Ctrl+U and Ctrl+K cut around the cursor, by chars"
        );
        record_type_ahead(b"x\rnext");
        assert_eq!(take_queued().as_deref(), Some("x"));
        record_type_ahead(&super::split_keys(b"\x1b[D\x1b[D!").text);
        assert_eq!(
            take_type_ahead(),
            "ne!xt",
            "Enter leaves the cursor in the new line"
        );

        // Up pulls queued messages back to edit, in front of what is being
        // typed, with the cursor after them; Enter queues the result again.
        record_type_ahead(b"fix teh tests\rand docs\rdraft");
        assert_eq!(queued_count(), 2);
        record_type_ahead(&super::split_keys(b"\x1b[A").text);
        assert_eq!(queued_count(), 0);
        let right7 = b"\x1b[C".repeat(7);
        let mut keys = b" too\x1b[H".to_vec();
        keys.extend_from_slice(&right7);
        keys.extend_from_slice(b"\x7f\x7fhe\r");
        record_type_ahead(&super::split_keys(&keys).text);
        assert_eq!(
            take_queued().as_deref(),
            Some("fix the tests\nand docs too\ndraft")
        );
        record_type_ahead(&super::split_keys(b"\x1b[Ax").text);
        assert_eq!(
            take_type_ahead(),
            "x",
            "Up with nothing queued does nothing"
        );
    }

    #[test]
    fn type_ahead_field_draws_the_cursor_where_it_is() {
        let field = super::type_ahead_field;
        assert_eq!(field("abc", 3, 20), "abc\x1b[7m \x1b[0m");
        assert_eq!(field("abc", 1, 20), "a\x1b[7mb\x1b[0mc");
        // Too long: the end shows, unless the cursor is further left.
        assert_eq!(field("abcdefgh", 8, 4), "fgh\x1b[7m \x1b[0m");
        assert_eq!(field("abcdefgh", 2, 4), "\x1b[7mc\x1b[0mdef");
        assert_eq!(field("abcdefgh", 6, 4), "f\x1b[7mg\x1b[0mh");
        assert_eq!(
            field("a\nb", 3, 20),
            "a↵b\x1b[7m \x1b[0m",
            "line breaks keep one row"
        );
    }
}
