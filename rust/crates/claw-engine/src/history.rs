//! claw's session messages ↔ Infinity's history entries.
//!
//! claw's [`Session`](runtime::Session) stays the record of a conversation:
//! checkpoints, `/undo`, `--resume`, compaction and learning all work on it.
//! A REPL turn on the engine seeds the root thread with the session's
//! messages ([`to_infinity`]) and hands the entries the turn added back
//! ([`from_infinity`]).

use std::collections::HashMap;

use infinity_agent_core::message::InfinityMessage;
use infinity_provider_protocol::{
    AssistantContent, Image, ImageMediaType, ImageSource, Reasoning, ReasoningContent, Text,
    ToolCall, ToolResult, ToolResultContent, UserContent,
};
use runtime::{ContentBlock, ConversationMessage, MessageRole};

use crate::batch;

/// `messages` as Infinity history entries, in order.
#[must_use]
pub fn to_infinity(messages: &[ConversationMessage]) -> Vec<InfinityMessage> {
    let mut entries = Vec::new();
    for message in messages {
        for block in &message.blocks {
            entries.extend(block_to_infinity(message.role, block));
        }
    }
    entries
}

fn block_to_infinity(role: MessageRole, block: &ContentBlock) -> Option<InfinityMessage> {
    match block {
        ContentBlock::Text { text } if text.is_empty() => None,
        ContentBlock::Text { text } => Some(match role {
            MessageRole::Assistant => InfinityMessage::Assistant {
                content: AssistantContent::Text(Text { text: text.clone() }),
            },
            MessageRole::User | MessageRole::System | MessageRole::Tool => InfinityMessage::User {
                content: UserContent::Text(Text { text: text.clone() }),
            },
        }),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => Some(InfinityMessage::Assistant {
            content: AssistantContent::Reasoning(Reasoning::new_with_signature(
                thinking.clone(),
                signature.clone(),
            )),
        }),
        ContentBlock::ToolUse { id, name, input } => Some(InfinityMessage::ToolCall {
            call: ToolCall::new(
                id.clone(),
                name.clone(),
                serde_json::from_str(input).unwrap_or_else(|_| serde_json::json!({})),
            ),
            display_as: None,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            output,
            is_error,
            ..
        } => {
            // Infinity results carry no error flag; the provider adapter
            // reads one from the "Error:" prefix.
            let text = if *is_error && !output.starts_with("Error:") {
                format!("Error: {output}")
            } else {
                output.clone()
            };
            Some(InfinityMessage::ToolResult {
                result: ToolResult {
                    id: tool_use_id.clone(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(Text { text })],
                },
                display_segments: None,
            })
        }
        ContentBlock::Image {
            media_type, data, ..
        } => Some(InfinityMessage::User {
            content: UserContent::Image(Image {
                data: ImageSource::Base64(data.clone()),
                media_type: ImageMediaType::from_mime_type(media_type),
            }),
        }),
    }
}

/// claw session messages for Infinity history `entries`: consecutive
/// assistant entries form one assistant message, each tool result is a tool
/// message, a batch call is unpacked into its calls and results, and a
/// child thread's report becomes a user note.
#[must_use]
pub fn from_infinity(entries: &[InfinityMessage]) -> Vec<ConversationMessage> {
    let mut messages: Vec<ConversationMessage> = Vec::new();
    // Tool names by call id, for the results' `tool_name`.
    let mut names = HashMap::<String, String>::new();
    // Inner call ids of batch calls, by batch id.
    let mut batches = HashMap::<String, Vec<String>>::new();
    let push_assistant =
        |messages: &mut Vec<ConversationMessage>, block: ContentBlock| match messages.last_mut() {
            Some(last) if last.role == MessageRole::Assistant => last.blocks.push(block),
            _ => messages.push(ConversationMessage::assistant(vec![block])),
        };
    for entry in entries {
        match entry {
            InfinityMessage::User { content } => match content {
                UserContent::Text(text) => {
                    messages.push(ConversationMessage::user_text(&text.text))
                }
                UserContent::Image(_) => {
                    messages.push(ConversationMessage::user_text("[image]"));
                }
                UserContent::ToolResult(result) => {
                    push_results(&mut messages, result, &names, &batches);
                }
            },
            InfinityMessage::Assistant { content } => match content {
                AssistantContent::Text(text) if text.text.is_empty() => {}
                AssistantContent::Text(text) => push_assistant(
                    &mut messages,
                    ContentBlock::Text {
                        text: text.text.clone(),
                    },
                ),
                AssistantContent::Reasoning(reasoning) => {
                    if let Some(block) = thinking_block(reasoning) {
                        push_assistant(&mut messages, block);
                    }
                }
                AssistantContent::ToolCall(call) => {
                    for block in call_blocks(call, &mut names, &mut batches) {
                        push_assistant(&mut messages, block);
                    }
                }
            },
            InfinityMessage::ToolCall { call, .. } => {
                for block in call_blocks(call, &mut names, &mut batches) {
                    push_assistant(&mut messages, block);
                }
            }
            InfinityMessage::ToolResult { result, .. } => {
                push_results(&mut messages, result, &names, &batches);
            }
            InfinityMessage::SubscriptionEvent {
                result,
                child_thread_id,
                ..
            } => {
                let text = result_text(result);
                let from = child_thread_id.as_ref().map_or_else(
                    || "an event".to_string(),
                    |id| format!("child thread {}", id.as_str()),
                );
                messages.push(ConversationMessage::user_text(format!(
                    "[Report from {from}]\n{text}"
                )));
            }
        }
    }
    messages
}

fn thinking_block(reasoning: &Reasoning) -> Option<ContentBlock> {
    let text = reasoning.content.iter().find_map(|content| match content {
        ReasoningContent::Text { text, .. } => Some(text.clone()),
        ReasoningContent::Summary(summary) => Some(summary.clone()),
        ReasoningContent::Redacted { .. } | ReasoningContent::Encrypted(_) => None,
    })?;
    Some(ContentBlock::Thinking {
        thinking: text,
        signature: reasoning.first_signature().map(ToOwned::to_owned),
    })
}

fn call_blocks(
    call: &ToolCall,
    names: &mut HashMap<String, String>,
    batches: &mut HashMap<String, Vec<String>>,
) -> Vec<ContentBlock> {
    if call.function.name == batch::BATCH_TOOL {
        let inner = batch::unpack_calls(&call.function.arguments);
        batches.insert(
            call.id.clone(),
            inner.iter().map(|inner| inner.id.clone()).collect(),
        );
        return inner
            .into_iter()
            .map(|inner| {
                names.insert(inner.id.clone(), inner.name.clone());
                ContentBlock::ToolUse {
                    id: inner.id,
                    name: inner.name,
                    input: inner.input.to_string(),
                }
            })
            .collect();
    }
    names.insert(call.id.clone(), call.function.name.clone());
    vec![ContentBlock::ToolUse {
        id: call.id.clone(),
        name: call.function.name.clone(),
        input: call.function.arguments.to_string(),
    }]
}

fn result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_results(
    messages: &mut Vec<ConversationMessage>,
    result: &ToolResult,
    names: &HashMap<String, String>,
    batches: &HashMap<String, Vec<String>>,
) {
    let text = result_text(result);
    let name_of = |id: &str| names.get(id).cloned().unwrap_or_default();
    if let Some(calls) = batches.get(&result.id) {
        for inner in batch::decode_results(&text, calls) {
            messages.push(ConversationMessage::tool_result(
                inner.id.clone(),
                name_of(&inner.id),
                inner.output,
                inner.is_error,
            ));
        }
        return;
    }
    let is_error = text.starts_with("Error:");
    messages.push(ConversationMessage::tool_result(
        result.id.clone(),
        name_of(&result.id),
        text,
        is_error,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_round_trips_through_infinity_history() {
        let messages = vec![
            ConversationMessage::user_text("hi"),
            ConversationMessage::assistant(vec![
                ContentBlock::Text {
                    text: "reading".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read_file".into(),
                    input: r#"{"path":"a"}"#.into(),
                },
            ]),
            ConversationMessage::tool_result("t1", "read_file", "content", false),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]),
        ];
        let back = from_infinity(&to_infinity(&messages));
        assert_eq!(back.len(), 4);
        assert_eq!(back[1].blocks.len(), 2, "text and call stay one message");
        assert!(matches!(
            &back[2].blocks[0],
            ContentBlock::ToolResult { tool_name, output, is_error: false, .. }
                if tool_name == "read_file" && output == "content"
        ));
    }

    #[test]
    fn batch_unpacks_into_calls_and_results() {
        let packed = batch::pack(vec![
            ToolCall::new("a", "read_file", json!({ "path": "x" })),
            ToolCall::new("b", "bash", json!({ "command": "ls" })),
        ])
        .expect("packed");
        let results = batch::encode_results(&[
            batch::InnerResult {
                id: "a".into(),
                output: "x".into(),
                is_error: false,
            },
            batch::InnerResult {
                id: "b".into(),
                output: "Error: no".into(),
                is_error: true,
            },
        ]);
        let entries = vec![
            InfinityMessage::ToolCall {
                call: packed,
                display_as: None,
            },
            InfinityMessage::ToolResult {
                result: ToolResult {
                    id: "a".into(),
                    call_id: None,
                    content: vec![ToolResultContent::Text(Text { text: results })],
                },
                display_segments: None,
            },
        ];
        let messages = from_infinity(&entries);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].blocks.len(), 2);
        assert!(matches!(
            &messages[2].blocks[0],
            ContentBlock::ToolResult { tool_name, is_error: true, .. } if tool_name == "bash"
        ));
    }

    #[test]
    fn error_results_keep_their_flag() {
        let messages = vec![ConversationMessage::tool_result("t", "bash", "boom", true)];
        let back = from_infinity(&to_infinity(&messages));
        assert!(matches!(
            &back[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "Error: boom"
        ));
    }
}
