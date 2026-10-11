use std::fmt::Write as FmtWrite;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::cursor::{MoveToColumn, RestorePosition, SavePosition};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor, Stylize};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{as_24_bit_terminal_escaped, LinesWithEndings};

/// Named colour themes, selectable with `/theme`, the `theme` setting, or
/// `CLAW_THEME`. `NO_COLOR` selects `none` unless a theme is set explicitly.
/// Ported from the theme set in `AzureAiHarness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Dark,
    Light,
    Solarized,
    SolarizedLight,
    TokyoNight,
    None,
}

static CURRENT_THEME: AtomicU8 = AtomicU8::new(u8::MAX);

impl ThemeName {
    pub const ALL: [Self; 6] = [
        Self::Dark,
        Self::Light,
        Self::Solarized,
        Self::SolarizedLight,
        Self::TokyoNight,
        Self::None,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Solarized => "solarized-dark",
            Self::SolarizedLight => "solarized-light",
            Self::TokyoNight => "tokyo-night",
            Self::None => "none",
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Dark => "dark backgrounds (default)",
            Self::Light => "light backgrounds",
            Self::Solarized => "Solarized Dark terminal scheme",
            Self::SolarizedLight => "Solarized Light terminal scheme",
            Self::TokyoNight => "Tokyo Night (truecolor terminals)",
            Self::None => "no colours (also chosen when NO_COLOR is set)",
        }
    }

    pub fn parse(name: &str) -> Result<Self, String> {
        let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "dark" | "default" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            "solarized" | "solarized-dark" => Ok(Self::Solarized),
            "solarized-light" => Ok(Self::SolarizedLight),
            "tokyo-night" | "tokyonight" | "tokyo" => Ok(Self::TokyoNight),
            "none" | "no" | "off" | "mono" | "monochrome" | "plain" => Ok(Self::None),
            _ => Err(format!(
                "unknown theme `{name}`. Themes: {}",
                Self::ALL.map(Self::as_str).join(", ")
            )),
        }
    }

    /// Theme from `CLAW_THEME`, falling back to `none` under `NO_COLOR`.
    #[must_use]
    pub fn from_env() -> Self {
        if let Some(theme) = std::env::var("CLAW_THEME")
            .ok()
            .and_then(|name| Self::parse(&name).ok())
        {
            return theme;
        }
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            Self::None
        } else {
            Self::Dark
        }
    }

    fn syntax_theme_name(self) -> &'static str {
        match self {
            // syntect bundles no Tokyo Night; Ocean is the closest blue-grey.
            Self::Dark | Self::TokyoNight | Self::None => "base16-ocean.dark",
            Self::Light => "InspiredGitHub",
            Self::Solarized => "Solarized (dark)",
            Self::SolarizedLight => "Solarized (light)",
        }
    }
}

/// The active theme for this process (defaults to [`ThemeName::from_env`]).
#[must_use]
pub fn current_theme() -> ThemeName {
    match CURRENT_THEME.load(Ordering::Relaxed) {
        0 => ThemeName::Dark,
        1 => ThemeName::Light,
        2 => ThemeName::Solarized,
        3 => ThemeName::SolarizedLight,
        4 => ThemeName::TokyoNight,
        5 => ThemeName::None,
        _ => ThemeName::from_env(),
    }
}

/// Switches the theme used by every renderer created afterwards.
pub fn set_current_theme(theme: ThemeName) {
    let index = ThemeName::ALL
        .iter()
        .position(|candidate| *candidate == theme)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(0);
    CURRENT_THEME.store(index, Ordering::Relaxed);
    crossterm::style::force_color_output(theme != ThemeName::None);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
    heading: Color,
    emphasis: Color,
    strong: Color,
    inline_code: Color,
    link: Color,
    quote: Color,
    table_border: Color,
    code_block_border: Color,
    spinner_active: Color,
    spinner_done: Color,
    spinner_failed: Color,
    /// 256-colour background for code blocks; `None` disables it.
    code_background: Option<u8>,
    monochrome: bool,
}

impl Default for ColorTheme {
    fn default() -> Self {
        Self::for_theme(ThemeName::Dark)
    }
}

impl ColorTheme {
    #[must_use]
    pub fn for_theme(theme: ThemeName) -> Self {
        match theme {
            ThemeName::Dark => Self {
                heading: Color::Cyan,
                emphasis: Color::Magenta,
                strong: Color::Yellow,
                inline_code: Color::Green,
                link: Color::Blue,
                quote: Color::AnsiValue(245),
                table_border: Color::DarkCyan,
                code_block_border: Color::AnsiValue(245),
                spinner_active: Color::Blue,
                spinner_done: Color::Green,
                spinner_failed: Color::Red,
                code_background: Some(236),
                monochrome: false,
            },
            ThemeName::Light => Self {
                heading: Color::DarkBlue,
                emphasis: Color::DarkMagenta,
                strong: Color::DarkYellow,
                inline_code: Color::DarkGreen,
                link: Color::DarkBlue,
                quote: Color::AnsiValue(245),
                table_border: Color::DarkCyan,
                code_block_border: Color::Grey,
                spinner_active: Color::DarkBlue,
                spinner_done: Color::DarkGreen,
                spinner_failed: Color::DarkRed,
                code_background: Some(255),
                monochrome: false,
            },
            // Solarized maps the "bright" ANSI slots to its base greys, so
            // only the normal accent colours are safe to use.
            ThemeName::Solarized | ThemeName::SolarizedLight => Self {
                heading: Color::DarkBlue,
                emphasis: Color::DarkMagenta,
                strong: Color::DarkYellow,
                inline_code: Color::DarkCyan,
                link: Color::DarkBlue,
                quote: if theme == ThemeName::Solarized {
                    Color::Green
                } else {
                    Color::Cyan
                },
                table_border: Color::DarkCyan,
                code_block_border: Color::DarkCyan,
                spinner_active: Color::DarkBlue,
                spinner_done: Color::DarkGreen,
                spinner_failed: Color::DarkRed,
                code_background: None,
                monochrome: false,
            },
            // Palette from folke/tokyonight.nvim ("night" style).
            ThemeName::TokyoNight => Self {
                heading: Color::Rgb {
                    r: 0x7a,
                    g: 0xa2,
                    b: 0xf7,
                },
                emphasis: Color::Rgb {
                    r: 0xbb,
                    g: 0x9a,
                    b: 0xf7,
                },
                strong: Color::Rgb {
                    r: 0xff,
                    g: 0x9e,
                    b: 0x64,
                },
                inline_code: Color::Rgb {
                    r: 0x9e,
                    g: 0xce,
                    b: 0x6a,
                },
                link: Color::Rgb {
                    r: 0x7d,
                    g: 0xcf,
                    b: 0xff,
                },
                quote: Color::Rgb {
                    r: 0x56,
                    g: 0x5f,
                    b: 0x89,
                },
                table_border: Color::Rgb {
                    r: 0x3b,
                    g: 0x42,
                    b: 0x61,
                },
                code_block_border: Color::Rgb {
                    r: 0x56,
                    g: 0x5f,
                    b: 0x89,
                },
                spinner_active: Color::Rgb {
                    r: 0x7a,
                    g: 0xa2,
                    b: 0xf7,
                },
                spinner_done: Color::Rgb {
                    r: 0x9e,
                    g: 0xce,
                    b: 0x6a,
                },
                spinner_failed: Color::Rgb {
                    r: 0xf7,
                    g: 0x76,
                    b: 0x8e,
                },
                code_background: Some(235),
                monochrome: false,
            },
            ThemeName::None => Self {
                heading: Color::Reset,
                emphasis: Color::Reset,
                strong: Color::Reset,
                inline_code: Color::Reset,
                link: Color::Reset,
                quote: Color::Reset,
                table_border: Color::Reset,
                code_block_border: Color::Reset,
                spinner_active: Color::Reset,
                spinner_done: Color::Reset,
                spinner_failed: Color::Reset,
                code_background: None,
                monochrome: true,
            },
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Spinner {
    frame_index: usize,
}

impl Spinner {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        let frame = Self::FRAMES[self.frame_index % Self::FRAMES.len()];
        self.frame_index += 1;
        queue!(
            out,
            SavePosition,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_active),
            Print(format!("{frame} {label}")),
            ResetColor,
            RestorePosition
        )?;
        out.flush()
    }

    pub fn finish(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.frame_index = 0;
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_done),
            Print(format!("{}{label}\n", " ".repeat(OUTPUT_MARGIN))),
            ResetColor
        )?;
        out.flush()
    }

    pub fn fail(
        &mut self,
        label: &str,
        theme: &ColorTheme,
        out: &mut impl Write,
    ) -> io::Result<()> {
        self.frame_index = 0;
        execute!(
            out,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            SetForegroundColor(theme.spinner_failed),
            Print(format!("{}{label}\n", " ".repeat(OUTPUT_MARGIN))),
            ResetColor
        )?;
        out.flush()
    }
}

/// The escape sequence that colours the REPL's activity row in `theme`.
#[must_use]
pub fn activity_color(theme: ThemeName) -> String {
    let colors = ColorTheme::for_theme(theme);
    if colors.monochrome {
        String::new()
    } else {
        format!("{}", SetForegroundColor(colors.spinner_active))
    }
}

/// Blank columns kept on each side of replies and tool output.
pub const OUTPUT_MARGIN: usize = 2;

/// A writer that keeps output inside a left and right margin, the way Claude
/// Code indents its transcript. Text is word-wrapped to the space between the
/// margins (measured in visible columns, so colour codes and wide characters
/// are handled), and a wrapped line keeps the indentation, gutter (`|`, `!`,
/// `│`, `>`) or list-bullet indent of the line it continues. Colours carry
/// over to the continuation, but the margin itself is never painted.
///
/// When the output is not a terminal, everything passes through unchanged so
/// pipes, files and tests see the original bytes.
pub struct MarginWriter<W: Write> {
    inner: W,
    /// Visible columns available between the margins; `None` passes through.
    width: Option<usize>,
    left: usize,
    col: usize,
    at_line_start: bool,
    prefix: PrefixState,
    /// What a wrapped line repeats after the margin, and its width.
    continuation: String,
    continuation_width: usize,
    /// SGR (colour) sequences active on the current line.
    sgr: String,
    word: Vec<(String, usize)>,
    pending_spaces: usize,
    escape: Option<String>,
    partial_utf8: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixState {
    /// Leading spaces of the line.
    Leading,
    /// After a gutter or bullet character: its trailing spaces.
    AfterMark,
    Done,
}

impl<W: Write> MarginWriter<W> {
    /// Margins of [`OUTPUT_MARGIN`] when stdout is a terminal.
    pub fn for_stdout(inner: W) -> Self {
        let width = runtime::turn_keys::stdout_is_terminal()
            .then(|| crossterm::terminal::size().ok())
            .flatten()
            .map(|(cols, _)| usize::from(cols));
        Self::with_width(inner, width, OUTPUT_MARGIN)
    }

    /// `terminal_cols` of `None` passes everything through; `left` is the
    /// left margin (the right margin is always [`OUTPUT_MARGIN`]).
    pub fn with_width(inner: W, terminal_cols: Option<usize>, left: usize) -> Self {
        let width = terminal_cols.map(|cols| cols.saturating_sub(left + OUTPUT_MARGIN).max(20));
        Self {
            inner,
            width,
            left,
            col: 0,
            at_line_start: true,
            prefix: PrefixState::Leading,
            continuation: String::new(),
            continuation_width: 0,
            sgr: String::new(),
            word: Vec::new(),
            pending_spaces: 0,
            escape: None,
            partial_utf8: Vec::new(),
        }
    }

    pub fn into_inner(mut self) -> io::Result<W> {
        self.flush()?;
        let Self { inner, .. } = self;
        Ok(inner)
    }

    fn emit(&mut self, text: &str) -> io::Result<()> {
        self.inner.write_all(text.as_bytes())
    }

    /// Writes the left margin if nothing has been written on this line yet.
    fn start_line(&mut self) -> io::Result<()> {
        if self.at_line_start {
            self.at_line_start = false;
            if self.sgr.is_empty() {
                self.emit(&" ".repeat(self.left))?;
            } else {
                let margin = format!("\x1b[0m{}{}", " ".repeat(self.left), self.sgr);
                self.emit(&margin)?;
            }
        }
        Ok(())
    }

    fn wrap(&mut self) -> io::Result<()> {
        let continuation = format!(
            "\x1b[0m\n{}\x1b[2m{}\x1b[0m{}",
            " ".repeat(self.left),
            self.continuation,
            self.sgr
        );
        self.emit(&continuation)?;
        self.col = self.continuation_width;
        self.pending_spaces = 0;
        Ok(())
    }

    fn track_sgr(&mut self, sequence: &str) {
        if sequence.starts_with("\x1b[") && sequence.ends_with('m') {
            if sequence == "\x1b[0m" || sequence == "\x1b[m" {
                self.sgr.clear();
            } else {
                self.sgr.push_str(sequence);
            }
        }
    }

    /// Emits the pending spaces and word, wrapping first if they don't fit.
    fn flush_word(&mut self) -> io::Result<()> {
        let Some(width) = self.width else {
            return Ok(());
        };
        if self.word.is_empty() && self.pending_spaces == 0 {
            return Ok(());
        }
        self.start_line()?;
        let word_width = self.word.iter().map(|(_, w)| w).sum::<usize>();
        if word_width > 0
            && self.col + self.pending_spaces + word_width > width
            && self.col > self.continuation_width
        {
            self.wrap()?;
        } else if self.pending_spaces > 0 {
            let spaces = " ".repeat(self.pending_spaces);
            self.emit(&spaces)?;
            self.col += self.pending_spaces;
            self.pending_spaces = 0;
        }
        for (token, token_width) in std::mem::take(&mut self.word) {
            if token_width > 0
                && self.col + token_width > width
                && self.col > self.continuation_width
            {
                // A word longer than the line: break it.
                self.wrap()?;
            }
            if token_width == 0 {
                self.track_sgr(&token);
            }
            self.emit(&token)?;
            self.col += token_width;
        }
        Ok(())
    }

    fn end_line(&mut self, ending: &str) -> io::Result<()> {
        // Trailing spaces are dropped; everything else is kept.
        if self.word.iter().all(|(_, width)| *width == 0) {
            self.pending_spaces = 0;
        }
        self.flush_word()?;
        self.emit(ending)?;
        self.col = 0;
        self.at_line_start = true;
        if ending == "\n" {
            self.prefix = PrefixState::Leading;
            self.continuation.clear();
            self.continuation_width = 0;
        }
        Ok(())
    }

    fn push_char(&mut self, ch: char) -> io::Result<()> {
        // Escape sequences (colours, cursor moves) take no columns.
        if let Some(sequence) = &mut self.escape {
            sequence.push(ch);
            let done = match sequence.as_bytes() {
                [0x1b, b'[', .., last] => (0x40..=0x7e).contains(last) && sequence.len() > 2,
                [0x1b, b']', ..] => ch == '\x07' || sequence.ends_with("\x1b\\"),
                [0x1b, _] => !matches!(ch, '[' | ']'),
                _ => true,
            };
            if done {
                let sequence = self.escape.take().unwrap_or_default();
                self.word.push((sequence, 0));
            }
            return Ok(());
        }
        match ch {
            '\x1b' => self.escape = Some(String::from(ch)),
            '\n' => self.end_line("\n")?,
            '\r' => self.end_line("\r")?,
            ' ' | '\t' => {
                let spaces = if ch == '\t' { 4 - self.col % 4 } else { 1 };
                if self.prefix == PrefixState::Done {
                    self.flush_word()?;
                    self.pending_spaces += spaces;
                } else {
                    self.word.push((" ".repeat(spaces), spaces));
                    self.flush_word()?;
                    self.continuation.push_str(&" ".repeat(spaces));
                    self.continuation_width += spaces;
                }
            }
            _ => {
                let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if self.prefix == PrefixState::Leading && matches!(ch, '|' | '!' | '│' | '>') {
                    self.word.push((ch.to_string(), char_width));
                    self.flush_word()?;
                    self.continuation.push(ch);
                    self.continuation_width += char_width;
                    self.prefix = PrefixState::AfterMark;
                } else if self.prefix == PrefixState::Leading && matches!(ch, '-' | '*' | '•') {
                    self.word.push((ch.to_string(), char_width));
                    self.flush_word()?;
                    self.continuation.push(' ');
                    self.continuation_width += char_width;
                    self.prefix = PrefixState::AfterMark;
                } else {
                    self.prefix = PrefixState::Done;
                    self.word.push((ch.to_string(), char_width));
                }
            }
        }
        Ok(())
    }
}

impl<W: Write> Write for MarginWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.width.is_none() {
            return self.inner.write(buf);
        }
        self.partial_utf8.extend_from_slice(buf);
        let bytes = std::mem::take(&mut self.partial_utf8);
        let (text, rest) = match std::str::from_utf8(&bytes) {
            Ok(text) => (text, &[][..]),
            Err(error) if error.error_len().is_none() => {
                let (valid, rest) = bytes.split_at(error.valid_up_to());
                (std::str::from_utf8(valid).unwrap_or_default(), rest)
            }
            Err(_) => {
                // Not UTF-8: pass it through as-is.
                self.inner.write_all(&bytes)?;
                return Ok(buf.len());
            }
        };
        for ch in text.chars() {
            self.push_char(ch)?;
        }
        self.partial_utf8 = rest.to_vec();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.width.is_some() && self.escape.is_none() {
            // Show the unfinished word now; a continuation in the next write
            // is checked character by character.
            self.flush_word()?;
        }
        self.inner.flush()
    }
}

/// Prints `text` and a newline through a [`MarginWriter`] on stdout.
pub fn print_with_margins(text: &str) {
    let mut out = MarginWriter::for_stdout(io::stdout());
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// The running "thinking" animation, if any.
static THINKING: Mutex<Option<ThinkingAnimation>> = Mutex::new(None);

struct ThinkingAnimation {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Shows an animated, ASCII-only `[=    ] thinking 3s` line on a terminal
/// until [`stop_thinking`] or the next [`StopThinkingOnWrite`] output.
pub fn start_thinking(label: &str) {
    // In the REPL the activity row at the bottom shows this instead.
    if !runtime::turn_keys::stdout_is_terminal() || runtime::turn_keys::status_rows_active() {
        return;
    }
    let mut slot = THINKING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_some() {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let label = label.to_string();
    let color = ColorTheme::for_theme(current_theme()).spinner_active;
    let started = Instant::now();
    let Ok(handle) = thread::Builder::new()
        .name("claw-thinking".to_string())
        .spawn(move || {
            const WIDTH: usize = 6;
            let mut tick = 0_usize;
            while !thread_stop.load(Ordering::SeqCst) {
                // A block bouncing between the brackets.
                let step = tick % (2 * WIDTH - 2);
                let position = if step < WIDTH {
                    step
                } else {
                    2 * WIDTH - 2 - step
                };
                let bar = (0..WIDTH)
                    .map(|index| if index == position { '=' } else { ' ' })
                    .collect::<String>();
                let queued = runtime::turn_keys::queued_count();
                let queued = if queued > 0 {
                    format!(" · {queued} queued")
                } else {
                    String::new()
                };
                let line = format!("[{bar}] {label} {}s{queued}", started.elapsed().as_secs());
                let mut stdout = io::stdout().lock();
                let _ = write!(stdout, "\r\x1b[2K{}", line.with(color));
                let _ = stdout.flush();
                drop(stdout);
                tick += 1;
                thread::sleep(Duration::from_millis(120));
            }
        })
    else {
        return;
    };
    *slot = Some(ThinkingAnimation { stop, handle });
}

/// Stops the thinking animation and clears its line.
pub fn stop_thinking() {
    let animation = THINKING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(animation) = animation {
        animation.stop.store(true, Ordering::SeqCst);
        let _ = animation.handle.join();
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\r\x1b[2K");
        let _ = stdout.flush();
    }
}

/// A writer that stops the thinking animation before its first output, so
/// streamed text never mixes with the animation line.
pub struct StopThinkingOnWrite<W: Write>(pub W);

impl<W: Write> Write for StopThinkingOnWrite<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            stop_thinking();
        }
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered { next_index: u64 },
}

#[derive(Debug, Default, Clone, PartialEq)]
struct TableState {
    /// Column alignment from the `|:--|:-:|--:|` row.
    alignments: Vec<Alignment>,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_head: bool,
}

impl TableState {
    fn push_cell(&mut self) {
        let cell = self.current_cell.trim().to_string();
        self.current_row.push(cell);
        self.current_cell.clear();
    }

    fn finish_row(&mut self) {
        if self.current_row.is_empty() {
            return;
        }
        let row = std::mem::take(&mut self.current_row);
        if self.in_head {
            self.headers = row;
        } else {
            self.rows.push(row);
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
struct RenderState {
    emphasis: usize,
    strong: usize,
    heading_level: Option<u8>,
    quote: usize,
    list_stack: Vec<ListKind>,
    link_stack: Vec<LinkState>,
    table: Option<TableState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkState {
    destination: String,
    text: String,
}

impl RenderState {
    fn style_text(&self, text: &str, theme: &ColorTheme) -> String {
        let mut style = text.stylize();

        if matches!(self.heading_level, Some(1 | 2)) || self.strong > 0 {
            style = style.bold();
        }
        if self.emphasis > 0 {
            style = style.italic();
        }

        if let Some(level) = self.heading_level {
            style = match level {
                1 => style.with(theme.heading),
                2 => style.white(),
                3 => style.with(Color::Blue),
                _ => style.with(Color::Grey),
            };
        } else if self.strong > 0 {
            style = style.with(theme.strong);
        } else if self.emphasis > 0 {
            style = style.with(theme.emphasis);
        }

        if self.quote > 0 {
            style = style.with(theme.quote);
        }

        format!("{style}")
    }

    fn append_raw(&mut self, output: &mut String, text: &str) {
        if let Some(link) = self.link_stack.last_mut() {
            link.text.push_str(text);
        } else if let Some(table) = self.table.as_mut() {
            table.current_cell.push_str(text);
        } else {
            output.push_str(text);
        }
    }

    fn append_styled(&mut self, output: &mut String, text: &str, theme: &ColorTheme) {
        let styled = self.style_text(text, theme);
        self.append_raw(output, &styled);
    }
}

#[derive(Debug)]
pub struct TerminalRenderer {
    syntax_set: SyntaxSet,
    syntax_theme: Theme,
    color_theme: ColorTheme,
}

impl Default for TerminalRenderer {
    fn default() -> Self {
        Self::with_theme(current_theme())
    }
}

impl TerminalRenderer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_theme(theme: ThemeName) -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults().themes;
        let syntax_theme = themes
            .remove(theme.syntax_theme_name())
            .or_else(|| themes.remove("base16-ocean.dark"))
            .unwrap_or_default();
        Self {
            syntax_set,
            syntax_theme,
            color_theme: ColorTheme::for_theme(theme),
        }
    }

    #[must_use]
    pub fn color_theme(&self) -> &ColorTheme {
        &self.color_theme
    }

    #[must_use]
    pub fn render_markdown(&self, markdown: &str) -> String {
        let normalized = normalize_nested_fences(markdown);
        let mut output = String::new();
        let mut state = RenderState::default();
        let mut code_language = String::new();
        let mut code_buffer = String::new();
        let mut in_code_block = false;

        for event in Parser::new_ext(&normalized, Options::all()) {
            self.render_event(
                event,
                &mut state,
                &mut output,
                &mut code_buffer,
                &mut code_language,
                &mut in_code_block,
            );
        }

        output.trim_end().to_string()
    }

    #[must_use]
    pub fn markdown_to_ansi(&self, markdown: &str) -> String {
        self.render_markdown(markdown)
    }

    #[allow(clippy::too_many_lines)]
    fn render_event(
        &self,
        event: Event<'_>,
        state: &mut RenderState,
        output: &mut String,
        code_buffer: &mut String,
        code_language: &mut String,
        in_code_block: &mut bool,
    ) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                Self::start_heading(state, level as u8, output);
            }
            Event::End(TagEnd::Paragraph) => output.push_str("\n\n"),
            Event::Start(Tag::BlockQuote(..)) => self.start_quote(state, output),
            Event::End(TagEnd::BlockQuote(..)) => {
                state.quote = state.quote.saturating_sub(1);
                output.push('\n');
            }
            Event::End(TagEnd::Heading(..)) => {
                state.heading_level = None;
                output.push_str("\n\n");
            }
            Event::End(TagEnd::Item) | Event::SoftBreak | Event::HardBreak => {
                state.append_raw(output, "\n");
            }
            Event::Start(Tag::List(first_item)) => {
                let kind = match first_item {
                    Some(index) => ListKind::Ordered { next_index: index },
                    None => ListKind::Unordered,
                };
                state.list_stack.push(kind);
            }
            Event::End(TagEnd::List(..)) => {
                state.list_stack.pop();
                output.push('\n');
            }
            Event::Start(Tag::Item) => Self::start_item(state, output),
            Event::Start(Tag::CodeBlock(kind)) => {
                *in_code_block = true;
                *code_language = match kind {
                    CodeBlockKind::Indented => String::from("text"),
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                };
                code_buffer.clear();
                self.start_code_block(code_language, output);
            }
            Event::End(TagEnd::CodeBlock) => {
                self.finish_code_block(code_buffer, code_language, output);
                *in_code_block = false;
                code_language.clear();
                code_buffer.clear();
            }
            Event::Start(Tag::Emphasis) => state.emphasis += 1,
            Event::End(TagEnd::Emphasis) => state.emphasis = state.emphasis.saturating_sub(1),
            Event::Start(Tag::Strong) => state.strong += 1,
            Event::End(TagEnd::Strong) => state.strong = state.strong.saturating_sub(1),
            Event::Code(code) => {
                let rendered =
                    format!("{}", format!("`{code}`").with(self.color_theme.inline_code));
                state.append_raw(output, &rendered);
            }
            Event::Rule => output.push_str("---\n"),
            Event::Text(text) => {
                self.push_text(text.as_ref(), state, output, code_buffer, *in_code_block);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                state.append_raw(output, &html);
            }
            Event::FootnoteReference(reference) => {
                state.append_raw(output, &format!("[{reference}]"));
            }
            Event::TaskListMarker(done) => {
                state.append_raw(output, if done { "[x] " } else { "[ ] " });
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                state.append_raw(output, &math);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                state.link_stack.push(LinkState {
                    destination: dest_url.to_string(),
                    text: String::new(),
                });
            }
            Event::End(TagEnd::Link) => {
                if let Some(link) = state.link_stack.pop() {
                    let label = if link.text.is_empty() {
                        link.destination.clone()
                    } else {
                        link.text
                    };
                    let rendered = format!(
                        "{}",
                        format!("[{label}]({})", link.destination)
                            .underlined()
                            .with(self.color_theme.link)
                    );
                    state.append_raw(output, &rendered);
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let rendered = format!(
                    "{}",
                    format!("[image:{dest_url}]").with(self.color_theme.link)
                );
                state.append_raw(output, &rendered);
            }
            Event::Start(Tag::Table(alignments)) => {
                state.table = Some(TableState {
                    alignments,
                    ..TableState::default()
                });
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = state.table.take() {
                    output.push_str(&self.render_table(&table));
                    output.push_str("\n\n");
                }
            }
            Event::Start(Tag::TableHead) => {
                if let Some(table) = state.table.as_mut() {
                    table.in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(table) = state.table.as_mut() {
                    table.finish_row();
                    table.in_head = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(table) = state.table.as_mut() {
                    table.current_row.clear();
                    table.current_cell.clear();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(table) = state.table.as_mut() {
                    table.finish_row();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = state.table.as_mut() {
                    table.current_cell.clear();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = state.table.as_mut() {
                    table.push_cell();
                }
            }
            Event::Start(Tag::Paragraph | Tag::MetadataBlock(..) | _)
            | Event::End(TagEnd::Image | TagEnd::MetadataBlock(..) | _) => {}
        }
    }

    fn start_heading(state: &mut RenderState, level: u8, output: &mut String) {
        state.heading_level = Some(level);
        if !output.is_empty() {
            output.push('\n');
        }
    }

    fn start_quote(&self, state: &mut RenderState, output: &mut String) {
        state.quote += 1;
        let _ = write!(output, "{}", "│ ".with(self.color_theme.quote));
    }

    fn start_item(state: &mut RenderState, output: &mut String) {
        let depth = state.list_stack.len().saturating_sub(1);
        output.push_str(&"  ".repeat(depth));

        let marker = match state.list_stack.last_mut() {
            Some(ListKind::Ordered { next_index }) => {
                let value = *next_index;
                *next_index += 1;
                format!("{value}. ")
            }
            _ => "• ".to_string(),
        };
        output.push_str(&marker);
    }

    fn start_code_block(&self, code_language: &str, output: &mut String) {
        let label = if code_language.is_empty() {
            "code".to_string()
        } else {
            code_language.to_string()
        };
        let _ = writeln!(
            output,
            "{}",
            format!("╭─ {label}")
                .bold()
                .with(self.color_theme.code_block_border)
        );
    }

    fn finish_code_block(&self, code_buffer: &str, code_language: &str, output: &mut String) {
        output.push_str(&self.highlight_code(code_buffer, code_language));
        let _ = write!(
            output,
            "{}",
            "╰─".bold().with(self.color_theme.code_block_border)
        );
        output.push_str("\n\n");
    }

    fn push_text(
        &self,
        text: &str,
        state: &mut RenderState,
        output: &mut String,
        code_buffer: &mut String,
        in_code_block: bool,
    ) {
        if in_code_block {
            code_buffer.push_str(text);
        } else {
            state.append_styled(output, text, &self.color_theme);
        }
    }

    /// Draws a table as a box, like Claude Code: `┌┬┐` on top, `├┼┤` between
    /// rows, `└┴┘` at the bottom, cells aligned as the `:--:` row says. On a
    /// terminal the columns shrink to fit between the output margins and long
    /// cells wrap inside their column, so the borders never break.
    fn render_table(&self, table: &TableState) -> String {
        let column_count = table
            .rows
            .iter()
            .chain(std::iter::once(&table.headers))
            .map(Vec::len)
            .max()
            .unwrap_or(0);
        if column_count == 0 {
            return String::new();
        }

        let natural = (0..column_count)
            .map(|column| {
                table
                    .rows
                    .iter()
                    .chain(std::iter::once(&table.headers))
                    .filter_map(|row| row.get(column))
                    .map(|cell| visible_width(cell))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect::<Vec<_>>();
        let available = runtime::turn_keys::stdout_is_terminal()
            .then(|| crossterm::terminal::size().ok())
            .flatten()
            .map(|(cols, _)| usize::from(cols).saturating_sub(2 * OUTPUT_MARGIN));
        let widths = fit_column_widths(&natural, available);

        let paint = |text: &str| format!("{}", text.with(self.color_theme.table_border));
        let rule = |left: &str, middle: &str, right: &str| {
            let inner = widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join(middle);
            paint(&format!("{left}{inner}{right}"))
        };

        let mut lines = vec![rule("┌", "┬", "┐")];
        if !table.headers.is_empty() {
            lines.extend(self.render_table_row(&table.headers, &widths, &table.alignments, true));
            if !table.rows.is_empty() {
                lines.push(rule("├", "┼", "┤"));
            }
        }
        for (index, row) in table.rows.iter().enumerate() {
            if index > 0 {
                lines.push(rule("├", "┼", "┤"));
            }
            lines.extend(self.render_table_row(row, &widths, &table.alignments, false));
        }
        lines.push(rule("└", "┴", "┘"));
        lines.join("\n")
    }

    /// One table row; a row whose cells wrap takes several lines.
    fn render_table_row(
        &self,
        row: &[String],
        widths: &[usize],
        alignments: &[Alignment],
        is_header: bool,
    ) -> Vec<String> {
        let border = format!("{}", "│".with(self.color_theme.table_border));
        let cells = widths
            .iter()
            .enumerate()
            .map(|(index, width)| wrap_cell(row.get(index).map_or("", String::as_str), *width))
            .collect::<Vec<_>>();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);

        (0..height)
            .map(|line_index| {
                let mut line = border.clone();
                for (index, width) in widths.iter().enumerate() {
                    let text = cells[index].get(line_index).map_or("", String::as_str);
                    let padding = width.saturating_sub(visible_width(text));
                    let (before, after) =
                        match alignments.get(index).copied().unwrap_or(Alignment::None) {
                            Alignment::Right => (padding, 0),
                            Alignment::Center => (padding / 2, padding - padding / 2),
                            Alignment::Left | Alignment::None => (0, padding),
                        };
                    line.push(' ');
                    line.push_str(&" ".repeat(before));
                    if is_header {
                        let _ = write!(line, "{}", text.bold().with(self.color_theme.heading));
                    } else {
                        line.push_str(text);
                    }
                    line.push_str(&" ".repeat(after + 1));
                    line.push_str(&border);
                }
                line
            })
            .collect()
    }

    #[must_use]
    pub fn highlight_code(&self, code: &str, language: &str) -> String {
        let syntax = self
            .syntax_set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());
        if self.color_theme.monochrome {
            return code.to_string();
        }
        let background = self.color_theme.code_background;
        let mut syntax_highlighter = HighlightLines::new(syntax, &self.syntax_theme);
        let mut colored_output = String::new();

        for line in LinesWithEndings::from(code) {
            match syntax_highlighter.highlight_line(line, &self.syntax_set) {
                Ok(ranges) => {
                    let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                    colored_output.push_str(&apply_code_block_background(&escaped, background));
                }
                Err(_) => {
                    colored_output.push_str(&apply_code_block_background(line, background));
                }
            }
        }

        colored_output
    }

    fn code_block_header(&self, language: &str) -> String {
        let label = if language.is_empty() {
            "code"
        } else {
            language
        };
        format!(
            "{}\n",
            format!("╭─ {label}")
                .bold()
                .with(self.color_theme.code_block_border)
        )
    }

    fn code_block_footer(&self) -> String {
        format!("{}\n", "╰─".bold().with(self.color_theme.code_block_border))
    }

    /// Highlights `code` and returns only its last line, so a streamed code
    /// block can be printed line by line with correct multi-line state.
    fn last_highlighted_line(&self, code: &str, language: &str) -> String {
        self.highlight_code(code, language)
            .split_inclusive('\n')
            .next_back()
            .unwrap_or_default()
            .to_string()
    }

    pub fn stream_markdown(&self, markdown: &str, out: &mut impl Write) -> io::Result<()> {
        let rendered_markdown = self.markdown_to_ansi(markdown);
        write!(out, "{rendered_markdown}")?;
        if !rendered_markdown.ends_with('\n') {
            writeln!(out)?;
        }
        out.flush()
    }
}

/// Streams assistant markdown as it arrives, the way `AzureAiHarness` does:
/// text lines are printed word by word (each inline span once it is closed),
/// code blocks are framed and syntax-highlighted line by line, and tables are
/// held until the table ends so their columns can be aligned.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MarkdownStreamState {
    /// Text after the last newline, waiting for the rest of its line.
    pending: String,
    /// How much of `pending` has already been printed.
    emitted: usize,
    /// `## ` and the like when the line being streamed is a heading, so later
    /// chunks keep the heading style.
    heading_prefix: Option<String>,
    fence: Option<OpenFence>,
    table: Vec<String>,
    last_line_blank: bool,
    started: bool,
    /// An emoji was dropped at the end of the last chunk, so a space that
    /// starts the next chunk belongs to it.
    skip_space: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenFence {
    marker: FenceMarker,
    language: String,
    /// Code so far; re-highlighted as a whole so multi-line constructs
    /// (strings, comments) keep their colours.
    code: String,
}

impl MarkdownStreamState {
    #[must_use]
    pub fn push(&mut self, renderer: &TerminalRenderer, delta: &str) -> Option<String> {
        let delta = if self.skip_space {
            delta.strip_prefix(' ').unwrap_or(delta)
        } else {
            delta
        };
        let (text, dropped_at_end) = replace_emoji(delta);
        self.skip_space = dropped_at_end;
        self.pending.push_str(&text);
        let mut output = String::new();
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=newline).collect();
            self.render_line(renderer, line.trim_end_matches(['\n', '\r']), &mut output);
        }
        self.stream_partial_line(renderer, &mut output);
        self.emit(output)
    }

    /// Prints the complete words of the unfinished line, as long as no inline
    /// span (`code`, **bold**, a link) is still open. Code, tables, quotes and
    /// lines that might turn out to be a rule or fence wait for their newline.
    fn stream_partial_line(&mut self, renderer: &TerminalRenderer, output: &mut String) {
        if self.fence.is_some() || !self.table.is_empty() {
            return;
        }
        let pending = &self.pending;
        if self.emitted == 0 {
            let trimmed = pending.trim_start();
            let Some(first_word) = trimmed.split_whitespace().next() else {
                return;
            };
            let undecided = trimmed.starts_with(['|', '`', '~', '>'])
                || first_word.len() == trimmed.len()
                || (first_word.len() >= 3 && first_word.chars().all(|ch| "-*_=".contains(ch)));
            if undecided {
                return;
            }
        }
        let Some(cut) = pending
            .char_indices()
            .rev()
            .take_while(|(index, _)| *index > self.emitted)
            .find(|(index, ch)| ch.is_whitespace() && inline_spans_closed(&pending[..*index]))
            .map(|(index, _)| index)
        else {
            return;
        };

        let chunk = pending[self.emitted..cut].to_string();
        if self.emitted == 0 {
            let trimmed = chunk.trim_start();
            output.push_str(&chunk[..chunk.len() - trimmed.len()]);
            output.push_str(&renderer.render_markdown(trimmed));
            let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
            if hashes > 0 && trimmed[hashes..].starts_with(' ') {
                self.heading_prefix = Some(format!("{} ", "#".repeat(hashes)));
            }
            self.last_line_blank = false;
        } else {
            output.push_str(&self.render_continuation(renderer, &chunk));
        }
        self.emitted = cut;
    }

    /// Renders a later chunk of a streamed line: its leading whitespace kept,
    /// and markers at its start (`- `, `1.`, `#`) not taken as new blocks.
    fn render_continuation(&self, renderer: &TerminalRenderer, chunk: &str) -> String {
        let rest = chunk.trim_start();
        if rest.is_empty() {
            return String::new();
        }
        let lead = &chunk[..chunk.len() - rest.len()];
        let markdown = match &self.heading_prefix {
            Some(prefix) => format!("{prefix}{rest}"),
            None => escape_block_start(rest),
        };
        format!("{lead}{}", renderer.render_markdown(&markdown))
    }

    #[must_use]
    pub fn flush(&mut self, renderer: &TerminalRenderer) -> Option<String> {
        let mut output = String::new();
        let pending = std::mem::take(&mut self.pending);
        if !pending.trim().is_empty() {
            self.render_line(renderer, &pending, &mut output);
        }
        self.finish_table(renderer, &mut output);
        if self.fence.take().is_some() {
            output.push_str(&renderer.code_block_footer());
        }
        self.last_line_blank = false;
        self.emit(output)
    }

    /// The first output of a reply replaces the "Thinking..." spinner line on
    /// a terminal, and starts on a line of its own when output is piped.
    fn emit(&mut self, output: String) -> Option<String> {
        if output.is_empty() {
            return None;
        }
        if !self.started {
            self.started = true;
            let separator = if runtime::turn_keys::stdout_is_terminal() {
                "\r\x1b[2K"
            } else {
                "\n"
            };
            return Some(format!("{separator}{output}"));
        }
        Some(output)
    }

    fn render_line(&mut self, renderer: &TerminalRenderer, line: &str, output: &mut String) {
        if self.emitted > 0 {
            // Finish a line whose beginning was already streamed.
            let rest = line.get(self.emitted..).unwrap_or_default().to_string();
            output.push_str(&self.render_continuation(renderer, &rest));
            output.push('\n');
            self.emitted = 0;
            self.heading_prefix = None;
            self.last_line_blank = false;
            return;
        }
        if let Some(fence) = &mut self.fence {
            if line_closes_fence(line, fence.marker) {
                self.fence = None;
                output.push_str(&renderer.code_block_footer());
            } else {
                fence.code.push_str(line);
                fence.code.push('\n');
                output.push_str(&renderer.last_highlighted_line(&fence.code, &fence.language));
            }
            self.last_line_blank = false;
            return;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with('|') {
            self.table.push(line.to_string());
            return;
        }
        self.finish_table(renderer, output);

        if let Some(marker) = parse_fence_opener(line) {
            let language = trimmed[marker.length..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            output.push_str(&renderer.code_block_header(&language));
            self.fence = Some(OpenFence {
                marker,
                language,
                code: String::new(),
            });
            self.last_line_blank = false;
            return;
        }

        if trimmed.is_empty() {
            if !self.last_line_blank {
                output.push('\n');
            }
            self.last_line_blank = true;
            return;
        }

        // Render the line on its own, keeping its indentation so nested
        // list items stay nested.
        output.push_str(&line[..line.len() - trimmed.len()]);
        output.push_str(&renderer.render_markdown(trimmed));
        output.push('\n');
        self.last_line_blank = false;
    }

    fn finish_table(&mut self, renderer: &TerminalRenderer, output: &mut String) {
        if self.table.is_empty() {
            return;
        }
        let table = std::mem::take(&mut self.table).join("\n");
        output.push_str(&renderer.render_markdown(&table));
        output.push('\n');
        self.last_line_blank = false;
    }
}

/// Replaces emoji, which many terminal fonts cannot draw, with ASCII: check
/// marks become `+` (a list bullet at line start), crosses `x`, warnings `!`;
/// other emoji are dropped together with the space after them. Also returns
/// whether an emoji was dropped at the very end (its space may come next).
fn replace_emoji(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut dropped_at_end = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let replacement = match ch {
            '\u{2705}' | '\u{2714}' | '\u{2713}' | '\u{2611}' => Some("+"),
            '\u{274c}' | '\u{274e}' | '\u{2716}' | '\u{2717}' | '\u{2718}' => Some("x"),
            '\u{26a0}' | '\u{2757}' | '\u{203c}' => Some("!"),
            '\u{fe0f}' | '\u{200d}' => Some(""),
            '\u{1f000}'..='\u{1faff}' | '\u{2600}'..='\u{27bf}' | '\u{2b00}'..='\u{2bff}' => None,
            _ => {
                out.push(ch);
                continue;
            }
        };
        match replacement {
            Some(ascii) => out.push_str(ascii),
            None => {
                while chars.peek() == Some(&'\u{fe0f}') {
                    chars.next();
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
                dropped_at_end = chars.peek().is_none();
            }
        }
    }
    (out, dropped_at_end)
}

/// Whether `text` leaves no inline span open: backticks, `**`, single `*`
/// and links all closed. A leading `* ` list marker is not emphasis.
fn inline_spans_closed(text: &str) -> bool {
    let trimmed = text.trim_start();
    let text = trimmed.strip_prefix("* ").unwrap_or(trimmed);
    let strong = text.matches("**").count();
    let single_stars = text.matches('*').count() - 2 * strong;
    let link_open = text.rfind('[').is_some_and(|open| {
        let tail = &text[open..];
        match (tail.find(']'), tail.find("](")) {
            (None, _) => true,
            (Some(_), Some(target)) => !tail[target..].contains(')'),
            (Some(_), None) => false,
        }
    });
    text.matches('`').count().is_multiple_of(2)
        && strong.is_multiple_of(2)
        && single_stars.is_multiple_of(2)
        && !link_open
}

/// Escapes a block marker at the start of `text` (`- `, `1.`, `#`, `>`), so
/// a chunk from the middle of a line renders as plain inline text.
fn escape_block_start(text: &str) -> String {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && text[digits..].starts_with(['.', ')']) {
        return format!("{}\\{}", &text[..digits], &text[digits..]);
    }
    if text.starts_with(['#', '>', '-', '+', '*', '=', '|']) && !text.starts_with("**") {
        return format!("\\{text}");
    }
    text.to_string()
}

fn apply_code_block_background(line: &str, background: Option<u8>) -> String {
    let trimmed = line.trim_end_matches('\n');
    let trailing_newline = if trimmed.len() == line.len() {
        ""
    } else {
        "\n"
    };
    let Some(bg) = background else {
        return format!("{trimmed}\u{1b}[0m{trailing_newline}");
    };
    let with_background = trimmed.replace("\u{1b}[0m", &format!("\u{1b}[0;48;5;{bg}m"));
    // `ESC[K` with the background still set paints the rest of the row, so
    // the block's background spans the full terminal width.
    format!("\u{1b}[48;5;{bg}m{with_background}\u{1b}[K\u{1b}[0m{trailing_newline}")
}

/// Pre-process raw markdown so that fenced code blocks whose body contains
/// fence markers of equal or greater length are wrapped with a longer fence.
///
/// LLMs frequently emit triple-backtick code blocks that contain triple-backtick
/// examples.  `CommonMark` (and pulldown-cmark) treats the inner marker as the
/// closing fence, breaking the render.  This function detects the situation and
/// upgrades the outer fence to use enough backticks (or tildes) that the inner
/// markers become ordinary content.
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::manual_repeat_n,
    clippy::manual_str_repeat
)]
fn normalize_nested_fences(markdown: &str) -> String {
    // A fence line is either "labeled" (has an info string ⇒ always an opener)
    // or "bare" (no info string ⇒ could be opener or closer).
    #[derive(Debug, Clone)]
    struct FenceLine {
        char: char,
        len: usize,
        has_info: bool,
        indent: usize,
    }

    fn parse_fence_line(line: &str) -> Option<FenceLine> {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        let indent = trimmed.chars().take_while(|c| *c == ' ').count();
        if indent > 3 {
            return None;
        }
        let rest = &trimmed[indent..];
        let ch = rest.chars().next()?;
        if ch != '`' && ch != '~' {
            return None;
        }
        let len = rest.chars().take_while(|c| *c == ch).count();
        if len < 3 {
            return None;
        }
        let after = &rest[len..];
        if ch == '`' && after.contains('`') {
            return None;
        }
        let has_info = !after.trim().is_empty();
        Some(FenceLine {
            char: ch,
            len,
            has_info,
            indent,
        })
    }

    let lines: Vec<&str> = markdown.split_inclusive('\n').collect();
    // Handle final line that may lack trailing newline.
    // split_inclusive already keeps the original chunks, including a
    // final chunk without '\n' if the input doesn't end with one.

    // First pass: classify every line.
    let fence_info: Vec<Option<FenceLine>> = lines.iter().map(|l| parse_fence_line(l)).collect();

    // Second pass: pair openers with closers using a stack, recording
    // (opener_idx, closer_idx) pairs plus the max fence length found between
    // them.
    struct StackEntry {
        line_idx: usize,
        fence: FenceLine,
    }

    let mut stack: Vec<StackEntry> = Vec::new();
    // Paired blocks: (opener_line, closer_line, max_inner_fence_len)
    let mut pairs: Vec<(usize, usize, usize)> = Vec::new();

    for (i, fi) in fence_info.iter().enumerate() {
        let Some(fl) = fi else { continue };

        if fl.has_info {
            // Labeled fence ⇒ always an opener.
            stack.push(StackEntry {
                line_idx: i,
                fence: fl.clone(),
            });
        } else {
            // Bare fence ⇒ try to close the top of the stack if compatible.
            let closes_top = stack
                .last()
                .is_some_and(|top| top.fence.char == fl.char && fl.len >= top.fence.len);
            if closes_top {
                let opener = stack.pop().unwrap();
                // Find max fence length of any fence line strictly between
                // opener and closer (these are the nested fences).
                let inner_max = fence_info[opener.line_idx + 1..i]
                    .iter()
                    .filter_map(|fi| fi.as_ref().map(|f| f.len))
                    .max()
                    .unwrap_or(0);
                pairs.push((opener.line_idx, i, inner_max));
            } else {
                // Treat as opener.
                stack.push(StackEntry {
                    line_idx: i,
                    fence: fl.clone(),
                });
            }
        }
    }

    // Determine which lines need rewriting.  A pair needs rewriting when
    // its opener length <= max inner fence length.
    struct Rewrite {
        char: char,
        new_len: usize,
        indent: usize,
    }
    let mut rewrites: std::collections::HashMap<usize, Rewrite> = std::collections::HashMap::new();

    for (opener_idx, closer_idx, inner_max) in &pairs {
        let opener_fl = fence_info[*opener_idx].as_ref().unwrap();
        if opener_fl.len <= *inner_max {
            let new_len = inner_max + 1;
            let info_part = {
                let trimmed = lines[*opener_idx]
                    .trim_end_matches('\n')
                    .trim_end_matches('\r');
                let rest = &trimmed[opener_fl.indent..];
                rest[opener_fl.len..].to_string()
            };
            rewrites.insert(
                *opener_idx,
                Rewrite {
                    char: opener_fl.char,
                    new_len,
                    indent: opener_fl.indent,
                },
            );
            let closer_fl = fence_info[*closer_idx].as_ref().unwrap();
            rewrites.insert(
                *closer_idx,
                Rewrite {
                    char: closer_fl.char,
                    new_len,
                    indent: closer_fl.indent,
                },
            );
            // Store info string only in the opener; closer keeps the trailing
            // portion which is already handled through the original line.
            // Actually, we rebuild both lines from scratch below, including
            // the info string for the opener.
            let _ = info_part; // consumed in rebuild
        }
    }

    if rewrites.is_empty() {
        return markdown.to_string();
    }

    // Rebuild.
    let mut out = String::with_capacity(markdown.len() + rewrites.len() * 4);
    for (i, line) in lines.iter().enumerate() {
        if let Some(rw) = rewrites.get(&i) {
            let fence_str: String = std::iter::repeat(rw.char).take(rw.new_len).collect();
            let indent_str: String = std::iter::repeat(' ').take(rw.indent).collect();
            // Recover the original info string (if any) and trailing newline.
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            let fi = fence_info[i].as_ref().unwrap();
            let info = &trimmed[fi.indent + fi.len..];
            let trailing = &line[trimmed.len()..];
            out.push_str(&indent_str);
            out.push_str(&fence_str);
            out.push_str(info);
            out.push_str(trailing);
        } else {
            out.push_str(line);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FenceMarker {
    character: char,
    length: usize,
}

fn parse_fence_opener(line: &str) -> Option<FenceMarker> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let character = rest.chars().next()?;
    if character != '`' && character != '~' {
        return None;
    }
    let length = rest.chars().take_while(|c| *c == character).count();
    if length < 3 {
        return None;
    }
    let info_string = &rest[length..];
    if character == '`' && info_string.contains('`') {
        return None;
    }
    Some(FenceMarker { character, length })
}

fn line_closes_fence(line: &str, opener: FenceMarker) -> bool {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let length = rest.chars().take_while(|c| *c == opener.character).count();
    if length < opener.length {
        return false;
    }
    rest[length..].chars().all(|c| c == ' ' || c == '\t')
}

fn visible_width(input: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(strip_ansi(input).as_str())
}

/// Column widths that fit a table in `available` columns: each column keeps
/// its natural width while the table fits; otherwise the widest columns give
/// up space first, down to a small minimum.
fn fit_column_widths(natural: &[usize], available: Option<usize>) -> Vec<usize> {
    const MIN_COLUMN: usize = 6;
    let mut widths = natural.to_vec();
    let Some(available) = available else {
        return widths;
    };
    // `│ cell │ cell │`: three border/padding columns per cell, plus one.
    let chrome = 3 * widths.len() + 1;
    let room = available.saturating_sub(chrome);
    while widths.iter().sum::<usize>() > room {
        let Some((widest, width)) = widths
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|&(_, width)| width)
        else {
            break;
        };
        if width <= MIN_COLUMN {
            break;
        }
        widths[widest] = width - 1;
    }
    widths
}

/// Splits a cell into lines of at most `width` columns, breaking at spaces
/// and cutting words longer than a line. A cell that fits is kept as is,
/// styling included; a wrapped cell loses its inline colours.
fn wrap_cell(cell: &str, width: usize) -> Vec<String> {
    if visible_width(cell) <= width {
        return vec![cell.to_string()];
    }
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;
    for word in strip_ansi(cell).split_whitespace() {
        let mut word = word.to_string();
        loop {
            let word_width = unicode_width::UnicodeWidthStr::width(word.as_str());
            let gap = usize::from(line_width > 0);
            if line_width + gap + word_width <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(&word);
                line_width += gap + word_width;
                break;
            }
            if line_width > 0 {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
                continue;
            }
            // A word longer than the column: cut it.
            let mut head = String::new();
            let mut head_width = 0;
            let mut rest = word.chars();
            for ch in rest.by_ref() {
                let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if head_width + ch_width > width && !head.is_empty() {
                    let tail: String = std::iter::once(ch).chain(rest).collect();
                    lines.push(head);
                    word = tail;
                    head = String::new();
                    break;
                }
                head.push(ch);
                head_width += ch_width;
            }
            if !head.is_empty() {
                line = head;
                line_width = head_width;
                break;
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            output.push(ch);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{strip_ansi, MarkdownStreamState, Spinner, TerminalRenderer};

    fn margined(text: &str, cols: usize) -> String {
        let mut out = super::MarginWriter::with_width(Vec::new(), Some(cols), 2);
        out.write_all(text.as_bytes()).expect("write");
        String::from_utf8(out.into_inner().expect("flush")).expect("utf8")
    }

    #[test]
    fn margin_writer_indents_and_word_wraps_inside_both_margins() {
        // 30 columns: 2 + 26 + 2.
        let out = margined("the quick brown fox jumps over the lazy dog again\n", 30);
        let lines = out.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "{out:?}");
        assert_eq!(super::strip_ansi(lines[0]), "  the quick brown fox jumps");
        assert_eq!(super::strip_ansi(lines[1]), "  over the lazy dog again");
        assert!(lines.iter().all(|line| super::visible_width(line) <= 28));
    }

    #[test]
    fn margin_writer_repeats_tool_gutters_and_breaks_long_words() {
        let out = margined(&format!("  | {}\n", "x".repeat(40)), 30);
        let lines = out.lines().map(super::strip_ansi).collect::<Vec<_>>();
        assert_eq!(lines[0], format!("    | {}", "x".repeat(22)));
        assert_eq!(lines[1], format!("    | {}", "x".repeat(18)));
    }

    #[test]
    fn margin_writer_keeps_bullet_indent_and_handles_split_writes() {
        let mut out = super::MarginWriter::with_width(Vec::new(), Some(30), 2);
        for chunk in ["- one two three ", "four five six seven", " eight\n"] {
            out.write_all(chunk.as_bytes()).expect("write");
            out.flush().expect("flush");
        }
        let out = String::from_utf8(out.into_inner().expect("flush")).expect("utf8");
        let lines = out.lines().map(super::strip_ansi).collect::<Vec<_>>();
        assert_eq!(lines[0], "  - one two three four five");
        assert_eq!(lines[1], "    six seven eight");
    }

    #[test]
    fn margin_writer_passes_through_when_not_a_terminal() {
        let mut out = super::MarginWriter::with_width(Vec::new(), None, 2);
        out.write_all(b"\x1b[1mraw\x1b[0m text\n").expect("write");
        assert_eq!(
            out.into_inner().expect("flush"),
            b"\x1b[1mraw\x1b[0m text\n"
        );
    }

    #[test]
    fn margin_writer_ignores_colour_codes_when_measuring() {
        let out = margined(
            "\x1b[31mred words that keep going and going on\x1b[0m\n",
            30,
        );
        let lines = out.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "{out:?}");
        assert!(
            lines[1].contains("\x1b[31m"),
            "colour carries over: {out:?}"
        );
    }

    #[test]
    fn tokyo_night_and_solarized_dark_parse() {
        assert_eq!(
            super::ThemeName::parse("tokyo-night"),
            Ok(super::ThemeName::TokyoNight)
        );
        assert_eq!(
            super::ThemeName::parse("Tokyo_Night"),
            Ok(super::ThemeName::TokyoNight)
        );
        assert_eq!(
            super::ThemeName::parse("solarized-dark").map(super::ThemeName::as_str),
            Ok("solarized-dark")
        );
        assert_eq!(
            super::ThemeName::parse("solarized"),
            Ok(super::ThemeName::Solarized)
        );
    }

    #[test]
    fn renders_markdown_with_styling_and_lists() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output = terminal_renderer
            .render_markdown("# Heading\n\nThis is **bold** and *italic*.\n\n- item\n\n`code`");

        assert!(markdown_output.contains("Heading"));
        assert!(markdown_output.contains("• item"));
        assert!(markdown_output.contains("code"));
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn renders_links_as_colored_markdown_labels() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.render_markdown("See [Claw](https://example.com/docs) now.");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("[Claw](https://example.com/docs)"));
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn highlights_fenced_code_blocks() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.markdown_to_ansi("```rust\nfn hi() { println!(\"hi\"); }\n```");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("╭─ rust"));
        assert!(plain_text.contains("fn hi"));
        assert!(markdown_output.contains('\u{1b}'));
        assert!(markdown_output.contains("[48;5;236m"));
    }

    #[test]
    fn renders_ordered_and_nested_lists() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.render_markdown("1. first\n2. second\n   - nested\n   - child");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("1. first"));
        assert!(plain_text.contains("2. second"));
        assert!(plain_text.contains("  • nested"));
        assert!(plain_text.contains("  • child"));
    }

    #[test]
    fn renders_tables_with_alignment() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output = terminal_renderer
            .render_markdown("| Name | Value |\n| ---- | ----- |\n| alpha | 1 |\n| beta | 22 |");
        let plain_text = strip_ansi(&markdown_output);
        let lines = plain_text.lines().collect::<Vec<_>>();

        assert_eq!(lines[0], "┌───────┬───────┐");
        assert_eq!(lines[1], "│ Name  │ Value │");
        assert_eq!(lines[2], "├───────┼───────┤");
        assert_eq!(lines[3], "│ alpha │ 1     │");
        assert_eq!(lines[4], "├───────┼───────┤");
        assert_eq!(lines[5], "│ beta  │ 22    │");
        assert_eq!(lines[6], "└───────┴───────┘");
        assert!(markdown_output.contains('\u{1b}'));
    }

    #[test]
    fn table_columns_follow_markdown_alignment() {
        let renderer = TerminalRenderer::new();
        let output =
            strip_ansi(&renderer.render_markdown(
                "| L | C | R |\n|:--|:-:|--:|\n| a | b | c |\n| long | mid | 100 |",
            ));
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines[3], "│ a    │  b  │   c │");
        assert_eq!(lines[5], "│ long │ mid │ 100 │");
    }

    #[test]
    fn wide_tables_shrink_and_wrap_inside_their_columns() {
        let widths = super::fit_column_widths(&[4, 60], Some(40));
        assert_eq!(widths.iter().sum::<usize>() + 3 * 2 + 1, 40);
        assert_eq!(widths[0], 4, "the narrow column keeps its width");
        let lines = super::wrap_cell("one two three four five six", 9);
        assert_eq!(lines, vec!["one two", "three", "four five", "six"]);
        assert_eq!(
            super::wrap_cell("abcdefghij", 4),
            vec!["abcd", "efgh", "ij"]
        );
        assert_eq!(super::fit_column_widths(&[10, 10], None), vec![10, 10]);
    }

    fn plain(output: Option<String>) -> String {
        strip_ansi(&output.unwrap_or_default())
    }

    /// A stream past its first output, so results carry no line separator.
    fn started_stream() -> MarkdownStreamState {
        MarkdownStreamState {
            started: true,
            ..MarkdownStreamState::default()
        }
    }

    #[test]
    fn streaming_state_starts_piped_output_on_its_own_line() {
        let renderer = TerminalRenderer::new();
        let mut state = MarkdownStreamState::default();

        assert_eq!(plain(state.push(&renderer, "hello\n")), "\nhello\n");
        assert_eq!(plain(state.push(&renderer, "again\n")), "again\n");
    }

    #[test]
    fn streaming_state_renders_each_line_when_it_completes() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        assert_eq!(state.push(&renderer, "# Head"), None);
        let heading = plain(state.push(&renderer, "ing\nPara"));
        assert!(heading.contains("Heading"));
        assert!(!heading.contains("Para"), "partial line is held back");
        assert!(plain(state.flush(&renderer)).contains("Para"));
    }

    #[test]
    fn streaming_state_frames_code_blocks_line_by_line() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        assert!(plain(state.push(&renderer, "```rust\n")).contains("╭─ rust"));
        let line = plain(state.push(&renderer, "fn main() {}\n"));
        assert!(line.contains("fn main() {}"));
        assert!(!line.contains("╭─"), "only the new line is emitted");
        assert!(plain(state.push(&renderer, "```\n")).contains("╰─"));
    }

    #[test]
    fn streaming_state_keeps_inner_fences_inside_a_longer_outer_fence() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        assert!(plain(state.push(&renderer, "````markdown\n")).contains("╭─ markdown"));
        assert!(plain(state.push(&renderer, "```rust\n")).contains("```rust"));
        assert!(plain(state.push(&renderer, "```\n")).contains("```"));
        assert!(plain(state.push(&renderer, "````\n")).contains("╰─"));
    }

    #[test]
    fn streaming_state_distinguishes_backtick_and_tilde_fences() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        let _ = state.push(&renderer, "~~~text\n");
        let inner = plain(state.push(&renderer, "```\nstill inside tilde fence\n"));
        assert!(inner.contains("still inside tilde fence"));
        assert!(
            !inner.contains("╰─"),
            "a backtick fence cannot close a tilde fence"
        );
        assert!(plain(state.push(&renderer, "~~~\n")).contains("╰─"));
    }

    #[test]
    fn streaming_state_holds_tables_until_they_end() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        assert_eq!(
            state.push(&renderer, "| Port | Service |\n|---|---|\n"),
            None
        );
        assert_eq!(state.push(&renderer, "| 22 | sshd |\n"), None);
        let table = plain(state.push(&renderer, "Done\n"));
        assert!(table.contains("sshd"));
        assert!(table.contains("Done"));
    }

    #[test]
    fn streaming_state_prints_complete_words_before_the_line_ends() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        assert_eq!(
            plain(state.push(&renderer, "The /etc directory hol")),
            "The /etc directory"
        );
        assert_eq!(plain(state.push(&renderer, "ds `syst")), " holds");
        let rest = plain(state.push(&renderer, "emctl` units.\n"));
        assert_eq!(rest, " `systemctl` units.\n");
    }

    #[test]
    fn streaming_state_keeps_list_and_heading_style_across_chunks() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        let first = plain(state.push(&renderer, "- first part "));
        assert_eq!(first, "• first part");
        let rest = plain(state.push(&renderer, "- not a new item\n"));
        assert_eq!(rest, " - not a new item\n");

        let heading = plain(state.push(&renderer, "## Disk usage report"));
        assert_eq!(heading, "Disk usage");
        assert_eq!(plain(state.push(&renderer, "\n")), " report\n");
    }

    #[test]
    fn streaming_state_waits_for_lines_that_may_be_rules_or_fences() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        assert_eq!(state.push(&renderer, "--- "), None);
        assert_eq!(state.push(&renderer, "``` "), None);
    }

    #[test]
    fn streaming_state_replaces_emoji_with_ascii() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        let output = plain(state.push(
            &renderer,
            "\u{2705} Confirm path\n\u{1f50d} Why it failed\n\u{26a0}\u{fe0f} careful\n",
        ));
        assert_eq!(output, "\u{2022} Confirm path\nWhy it failed\n! careful\n");

        // The space after an emoji may arrive in the next chunk.
        let _ = state.push(&renderer, "Done \u{1f680}");
        let rest = plain(state.push(&renderer, " now\n"));
        assert!(rest.ends_with("now\n") && !rest.contains("  "), "{rest:?}");
    }

    #[test]
    fn streaming_state_collapses_repeated_blank_lines() {
        let renderer = TerminalRenderer::new();
        let mut state = started_stream();

        let output = plain(state.push(&renderer, "one\n\n\n\ntwo\n"));
        assert_eq!(output, "one\n\ntwo\n");
    }

    #[test]
    fn renders_nested_fenced_code_block_preserves_inner_markers() {
        let terminal_renderer = TerminalRenderer::new();
        let markdown_output =
            terminal_renderer.markdown_to_ansi("````markdown\n```rust\nfn nested() {}\n```\n````");
        let plain_text = strip_ansi(&markdown_output);

        assert!(plain_text.contains("╭─ markdown"));
        assert!(plain_text.contains("```rust"));
        assert!(plain_text.contains("fn nested()"));
    }

    #[test]
    fn spinner_advances_frames() {
        let terminal_renderer = TerminalRenderer::new();
        let mut spinner = Spinner::new();
        let mut out = Vec::new();
        spinner
            .tick("Working", terminal_renderer.color_theme(), &mut out)
            .expect("tick succeeds");
        spinner
            .tick("Working", terminal_renderer.color_theme(), &mut out)
            .expect("tick succeeds");

        let output = String::from_utf8_lossy(&out);
        assert!(output.contains("Working"));
    }
}
