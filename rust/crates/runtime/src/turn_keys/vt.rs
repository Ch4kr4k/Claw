//! A small terminal emulator for full-screen mode. Output that would have
//! gone to the terminal is fed in as bytes (text, colours, cursor moves,
//! erases) and kept as lines of styled cells, so the screen can be drawn
//! again at any size and scrolled.
//!
//! Two kinds of buffer use it: the conversation, whose lines are not
//! wrapped (they are wrapped when drawn, so they reflow on resize), and the
//! bottom pane (the input box or a prompt), which wraps at the terminal
//! width like a real terminal so cursor-relative redraws land where the
//! drawing code expects.

use std::fmt::Write as _;

use unicode_width::UnicodeWidthChar;

const BOLD: u8 = 1;
const DIM: u8 = 2;
const ITALIC: u8 = 4;
const UNDERLINE: u8 = 8;
const REVERSE: u8 = 16;
const STRIKE: u8 = 32;

/// Colours: 0 is the default, 1..=256 an indexed colour plus one, and
/// `RGB | 0xRRGGBB` a true colour.
const RGB: u32 = 0x0100_0000;

/// Text attributes of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Style {
    fg: u32,
    bg: u32,
    flags: u8,
}

impl Style {
    fn apply_sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            *self = Self::default();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            let param = params[i];
            match param {
                0 => *self = Self::default(),
                1 => self.flags |= BOLD,
                2 => self.flags |= DIM,
                3 => self.flags |= ITALIC,
                4 => self.flags |= UNDERLINE,
                7 => self.flags |= REVERSE,
                9 => self.flags |= STRIKE,
                22 => self.flags &= !(BOLD | DIM),
                23 => self.flags &= !ITALIC,
                24 => self.flags &= !UNDERLINE,
                27 => self.flags &= !REVERSE,
                29 => self.flags &= !STRIKE,
                30..=37 => self.fg = u32::from(param - 30) + 1,
                90..=97 => self.fg = u32::from(param - 90 + 8) + 1,
                39 => self.fg = 0,
                40..=47 => self.bg = u32::from(param - 40) + 1,
                100..=107 => self.bg = u32::from(param - 100 + 8) + 1,
                49 => self.bg = 0,
                38 | 48 => {
                    let (color, used) = extended_color(&params[i + 1..]);
                    if let Some(color) = color {
                        if param == 38 {
                            self.fg = color;
                        } else {
                            self.bg = color;
                        }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// The SGR sequence that sets exactly this style.
    fn sgr(self) -> String {
        let mut out = String::from("\x1b[0");
        for (flag, code) in [
            (BOLD, 1),
            (DIM, 2),
            (ITALIC, 3),
            (UNDERLINE, 4),
            (REVERSE, 7),
            (STRIKE, 9),
        ] {
            if self.flags & flag != 0 {
                let _ = write!(out, ";{code}");
            }
        }
        for (color, base) in [(self.fg, 38), (self.bg, 48)] {
            match color {
                0 => {}
                1..=256 => {
                    let _ = write!(out, ";{base};5;{}", color - 1);
                }
                _ => {
                    let rgb = color & 0x00ff_ffff;
                    let _ = write!(
                        out,
                        ";{base};2;{};{};{}",
                        rgb >> 16,
                        (rgb >> 8) & 0xff,
                        rgb & 0xff
                    );
                }
            }
        }
        out.push('m');
        out
    }
}

/// `5;n` or `2;r;g;b` after a 38/48: the colour and how many parameters it
/// took.
fn extended_color(params: &[u16]) -> (Option<u32>, usize) {
    match params.first() {
        Some(5) => (params.get(1).map(|&index| u32::from(index.min(255)) + 1), 2),
        Some(2) if params.len() >= 4 => {
            let [r, g, b] = [params[1], params[2], params[3]].map(|c| u32::from(c.min(255)));
            (Some(RGB | (r << 16) | (g << 8) | b), 4)
        }
        _ => (None, params.len()),
    }
}

/// One column of text. A wide character takes two cells: itself (width 2)
/// and a continuation (width 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Cell {
    ch: char,
    /// A combining character drawn on `ch` (an accent, or the row of an
    /// inline image placeholder); `'\0'` for none.
    mark: char,
    width: u8,
    style: Style,
}

const BLANK: Cell = Cell {
    ch: ' ',
    mark: '\0',
    width: 1,
    style: Style {
        fg: 0,
        bg: 0,
        flags: 0,
    },
};

pub(super) type Line = Vec<Cell>;

/// Lines of cells written to like a terminal.
#[derive(Debug, Clone)]
pub(super) struct Buffer {
    pub(super) lines: Vec<Line>,
    pub(super) row: usize,
    pub(super) col: usize,
    style: Style,
    saved: (usize, usize),
    /// Wrap at this many columns (the pane); `None` keeps lines whole.
    wrap: Option<usize>,
    pub(super) cursor_visible: bool,
    /// Bytes of an unfinished escape sequence or character.
    pending: Vec<u8>,
    /// Sequences meant for the terminal itself (modes, queries, clipboard,
    /// title), to be written to it as they are.
    pub(super) passthrough: Vec<u8>,
    /// Every kitty graphics command (inline images) seen: the alternate
    /// screen keeps its images apart, so they are sent again when the
    /// conversation is printed to the normal screen.
    pub(super) graphics: Vec<u8>,
}

impl Buffer {
    pub(super) fn new(wrap: Option<usize>) -> Self {
        Self {
            lines: Vec::new(),
            row: 0,
            col: 0,
            style: Style::default(),
            saved: (0, 0),
            wrap: wrap.map(|width| width.max(1)),
            cursor_visible: true,
            pending: Vec::new(),
            passthrough: Vec::new(),
            graphics: Vec::new(),
        }
    }

    pub(super) fn set_wrap(&mut self, width: usize) {
        if self.wrap.is_some() {
            self.wrap = Some(width.max(1));
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut i = 0;
        while i < data.len() {
            let byte = data[i];
            if byte == 0x1b {
                match self.escape(&data[i..]) {
                    Some(len) => i += len,
                    None => {
                        self.pending = data[i..].to_vec();
                        return;
                    }
                }
            } else if byte < 0x20 || byte == 0x7f {
                self.control(byte);
                i += 1;
            } else {
                let start = i;
                while i < data.len() && data[i] >= 0x20 && data[i] != 0x7f && data[i] != 0x1b {
                    i += 1;
                }
                let run = &data[start..i];
                match std::str::from_utf8(run) {
                    Ok(text) => self.print(text),
                    Err(error) => {
                        let valid = error.valid_up_to();
                        self.print(&String::from_utf8_lossy(&run[..valid]));
                        if error.error_len().is_none() && i == data.len() {
                            // A character cut off by the read: keep it.
                            self.pending = run[valid..].to_vec();
                            return;
                        }
                        self.print(&String::from_utf8_lossy(&run[valid..]));
                    }
                }
            }
        }
    }

    fn print(&mut self, text: &str) {
        for ch in text.chars() {
            let width = ch.width().unwrap_or(0);
            if width > 0 {
                #[allow(clippy::cast_possible_truncation)]
                self.put(ch, width as u8);
            } else {
                self.mark(ch);
            }
        }
    }

    /// Puts a combining character on the cell before the cursor; a second
    /// one on the same cell is dropped.
    fn mark(&mut self, mark: char) {
        let col = self.col;
        let Some(line) = self.lines.get_mut(self.row) else {
            return;
        };
        let cell = line.iter_mut().take(col).rev().find(|cell| cell.width > 0);
        if let Some(cell) = cell.filter(|cell| cell.mark == '\0' && !mark.is_control()) {
            cell.mark = mark;
        }
    }

    fn put(&mut self, ch: char, width: u8) {
        if let Some(wrap) = self.wrap {
            if self.col + usize::from(width) > wrap {
                self.row += 1;
                self.col = 0;
            }
        }
        self.ensure_row();
        let style = self.style;
        let col = self.col;
        let line = &mut self.lines[self.row];
        if line.len() < col {
            line.resize(col, BLANK);
        }
        set_cell(
            line,
            col,
            Cell {
                ch,
                mark: '\0',
                width,
                style,
            },
        );
        if width == 2 {
            set_cell(
                line,
                col + 1,
                Cell {
                    ch: ' ',
                    mark: '\0',
                    width: 0,
                    style,
                },
            );
        }
        self.col += usize::from(width);
    }

    fn control(&mut self, byte: u8) {
        match byte {
            b'\r' => self.col = 0,
            // Output is written in cooked mode, where a newline also returns
            // the cursor; raw-mode writers send `\r\n` anyway.
            b'\n' => {
                self.row += 1;
                self.col = 0;
                self.ensure_row();
            }
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => {
                let next = (self.col / 8 + 1) * 8;
                let next = self.wrap.map_or(next, |wrap| next.min(wrap));
                while self.col < next {
                    self.put(' ', 1);
                }
            }
            _ => {}
        }
    }

    /// Handles the escape sequence at the start of `s`; returns its length,
    /// or `None` when it is not complete yet.
    fn escape(&mut self, s: &[u8]) -> Option<usize> {
        let kind = *s.get(1)?;
        match kind {
            b'[' => {
                let end = s[2..]
                    .iter()
                    .position(|b| (0x40..=0x7e).contains(b))
                    .map(|offset| offset + 2);
                let Some(end) = end else {
                    // Give up on absurdly long sequences instead of waiting.
                    return (s.len() > 256).then_some(s.len());
                };
                self.csi(&s[2..end], s[end], &s[..=end]);
                Some(end + 1)
            }
            b']' | b'_' | b'P' | b'^' | b'X' => {
                let mut j = 2;
                let (end, body_end) = loop {
                    let byte = *s.get(j)?;
                    if byte == 0x07 {
                        break (j + 1, j);
                    }
                    if byte == 0x1b && *s.get(j + 1)? == b'\\' {
                        break (j + 2, j);
                    }
                    j += 1;
                };
                // Window titles, the clipboard (OSC 52) and kitty graphics
                // (inline images, `ESC _ G`) go to the terminal; hyperlinks
                // and other strings are dropped.
                let body = &s[2..body_end];
                if kind == b'_' && body.starts_with(b"G") {
                    self.graphics.extend_from_slice(&s[..end]);
                    self.passthrough.extend_from_slice(&s[..end]);
                } else if kind == b']' && !body.starts_with(b"8;") {
                    self.passthrough.extend_from_slice(&s[..end]);
                }
                Some(end)
            }
            b'7' => {
                self.saved = (self.row, self.col);
                Some(2)
            }
            b'8' => {
                (self.row, self.col) = self.saved;
                self.ensure_row();
                Some(2)
            }
            b'D' => {
                self.row += 1;
                self.ensure_row();
                Some(2)
            }
            b'M' => {
                self.row = self.row.saturating_sub(1);
                Some(2)
            }
            b'E' => {
                self.control(b'\n');
                Some(2)
            }
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' => (s.len() >= 3).then_some(3),
            _ => Some(2),
        }
    }

    fn csi(&mut self, params: &[u8], final_byte: u8, raw: &[u8]) {
        let (prefix, params) = match params.first() {
            Some(&p @ (b'?' | b'>' | b'<' | b'=')) => (Some(p), &params[1..]),
            _ => (None, params),
        };
        let numbers: Vec<u16> = if params.is_empty() {
            Vec::new()
        } else {
            params
                .split(|&b| b == b';' || b == b':')
                .map(|part| {
                    std::str::from_utf8(part)
                        .ok()
                        .and_then(|text| text.parse().ok())
                        .unwrap_or(0)
                })
                .collect()
        };
        if let Some(prefix) = prefix {
            if prefix == b'?' && matches!(final_byte, b'h' | b'l') {
                // The screen owns the cursor, wrapping, the alternate screen
                // and the mouse; other modes (bracketed paste, …) go through.
                let mut forward = Vec::new();
                for &mode in &numbers {
                    match mode {
                        25 => self.cursor_visible = final_byte == b'h',
                        7 | 47 | 1000 | 1002 | 1003 | 1006 | 1015 | 1047 | 1048 | 1049 | 2026 => {}
                        _ => forward.push(mode.to_string()),
                    }
                }
                if !forward.is_empty() {
                    let _ = write!(
                        PassthroughWriter(&mut self.passthrough),
                        "\x1b[?{}{}",
                        forward.join(";"),
                        final_byte as char
                    );
                }
            } else {
                // Keyboard protocol, queries and the like.
                self.passthrough.extend_from_slice(raw);
            }
            return;
        }
        let first = numbers.first().copied().unwrap_or(0);
        let count = usize::from(first.max(1));
        match final_byte {
            b'm' => self.style.apply_sgr(&numbers),
            b'A' => self.row = self.row.saturating_sub(count),
            b'B' => {
                self.row += count;
                self.ensure_row();
            }
            b'C' => {
                self.col += count;
                if let Some(wrap) = self.wrap {
                    self.col = self.col.min(wrap.saturating_sub(1));
                }
            }
            b'D' => self.col = self.col.saturating_sub(count),
            b'G' | b'`' => self.col = count - 1,
            b'E' => {
                self.row += count;
                self.col = 0;
                self.ensure_row();
            }
            b'F' => {
                self.row = self.row.saturating_sub(count);
                self.col = 0;
            }
            b'H' | b'f' => {
                // Absolute rows only mean something in the pane.
                if self.wrap.is_some() {
                    self.row = usize::from(first.max(1)) - 1;
                    self.ensure_row();
                }
                self.col = usize::from(numbers.get(1).copied().unwrap_or(1).max(1)) - 1;
            }
            b'K' => self.erase_line(first),
            b'J' => self.erase_display(first),
            b's' => self.saved = (self.row, self.col),
            b'u' => {
                (self.row, self.col) = self.saved;
                self.ensure_row();
            }
            b'c' | b'n' => self.passthrough.extend_from_slice(raw),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        self.ensure_row();
        let col = self.col;
        let line = &mut self.lines[self.row];
        match mode {
            0 => line.truncate(col),
            1 => {
                let end = (col + 1).min(line.len());
                line[..end].fill(BLANK);
            }
            _ => line.clear(),
        }
    }

    fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.erase_line(0);
                self.lines.truncate(self.row + 1);
            }
            1 => {}
            _ => {
                self.lines.clear();
                self.row = 0;
                self.col = 0;
            }
        }
    }

    fn ensure_row(&mut self) {
        while self.lines.len() <= self.row {
            self.lines.push(Vec::new());
        }
    }

    /// Drops the oldest lines beyond `max`; returns how many were dropped.
    pub(super) fn trim(&mut self, max: usize) -> usize {
        if self.lines.len() <= max {
            return 0;
        }
        let drop = self.lines.len() - max + max / 10;
        self.lines.drain(..drop);
        self.row = self.row.saturating_sub(drop);
        self.saved.0 = self.saved.0.saturating_sub(drop);
        drop
    }

    /// Rows in use: up to the last non-empty line, and the cursor's row when
    /// it is shown.
    pub(super) fn height(&self) -> usize {
        let content = self
            .lines
            .iter()
            .rposition(|line| {
                line.iter()
                    .any(|cell| cell.ch != ' ' || cell.style != BLANK.style)
            })
            .map_or(0, |last| last + 1);
        if self.cursor_visible {
            content.max(self.row + 1)
        } else {
            content
        }
    }

    /// Adds whole lines after the current output, on fresh lines.
    pub(super) fn append_lines(&mut self, lines: impl IntoIterator<Item = Line>) {
        self.ensure_row();
        if !self.lines[self.row].is_empty() {
            self.control(b'\n');
        }
        for line in lines {
            self.lines[self.row] = line;
            self.control(b'\n');
        }
    }

    /// Lines `0..rows` that hold anything, trailing blank ones left out.
    pub(super) fn take_lines(&self, rows: usize) -> Vec<Line> {
        let mut lines: Vec<Line> = self.lines.iter().take(rows).cloned().collect();
        while lines.last().is_some_and(Vec::is_empty) {
            lines.pop();
        }
        lines
    }
}

struct PassthroughWriter<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for PassthroughWriter<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

fn set_cell(line: &mut Line, index: usize, cell: Cell) {
    if index < line.len() {
        line[index] = cell;
    } else {
        line.push(cell);
    }
}

/// Where `line` breaks into rows `width` columns wide: the cell ranges of
/// each row (an empty line is one empty row).
pub(super) fn wrap_ranges(line: &[Cell], width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    // A line drawn for a wider window (before the side panel opened) whose
    // overflow is only padding or a rule is cut, not wrapped: a coloured
    // diff row or a rule would otherwise spill onto an empty-looking row.
    let fits = clip(line, width).len();
    if fits < line.len() && line[fits..].iter().all(|cell| is_filler(cell.ch)) {
        return vec![(0, fits)];
    }
    let mut ranges = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (index, cell) in line.iter().enumerate() {
        let cell_width = usize::from(cell.width);
        if cell_width > 0 && used + cell_width > width && index > start {
            ranges.push((start, index));
            start = index;
            used = 0;
        }
        used += cell_width;
    }
    ranges.push((start, line.len()));
    ranges
}

/// Padding or rule characters, which may be cut off at the window's edge.
fn is_filler(ch: char) -> bool {
    matches!(ch, ' ' | '\u{a0}' | '─')
}

pub(super) fn row_count(line: &[Cell], width: usize) -> usize {
    wrap_ranges(line, width).len()
}

/// The cells as text with the SGR sequences they need, ending unstyled.
pub(super) fn render_cells(cells: &[Cell]) -> String {
    let mut out = String::new();
    let mut current = Style::default();
    for cell in cells {
        if cell.width == 0 {
            continue;
        }
        if cell.style != current {
            out.push_str(&cell.style.sgr());
            current = cell.style;
        }
        out.push(cell.ch);
        if cell.mark != '\0' {
            out.push(cell.mark);
        }
    }
    if current != Style::default() {
        out.push_str("\x1b[0m");
    }
    out
}

/// The first `width` columns of `cells`.
pub(super) fn clip(cells: &[Cell], width: usize) -> &[Cell] {
    let mut used = 0;
    for (index, cell) in cells.iter().enumerate() {
        used += usize::from(cell.width);
        if used > width {
            return &cells[..index];
        }
    }
    cells
}

/// `cells` with the ones in `from..to` drawn in reverse video (a mouse
/// selection); cells already reversed are drawn normally, so they still show.
pub(super) fn highlight(cells: &[Cell], from: usize, to: usize) -> Vec<Cell> {
    let mut cells = cells.to_vec();
    let end = to.min(cells.len());
    for cell in cells.iter_mut().take(end).skip(from) {
        cell.style.flags ^= REVERSE;
    }
    cells
}

/// The text of `line`, without styles.
pub(super) fn plain(line: &[Cell]) -> String {
    let mut text = String::new();
    for cell in line.iter().filter(|cell| cell.width > 0) {
        text.push(cell.ch);
        if cell.mark != '\0' {
            text.push(cell.mark);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{plain, render_cells, row_count, Buffer};

    fn lines(buffer: &Buffer) -> Vec<String> {
        buffer.lines.iter().map(|line| plain(line)).collect()
    }

    #[test]
    fn output_lines_keep_text_and_colours() {
        let mut buffer = Buffer::new(None);
        buffer.feed(b"plain \x1b[1;36mbold cyan\x1b[0m done\nnext");
        assert_eq!(lines(&buffer), vec!["plain bold cyan done", "next"]);
        let rendered = render_cells(&buffer.lines[0]);
        assert_eq!(rendered, "plain \x1b[0;1;38;5;6mbold cyan\x1b[0m done");
    }

    #[test]
    fn carriage_returns_and_erases_rewrite_a_line() {
        let mut buffer = Buffer::new(None);
        buffer.feed(b"[=    ] thinking\r\x1b[2Kanswer\nsecond\x1b[1A\r\x1b[Kfirst");
        assert_eq!(lines(&buffer), vec!["first", "second"]);
    }

    #[test]
    fn split_sequences_and_characters_are_completed_by_the_next_read() {
        let mut buffer = Buffer::new(None);
        buffer.feed(b"a\x1b[3");
        buffer.feed(b"1mred \xe2\x9d");
        buffer.feed(b"\xaf");
        assert_eq!(lines(&buffer), vec!["ared ❯"]);
        assert!(render_cells(&buffer.lines[0]).contains("38;5;1m"));
    }

    #[test]
    fn the_pane_wraps_and_redraws_in_place() {
        let mut pane = Buffer::new(Some(10));
        pane.feed(b"0123456789abc");
        assert_eq!(lines(&pane), vec!["0123456789", "abc"]);
        assert_eq!((pane.row, pane.col), (1, 3));
        // Redraw like the input box: up, clear, write a shorter frame.
        pane.feed(b"\x1b[1A\r\x1b[J\xe2\x9d\xaf hi");
        assert_eq!(lines(&pane), vec!["❯ hi"]);
        assert_eq!(pane.height(), 1);
    }

    #[test]
    fn terminal_modes_and_queries_pass_through() {
        let mut buffer = Buffer::new(Some(20));
        buffer.feed(b"\x1b[?2004h\x1b[?25l\x1b[?1049h\x1b[>1u\x1b]52;c;aGk=\x07x");
        assert_eq!(
            String::from_utf8(buffer.passthrough.clone()).unwrap(),
            "\x1b[?2004h\x1b[>1u\x1b]52;c;aGk=\x07"
        );
        assert!(!buffer.cursor_visible);
        assert_eq!(lines(&buffer), vec!["x"]);
    }

    #[test]
    fn kitty_images_pass_through_and_keep_their_placeholders() {
        let mut buffer = Buffer::new(None);
        buffer.feed(b"\x1b_Ga=T,U=1,i=7;AAAA\x1b\\\x1b_Xother\x1b\\");
        buffer.feed("\u{10EEEE}\u{305}\u{10EEEE}e\u{301}".as_bytes());
        assert_eq!(
            String::from_utf8(buffer.passthrough.clone()).unwrap(),
            "\x1b_Ga=T,U=1,i=7;AAAA\x1b\\"
        );
        assert_eq!(lines(&buffer), vec!["\u{10EEEE}\u{305}\u{10EEEE}e\u{301}"]);
        assert_eq!(
            render_cells(&buffer.lines[0]),
            "\u{10EEEE}\u{305}\u{10EEEE}e\u{301}"
        );
    }

    #[test]
    fn wide_characters_wrap_whole() {
        let mut buffer = Buffer::new(None);
        buffer.feed("ab日本".as_bytes());
        assert_eq!(row_count(&buffer.lines[0], 4), 2);
        assert_eq!(row_count(&buffer.lines[0], 6), 1);
        assert_eq!(row_count(&[], 6), 1);
    }

    #[test]
    fn overflow_of_only_padding_or_rule_is_cut_not_wrapped() {
        let mut buffer = Buffer::new(None);
        buffer.feed("\x1b[42mcode\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\u{a0}\x1b[0m\n".as_bytes());
        buffer.feed("──────────\n".as_bytes());
        buffer.feed("words that wrap\n".as_bytes());
        assert_eq!(super::wrap_ranges(&buffer.lines[0], 6), vec![(0, 6)]);
        assert_eq!(super::wrap_ranges(&buffer.lines[1], 4), vec![(0, 4)]);
        assert_eq!(super::row_count(&buffer.lines[2], 6), 3);
    }

    #[test]
    fn old_lines_are_trimmed() {
        let mut buffer = Buffer::new(None);
        for n in 0..30 {
            buffer.feed(format!("{n}\n").as_bytes());
        }
        let dropped = buffer.trim(20);
        assert_eq!(dropped, 13);
        assert_eq!(plain(&buffer.lines[0]), "13");
        assert_eq!(buffer.row, buffer.lines.len() - 1);
    }
}
