//! Each model reply as the engine streams it ([`reply_stream`]): its tool
//! calls are held back and packed into one (see [`crate::batch`]), and when
//! it ends it is checked the way claw's classic loop checked replies:
//!
//! - A reply that came back empty is asked for once more.
//! - A reply cut off at the output limit ends inside its last tool call:
//!   that call's input is unfinished JSON and is not run.
//! - Tool calls a local server's parser missed, left in the text, are run
//!   as the calls they are (`CLAW_TEXT_TOOL_CALLS=0` turns that off).
//! - A reply stuck repeating itself is marked, so it is not continued and
//!   its text is not sent back (that starts the loop again).
//!
//! What the reply did goes to the turn's [`TurnGuards`], which decide what
//! the thread is sent next (a continuation, a reminder). Text the model
//! should not see again is recorded in [`Rewrites`] and replaced in later
//! requests and in the messages the turn returns.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{stream, StreamExt};
use infinity_provider_protocol::{
    AssistantContent, CompletionError, CompletionRequest, Message, ModelProvider, ModelStream,
    StreamChunk, Text, ToolCall, ToolResultContent, UserContent,
};
use runtime::turn_guards::{self, REPETITION_NOTE, THINKING_TAIL_CHARS};

use crate::batch;
use crate::guards::{ReplyReport, TurnGuards};
use crate::provider::NameResolver;

/// What claw's provider saw of the reply it streams that Infinity's chunks
/// don't carry (their final chunk has no stop reason).
#[derive(Debug, Default)]
pub struct ReplyFlags {
    /// The reply stopped at the output-token limit.
    pub cut_off: AtomicBool,
    /// Calls whose streamed input was not valid JSON (repaired or not).
    pub malformed_calls: Mutex<Vec<String>>,
}

impl ReplyFlags {
    fn reset(&self) {
        self.cut_off.store(false, Ordering::Relaxed);
        lock(&self.malformed_calls).clear();
    }
}

tokio::task_local! {
    /// The flags of the reply being requested; set by [`reply_stream`]'s
    /// caller around the provider call, read by claw's provider.
    pub static REPLY_FLAGS: Arc<ReplyFlags>;
}

/// Text to send in place of a reply's text (the original, then its
/// replacement), by the hash of that text.
pub type Rewrites = Arc<Mutex<HashMap<u64, (String, String)>>>;

/// One reply being requested again (it came back empty).
pub struct Retry {
    pub provider: Arc<dyn ModelProvider>,
    pub model_id: String,
    pub request: CompletionRequest,
}

/// Where a reply is checked and reported; see the module docs.
pub struct ReplyContext {
    pub thread: String,
    pub resolve_name: Option<NameResolver>,
    pub guards: Option<Arc<TurnGuards>>,
    pub rewrites: Rewrites,
    pub flags: Arc<ReplyFlags>,
    pub retry: Option<Retry>,
}

#[derive(Default)]
struct Seen {
    text: String,
    thinking: String,
    calls: Vec<ToolCall>,
    /// Any text, thinking or call came through.
    content: bool,
}

/// `inner` checked and packed; see the module docs.
pub fn reply_stream(inner: ModelStream, context: ReplyContext) -> ModelStream {
    struct State {
        inner: ModelStream,
        context: ReplyContext,
        seen: Seen,
        pending: VecDeque<Result<StreamChunk, CompletionError>>,
        done: bool,
    }
    let state = State {
        inner,
        context,
        seen: Seen::default(),
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
            let next = state.inner.next().await;
            let ended = matches!(next, None | Some(Ok(StreamChunk::Final(_))));
            if ended && !state.seen.content {
                // An empty reply: once more, as the classic loop did.
                if let Some(retry) = state.context.retry.take() {
                    state.context.flags.reset();
                    let flags = Arc::clone(&state.context.flags);
                    match REPLY_FLAGS
                        .scope(
                            flags,
                            retry.provider.invoke_model(&retry.model_id, retry.request),
                        )
                        .await
                    {
                        Ok(stream) => {
                            state.inner = stream;
                            continue;
                        }
                        Err(error) => {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                    }
                }
            }
            match next {
                Some(Ok(StreamChunk::ToolCall(mut call))) => {
                    if let Some(resolve) = &state.context.resolve_name {
                        call.function.name = resolve(&call.function.name);
                    }
                    state.seen.content = true;
                    state.seen.calls.push(call);
                }
                Some(Ok(StreamChunk::Final(final_response))) => {
                    finish(&mut state.seen, &state.context, &mut state.pending);
                    state
                        .pending
                        .push_back(Ok(StreamChunk::Final(final_response)));
                    state.done = true;
                }
                // An error discards the reply; held calls go with it.
                Some(Err(error)) => {
                    state.seen.calls.clear();
                    state.pending.push_back(Err(error));
                }
                Some(Ok(chunk)) => {
                    match &chunk {
                        StreamChunk::Text(text) => {
                            state.seen.content |= !text.is_empty();
                            state.seen.text.push_str(text);
                        }
                        StreamChunk::ReasoningDelta { text, .. } => {
                            state.seen.content |= !text.is_empty();
                            state.seen.thinking.push_str(text);
                        }
                        StreamChunk::Reasoning(reasoning) => {
                            state.seen.content = true;
                            if state.seen.thinking.is_empty() {
                                state
                                    .seen
                                    .thinking
                                    .push_str(reasoning.first_text().unwrap_or_default());
                            }
                        }
                        _ => {}
                    }
                    state.pending.push_back(Ok(chunk));
                }
                None => {
                    state.done = true;
                    finish(&mut state.seen, &state.context, &mut state.pending);
                }
            }
        }
    }))
}

/// The end of a reply: checks, the report to the guards, the packed call.
fn finish(
    seen: &mut Seen,
    context: &ReplyContext,
    pending: &mut VecDeque<Result<StreamChunk, CompletionError>>,
) {
    let cut_off = context.flags.cut_off.load(Ordering::Relaxed);
    let mut calls = std::mem::take(&mut seen.calls);
    // A reply cut off at the token limit ends inside its last tool call.
    let dropped_cut_off_call = cut_off
        && calls
            .last()
            .is_some_and(|call| lock(&context.flags.malformed_calls).contains(&call.id));
    if dropped_cut_off_call {
        calls.pop();
    }
    // A local server whose tool-call parser missed the model's format
    // hands the calls over as text: run them as the calls they are.
    if calls.is_empty() && !cut_off && runtime::text_tool_calls::enabled() {
        let (rest, found) = runtime::text_tool_calls::extract(&seen.text);
        if !found.is_empty() {
            eprintln!(
                "\n\x1b[2m(the model wrote {} tool call(s) as text; running {})\x1b[0m",
                found.len(),
                if found.len() == 1 { "it" } else { "them" }
            );
            note_rewrite(&context.rewrites, &seen.text, rest);
            for call in found {
                let name = context
                    .resolve_name
                    .as_ref()
                    .map_or_else(|| call.name.clone(), |resolve| resolve(&call.name));
                let input = serde_json::from_str(&call.input)
                    .unwrap_or_else(|_| serde_json::json!({ "_raw": call.input }));
                calls.push(ToolCall::new(
                    format!("toolu_text_{}", uuid::Uuid::new_v4().simple()),
                    name,
                    input,
                ));
            }
        }
    }
    let looped = turn_guards::is_looped_reply(&seen.text, cut_off);
    if looped {
        eprintln!("\n\x1b[2m(the reply got stuck repeating itself, so it was stopped)\x1b[0m");
        note_rewrite(&context.rewrites, &seen.text, REPETITION_NOTE.to_string());
    }
    let thinking = seen.thinking.trim();
    let thinking_tail = (seen.text.trim().is_empty() && calls.is_empty() && !thinking.is_empty())
        .then(|| {
            let skip = thinking.chars().count().saturating_sub(THINKING_TAIL_CHARS);
            thinking.chars().skip(skip).collect::<String>()
        });
    if let Some(guards) = &context.guards {
        guards.after_reply(
            &context.thread,
            &ReplyReport {
                tool_calls: calls.len(),
                cut_off,
                thinking_tail,
                looped,
                dropped_cut_off_call,
            },
        );
    }
    pending.extend(batch::pack(calls).map(|call| Ok(StreamChunk::ToolCall(call))));
}

fn note_rewrite(rewrites: &Rewrites, text: &str, replacement: String) {
    if !text.is_empty() {
        lock(rewrites).insert(hash(text), (text.to_string(), replacement));
    }
}

/// The thread a request is for: Infinity names it in the preamble.
#[must_use]
pub fn thread_of(preamble: Option<&str>) -> String {
    const MARKER: &str = "Your current thread ID is `";
    preamble
        .and_then(|preamble| {
            let start = preamble.find(MARKER)? + MARKER.len();
            let end = preamble[start..].find('`')?;
            Some(preamble[start..start + end].to_string())
        })
        .unwrap_or_default()
}

/// `history` as it should reach the model: rewritten reply text (loops,
/// tool-call markup), and only the latest few tool images.
pub fn prepare_history(history: &mut [Message], rewrites: &Rewrites) {
    let rewrites = lock(rewrites);
    let mut images = 0;
    for message in history.iter_mut().rev() {
        match message {
            Message::Assistant { content } if !rewrites.is_empty() => {
                for item in content.iter_mut() {
                    if let AssistantContent::Text(text) = item {
                        if let Some((_, replacement)) = rewrites.get(&hash(&text.text)) {
                            text.text.clone_from(replacement);
                        }
                    }
                }
            }
            Message::User { content } => {
                for item in content.iter_mut().rev() {
                    let UserContent::ToolResult(result) = item else {
                        continue;
                    };
                    for part in result.content.iter_mut().rev() {
                        if matches!(part, ToolResultContent::Image(_)) {
                            images += 1;
                            if images > runtime::tool_images::KEEP_TOOL_IMAGES {
                                *part = ToolResultContent::Text(Text {
                                    text: "[older image left out]".to_string(),
                                });
                            }
                        }
                    }
                }
            }
            Message::Assistant { .. } => {}
        }
    }
}

/// `messages` (claw's form) with reply text rewritten as in requests, so
/// the session keeps what the model is shown.
pub fn rewrite_messages(messages: &mut [runtime::ConversationMessage], rewrites: &Rewrites) {
    let rewrites = lock(rewrites);
    if rewrites.is_empty() {
        return;
    }
    for message in messages {
        if message.role != runtime::MessageRole::Assistant {
            continue;
        }
        message.blocks.retain_mut(|block| match block {
            runtime::ContentBlock::Text { text } => match rewrites.get(&hash(text)) {
                Some((_, replacement)) if replacement.trim().is_empty() => false,
                Some((_, replacement)) => {
                    text.clone_from(replacement);
                    true
                }
                None => true,
            },
            _ => true,
        });
    }
}

/// `text` (a reply's text, or several replies' run together) with each
/// rewritten reply in it replaced.
#[must_use]
pub fn rewrite_text(text: &str, rewrites: &Rewrites) -> String {
    let rewrites = lock(rewrites);
    if rewrites.is_empty() {
        return text.to_string();
    }
    let mut text = text.to_string();
    for (original, replacement) in rewrites.values() {
        text = text.replace(original.as_str(), replacement);
    }
    text.trim().to_string()
}

/// Whether the request's last message follows a reply cut off mid-thought
/// or a cut-off call (see [`turn_guards::follows_thinking_cut_off`]).
#[must_use]
pub fn follows_thinking_cut_off(request: &CompletionRequest) -> bool {
    let Some(Message::User { content }) = request.chat_history.last() else {
        return false;
    };
    content.iter().any(|item| match item {
        UserContent::Text(text) => turn_guards::follows_thinking_cut_off(&text.text),
        UserContent::ToolResult(result) => result.content.iter().any(|part| {
            matches!(part, ToolResultContent::Text(text)
                if turn_guards::follows_thinking_cut_off(&text.text))
        }),
        UserContent::Image(_) => false,
    })
}

fn hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
