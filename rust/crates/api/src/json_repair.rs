//! Repairs the small JSON mistakes models make in tool-call arguments, so
//! the call runs instead of failing with "invalid tool input JSON" and being
//! made again: a Markdown code fence around it, trailing commas, Python's
//! `True`/`False`/`None`, single-quoted strings, raw newlines or tabs inside
//! strings, and backslashes that start no valid escape (`\d` in a regex).
//!
//! Unbalanced input (an unclosed string, object or array) is never
//! completed: that is a call cut off at the output limit, and running half
//! of a `write_file` would write half a file.

use serde_json::Value;

/// `text` parsed as JSON after the repairs above; `None` when it still does
/// not parse (or needed none and is not JSON).
#[must_use]
pub fn repair_json(text: &str) -> Option<Value> {
    let trimmed = strip_fence(text.trim());
    let start = trimmed.find(['{', '['])?;
    let candidate = &trimmed[start..];
    let repaired = rewrite(candidate)?;
    serde_json::from_str(&repaired).ok()
}

fn strip_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.split_once('\n').map_or("", |(_, body)| body);
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
}

/// One pass over `text`, rewriting as it goes. `None` when brackets or
/// strings do not balance.
fn rewrite(text: &str) -> Option<String> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut out = String::with_capacity(text.len() + 16);
    let mut stack = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                i = read_string(&chars, i, &mut out)?;
                continue;
            }
            '{' | '[' => stack.push(c),
            '}' | ']' => {
                let open = stack.pop()?;
                if (open == '{') != (c == '}') {
                    return None;
                }
                // A comma right before a closing bracket is dropped.
                let kept = out.trim_end().len();
                if out[..kept].ends_with(',') {
                    out.truncate(kept - 1);
                }
            }
            _ if c.is_ascii_alphabetic() => {
                let end = (i..chars.len())
                    .find(|&j| !chars[j].is_ascii_alphanumeric() && chars[j] != '_')
                    .unwrap_or(chars.len());
                let word = chars[i..end].iter().collect::<String>();
                out.push_str(match word.as_str() {
                    "True" => "true",
                    "False" => "false",
                    "None" => "null",
                    other => other,
                });
                i = end;
                continue;
            }
            _ => {}
        }
        out.push(c);
        i += 1;
        if stack.is_empty() && matches!(c, '}' | ']') {
            // Anything after the top-level value (a note the model added)
            // is dropped.
            return Some(out);
        }
    }
    None
}

/// Copies the string starting at `chars[start]` (a `"` or `'`) to `out` as
/// a valid JSON string; returns the index after it, or `None` when it does
/// not end.
fn read_string(chars: &[char], start: usize, out: &mut String) -> Option<usize> {
    let quote = chars[start];
    out.push('"');
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == quote {
            out.push('"');
            return Some(i + 1);
        }
        match c {
            '\\' => {
                let next = *chars.get(i + 1)?;
                match next {
                    '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => {
                        out.push('\\');
                        out.push(next);
                    }
                    'u' if chars.len() > i + 5
                        && chars[i + 2..i + 6].iter().all(char::is_ascii_hexdigit) =>
                    {
                        out.push('\\');
                        out.push('u');
                    }
                    // `\'` in a single-quoted string is a quote.
                    '\'' => out.push('\''),
                    // Any other escape keeps its backslash as a character.
                    _ => {
                        out.push_str("\\\\");
                        i += 1;
                        continue;
                    }
                }
                i += 2;
                continue;
            }
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::repair_json;
    use serde_json::json;

    #[test]
    fn common_model_mistakes_are_repaired() {
        assert_eq!(
            repair_json("```json\n{\"path\": \"a.rs\", \"limit\": 5,}\n```"),
            Some(json!({"path": "a.rs", "limit": 5}))
        );
        assert_eq!(
            repair_json("{'pattern': 'fn \\d+', 'literal': True, 'glob': None}"),
            Some(json!({"pattern": "fn \\d+", "literal": true, "glob": null}))
        );
        assert_eq!(
            repair_json("{\"content\": \"line 1\nline\t2\"} trailing note"),
            Some(json!({"content": "line 1\nline\t2"}))
        );
        assert_eq!(
            repair_json("{\"items\": [1, 2, ], \"q\": \"say \\\"hi\\\"\"}"),
            Some(json!({"items": [1, 2], "q": "say \"hi\""}))
        );
    }

    #[test]
    fn a_cut_off_call_is_not_completed() {
        assert_eq!(
            repair_json("{\"path\": \"a.rs\", \"content\": \"fn main"),
            None
        );
        assert_eq!(repair_json("{\"items\": [1, 2"), None);
        assert_eq!(repair_json("not json"), None);
    }
}
