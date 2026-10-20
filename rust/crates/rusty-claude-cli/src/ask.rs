//! Interactive picker for the `AskUserQuestion` tool, like Claude Code's:
//! the model asks a question with options (its recommendation marked), the
//! user picks one with the arrow keys or a number, ticks several in
//! multi-select mode, or types an answer of their own.
//!
//! Key handling ([`Picker::handle_key`]) is separate from drawing so it can
//! be tested without a terminal.

use std::io::{self, IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use runtime::turn_keys::screen;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::render::{current_theme, ThemeName, OUTPUT_MARGIN};

const OTHER_LABEL: &str = "Type something else…";
const RECOMMENDED_SUFFIX: &str = "(Recommended)";

/// The tool input. `options` items may be plain strings or objects.
#[derive(Debug, Deserialize)]
pub struct AskInput {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<AskOptionInput>,
    #[serde(default, alias = "multiSelect")]
    pub multi_select: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AskOptionInput {
    Label(String),
    Detailed {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        recommended: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    label: String,
    description: Option<String>,
    recommended: bool,
}

impl From<AskOptionInput> for Choice {
    fn from(option: AskOptionInput) -> Self {
        let (label, description, recommended) = match option {
            AskOptionInput::Label(label) => (label, None, false),
            AskOptionInput::Detailed {
                label,
                description,
                recommended,
            } => (label, description, recommended),
        };
        let marked = label.trim_end().ends_with(RECOMMENDED_SUFFIX);
        let label = label
            .trim_end()
            .trim_end_matches(RECOMMENDED_SUFFIX)
            .trim_end()
            .to_string();
        Self {
            label,
            description: description.filter(|d| !d.trim().is_empty()),
            recommended: recommended || marked,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Chosen(Vec<String>),
    Typed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyOutcome {
    Continue,
    Done(Answer),
}

/// What is shown and selected. The last row is always "Type something else".
struct Picker {
    header: Option<String>,
    question: String,
    choices: Vec<Choice>,
    multi: bool,
    cursor: usize,
    checked: Vec<bool>,
    /// `Some` while the user types a custom answer.
    typing: Option<String>,
    /// Whether the last row offers "Type something else".
    allow_other: bool,
    /// Type-to-filter, for long lists (`/model`): every choice, and the
    /// query; `choices` holds the ones matching it.
    filter: Option<(Vec<Choice>, String)>,
}

/// Most option rows shown at once; longer lists scroll with the cursor.
const MAX_VISIBLE_ROWS: usize = 10;

impl Picker {
    fn new(input: AskInput) -> Self {
        let choices = input
            .options
            .into_iter()
            .map(Choice::from)
            .collect::<Vec<_>>();
        let cursor = choices.iter().position(|c| c.recommended).unwrap_or(0);
        let checked = vec![false; choices.len()];
        let typing = choices.is_empty().then(String::new);
        Self {
            header: input.header.filter(|h| !h.trim().is_empty()),
            question: input.question,
            multi: input.multi_select && !choices.is_empty(),
            cursor,
            checked,
            choices,
            typing,
            allow_other: true,
            filter: None,
        }
    }

    /// Turns on type-to-filter: typed text narrows the list to choices whose
    /// label or description contains every word of it (any case).
    fn filterable(mut self) -> Self {
        self.allow_other = false;
        self.typing = None;
        self.filter = Some((self.choices.clone(), String::new()));
        self
    }

    fn refilter(&mut self) {
        let Some((all, query)) = &self.filter else {
            return;
        };
        let words = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        self.choices = all
            .iter()
            .filter(|choice| {
                let text = format!(
                    "{} {}",
                    choice.label,
                    choice.description.as_deref().unwrap_or("")
                )
                .to_lowercase();
                words.iter().all(|word| text.contains(word.as_str()))
            })
            .cloned()
            .collect();
        self.checked = vec![false; self.choices.len()];
        self.cursor = 0;
    }

    /// Keys of a filterable list that edit the query. `None` when the key
    /// is not one of them (arrows, Enter, Esc on an empty query).
    fn handle_filter_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<KeyOutcome> {
        let (_, query) = self.filter.as_mut()?;
        match key.code {
            KeyCode::Char('u') if ctrl => query.clear(),
            KeyCode::Char(ch) if !ctrl => query.push(ch),
            KeyCode::Backspace => {
                query.pop()?;
            }
            KeyCode::Esc if !query.is_empty() => query.clear(),
            // Nothing matches: Enter uses the query as typed.
            KeyCode::Enter if self.choices.is_empty() && !query.trim().is_empty() => {
                return Some(KeyOutcome::Done(Answer::Typed(query.trim().to_string())));
            }
            KeyCode::Enter if self.choices.is_empty() => return Some(KeyOutcome::Continue),
            _ => return None,
        }
        self.refilter();
        Some(KeyOutcome::Continue)
    }

    /// Rows the cursor can be on: the choices plus "Type something else".
    fn rows(&self) -> usize {
        self.choices.len() + usize::from(self.allow_other)
    }

    fn is_other(&self, row: usize) -> bool {
        self.allow_other && row == self.choices.len()
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if key.kind == KeyEventKind::Release {
            return KeyOutcome::Continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'd')) {
            return KeyOutcome::Done(Answer::Cancelled);
        }
        if let Some(text) = &mut self.typing {
            match key.code {
                KeyCode::Enter if !text.trim().is_empty() => {
                    return KeyOutcome::Done(Answer::Typed(text.trim().to_string()));
                }
                KeyCode::Esc if self.choices.is_empty() => {
                    return KeyOutcome::Done(Answer::Cancelled);
                }
                KeyCode::Esc => self.typing = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(ch) if !ctrl => text.push(ch),
                _ => {}
            }
            return KeyOutcome::Continue;
        }
        if let Some(outcome) = self.handle_filter_key(key, ctrl) {
            return outcome;
        }
        if self.rows() == 0 {
            return match key.code {
                KeyCode::Esc => KeyOutcome::Done(Answer::Cancelled),
                _ => KeyOutcome::Continue,
            };
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = (self.cursor + self.rows() - 1) % self.rows();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.cursor = (self.cursor + 1) % self.rows();
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let row = digit as usize - '1' as usize;
                if row < self.rows() {
                    self.cursor = row;
                    if !self.multi {
                        return self.select();
                    }
                    return self.toggle();
                }
            }
            KeyCode::Char(' ') if self.multi => return self.toggle(),
            KeyCode::Enter => return self.select(),
            KeyCode::Esc => return KeyOutcome::Done(Answer::Cancelled),
            _ => {}
        }
        KeyOutcome::Continue
    }

    fn toggle(&mut self) -> KeyOutcome {
        if self.is_other(self.cursor) {
            self.typing = Some(String::new());
        } else {
            self.checked[self.cursor] = !self.checked[self.cursor];
        }
        KeyOutcome::Continue
    }

    fn select(&mut self) -> KeyOutcome {
        if self.is_other(self.cursor) {
            self.typing = Some(String::new());
            return KeyOutcome::Continue;
        }
        if self.multi {
            let chosen = self
                .choices
                .iter()
                .zip(&self.checked)
                .filter(|(_, checked)| **checked)
                .map(|(choice, _)| choice.label.clone())
                .collect::<Vec<_>>();
            if chosen.is_empty() {
                // Enter with nothing ticked picks the highlighted option.
                return KeyOutcome::Done(Answer::Chosen(vec![self.choices[self.cursor]
                    .label
                    .clone()]));
            }
            return KeyOutcome::Done(Answer::Chosen(chosen));
        }
        KeyOutcome::Done(Answer::Chosen(vec![self.choices[self.cursor]
            .label
            .clone()]))
    }

    /// The picker's screen lines (without the left margin), fitted to
    /// `width` and, by showing fewer options at once, to `height` lines: a
    /// picker taller than the terminal scrolls its question, filter and
    /// highlighted row out of sight.
    fn lines(&self, width: usize, height: usize, colored: bool) -> Vec<String> {
        let mut visible = MAX_VISIBLE_ROWS;
        loop {
            let lines = self.lines_showing(visible, width, colored);
            if lines.len() <= height || visible == 1 {
                return lines;
            }
            visible -= 1;
        }
    }

    fn lines_showing(&self, visible: usize, width: usize, colored: bool) -> Vec<String> {
        let paint = |style: &str, text: &str| {
            if colored {
                format!("{style}{text}\x1b[0m")
            } else {
                text.to_string()
            }
        };
        let fit = |text: &str, room: usize| -> String {
            if text.chars().count() <= room {
                text.to_string()
            } else {
                let mut cut = text
                    .chars()
                    .take(room.saturating_sub(1))
                    .collect::<String>();
                cut.push('…');
                cut
            }
        };
        let mut lines = Vec::new();
        let header = self
            .header
            .as_ref()
            .map(|h| format!("[{}] ", fit(h, 16)))
            .unwrap_or_default();
        let question_room = width.saturating_sub(header.chars().count());
        let mut question_lines = wrap_words(&self.question, question_room.max(10)).into_iter();
        let first = question_lines.next().unwrap_or_default();
        lines.push(format!(
            "{}{}",
            paint("\x1b[2m", &header),
            paint("\x1b[1m", &first)
        ));
        for rest in question_lines {
            lines.push(format!(
                "{}{}",
                " ".repeat(header.chars().count()),
                paint("\x1b[1m", &rest)
            ));
        }
        if let Some((_, query)) = &self.filter {
            lines.push(format!("{} {query}▏", paint("\x1b[2m", "filter:")));
            if self.choices.is_empty() {
                let note = if query.trim().is_empty() {
                    "nothing to choose from".to_string()
                } else {
                    format!("no match · Enter uses “{}”", query.trim())
                };
                lines.push(paint("\x1b[2m", &format!("  {}", fit(&note, width))));
            }
        }
        // A long list shows a window around the cursor.
        let rows = self.rows();
        let start = if rows > visible {
            self.cursor.saturating_sub(visible / 2).min(rows - visible)
        } else {
            0
        };
        let end = (start + visible).min(rows);
        if start > 0 {
            lines.push(paint("\x1b[2m", &format!("  ↑ {start} more")));
        }
        for row in start..end {
            let selected = row == self.cursor;
            let pointer = if selected { "❯ " } else { "  " };
            let checkbox = if self.multi && !self.is_other(row) {
                if self.checked[row] {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else if self.multi {
                "    "
            } else {
                ""
            };
            let (label, description) = if self.is_other(row) {
                match (&self.typing, selected) {
                    (Some(text), true) => (format!("> {text}▏"), None),
                    _ => (OTHER_LABEL.to_string(), None),
                }
            } else {
                let choice = &self.choices[row];
                let label = if choice.recommended {
                    format!("{} {RECOMMENDED_SUFFIX}", choice.label)
                } else {
                    choice.label.clone()
                };
                (label, choice.description.clone())
            };
            let number = format!("{}. ", row + 1);
            let lead = format!("{pointer}{checkbox}{number}");
            let room = width.saturating_sub(lead.chars().count());
            let text = fit(&label, room);
            let style = if selected { "\x1b[36m" } else { "" };
            lines.push(paint(style, &format!("{lead}{text}")));
            if let Some(description) = description {
                let indent = " ".repeat(lead.chars().count());
                let room = width.saturating_sub(indent.chars().count());
                for line in wrap_words(&description, room.max(10)).into_iter().take(2) {
                    lines.push(format!("{indent}{}", paint("\x1b[2m", &line)));
                }
            }
        }
        if end < rows {
            lines.push(paint("\x1b[2m", &format!("  ↓ {} more", rows - end)));
        }
        let hint = if self.typing.is_some() {
            "type your answer · Enter send · Esc back"
        } else if self.filter.is_some() {
            "type to filter · ↑↓ move · Enter select · Esc clear/cancel"
        } else if self.multi {
            "↑↓ move · Space toggle · Enter confirm · Esc cancel"
        } else {
            "↑↓ move · Enter select · 1-9 jump · Esc cancel"
        };
        lines.push(paint("\x1b[2m", &fit(hint, width)));
        lines
    }
}

/// Splits `text` into lines of at most `width` characters at spaces.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

/// Shows the picker and returns the tool result for the model. Without a
/// terminal it answers right away that nobody can be asked.
pub fn ask_user(input: &Value) -> Result<String, String> {
    let input: AskInput = serde_json::from_value(input.clone())
        .map_err(|error| format!("invalid AskUserQuestion input: {error}"))?;
    let question = input.question.clone();
    if !io::stdin().is_terminal() || !runtime::turn_keys::stdout_is_terminal() {
        return Ok(no_user_result(&question));
    }
    let _keys_paused = runtime::turn_keys::pause();
    crate::render::stop_thinking();
    let answer = run_picker(Picker::new(input)).map_err(|error| error.to_string())?;
    Ok(answer_result(&question, &answer))
}

fn run_picker(mut picker: Picker) -> io::Result<Answer> {
    let colored = current_theme() != ThemeName::None;
    let mut stdout = io::stdout();
    writeln!(stdout)?;
    terminal::enable_raw_mode()?;
    let result: io::Result<(Answer, usize)> = (|| {
        let mut drawn = 0_usize;
        loop {
            let (cols, term_rows) = terminal::size().map_or((80, 24), |(cols, rows)| {
                (usize::from(cols), usize::from(rows))
            });
            let width = cols.saturating_sub(2 * OUTPUT_MARGIN).max(20);
            // Leave room for the blank line above and the status lines below.
            let height = term_rows.saturating_sub(4).max(6);
            let lines = picker.lines(width, height, colored);
            let mut frame = String::new();
            if drawn > 0 {
                frame.push_str(&format!("\x1b[{drawn}A"));
            }
            frame.push('\r');
            for line in &lines {
                frame.push_str(&format!("\x1b[2K{}{line}\r\n", " ".repeat(OUTPUT_MARGIN)));
            }
            // Clear rows left over from a taller previous frame.
            for _ in lines.len()..drawn {
                frame.push_str("\x1b[2K\r\n");
            }
            let extra = drawn.saturating_sub(lines.len());
            if extra > 0 {
                frame.push_str(&format!("\x1b[{extra}A"));
            }
            if drawn == 0 && screen::active() {
                // Full-screen mode draws it in the bottom pane, cursor hidden.
                frame.insert_str(0, "\x1b[?25l");
            }
            screen::write_pane(frame.as_bytes())?;
            drawn = lines.len();
            if let Event::Key(key) = event::read()? {
                if let KeyOutcome::Done(answer) = picker.handle_key(key) {
                    return Ok((answer, drawn));
                }
            }
        }
    })();
    terminal::disable_raw_mode()?;
    let (answer, drawn) = result?;
    // Replace the picker with a one-line summary of the answer.
    let summary = match &answer {
        Answer::Chosen(labels) => format!("└ {}", labels.join(", ")),
        Answer::Typed(text) => format!("└ {text}"),
        Answer::Cancelled => "└ no answer".to_string(),
    };
    // The `[ask] question` tool line above already shows the question.
    let summary = format!("{}\x1b[36m{summary}\x1b[0m\n", " ".repeat(OUTPUT_MARGIN));
    let mut frame = String::new();
    if screen::active() {
        screen::clear_pane();
    } else {
        frame.push_str(&format!("\x1b[{drawn}A\r"));
        for _ in 0..drawn {
            frame.push_str("\x1b[2K\n");
        }
        frame.push_str(&format!("\x1b[{drawn}A\r"));
    }
    frame.push_str(&summary);
    stdout.write_all(frame.as_bytes())?;
    stdout.flush()?;
    Ok(answer)
}

fn answer_result(question: &str, answer: &Answer) -> String {
    let value = match answer {
        Answer::Chosen(labels) => json!({
            "question": question,
            "answer": labels.join(", "),
            "answers": labels,
            "status": "answered",
        }),
        Answer::Typed(text) => json!({
            "question": question,
            "answer": text,
            "custom": true,
            "status": "answered",
        }),
        Answer::Cancelled => json!({
            "question": question,
            "answer": null,
            "status": "cancelled",
            "note": "The user dismissed the question. Do not ask it again right away; continue with a sensible default and say which one you chose, or ask in plain text.",
        }),
    };
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

fn no_user_result(question: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "question": question,
        "answer": null,
        "status": "unavailable",
        "note": "No interactive user is attached (non-terminal run). Choose the option you would recommend, say so in your reply, and continue.",
    }))
    .unwrap_or_default()
}

/// What [`pick`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// The index of the chosen option.
    Index(usize),
    /// Text typed on the "Type something else" row.
    Typed(String),
    /// Esc or Ctrl+C.
    Cancelled,
}

/// A list to choose from with the arrow keys (Enter selects, 1-9 jump, Esc
/// cancels), starting on `default`. `allow_other` adds a row for typing an
/// answer that is not listed. Long lists scroll.
pub fn pick(
    question: &str,
    options: &[(String, Option<String>)],
    default: usize,
    allow_other: bool,
) -> io::Result<Pick> {
    let mut picker = Picker::new(AskInput {
        question: question.to_string(),
        header: None,
        options: options
            .iter()
            .map(|(label, description)| AskOptionInput::Detailed {
                label: label.clone(),
                description: description.clone(),
                recommended: false,
            })
            .collect(),
        multi_select: false,
    });
    picker.allow_other = allow_other;
    picker.cursor = default.min(picker.rows().saturating_sub(1));
    if !allow_other && options.is_empty() {
        return Ok(Pick::Cancelled);
    }
    terminal_keys_paused(|| {
        Ok(match run_picker(picker)? {
            Answer::Chosen(labels) => labels
                .first()
                .and_then(|label| options.iter().position(|(option, _)| option == label))
                .map_or(Pick::Cancelled, Pick::Index),
            Answer::Typed(text) => Pick::Typed(text),
            Answer::Cancelled => Pick::Cancelled,
        })
    })
}

/// [`pick`] for long lists: typing filters the options (see
/// `Picker::filterable`), and Enter with no match returns the typed text.
pub fn pick_filtered(
    question: &str,
    options: &[(String, Option<String>)],
    default: usize,
) -> io::Result<Pick> {
    let mut picker = Picker::new(AskInput {
        question: question.to_string(),
        header: None,
        options: options
            .iter()
            .map(|(label, description)| AskOptionInput::Detailed {
                label: label.clone(),
                description: description.clone(),
                recommended: false,
            })
            .collect(),
        multi_select: false,
    })
    .filterable();
    picker.cursor = default.min(options.len().saturating_sub(1));
    terminal_keys_paused(|| {
        Ok(match run_picker(picker)? {
            Answer::Chosen(labels) => labels
                .first()
                .and_then(|label| options.iter().position(|(option, _)| option == label))
                .map_or(Pick::Cancelled, Pick::Index),
            Answer::Typed(text) => Pick::Typed(text),
            Answer::Cancelled => Pick::Cancelled,
        })
    })
}

/// A line of text typed in raw mode, so it works in the full-screen view
/// too: Enter accepts (an empty line means "keep the default"), Esc cancels
/// (`None`). `hidden` shows `•` for each character, for keys and passwords.
pub fn ask_text(prompt: &str, hidden: bool) -> io::Result<Option<String>> {
    terminal_keys_paused(|| read_input(prompt, !hidden))
}

/// Runs `f` with the turn key watcher paused (if a turn is running), so the
/// keyboard reaches `f`.
fn terminal_keys_paused<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let _keys_paused = runtime::turn_keys::pause();
    crate::render::stop_thinking();
    f()
}

/// Asks for what `ssh` needs while a command runs, usually a key
/// passphrase: `SSH_ASKPASS` reaches here through [`runtime::askpass`].
/// Typed characters show as `•` unless the prompt expects a visible answer
/// (yes/no, a user name). `None` when cancelled, or outside a turn, where
/// the line editor owns the keyboard.
pub fn ask_secret(prompt: &str) -> Option<String> {
    if !runtime::turn_keys::watching()
        || !io::stdin().is_terminal()
        || !runtime::turn_keys::stdout_is_terminal()
    {
        return None;
    }
    let _keys_paused = runtime::turn_keys::pause();
    crate::render::stop_thinking();
    read_input(prompt, shows_answer(prompt)).ok().flatten()
}

/// Whether the answer may be shown as typed.
fn shows_answer(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("yes/no") || lower.contains("username")
}

/// The answer being typed for [`ask_secret`].
#[derive(Default)]
struct SecretInput {
    text: String,
}

impl SecretInput {
    /// `Some(answer)` once Enter or a cancel key is pressed.
    fn handle_key(&mut self, key: KeyEvent) -> Option<Option<String>> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return Some(Some(std::mem::take(&mut self.text))),
            KeyCode::Esc => return Some(None),
            KeyCode::Char('c' | 'd') if ctrl => return Some(None),
            KeyCode::Char('u') if ctrl => self.text.clear(),
            KeyCode::Backspace => {
                self.text.pop();
            }
            KeyCode::Char(ch) if !ctrl => self.text.push(ch),
            _ => {}
        }
        None
    }
}

fn read_input(prompt: &str, visible: bool) -> io::Result<Option<String>> {
    let margin = " ".repeat(OUTPUT_MARGIN);
    let mut stdout = io::stdout();
    writeln!(stdout)?;
    terminal::enable_raw_mode()?;
    let result: io::Result<(Option<String>, usize)> = (|| {
        let mut input = SecretInput::default();
        let mut drawn = 0_usize;
        loop {
            let width = terminal::size().map_or(80, |(cols, _)| usize::from(cols));
            let width = width.saturating_sub(2 * OUTPUT_MARGIN + 3).max(20);
            let shown = if visible {
                input.text.clone()
            } else {
                "•".repeat(input.text.chars().count())
            };
            let mut lines: Vec<String> = prompt
                .lines()
                .map(|line| line.chars().take(width).collect())
                .collect();
            if let Some(last) = lines.last_mut() {
                last.push(' ');
                last.push_str(&shown);
            }
            lines.push("\x1b[2mEnter to accept · Esc to cancel\x1b[0m".to_string());
            let mut frame = String::new();
            if drawn > 0 {
                frame.push_str(&format!("\x1b[{drawn}A"));
            }
            frame.push('\r');
            for (index, line) in lines.iter().enumerate() {
                let mark = match (index, visible) {
                    (0, false) => "\x1b[33m🔑\x1b[0m ",
                    (0, true) => "\x1b[33m›\x1b[0m ",
                    _ => "   ",
                };
                frame.push_str(&format!("\x1b[2K{margin}{mark}{line}\r\n"));
            }
            if drawn == 0 && screen::active() {
                frame.insert_str(0, "\x1b[?25l");
            }
            screen::write_pane(frame.as_bytes())?;
            drawn = lines.len();
            if let Event::Key(key) = event::read()? {
                if let Some(answer) = input.handle_key(key) {
                    return Ok((answer, drawn));
                }
            }
        }
    })();
    terminal::disable_raw_mode()?;
    let (answer, drawn) = result?;
    // Never echo a secret: the summary only says whether one was given.
    let summary = match &answer {
        None => "cancelled".to_string(),
        Some(text) if text.is_empty() => "kept".to_string(),
        Some(text) if visible => text.clone(),
        Some(_) => "entered".to_string(),
    };
    let first = prompt.lines().next().unwrap_or("").trim();
    let icon = if visible { "›" } else { "🔑" };
    let summary = format!("{margin}\x1b[36m{icon} {first} └ {summary}\x1b[0m\n");
    let mut frame = String::new();
    if screen::active() {
        screen::clear_pane();
    } else {
        frame.push_str(&format!("\x1b[{drawn}A\r"));
        for _ in 0..drawn {
            frame.push_str("\x1b[2K\n");
        }
        frame.push_str(&format!("\x1b[{drawn}A\r"));
    }
    frame.push_str(&summary);
    stdout.write_all(frame.as_bytes())?;
    stdout.flush()?;
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use super::{Answer, AskInput, KeyOutcome, Picker};

    fn picker(input: serde_json::Value) -> Picker {
        Picker::new(serde_json::from_value::<AskInput>(input).expect("input"))
    }

    fn press(picker: &mut Picker, code: KeyCode) -> KeyOutcome {
        picker.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn filterable_lists_narrow_as_you_type() {
        let mut p = picker(json!({
            "question": "Switch model",
            "options": [
                {"label": "openrouter/poolside/laguna-s-2.1:free", "description": "OpenRouter · free"},
                {"label": "nvidia/deepseek-ai/deepseek-v4.1-flash", "description": "NVIDIA NIM"},
                {"label": "openrouter/deepseek/deepseek-v4.1-flash", "description": "OpenRouter"},
            ]
        }))
        .filterable();
        // Letters and digits go to the filter, not to j/k or 1-9.
        for ch in "deep 4.1".chars() {
            press(&mut p, KeyCode::Char(ch));
        }
        assert_eq!(p.choices.len(), 2);
        for ch in " nvidia".chars() {
            press(&mut p, KeyCode::Char(ch));
        }
        assert_eq!(p.choices.len(), 1);
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Answer::Chosen(vec![
                "nvidia/deepseek-ai/deepseek-v4.1-flash".to_string()
            ]))
        );
    }

    #[test]
    fn filterable_lists_use_unmatched_text_and_esc_clears_first() {
        let mut p = picker(json!({"question": "q", "options": ["alpha", "beta"]})).filterable();
        for ch in "zai/glm-4.6".chars() {
            press(&mut p, KeyCode::Char(ch));
        }
        assert!(p.choices.is_empty());
        assert_eq!(press(&mut p, KeyCode::Esc), KeyOutcome::Continue);
        assert_eq!(p.choices.len(), 2);
        press(&mut p, KeyCode::Char('x'));
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Answer::Typed("x".to_string()))
        );
        let mut p = picker(json!({"question": "q", "options": ["alpha"]})).filterable();
        assert_eq!(
            press(&mut p, KeyCode::Esc),
            KeyOutcome::Done(Answer::Cancelled)
        );
    }

    #[test]
    fn cursor_starts_on_the_recommended_option_and_enter_selects_it() {
        let mut p = picker(json!({
            "question": "Which database?",
            "options": [
                {"label": "SQLite", "description": "zero setup"},
                {"label": "PostgreSQL", "recommended": true},
            ]
        }));
        assert_eq!(p.cursor, 1);
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Answer::Chosen(vec!["PostgreSQL".to_string()]))
        );
    }

    #[test]
    fn recommended_suffix_in_a_plain_label_is_recognised() {
        let p = picker(json!({"question": "q", "options": ["A", "B (Recommended)"]}));
        assert_eq!(p.cursor, 1);
        assert_eq!(p.choices[1].label, "B");
        assert!(p.choices[1].recommended);
    }

    #[test]
    fn number_keys_pick_directly() {
        let mut p = picker(json!({"question": "q", "options": ["A", "B", "C"]}));
        assert_eq!(
            press(&mut p, KeyCode::Char('3')),
            KeyOutcome::Done(Answer::Chosen(vec!["C".to_string()]))
        );
    }

    #[test]
    fn multi_select_toggles_checkboxes_and_returns_all_ticked() {
        let mut p = picker(json!({
            "question": "Which tests?",
            "options": ["Unit", "Integration", "E2E"],
            "multiSelect": true
        }));
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Answer::Chosen(vec!["Unit".to_string(), "E2E".to_string()]))
        );
    }

    #[test]
    fn last_row_lets_the_user_type_their_own_answer() {
        let mut p = picker(json!({"question": "q", "options": ["A", "B"]}));
        press(&mut p, KeyCode::Up);
        assert!(p.is_other(p.cursor));
        press(&mut p, KeyCode::Enter);
        for ch in "MySQL".chars() {
            press(&mut p, KeyCode::Char(ch));
        }
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Answer::Typed("MySQL".to_string()))
        );
    }

    #[test]
    fn esc_while_typing_goes_back_and_esc_again_cancels() {
        let mut p = picker(json!({"question": "q", "options": ["A"]}));
        press(&mut p, KeyCode::Char('2'));
        assert!(p.typing.is_some());
        assert_eq!(press(&mut p, KeyCode::Esc), KeyOutcome::Continue);
        assert!(p.typing.is_none());
        assert_eq!(
            press(&mut p, KeyCode::Esc),
            KeyOutcome::Done(Answer::Cancelled)
        );
    }

    #[test]
    fn a_question_without_options_is_a_text_box() {
        let mut p = picker(json!({"question": "Project name?"}));
        assert!(p.typing.is_some());
        press(&mut p, KeyCode::Char('x'));
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Answer::Typed("x".to_string()))
        );
    }

    #[test]
    fn long_lists_shrink_to_fit_the_terminal_height() {
        let options = (1..=30)
            .map(|n| json!({"label": format!("model-{n}"), "description": "provider · 128k context"}))
            .collect::<Vec<_>>();
        let p = picker(json!({"question": "Switch model", "options": options})).filterable();
        let tall = p.lines(80, 100, false);
        let short = p.lines(80, 12, false);
        assert!(tall.len() > 12);
        assert!(short.len() <= 12, "{short:#?}");
        // The question, the filter and the highlighted row stay in view.
        assert!(short[0].contains("Switch model"));
        assert!(short[1].contains("filter:"));
        assert!(short.iter().any(|line| line.contains("❯ 1. model-1")));
    }

    #[test]
    fn lines_show_recommendation_checkboxes_and_fit_the_width() {
        let p = picker(json!({
            "question": "Which tests should I add?",
            "header": "Tests",
            "options": [
                {"label": "Unit", "description": "fast checks for each function", "recommended": true},
                "Integration"
            ],
            "multi_select": true
        }));
        let lines = p.lines(40, 100, false);
        assert_eq!(lines[0], "[Tests] Which tests should I add?");
        assert_eq!(lines[1], "❯ [ ] 1. Unit (Recommended)");
        assert_eq!(lines[2], "         fast checks for each function");
        assert_eq!(lines[3], "  [ ] 2. Integration");
        assert_eq!(lines[4], "      3. Type something else…");
        assert!(lines.iter().all(|line| line.chars().count() <= 40));
    }

    #[test]
    fn secret_input_edits_and_ends() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut input = super::SecretInput::default();
        for ch in "t00rx".chars() {
            assert_eq!(input.handle_key(key(KeyCode::Char(ch))), None);
        }
        assert_eq!(input.handle_key(key(KeyCode::Backspace)), None);
        assert_eq!(
            input.handle_key(key(KeyCode::Enter)),
            Some(Some("t00r".to_string()))
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(input.handle_key(ctrl_c), Some(None));
        assert!(super::shows_answer(
            "Are you sure you want to continue connecting (yes/no/[fingerprint])?"
        ));
        assert!(!super::shows_answer(
            "Enter passphrase for key '/home/u/.ssh/id_ed25519':"
        ));
    }
}
