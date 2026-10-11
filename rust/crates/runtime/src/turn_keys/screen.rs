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
        let height = rows.saturating_sub(pane_rows.len());
        self.view = (height, cols);
        let mut frame = self.conversation_rows(height, cols);
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
        if let (Some(hint), Some(last)) = (hint, frame.last_mut()) {
            let width = hint.chars().count();
            let pad = cols.saturating_sub(width) / 2;
            *last = format!("{}\x1b[7m{hint}\x1b[0m", " ".repeat(pad));
        }
        let cursor = cursor.map(|(row, col)| (height + row, col));
        frame.extend(pane_rows);
        (frame, cursor)
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
