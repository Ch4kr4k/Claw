//! Permission prompt for a tool call, like Claude Code's: a rounded box with
//! what the tool will do, then a menu picked with the arrow keys or a number:
//! allow once, allow for the rest of the session, or refuse and tell the
//! model what to do instead.
//!
//! Key handling ([`Prompt::handle_key`]) is separate from drawing so it can
//! be tested without a terminal. Glyphs are limited to ones the user's font
//! (FiraCode Nerd Font) has: box drawing, `❯` and `▏`.

use std::io::{self, Write};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use runtime::turn_keys::screen;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::{current_theme, ThemeName, OUTPUT_MARGIN};

/// Lines of the command or input shown before the rest is elided.
const MAX_SUBJECT_LINES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allow this and matching requests for the rest of the session.
    AllowForSession,
    /// Refuse; `Some` carries what the user wants done instead.
    Deny(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyOutcome {
    Continue,
    Done(Decision),
}

/// What the box shows. Built by the caller from the permission request.
pub struct Request {
    /// e.g. "Bash command", "Edit file".
    pub title: String,
    /// The command, path or input summary, one entry per line.
    pub subject: Vec<String>,
    /// Lines shown dimmed under the subject (why approval is needed).
    pub note: Option<String>,
    /// What "don't ask again" covers, e.g. "sudo emerge" or "web_search".
    pub session_scope: String,
}

struct Prompt {
    request: Request,
    cursor: usize,
    /// `Some` while the user types what to do instead (option 3).
    feedback: Option<String>,
}

const ROWS: usize = 3;

impl Prompt {
    fn new(request: Request) -> Self {
        Self {
            request,
            cursor: 0,
            feedback: None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        if key.kind == KeyEventKind::Release {
            return KeyOutcome::Continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'd')) {
            return KeyOutcome::Done(Decision::Deny(None));
        }
        if let Some(text) = &mut self.feedback {
            match key.code {
                KeyCode::Enter => {
                    let text = text.trim().to_string();
                    return KeyOutcome::Done(Decision::Deny((!text.is_empty()).then_some(text)));
                }
                KeyCode::Esc => self.feedback = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(ch) if !ctrl => text.push(ch),
                _ => {}
            }
            return KeyOutcome::Continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = (self.cursor + ROWS - 1) % ROWS,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                self.cursor = (self.cursor + 1) % ROWS;
            }
            KeyCode::Char(digit @ '1'..='3') => {
                self.cursor = digit as usize - '1' as usize;
                return self.select();
            }
            // The old prompt's letters still work.
            KeyCode::Char('y') => return KeyOutcome::Done(Decision::Allow),
            KeyCode::Char('a') => return KeyOutcome::Done(Decision::AllowForSession),
            KeyCode::Char('n') => {
                self.cursor = 2;
                return self.select();
            }
            KeyCode::Enter => return self.select(),
            KeyCode::Esc => return KeyOutcome::Done(Decision::Deny(None)),
            _ => {}
        }
        KeyOutcome::Continue
    }

    fn select(&mut self) -> KeyOutcome {
        match self.cursor {
            0 => KeyOutcome::Done(Decision::Allow),
            1 => KeyOutcome::Done(Decision::AllowForSession),
            _ => {
                self.feedback = Some(String::new());
                KeyOutcome::Continue
            }
        }
    }

    /// The box's screen lines (without the left margin), `width` columns wide.
    fn lines(&self, width: usize, colored: bool) -> Vec<String> {
        let paint = |style: &str, text: &str| {
            if colored && !style.is_empty() {
                format!("{style}{text}\x1b[0m")
            } else {
                text.to_string()
            }
        };
        let border = "\x1b[38;5;214m";
        let inner = width.saturating_sub(4).max(10);
        // One boxed row: `│ text<pad> │`, `text` already styled.
        let row = |styled: &str, plain_width: usize| {
            format!(
                "{} {styled}{} {}",
                paint(border, "│"),
                " ".repeat(inner.saturating_sub(plain_width)),
                paint(border, "│")
            )
        };
        let mut lines = Vec::new();

        // `╭─ title ` + dashes + `╮` is title + 5 columns plus the dashes.
        let title = fit(&self.request.title, inner.saturating_sub(2));
        lines.push(paint(
            border,
            &format!(
                "╭─ {title} {}╮",
                "─".repeat(width.saturating_sub(title.width() + 5))
            ),
        ));
        lines.push(row("", 0));

        let mut subject = Vec::new();
        for line in &self.request.subject {
            subject.extend(wrap_width(line, inner.saturating_sub(2)));
        }
        let hidden = subject.len().saturating_sub(MAX_SUBJECT_LINES);
        subject.truncate(MAX_SUBJECT_LINES);
        for line in &subject {
            lines.push(row(
                &format!("  {}", paint("\x1b[1m", line)),
                line.width() + 2,
            ));
        }
        if hidden > 0 {
            let more = format!("  … +{hidden} more lines");
            lines.push(row(&paint("\x1b[2m", &more), more.width()));
        }
        if let Some(note) = &self.request.note {
            for line in wrap_width(note, inner.saturating_sub(2)) {
                let text = format!("  {line}");
                lines.push(row(&paint("\x1b[2m", &text), text.width()));
            }
        }
        lines.push(row("", 0));

        let ask = "Do you want to proceed?";
        lines.push(row(ask, ask.width()));
        let scope = fit(&self.request.session_scope, inner.saturating_sub(40).max(8));
        let labels = [
            "Yes".to_string(),
            format!("Yes, and don't ask again for `{scope}` this session"),
            "No, and tell the model what to do differently (esc)".to_string(),
        ];
        for (index, label) in labels.iter().enumerate() {
            let selected = index == self.cursor;
            let pointer = if selected { "❯ " } else { "  " };
            let text = fit(&format!("{pointer}{}. {label}", index + 1), inner);
            let style = if selected { "\x1b[36m" } else { "" };
            lines.push(row(&paint(style, &text), text.width()));
        }
        if let Some(text) = &self.feedback {
            let shown = format!("  > {text}▏");
            let shown = fit_tail(&shown, inner);
            lines.push(row(&paint("\x1b[36m", &shown), shown.width()));
        }
        lines.push(paint(
            border,
            &format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        ));
        let hint = if self.feedback.is_some() {
            "type what to do instead · Enter send · Esc back"
        } else {
            "↑↓ move · Enter select · 1-3 pick · Esc refuse"
        };
        lines.push(paint(
            "\x1b[2m",
            &format!(" {}", fit(hint, width.saturating_sub(1))),
        ));
        lines
    }
}

/// Cuts `text` to at most `room` columns, ending in `…` when cut.
fn fit(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > room {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Keeps the end of `text` (where the user is typing) within `room` columns.
fn fit_tail(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_string();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for ch in text.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        kept.push(ch);
        used += w;
    }
    kept.push('…');
    kept.into_iter().rev().collect()
}

/// Breaks `text` into pieces of at most `width` columns (commands are not
/// word-wrapped: every character matters, so they are cut anywhere).
fn wrap_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let text = text.replace('\t', "    ");
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push(ch);
        used += w;
    }
    lines.push(line);
    lines
}

/// Shows the box and menu and waits for an answer. The caller has paused
/// the turn's key watcher, and stdin and stdout are terminals.
pub fn ask(request: Request) -> io::Result<Decision> {
    let colored = current_theme() != ThemeName::None;
    let mut prompt = Prompt::new(request);
    let mut stdout = io::stdout();
    writeln!(stdout)?;
    terminal::enable_raw_mode()?;
    let result: io::Result<(Decision, usize)> = (|| {
        let mut drawn = 0_usize;
        loop {
            let cols = terminal::size().map_or(80, |(cols, _)| usize::from(cols));
            let width = cols.saturating_sub(2 * OUTPUT_MARGIN).max(30);
            let lines = prompt.lines(width, colored);
            let mut frame = String::new();
            if drawn > 0 {
                frame.push_str(&format!("\x1b[{drawn}A"));
            }
            frame.push('\r');
            for line in &lines {
                frame.push_str(&format!("\x1b[2K{}{line}\r\n", " ".repeat(OUTPUT_MARGIN)));
            }
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
                if let KeyOutcome::Done(decision) = prompt.handle_key(key) {
                    return Ok((decision, drawn));
                }
            }
        }
    })();
    terminal::disable_raw_mode()?;
    let (decision, drawn) = result?;

    // Replace the box with one line saying what was decided.
    let (style, summary) = match &decision {
        Decision::Allow => ("\x1b[38;5;245m", "Allowed".to_string()),
        Decision::AllowForSession => (
            "\x1b[38;5;245m",
            format!(
                "Allowed, and won't ask again for `{}` this session",
                prompt.request.session_scope
            ),
        ),
        Decision::Deny(None) => ("\x1b[38;5;203m", "Refused".to_string()),
        Decision::Deny(Some(text)) => ("\x1b[38;5;203m", format!("Refused: {text}")),
    };
    let summary = if colored {
        format!("{style}{summary}\x1b[0m")
    } else {
        summary
    };
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
    frame.push_str(&format!("{}  └ {summary}\n", " ".repeat(OUTPUT_MARGIN)));
    stdout.write_all(frame.as_bytes())?;
    stdout.flush()?;
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use unicode_width::UnicodeWidthStr;

    use super::{Decision, KeyOutcome, Prompt, Request};

    fn prompt() -> Prompt {
        Prompt::new(Request {
            title: "Bash command".to_string(),
            subject: vec!["sudo emerge -av x11-misc/slock".to_string()],
            note: Some("needs danger-full-access".to_string()),
            session_scope: "sudo emerge".to_string(),
        })
    }

    fn press(prompt: &mut Prompt, code: KeyCode) -> KeyOutcome {
        prompt.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn enter_on_the_first_row_allows_once() {
        let mut p = prompt();
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Decision::Allow)
        );
    }

    #[test]
    fn arrows_and_numbers_pick_rows() {
        let mut p = prompt();
        press(&mut p, KeyCode::Down);
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Decision::AllowForSession)
        );
        let mut p = prompt();
        press(&mut p, KeyCode::Up);
        assert_eq!(p.cursor, 2);
        assert_eq!(
            press(&mut p, KeyCode::Char('2')),
            KeyOutcome::Done(Decision::AllowForSession)
        );
    }

    #[test]
    fn refusing_can_carry_instructions_for_the_model() {
        let mut p = prompt();
        assert_eq!(press(&mut p, KeyCode::Char('3')), KeyOutcome::Continue);
        for ch in "use qsearch".chars() {
            press(&mut p, KeyCode::Char(ch));
        }
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Decision::Deny(Some("use qsearch".to_string())))
        );

        let mut p = prompt();
        press(&mut p, KeyCode::Char('3'));
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            KeyOutcome::Done(Decision::Deny(None))
        );
    }

    #[test]
    fn esc_refuses_and_esc_while_typing_goes_back() {
        let mut p = prompt();
        press(&mut p, KeyCode::Char('3'));
        assert_eq!(press(&mut p, KeyCode::Esc), KeyOutcome::Continue);
        assert!(p.feedback.is_none());
        assert_eq!(
            press(&mut p, KeyCode::Esc),
            KeyOutcome::Done(Decision::Deny(None))
        );
    }

    #[test]
    fn old_letter_answers_still_work() {
        let mut p = prompt();
        assert_eq!(
            press(&mut p, KeyCode::Char('y')),
            KeyOutcome::Done(Decision::Allow)
        );
        let mut p = prompt();
        assert_eq!(
            press(&mut p, KeyCode::Char('a')),
            KeyOutcome::Done(Decision::AllowForSession)
        );
    }

    #[test]
    fn box_lines_are_all_the_same_width() {
        let mut p = prompt();
        p.request.subject.push("x".repeat(200));
        press(&mut p, KeyCode::Char('3'));
        let lines = p.lines(60, false);
        assert!(lines[0].starts_with("╭─ Bash command "));
        assert!(lines
            .iter()
            .any(|l| l.contains("❯ 3. No, and tell the model")));
        assert!(lines
            .iter()
            .any(|l| l.contains("sudo emerge -av x11-misc/slock")));
        let boxed = &lines[..lines.len() - 1];
        assert!(boxed.iter().all(|l| l.width() == 60), "{boxed:#?}");
    }
}
