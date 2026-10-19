//! Full-screen mode, like Claude Code's: claw draws the whole window. The
//! conversation fills the top and scrolls inside claw (mouse wheel, PgUp/
//! PgDn, Ctrl+Home/Ctrl+End), and the input box and status line stay at the
//! bottom whatever is scrolled, growing for a long prompt or a question and
//! shrinking back afterwards.
//!
//! For the whole session stdout and stderr go into a pipe. A relay thread
//! feeds what arrives into the conversation (a [`Buffer`] of styled lines),
//! except bytes marked as the bottom pane (the input box and prompts, see
//! [`write_pane`]), which go into a second buffer that wraps like a real
//! terminal so their cursor-relative redraws keep working. A draw thread
//! paints the window from both, changed rows only, and follows resizes.
//! During a turn the pane shows the activity block instead.
//!
//! On exit the window is given back and the conversation is printed into the
//! terminal's history, so it is still there to scroll through.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::live::{split_marks, Event, MARK_END, MARK_START};
use super::vt::{self, Buffer};
use super::Scroll;

/// Lines of conversation kept; older ones are dropped.
const MAX_LINES: usize = 20_000;

/// Shortest time between two draws, so a burst of output is drawn once.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// The activity block's animation step.
const ANIMATION: Duration = Duration::from_millis(100);
/// How often the size is checked while nothing happens.
const IDLE_CHECK: Duration = Duration::from_millis(250);
/// Alternate screen, mouse reports with drags (SGR encoding), no autowrap,
/// cleared.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?7l\x1b[2J";
const LEAVE: &[u8] = b"\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[?7h\x1b[?25h\x1b[?1049l";
const JUMP_HINT: &str = " Jump to bottom (ctrl+End) ↓ ";
/// The first line of the focus view.
const FOCUS_HEADER: &str = "\x1b[2m── focus view · /focus for all ──\x1b[0m\r\n";
/// Longest wait for the rest of a pane redraw before drawing anyway.
const PANE_WRITE_WAIT: Duration = Duration::from_millis(50);
/// How long "Copied" stays in the bottom row after a selection.
const NOTICE_TIME: Duration = Duration::from_secs(2);
/// Narrowest window that has room for the side panel.
pub const SIDE_PANEL_MIN_COLS: usize = 110;
/// The rule between the conversation and the side panel.
const PANEL_RULE: &str = "\x1b[38;5;240m│\x1b[0m";
const CLOSE_MARK: &str = "✕";

/// Draws the side panel's content `width` columns wide: one string per row,
/// each exactly `width` columns. Called from the draw thread, outside the
/// screen's lock, and again only when the width changes.
pub type PanelRender = Arc<dyn Fn(usize) -> Vec<String> + Send + Sync>;

/// A panel to the right of the conversation (the diff panel).
struct Panel {
    /// What it shows, for the notice when the window is too narrow.
    name: String,
    render: PanelRender,
    /// The rows last drawn and the width they were drawn for.
    rows: Option<(usize, Arc<Vec<String>>)>,
    /// First content row shown.
    scroll: usize,
}

/// Where the last frame put things, for mouse hits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Layout {
    /// Columns the conversation used.
    conversation_cols: usize,
    /// Rows the conversation (and panel) used.
    height: usize,
    /// The panel's first column (its rule), when it is shown.
    panel_left: Option<usize>,
    /// The panel's ✕, as (row, col).
    panel_close: Option<(usize, usize)>,
    /// The too-narrow notice's ✕, as (row, col).
    notice_close: Option<(usize, usize)>,
    /// The "Jump to bottom" hint, as (row, first col, past last col).
    jump_hint: Option<(usize, usize, usize)>,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// Puts selected text on the clipboard; set by the binary (see
/// [`set_copy_handler`]). Returns false when nothing could take it.
static COPY_HANDLER: Mutex<Option<CopyHandler>> = Mutex::new(None);
type CopyHandler = fn(&str) -> bool;

/// A mouse event over the window, in 0-based screen cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mouse {
    /// Left button pressed.
    Press { row: usize, col: usize },
    /// Moved with the left button held.
    Drag { row: usize, col: usize },
    /// Left button released.
    Release { row: usize, col: usize },
}

/// A mouse selection in the conversation, as (line, cell) positions, so it
/// stays on the same text while scrolling and while output arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl Selection {
    /// First position and the position just past the last selected cell.
    fn range(self) -> ((usize, usize), (usize, usize)) {
        let (start, end) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        (start, (end.0, end.1 + 1))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Session {
    shared: Arc<Shared>,
    /// The terminal's own stdout and stderr, to put back.
    stdout: OwnedFd,
    stderr: OwnedFd,
    draw: Option<JoinHandle<()>>,
    /// `stty -g` from before the session, to put back.
    tty_modes: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stop: AtomicBool,
    /// `sync` marks sent and handled, so [`leave`] can wait for the relay.
    sent: AtomicU64,
    handled: AtomicU64,
}

struct State {
    conversation: Buffer,
    pane: Buffer,
    /// A turn is running: the activity block replaces the pane.
    block: bool,
    /// The relay is inside marked pane bytes.
    in_pane: bool,
    /// When the relay entered the pane bytes it is in.
    pane_since: Instant,
    /// The focus view: the last prompt, one line per tool call and the
    /// final reply (see [`focus_write`]); shown instead of the
    /// conversation while `focus_mode` is on.
    focus: Buffer,
    focus_mode: bool,
    /// The relay is inside marked focus bytes.
    in_focus: bool,
    /// The top row shown while scrolled up, as (line, row within the
    /// line); `None` follows the end of the conversation.
    anchor: Option<(usize, usize)>,
    /// The conversation area at the last draw (rows, columns).
    view: (usize, usize),
    dirty: bool,
    passthrough: Vec<u8>,
    selection: Option<Selection>,
    /// The left button is down and moving it extends the selection.
    selecting: bool,
    /// The conversation rows of the last draw, as (line, start cell, end
    /// cell), to map mouse positions to text.
    shown_rows: Vec<(usize, usize, usize)>,
    /// A short message in the bottom row ("Copied …"), until it expires.
    notice: Option<(String, Instant)>,
    panel: Option<Panel>,
    /// The user closed the panel (its ✕, `/diffpanel off`).
    panel_closed: bool,
    /// The user dismissed the "too narrow for the panel" notice.
    narrow_notice_dismissed: bool,
    layout: Layout,
}

/// Whether full-screen mode is wanted: `CLAW_FULLSCREEN=0` turns it off,
/// else the `tui` user setting (`/tui default` saves `"default"`).
#[must_use]
pub fn enabled() -> bool {
    std::env::var("CLAW_FULLSCREEN").map_or_else(
        |_| crate::read_user_setting("tui").as_deref() != Some("default"),
        |value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        },
    )
}

/// Whether claw is drawing the whole window.
#[must_use]
pub fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Gives the window back when dropped (see [`enter`]).
pub struct ScreenGuard(());

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        leave();
    }
}

/// Starts full-screen mode when it is enabled and stdin and stdout are a
/// terminal. Keep the guard for as long as the window should be drawn.
#[must_use]
pub fn enter() -> Option<ScreenGuard> {
    if active() || !enabled() || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    let session = start().ok()?;
    *lock(&SESSION) = Some(session);
    ACTIVE.store(true, Ordering::SeqCst);
    install_panic_hook();
    Some(ScreenGuard(()))
}

fn start() -> io::Result<Session> {
    let _ = io::stdout().flush();
    // Mouse reports stay on for the whole session, but the keyboard is only
    // read while the input box, a prompt or the turn watcher is up. With echo
    // on, wheel reports arriving in between (a slow command, auto-compaction)
    // would be printed over the window as `^[[<64;10;5M`. Everything that
    // reads the keyboard saves and restores the modes it finds, so turning
    // echo off here keeps it off for all of them.
    let tty_modes = super::imp::stty(&["-g"]);
    if tty_modes.is_some() {
        super::imp::stty(&["-echo"]);
    }
    let stdout = rustix::io::dup(io::stdout())?;
    let stderr = rustix::io::dup(io::stderr())?;
    let tty = File::from(stdout.try_clone()?);
    let (reader, writer) = io::pipe()?;
    let shared = Arc::new(Shared {
        state: Mutex::new(State::new()),
        wake: Condvar::new(),
        stop: AtomicBool::new(false),
        sent: AtomicU64::new(0),
        handled: AtomicU64::new(0),
    });
    let relay_shared = Arc::clone(&shared);
    thread::Builder::new()
        .name("claw-screen-relay".to_string())
        .spawn(move || relay(&relay_shared, reader))?;
    let draw_shared = Arc::clone(&shared);
    let draw = thread::Builder::new()
        .name("claw-screen-draw".to_string())
        .spawn(move || draw_loop(&draw_shared, tty))?;
    let redirected =
        rustix::stdio::dup2_stdout(&writer).and_then(|()| rustix::stdio::dup2_stderr(&writer));
    if let Err(error) = redirected {
        let _ = rustix::stdio::dup2_stdout(&stdout);
        let _ = rustix::stdio::dup2_stderr(&stderr);
        shared.stop.store(true, Ordering::SeqCst);
        shared.wake.notify_all();
        let _ = draw.join();
        restore_tty_modes(tty_modes.as_deref());
        return Err(error.into());
    }
    drop(writer);
    Ok(Session {
        shared,
        stdout,
        stderr,
        draw: Some(draw),
        tty_modes,
    })
}

/// Gives the window back and prints the conversation into the terminal's
/// history.
pub fn leave() {
    if !active() {
        return;
    }
    sync();
    let Some(mut session) = lock(&SESSION).take() else {
        return;
    };
    ACTIVE.store(false, Ordering::SeqCst);
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    let _ = rustix::stdio::dup2_stdout(&session.stdout);
    let _ = rustix::stdio::dup2_stderr(&session.stderr);
    session.shared.stop.store(true, Ordering::SeqCst);
    session.shared.wake.notify_all();
    if let Some(draw) = session.draw.take() {
        let _ = draw.join();
    }
    restore_tty_modes(session.tty_modes.as_deref());
    let state = lock(&session.shared.state);
    let lines = &state.conversation.lines;
    let end = lines
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |last| last + 1);
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(&state.conversation.graphics);
    for line in &lines[..end] {
        let _ = writeln!(stdout, "{}", vt::render_cells(line));
    }
    let _ = stdout.flush();
}

/// After a panic: put the terminal back without waiting for anything.
fn emergency_restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let Ok(guard) = SESSION.try_lock() else {
        return;
    };
    if let Some(session) = guard.as_ref() {
        session.shared.stop.store(true, Ordering::SeqCst);
        let _ = rustix::stdio::dup2_stdout(&session.stdout);
        let _ = rustix::stdio::dup2_stderr(&session.stderr);
        let _ = io::stdout().write_all(LEAVE);
        let _ = io::stdout().flush();
        restore_tty_modes(session.tty_modes.as_deref());
    }
}

fn restore_tty_modes(modes: Option<&str>) {
    if let Some(modes) = modes {
        super::imp::stty(&[modes]);
    }
}

fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            emergency_restore();
            previous(info);
        }));
    });
}

fn shared() -> Option<Arc<Shared>> {
    lock(&SESSION)
        .as_ref()
        .map(|session| Arc::clone(&session.shared))
}

fn with_state(update: impl FnOnce(&mut State)) {
    if let Some(shared) = shared() {
        let mut state = lock(&shared.state);
        update(&mut state);
        state.dirty = true;
        drop(state);
        shared.wake.notify_all();
    }
}

fn mark_bytes(name: &[u8]) -> Vec<u8> {
    let mut mark = MARK_START.to_vec();
    mark.extend_from_slice(name);
    mark.extend_from_slice(MARK_END);
    mark
}

/// Writes a mark behind everything printed so far.
fn mark(name: &[u8]) {
    let _ = io::stderr().flush();
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(&mark_bytes(name));
    let _ = stdout.flush();
}

/// Waits (briefly) until the relay has taken in everything printed so far.
fn sync() {
    let Some(shared) = shared() else {
        return;
    };
    let target = shared.sent.fetch_add(1, Ordering::SeqCst) + 1;
    mark(b"sync");
    let deadline = Instant::now() + Duration::from_secs(1);
    while shared.handled.load(Ordering::SeqCst) < target && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
}

/// Draws into the bottom pane: `bytes` are what the caller would write to a
/// terminal (relative cursor moves, erases, text). Without full-screen mode
/// they go to stdout as they are.
pub fn write_pane(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if active() {
        let mut framed = mark_bytes(b"p");
        framed.extend_from_slice(bytes);
        framed.extend_from_slice(&mark_bytes(b"o"));
        stdout.write_all(&framed)?;
    } else {
        stdout.write_all(bytes)?;
    }
    stdout.flush()
}

/// Starts a new, empty pane (an input box or prompt is about to draw).
pub fn begin_pane() {
    if active() {
        mark(b"begin");
    }
}

/// Moves what the pane shows above its cursor into the conversation (a
/// submitted prompt), then empties it.
pub fn commit_pane() {
    if active() {
        mark(b"commit");
    }
}

/// Empties the pane.
pub fn clear_pane() {
    if active() {
        mark(b"clear");
    }
}

/// Empties the conversation (Ctrl+L, `/clear`).
pub fn clear_conversation() {
    with_state(|state| {
        state.conversation = Buffer::new(None);
        state.focus = Buffer::new(None);
        state.anchor = None;
        state.selection = None;
        state.selecting = false;
    });
}

/// Scrolls the conversation.
pub fn scroll(scroll: Scroll) {
    with_state(|state| state.scroll(scroll));
}

/// The mouse wheel at a screen cell: scrolls the side panel when the
/// pointer is over it, otherwise the conversation.
pub fn wheel(scroll: Scroll, row: usize, col: usize) {
    with_state(|state| {
        if state.over_panel(row, col) {
            state.scroll_panel(scroll);
        } else {
            state.scroll(scroll);
        }
    });
}

/// Shows `render`'s rows in a panel right of the conversation (when the
/// window is at least [`SIDE_PANEL_MIN_COLS`] wide), or removes the panel
/// with `None`. `name` says what it is ("diff"). Replacing the content keeps
/// the panel's scroll position.
pub fn set_side_panel(name: &str, render: Option<PanelRender>) {
    with_state(|state| match render {
        Some(render) => {
            let scroll = state.panel.as_ref().map_or(0, |panel| panel.scroll);
            state.panel = Some(Panel {
                name: name.to_string(),
                render,
                rows: None,
                scroll,
            });
        }
        None => state.panel = None,
    });
}

/// Closes the side panel, or opens it again (which also brings back the
/// "too narrow" notice).
pub fn set_side_panel_closed(closed: bool) {
    with_state(|state| {
        state.panel_closed = closed;
        if !closed {
            state.narrow_notice_dismissed = false;
        }
    });
}

/// Whether the user closed the side panel.
#[must_use]
pub fn side_panel_closed() -> bool {
    shared().is_some_and(|shared| lock(&shared.state).panel_closed)
}

/// The width (without its rule) a side panel gets at the window's current
/// size, when one would be shown: the window is wide enough and the user
/// has not closed it.
#[must_use]
pub fn side_panel_width() -> Option<usize> {
    let shared = shared()?;
    let closed = lock(&shared.state).panel_closed;
    let cols = super::terminal_cols();
    (!closed && cols >= SIDE_PANEL_MIN_COLS).then(|| side_panel_width_for(cols))
}

/// Two fifths of a window `cols` wide, at least 44 columns.
fn side_panel_width_for(cols: usize) -> usize {
    (cols * 2 / 5).max(44)
}

/// Columns the conversation gets: the window's width, less the side panel
/// when one is shown. `None` outside full-screen mode.
#[must_use]
pub fn conversation_cols() -> Option<usize> {
    let shared = shared()?;
    let state = lock(&shared.state);
    let cols = super::terminal_cols();
    Some(
        state
            .panel_width(cols)
            .map_or(cols, |width| cols - width - 1),
    )
}

/// Adds to the focus view (a tool call's one-line summary, the final
/// reply); `bytes` are terminal text. Kept in order with other output.
pub fn focus_write(bytes: &[u8]) {
    if !active() {
        return;
    }
    let mut framed = mark_bytes(b"F");
    framed.extend_from_slice(bytes);
    framed.extend_from_slice(&mark_bytes(b"f"));
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(&framed);
    let _ = stdout.flush();
}

/// Shows the focus view instead of the whole conversation, or back.
pub fn set_focus(on: bool) {
    with_state(|state| state.set_focus_mode(on));
}

/// Whether the focus view is shown.
#[must_use]
pub fn focus() -> bool {
    shared().is_some_and(|shared| lock(&shared.state).focus_mode)
}

/// Sets what copies a finished mouse selection to the clipboard.
pub fn set_copy_handler(handler: CopyHandler) {
    *lock(&COPY_HANDLER) = Some(handler);
}

/// Selects conversation text with the mouse, like a terminal does: press,
/// drag, release. The selection is copied to the clipboard on release and
/// stays highlighted until the next click.
pub fn mouse(event: Mouse) {
    let mut copied = None;
    with_state(|state| copied = state.mouse(event));
    let Some(text) = copied else {
        return;
    };
    let Some(handler) = *lock(&COPY_HANDLER) else {
        return;
    };
    // Clipboard programs can be slow to start; input must not wait.
    let _ = thread::Builder::new()
        .name("claw-copy".to_string())
        .spawn(move || {
            let chars = text.chars().count();
            let notice = if handler(&text) {
                format!("Copied {chars} characters")
            } else {
                "Could not copy: no clipboard tool found".to_string()
            };
            with_state(|state| state.notice = Some((notice, Instant::now())));
        });
}

/// A turn started: show the activity block in the pane.
pub(super) fn begin_turn() {
    with_state(|state| {
        state.block = true;
        state.reset_pane();
    });
}

pub(super) fn end_turn() {
    with_state(|state| {
        state.block = false;
        state.reset_pane();
    });
}

/// A prompt mid-turn reads the keyboard: it draws in the pane instead.
pub(super) fn pause_turn() {
    with_state(|state| {
        state.block = false;
        state.reset_pane();
    });
}

pub(super) fn resume_turn() {
    with_state(|state| {
        state.block = true;
        state.reset_pane();
    });
}

/// Something in the activity block changed (typing, a new activity).
pub(super) fn mark_dirty() {
    with_state(|_| {});
}

fn relay(shared: &Shared, mut pipe: impl Read) {
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut pending = Vec::new();
    loop {
        let read = match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        pending.extend_from_slice(&buffer[..read]);
        let (events, rest) = split_marks(&pending);
        pending = rest;
        let mut state = lock(&shared.state);
        for event in events {
            match event {
                Event::Output(bytes) => state.feed(&bytes),
                Event::Mark(name) => match name.as_slice() {
                    b"p" => {
                        state.in_pane = true;
                        state.pane_since = Instant::now();
                    }
                    b"o" => state.in_pane = false,
                    b"F" => state.in_focus = true,
                    b"f" => state.in_focus = false,
                    b"begin" | b"clear" => state.reset_pane(),
                    b"commit" => state.commit_pane(),
                    b"sync" => {
                        shared.handled.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                },
            }
        }
        state.dirty = true;
        drop(state);
        shared.wake.notify_all();
    }
}

fn draw_loop(shared: &Shared, mut tty: File) {
    let _ = tty.write_all(ENTER);
    let _ = tty.flush();
    let mut shown: Vec<String> = Vec::new();
    let mut shown_size = (0, 0);
    let mut last_animation = Instant::now();
    while !shared.stop.load(Ordering::SeqCst) {
        let mut state = lock(&shared.state);
        if !state.dirty {
            let wait = if state.block { ANIMATION } else { IDLE_CHECK };
            state = shared
                .wake
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let size = super::terminal_size().unwrap_or((24, 80));
        let animate = state.block && last_animation.elapsed() >= ANIMATION;
        if !state.dirty && !animate && size == shown_size {
            continue;
        }
        // A pane redraw (the input box after a keystroke) reaches the pipe in
        // pieces, because stdout is line-buffered: drawing between them
        // shows a half-drawn box for a frame, which flickers on every key.
        if state.in_pane && state.pane_since.elapsed() < PANE_WRITE_WAIT {
            drop(state);
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        state.dirty = false;
        if animate {
            last_animation = Instant::now();
        }
        if let Some((render, width)) = state.panel_render_needed(usize::from(size.1)) {
            // Drawn without the lock: output keeps flowing meanwhile.
            drop(state);
            let rows = render(width);
            state = lock(&shared.state);
            state.store_panel_rows(&render, width, rows);
        }
        let (frame, cursor) = state.compose(usize::from(size.0), usize::from(size.1));
        let passthrough = std::mem::take(&mut state.passthrough);
        drop(state);

        let mut out = passthrough;
        out.extend_from_slice(b"\x1b[?2026h");
        if size != shown_size {
            out.extend_from_slice(b"\x1b[2J");
            shown.clear();
            shown_size = size;
        }
        for (index, row) in frame.iter().enumerate() {
            if shown.get(index) != Some(row) {
                out.extend_from_slice(format!("\x1b[{};1H\x1b[2K{row}", index + 1).as_bytes());
            }
        }
        match cursor {
            Some((row, col)) => {
                out.extend_from_slice(format!("\x1b[{};{}H\x1b[?25h", row + 1, col + 1).as_bytes());
            }
            None => out.extend_from_slice(b"\x1b[?25l"),
        }
        out.extend_from_slice(b"\x1b[?2026l");
        shown = frame;
        let _ = tty.write_all(&out);
        let _ = tty.flush();
        thread::sleep(FRAME_INTERVAL);
    }
    let _ = tty.write_all(LEAVE);
    let _ = tty.flush();
}

impl State {
    fn new() -> Self {
        Self {
            conversation: Buffer::new(None),
            pane: Buffer::new(Some(super::terminal_cols())),
            block: false,
            in_pane: false,
            pane_since: Instant::now(),
            focus: Buffer::new(None),
            focus_mode: false,
            in_focus: false,
            anchor: None,
            view: (0, 0),
            dirty: true,
            passthrough: Vec::new(),
            selection: None,
            selecting: false,
            shown_rows: Vec::new(),
            notice: None,
            panel: None,
            panel_closed: false,
            narrow_notice_dismissed: false,
            layout: Layout::default(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        if self.in_pane {
            self.pane.set_wrap(super::terminal_cols());
            self.pane.feed(bytes);
            self.passthrough.append(&mut self.pane.passthrough);
        } else if self.in_focus {
            self.focus.feed(bytes);
            self.focus.passthrough.clear();
        } else {
            self.conversation.feed(bytes);
            self.passthrough.append(&mut self.conversation.passthrough);
            let dropped = self.conversation.trim(MAX_LINES);
            if dropped > 0 {
                self.selection = self.selection.and_then(|selection| {
                    let shift = |(line, cell): (usize, usize)| {
                        line.checked_sub(dropped).map(|line| (line, cell))
                    };
                    Some(Selection {
                        anchor: shift(selection.anchor)?,
                        head: shift(selection.head)?,
                    })
                });
                self.anchor = self
                    .anchor
                    .map(|(line, sub)| match line.checked_sub(dropped) {
                        Some(line) => (line, sub),
                        None => (0, 0),
                    });
            }
        }
    }

    fn reset_pane(&mut self) {
        self.pane = Buffer::new(Some(super::terminal_cols()));
    }

    fn commit_pane(&mut self) {
        let rows = if self.pane.col == 0 {
            self.pane.row
        } else {
            self.pane.row + 1
        };
        let lines = self.pane.take_lines(rows);
        // A new prompt starts a new focus view.
        self.focus = Buffer::new(None);
        self.focus.feed(FOCUS_HEADER.as_bytes());
        self.focus.append_lines(lines.clone());
        self.conversation.append_lines(lines);
        self.reset_pane();
    }

    fn set_focus_mode(&mut self, on: bool) {
        if self.focus_mode != on {
            self.focus_mode = on;
            self.anchor = None;
            self.selection = None;
            self.selecting = false;
        }
    }

    /// The window's rows (each a string to draw at column 1) and where the
    /// cursor goes, if it is shown.
    fn compose(&mut self, rows: usize, cols: usize) -> (Vec<String>, Option<(usize, usize)>) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let (mut pane_rows, mut cursor) = if self.block {
            (super::block_rows(cols), None)
        } else {
            let pane_rows = (0..self.pane.height())
                .map(|row| {
                    self.pane
                        .lines
                        .get(row)
                        .map_or_else(String::new, |line| vt::render_cells(vt::clip(line, cols)))
                })
                .collect::<Vec<_>>();
            let cursor = self
                .pane
                .cursor_visible
                .then(|| (self.pane.row, self.pane.col.min(cols - 1)));
            (pane_rows, cursor)
        };
        // Leave the conversation at least a couple of rows.
        let room = rows.saturating_sub(2).max(1);
        if pane_rows.len() > room {
            let skip = pane_rows.len() - room;
            pane_rows.drain(..skip);
            cursor = cursor.and_then(|(row, col)| row.checked_sub(skip).map(|row| (row, col)));
        }
        let mut height = rows.saturating_sub(pane_rows.len());
        self.layout = Layout::default();
        let notice_rows = self.narrow_notice_rows(cols, height);
        height -= notice_rows.len();
        if !notice_rows.is_empty() {
            self.layout.notice_close = Some((height, cols.saturating_sub(3)));
        }
        let panel_width = self.panel_width(cols);
        let conversation_cols = panel_width.map_or(cols, |width| cols - width - 1);
        self.layout.conversation_cols = conversation_cols;
        self.layout.height = height;
        self.view = (height, conversation_cols);
        let mut frame = self.conversation_rows(height, conversation_cols);
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= NOTICE_TIME)
        {
            self.notice = None;
        }
        let hint = match &self.notice {
            Some((notice, _)) => Some(format!(" {notice} ")),
            None => self.anchor.is_some().then(|| JUMP_HINT.to_string()),
        };
        let is_jump_hint = self.notice.is_none() && self.anchor.is_some();
        if let (Some(hint), Some(last)) = (hint, frame.last_mut()) {
            let width = hint.chars().count();
            let pad = conversation_cols.saturating_sub(width) / 2;
            *last = format!("{}\x1b[7m{hint}\x1b[0m", " ".repeat(pad));
            if is_jump_hint {
                self.layout.jump_hint = Some((height - 1, pad, pad + width));
            }
        }
        if let Some(width) = panel_width {
            self.add_panel(&mut frame, conversation_cols, width);
        }
        let cursor = cursor.map(|(row, col)| (height + notice_rows.len() + row, col));
        frame.extend(notice_rows);
        frame.extend(pane_rows);
        (frame, cursor)
    }

    /// Whether a panel is set and the user has not closed it.
    fn panel_wanted(&self) -> bool {
        self.panel.is_some() && !self.panel_closed
    }

    /// The panel's width (without its rule) in a window `cols` wide, when
    /// it is shown: two fifths of the window, at least 44 columns.
    fn panel_width(&self, cols: usize) -> Option<usize> {
        (self.panel_wanted() && cols >= SIDE_PANEL_MIN_COLS).then(|| side_panel_width_for(cols))
    }

    /// The boxed "too narrow for the panel" notice, with a ✕ to dismiss it.
    fn narrow_notice_rows(&self, cols: usize, height: usize) -> Vec<String> {
        let Some(panel) = &self.panel else {
            return Vec::new();
        };
        if self.panel_closed
            || self.narrow_notice_dismissed
            || !(20..SIDE_PANEL_MIN_COLS).contains(&cols)
            || height < 8
        {
            return Vec::new();
        }
        let inner = cols - 2;
        let text = format!(
            " Resize your terminal to at least {SIDE_PANEL_MIN_COLS} columns to show the {} panel",
            panel.name
        );
        let text: String = text.chars().take(inner).collect();
        let pad = inner.saturating_sub(text.chars().count());
        vec![
            format!(
                "\x1b[38;5;245m╭{}{CLOSE_MARK}─╮\x1b[0m",
                "─".repeat(inner.saturating_sub(2))
            ),
            format!(
                "\x1b[38;5;245m│\x1b[0m{text}{}\x1b[38;5;245m│\x1b[0m",
                " ".repeat(pad)
            ),
            format!("\x1b[38;5;245m╰{}╯\x1b[0m", "─".repeat(inner)),
        ]
    }

    /// Draws the panel into the right of `frame`'s rows: a row with its ✕,
    /// then its content from the scroll position.
    fn add_panel(&mut self, frame: &mut [String], conversation_cols: usize, width: usize) {
        let height = frame.len();
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        let content: Arc<Vec<String>> = match &panel.rows {
            Some((drawn_for, rows)) if *drawn_for == width => Arc::clone(rows),
            _ => Arc::new(Vec::new()),
        };
        let max_scroll = content.len().saturating_sub(height.saturating_sub(1));
        panel.scroll = panel.scroll.min(max_scroll);
        let left = conversation_cols + 1;
        self.layout.panel_left = Some(conversation_cols);
        self.layout.panel_close = Some((0, left + width.saturating_sub(2)));
        for (index, row) in frame.iter_mut().enumerate() {
            let cells = if index == 0 {
                format!(
                    "{}\x1b[38;5;245m{CLOSE_MARK}\x1b[0m ",
                    " ".repeat(width.saturating_sub(2))
                )
            } else {
                content
                    .get(panel.scroll + index - 1)
                    .cloned()
                    .unwrap_or_default()
            };
            row.push_str(&format!("\x1b[0m\x1b[{left}G{PANEL_RULE}{cells}\x1b[0m"));
        }
    }

    /// The panel's content needs drawing for this window width: its
    /// renderer and the width.
    fn panel_render_needed(&self, cols: usize) -> Option<(PanelRender, usize)> {
        let width = self.panel_width(cols)?;
        let panel = self.panel.as_ref()?;
        match &panel.rows {
            Some((drawn_for, _)) if *drawn_for == width => None,
            _ => Some((Arc::clone(&panel.render), width)),
        }
    }

    fn store_panel_rows(&mut self, render: &PanelRender, width: usize, rows: Vec<String>) {
        if let Some(panel) = self.panel.as_mut() {
            // The panel may have been replaced while it was drawn.
            if Arc::ptr_eq(&panel.render, render) {
                panel.rows = Some((width, Arc::new(rows)));
            }
        }
    }

    fn over_panel(&self, row: usize, col: usize) -> bool {
        self.layout
            .panel_left
            .is_some_and(|left| col >= left && row < self.layout.height)
    }

    fn scroll_panel(&mut self, scroll: Scroll) {
        let page = self.layout.height.saturating_sub(2).max(1);
        if let Some(panel) = self.panel.as_mut() {
            panel.scroll = match scroll {
                Scroll::Up(rows) => panel.scroll.saturating_sub(rows),
                Scroll::Down(rows) => panel.scroll + rows,
                Scroll::PageUp => panel.scroll.saturating_sub(page),
                Scroll::PageDown => panel.scroll + page,
                Scroll::Top => 0,
                Scroll::Bottom => usize::MAX,
            };
        }
    }

    /// A click on a ✕ (the panel's or the notice's); true when it hit one.
    fn click_close(&mut self, row: usize, col: usize) -> bool {
        let hit = |target: Option<(usize, usize)>| {
            target.is_some_and(|(r, c)| r == row && col + 1 >= c && col <= c + 1)
        };
        if hit(self.layout.panel_close) {
            self.panel_closed = true;
            return true;
        }
        if hit(self.layout.notice_close) {
            self.narrow_notice_dismissed = true;
            return true;
        }
        false
    }

    /// The conversation's lines, without the empty line the cursor waits
    /// on after the last newline (it would be a blank row above the pane).
    fn shown(&self) -> &[vt::Line] {
        let lines = if self.focus_mode {
            &self.focus.lines
        } else {
            &self.conversation.lines
        };
        match lines.last() {
            Some(last) if last.is_empty() => &lines[..lines.len() - 1],
            _ => lines,
        }
    }

    /// `height` rows of conversation `cols` wide: from the anchor when
    /// scrolled up, otherwise the end (top-aligned while it is short).
    fn conversation_rows(&mut self, height: usize, cols: usize) -> Vec<String> {
        let mut picked: Vec<(usize, usize, usize)> = Vec::new();
        if let Some((line, sub)) = self.anchor {
            picked = self.rows_forward(line, sub, height + 1, cols);
            if picked.len() <= height {
                // Scrolled back down to the end: follow it again.
                self.anchor = None;
                picked.clear();
            } else {
                picked.truncate(height);
            }
        }
        let lines = self.shown();
        if self.anchor.is_none() {
            'lines: for (index, line) in lines.iter().enumerate().rev() {
                for &(start, end) in vt::wrap_ranges(line, cols).iter().rev() {
                    if picked.len() == height {
                        break 'lines;
                    }
                    picked.push((index, start, end));
                }
            }
            picked.reverse();
        }
        let selected = self.selection.map(Selection::range);
        let mut frame: Vec<String> = picked
            .iter()
            .map(|&(index, start, end)| {
                let cells = &lines[index][start..end];
                match selected {
                    Some(((first_line, first_cell), (last_line, past_cell)))
                        if (first_line..=last_line).contains(&index) =>
                    {
                        let from = if index == first_line { first_cell } else { 0 };
                        let to = if index == last_line {
                            past_cell
                        } else {
                            usize::MAX
                        };
                        let from = from.clamp(start, end) - start;
                        let to = to.clamp(start, end) - start;
                        vt::render_cells(&vt::highlight(cells, from, to))
                    }
                    _ => vt::render_cells(cells),
                }
            })
            .collect();
        frame.resize(height, String::new());
        self.shown_rows = picked;
        frame
    }

    /// The text position under a screen cell. Below the conversation (over
    /// the input box) it is the end of the last row shown.
    fn position_at(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let last = self.shown_rows.len().checked_sub(1)?;
        let (index, start, end) = self.shown_rows[row.min(last)];
        if row > last || end == start {
            return Some((index, end.saturating_sub(1).max(start)));
        }
        // The cells that fit left of `col`; the next one is under it.
        let offset = vt::clip(&self.shown()[index][start..end], col).len();
        Some((index, (start + offset).min(end - 1)))
    }

    /// The selected text, one line per conversation line.
    fn selected_text(&self) -> String {
        let Some(((first_line, first_cell), (last_line, past_cell))) =
            self.selection.map(Selection::range)
        else {
            return String::new();
        };
        let lines = self.shown();
        (first_line..=last_line.min(lines.len().saturating_sub(1)))
            .map(|index| {
                let line = &lines[index];
                let from = if index == first_line { first_cell } else { 0 };
                let to = if index == last_line {
                    past_cell
                } else {
                    line.len()
                };
                let to = to.min(line.len());
                vt::plain(&line[from.min(to)..to]).trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Applies a mouse event; returns the text to copy when a drag ends.
    fn mouse(&mut self, event: Mouse) -> Option<String> {
        if let Mouse::Press { row, col } = event {
            if self.click_close(row, col) || self.over_panel(row, col) {
                return None;
            }
            // A click on "Jump to bottom" does what Ctrl+End does.
            if let Some((hint_row, from, to)) = self.layout.jump_hint {
                if row == hint_row && (from..to).contains(&col) {
                    self.anchor = None;
                    self.selection = None;
                    self.selecting = false;
                    return None;
                }
            }
        }
        // A drag into the panel selects up to the conversation's edge.
        let event = match (event, self.layout.panel_left) {
            (Mouse::Drag { row, col }, Some(left)) => Mouse::Drag {
                row,
                col: col.min(left.saturating_sub(1)),
            },
            (Mouse::Release { row, col }, Some(left)) => Mouse::Release {
                row,
                col: col.min(left.saturating_sub(1)),
            },
            (event, _) => event,
        };
        match event {
            Mouse::Press { row, col } => {
                self.selection = self.position_at(row, col).map(|position| Selection {
                    anchor: position,
                    head: position,
                });
                self.selecting = self.selection.is_some();
                None
            }
            Mouse::Drag { row, col } => {
                if !self.selecting {
                    return None;
                }
                // Dragging past the top or bottom edge scrolls.
                if row == 0 {
                    self.scroll(Scroll::Up(1));
                } else if row + 1 >= self.view.0 {
                    self.scroll(Scroll::Down(1));
                }
                let head = self.position_at(row, col);
                if let (Some(selection), Some(head)) = (self.selection.as_mut(), head) {
                    selection.head = head;
                }
                None
            }
            Mouse::Release { row, col } => {
                if !self.selecting {
                    return None;
                }
                self.selecting = false;
                let head = self.position_at(row, col);
                if let (Some(selection), Some(head)) = (self.selection.as_mut(), head) {
                    selection.head = head;
                }
                let selection = self.selection?;
                if selection.anchor == selection.head {
                    // A click, not a drag: nothing is selected.
                    self.selection = None;
                    return None;
                }
                let text = self.selected_text();
                (!text.is_empty()).then_some(text)
            }
        }
    }

    /// Up to `count` rows starting at row `sub` of line `line`, as (line,
    /// start cell, end cell).
    fn rows_forward(
        &self,
        line: usize,
        sub: usize,
        count: usize,
        cols: usize,
    ) -> Vec<(usize, usize, usize)> {
        let mut rows = Vec::new();
        for (index, cells) in self.shown().iter().enumerate().skip(line) {
            let ranges = vt::wrap_ranges(cells, cols);
            let skip = if index == line {
                sub.min(ranges.len() - 1)
            } else {
                0
            };
            for &(start, end) in &ranges[skip..] {
                if rows.len() == count {
                    return rows;
                }
                rows.push((index, start, end));
            }
        }
        rows
    }

    /// The first row shown while following the end, or `None` when the
    /// whole conversation fits.
    fn follow_top(&self, height: usize, cols: usize) -> Option<(usize, usize)> {
        let mut seen = 0;
        for (index, line) in self.shown().iter().enumerate().rev() {
            let count = vt::row_count(line, cols);
            if seen + count >= height {
                let sub = count - (height - seen);
                // A row above this one must exist to scroll to.
                return (index > 0 || sub > 0).then_some((index, sub));
            }
            seen += count;
        }
        None
    }

    fn step_back(
        &self,
        (mut line, mut sub): (usize, usize),
        rows: usize,
        cols: usize,
    ) -> (usize, usize) {
        for _ in 0..rows {
            if sub > 0 {
                sub -= 1;
            } else if line > 0 {
                line -= 1;
                sub = vt::row_count(&self.shown()[line], cols) - 1;
            } else {
                break;
            }
        }
        (line, sub)
    }

    fn step_forward(
        &self,
        (mut line, mut sub): (usize, usize),
        rows: usize,
        cols: usize,
    ) -> (usize, usize) {
        let lines = self.shown();
        for _ in 0..rows {
            let count = lines
                .get(line)
                .map_or(1, |cells| vt::row_count(cells, cols));
            if sub + 1 < count {
                sub += 1;
            } else if line + 1 < lines.len() {
                line += 1;
                sub = 0;
            } else {
                break;
            }
        }
        (line, sub)
    }

    fn scroll(&mut self, scroll: Scroll) {
        let (height, cols) = self.view;
        if height == 0 {
            return;
        }
        let page = height.saturating_sub(1).max(1);
        match scroll {
            Scroll::Bottom => self.anchor = None,
            Scroll::Top => {
                if self.follow_top(height, cols).is_some() {
                    self.anchor = Some((0, 0));
                }
            }
            Scroll::Up(_) | Scroll::PageUp => {
                let rows = if let Scroll::Up(rows) = scroll {
                    rows
                } else {
                    page
                };
                let top = match self.anchor {
                    Some(top) => Some(top),
                    None => self.follow_top(height, cols),
                };
                if let Some(top) = top {
                    self.anchor = Some(self.step_back(top, rows, cols));
                }
            }
            Scroll::Down(_) | Scroll::PageDown => {
                let rows = if let Scroll::Down(rows) = scroll {
                    rows
                } else {
                    page
                };
                if let Some(top) = self.anchor {
                    // Drawing goes back to following once the end is in view.
                    self.anchor = Some(self.step_forward(top, rows, cols));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mouse, Scroll, State, JUMP_HINT};

    #[test]
    fn focus_view_shows_the_last_prompt_and_focus_text_only() {
        let mut state = state_with(5);
        state.in_pane = true;
        state.feed("❯ do it\r\n".as_bytes());
        state.in_pane = false;
        state.commit_pane();
        state.feed(b"lots of tool output\n");
        state.in_focus = true;
        state.feed("● Bash(ls) ✓\nDone.\n".as_bytes());
        state.in_focus = false;

        state.set_focus_mode(true);
        let (frame, _) = state.compose(10, 40);
        let rows: Vec<String> = frame.iter().map(|row| plain(row)).collect();
        assert!(rows[0].contains("focus view"));
        assert_eq!(rows[1], "❯ do it");
        assert_eq!(rows[2], "● Bash(ls) ✓");
        assert_eq!(rows[3], "Done.");
        assert!(!rows.iter().any(|row| row.contains("tool output")));

        state.set_focus_mode(false);
        let (frame, _) = state.compose(10, 40);
        assert!(frame.iter().any(|row| plain(row) == "lots of tool output"));
    }

    #[test]
    fn dragging_selects_text_across_lines_and_highlights_it() {
        let mut state = state_with(3);
        state.compose(10, 40);
        // From "1" in "line 1" to the "n" of "line 2".
        assert_eq!(state.mouse(Mouse::Press { row: 1, col: 5 }), None);
        assert_eq!(state.mouse(Mouse::Drag { row: 2, col: 2 }), None);
        let copied = state.mouse(Mouse::Release { row: 2, col: 2 });
        assert_eq!(copied.as_deref(), Some("1\nlin"));

        let (frame, _) = state.compose(10, 40);
        assert!(frame[1].contains("\x1b[0;7m"), "selection is highlighted");
        assert!(!frame[0].contains(";7m"));
        assert_eq!(plain(&frame[1]), "line 1", "text itself is unchanged");
    }

    #[test]
    fn a_click_without_drag_selects_nothing_and_clears_the_old_selection() {
        let mut state = state_with(3);
        state.compose(10, 40);
        state.mouse(Mouse::Press { row: 0, col: 0 });
        assert!(state.mouse(Mouse::Release { row: 0, col: 3 }).is_some());
        state.mouse(Mouse::Press { row: 2, col: 1 });
        assert_eq!(state.mouse(Mouse::Release { row: 2, col: 1 }), None);
        assert_eq!(state.selection, None);
    }

    #[test]
    fn selection_ending_over_the_pane_runs_to_the_last_row() {
        let mut state = state_with(3);
        state.in_pane = true;
        state.feed("❯ ".as_bytes());
        state.in_pane = false;
        state.compose(10, 40);
        state.mouse(Mouse::Press { row: 2, col: 0 });
        // Rows 3.. are empty conversation rows and the pane.
        let copied = state.mouse(Mouse::Release { row: 9, col: 0 });
        assert_eq!(copied.as_deref(), Some("line 2"));
    }

    fn with_panel(state: &mut State, rows: usize) {
        let render: super::PanelRender = std::sync::Arc::new(move |width: usize| {
            (0..rows)
                .map(|n| format!("{:<width$}", format!("diff row {n}")))
                .collect()
        });
        state.panel = Some(super::Panel {
            name: "diff".to_string(),
            render,
            rows: None,
            scroll: 0,
        });
    }

    /// What the draw thread does before composing: draws the panel's rows.
    fn compose_with_panel(state: &mut State, rows: usize, cols: usize) -> Vec<String> {
        if let Some((render, width)) = state.panel_render_needed(cols) {
            let drawn = render(width);
            state.store_panel_rows(&render, width, drawn);
        }
        state
            .compose(rows, cols)
            .0
            .iter()
            .map(|row| plain(row))
            .collect()
    }

    #[test]
    fn a_wide_window_shows_the_panel_right_of_the_conversation() {
        let mut state = state_with(3);
        with_panel(&mut state, 30);
        let frame = compose_with_panel(&mut state, 10, 120);
        let width = 120 * 2 / 5;
        assert_eq!(state.layout.conversation_cols, 120 - width - 1);
        assert!(frame[0].starts_with("line 0"));
        assert!(frame[0].ends_with("✕ "), "the panel's first row has its ✕");
        assert!(frame[1].contains("│diff row 0"));
        assert!(frame[2].contains("line 2") && frame[2].contains("diff row 1"));

        // The wheel over the panel scrolls the panel, not the conversation.
        let left = state.layout.panel_left.expect("panel shown");
        state.scroll_panel(Scroll::Down(5));
        let frame = compose_with_panel(&mut state, 10, 120);
        assert!(frame[1].contains("diff row 5"));
        assert!(state.over_panel(3, left + 2));
        assert!(!state.over_panel(3, left - 1));

        // Its ✕ closes it and gives the conversation the whole width.
        let (row, col) = state.layout.panel_close.expect("close mark");
        assert_eq!(state.mouse(Mouse::Press { row, col }), None);
        assert!(state.panel_closed);
        let frame = compose_with_panel(&mut state, 10, 120);
        assert!(!frame.iter().any(|row| row.contains("diff row")));
        assert_eq!(state.layout.conversation_cols, 120);
    }

    #[test]
    fn clicking_the_jump_hint_goes_back_to_the_end() {
        let mut state = state_with(40);
        state.compose(10, 40);
        state.scroll(Scroll::Up(10));
        let frame = compose_with_panel(&mut state, 10, 40);
        let (row, from, _) = state.layout.jump_hint.expect("hint shown");
        assert!(frame[row].contains("Jump to bottom"));
        assert_eq!(state.mouse(Mouse::Press { row, col: from + 2 }), None);
        assert_eq!(state.anchor, None, "following the end again");
        let frame = compose_with_panel(&mut state, 10, 40);
        assert!(!frame.iter().any(|row| row.contains("Jump to bottom")));
        assert_eq!(state.selection, None, "the click selected nothing");
    }

    #[test]
    fn a_narrow_window_says_how_wide_it_must_be_until_dismissed() {
        let mut state = state_with(3);
        with_panel(&mut state, 30);
        let frame = compose_with_panel(&mut state, 12, 80);
        assert!(frame.iter().any(|row| row
            .contains("Resize your terminal to at least 110 columns to show the diff panel")));
        assert!(!frame.iter().any(|row| row.contains("diff row")));
        assert_eq!(state.layout.conversation_cols, 80);

        let (row, col) = state.layout.notice_close.expect("notice ✕");
        state.mouse(Mouse::Press { row, col });
        let frame = compose_with_panel(&mut state, 12, 80);
        assert!(!frame.iter().any(|row| row.contains("Resize your terminal")));
    }

    fn state_with(lines: usize) -> State {
        let mut state = State::new();
        for n in 0..lines {
            state.feed(format!("line {n}\n").as_bytes());
        }
        state
    }

    fn plain(row: &str) -> String {
        let mut out = String::new();
        let mut chars = row.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for ch in chars.by_ref() {
                    if ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn the_pane_stays_at_the_bottom_under_the_conversation() {
        let mut state = state_with(3);
        state.in_pane = true;
        state.feed("───\r\n❯ hi\r\n───\r\n  status".as_bytes());
        state.in_pane = false;
        let (frame, cursor) = state.compose(10, 40);
        let frame: Vec<String> = frame.iter().map(|row| plain(row)).collect();
        assert_eq!(frame.len(), 10);
        assert_eq!(frame[0], "line 0");
        assert_eq!(frame[2], "line 2");
        assert_eq!(frame[3], "", "short conversations are top-aligned");
        assert_eq!(frame[5], "");
        assert_eq!(&frame[6..], ["───", "❯ hi", "───", "  status"]);
        assert_eq!(cursor, Some((9, 8)));
    }

    #[test]
    fn scrolling_up_keeps_the_pane_and_shows_the_jump_hint() {
        let mut state = state_with(50);
        state.in_pane = true;
        state.feed("❯ ".as_bytes());
        state.in_pane = false;
        let (frame, _) = state.compose(10, 40);
        // Nine rows of conversation: lines 41..=49.
        assert_eq!(plain(&frame[0]), "line 41");
        assert_eq!(plain(&frame[9]), "❯ ");

        state.scroll(Scroll::Up(5));
        let (frame, _) = state.compose(10, 40);
        assert_eq!(plain(&frame[0]), "line 36");
        assert!(plain(&frame[8]).contains(JUMP_HINT.trim()));
        assert_eq!(plain(&frame[9]), "❯ ", "the input stays put");

        // New output does not move the view while scrolled up.
        state.feed(b"more\n");
        let (frame, _) = state.compose(10, 40);
        assert_eq!(plain(&frame[0]), "line 36");

        state.scroll(Scroll::Down(100));
        let (frame, _) = state.compose(10, 40);
        assert!(state.anchor.is_none(), "back at the end, following again");
        assert_eq!(plain(&frame[8]), "more");
    }

    #[test]
    fn page_keys_and_top_and_bottom() {
        let mut state = state_with(100);
        let _ = state.compose(11, 40);
        state.scroll(Scroll::Top);
        let (frame, _) = state.compose(11, 40);
        assert_eq!(plain(&frame[0]), "line 0");
        state.scroll(Scroll::PageDown);
        let (frame, _) = state.compose(11, 40);
        assert_eq!(plain(&frame[0]), "line 9");
        state.scroll(Scroll::Bottom);
        assert!(state.anchor.is_none());
        // Nothing to scroll when everything fits.
        let mut short = state_with(2);
        let _ = short.compose(11, 40);
        short.scroll(Scroll::PageUp);
        assert!(short.anchor.is_none());
    }

    #[test]
    fn long_lines_wrap_to_the_window_and_reflow() {
        let mut state = State::new();
        state.feed(format!("{}\n", "x".repeat(50)).as_bytes());
        let (frame, _) = state.compose(6, 20);
        assert_eq!(plain(&frame[0]), "x".repeat(20));
        assert_eq!(plain(&frame[2]), "x".repeat(10));
        let (frame, _) = state.compose(6, 60);
        assert_eq!(plain(&frame[0]), "x".repeat(50));
    }

    #[test]
    fn a_submitted_prompt_moves_into_the_conversation() {
        let mut state = state_with(1);
        state.in_pane = true;
        state.feed("───\r\n❯ hello\r\n".as_bytes());
        state.in_pane = false;
        state.commit_pane();
        state.feed(b"reply\n");
        let (frame, _) = state.compose(8, 40);
        let rows: Vec<String> = frame.iter().map(|row| plain(row)).collect();
        assert_eq!(&rows[..4], ["line 0", "───", "❯ hello", "reply"]);
    }

    #[test]
    fn a_tall_pane_leaves_the_conversation_some_rows() {
        let mut state = state_with(5);
        state.in_pane = true;
        for n in 0..20 {
            state.feed(format!("option {n}\r\n").as_bytes());
        }
        state.in_pane = false;
        let (frame, cursor) = state.compose(10, 40);
        // Two rows of conversation stay: the last two lines.
        assert_eq!(plain(&frame[0]), "line 3");
        assert_eq!(plain(&frame[1]), "line 4");
        assert_eq!(cursor, Some((9, 0)));
    }
}
