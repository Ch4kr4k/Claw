//! The block under the output during a turn, like Claude Code: output goes
//! into the terminal's history as usual, and the activity row, input box and
//! status line are drawn right below its last line. They move down as output
//! arrives until they reach the bottom of the screen, then stay there while
//! the output scrolls above them.
//!
//! While a turn runs, the process's stdout and stderr go into a pipe. A relay
//! thread erases the block, writes what arrived and draws the block again
//! below it, so output printed from anywhere lands above the block. A line
//! that is still being written (streamed text) is shown in the block until
//! its newline arrives. Instructions for the relay (stop drawing while a
//! prompt reads the keyboard, start again, finish) travel through the same
//! pipe, so they stay in order with the output before them.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;

/// An instruction for the relay inside the output: an APC string, which
/// terminals ignore should one ever get through.
pub(super) const MARK_START: &[u8] = b"\x1b_claw:";
pub(super) const MARK_END: &[u8] = b"\x1b\\";
/// Synchronized output: the terminal shows the erase and redraw at once.
const SYNC_START: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
/// An unfinished line taller than this is written above the block, so the
/// block always fits on the screen.
const MAX_PARTIAL_ROWS: usize = 4;

/// Set while stdout and stderr go through the relay.
static REDIRECTED: AtomicBool = AtomicBool::new(false);
static CURRENT: Mutex<Option<Redirect>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The running relay and the terminal's own stdout and stderr, to put back.
struct Redirect {
    relay: Arc<Relay>,
    stdout: OwnedFd,
    stderr: OwnedFd,
}

struct Relay {
    screen: Mutex<Screen>,
    /// Instructions sent and handled, so a sender can wait for its turn.
    sent: AtomicU64,
    handled: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Erase the block and pass output through untouched (a prompt is
    /// reading the keyboard and drawing itself).
    Pause,
    /// Draw the block again.
    Resume,
    /// The turn is over: erase the block for good.
    End,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::End => "end",
        }
    }

    fn parse(name: &[u8]) -> Option<Self> {
        [Self::Pause, Self::Resume, Self::End]
            .into_iter()
            .find(|command| command.name().as_bytes() == name)
    }
}

/// Whether output is going through the relay (a turn is running on a
/// terminal).
pub(super) fn active() -> bool {
    REDIRECTED.load(Ordering::SeqCst)
}

/// Sends stdout and stderr through the relay and draws the block.
pub(super) fn start() {
    if active() {
        return;
    }
    let _ = io::stdout().flush();
    let Ok(redirect) = redirect_output() else {
        return;
    };
    lock(&redirect.relay.screen).begin();
    *lock(&CURRENT) = Some(redirect);
    REDIRECTED.store(true, Ordering::SeqCst);
}

fn redirect_output() -> io::Result<Redirect> {
    let stdout = rustix::io::dup(io::stdout())?;
    let stderr = rustix::io::dup(io::stderr())?;
    let tty = File::from(stdout.try_clone()?);
    let (reader, writer) = io::pipe()?;
    let relay = Arc::new(Relay {
        screen: Mutex::new(Screen::new(Box::new(tty), Box::new(super::terminal_cols))),
        sent: AtomicU64::new(0),
        handled: AtomicU64::new(0),
    });
    let thread_relay = Arc::clone(&relay);
    thread::Builder::new()
        .name("claw-turn-output".to_string())
        .spawn(move || run_relay(&thread_relay, reader))?;
    rustix::stdio::dup2_stdout(&writer)?;
    if let Err(error) = rustix::stdio::dup2_stderr(&writer) {
        let _ = rustix::stdio::dup2_stdout(&stdout);
        return Err(error.into());
    }
    // Only stdout and stderr hold the pipe now: the relay sees its end once
    // they are put back (and any child that inherited them has exited).
    drop(writer);
    Ok(Redirect {
        relay,
        stdout,
        stderr,
    })
}

/// Erases the block and gives stdout and stderr back to the terminal.
pub(super) fn stop() {
    if !active() {
        return;
    }
    instruct(Command::End);
    let Some(redirect) = lock(&CURRENT).take() else {
        return;
    };
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    let _ = rustix::stdio::dup2_stdout(&redirect.stdout);
    let _ = rustix::stdio::dup2_stderr(&redirect.stderr);
    REDIRECTED.store(false, Ordering::SeqCst);
}

/// Erases the block until [`resume`], so a prompt can draw itself.
pub(super) fn pause() {
    instruct(Command::Pause);
}

pub(super) fn resume() {
    instruct(Command::Resume);
}

/// Draws the block again (animation, typing, a new activity).
pub(super) fn redraw() {
    let relay = lock(&CURRENT)
        .as_ref()
        .map(|redirect| Arc::clone(&redirect.relay));
    if let Some(relay) = relay {
        lock(&relay.screen).redraw();
    }
}

/// Sends `command` behind the output written so far and waits (briefly) for
/// the relay to handle it.
fn instruct(command: Command) {
    let relay = lock(&CURRENT)
        .as_ref()
        .map(|redirect| Arc::clone(&redirect.relay));
    let Some(relay) = relay else {
        return;
    };
    let _ = io::stderr().flush();
    let target = relay.sent.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut stdout = io::stdout().lock();
        let mut mark = MARK_START.to_vec();
        mark.extend_from_slice(command.name().as_bytes());
        mark.extend_from_slice(MARK_END);
        let _ = stdout.write_all(&mark);
        let _ = stdout.flush();
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while relay.handled.load(Ordering::SeqCst) < target && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
}

fn run_relay(relay: &Relay, mut pipe: impl Read) {
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
        for event in events {
            match event {
                Event::Output(bytes) => lock(&relay.screen).write_output(&bytes),
                Event::Mark(name) => {
                    if let Some(command) = Command::parse(&name) {
                        lock(&relay.screen).apply(command);
                    }
                    relay.handled.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }
    if !pending.is_empty() {
        lock(&relay.screen).write_output(&pending);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Output(Vec<u8>),
    /// The name inside an instruction mark.
    Mark(Vec<u8>),
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Splits what was read from the pipe into output and instructions. The
/// tail that may be the start of an instruction is returned, to be read
/// again with the next bytes.
pub(super) fn split_marks(data: &[u8]) -> (Vec<Event>, Vec<u8>) {
    let mut events = Vec::new();
    let push_output = |events: &mut Vec<Event>, bytes: &[u8]| {
        if !bytes.is_empty() {
            events.push(Event::Output(bytes.to_vec()));
        }
    };
    let mut rest = data;
    loop {
        let Some(at) = find(rest, MARK_START) else {
            let hold = (1..MARK_START.len())
                .rev()
                .find(|&len| rest.ends_with(&MARK_START[..len]))
                .unwrap_or(0);
            push_output(&mut events, &rest[..rest.len() - hold]);
            return (events, rest[rest.len() - hold..].to_vec());
        };
        let body = &rest[at + MARK_START.len()..];
        let Some(end) = find(body, MARK_END) else {
            push_output(&mut events, &rest[..at]);
            return (events, rest[at..].to_vec());
        };
        push_output(&mut events, &rest[..at]);
        events.push(Event::Mark(body[..end].to_vec()));
        rest = &body[end + MARK_END.len()..];
    }
}

/// Columns the text takes on screen: escape sequences take none, wide
/// characters two; after a carriage return, the longest stretch counts.
fn visible_width(bytes: &[u8]) -> usize {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars();
    let (mut widest, mut line) = (0, 0);
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for ch in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&ch) {
                            break;
                        }
                    }
                }
                // OSC, APC, DCS: up to BEL or ESC \.
                Some(']' | '_' | 'P') => {
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {
                widest = widest.max(line);
                line = 0;
            }
            '\t' => line = (line / 8 + 1) * 8,
            ch if ch.is_control() => {}
            ch => line += ch.width().unwrap_or(0),
        }
    }
    widest.max(line)
}

/// Screen rows a line `width` columns wide takes on a `cols`-wide terminal.
fn rows_for(width: usize, cols: usize) -> usize {
    width.div_ceil(cols.max(1)).max(1)
}

struct Screen {
    tty: Box<dyn Write + Send>,
    cols: Box<dyn Fn() -> usize + Send>,
    /// Output after the last newline, shown at the top of the block.
    partial: Vec<u8>,
    /// Width of each line of the block as drawn; empty when none is shown.
    drawn: Vec<usize>,
    /// Output goes straight through (paused, or the turn ended).
    passthrough: bool,
    ended: bool,
    /// While passing through: whether the last byte ended a line.
    at_line_start: bool,
}

impl Screen {
    fn new(tty: Box<dyn Write + Send>, cols: Box<dyn Fn() -> usize + Send>) -> Self {
        Self {
            tty,
            cols,
            partial: Vec::new(),
            drawn: Vec::new(),
            passthrough: false,
            ended: false,
            at_line_start: true,
        }
    }

    fn emit(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.tty.write_all(bytes);
            let _ = self.tty.flush();
        }
    }

    /// Hides the cursor (the input box draws its own) and draws the block.
    fn begin(&mut self) {
        let cols = (self.cols)();
        let mut out = b"\x1b[?25l".to_vec();
        self.draw(cols, &mut out);
        self.emit(&out);
    }

    /// Moves to where the block starts and clears it. The cursor is on the
    /// block's last line; after a resize, lines the terminal rewrapped take
    /// more (or fewer) rows.
    fn erase(&mut self, cols: usize, out: &mut Vec<u8>) {
        let rows: usize = self.drawn.iter().map(|&width| rows_for(width, cols)).sum();
        self.drawn.clear();
        if rows == 0 {
            return;
        }
        out.push(b'\r');
        if rows > 1 {
            out.extend_from_slice(format!("\x1b[{}A", rows - 1).as_bytes());
        }
        out.extend_from_slice(b"\x1b[J");
    }

    fn draw(&mut self, cols: usize, out: &mut Vec<u8>) {
        if !self.partial.is_empty() {
            out.extend_from_slice(&self.partial);
            out.extend_from_slice(b"\x1b[0m\r\n");
            self.drawn.push(visible_width(&self.partial));
        }
        // No autowrap: a row that is too wide is cut, never wrapped.
        out.extend_from_slice(b"\x1b[?7l");
        for (index, row) in super::block_rows(cols).iter().enumerate() {
            if index > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(row.as_bytes());
            self.drawn.push(visible_width(row.as_bytes()).min(cols));
        }
        out.extend_from_slice(b"\x1b[?7h");
    }

    fn write_output(&mut self, bytes: &[u8]) {
        if self.passthrough {
            if let Some(&last) = bytes.last() {
                self.at_line_start = last == b'\n';
            }
            self.emit(bytes);
            return;
        }
        let cols = (self.cols)();
        let mut out = SYNC_START.to_vec();
        self.erase(cols, &mut out);
        self.partial.extend_from_slice(bytes);
        if let Some(end) = self.partial.iter().rposition(|&byte| byte == b'\n') {
            out.extend(self.partial.drain(..=end));
        }
        if rows_for(visible_width(&self.partial), cols) > MAX_PARTIAL_ROWS {
            out.append(&mut self.partial);
            out.extend_from_slice(b"\r\n");
        }
        self.draw(cols, &mut out);
        out.extend_from_slice(SYNC_END);
        self.emit(&out);
    }

    fn redraw(&mut self) {
        if self.passthrough {
            return;
        }
        let cols = (self.cols)();
        let mut out = SYNC_START.to_vec();
        self.erase(cols, &mut out);
        self.draw(cols, &mut out);
        out.extend_from_slice(SYNC_END);
        self.emit(&out);
    }

    fn apply(&mut self, command: Command) {
        let cols = (self.cols)();
        let mut out = Vec::new();
        match command {
            Command::Pause | Command::End if !self.passthrough => {
                self.erase(cols, &mut out);
                let unfinished = !self.partial.is_empty();
                out.append(&mut self.partial);
                // A prompt starts on its own line; at the end, what follows
                // continues the unfinished line.
                self.at_line_start = !unfinished;
                if command == Command::Pause && unfinished {
                    out.extend_from_slice(b"\x1b[0m\r\n");
                    self.at_line_start = true;
                }
                out.extend_from_slice(b"\x1b[?25h");
                self.passthrough = true;
            }
            Command::Resume if self.passthrough && !self.ended => {
                if !self.at_line_start {
                    out.extend_from_slice(b"\r\n");
                }
                out.extend_from_slice(b"\x1b[?25l");
                self.passthrough = false;
                self.draw(cols, &mut out);
            }
            _ => {}
        }
        if command == Command::End {
            self.ended = true;
        }
        self.emit(&out);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{rows_for, split_marks, visible_width, Command, Event, Screen};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn take(&self) -> String {
            String::from_utf8_lossy(&std::mem::take(&mut *self.0.lock().unwrap())).to_string()
        }
    }

    static COLS: AtomicUsize = AtomicUsize::new(80);

    fn screen() -> (Screen, Captured) {
        let captured = Captured::default();
        let screen = Screen::new(
            Box::new(captured.clone()),
            Box::new(|| COLS.load(Ordering::SeqCst)),
        );
        (screen, captured)
    }

    #[test]
    fn instructions_are_split_from_output_even_across_reads() {
        let (events, rest) = split_marks(b"one\n\x1b_claw:pause\x1b\\two\x1b_cl");
        assert_eq!(
            events,
            vec![
                Event::Output(b"one\n".to_vec()),
                Event::Mark(b"pause".to_vec()),
                Event::Output(b"two".to_vec()),
            ]
        );
        assert_eq!(rest, b"\x1b_cl");
        let mut next = rest;
        next.extend_from_slice(b"aw:end\x1b\\");
        assert_eq!(
            split_marks(&next),
            (vec![Event::Mark(b"end".to_vec())], Vec::new())
        );
        // Ordinary escape sequences pass straight through.
        let (events, rest) = split_marks(b"\x1b[1mbold\x1b[0m");
        assert_eq!(events, vec![Event::Output(b"\x1b[1mbold\x1b[0m".to_vec())]);
        assert!(rest.is_empty());
    }

    #[test]
    fn width_ignores_escape_sequences_and_counts_wide_characters() {
        assert_eq!(visible_width(b"\x1b[1;36m\xe2\x9d\xaf\x1b[0m hi"), 4);
        assert_eq!(visible_width("日本".as_bytes()), 4);
        assert_eq!(visible_width(b"\x1b]8;;http://x\x07link\x1b]8;;\x07"), 4);
        assert_eq!(visible_width(b"long line\rab"), 9);
        assert_eq!(rows_for(0, 80), 1);
        assert_eq!(rows_for(81, 80), 2);
    }

    #[test]
    fn output_lands_above_the_block_and_unfinished_lines_stay_in_it() {
        COLS.store(80, Ordering::SeqCst);
        let (mut screen, captured) = screen();
        screen.begin();
        let first = captured.take();
        assert!(first.starts_with("\x1b[?25l"));
        let block_rows = screen.drawn.len();
        assert_eq!(block_rows, 6, "blank, activity, rule, input, rule, status");

        screen.write_output(b"hello");
        let out = captured.take();
        // Erases the six rows, then shows the unfinished line in the block.
        assert!(out.contains("\r\x1b[5A\x1b[J"), "{out:?}");
        assert!(out.contains("hello\x1b[0m\r\n"));
        assert_eq!(screen.drawn.len(), 7);

        screen.write_output(b" world\nnext");
        let out = captured.take();
        assert!(out.contains("\r\x1b[6A\x1b[J"), "{out:?}");
        assert!(out.contains("hello world\nnext\x1b[0m\r\n"), "{out:?}");
        assert_eq!(screen.partial, b"next");
    }

    #[test]
    fn erasing_after_a_resize_counts_rewrapped_rows() {
        COLS.store(80, Ordering::SeqCst);
        let (mut screen, captured) = screen();
        screen.write_output(&[b'x'; 70]);
        let _ = captured.take();
        // The 70-column line now takes two rows of 40.
        let mut out = Vec::new();
        screen.erase(40, &mut out);
        let out = String::from_utf8(out).unwrap();
        let rows: usize = out
            .trim_start_matches("\r\x1b[")
            .split('A')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(rows >= 7, "partial takes 2 rows, rules may rewrap: {out:?}");
    }

    #[test]
    fn pausing_hands_the_screen_to_a_prompt_and_resuming_draws_again() {
        COLS.store(80, Ordering::SeqCst);
        let (mut screen, captured) = screen();
        screen.begin();
        screen.write_output(b"partial");
        let _ = captured.take();

        screen.apply(Command::Pause);
        let out = captured.take();
        assert!(out.ends_with("partial\x1b[0m\r\n\x1b[?25h"), "{out:?}");
        screen.write_output(b"? Allow bash\r\n");
        assert_eq!(captured.take(), "? Allow bash\r\n", "passed through");
        screen.redraw();
        assert_eq!(captured.take(), "", "no block while paused");

        screen.apply(Command::Resume);
        let out = captured.take();
        assert!(
            out.starts_with("\x1b[?25l"),
            "already at a line start: {out:?}"
        );
        assert_eq!(screen.drawn.len(), 6);

        screen.apply(Command::End);
        let out = captured.take();
        assert!(out.ends_with("\x1b[J\x1b[?25h"), "{out:?}");
        screen.apply(Command::Resume);
        assert_eq!(captured.take(), "", "nothing is drawn after the end");
    }
}
