//! [`ClawModelProvider`]: claw's provider clients (`api::ProviderClient`:
//! Anthropic and every OpenAI-compatible backend) behind Infinity's
//! [`ModelProvider`] trait.
//!
//! Requests are translated from Infinity's [`CompletionRequest`] into claw's
//! [`MessageRequest`]; claw's streamed [`StreamEvent`]s are folded back into
//! Infinity [`StreamChunk`]s (text and reasoning as deltas, tool calls and
//! reasoning blocks whole once their content block stops).

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::batch;

use api::{
    ApiError, ContentBlockDelta, ImageSource as ClawImageSource, InputContentBlock,
    InputMessage as ClawMessage, MessageRequest, MessageStream, OutputContentBlock, ProviderClient,
    StreamEvent, ToolChoice, ToolDefinition as ClawToolDefinition, ToolResultContentBlock,
    Usage as ClawUsage,
};
use async_trait::async_trait;
use futures_util::stream;
use infinity_provider_protocol::{
    AssistantContent, CompletionError, CompletionRequest, ErrorClass, FinalResponse, Image,
    ImageSource, Message, ModelEntry, ModelProvider, ModelStream, Reasoning, ReasoningContent,
    StreamChunk, ToolCall, ToolCallDeltaContent, ToolResultContent, Usage, UserContent,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Context window assumed for models claw has no limit for.
const DEFAULT_CONTEXT_WINDOW: usize = 200_000;

/// One claw model, served through its provider client.
pub struct ClawModelProvider {
    client: ProviderClient,
    entry: ModelEntry,
    /// `/effort`: sent as `reasoning_effort` (each backend maps it).
    reasoning_effort: Option<String>,
}

impl ClawModelProvider {
    /// A provider for `model` (aliases such as `opus` are resolved), using
    /// the credentials claw would use for it.
    pub fn from_model(model: &str) -> Result<Self, ApiError> {
        let client = ProviderClient::from_model(model)?;
        Ok(Self::new(client, model))
    }

    /// A provider for `model` over an already configured `client`.
    #[must_use]
    pub fn new(client: ProviderClient, model: &str) -> Self {
        let model_id = api::resolve_model_alias(model);
        let limit = api::model_token_limit(&model_id);
        let entry = ModelEntry {
            display_name: model_id.clone(),
            context_window: limit.as_ref().map_or(DEFAULT_CONTEXT_WINDOW, |limit| {
                usize::try_from(limit.context_window_tokens).unwrap_or(DEFAULT_CONTEXT_WINDOW)
            }),
            max_output_tokens: Some(u64::from(api::max_tokens_for_model(&model_id))),
            // Image tool results are not wired through the engine yet (see
            // docs/infinity-engine.md); the runtime then sends placeholders.
            supports_image_input: false,
            model_id,
        };
        Self {
            client,
            entry,
            reasoning_effort: None,
        }
    }

    /// Sends `effort` (`low`, `medium`, `high`, …) with every request.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// Caps replies at `tokens` output tokens (claw's reply budget, which
    /// leaves room in a local model's window).
    #[must_use]
    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.entry.max_output_tokens = Some(u64::from(tokens));
        self
    }

    /// Whether the model reads images. Without, Infinity sends a text
    /// placeholder in place of each image.
    #[must_use]
    pub fn with_image_input(mut self, supported: bool) -> Self {
        self.entry.supports_image_input = supported;
        self
    }

    /// The model this provider serves.
    #[must_use]
    pub fn entry(&self) -> &ModelEntry {
        &self.entry
    }
}

#[async_trait]
impl ModelProvider for ClawModelProvider {
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        Ok(vec![self.entry.clone()])
    }

    async fn invoke_model(
        &self,
        model_id: &str,
        request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError> {
        let max_tokens = request
            .max_tokens
            .or(self.entry.max_output_tokens)
            .map_or_else(
                || api::max_tokens_for_model(model_id),
                |tokens| u32::try_from(tokens).unwrap_or(u32::MAX),
            );
        let mut message_request = to_message_request(model_id, max_tokens, request);
        message_request
            .reasoning_effort
            .clone_from(&self.reasoning_effort);
        let message_stream = self
            .client
            .stream_message(&message_request)
            .await
            .map_err(|error| to_completion_error(&error))?;
        Ok(into_chunk_stream(message_stream))
    }
}

/// Where Infinity's per-thread line starts in the preamble it builds.
const THREAD_INFO_MARKER: &str = "Your current thread ID is `";

/// `preamble` without Infinity's built-in base prompt. infinity-agent-core
/// always starts the preamble with the prompt of Infinity Code, which
/// describes tools claw does not have (`clone_repo`, `squash_sandbox`,
/// `list_steering`); claw supplies its own prompt (with the engine's thread
/// guidance) as the extra system prompt, after the thread-id line that is
/// kept here.
#[must_use]
pub fn strip_base_prompt(preamble: &str) -> String {
    preamble.find(THREAD_INFO_MARKER).map_or_else(
        || preamble.to_string(),
        |start| preamble[start..].to_string(),
    )
}

/// Any provider as the engine needs it: Infinity's base prompt is removed
/// from each request (see [`strip_base_prompt`]), and the tool calls of each
/// reply are held back and sent as one packed call when the reply ends (see
/// [`crate::batch`]). The engine wraps every provider in it.
pub struct ClawPrompt(pub std::sync::Arc<dyn ModelProvider>);

#[async_trait]
impl ModelProvider for ClawPrompt {
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        self.0.list_models().await
    }

    async fn invoke_model(
        &self,
        model_id: &str,
        mut request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError> {
        request.preamble = request
            .preamble
            .map(|preamble| strip_base_prompt(&preamble));
        let inner = self.0.invoke_model(model_id, request).await?;
        Ok(pack_tool_calls(inner))
    }
}

/// `inner` with its tool calls packed into one, sent just before `Final`
/// (or at the end of the stream).
fn pack_tool_calls(inner: ModelStream) -> ModelStream {
    use futures_util::StreamExt;
    struct State {
        inner: ModelStream,
        calls: Vec<ToolCall>,
        pending: VecDeque<Result<StreamChunk, CompletionError>>,
        done: bool,
    }
    let state = State {
        inner,
        calls: Vec::new(),
        pending: VecDeque::new(),
        done: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }
            if state.done {
                return None;
            }
            match state.inner.next().await {
                Some(Ok(StreamChunk::ToolCall(call))) => state.calls.push(call),
                Some(Ok(StreamChunk::Final(final_response))) => {
                    let packed = batch::pack(std::mem::take(&mut state.calls));
                    state
                        .pending
                        .extend(packed.map(|call| Ok(StreamChunk::ToolCall(call))));
                    state
                        .pending
                        .push_back(Ok(StreamChunk::Final(final_response)));
                }
                // An error discards the reply; held calls go with it.
                Some(Err(error)) => {
                    state.calls.clear();
                    state.pending.push_back(Err(error));
                }
                Some(Ok(chunk)) => state.pending.push_back(Ok(chunk)),
                None => {
                    state.done = true;
                    let packed = batch::pack(std::mem::take(&mut state.calls));
                    state
                        .pending
                        .extend(packed.map(|call| Ok(StreamChunk::ToolCall(call))));
                }
            }
        }
    }))
}

/// Infinity's request in claw's wire form. Batches of tool calls (see
/// [`crate::batch`]) are unpacked into the calls and results the model made.
#[must_use]
pub fn to_message_request(
    model_id: &str,
    max_tokens: u32,
    request: CompletionRequest,
) -> MessageRequest {
    let tools = (!request.tools.is_empty()).then(|| {
        request
            .tools
            .into_iter()
            .filter(|tool| tool.name != batch::BATCH_TOOL)
            .map(|tool| ClawToolDefinition {
                name: tool.name,
                description: Some(tool.description),
                input_schema: tool.parameters,
            })
            .collect::<Vec<_>>()
    });
    let mut extra_body = BTreeMap::new();
    if let Some(serde_json::Value::Object(params)) = request.additional_params {
        extra_body.extend(params);
    }
    let batches = batch::batches_in(&request.chat_history);
    MessageRequest {
        model: model_id.to_string(),
        max_tokens,
        messages: merge_same_role(
            request
                .chat_history
                .into_iter()
                .filter_map(|message| to_claw_message(message, &batches)),
        ),
        system: request.preamble.filter(|preamble| !preamble.is_empty()),
        tool_choice: tools.as_ref().map(|_| ToolChoice::Auto),
        tools,
        stream: true,
        extra_body,
        ..MessageRequest::default()
    }
}

/// One message per run of same-role messages: Infinity keeps each history
/// entry as its own message, and providers want roles to alternate (all
/// results of a run of tool calls in one user message).
fn merge_same_role(messages: impl Iterator<Item = ClawMessage>) -> Vec<ClawMessage> {
    let mut merged: Vec<ClawMessage> = Vec::new();
    for message in messages {
        match merged.last_mut() {
            Some(last) if last.role == message.role => last.content.extend(message.content),
            _ => merged.push(message),
        }
    }
    merged
}

fn to_claw_message(
    message: Message,
    batches: &HashMap<String, Vec<String>>,
) -> Option<ClawMessage> {
    let (role, content) = match message {
        Message::User { content } => ("user", user_blocks(content, batches)),
        Message::Assistant { content } => ("assistant", assistant_blocks(content)),
    };
    (!content.is_empty()).then(|| ClawMessage {
        role: role.to_string(),
        content,
    })
}

fn user_blocks(
    content: Vec<UserContent>,
    batches: &HashMap<String, Vec<String>>,
) -> Vec<InputContentBlock> {
    let mut blocks = Vec::new();
    // Images a tool returned go after the run of tool results, so each
    // result stays paired with its call (claw's results carry text only).
    let mut trailing_images = Vec::new();
    for item in content {
        match item {
            UserContent::Text(text) => blocks.push(InputContentBlock::Text { text: text.text }),
            UserContent::Image(image) => blocks.push(image_block(image)),
            UserContent::ToolResult(result) => {
                let mut texts = Vec::new();
                for part in result.content {
                    match part {
                        ToolResultContent::Text(text) => texts.push(text.text),
                        ToolResultContent::Image(image) => trailing_images.push(image_block(image)),
                    }
                }
                let text = texts.join("\n");
                if let Some(calls) = batches.get(&result.id) {
                    blocks.extend(
                        batch::decode_results(&text, calls)
                            .into_iter()
                            .map(|inner| InputContentBlock::ToolResult {
                                tool_use_id: inner.id,
                                content: vec![ToolResultContentBlock::Text { text: inner.output }],
                                is_error: inner.is_error,
                            }),
                    );
                    continue;
                }
                let is_error = text.starts_with("Error:");
                blocks.push(InputContentBlock::ToolResult {
                    tool_use_id: result.id,
                    content: vec![ToolResultContentBlock::Text { text }],
                    is_error,
                });
            }
        }
    }
    blocks.extend(trailing_images);
    blocks
}

fn image_block(image: Image) -> InputContentBlock {
    match image.data {
        ImageSource::Base64(data) => InputContentBlock::Image {
            source: ClawImageSource::base64(
                image
                    .media_type
                    .map_or("image/png", |media_type| media_type.to_mime_type()),
                data,
            ),
        },
        ImageSource::Url(url) => InputContentBlock::Text {
            text: format!("[image: {url}]"),
        },
    }
}

fn assistant_blocks(content: Vec<AssistantContent>) -> Vec<InputContentBlock> {
    content
        .into_iter()
        .flat_map(|item| match item {
            AssistantContent::ToolCall(call) if call.function.name == batch::BATCH_TOOL => {
                batch::unpack_calls(&call.function.arguments)
                    .into_iter()
                    .map(|inner| InputContentBlock::ToolUse {
                        id: inner.id,
                        name: inner.name,
                        input: inner.input,
                    })
                    .collect::<Vec<_>>()
            }
            other => assistant_block(other).into_iter().collect(),
        })
        .collect()
}

fn assistant_block(item: AssistantContent) -> Option<InputContentBlock> {
    match item {
        AssistantContent::Text(text) if text.text.is_empty() => None,
        AssistantContent::Text(text) => Some(InputContentBlock::Text { text: text.text }),
        AssistantContent::ToolCall(call) => Some(InputContentBlock::ToolUse {
            id: call.id,
            name: call.function.name,
            input: call.function.arguments,
        }),
        AssistantContent::Reasoning(reasoning) => {
            let thinking = reasoning.first_text().unwrap_or_default().to_string();
            let signature = reasoning.first_signature().map(ToOwned::to_owned);
            // Unsigned thinking cannot be replayed to Anthropic, and the
            // OpenAI-compatible backends drop it anyway.
            signature.map(|signature| InputContentBlock::Thinking {
                thinking,
                signature: Some(signature),
            })
        }
    }
}

/// How the engine should treat a claw API error.
#[must_use]
pub fn error_class(error: &ApiError) -> ErrorClass {
    if error.is_context_window_failure() {
        return ErrorClass::ContextOverflow;
    }
    match error {
        // claw's client has already retried with backoff; retrying again
        // would multiply the wait.
        ApiError::RetriesExhausted { .. } => ErrorClass::Fatal,
        ApiError::Api { status, .. } if status.as_u16() == 429 || status.as_u16() == 529 => {
            ErrorClass::Throttled
        }
        _ if error.is_retryable() => ErrorClass::Transient,
        _ => ErrorClass::Fatal,
    }
}

fn to_completion_error(error: &ApiError) -> CompletionError {
    CompletionError::provider(error_class(error), error.to_string())
}

/// A content block being streamed.
enum OpenBlock {
    Text,
    Tool {
        id: String,
        name: String,
        json: String,
        initial: serde_json::Value,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
    Redacted(serde_json::Value),
}

/// Folds claw stream events into Infinity chunks.
#[derive(Default)]
pub struct ChunkFolder {
    blocks: BTreeMap<u32, OpenBlock>,
    usage: ClawUsage,
    finished: bool,
}

impl ChunkFolder {
    /// The chunks `event` produces.
    pub fn push(&mut self, event: StreamEvent) -> Vec<StreamChunk> {
        match event {
            StreamEvent::MessageStart(start) => {
                self.usage = start.message.usage;
                Vec::new()
            }
            StreamEvent::MessageDelta(delta) => {
                let usage = delta.usage;
                // Providers report running totals here; keep the largest.
                self.usage.input_tokens = self.usage.input_tokens.max(usage.input_tokens);
                self.usage.output_tokens = self.usage.output_tokens.max(usage.output_tokens);
                self.usage.cache_read_input_tokens = self
                    .usage
                    .cache_read_input_tokens
                    .max(usage.cache_read_input_tokens);
                self.usage.cache_creation_input_tokens = self
                    .usage
                    .cache_creation_input_tokens
                    .max(usage.cache_creation_input_tokens);
                Vec::new()
            }
            StreamEvent::ContentBlockStart(start) => {
                self.start_block(start.index, start.content_block)
            }
            StreamEvent::ContentBlockDelta(delta) => self.delta(delta.index, delta.delta),
            StreamEvent::ContentBlockStop(stop) => self.stop_block(stop.index),
            StreamEvent::MessageStop(_) => self.finish(),
        }
    }

    /// Chunks still owed when the stream ends (open blocks, the final usage).
    pub fn finish(&mut self) -> Vec<StreamChunk> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let indexes = self.blocks.keys().copied().collect::<Vec<_>>();
        let mut chunks = indexes
            .into_iter()
            .flat_map(|index| self.stop_block(index))
            .collect::<Vec<_>>();
        chunks.push(StreamChunk::Final(FinalResponse {
            usage: Some(to_usage(&self.usage)),
        }));
        chunks
    }

    fn start_block(&mut self, index: u32, block: OutputContentBlock) -> Vec<StreamChunk> {
        let (open, chunks) = match block {
            OutputContentBlock::Text { text } => (
                OpenBlock::Text,
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![StreamChunk::Text(text)]
                },
            ),
            OutputContentBlock::ToolUse { id, name, input } => (
                OpenBlock::Tool {
                    id: id.clone(),
                    name: name.clone(),
                    json: String::new(),
                    initial: input,
                },
                vec![StreamChunk::ToolCallDelta {
                    id,
                    content: ToolCallDeltaContent::Name(name),
                }],
            ),
            OutputContentBlock::Thinking {
                thinking,
                signature,
            } => {
                let chunks = if thinking.is_empty() {
                    Vec::new()
                } else {
                    vec![StreamChunk::ReasoningDelta {
                        id: None,
                        text: thinking.clone(),
                    }]
                };
                (
                    OpenBlock::Thinking {
                        text: thinking,
                        signature,
                    },
                    chunks,
                )
            }
            OutputContentBlock::RedactedThinking { data } => {
                (OpenBlock::Redacted(data), Vec::new())
            }
        };
        self.blocks.insert(index, open);
        chunks
    }

    fn delta(&mut self, index: u32, delta: ContentBlockDelta) -> Vec<StreamChunk> {
        let block = self.blocks.entry(index).or_insert(OpenBlock::Text);
        match (block, delta) {
            (_, ContentBlockDelta::TextDelta { text }) if !text.is_empty() => {
                vec![StreamChunk::Text(text)]
            }
            (
                OpenBlock::Tool { id, json, .. },
                ContentBlockDelta::InputJsonDelta { partial_json },
            ) => {
                json.push_str(&partial_json);
                vec![StreamChunk::ToolCallDelta {
                    id: id.clone(),
                    content: ToolCallDeltaContent::Delta(partial_json),
                }]
            }
            (OpenBlock::Thinking { text, .. }, ContentBlockDelta::ThinkingDelta { thinking }) => {
                text.push_str(&thinking);
                vec![StreamChunk::ReasoningDelta {
                    id: None,
                    text: thinking,
                }]
            }
            (
                OpenBlock::Thinking { signature, .. },
                ContentBlockDelta::SignatureDelta { signature: part },
            ) => {
                signature.get_or_insert_with(String::new).push_str(&part);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn stop_block(&mut self, index: u32) -> Vec<StreamChunk> {
        match self.blocks.remove(&index) {
            None | Some(OpenBlock::Text) => Vec::new(),
            Some(OpenBlock::Tool {
                id,
                name,
                json,
                initial,
            }) => {
                let arguments = parse_tool_input(&json, initial);
                vec![StreamChunk::ToolCall(ToolCall::new(id, name, arguments))]
            }
            Some(OpenBlock::Thinking { text, signature }) => {
                vec![StreamChunk::Reasoning(Reasoning::new_with_signature(
                    text, signature,
                ))]
            }
            Some(OpenBlock::Redacted(data)) => vec![StreamChunk::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Redacted {
                    data: data
                        .as_str()
                        .map_or_else(|| data.to_string(), ToOwned::to_owned),
                }],
            })],
        }
    }
}

/// The arguments of a finished tool call: the streamed JSON (repaired if a
/// local model left it malformed), else the block's initial `input`.
fn parse_tool_input(json: &str, initial: serde_json::Value) -> serde_json::Value {
    if json.trim().is_empty() {
        return if initial.is_null() {
            serde_json::json!({})
        } else {
            initial
        };
    }
    serde_json::from_str(json)
        .ok()
        .or_else(|| api::repair_json(json))
        .unwrap_or_else(|| serde_json::json!({ "_raw": json }))
}

fn to_usage(usage: &ClawUsage) -> Usage {
    let input = u64::from(usage.input_tokens)
        + u64::from(usage.cache_read_input_tokens)
        + u64::from(usage.cache_creation_input_tokens);
    let output = u64::from(usage.output_tokens);
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        cached_input_tokens: u64::from(usage.cache_read_input_tokens),
    }
}

fn into_chunk_stream(message_stream: MessageStream) -> ModelStream {
    struct State {
        stream: MessageStream,
        folder: ChunkFolder,
        pending: VecDeque<StreamChunk>,
        done: bool,
    }
    let state = State {
        stream: message_stream,
        folder: ChunkFolder::default(),
        pending: VecDeque::new(),
        done: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(chunk) = state.pending.pop_front() {
                return Some((Ok(chunk), state));
            }
            if state.done {
                return None;
            }
            match state.stream.next_event().await {
                Ok(Some(event)) => state.pending.extend(state.folder.push(event)),
                Ok(None) => {
                    state.done = true;
                    state.pending.extend(state.folder.finish());
                }
                Err(error) => {
                    state.done = true;
                    return Some((Err(to_completion_error(&error)), state));
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{
        ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent, MessageStopEvent,
    };
    use infinity_provider_protocol::{Text, ToolDefinition, ToolResult};
    use serde_json::json;

    fn start(index: u32, block: OutputContentBlock) -> StreamEvent {
        StreamEvent::ContentBlockStart(ContentBlockStartEvent {
            index,
            content_block: block,
        })
    }

    fn delta(index: u32, delta: ContentBlockDelta) -> StreamEvent {
        StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent { index, delta })
    }

    fn stop(index: u32) -> StreamEvent {
        StreamEvent::ContentBlockStop(ContentBlockStopEvent { index })
    }

    #[test]
    fn folds_text_thinking_and_tool_use_into_chunks() {
        let mut folder = ChunkFolder::default();
        let events = vec![
            start(
                0,
                OutputContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                },
            ),
            delta(
                0,
                ContentBlockDelta::ThinkingDelta {
                    thinking: "hm".into(),
                },
            ),
            delta(
                0,
                ContentBlockDelta::SignatureDelta {
                    signature: "sig".into(),
                },
            ),
            stop(0),
            start(
                1,
                OutputContentBlock::Text {
                    text: String::new(),
                },
            ),
            delta(1, ContentBlockDelta::TextDelta { text: "Hi".into() }),
            stop(1),
            start(
                2,
                OutputContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: json!({}),
                },
            ),
            delta(
                2,
                ContentBlockDelta::InputJsonDelta {
                    partial_json: "{\"command\":".into(),
                },
            ),
            delta(
                2,
                ContentBlockDelta::InputJsonDelta {
                    partial_json: "\"ls\"}".into(),
                },
            ),
            stop(2),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];
        let chunks = events
            .into_iter()
            .flat_map(|event| folder.push(event))
            .collect::<Vec<_>>();
        assert!(
            chunks.contains(&StreamChunk::Reasoning(Reasoning::new_with_signature(
                "hm",
                Some("sig".into())
            )))
        );
        assert!(chunks.contains(&StreamChunk::Text("Hi".into())));
        assert!(chunks.contains(&StreamChunk::ToolCall(ToolCall::new(
            "t1",
            "bash",
            json!({ "command": "ls" })
        ))));
        assert!(matches!(chunks.last(), Some(StreamChunk::Final(_))));
        assert!(folder.finish().is_empty(), "final is sent once");
    }

    #[test]
    fn unterminated_tool_call_is_closed_at_end_of_stream() {
        let mut folder = ChunkFolder::default();
        folder.push(start(
            0,
            OutputContentBlock::ToolUse {
                id: "t".into(),
                name: "read_file".into(),
                input: json!({ "path": "a" }),
            },
        ));
        let chunks = folder.finish();
        assert_eq!(
            chunks[0],
            StreamChunk::ToolCall(ToolCall::new("t", "read_file", json!({ "path": "a" })))
        );
    }

    #[test]
    fn request_keeps_tool_pairs_and_marks_errors() {
        let request = CompletionRequest {
            preamble: Some("sys".into()),
            chat_history: vec![
                Message::user("hi"),
                Message::Assistant {
                    content: vec![AssistantContent::ToolCall(ToolCall::new(
                        "c1",
                        "bash",
                        json!({ "command": "ls" }),
                    ))],
                },
                Message::User {
                    content: vec![UserContent::ToolResult(ToolResult {
                        id: "c1".into(),
                        call_id: None,
                        content: vec![ToolResultContent::Text(Text {
                            text: "Error: denied".into(),
                        })],
                    })],
                },
            ],
            tools: vec![ToolDefinition {
                name: "bash".into(),
                description: "run".into(),
                parameters: json!({ "type": "object" }),
            }],
            max_tokens: None,
            additional_params: None,
        };
        let message_request = to_message_request("m", 100, request);
        assert_eq!(message_request.system.as_deref(), Some("sys"));
        assert_eq!(message_request.messages.len(), 3);
        assert!(matches!(
            &message_request.messages[1].content[0],
            InputContentBlock::ToolUse { id, .. } if id == "c1"
        ));
        assert!(matches!(
            &message_request.messages[2].content[0],
            InputContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "c1"
        ));
        assert_eq!(message_request.tool_choice, Some(ToolChoice::Auto));
        assert!(message_request.stream);
    }

    #[test]
    fn batch_is_unpacked_into_the_models_own_calls_and_results() {
        let packed = batch::pack(vec![
            ToolCall::new("a", "read_file", json!({ "path": "x" })),
            ToolCall::new("b", "grep_search", json!({ "pattern": "y" })),
        ])
        .expect("packed");
        let results = batch::encode_results(&[
            batch::InnerResult {
                id: "a".into(),
                output: "x-content".into(),
                is_error: false,
            },
            batch::InnerResult {
                id: "b".into(),
                output: "Error: bad pattern".into(),
                is_error: true,
            },
        ]);
        let request = CompletionRequest {
            chat_history: vec![
                Message::user("go"),
                Message::Assistant {
                    content: vec![AssistantContent::ToolCall(packed)],
                },
                Message::User {
                    content: vec![UserContent::ToolResult(ToolResult {
                        id: "a".into(),
                        call_id: None,
                        content: vec![ToolResultContent::Text(Text { text: results })],
                    })],
                },
            ],
            tools: vec![ToolDefinition {
                name: batch::BATCH_TOOL.into(),
                description: String::new(),
                parameters: json!({}),
            }],
            ..CompletionRequest::default()
        };
        let message_request = to_message_request("m", 100, request);
        assert!(
            message_request.tools.as_ref().is_some_and(Vec::is_empty)
                || message_request.tools.is_none()
                || message_request
                    .tools
                    .as_ref()
                    .is_some_and(|tools| tools.iter().all(|tool| tool.name != batch::BATCH_TOOL))
        );
        let uses = &message_request.messages[1].content;
        assert!(
            matches!(&uses[0], InputContentBlock::ToolUse { id, name, .. } if id == "a" && name == "read_file")
        );
        assert!(
            matches!(&uses[1], InputContentBlock::ToolUse { id, name, .. } if id == "b" && name == "grep_search")
        );
        let answers = &message_request.messages[2].content;
        assert!(
            matches!(&answers[0], InputContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "a")
        );
        assert!(
            matches!(&answers[1], InputContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "b")
        );
    }

    #[test]
    fn strips_only_the_infinity_base_prompt() {
        let preamble = "You are Infinity...\n\nYour current thread ID is `t`. Root `t`.\n\nclaw";
        assert_eq!(
            strip_base_prompt(preamble),
            "Your current thread ID is `t`. Root `t`.\n\nclaw"
        );
        assert_eq!(strip_base_prompt("no marker"), "no marker");
    }

    #[test]
    fn context_window_errors_map_to_overflow() {
        let error = ApiError::ContextWindowExceeded {
            model: "m".into(),
            estimated_input_tokens: 10,
            requested_output_tokens: 10,
            estimated_total_tokens: 20,
            context_window_tokens: 5,
        };
        assert_eq!(error_class(&error), ErrorClass::ContextOverflow);
        assert_eq!(error_class(&ApiError::Auth("no".into())), ErrorClass::Fatal);
    }
}
