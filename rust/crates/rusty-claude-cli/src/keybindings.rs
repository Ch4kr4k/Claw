//! User keybindings for the input box: `~/.claw/keybindings.json` maps key
//! chords to editor actions, slash commands or text, ahead of the built-in
//! keys. `/keybindings` creates the file and lists what is bound.
//!
//! ```json
//! { "bindings": { "ctrl+o": "/focus", "ctrl+g": "submit", "ctrl+t": "text:TODO: " } }
//! ```

use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a bound key does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Binding {
    /// A built-in editor action (see [`ACTIONS`]).
    Action(&'static str),
    /// Submits this slash command (`"/compact"`), keeping the typed text.
    Command(String),
    /// Inserts text at the cursor (`"text:…"`).
    Text(String),
    /// Does nothing: switches a built-in key off (`"none"`).
    Unbound,
}

/// The editor actions a key can be bound to, with what they do.
pub(crate) const ACTIONS: &[(&str, &str)] = &[
    ("submit", "send the prompt"),
    ("newline", "insert a newline"),
    ("clear-input", "empty the input box"),
    ("cancel", "like Esc: close the menu or empty the input"),
    ("history-prev", "previous prompt from history"),
    ("history-next", "next prompt from history"),
    ("history-search", "search history (like Ctrl+R)"),
    ("line-start", "cursor to the start of the line"),
    ("line-end", "cursor to the end of the line"),
    ("word-left", "cursor one word left"),
    ("word-right", "cursor one word right"),
    ("delete-word", "delete the word before the cursor"),
    ("delete-to-start", "delete to the start of the line"),
    ("delete-to-end", "delete to the end of the line"),
    ("clear-screen", "clear the conversation view"),
    ("cycle-mode", "next permission mode (like Shift+Tab)"),
    ("complete", "complete a path or command (like Tab)"),
];

/// A key chord: the key and its modifiers (Shift only for non-character
/// keys; for characters the case carries it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Chord {
    /// `ctrl+o`, `alt+enter`, `ctrl+shift+up`, `f5`, `pageup`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_ascii_lowercase();
        let mut modifiers = KeyModifiers::NONE;
        let mut key = None;
        for part in text.split('+') {
            match part {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                other => {
                    if key.is_some() {
                        return None;
                    }
                    key = Some(parse_key(other)?);
                }
            }
        }
        let code = key?;
        if matches!(code, KeyCode::Char(_)) {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        Some(Self { code, modifiers })
    }

    pub(crate) fn of(event: &KeyEvent) -> Self {
        let mut modifiers =
            event.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let code = match event.code {
            KeyCode::Char(ch) => {
                modifiers.remove(KeyModifiers::SHIFT);
                KeyCode::Char(ch.to_ascii_lowercase())
            }
            // Shift+Tab arrives as BackTab.
            KeyCode::BackTab => {
                modifiers |= KeyModifiers::SHIFT;
                KeyCode::Tab
            }
            other => other,
        };
        Self { code, modifiers }
    }
}

fn parse_key(name: &str) -> Option<KeyCode> {
    Some(match name {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        name if name.len() > 1 && name.starts_with('f') => KeyCode::F(name[1..].parse().ok()?),
        name => {
            let mut chars = name.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(ch)
        }
    })
}

fn parse_binding(value: &str) -> Option<Binding> {
    // Text is inserted as written, spaces included.
    if let Some(text) = value.trim_start().strip_prefix("text:") {
        return Some(Binding::Text(text.replace("\\n", "\n")));
    }
    let value = value.trim();
    if value == "none" || value.is_empty() {
        return Some(Binding::Unbound);
    }
    if value.starts_with('/') {
        return Some(Binding::Command(value.to_string()));
    }
    ACTIONS
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(name, _)| Binding::Action(name))
}

/// The keybindings file.
pub(crate) fn path() -> PathBuf {
    runtime::default_config_home().join("keybindings.json")
}

/// The bindings in the file, and the entries that could not be read.
pub(crate) fn load() -> (Vec<(Chord, Binding)>, Vec<String>) {
    let Ok(text) = fs::read_to_string(path()) else {
        return (Vec::new(), Vec::new());
    };
    parse(&text)
}

fn parse(text: &str) -> (Vec<(Chord, Binding)>, Vec<String>) {
    let mut bindings = Vec::new();
    let mut problems = Vec::new();
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return (bindings, vec![format!("not valid JSON: {error}")]),
    };
    let Some(entries) = value.get("bindings").and_then(serde_json::Value::as_object) else {
        return (bindings, problems);
    };
    for (key, action) in entries {
        let Some(chord) = Chord::parse(key) else {
            problems.push(format!("unknown key `{key}`"));
            continue;
        };
        match action.as_str().and_then(parse_binding) {
            Some(binding) => bindings.push((chord, binding)),
            None => problems.push(format!("unknown action for `{key}`: {action}")),
        }
    }
    (bindings, problems)
}

const TEMPLATE: &str = r#"{
  "bindings": {
    "ctrl+o": "/focus"
  }
}
"#;

/// `/keybindings`: creates the file if needed and shows what is bound.
pub(crate) fn report() -> std::io::Result<String> {
    let file = path();
    let mut lines = Vec::new();
    if file.exists() {
        lines.push(format!("Keybindings: {}", file.display()));
    } else {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file, TEMPLATE)?;
        lines.push(format!(
            "Created {} (Ctrl+O toggles the focus view).",
            file.display()
        ));
    }
    let text = fs::read_to_string(&file).unwrap_or_default();
    let (bindings, problems) = parse(&text);
    let raw: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    if let Some(entries) = raw.get("bindings").and_then(serde_json::Value::as_object) {
        for (key, action) in entries {
            lines.push(format!("  {key:<16} {}", action.as_str().unwrap_or("?")));
        }
    }
    if bindings.is_empty() && problems.is_empty() {
        lines.push("  (nothing bound)".to_string());
    }
    for problem in problems {
        lines.push(format!("  ! {problem}"));
    }
    lines.push(String::new());
    lines.push(
        "Edit the file; changes apply at the next prompt. A key maps to an action, a".to_string(),
    );
    lines.push(
        "slash command (\"/compact\"), text to insert (\"text:…\"), or \"none\" to switch a"
            .to_string(),
    );
    lines.push(
        "built-in key off. Keys: ctrl+x, alt+x, shift+up, f5, pageup, enter, tab, esc.".to_string(),
    );
    lines.push("Actions:".to_string());
    for (name, what) in ACTIONS {
        lines.push(format!("  {name:<16} {what}"));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{parse, Binding, Chord};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn chords_parse_and_match_events() {
        let ctrl_o = Chord::parse("ctrl+o").expect("chord");
        assert_eq!(
            ctrl_o,
            Chord::of(&KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            Chord::parse("Ctrl+Shift+O"),
            Some(ctrl_o),
            "shift is carried by the character"
        );
        assert_eq!(
            Chord::parse("shift+tab"),
            Some(Chord::of(&KeyEvent::new(
                KeyCode::BackTab,
                KeyModifiers::SHIFT
            )))
        );
        assert_eq!(
            Chord::parse("alt+enter"),
            Some(Chord::of(&KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)))
        );
        assert!(Chord::parse("f5").is_some());
        assert!(Chord::parse("ctrl+").is_none());
        assert!(Chord::parse("ctrl+xy").is_none());
        assert!(Chord::parse("ctrl+a+b").is_none());
    }

    #[test]
    fn bindings_file_maps_actions_commands_and_text() {
        let (bindings, problems) = parse(
            r#"{"bindings": {
                "ctrl+g": "submit",
                "ctrl+o": "/focus",
                "ctrl+t": "text:TODO: ",
                "ctrl+l": "none",
                "ctrl+q": "explode",
                "hyper+q": "submit"
            }}"#,
        );
        let find = |key: &str| {
            let chord = Chord::parse(key).expect("chord");
            bindings
                .iter()
                .find(|(c, _)| *c == chord)
                .map(|(_, b)| b.clone())
        };
        assert_eq!(find("ctrl+g"), Some(Binding::Action("submit")));
        assert_eq!(find("ctrl+o"), Some(Binding::Command("/focus".to_string())));
        assert_eq!(find("ctrl+t"), Some(Binding::Text("TODO: ".to_string())));
        assert_eq!(find("ctrl+l"), Some(Binding::Unbound));
        assert_eq!(problems.len(), 2, "{problems:?}");
    }
}
