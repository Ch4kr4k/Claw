//! Diffs drawn like Claude Code's: line numbers, removed lines on red and
//! added lines on green across the whole row, syntax colours, and long
//! lines wrapped with their `+`/`-` marker. Used for edits in the
//! conversation and by the diff panel.

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthChar;

use crate::render::{current_theme, ThemeName};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[38;5;245m";

/// A hunk of a unified diff: where it starts in the old and new file
/// (1-based) and its lines, each starting with `' '`, `'-'` or `'+'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<String>,
}

impl Hunk {
    /// Hunks from a tool's `structuredPatch` JSON.
    pub(crate) fn from_structured_patch(patch: &serde_json::Value) -> Vec<Self> {
        let number = |hunk: &serde_json::Value, key: &str| {
            hunk.get(key)
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(1)
        };
        patch
            .as_array()
            .map(|hunks| {
                hunks
                    .iter()
                    .map(|hunk| Self {
                        old_start: number(hunk, "oldStart"),
                        new_start: number(hunk, "newStart"),
                        lines: hunk
                            .get("lines")
                            .and_then(serde_json::Value::as_array)
                            .map(|lines| {
                                lines
                                    .iter()
                                    .filter_map(serde_json::Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Lines added and removed in `hunks`.
pub(crate) fn line_counts(hunks: &[Hunk]) -> (usize, usize) {
    hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .fold((0, 0), |(added, removed), line| match line.chars().next() {
            Some('+') => (added + 1, removed),
            Some('-') => (added, removed + 1),
            _ => (added, removed),
        })
}

/// `Added 3 lines, removed 1 line`, as Claude Code words it.
pub(crate) fn change_summary(added: usize, removed: usize) -> String {
    let lines = |count: usize| {
        if count == 1 {
            "1 line".to_string()
        } else {
            format!("{count} lines")
        }
    };
    match (added, removed) {
        (0, 0) => "No changes".to_string(),
        (added, 0) => format!("Added {}", lines(added)),
        (0, removed) => format!("Removed {}", lines(removed)),
        (added, removed) => format!("Added {}, removed {}", lines(added), lines(removed)),
    }
}

/// How a diff is drawn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout<'a> {
    /// Width of each row, in columns, including `indent`.
    pub width: usize,
    /// Columns left blank before the line numbers.
    pub indent: usize,
    /// The file, for its syntax colours (by extension).
    pub path: &'a str,
    /// Rows drawn at most; the rest is counted in a last dim row.
    pub max_rows: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Context,
    Added,
    Removed,
}

/// The diff as terminal rows, each exactly `layout.width` columns wide.
pub(crate) fn render(hunks: &[Hunk], layout: Layout<'_>) -> Vec<String> {
    let colors = Colors::current();
    let number_width = hunks
        .iter()
        .map(|hunk| {
            let (added, removed) = line_counts(std::slice::from_ref(hunk));
            (hunk.old_start + removed).max(hunk.new_start + added)
        })
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let indent = " ".repeat(layout.indent);
    let mut highlighter = Highlighter::for_path(layout.path);
    let mut rows = Vec::new();
    // Rows past the limit are only counted, not drawn.
    let limit = layout.max_rows.unwrap_or(usize::MAX);
    let mut hidden = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        if index > 0 {
            rows.push(pad(
                &format!("{indent}{DIM}{:>number_width$}{RESET}", "⋮"),
                layout.width,
            ));
        }
        let (mut old_number, mut new_number) = (hunk.old_start, hunk.new_start);
        for line in &hunk.lines {
            let (kind, text) = match line.chars().next() {
                Some('+') => (Kind::Added, &line[1..]),
                Some('-') => (Kind::Removed, &line[1..]),
                Some(' ') => (Kind::Context, &line[1..]),
                _ => (Kind::Context, line.as_str()),
            };
            let number = match kind {
                Kind::Removed => {
                    old_number += 1;
                    old_number - 1
                }
                Kind::Added => {
                    new_number += 1;
                    new_number - 1
                }
                Kind::Context => {
                    old_number += 1;
                    new_number += 1;
                    new_number - 1
                }
            };
            if rows.len() >= limit {
                hidden += 1;
                continue;
            }
            let cells = highlighter.cells(&expand_tabs(text));
            rows.extend(line_rows(
                kind,
                number,
                &cells,
                &indent,
                number_width,
                layout.width,
                &colors,
            ));
        }
    }
    if rows.len() > limit || hidden > 0 {
        let cut = rows.len().saturating_sub(limit.saturating_sub(1));
        rows.truncate(limit.saturating_sub(1));
        rows.push(pad(
            &format!("{indent}{DIM}… +{} more lines{RESET}", hidden + cut),
            layout.width,
        ));
    }
    rows
}

/// One diff line as one or more rows: the number and marker, then the code;
/// what does not fit continues on rows that start with the marker.
fn line_rows(
    kind: Kind,
    number: usize,
    cells: &[(Option<[u8; 3]>, char)],
    indent: &str,
    number_width: usize,
    width: usize,
    colors: &Colors,
) -> Vec<String> {
    let (marker, background, marker_color) = match kind {
        Kind::Added => ('+', colors.added_background, colors.added_marker),
        Kind::Removed => ('-', colors.removed_background, colors.removed_marker),
        Kind::Context => (' ', None, None),
    };
    let start_row = |prefix: String| {
        let mut row = String::new();
        if let Some(background) = background {
            row.push_str(background);
        }
        row.push_str(&prefix);
        row
    };
    let number_style = if kind == Kind::Context { DIM } else { "" };
    let marker_text = |marker: char| match marker_color {
        Some(color) => format!("{color}{marker}{}", colors.default_foreground),
        None => marker.to_string(),
    };
    let first_prefix_width = indent.len() + number_width + 2;
    let mut rows = Vec::new();
    let mut row = start_row(format!(
        "{indent}{number_style}{number:>number_width$}{} {}",
        if number_style.is_empty() {
            ""
        } else {
            colors.default_foreground
        },
        marker_text(marker)
    ));
    let mut used = first_prefix_width;
    let mut current: Option<[u8; 3]> = None;
    for &(color, ch) in cells {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > width && used > indent.len() + 1 {
            rows.push(finish_row(row, used, width));
            row = start_row(format!("{indent}{}", marker_text(marker)));
            used = indent.len() + 1;
            current = None;
        }
        if color != current {
            match color {
                Some([r, g, b]) => row.push_str(&format!("\x1b[38;2;{r};{g};{b}m")),
                None => row.push_str(colors.default_foreground),
            }
            current = color;
        }
        row.push(ch);
        used += ch_width;
    }
    rows.push(finish_row(row, used, width));
    rows
}

/// Pads a row out to `width` (keeping its background) and resets.
fn finish_row(mut row: String, used: usize, width: usize) -> String {
    row.push_str(&" ".repeat(width.saturating_sub(used)));
    row.push_str(RESET);
    row
}

/// `text` (already styled) padded with spaces to `width` columns.
fn pad(text: &str, width: usize) -> String {
    let shown = visible_width(text);
    format!("{text}{}{RESET}", " ".repeat(width.saturating_sub(shown)))
}

/// Columns `text` takes on screen, escape sequences left out.
pub(crate) fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += ch.width().unwrap_or(0);
    }
    width
}

fn expand_tabs(text: &str) -> String {
    text.replace('\t', "    ")
}

/// Row colours for the current theme.
struct Colors {
    added_background: Option<&'static str>,
    removed_background: Option<&'static str>,
    added_marker: Option<&'static str>,
    removed_marker: Option<&'static str>,
    /// Foreground for text the highlighter leaves uncoloured.
    default_foreground: &'static str,
    highlight: bool,
}

impl Colors {
    fn current() -> Self {
        match current_theme() {
            ThemeName::None => Self {
                added_background: None,
                removed_background: None,
                added_marker: None,
                removed_marker: None,
                default_foreground: "",
                highlight: false,
            },
            ThemeName::Light | ThemeName::SolarizedLight => Self {
                added_background: Some("\x1b[48;2;214;245;214m"),
                removed_background: Some("\x1b[48;2;250;218;218m"),
                added_marker: Some("\x1b[38;2;30;120;30m"),
                removed_marker: Some("\x1b[38;2;170;30;30m"),
                default_foreground: "\x1b[39m",
                highlight: true,
            },
            ThemeName::Dark | ThemeName::Solarized | ThemeName::TokyoNight => Self {
                added_background: Some("\x1b[48;2;2;40;0m"),
                removed_background: Some("\x1b[48;2;61;1;0m"),
                added_marker: Some("\x1b[38;2;80;200;80m"),
                removed_marker: Some("\x1b[38;2;220;90;90m"),
                default_foreground: "\x1b[39m",
                highlight: true,
            },
        }
    }
}

fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// Colours code line by line, keeping state across the lines of one file.
struct Highlighter {
    inner: Option<HighlightLines<'static>>,
}

impl Highlighter {
    fn for_path(path: &str) -> Self {
        if !Colors::current().highlight {
            return Self { inner: None };
        }
        let syntaxes = syntax_set();
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("");
        let file_name = std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let syntax = syntaxes
            .find_syntax_by_extension(extension)
            .or_else(|| syntaxes.find_syntax_by_extension(file_name));
        let theme = theme_set()
            .themes
            .get(current_theme().syntax_theme_name())
            .or_else(|| theme_set().themes.get("base16-ocean.dark"));
        let inner = syntax
            .zip(theme)
            .map(|(syntax, theme)| HighlightLines::new(syntax, theme));
        Self { inner }
    }

    /// The characters of `text` with their foreground colour (`None`: the
    /// terminal's).
    fn cells(&mut self, text: &str) -> Vec<(Option<[u8; 3]>, char)> {
        let Some(inner) = self.inner.as_mut() else {
            return text.chars().map(|ch| (None, ch)).collect();
        };
        let line = format!("{text}\n");
        match inner.highlight_line(&line, syntax_set()) {
            Ok(ranges) => ranges
                .into_iter()
                .flat_map(|(style, piece)| {
                    let color = [style.foreground.r, style.foreground.g, style.foreground.b];
                    piece
                        .chars()
                        .filter(|ch| *ch != '\n')
                        .map(move |ch| (Some(color), ch))
                })
                .collect(),
            Err(_) => text.chars().map(|ch| (None, ch)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{change_summary, line_counts, render, visible_width, Hunk, Layout};

    fn hunk() -> Hunk {
        Hunk {
            old_start: 9,
            new_start: 9,
            lines: vec![
                " keep".to_string(),
                "-old line".to_string(),
                "+new line".to_string(),
                "+another".to_string(),
                " tail".to_string(),
            ],
        }
    }

    fn plain(row: &str) -> String {
        let mut out = String::new();
        let mut chars = row.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    #[test]
    fn summary_is_worded_like_claude_code() {
        assert_eq!(change_summary(27, 0), "Added 27 lines");
        assert_eq!(change_summary(10, 7), "Added 10 lines, removed 7 lines");
        assert_eq!(change_summary(0, 1), "Removed 1 line");
        assert_eq!(line_counts(&[hunk()]), (2, 1));
    }

    #[test]
    fn rows_are_numbered_marked_and_full_width() {
        let rows = render(
            &[hunk()],
            Layout {
                width: 30,
                indent: 2,
                path: "a.txt",
                max_rows: None,
            },
        );
        let text: Vec<String> = rows.iter().map(|row| plain(row)).collect();
        assert_eq!(text[0].trim_end(), "   9  keep");
        assert_eq!(text[1].trim_end(), "  10 -old line");
        assert_eq!(text[2].trim_end(), "  10 +new line");
        assert_eq!(text[3].trim_end(), "  11 +another");
        assert_eq!(text[4].trim_end(), "  12  tail");
        assert!(rows.iter().all(|row| visible_width(row) == 30));
    }

    #[test]
    fn long_lines_wrap_with_their_marker() {
        let long = Hunk {
            old_start: 1,
            new_start: 1,
            lines: vec![format!("+{}", "x".repeat(30))],
        };
        let rows = render(
            &[long],
            Layout {
                width: 20,
                indent: 0,
                path: "a.txt",
                max_rows: None,
            },
        );
        let text: Vec<String> = rows.iter().map(|row| plain(row)).collect();
        // 17 characters after "1 +", the other 13 after the "+" that
        // starts the continuation row.
        assert_eq!(text.len(), 2);
        assert_eq!(text[0], format!("1 +{}", "x".repeat(17)));
        assert_eq!(text[1].trim_end(), format!("+{}", "x".repeat(13)));
        assert!(rows.iter().all(|row| visible_width(row) == 20));
    }

    #[test]
    fn max_rows_counts_what_is_left_out() {
        let rows = render(
            &[hunk()],
            Layout {
                width: 30,
                indent: 0,
                path: "a.txt",
                max_rows: Some(3),
            },
        );
        assert_eq!(rows.len(), 3);
        assert!(plain(&rows[2]).contains("+3 more lines"));
    }
}
