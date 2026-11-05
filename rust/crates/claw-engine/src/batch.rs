//! Parallel tool calls on a one-call-per-completion engine.
//!
//! Infinity runs only the first tool call of a reply and drops the rest, but
//! models routinely ask for several at once (read three files, grep twice).
//! Instead of losing them, the stream adapter packs every call of a reply
//! into one call to the internal [`BATCH_TOOL`]; [`BatchTool`] runs them
//! (runs of read-only calls on parallel OS threads, everything else one by
//! one, in order) and answers with every result. When history goes back to
//! the provider the batch is unpacked again, so the model only ever sees its
//! own calls and their individual results.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use infinity_provider_protocol::{
    AssistantContent, Message, Text, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use rap_protocol::{ProviderCallId, ToolCallId};
use serde_json::{json, Value};

use crate::tools::ToolHost;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Name of the internal batch tool. Never shown to the model.
pub const BATCH_TOOL: &str = "parallel_tools";

/// Key of the batch result object.
const RESULTS_KEY: &str = "parallel_tools_results";

/// Tools that may run at the same time as each other: they only read.
pub const PARALLEL_SAFE: &[&str] = &[
    "read_file",
    "glob_search",
    "grep_search",
    "WebFetch",
    "WebSearch",
    "web_fetch",
    "web_search",
];

/// One call inside a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct InnerCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// One result of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerResult {
    pub id: String,
    pub output: String,
    pub is_error: bool,
}

/// The calls of one reply as the single call the engine runs: the call
/// itself when there is one, else a [`BATCH_TOOL`] call carrying them all
/// (under the first call's id).
#[must_use]
pub fn pack(mut calls: Vec<ToolCall>) -> Option<ToolCall> {
    match calls.len() {
        0 => None,
        1 => calls.pop(),
        _ => {
            let id = calls[0].id.clone();
            let inner = calls
                .into_iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "name": call.function.name,
                        "input": call.function.arguments,
                    })
                })
                .collect::<Vec<_>>();
            Some(ToolCall::new(id, BATCH_TOOL, json!({ "calls": inner })))
        }
    }
}

/// The calls a [`BATCH_TOOL`] call carries.
#[must_use]
pub fn unpack_calls(arguments: &Value) -> Vec<InnerCall> {
    arguments["calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .map(|call| InnerCall {
                    id: call["id"].as_str().unwrap_or_default().to_string(),
                    name: call["name"].as_str().unwrap_or_default().to_string(),
                    input: call.get("input").cloned().unwrap_or_else(|| json!({})),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A batch's results as tool-result text.
#[must_use]
pub fn encode_results(results: &[InnerResult]) -> String {
    let results = results
        .iter()
        .map(|result| {
            json!({ "id": result.id, "output": result.output, "is_error": result.is_error })
        })
        .collect::<Vec<_>>();
    json!({ RESULTS_KEY: results }).to_string()
}

/// The per-call results of a batch result `text`. A result that is not an
/// encoded batch (the engine's own error, an interruption) is given to
/// every call of `calls`, so each call still gets an answer.
#[must_use]
pub fn decode_results(text: &str, calls: &[String]) -> Vec<InnerResult> {
    let decoded = serde_json::from_str::<Value>(text).ok().and_then(|value| {
        value[RESULTS_KEY].as_array().map(|results| {
            results
                .iter()
                .map(|result| InnerResult {
                    id: result["id"].as_str().unwrap_or_default().to_string(),
                    output: result["output"].as_str().unwrap_or_default().to_string(),
                    is_error: result["is_error"].as_bool().unwrap_or(false),
                })
                .collect::<Vec<_>>()
        })
    });
    let decoded = decoded.unwrap_or_default();
    calls
        .iter()
        .map(|id| {
            decoded
                .iter()
                .find(|result| &result.id == id)
                .cloned()
                .unwrap_or_else(|| InnerResult {
                    id: id.clone(),
                    output: if text.starts_with("Error:") {
                        text.to_string()
                    } else {
                        "Error: no result for this call".to_string()
                    },
                    is_error: true,
                })
        })
        .collect()
}

/// The inner call ids of every batch call in `history`, by batch id.
#[must_use]
pub fn batches_in(history: &[Message]) -> HashMap<String, Vec<String>> {
    history
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content } => Some(content),
            Message::User { .. } => None,
        })
        .flatten()
        .filter_map(|item| match item {
            AssistantContent::ToolCall(call) if call.function.name == BATCH_TOOL => Some((
                call.id.clone(),
                unpack_calls(&call.function.arguments)
                    .into_iter()
                    .map(|call| call.id)
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

/// Runs a batch: each run of consecutive [`PARALLEL_SAFE`] calls on scoped
/// threads, every other call alone, results in call order. Blocking.
fn run_batch(host: &ToolHost, thread: &str, calls: &[InnerCall]) -> Vec<InnerResult> {
    let mut results = Vec::with_capacity(calls.len());
    let mut index = 0;
    while index < calls.len() {
        let end = if PARALLEL_SAFE.contains(&calls[index].name.as_str()) {
            calls[index..]
                .iter()
                .position(|call| !PARALLEL_SAFE.contains(&call.name.as_str()))
                .map_or(calls.len(), |offset| index + offset)
        } else {
            index + 1
        };
        let group = &calls[index..end];
        let outputs: Vec<(String, bool)> = if group.len() == 1 {
            vec![host.run(thread, &group[0].name, &group[0].input)]
        } else {
            std::thread::scope(|scope| {
                let handles = group
                    .iter()
                    .map(|call| scope.spawn(|| host.run(thread, &call.name, &call.input)))
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .unwrap_or_else(|_| ("Error: tool panicked".to_string(), true))
                    })
                    .collect()
            })
        };
        results.extend(
            group
                .iter()
                .zip(outputs)
                .map(|(call, (output, is_error))| InnerResult {
                    id: call.id.clone(),
                    output,
                    is_error,
                }),
        );
        index = end;
    }
    results
}

/// The internal tool that runs a packed batch. Dispatched like any slow
/// tool: the thread yields while the batch runs.
pub struct BatchTool {
    host: Arc<ToolHost>,
}

impl BatchTool {
    #[must_use]
    pub fn new(host: Arc<ToolHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl Tool<ChannelSender> for BatchTool {
    fn name(&self) -> &str {
        BATCH_TOOL
    }

    fn description(&self) -> &str {
        "Internal: runs several tool calls of one reply."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "calls": { "type": "array" } },
            "required": ["calls"]
        })
    }

    async fn execute(
        &self,
        args: Value,
        id: ToolCallId,
        call_id: Option<ProviderCallId>,
        context: &ToolContext<ChannelSender>,
    ) -> Result<(), BoxError> {
        let calls = unpack_calls(&args);
        let host = Arc::clone(&self.host);
        let thread = context.group_id.as_str().to_string();
        let sender = context.message_sender.clone();
        let group_id = context.group_id.clone();
        tokio::spawn(async move {
            let (text, images) = tokio::task::spawn_blocking(move || {
                let mut images = Vec::new();
                let mut results = run_batch(&host, &thread, &calls);
                for result in results.iter_mut().filter(|result| !result.is_error) {
                    let (text, found) =
                        crate::tools::with_images(std::mem::take(&mut result.output));
                    result.output = text;
                    images.extend(found);
                }
                (encode_results(&results), images)
            })
            .await
            .unwrap_or_else(|error| (format!("Error: tool batch panicked: {error}"), Vec::new()));
            let mut content = vec![ToolResultContent::Text(Text { text })];
            content.extend(images);
            let message = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(ToolResult {
                    id: id.as_str().to_string(),
                    call_id: call_id.map(ProviderCallId::into_inner),
                    content,
                })),
                group_id,
                metadata: None,
                synthetic: None,
                display_as: None,
                subscription: false,
            };
            // A send error means the engine shut down; nobody awaits the result.
            let _ = sender.send_to_input_queue(message, id.as_str()).await;
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_call_is_left_alone_and_several_are_packed() {
        let one = ToolCall::new("a", "read_file", json!({ "path": "x" }));
        assert_eq!(pack(vec![one.clone()]), Some(one));
        let packed = pack(vec![
            ToolCall::new("a", "read_file", json!({ "path": "x" })),
            ToolCall::new("b", "bash", json!({ "command": "ls" })),
        ])
        .expect("packed");
        assert_eq!(packed.id, "a");
        assert_eq!(packed.function.name, BATCH_TOOL);
        let calls = unpack_calls(&packed.function.arguments);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].name, "bash");
    }

    #[test]
    fn results_round_trip_and_errors_reach_every_call() {
        let results = vec![
            InnerResult {
                id: "a".into(),
                output: "x".into(),
                is_error: false,
            },
            InnerResult {
                id: "b".into(),
                output: "Error: no".into(),
                is_error: true,
            },
        ];
        let ids = vec!["a".to_string(), "b".to_string()];
        assert_eq!(decode_results(&encode_results(&results), &ids), results);
        let interrupted = decode_results("Error: interrupted", &ids);
        assert!(interrupted
            .iter()
            .all(|result| result.is_error && result.output == "Error: interrupted"));
    }
}
