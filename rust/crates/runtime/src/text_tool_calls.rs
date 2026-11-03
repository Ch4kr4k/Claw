//! Tool calls a local model wrote out as text instead of sending them as
//! native tool calls. llama.cpp, Ollama and vLLM each parse a model's
//! tool-call syntax with their own per-model parser; when that parser does
//! not match (a wrong chat template, a fine-tune with a different format, a
//! server started without `--jinja`), the call arrives as plain reply text
//! and the turn used to end there, the model's work silently dropped.
//!
//! Two formats are recognised, and only outside fenced code blocks:
//! - Hermes / Qwen: `<tool_call>{"name": …, "arguments": {…}}</tool_call>`
//! - Qwen3-Coder: `<function=name><parameter=key>value</parameter></function>`,
//!   bare or inside `<tool_call>`.
//!
//! A reply that already has a native tool call is left alone. Turned off
//! with `CLAW_TEXT_TOOL_CALLS=0`.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

use crate::session::{ContentBlock, ConversationMessage};

/// A tool call parsed out of reply text: the tool's name and its input JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextToolCall {
    pub name: String,
    pub input: String,
}

/// Whether text tool calls are recovered (`CLAW_TEXT_TOOL_CALLS`, default on).
#[must_use]
pub fn enabled() -> bool {
    std::env::var("CLAW_TEXT_TOOL_CALLS").map_or(true, |value| {
        !matches!(value.trim(), "0" | "false" | "off" | "no")
    })
}

/// Turns tool calls written as text in `message` into tool-use blocks and
/// removes their markup from the text. Returns how many were recovered; 0
/// (and `message` unchanged) when it already has a native tool call or
/// none is found.
pub fn recover(message: &mut ConversationMessage) -> usize {
    if message
        .blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    {
        return 0;
    }
    let mut calls = Vec::new();
    let mut blocks = Vec::with_capacity(message.blocks.len());
    for block in message.blocks.drain(..) {
        match block {
            ContentBlock::Text { text } => {
                let (rest, found) = extract(&text);
                if found.is_empty() {
                    blocks.push(ContentBlock::Text { text });
                    continue;
                }
                if !rest.trim().is_empty() {
                    blocks.push(ContentBlock::Text { text: rest });
                }
                calls.extend(found);
            }
            other => blocks.push(other),
        }
    }
    let count = calls.len();
    for call in calls {
        blocks.push(ContentBlock::ToolUse {
            id: next_call_id(),
            name: call.name,
            input: call.input,
        });
    }
    message.blocks = blocks;
    count
}

/// The text with recognised tool-call markup removed (trimmed), and the
/// calls in order. Markup that does not parse is left in the text.
#[must_use]
pub fn extract(text: &str) -> (String, Vec<TextToolCall>) {
    let fences = fenced_ranges(text);
    let in_fence = |at: usize| {
        fences
            .iter()
            .any(|(start, end)| (*start..*end).contains(&at))
    };
    let mut calls = Vec::new();
    let mut rest = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        let next = [
            ("<tool_call>", "</tool_call>"),
            ("<function=", "</function>"),
        ]
        .into_iter()
        .filter_map(|(open, close)| {
            let mut from = cursor;
            loop {
                let start = from + text[from..].find(open)?;
                if !in_fence(start) {
                    return Some((start, open, close));
                }
                from = start + open.len();
            }
        })
        .min_by_key(|(start, ..)| *start);
        let Some((start, open, close)) = next else {
            break;
        };
        let Some(close_at) = text[start..].find(close).map(|at| start + at) else {
            break;
        };
        let end = close_at + close.len();
        let parsed = if open == "<tool_call>" {
            parse_tool_call_body(&text[start + open.len()..close_at])
        } else {
            parse_function(&text[start..end])
        };
        rest.push_str(&text[cursor..start]);
        match parsed {
            Some(call) => calls.push(call),
            None => rest.push_str(&text[start..end]),
        }
        cursor = end;
    }
    rest.push_str(&text[cursor..]);
    if calls.is_empty() {
        return (text.to_string(), calls);
    }
    (rest.trim().to_string(), calls)
}

/// The inside of `<tool_call>…</tool_call>`: Hermes JSON or a Qwen3-Coder
/// `<function=…>` block.
fn parse_tool_call_body(body: &str) -> Option<TextToolCall> {
    let body = body.trim();
    if body.starts_with("<function=") {
        return parse_function(body);
    }
    let value: Value = serde_json::from_str(strip_code_fence(body)).ok()?;
    let object = value.as_object()?;
    // Some templates nest it: {"function": {"name": …, "arguments": …}}.
    let object = object
        .get("function")
        .and_then(Value::as_object)
        .unwrap_or(object);
    let name = object.get("name")?.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let arguments = object
        .get("arguments")
        .or_else(|| object.get("parameters"))
        .or_else(|| object.get("input"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    // Arguments are sometimes a JSON string holding the object.
    let arguments = match arguments {
        Value::String(raw) => serde_json::from_str::<Value>(&raw).ok()?,
        other => other,
    };
    arguments.is_object().then(|| TextToolCall {
        name: name.to_string(),
        input: arguments.to_string(),
    })
}

/// `<function=name><parameter=key>value</parameter>…</function>`.
fn parse_function(block: &str) -> Option<TextToolCall> {
    let after_open = block.strip_prefix("<function=")?;
    let name_end = after_open.find('>')?;
    let name = after_open[..name_end].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut body = &after_open[name_end + 1..];
    let mut arguments = Map::new();
    while let Some(start) = body.find("<parameter=") {
        let after = &body[start + "<parameter=".len()..];
        let key_end = after.find('>')?;
        let key = after[..key_end].trim().to_string();
        let value_and_rest = &after[key_end + 1..];
        let value_end = value_and_rest.find("</parameter>")?;
        let raw = trim_one_newline(&value_and_rest[..value_end]);
        arguments.insert(key, parameter_value(raw));
        body = &value_and_rest[value_end + "</parameter>".len()..];
    }
    Some(TextToolCall {
        name: name.to_string(),
        input: Value::Object(arguments).to_string(),
    })
}

/// A parameter's value: JSON when it is a number, boolean, null, array or
/// object, otherwise the text as a string (paths, commands, file content).
fn parameter_value(raw: &str) -> Value {
    let trimmed = raw.trim();
    let looks_json = matches!(trimmed.chars().next(), Some('[' | '{'))
        || matches!(trimmed, "true" | "false" | "null")
        || trimmed.parse::<f64>().is_ok();
    if looks_json {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            return value;
        }
    }
    Value::String(raw.to_string())
}

fn trim_one_newline(text: &str) -> &str {
    let text = text
        .strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))
        .unwrap_or(text);
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text)
}

/// JSON wrapped in a ```json fence inside the tool-call tags.
fn strip_code_fence(body: &str) -> &str {
    let Some(inner) = body.strip_prefix("```") else {
        return body;
    };
    let inner = inner.split_once('\n').map_or("", |(_, rest)| rest);
    inner.trim_end().strip_suffix("```").unwrap_or(inner).trim()
}

/// Byte ranges of fenced code blocks (``` or ~~~), an unclosed one running
/// to the end: markup shown there is an example, not a call.
fn fenced_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut open: Option<(usize, &str)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        match (open, marker) {
            (None, Some(marker)) => open = Some((offset, marker)),
            (Some((start, fence)), Some(marker)) if marker == fence => {
                ranges.push((start, offset + line.len()));
                open = None;
            }
            _ => {}
        }
        offset += line.len();
    }
    if let Some((start, _)) = open {
        ranges.push((start, text.len()));
    }
    ranges
}

fn next_call_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    format!(
        "call_text_{millis}_{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(call: &TextToolCall) -> Value {
        serde_json::from_str(&call.input).unwrap()
    }

    #[test]
    fn hermes_json_call_is_recovered_and_removed_from_text() {
        let text = "Let me look.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"src/main.rs\"}}\n</tool_call>";
        let (rest, calls) = extract(text);
        assert_eq!(rest, "Let me look.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(input(&calls[0])["path"], "src/main.rs");
    }

    #[test]
    fn string_arguments_and_nested_function_object_are_accepted() {
        let text = r#"<tool_call>{"function": {"name": "bash", "arguments": "{\"command\": \"ls\"}"}}</tool_call>"#;
        let (rest, calls) = extract(text);
        assert!(rest.is_empty());
        assert_eq!(calls[0].name, "bash");
        assert_eq!(input(&calls[0])["command"], "ls");
    }

    #[test]
    fn qwen_coder_function_call_with_typed_and_multiline_parameters() {
        let text = "<tool_call>\n<function=write_file>\n<parameter=path>\na.txt\n</parameter>\n<parameter=content>\nline 1\n  line 2\n</parameter>\n<parameter=limit>\n5\n</parameter>\n</function>\n</tool_call>";
        let (_, calls) = extract(text);
        assert_eq!(calls.len(), 1);
        let args = input(&calls[0]);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(args["path"], "a.txt");
        assert_eq!(args["content"], "line 1\n  line 2");
        assert_eq!(args["limit"], 5);
    }

    #[test]
    fn bare_function_blocks_and_several_calls_keep_their_order() {
        let text = "<function=glob_search><parameter=pattern>*.rs</parameter></function> then <tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"pwd\"}}</tool_call>";
        let (rest, calls) = extract(text);
        assert_eq!(rest, "then");
        let names: Vec<_> = calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(names, ["glob_search", "bash"]);
    }

    #[test]
    fn markup_inside_a_code_fence_is_an_example_not_a_call() {
        let text = "Use this format:\n```\n<tool_call>{\"name\":\"bash\",\"arguments\":{}}</tool_call>\n```\n";
        let (rest, calls) = extract(text);
        assert!(calls.is_empty());
        assert_eq!(rest, text);
    }

    #[test]
    fn unparseable_or_unclosed_markup_is_left_as_text() {
        for text in [
            "<tool_call>not json</tool_call>",
            "<tool_call>{\"name\": \"bash\", \"arguments\": {\"command\": \"ls\"",
            "<tool_call>{\"arguments\": {}}</tool_call>",
        ] {
            let (rest, calls) = extract(text);
            assert!(calls.is_empty(), "{text}");
            assert_eq!(rest, text);
        }
    }

    #[test]
    fn recover_turns_text_into_tool_use_blocks_unless_a_native_call_exists() {
        let mut message = ConversationMessage::assistant(vec![
            ContentBlock::Thinking {
                thinking: "plan".to_string(),
                signature: None,
            },
            ContentBlock::Text {
                text:
                    "<tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}</tool_call>"
                        .to_string(),
            },
        ]);
        assert_eq!(recover(&mut message), 1);
        assert!(matches!(message.blocks[0], ContentBlock::Thinking { .. }));
        assert!(matches!(
            &message.blocks[1],
            ContentBlock::ToolUse { name, .. } if name == "bash"
        ));
        assert_eq!(message.blocks.len(), 2);

        let mut native = ConversationMessage::assistant(vec![
            ContentBlock::Text {
                text: "<tool_call>{\"name\":\"bash\",\"arguments\":{}}</tool_call>".to_string(),
            },
            ContentBlock::ToolUse {
                id: "1".to_string(),
                name: "read_file".to_string(),
                input: "{}".to_string(),
            },
        ]);
        assert_eq!(recover(&mut native), 0);
        assert_eq!(native.blocks.len(), 2);
    }
}
