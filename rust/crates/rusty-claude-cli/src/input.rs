//! Interactive line editor for the REPL prompt, ported from `LineEditor` in
//! `AzureAiHarness`.
//!
//! - Markdown-aware: the input is highlighted while you type (`code`,
//!   **bold**, `#` headings, ``` fences, `/commands`, `@paths`).
//! - Multi-line: pasted text keeps its newlines (bracketed paste, or a burst
//!   of keys arriving faster than anyone types); Shift/Alt+Enter, Ctrl+J or a
//!   trailing `\` insert a newline. Large pastes collapse to a
//!   `[Pasted text #N +M lines]` placeholder that is expanded on submit.
//! - A `/` command menu below the prompt filters as you type: Up/Down
//!   select, Tab completes, Enter runs, Esc hides it.
//! - Tab completes `/commands` and `@paths`; history persists across
//!   sessions; text typed while a reply was running is pre-filled.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{execute, terminal};
use runtime::turn_keys::{screen, Scroll};

use crate::keybindings::{Binding, Chord};
use unicode_width::UnicodeWidthChar;

use crate::render::{current_theme, ThemeName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Submit(String),
    Cancel,
    Exit,
    /// Shift+Tab: switch to the next permission mode. Carries the text
    /// typed so far; hand it back with [`LineEditor::restore_draft`].
    CycleMode(String),
    /// A background shell or agent finished while the prompt was empty: run
    /// a turn so the agent can pick up its result (like Claude Code).
    Wake,
}

const CONTINUATION_PREFIX: &str = "  ";
const MENU_ROWS: usize = 8;
const MAX_HISTORY: usize = 500;
const PASTE_PLACEHOLDER_LINES: usize = 3;
const PASTE_PLACEHOLDER_CHARS: usize = 1000;
/// Keys arriving within this window of each other are a paste, not typing.
const PASTE_BURST_WINDOW: Duration = Duration::from_millis(2);
/// How often the idle prompt checks for finished background jobs.
const WAKE_POLL: Duration = Duration::from_millis(250);
/// A second Ctrl+C/Ctrl+D within this window of the first exits.
const EXIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

pub struct LineEditor {
    prompt: String,
    completions: Vec<String>,
    /// `/` menu entries: command name (without `/`) and description.
    commands: Vec<(String, String)>,
    history: Vec<String>,
    history_path: Option<PathBuf>,
    state: EditState,
    /// Text to put back in the next prompt (after Shift+Tab).
    draft: Option<String>,
    /// Drawing in full-screen mode's bottom pane (while reading a line).
    pane: bool,
    /// Keys from `~/.claw/keybindings.json`, read at each prompt.
    bindings: Vec<(Chord, Binding)>,
}

/// What a key did to the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyAction {
    Continue,
    Done(ReadOutcome),
}

#[derive(Debug, Default)]
struct EditState {
    buf: Vec<char>,
    pos: usize,
    pastes: Vec<(String, String)>,
    paste_counter: usize,
    history_index: usize,
    draft: String,
    menu: Vec<usize>,
    menu_index: usize,
    menu_top: usize,
    menu_query: Option<String>,
    menu_dismissed: Option<String>,
    menu_off: bool,
    /// Printed above the prompt on the next redraw (ambiguous completions).
    listing: Option<String>,
    clear_screen: bool,
    /// When Ctrl+C/Ctrl+D was pressed on an empty input; pressing it again
    /// before [`EXIT_CONFIRM_WINDOW`] passes exits (like Claude Code).
    exit_armed: Option<(Instant, &'static str)>,
    old_rows: usize,
    old_cursor_row: usize,
    /// Ctrl-R reverse history search, while active.
    search: Option<HistorySearch>,
    /// Set for the last redraw after submitting: the bottom rule, status line
    /// and menu are left out, so the transcript keeps only the prompt.
    finished: bool,
}

#[derive(Debug, Default)]
struct HistorySearch {
    query: String,
    /// History index of the shown match.
    found: Option<usize>,
    /// The buffer before the search, restored by Esc.
    original: String,
}

impl LineEditor {
    #[must_use]
    pub fn new(prompt: impl Into<String>, completions: Vec<String>) -> Self {
        let history_path =
            (!cfg!(test)).then(|| runtime::default_config_home().join("history.jsonl"));
        let mut editor = Self {
            prompt: prompt.into(),
            completions: Vec::new(),
            commands: Vec::new(),
            history: history_path
                .as_deref()
                .map(load_history)
                .unwrap_or_default(),
            history_path,
            state: EditState::default(),
            draft: None,
            pane: false,
            bindings: Vec::new(),
        };
        editor.set_completions(completions);
        editor
    }

    pub fn push_history(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.trim().is_empty() || self.history.last() == Some(&entry) {
            return;
        }
        self.history.push(entry.clone());
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
        if let Some(path) = &self.history_path {
            append_history(path, &entry);
        }
    }

    /// Puts `text` back into the next prompt, e.g. after a Shift+Tab mode
    /// switch interrupted typing.
    pub fn restore_draft(&mut self, text: String) {
        self.draft = (!text.is_empty()).then_some(text);
    }

    pub fn set_completions(&mut self, completions: Vec<String>) {
        self.completions = normalize_completions(completions);
        self.commands = menu_commands(&self.completions);
    }

    pub fn read_line(&mut self) -> io::Result<ReadOutcome> {
        if !io::stdin().is_terminal() || !runtime::turn_keys::stdout_is_terminal() {
            return self.read_line_fallback();
        }
        if !cfg!(test) {
            self.bindings = crate::keybindings::load().0;
        }
        // In full-screen mode the input box is drawn in the bottom pane.
        self.pane = screen::active();
        if self.pane {
            screen::begin_pane();
        }
        let outcome = self.read_line_on_terminal();
        self.pane = false;
        outcome
    }

    fn read_line_on_terminal(&mut self) -> io::Result<ReadOutcome> {
        self.state = EditState {
            history_index: self.history.len(),
            old_rows: 1,
            ..EditState::default()
        };
        let type_ahead = runtime::turn_keys::take_type_ahead();
        self.insert_text(type_ahead.trim_end_matches('\n'));
        if let Some(draft) = self.draft.take() {
            self.insert_text(&draft);
        }

        let _terminal = TerminalModeGuard::enter()?;
        self.print_separator()?;
        self.refresh()?;
        loop {
            if let Some((armed_at, _)) = self.state.exit_armed {
                // Drop the "press again to exit" hint once it expires.
                let remaining = EXIT_CONFIRM_WINDOW.saturating_sub(armed_at.elapsed());
                if !event::poll(remaining)? {
                    self.state.exit_armed = None;
                    self.refresh()?;
                    continue;
                }
            }
            // Poll instead of blocking, so a finished background job can wake
            // the agent while nothing is being typed.
            if !event::poll(WAKE_POLL)? {
                if self.state.buf.is_empty()
                    && self.state.search.is_none()
                    && runtime::global_background_jobs().has_notices()
                {
                    self.erase()?;
                    return Ok(ReadOutcome::Wake);
                }
                continue;
            }
            let event = event::read()?;
            let action = self.handle_event(event)?;
            if let KeyAction::Done(outcome) = action {
                if matches!(outcome, ReadOutcome::CycleMode(_)) {
                    // Shift+Tab redraws the box in place with the new mode in
                    // the status line, instead of leaving a finished prompt.
                    self.erase()?;
                } else {
                    self.finish()?;
                }
                return Ok(outcome);
            }
            self.refresh()?;
        }
    }

    fn handle_event(&mut self, event: Event) -> io::Result<KeyAction> {
        if self.pane {
            if let Some(scroll) = scroll_for(&event) {
                screen::scroll(scroll);
                return Ok(KeyAction::Continue);
            }
            if let Some(mouse) = selection_mouse(&event) {
                screen::mouse(mouse);
                return Ok(KeyAction::Continue);
            }
        }
        match event {
            Event::Paste(text) => {
                self.insert_text(&text);
                Ok(KeyAction::Continue)
            }
            Event::Key(key) if key.kind == KeyEventKind::Release => Ok(KeyAction::Continue),
            Event::Key(key) if key_text(&key).is_some() && event::poll(Duration::ZERO)? => {
                // A burst of keys is a paste: take it all as text, so pasted
                // Enters never submit and pasted Tabs never complete.
                let mut text = key_text(&key).unwrap_or_default();
                let mut leftover = None;
                while event::poll(PASTE_BURST_WINDOW)? {
                    match event::read()? {
                        Event::Key(next) if next.kind == KeyEventKind::Release => {}
                        Event::Key(next) if key_text(&next).is_some() => {
                            text.push_str(&key_text(&next).unwrap_or_default());
                        }
                        other => {
                            leftover = Some(other);
                            break;
                        }
                    }
                }
                self.insert_text(&text);
                match leftover {
                    Some(event) => self.handle_event(event),
                    None => Ok(KeyAction::Continue),
                }
            }
            Event::Key(key) => Ok(self.handle_key(key)),
            _ => Ok(KeyAction::Continue),
        }
    }

    /// Most recent history entry before `before` containing `query`.
    fn find_in_history(&self, query: &str, before: usize) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        let needle = query.to_lowercase();
        self.history[..before.min(self.history.len())]
            .iter()
            .rposition(|entry| entry.to_lowercase().contains(&needle))
    }

    /// Keys while Ctrl-R search is active. `None` ends the search and lets
    /// the key act normally on the match (arrows, Home, ...).
    fn handle_search_key(&mut self, key: KeyEvent) -> Option<KeyAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let mut search = self.state.search.take()?;
        match key.code {
            KeyCode::Char('r') if ctrl => {
                let before = search.found.unwrap_or(self.history.len());
                if let Some(index) = self.find_in_history(&search.query, before) {
                    search.found = Some(index);
                }
            }
            KeyCode::Esc | KeyCode::Char('g' | 'c') if key.code == KeyCode::Esc || ctrl => {
                self.state.set_buffer(&search.original);
                return Some(KeyAction::Continue);
            }
            KeyCode::Enter => {
                return Some(KeyAction::Done(ReadOutcome::Submit(
                    self.state.expanded_text(),
                )));
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.found = self.find_in_history(&search.query, self.history.len());
            }
            KeyCode::Char(ch) if !ctrl => {
                search.query.push(ch);
                search.found = self.find_in_history(&search.query, self.history.len());
            }
            _ => return None,
        }
        let shown = search.found.map_or_else(
            || search.original.clone(),
            |index| self.history[index].clone(),
        );
        self.state.set_buffer(&shown);
        self.state.search = Some(search);
        Some(KeyAction::Continue)
    }

    /// A key: a user keybinding if one matches, else the built-in keys.
    fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        if self.state.search.is_none() {
            let chord = Chord::of(&key);
            if let Some((_, binding)) = self.bindings.iter().find(|(c, _)| *c == chord) {
                let binding = binding.clone();
                return self.apply_binding(binding);
            }
        }
        self.handle_builtin_key(key)
    }

    fn apply_binding(&mut self, binding: Binding) -> KeyAction {
        let key = |code: KeyCode, modifiers: KeyModifiers| KeyEvent::new(code, modifiers);
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let builtin = match binding {
            Binding::Unbound => return KeyAction::Continue,
            Binding::Text(text) => {
                self.state.insert(&text);
                self.update_menu();
                return KeyAction::Continue;
            }
            Binding::Command(command) => {
                // What was typed comes back in the next prompt.
                let typed = self.state.expanded_text();
                if !typed.trim().is_empty() {
                    self.draft = Some(typed);
                }
                return KeyAction::Done(ReadOutcome::Submit(command));
            }
            Binding::Action("clear-input") => {
                self.state.set_buffer("");
                self.update_menu();
                return KeyAction::Continue;
            }
            Binding::Action(action) => match action {
                "submit" => key(KeyCode::Enter, none),
                "newline" => key(KeyCode::Char('j'), ctrl),
                "cancel" => key(KeyCode::Esc, none),
                "history-prev" => key(KeyCode::Up, none),
                "history-next" => key(KeyCode::Down, none),
                "history-search" => key(KeyCode::Char('r'), ctrl),
                "line-start" => key(KeyCode::Home, none),
                "line-end" => key(KeyCode::End, none),
                "word-left" => key(KeyCode::Left, ctrl),
                "word-right" => key(KeyCode::Right, ctrl),
                "delete-word" => key(KeyCode::Char('w'), ctrl),
                "delete-to-start" => key(KeyCode::Char('u'), ctrl),
                "delete-to-end" => key(KeyCode::Char('k'), ctrl),
                "clear-screen" => key(KeyCode::Char('l'), ctrl),
                "cycle-mode" => key(KeyCode::BackTab, KeyModifiers::SHIFT),
                "complete" => key(KeyCode::Tab, none),
                _ => return KeyAction::Continue,
            },
        };
        self.handle_builtin_key(builtin)
    }

    #[allow(clippy::too_many_lines)]
    fn handle_builtin_key(&mut self, key: KeyEvent) -> KeyAction {
        if self.state.search.is_some() {
            if let Some(action) = self.handle_search_key(key) {
                return action;
            }
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let state = &mut self.state;
        // Any other key cancels a pending "press again to exit".
        let exit_armed = state.exit_armed.take();

        match key.code {
            KeyCode::Char(c @ ('c' | 'd')) if ctrl => {
                let label = if c == 'c' { "Ctrl-C" } else { "Ctrl-D" };
                if !state.buf.is_empty() {
                    if c == 'c' {
                        state.set_buffer("");
                    } else {
                        state.delete();
                    }
                } else if exit_armed.is_some_and(|(armed_at, armed_label)| {
                    armed_label == label && armed_at.elapsed() < EXIT_CONFIRM_WINDOW
                }) {
                    return KeyAction::Done(ReadOutcome::Exit);
                } else {
                    state.exit_armed = Some((Instant::now(), label));
                }
            }
            KeyCode::Char('r') if ctrl => {
                state.search = Some(HistorySearch {
                    original: state.buf.iter().collect(),
                    ..HistorySearch::default()
                });
            }
            KeyCode::Char('j') if ctrl => state.insert("\n"),
            KeyCode::Enter if shift || alt => state.insert("\n"),
            KeyCode::Enter => {
                if state.pos == state.buf.len() && state.buf.last() == Some(&'\\') {
                    state.buf.pop();
                    state.pos -= 1;
                    state.insert("\n");
                    return KeyAction::Continue;
                }
                if let Some(&command) = state.menu.get(state.menu_index) {
                    let name = format!("/{}", self.commands[command].0);
                    state.set_buffer(&name);
                }
                return KeyAction::Done(ReadOutcome::Submit(state.expanded_text()));
            }
            KeyCode::BackTab => {
                return KeyAction::Done(ReadOutcome::CycleMode(state.expanded_text()));
            }
            KeyCode::Esc => {
                if state.menu.is_empty() {
                    state.set_buffer("");
                } else {
                    state.menu_dismissed.clone_from(&state.menu_query);
                }
            }
            KeyCode::Tab => {
                if let Some(&command) = state.menu.get(state.menu_index) {
                    let name = format!("/{} ", self.commands[command].0);
                    state.set_buffer(&name);
                } else {
                    let completions = &self.completions;
                    complete(state, completions);
                }
            }
            KeyCode::Backspace if ctrl || alt => state.delete_word_back(),
            KeyCode::Char('w') if ctrl => state.delete_word_back(),
            KeyCode::Backspace | KeyCode::Char('h') if key.code == KeyCode::Backspace || ctrl => {
                state.backspace();
            }
            KeyCode::Delete => state.delete(),
            KeyCode::Left if ctrl || alt => state.pos = state.word_left(),
            KeyCode::Right if ctrl || alt => state.pos = state.word_right(),
            KeyCode::Left => state.pos = state.pos.saturating_sub(1),
            KeyCode::Right => state.pos = (state.pos + 1).min(state.buf.len()),
            KeyCode::Home => state.pos = state.line_start(),
            KeyCode::Char('a') if ctrl => state.pos = state.line_start(),
            KeyCode::End => state.pos = state.line_end(),
            KeyCode::Char('e') if ctrl => state.pos = state.line_end(),
            KeyCode::Char('u') if ctrl => {
                let start = state.line_start();
                state.buf.drain(start..state.pos);
                state.pos = start;
            }
            KeyCode::Char('k') if ctrl => {
                let end = state.line_end();
                state.buf.drain(state.pos..end);
            }
            KeyCode::Char('l') if ctrl => state.clear_screen = true,
            KeyCode::Up if !state.menu.is_empty() => {
                state.menu_index = (state.menu_index + state.menu.len() - 1) % state.menu.len();
            }
            KeyCode::Down if !state.menu.is_empty() => {
                state.menu_index = (state.menu_index + 1) % state.menu.len();
            }
            KeyCode::Up => {
                if state.buf[..state.pos].contains(&'\n') {
                    state.move_vertical(-1);
                } else if state.history_index > 0 {
                    if state.history_index == self.history.len() {
                        state.draft = state.text();
                    }
                    state.history_index -= 1;
                    let entry = self.history[state.history_index].clone();
                    state.set_buffer(&entry);
                }
            }
            KeyCode::Down => {
                if state.buf[state.pos..].contains(&'\n') {
                    state.move_vertical(1);
                } else if state.history_index < self.history.len() {
                    state.history_index += 1;
                    let entry = if state.history_index == self.history.len() {
                        state.draft.clone()
                    } else {
                        self.history[state.history_index].clone()
                    };
                    state.set_buffer(&entry);
                }
            }
            KeyCode::Char(ch) if !ctrl && !alt => state.insert(&ch.to_string()),
            _ => {}
        }
        self.update_menu();
        KeyAction::Continue
    }

    fn insert_text(&mut self, text: &str) {
        self.state.insert_text(text);
        self.update_menu();
    }

    /// Recomputes the menu: open while the input is a single `/word`. A bare
    /// `/` lists everything in declared order; otherwise exact match, then
    /// prefix matches A-Z, then names containing the query.
    fn update_menu(&mut self) {
        let state = &mut self.state;
        let text = state.text();
        let query =
            (!state.menu_off && text.starts_with('/') && !text.contains(char::is_whitespace))
                .then(|| text[1..].to_ascii_lowercase());
        if query.is_none() || query == state.menu_dismissed {
            if query != state.menu_dismissed {
                state.menu_dismissed = None;
            }
            state.menu.clear();
            state.menu_query = query;
            return;
        }
        state.menu_dismissed = None;
        if query == state.menu_query && !state.menu.is_empty() {
            return;
        }
        let query = query.unwrap_or_default();
        let mut ranked = self
            .commands
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
                Some((
                    rank,
                    if rank == 1 && !query.is_empty() {
                        name
                    } else {
                        String::new()
                    },
                    index,
                ))
            })
            .collect::<Vec<_>>();
        ranked.sort();
        state.menu = ranked.into_iter().map(|(_, _, index)| index).collect();
        state.menu_query = Some(query);
        state.menu_index = 0;
        state.menu_top = 0;
    }

    /// The menu's screen lines (selected row marked), scrolled so the
    /// selection is visible. One screen row each, so redraws stay exact.
    fn menu_lines(&mut self, width: usize) -> Vec<(String, bool)> {
        let state = &mut self.state;
        if state.menu.is_empty() || width < 20 {
            return Vec::new();
        }
        if state.menu_index < state.menu_top {
            state.menu_top = state.menu_index;
        }
        if state.menu_index >= state.menu_top + MENU_ROWS {
            state.menu_top = state.menu_index + 1 - MENU_ROWS;
        }
        let visible =
            &state.menu[state.menu_top..(state.menu_top + MENU_ROWS).min(state.menu.len())];
        let name_width = visible
            .iter()
            .map(|&index| self.commands[index].0.chars().count())
            .max()
            .unwrap_or(0)
            .min(22)
            + 1;
        let mut lines = visible
            .iter()
            .enumerate()
            .map(|(offset, &index)| {
                let (name, description) = &self.commands[index];
                let selected = state.menu_top + offset == state.menu_index;
                let line = format!(
                    "{} /{name:<name_width$} {description}",
                    if selected { ">" } else { " " }
                );
                (truncate_to_width(&line, width - 1), selected)
            })
            .collect::<Vec<_>>();
        if state.menu.len() > visible.len() {
            lines.push((
                format!(
                    "  ({}/{}, ↑↓ for more)",
                    state.menu_index + 1,
                    state.menu.len()
                ),
                false,
            ));
        }
        lines
    }

    fn refresh(&mut self) -> io::Result<()> {
        let width = terminal::size().map_or(80, |(cols, _)| usize::from(cols).max(10));
        let colored = current_theme() != ThemeName::None;
        let mut out = String::new();

        if std::mem::take(&mut self.state.clear_screen) {
            if self.pane {
                screen::clear_conversation();
            }
            out.push_str("\x1b[2J\x1b[H");
            self.state.old_rows = 1;
            self.state.old_cursor_row = 0;
        } else {
            // Go to the last row of the previous render, then clear upwards.
            let state = &self.state;
            if state.old_rows - 1 > state.old_cursor_row {
                out.push_str(&format!(
                    "\x1b[{}B",
                    state.old_rows - 1 - state.old_cursor_row
                ));
            }
            for _ in 1..state.old_rows {
                out.push_str("\r\x1b[K\x1b[1A");
            }
            out.push_str("\r\x1b[K");
        }
        if let Some(listing) = self.state.listing.take() {
            let listing = paint(colored, "\x1b[38;5;245m", &listing.replace('\n', "\r\n"));
            if self.pane {
                // Full-screen mode: the listing goes into the conversation.
                let mut stdout = io::stdout();
                write!(stdout, "{listing}\r\n")?;
                stdout.flush()?;
            } else {
                out.push_str(&listing);
                out.push_str("\r\n");
            }
        }

        // Prompt and highlighted buffer.
        out.push_str(&paint(colored, &prompt_style(), &self.prompt));
        out.push_str(&render_buffer(&self.state.buf, colored));

        // Text ending exactly at the right edge: force the wrap so the cursor
        // position is unambiguous.
        let (mut end_row, end_col) = layout(&self.prompt, &self.state.buf, width);
        if end_col == width {
            out.push_str("\r\n");
            end_row += 1;
        }
        if let Some((_, label)) = self.state.exit_armed {
            out.push_str("\r\n\x1b[K");
            let hint = format!("Press {label} again to exit");
            out.push_str(&paint(
                colored,
                "\x1b[38;5;245m",
                &truncate_to_width(&hint, width - 1),
            ));
            end_row += 1;
        }
        if let Some(search) = &self.state.search {
            out.push_str("\r\n\x1b[K");
            let status = if search.query.is_empty() || search.found.is_some() {
                ""
            } else {
                " (no match)"
            };
            let hint = format!(
                "search history: {}{status} · ctrl+r older · enter run · esc cancel",
                search.query
            );
            out.push_str(&paint(
                colored,
                "\x1b[36m",
                &truncate_to_width(&hint, width - 1),
            ));
            end_row += 1;
        }
        if !self.state.finished {
            // The input box's bottom rule, then the `/` menu while it is
            // open, otherwise the status line (mode, model, context), like
            // Claude Code.
            out.push_str("\r\n\x1b[K");
            out.push_str(&runtime::turn_keys::rule_line(width, colored));
            end_row += 1;
            let menu = self.menu_lines(width);
            if menu.is_empty() {
                out.push_str("\r\n\x1b[K");
                out.push_str(&runtime::turn_keys::footer_status_line(width, colored));
                end_row += 1;
            }
            for (line, selected) in menu {
                out.push_str("\r\n\x1b[K");
                let style = if selected {
                    "\x1b[36m"
                } else {
                    "\x1b[38;5;245m"
                };
                out.push_str(&paint(colored, style, &line));
                end_row += 1;
            }
        }

        let (mut cursor_row, mut cursor_col) =
            layout(&self.prompt, &self.state.buf[..self.state.pos], width);
        if cursor_col == width {
            cursor_row += 1;
            cursor_col = 0;
        }
        if end_row > cursor_row {
            out.push_str(&format!("\x1b[{}A", end_row - cursor_row));
        }
        out.push('\r');
        if cursor_col > 0 {
            out.push_str(&format!("\x1b[{cursor_col}C"));
        }

        self.emit(&out)?;
        self.state.old_rows = end_row + 1;
        self.state.old_cursor_row = cursor_row;
        Ok(())
    }

    /// Shows a prompt that was queued during the last turn as if it had just
    /// been typed and submitted.
    pub fn echo_queued(&self, text: &str) -> io::Result<()> {
        self.print_separator()?;
        let colored = current_theme() != ThemeName::None;
        let buf = text.chars().collect::<Vec<_>>();
        let mut stdout = io::stdout();
        write!(
            stdout,
            "{}{}\r\n",
            paint(colored, &prompt_style(), &self.prompt),
            render_buffer(&buf, colored)
        )?;
        stdout.flush()
    }

    /// Writes part of the input box: into full-screen mode's bottom pane
    /// while reading a line there, otherwise to the terminal.
    fn emit(&self, text: &str) -> io::Result<()> {
        if self.pane {
            screen::write_pane(text.as_bytes())
        } else {
            let mut stdout = io::stdout();
            stdout.write_all(text.as_bytes())?;
            stdout.flush()
        }
    }

    /// A dim rule above the prompt, separating the input from the output.
    fn print_separator(&self) -> io::Result<()> {
        let width = terminal::size().map_or(80, |(cols, _)| usize::from(cols));
        let rule = "─".repeat(width.clamp(10, 200));
        let colored = current_theme() != ThemeName::None;
        self.emit(&format!("\r{}\r\n", paint(colored, &rule_style(), &rule)))
    }

    /// Removes the whole input box, top rule included, and leaves the cursor
    /// where the rule was, so the next prompt is drawn in the same place.
    fn erase(&mut self) -> io::Result<()> {
        let up = self.state.old_cursor_row + 1;
        self.emit(&format!("\x1b[{up}A\r\x1b[J"))?;
        if self.pane {
            screen::clear_pane();
        }
        Ok(())
    }

    /// Erases the menu and leaves the cursor on a fresh line below the input.
    /// In full-screen mode the submitted prompt moves into the conversation.
    fn finish(&mut self) -> io::Result<()> {
        self.state.finished = true;
        self.state.menu_off = true;
        self.state.menu.clear();
        self.state.pos = self.state.buf.len();
        self.refresh()?;
        self.emit("\r\n")?;
        if self.pane {
            screen::commit_pane();
        }
        Ok(())
    }

    fn read_line_fallback(&self) -> io::Result<ReadOutcome> {
        let mut stdout = io::stdout();
        write!(stdout, "{}", self.prompt)?;
        stdout.flush()?;

        let mut buffer = String::new();
        let bytes_read = io::stdin().read_line(&mut buffer)?;
        if bytes_read == 0 {
            return Ok(ReadOutcome::Exit);
        }

        while matches!(buffer.chars().last(), Some('\n' | '\r')) {
            buffer.pop();
        }
        Ok(ReadOutcome::Submit(buffer))
    }
}

impl EditState {
    fn text(&self) -> String {
        self.buf.iter().collect()
    }

    fn set_buffer(&mut self, text: &str) {
        self.buf = text.chars().collect();
        self.pos = self.buf.len();
    }

    fn insert(&mut self, text: &str) {
        let chars = text.chars().collect::<Vec<_>>();
        let count = chars.len();
        self.buf.splice(self.pos..self.pos, chars);
        self.pos += count;
    }

    /// Inserts typed or pasted text; large pastes become a placeholder.
    fn insert_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = text.trim_end_matches('\n').matches('\n').count() + 1;
        if lines >= PASTE_PLACEHOLDER_LINES || text.chars().count() > PASTE_PLACEHOLDER_CHARS {
            self.paste_counter += 1;
            let token = format!("[Pasted text #{} +{lines} lines]", self.paste_counter);
            self.pastes
                .push((token.clone(), text.trim_end_matches('\n').to_string()));
            self.insert(&token);
        } else {
            self.insert(&text);
        }
    }

    fn expanded_text(&self) -> String {
        let mut text = self.text();
        for (token, content) in &self.pastes {
            text = text.replace(token, content);
        }
        text
    }

    /// The paste placeholder covering buffer index `index`, as
    /// `(start, length, paste index)`.
    fn placeholder_at(&self, index: usize) -> Option<(usize, usize, usize)> {
        self.pastes
            .iter()
            .enumerate()
            .find_map(|(paste, (token, _))| {
                let token = token.chars().collect::<Vec<_>>();
                let first = index.saturating_sub(token.len() - 1);
                (first..=index)
                    .find(|&start| self.buf[start..].starts_with(&token))
                    .map(|start| (start, token.len(), paste))
            })
    }

    /// Removes the character at `index`, or the whole placeholder it is in.
    fn remove_at(&mut self, index: usize) {
        let (start, length) = match self.placeholder_at(index) {
            Some((start, length, paste)) => {
                self.pastes.remove(paste);
                (start, length)
            }
            None => (index, 1),
        };
        self.buf.drain(start..start + length);
        if self.pos > start {
            self.pos = start.max(self.pos.saturating_sub(length));
        }
    }

    fn backspace(&mut self) {
        if self.pos > 0 {
            self.remove_at(self.pos - 1);
        }
    }

    fn delete(&mut self) {
        if self.pos < self.buf.len() {
            self.remove_at(self.pos);
        }
    }

    fn word_left(&self) -> usize {
        let mut pos = self.pos;
        while pos > 0 && self.buf[pos - 1].is_whitespace() {
            pos -= 1;
        }
        while pos > 0 && !self.buf[pos - 1].is_whitespace() {
            pos -= 1;
        }
        pos
    }

    fn word_right(&self) -> usize {
        let mut pos = self.pos;
        while pos < self.buf.len() && self.buf[pos].is_whitespace() {
            pos += 1;
        }
        while pos < self.buf.len() && !self.buf[pos].is_whitespace() {
            pos += 1;
        }
        pos
    }

    fn delete_word_back(&mut self) {
        let start = self.word_left();
        self.buf.drain(start..self.pos);
        self.pos = start;
    }

    fn line_start(&self) -> usize {
        self.buf[..self.pos]
            .iter()
            .rposition(|&ch| ch == '\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.buf[self.pos..]
            .iter()
            .position(|&ch| ch == '\n')
            .map_or(self.buf.len(), |index| self.pos + index)
    }

    fn move_vertical(&mut self, direction: i32) {
        let column = self.pos - self.line_start();
        if direction < 0 {
            let previous_end = self.line_start() - 1;
            let previous_start = self.buf[..previous_end]
                .iter()
                .rposition(|&ch| ch == '\n')
                .map_or(0, |index| index + 1);
            self.pos = (previous_start + column).min(previous_end);
        } else {
            let next_start = self.line_end() + 1;
            let next_end = self.buf[next_start..]
                .iter()
                .position(|&ch| ch == '\n')
                .map_or(self.buf.len(), |index| next_start + index);
            self.pos = (next_start + column).min(next_end);
        }
    }
}

/// Tab completion of a leading `/command` or an `@path` before the cursor.
fn complete(state: &mut EditState, completions: &[String]) {
    // A slash command completes as a whole (`/model o` → `/model opus`);
    // anything else completes the word before the cursor.
    let whole_command = state.buf.first() == Some(&'/') && !state.buf[..state.pos].contains(&'\n');
    let start = if whole_command {
        0
    } else {
        state.buf[..state.pos]
            .iter()
            .rposition(|ch| ch.is_whitespace())
            .map_or(0, |index| index + 1)
    };
    let token = state.buf[start..state.pos].iter().collect::<String>();

    let candidates = if start == 0 && token.starts_with('/') {
        completions
            .iter()
            .filter(|candidate| candidate.starts_with(&token))
            .cloned()
            .collect::<Vec<_>>()
    } else if let Some(partial) = token.strip_prefix('@') {
        complete_path(partial)
            .into_iter()
            .map(|path| format!("@{path}"))
            .collect()
    } else {
        return;
    };
    if candidates.is_empty() {
        return;
    }

    let common = common_prefix(&candidates);
    if common.chars().count() > token.chars().count() {
        let suffix = if candidates.len() == 1 && !common.ends_with('/') && !common.ends_with(' ') {
            " "
        } else {
            ""
        };
        state
            .buf
            .splice(start..state.pos, format!("{common}{suffix}").chars());
        state.pos = start + common.chars().count() + suffix.len();
    } else if candidates.len() > 1 {
        let shown = candidates
            .iter()
            .take(60)
            .map(|candidate| candidate.trim_start_matches('@'))
            .collect::<Vec<_>>()
            .join("  ");
        let more = candidates.len().saturating_sub(60);
        state.listing = Some(if more > 0 {
            format!("{shown}  … (+{more})")
        } else {
            shown
        });
    }
}

fn complete_path(partial: &str) -> Vec<String> {
    let (dir_part, prefix) = partial
        .rfind('/')
        .map_or(("", partial), |index| partial.split_at(index + 1));
    let dir = if let Some(rest) = dir_part.strip_prefix("~/") {
        std::env::var_os("HOME").map_or_else(
            || PathBuf::from(dir_part),
            |home| Path::new(&home).join(rest),
        )
    } else if dir_part.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(dir_part)
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let hidden_ok = prefix.starts_with('.') || !name.starts_with('.');
            (name.starts_with(prefix) && hidden_ok).then(|| {
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                format!("{dir_part}{name}{}", if is_dir { "/" } else { "" })
            })
        })
        .collect::<Vec<_>>();
    names.sort();
    names.truncate(500);
    names
}

fn common_prefix(items: &[String]) -> String {
    let mut prefix = items[0].chars().collect::<Vec<_>>();
    for item in &items[1..] {
        let shared = prefix
            .iter()
            .zip(item.chars())
            .take_while(|(a, b)| *a == b)
            .count();
        prefix.truncate(shared);
    }
    prefix.into_iter().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Command,
    Mention,
    Code,
    Bold,
    Heading,
    Fence,
    Placeholder,
}

impl Style {
    fn sgr(self) -> &'static str {
        match self {
            Self::Plain => "\x1b[0m",
            Self::Command => "\x1b[0;1;36m",
            Self::Mention => "\x1b[0;34m",
            Self::Code => "\x1b[0;32m",
            Self::Bold => "\x1b[0;1m",
            Self::Heading => "\x1b[0;1;33m",
            Self::Fence => "\x1b[0;90m",
            Self::Placeholder => "\x1b[0;35m",
        }
    }
}

/// Markdown-aware styles for each character of the input.
fn highlight(buf: &[char]) -> Vec<Style> {
    let mut styles = vec![Style::Plain; buf.len()];
    let mut in_fence = false;
    let mut line_start = 0;
    while line_start <= buf.len() {
        let line_end = buf[line_start..]
            .iter()
            .position(|&ch| ch == '\n')
            .map_or(buf.len(), |index| line_start + index);
        let line = &buf[line_start..line_end];
        let trimmed = line.iter().skip_while(|ch| **ch == ' ').collect::<String>();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            styles[line_start..line_end].fill(Style::Fence);
            in_fence = !in_fence;
        } else if in_fence {
            styles[line_start..line_end].fill(Style::Code);
        } else if trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ') {
            styles[line_start..line_end].fill(Style::Heading);
        } else {
            highlight_inline(line, &mut styles[line_start..line_end]);
        }
        line_start = line_end + 1;
    }
    if buf.first() == Some(&'/') {
        let end = buf
            .iter()
            .position(|ch| ch.is_whitespace())
            .unwrap_or(buf.len());
        styles[..end].fill(Style::Command);
    }
    styles
}

fn highlight_inline(line: &[char], styles: &mut [Style]) {
    let find = |from: usize, pattern: &[char]| {
        (from..=line.len().saturating_sub(pattern.len()))
            .find(|&index| line[index..].starts_with(pattern))
    };
    let mut index = 0;
    while index < line.len() {
        let span = match line[index] {
            '`' => find(index + 1, &['`']).map(|end| (end + 1, Style::Code)),
            '*' if line[index..].starts_with(&['*', '*']) => {
                find(index + 2, &['*', '*']).map(|end| (end + 2, Style::Bold))
            }
            '@' if index == 0 || line[index - 1].is_whitespace() => {
                let end = line[index..]
                    .iter()
                    .position(|ch| ch.is_whitespace())
                    .map_or(line.len(), |offset| index + offset);
                Some((end, Style::Mention))
            }
            '[' if line[index..].starts_with(&"[Pasted text #".chars().collect::<Vec<_>>()) => {
                find(index, &[']']).map(|end| (end + 1, Style::Placeholder))
            }
            _ => None,
        };
        match span {
            Some((end, style)) => {
                styles[index..end].fill(style);
                index = end;
            }
            None => index += 1,
        }
    }
}

/// The buffer as terminal output: highlighted, with `\r\n` plus the
/// continuation prefix for each newline (the terminal is in raw mode).
fn render_buffer(buf: &[char], colored: bool) -> String {
    let styles = highlight(buf);
    let mut out = String::new();
    let mut current = Style::Plain;
    for (&ch, &style) in buf.iter().zip(&styles) {
        if ch == '\n' {
            out.push_str("\r\n");
            out.push_str(CONTINUATION_PREFIX);
            continue;
        }
        if colored && style != current {
            out.push_str(style.sgr());
            current = style;
        }
        out.push(ch);
    }
    if colored && current != Style::Plain {
        out.push_str(Style::Plain.sgr());
    }
    out
}

/// Row and column after laying out the prompt and `buf` in a terminal of
/// the given width. The column may equal the width (a pending wrap).
fn layout(prompt: &str, buf: &[char], width: usize) -> (usize, usize) {
    let mut row = 0;
    let mut col = 0;
    let mut place = |ch: char| {
        if ch == '\n' {
            row += 1;
            col = 0;
            for _ in CONTINUATION_PREFIX.chars() {
                col += 1;
            }
            return;
        }
        let char_width = ch.width().unwrap_or(0);
        if col + char_width > width {
            row += 1;
            col = 0;
        }
        col += char_width;
    };
    prompt.chars().for_each(&mut place);
    buf.iter().copied().for_each(&mut place);
    (row, col)
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width > width.saturating_sub(1) {
            out.push('…');
            return out;
        }
        used += char_width;
        out.push(ch);
    }
    out
}

/// The `❯` in the session's `/color`.
fn prompt_style() -> String {
    format!("\x1b[1;{}m", runtime::turn_keys::prompt_color_sgr())
}

/// The input box rules in the session's `/color`.
fn rule_style() -> String {
    format!("\x1b[{}m", runtime::turn_keys::prompt_color_sgr())
}

fn paint(colored: bool, style: &str, text: &str) -> String {
    if colored {
        format!("{style}{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The text a key inserts when it arrives as part of a paste burst.
fn key_text(key: &KeyEvent) -> Option<String> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return None;
    }
    match key.code {
        KeyCode::Char(ch) => Some(ch.to_string()),
        KeyCode::Enter => Some("\n".to_string()),
        KeyCode::Tab => Some("\t".to_string()),
        _ => None,
    }
}

/// Full-screen mode's text selection: left button press, drag, release
/// (with Shift, Alt or Ctrl held the terminal selects by itself).
fn selection_mouse(event: &Event) -> Option<screen::Mouse> {
    let Event::Mouse(mouse) = event else {
        return None;
    };
    if !mouse.modifiers.is_empty() {
        return None;
    }
    let (row, col) = (usize::from(mouse.row), usize::from(mouse.column));
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(screen::Mouse::Press { row, col }),
        MouseEventKind::Drag(MouseButton::Left) => Some(screen::Mouse::Drag { row, col }),
        MouseEventKind::Up(MouseButton::Left) => Some(screen::Mouse::Release { row, col }),
        _ => None,
    }
}

/// Full-screen mode's scrolling: mouse wheel, PgUp/PgDn, Ctrl+Home/End.
fn scroll_for(event: &Event) -> Option<Scroll> {
    match event {
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Some(Scroll::Up(runtime::turn_keys::wheel_rows())),
            MouseEventKind::ScrollDown => Some(Scroll::Down(runtime::turn_keys::wheel_rows())),
            _ => None,
        },
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::PageUp => Some(Scroll::PageUp),
                KeyCode::PageDown => Some(Scroll::PageDown),
                KeyCode::Home if ctrl => Some(Scroll::Top),
                KeyCode::End if ctrl => Some(Scroll::Bottom),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Raw mode, bracketed paste and (where supported) the kitty keyboard
/// protocol, so Shift+Enter is distinguishable; all undone on drop.
struct TerminalModeGuard {
    keyboard_enhanced: bool,
}

impl TerminalModeGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        let _ = execute!(stdout, EnableBracketedPaste);
        static SUPPORTS_ENHANCEMENT: OnceLock<bool> = OnceLock::new();
        let keyboard_enhanced = *SUPPORTS_ENHANCEMENT
            .get_or_init(|| terminal::supports_keyboard_enhancement().unwrap_or(false));
        if keyboard_enhanced {
            let _ = execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
        }
        Ok(Self { keyboard_enhanced })
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(stdout, DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
    }
}

fn load_history(path: &Path) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let entries = contents
        .lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|entry| !entry.trim().is_empty())
        .collect::<Vec<_>>();
    let keep = entries[entries.len().saturating_sub(MAX_HISTORY)..].to_vec();
    if entries.len() > MAX_HISTORY * 2 {
        // Compact the file so it does not grow forever.
        let compacted = keep
            .iter()
            .filter_map(|entry| serde_json::to_string(entry).ok())
            .map(|line| line + "\n")
            .collect::<String>();
        let _ = fs::write(path, compacted);
    }
    keep
}

fn append_history(path: &Path, entry: &str) {
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

fn normalize_completions(completions: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    completions
        .into_iter()
        .filter(|candidate| candidate.starts_with('/'))
        .filter(|candidate| seen.insert(candidate.clone()))
        .collect()
}

/// Menu entries for the single-word completions, in the order commands are
/// declared (most useful first), each with its summary.
fn menu_commands(completions: &[String]) -> Vec<(String, String)> {
    let specs = commands::slash_command_specs();
    let mut entries = completions
        .iter()
        .map(|candidate| candidate.trim_end().trim_start_matches('/'))
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|name| {
            let spec = specs
                .iter()
                .position(|spec| spec.name == name || spec.aliases.contains(&name));
            let description = spec.map_or("", |index| specs[index].summary);
            (
                spec.unwrap_or(usize::MAX),
                name.to_string(),
                description.to_string(),
            )
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
        .into_iter()
        .map(|(_, name, description)| (name, description))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        common_prefix, highlight, layout, EditState, KeyAction, LineEditor, ReadOutcome, Style,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn editor() -> LineEditor {
        LineEditor::new(
            "> ",
            vec![
                "/help".to_string(),
                "/model".to_string(),
                "/model opus".to_string(),
                "/memory".to_string(),
                "/status".to_string(),
            ],
        )
    }

    fn press(editor: &mut LineEditor, code: KeyCode, modifiers: KeyModifiers) -> KeyAction {
        editor.handle_key(KeyEvent::new(code, modifiers))
    }

    fn type_text(editor: &mut LineEditor, text: &str) {
        for ch in text.chars() {
            press(editor, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    #[test]
    fn user_keybindings_override_builtin_keys() {
        use crate::keybindings::{Binding, Chord};
        let mut editor = editor();
        let chord = |text: &str| Chord::parse(text).expect("chord");
        editor.bindings = vec![
            (chord("ctrl+t"), Binding::Text("TODO: ".to_string())),
            (chord("ctrl+l"), Binding::Unbound),
            (chord("ctrl+g"), Binding::Action("submit")),
            (chord("ctrl+o"), Binding::Command("/focus".to_string())),
        ];
        press(&mut editor, KeyCode::Char('t'), KeyModifiers::CONTROL);
        type_text(&mut editor, "fix it");
        assert_eq!(editor.state.text(), "TODO: fix it");

        press(&mut editor, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(!editor.state.clear_screen, "ctrl+l was switched off");

        assert_eq!(
            press(&mut editor, KeyCode::Char('o'), KeyModifiers::CONTROL),
            KeyAction::Done(ReadOutcome::Submit("/focus".to_string()))
        );
        assert_eq!(
            editor.draft.as_deref(),
            Some("TODO: fix it"),
            "typed text comes back"
        );

        assert_eq!(
            press(&mut editor, KeyCode::Char('g'), KeyModifiers::CONTROL),
            KeyAction::Done(ReadOutcome::Submit("TODO: fix it".to_string()))
        );
    }

    #[test]
    fn ctrl_r_searches_history_newest_first() {
        let mut editor = editor();
        editor.history = vec![
            "emerge -s slock".to_string(),
            "git status".to_string(),
            "emerge -pv i3lock".to_string(),
        ];
        editor.state.history_index = editor.history.len();
        type_text(&mut editor, "draft");

        press(&mut editor, KeyCode::Char('r'), KeyModifiers::CONTROL);
        type_text(&mut editor, "EMERGE");
        assert_eq!(editor.state.text(), "emerge -pv i3lock");
        press(&mut editor, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(editor.state.text(), "emerge -s slock");

        // Esc gives back what was typed before the search.
        press(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(editor.state.text(), "draft");
        assert!(editor.state.search.is_none());

        press(&mut editor, KeyCode::Char('r'), KeyModifiers::CONTROL);
        type_text(&mut editor, "git");
        assert_eq!(
            press(&mut editor, KeyCode::Enter, KeyModifiers::NONE),
            KeyAction::Done(ReadOutcome::Submit("git status".to_string()))
        );
    }

    #[test]
    fn shift_tab_hands_back_the_draft() {
        let mut editor = editor();
        type_text(&mut editor, "half typed");
        assert_eq!(
            press(&mut editor, KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyAction::Done(ReadOutcome::CycleMode("half typed".to_string()))
        );
    }

    #[test]
    fn enter_submits_and_newline_keys_do_not() {
        let mut editor = editor();
        type_text(&mut editor, "one");
        press(&mut editor, KeyCode::Enter, KeyModifiers::SHIFT);
        type_text(&mut editor, "two\\");
        assert_eq!(
            press(&mut editor, KeyCode::Enter, KeyModifiers::NONE),
            KeyAction::Continue
        );
        press(&mut editor, KeyCode::Char('j'), KeyModifiers::CONTROL);
        type_text(&mut editor, "three");
        assert_eq!(
            press(&mut editor, KeyCode::Enter, KeyModifiers::NONE),
            KeyAction::Done(ReadOutcome::Submit("one\ntwo\n\nthree".to_string()))
        );
    }

    #[test]
    fn large_pastes_collapse_to_a_placeholder_and_expand_on_submit() {
        let mut state = EditState::default();
        state.insert_text("see: ");
        state.insert_text("a\nb\nc\n");
        assert_eq!(state.text(), "see: [Pasted text #1 +3 lines]");
        assert_eq!(state.expanded_text(), "see: a\nb\nc");
        state.pos -= 3;
        state.backspace();
        assert_eq!(
            state.text(),
            "see: ",
            "backspace inside the placeholder removes all of it"
        );
        assert_eq!(state.pos, 5);
    }

    #[test]
    fn slash_menu_filters_and_enter_runs_the_selection() {
        let mut editor = editor();
        type_text(&mut editor, "/m");
        let names = editor
            .state
            .menu
            .iter()
            .map(|&index| editor.commands[index].0.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["memory", "model"]);
        press(&mut editor, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            press(&mut editor, KeyCode::Enter, KeyModifiers::NONE),
            KeyAction::Done(ReadOutcome::Submit("/model".to_string()))
        );
    }

    #[test]
    fn esc_hides_the_menu_then_clears_the_input() {
        let mut editor = editor();
        type_text(&mut editor, "/st");
        assert!(!editor.state.menu.is_empty());
        press(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        assert!(editor.state.menu.is_empty());
        assert_eq!(editor.state.text(), "/st");
        press(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(editor.state.text(), "");
    }

    #[test]
    fn tab_completes_command_arguments_when_the_menu_is_closed() {
        let mut editor = editor();
        type_text(&mut editor, "/model o");
        press(&mut editor, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(editor.state.text(), "/model opus ");
    }

    #[test]
    fn up_and_down_walk_history_and_restore_the_draft() {
        let mut editor = editor();
        editor.push_history("first");
        editor.push_history("second");
        editor.state.history_index = editor.history.len();
        type_text(&mut editor, "draft");
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(editor.state.text(), "second");
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(editor.state.text(), "first");
        press(&mut editor, KeyCode::Down, KeyModifiers::NONE);
        press(&mut editor, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(editor.state.text(), "draft");
    }

    #[test]
    fn ctrl_c_clears_input_then_needs_a_second_press_to_exit() {
        let mut editor = editor();
        type_text(&mut editor, "abc");
        assert_eq!(
            press(&mut editor, KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyAction::Continue
        );
        assert_eq!(editor.state.text(), "");
        assert_eq!(
            press(&mut editor, KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyAction::Continue,
            "the first press on an empty input only arms the exit"
        );
        assert_eq!(
            press(&mut editor, KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyAction::Done(ReadOutcome::Exit)
        );
    }

    #[test]
    fn typing_cancels_a_pending_ctrl_c_exit() {
        let mut editor = editor();
        press(&mut editor, KeyCode::Char('c'), KeyModifiers::CONTROL);
        type_text(&mut editor, "x");
        press(&mut editor, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(editor.state.text(), "");
        assert_eq!(
            press(&mut editor, KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyAction::Continue
        );
        assert_eq!(
            press(&mut editor, KeyCode::Char('d'), KeyModifiers::CONTROL),
            KeyAction::Continue,
            "Ctrl-D does not confirm a Ctrl-C exit"
        );
    }

    #[test]
    fn highlights_markdown_in_the_input() {
        let buf = "/run `ls` **now** @src\n# Title\n```\ncode\n```"
            .chars()
            .collect::<Vec<_>>();
        let styles = highlight(&buf);
        let style_at = |needle: &str| {
            let text = buf.iter().collect::<String>();
            styles[text.find(needle).expect("needle")]
        };
        assert_eq!(style_at("/run"), Style::Command);
        assert_eq!(style_at("ls"), Style::Code);
        assert_eq!(style_at("now"), Style::Bold);
        assert_eq!(style_at("@src"), Style::Mention);
        assert_eq!(style_at("Title"), Style::Heading);
        assert_eq!(style_at("code"), Style::Code);
    }

    #[test]
    fn layout_wraps_wide_characters_and_continuation_lines() {
        let buf = "ab\ncd".chars().collect::<Vec<_>>();
        assert_eq!(layout("> ", &buf, 80), (1, 4));
        let wide = "界界界".chars().collect::<Vec<_>>();
        assert_eq!(layout("> ", &wide, 6), (1, 2));
    }

    #[test]
    fn common_prefix_of_candidates() {
        let items = vec!["/model".to_string(), "/model opus".to_string()];
        assert_eq!(common_prefix(&items), "/model");
    }
}
