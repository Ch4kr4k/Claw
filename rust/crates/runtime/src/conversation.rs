use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde_json::{Map, Value};
use telemetry::SessionTracer;

use crate::compact::{
    compact_session, compact_session_with, estimate_session_tokens, prune_old_tool_results,
    summary_transcript, truncate_large_tool_results, CompactionConfig, CompactionResult,
    SUMMARY_SYSTEM_PROMPT,
};
use crate::config::RuntimeFeatureConfig;
use crate::hooks::{HookAbortSignal, HookProgressReporter, HookRunResult, HookRunner};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter,
};
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::usage::{TokenUsage, UsageTracker};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 100_000;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";
/// How many times one turn asks the model to go on after a reply was cut
/// off at the output-token limit, before the turn ends anyway.
const MAX_CUT_OFF_CONTINUATIONS: usize = 3;
const CUT_OFF_CONTINUE_PROMPT: &str = "<system-reminder>\nYour previous reply was cut off at the \
     output-token limit. Continue exactly where it stopped, without repeating what you already \
     wrote. Keep each reply and each tool call smaller.\n</system-reminder>";
const CUT_OFF_TOOL_CALL_RESULT: &str = "This tool call was cut off: the reply reached the \
     output-token limit before the tool input was complete, so it was not run. Send less in \
     one call: for a large file, write the first part with write_file and add the rest in \
     further edit_file calls.";

/// Sent instead of a `read_file` result the session already holds.
const UNCHANGED_READ_NOTE: &str = "[Unchanged: your earlier read of these same lines is still \
     in this conversation, above. Use it instead of reading again; to see other lines, pass a \
     different offset and limit.]";

/// How many times in a row one call (same tool, same input) may return the
/// same result in a turn before it is no longer run. Repeating a call that
/// changed nothing is a loop; `cargo test` after an edit gives a new result
/// and is never stopped.
const REPEATED_CALL_LIMIT: u32 = 3;

const REPEATED_CALL_NOTE: &str = "Not run: you already made this exact call several times this \
     turn and it returned the same result each time. Repeating it will not change anything. \
     Use the result you have, try a different approach, or answer the user with what you \
     found.";

/// Counts how many times in a row the call `key` returned this same result.
fn note_repeated_call(
    repeated_calls: &mut BTreeMap<(String, String), (u64, u32)>,
    key: (String, String),
    message: &ConversationMessage,
) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for block in &message.blocks {
        if let ContentBlock::ToolResult {
            output, is_error, ..
        } = block
        {
            output.hash(&mut hasher);
            is_error.hash(&mut hasher);
        }
    }
    let result = hasher.finish();
    let entry = repeated_calls.entry(key).or_insert((result, 0));
    if entry.0 == result {
        entry.1 += 1;
    } else {
        *entry = (result, 1);
    }
}

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: Vec<String>,
    pub messages: Vec<ConversationMessage>,
}

/// Starts the prompt sent after a reply that ended while still thinking
/// (cut off at the token limit, or stopped with nothing but thinking);
/// clients can switch thinking off for that one request.
pub const THINKING_CUT_OFF_PROMPT: &str =
    "<system-reminder>\nYour previous reply ended while you were still thinking";

impl ApiRequest {
    /// The request continues after a reply cut off mid-thought, or after a
    /// tool call cut off at the token limit. Models that were asked to think
    /// briefly still thought up to the limit again, so a client should send
    /// this request with thinking switched off. A cut-off tool call is
    /// included: Qwen thought 36k characters before an edit, the edit was cut
    /// off, and the retry thought at length again.
    #[must_use]
    pub fn follows_thinking_cut_off(&self) -> bool {
        self.messages.last().is_some_and(|message| {
            message.blocks.iter().any(|block| match block {
                ContentBlock::Text { text } => {
                    message.role == MessageRole::User && text.starts_with(THINKING_CUT_OFF_PROMPT)
                }
                ContentBlock::ToolResult { output, .. } => {
                    output.starts_with(CUT_OFF_TOOL_CALL_RESULT)
                }
                _ => false,
            })
        })
    }
}

/// Streamed events emitted while processing a single assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantEvent {
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    TextDelta(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    Usage(TokenUsage),
    PromptCache(PromptCacheEvent),
    /// The reply stopped at the output-token limit (or a full context
    /// window) rather than because the model was done.
    OutputLimitReached,
    MessageStop,
}

/// Prompt-cache telemetry captured from the provider response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheEvent {
    pub unexpected: bool,
    pub reason: String,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

/// Whether a provider stop reason means the reply hit the output-token
/// limit (Anthropic `max_tokens`, OpenAI-compatible `length`) instead of
/// ending on its own.
#[must_use]
pub fn is_output_limit_stop_reason(stop_reason: &str) -> bool {
    matches!(
        stop_reason,
        "max_tokens" | "length" | "model_context_window_exceeded"
    )
}

/// Minimal streaming API contract required by [`ConversationRuntime`].
pub trait ApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError>;

    /// Asks the model for a compaction summary of `transcript` (see
    /// [`crate::summary_transcript`]) under `system`, without tools and
    /// without showing it. `None` (the default, and for clients that would
    /// rather not spend a request on it) keeps the summary built from the
    /// messages themselves.
    fn summarize(&mut self, _system: &str, _transcript: &str) -> Option<String> {
        None
    }
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
pub trait ToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;

    /// Whether calls to `tool_name` may run at the same time as other such
    /// calls: the tool must only read and share no mutable state.
    fn can_run_in_parallel(&self, _tool_name: &str) -> bool {
        false
    }

    /// Runs calls that are all [`Self::can_run_in_parallel`], returning their
    /// results in the same order. The default runs them one after another.
    fn execute_batch(&mut self, calls: &[(String, String)]) -> Vec<Result<String, ToolError>> {
        calls
            .iter()
            .map(|(tool_name, input)| self.execute(tool_name, input))
            .collect()
    }
}

/// A tool call after its pre-hook and permission check, waiting to run.
struct PreparedToolUse {
    tool_use_id: String,
    tool_name: String,
    effective_input: String,
    pre_hook_result: HookRunResult,
    outcome: PermissionOutcome,
}

/// Splits tool calls into groups that run together: each run of consecutive
/// parallel-safe calls is one group, every other call is a group of its own.
/// Order is preserved, so a write never moves past a read or vice versa.
fn group_for_parallel_execution(
    tool_uses: Vec<(String, String, String)>,
    can_run_in_parallel: impl Fn(&str) -> bool,
) -> Vec<Vec<(String, String, String)>> {
    let mut groups: Vec<Vec<(String, String, String)>> = Vec::new();
    let mut previous_parallel = false;
    for tool_use in tool_uses {
        let parallel = can_run_in_parallel(&tool_use.1);
        match groups.last_mut() {
            Some(group) if parallel && previous_parallel => group.push(tool_use),
            _ => groups.push(vec![tool_use]),
        }
        previous_parallel = parallel;
    }
    groups
}

/// Error returned when a tool invocation fails locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// Error returned when a conversation turn cannot be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Summary of one completed runtime turn, including tool results and usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub prompt_cache_events: Vec<PromptCacheEvent>,
    pub iterations: usize,
    pub usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
}

/// Details about automatic session compaction applied during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
    /// Old tool results cleared before (or instead of) summarizing.
    pub pruned_tool_results: usize,
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    system_prompt: Vec<String>,
    max_iterations: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    auto_compaction_input_tokens_threshold: u32,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<Box<dyn HookProgressReporter>>,
    session_tracer: Option<SessionTracer>,
    /// Pass notices about finished background jobs and agents to the model
    /// between steps. Only the interactive main runtime sets this, so
    /// sub-agents running in other threads do not take its notices.
    deliver_job_notices: bool,
}

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
    ) -> Self {
        Self::new_with_features(
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            &RuntimeFeatureConfig::default(),
        )
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new_with_features(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
        feature_config: &RuntimeFeatureConfig,
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        Self {
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            max_iterations: usize::MAX,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
            deliver_job_notices: false,
        }
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Like [`Self::with_job_notices`], on an existing runtime.
    pub fn set_job_notices(&mut self, deliver: bool) {
        self.deliver_job_notices = deliver;
    }

    /// Delivers background job/agent notices to the model between steps.
    #[must_use]
    pub fn with_job_notices(mut self, deliver: bool) -> Self {
        self.deliver_job_notices = deliver;
        self
    }

    #[must_use]
    pub fn with_auto_compaction_input_tokens_threshold(mut self, threshold: u32) -> Self {
        self.auto_compaction_input_tokens_threshold = threshold;
        self
    }

    /// Update the auto-compaction threshold after construction. This allows the
    /// caller to tune the threshold based on runtime information (e.g., the
    /// server-returned context window size from a 400 error).
    pub fn set_auto_compaction_input_tokens_threshold(&mut self, threshold: u32) {
        self.auto_compaction_input_tokens_threshold = threshold;
    }

    #[must_use]
    pub fn with_hook_abort_signal(mut self, hook_abort_signal: HookAbortSignal) -> Self {
        self.hook_abort_signal = hook_abort_signal;
        self
    }

    #[must_use]
    pub fn with_hook_progress_reporter(
        mut self,
        hook_progress_reporter: Box<dyn HookProgressReporter>,
    ) -> Self {
        self.hook_progress_reporter = Some(hook_progress_reporter);
        self
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.session_tracer = Some(session_tracer);
        self
    }

    /// Runs the pre-tool-use hook and the permission check for one call.
    /// Returns the hook result, the (possibly hook-updated) input and whether
    /// the call may run.
    fn authorize_tool_use(
        &mut self,
        tool_name: &str,
        input: &str,
        prompter: Option<&mut (dyn PermissionPrompter + '_)>,
    ) -> (HookRunResult, String, PermissionOutcome) {
        let pre_hook_result = self.run_pre_tool_use_hook(tool_name, input);
        let effective_input = pre_hook_result
            .updated_input()
            .map_or_else(|| input.to_string(), ToOwned::to_owned);
        let permission_context = PermissionContext::new(
            pre_hook_result.permission_override(),
            pre_hook_result.permission_reason().map(ToOwned::to_owned),
        );

        let hook_stop = if pre_hook_result.is_cancelled() {
            Some("cancelled")
        } else if pre_hook_result.is_failed() {
            Some("failed for")
        } else if pre_hook_result.is_denied() {
            Some("denied")
        } else {
            None
        };
        let outcome = match hook_stop {
            Some(verb) => PermissionOutcome::Deny {
                reason: format_hook_message(
                    &pre_hook_result,
                    &format!("PreToolUse hook {verb} tool `{tool_name}`"),
                ),
            },
            None => self.permission_policy.authorize_with_context(
                tool_name,
                &effective_input,
                &permission_context,
                prompter.map(|prompt| -> &mut dyn PermissionPrompter { prompt }),
            ),
        };
        (pre_hook_result, effective_input, outcome)
    }

    /// Runs the post-tool-use hooks for a call that ran and builds its result.
    fn finish_tool_use(
        &mut self,
        call: PreparedToolUse,
        result: Result<String, ToolError>,
    ) -> ConversationMessage {
        let (output, mut is_error) = match result {
            Ok(output) => (output, false),
            Err(error) => (error.to_string(), true),
        };
        let mut output = merge_hook_feedback(call.pre_hook_result.messages(), output, false);

        let post_hook_result = if is_error {
            self.run_post_tool_use_failure_hook(&call.tool_name, &call.effective_input, &output)
        } else {
            self.run_post_tool_use_hook(&call.tool_name, &call.effective_input, &output, false)
        };
        let post_hook_stopped = post_hook_result.is_denied()
            || post_hook_result.is_failed()
            || post_hook_result.is_cancelled();
        if post_hook_stopped {
            is_error = true;
        }
        output = merge_hook_feedback(post_hook_result.messages(), output, post_hook_stopped);

        let mut message =
            ConversationMessage::tool_result(call.tool_use_id, call.tool_name, output, is_error);
        if !is_error {
            crate::tool_images::attach_tool_images(&mut message);
        }
        message
    }

    fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_failure_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    /// Run a session health probe to verify the runtime is functional after compaction.
    /// Returns Ok(()) if healthy, Err if the session appears broken.
    fn run_session_health_probe(&mut self) -> Result<(), String> {
        // Check if we have basic session integrity
        if self.session.messages.is_empty() && self.session.compaction.is_some() {
            // Freshly compacted with no messages - this is normal
            return Ok(());
        }

        // Verify tool executor is responsive with a non-destructive probe
        // Using glob_search with a pattern that won't match anything
        let probe_input = r#"{"pattern": "*.health-check-probe-"}"#;
        match self.tool_executor.execute("glob_search", probe_input) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Tool executor probe failed: {e}")),
        }
    }

    pub fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        self.run_turn_with_attachments(user_input, Vec::new(), prompter)
    }

    /// Like [`Self::run_turn`], with extra blocks (e.g. images) appended to
    /// the user message.
    pub fn run_turn_with_attachments(
        &mut self,
        user_input: impl Into<String>,
        attachments: Vec<ContentBlock>,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        self.drive_turn(user_input.into(), Some(attachments), prompter)
    }

    /// Picks up a turn whose request is already in the session, e.g. after
    /// it failed on a full context window and the session was compacted:
    /// the work it did is kept and `turn_request` is not sent a second time.
    pub fn continue_turn(
        &mut self,
        turn_request: impl Into<String>,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        self.drive_turn(turn_request.into(), None, prompter)
    }

    /// Runs the agent loop for `user_input`. With `attachments` the request
    /// is a new user message; without, the session already holds it.
    #[allow(clippy::too_many_lines)]
    fn drive_turn(
        &mut self,
        user_input: String,
        attachments: Option<Vec<ContentBlock>>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        // ROADMAP #38: Session-health canary - probe if context was compacted
        if self.session.compaction.is_some() {
            if let Err(error) = self.run_session_health_probe() {
                return Err(RuntimeError::new(format!(
                    "Session health probe failed after compaction: {error}. \
                     The session may be in an inconsistent state. \
                     Consider starting a fresh session with /session new."
                )));
            }
        }

        self.record_turn_started(&user_input);
        let turn_request = user_input.clone();
        if let Some(attachments) = attachments {
            self.session
                .push_user_blocks(user_input, attachments)
                .map_err(|error| RuntimeError::new(error.to_string()))?;
        } else {
            keep_turn_request(&mut self.session, &turn_request);
        }

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        let mut auto_compaction = None;
        let mut cut_off_continuations = 0;
        // (tool, input) -> (hash of its last result, times in a row it was that).
        let mut repeated_calls: BTreeMap<(String, String), (u64, u32)> = BTreeMap::new();

        loop {
            iterations += 1;
            if iterations > self.max_iterations {
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                );
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }

            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: self.session.messages.clone(),
            };
            let events = match self.api_client.stream(request) {
                Ok(events) => events,
                Err(error) => {
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
            };
            let reply_cut_off = events
                .iter()
                .any(|event| matches!(event, AssistantEvent::OutputLimitReached));
            let (assistant_message, usage, turn_prompt_cache_events) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            if let Some(usage) = usage {
                self.usage_tracker.record(usage);
            }
            prompt_cache_events.extend(turn_prompt_cache_events);
            let mut pending_tool_uses = assistant_message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            // A reply cut off at the token limit ends inside its last tool
            // call: that input is unfinished JSON and must not be run.
            let cut_off_tool_use = if reply_cut_off
                && pending_tool_uses.last().is_some_and(|(_, _, input)| {
                    serde_json::from_str::<serde_json::Value>(input).is_err()
                }) {
                pending_tool_uses.pop()
            } else {
                None
            };
            self.record_assistant_iteration(
                iterations,
                &assistant_message,
                pending_tool_uses.len(),
            );
            // A reply cut off while still thinking (no text, no tool call):
            // what it thought so far, so the continuation can pick it up.
            // A reply that stopped with only thinking (no text, no tool
            // call) is handled the same way: Nemotron ended a turn after 19k
            // characters of thinking, with no answer and nothing changed.
            let cut_off_thinking = Self::cut_off_thinking_tail(&assistant_message);
            let reply_cut_off = reply_cut_off || cut_off_thinking.is_some();

            // A reply cut off mid-thought is not kept: its thinking (the whole
            // reply budget, 8k tokens on a 32k local window) went back to the
            // model with every later step of the turn, so the next request
            // overflowed, compaction dropped the files just read, and the
            // model read them again and was cut off again. Its tail goes
            // into the continuation prompt instead.
            let keep_reply = cut_off_thinking.is_none();
            if keep_reply {
                self.session
                    .push_message(assistant_message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
            }
            assistant_messages.push(assistant_message);

            // Run auto-compaction check before next API call, including on the terminal
            // (no-tool) iteration, to prevent unbounded session growth (#3106).
            if let Some(compaction) = self.maybe_auto_compact(&turn_request, keep_reply) {
                auto_compaction = Some(compaction);
            }

            if pending_tool_uses.is_empty() && cut_off_tool_use.is_none() {
                if reply_cut_off && cut_off_continuations < MAX_CUT_OFF_CONTINUATIONS {
                    cut_off_continuations += 1;
                    // After a cut-off in the middle of thinking, a bare "continue"
                    // made models start the same reasoning over (their thinking is
                    // not shown back to them), so they ran into the limit again.
                    let prompt = cut_off_thinking.as_deref().map_or_else(
                        || CUT_OFF_CONTINUE_PROMPT.to_string(),
                        |tail| {
                            format!(
                                "{THINKING_CUT_OFF_PROMPT}, without an answer or a tool call: nothing \
                                 was shown or done. Your thinking \
                                 ended with:\n\n…{tail}\n\nDo not start that reasoning over. Decide now and act \
                                 on it: call the next tool or give the answer, with only a short thought \
                                 first.\n</system-reminder>"
                            )
                        },
                    );
                    self.session
                        .push_message(crate::session::ConversationMessage::user_text(prompt))
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    continue;
                }
                break;
            }
            // The limit is on cut-offs in a row: a long turn that keeps
            // making progress between them is not ended without an answer.
            if !pending_tool_uses.is_empty() {
                cut_off_continuations = 0;
            }

            // Consecutive read-only calls run concurrently; every other call
            // runs alone, in order. Hooks, permission prompts and session writes
            // all stay on this thread, in the original order.
            let groups = group_for_parallel_execution(pending_tool_uses, |tool_name| {
                !tool_name.eq_ignore_ascii_case("bash")
                    && self.tool_executor.can_run_in_parallel(tool_name)
            });
            for group in groups {
                let mut prepared = Vec::with_capacity(group.len());
                for (tool_use_id, tool_name, input) in group {
                    let (pre_hook_result, effective_input, mut outcome) =
                        self.authorize_tool_use(&tool_name, &input, prompter.as_deref_mut());
                    if outcome == PermissionOutcome::Allow
                        && repeated_calls
                            .get(&(tool_name.clone(), effective_input.clone()))
                            .is_some_and(|(_, times)| *times >= REPEATED_CALL_LIMIT)
                    {
                        outcome = PermissionOutcome::Deny {
                            reason: REPEATED_CALL_NOTE.to_string(),
                        };
                    }
                    prepared.push(PreparedToolUse {
                        tool_use_id,
                        tool_name,
                        effective_input,
                        pre_hook_result,
                        outcome,
                    });
                }

                let allowed = prepared
                    .iter()
                    .filter(|call| call.outcome == PermissionOutcome::Allow)
                    .map(|call| (call.tool_name.clone(), call.effective_input.clone()))
                    .collect::<Vec<_>>();
                for (tool_name, _) in &allowed {
                    self.record_tool_started(iterations, tool_name);
                }
                // Esc/Stop ends shell commands this turn is running.
                let _abort_scope = crate::bash::TurnAbortScope::enter(&self.hook_abort_signal);
                let mut results = if allowed.len() > 1 {
                    self.tool_executor.execute_batch(&allowed)
                } else {
                    allowed
                        .iter()
                        .map(|(tool_name, input)| {
                            let _host_scope = crate::bash::HostExecutionScope::for_authorized_call(
                                tool_name, input,
                            );
                            self.tool_executor.execute(tool_name, input)
                        })
                        .collect()
                }
                .into_iter();

                for call in prepared {
                    let result_message = match call.outcome {
                        PermissionOutcome::Allow => {
                            let result = results.next().unwrap_or_else(|| {
                                Err(ToolError::new("tool executor returned no result"))
                            });
                            let key = (call.tool_name.clone(), call.effective_input.clone());
                            let mut message = self.finish_tool_use(call, result);
                            note_repeated_call(&mut repeated_calls, key, &message);
                            self.note_unchanged_read(&mut message);
                            message
                        }
                        PermissionOutcome::Deny { reason } => ConversationMessage::tool_result(
                            call.tool_use_id,
                            call.tool_name,
                            merge_hook_feedback(call.pre_hook_result.messages(), reason, true),
                            true,
                        ),
                    };
                    self.session
                        .push_message(result_message.clone())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    crate::tool_images::drop_old_tool_images(
                        &mut self.session.messages,
                        crate::tool_images::KEEP_TOOL_IMAGES,
                    );
                    self.record_tool_finished(iterations, &result_message);
                    tool_results.push(result_message);
                }
            }
            if let Some((tool_use_id, tool_name, _)) = cut_off_tool_use {
                let result_message = ConversationMessage::tool_result(
                    tool_use_id,
                    tool_name,
                    CUT_OFF_TOOL_CALL_RESULT.to_string(),
                    true,
                );
                self.session
                    .push_message(result_message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                self.record_tool_finished(iterations, &result_message);
                tool_results.push(result_message);
            }

            // Background shells or agents that finished while the tools ran
            // are reported before the next step, so the model can use them.
            if self.deliver_job_notices {
                let notices = crate::global_background_jobs().drain_notices();
                if !notices.is_empty() {
                    self.session
                        .push_message(crate::session::ConversationMessage::user_text(format!(
                            "<system-reminder>\n{}\n</system-reminder>",
                            notices.join("\n")
                        )))
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                }
            }

            // `/btw` answers that came in while the tools ran.
            crate::turn_keys::print_side_answers();
            // Prompts the user queued while the tools ran are passed to the
            // model before its next step, so a running task can be steered.
            for text in crate::turn_keys::take_queued_for_running_turn() {
                crate::turn_keys::announce_sent_to_running_turn(&text);
                self.session
                    .push_message(crate::session::ConversationMessage::user_text(text))
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
            }
        }

        let summary = TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            usage: self.usage_tracker.cumulative_usage(),
            auto_compaction,
        };
        self.record_turn_completed(&summary);

        Ok(summary)
    }

    #[must_use]
    pub fn compact(&self, config: CompactionConfig) -> CompactionResult {
        compact_session(&self.session, config)
    }

    /// [`Self::compact`] with the summary written by the model when the
    /// client offers that ([`ApiClient::summarize`]).
    pub fn compact_with_model_summary(&mut self, config: CompactionConfig) -> CompactionResult {
        let api_client = &mut self.api_client;
        compact_session_with(&self.session, config, &mut |previous, removed| {
            api_client.summarize(
                SUMMARY_SYSTEM_PROMPT,
                &summary_transcript(previous, removed),
            )
        })
    }

    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_session_tokens(&self.session)
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTracker {
        &self.usage_tracker
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn api_client_mut(&mut self) -> &mut C {
        &mut self.api_client
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    #[must_use]
    pub fn fork_session(&self, branch_name: Option<String>) -> Session {
        self.session.fork(branch_name)
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }

    /// Replaces a `read_file` result with a short note when the session
    /// already holds the identical result, unpruned: the file is unchanged
    /// and the model can still see it (hermes answers such reads with "File
    /// unchanged since last read"). Qwen read the same file whole four times
    /// in one turn with the first copy still in its context, each time
    /// adding 4k tokens to a 32k window.
    fn note_unchanged_read(&self, message: &mut ConversationMessage) {
        const MIN_CHARS: usize = 600;
        for block in &mut message.blocks {
            let ContentBlock::ToolResult {
                tool_name,
                output,
                is_error: false,
                ..
            } = block
            else {
                continue;
            };
            if !matches!(tool_name.as_str(), "read_file" | "Read") || output.len() < MIN_CHARS {
                continue;
            }
            let seen = self.session.messages.iter().any(|earlier| {
                earlier.blocks.iter().any(|earlier_block| {
                    matches!(earlier_block, ContentBlock::ToolResult { output: earlier_output, is_error: false, .. }
                        if earlier_output == output)
                })
            });
            if seen {
                *output = UNCHANGED_READ_NOTE.to_string();
            }
        }
    }

    /// The last ~1500 characters of a reply's thinking when the reply has no
    /// text and no tool call (it was cut off mid-thought).
    fn cut_off_thinking_tail(message: &ConversationMessage) -> Option<String> {
        let mut thinking = String::new();
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } if !text.trim().is_empty() => return None,
                ContentBlock::ToolUse { .. } => return None,
                ContentBlock::Thinking { thinking: part, .. } => thinking.push_str(part),
                _ => {}
            }
        }
        let thinking = thinking.trim();
        if thinking.is_empty() {
            return None;
        }
        let skip = thinking.chars().count().saturating_sub(1_500);
        Some(thinking.chars().skip(skip).collect())
    }

    /// Compacts when the last request filled the context past the
    /// threshold. `turn_request` is the prompt of the running turn: if it is
    /// summarized away, it is kept word for word in the summary message, so
    /// the model still knows what it is in the middle of doing.
    /// `reply_kept`: the last reply is in the session (a reply cut off
    /// while thinking is not, so its tokens do not count).
    fn maybe_auto_compact(
        &mut self,
        turn_request: &str,
        reply_kept: bool,
    ) -> Option<AutoCompactionEvent> {
        // What the last request sent, cached or not. The cumulative total
        // counts the context again for every step of every turn, so it
        // crossed the threshold long before the context was full, and once
        // crossed it compacted after every step.
        let latest = self.usage_tracker.current_turn_usage();
        // The reply is in the session now too, so it counts toward the next
        // request: a long reply (a whole file written) can by itself push
        // that request past a small window.
        let context_tokens = latest
            .input_tokens
            .saturating_add(latest.cache_creation_input_tokens)
            .saturating_add(latest.cache_read_input_tokens)
            .saturating_add(if reply_kept { latest.output_tokens } else { 0 });
        let threshold = self.auto_compaction_input_tokens_threshold;
        if context_tokens < threshold {
            return None;
        }

        // What the request spends outside the session (system prompt, tool
        // definitions). The compacted session has to land well under the
        // threshold: keeping a fixed 4 messages left a small window above
        // it right after compacting (two file reads plus a ~13k prompt), so
        // every step compacted again, the model saw only its last two calls
        // plus the re-attached request, and started the task over.
        let session_tokens =
            u32::try_from(estimate_session_tokens(&self.session)).unwrap_or(u32::MAX);
        let overhead = context_tokens.saturating_sub(session_tokens);
        let target = overhead + threshold.saturating_sub(overhead) / 2;
        let size = |session: &Session| {
            overhead
                .saturating_add(u32::try_from(estimate_session_tokens(session)).unwrap_or(u32::MAX))
        };

        // First the cheap step: clear old tool output. When that alone gets
        // under the target, no messages are dropped and no summary is
        // written (no extra request, nothing lost but stale output).
        let mut pruned_session = self.session.clone();
        let pruned = prune_old_tool_results(&mut pruned_session, 8);
        if pruned > 0 && size(&pruned_session) <= target {
            self.session = pruned_session;
            return Some(AutoCompactionEvent {
                removed_message_count: 0,
                pruned_tool_results: pruned,
            });
        }

        // When the prompt alone (nearly) fills the threshold no compaction
        // gets under it: keep the usual 4 messages rather than strip the
        // session bare.
        let room = threshold.saturating_sub(overhead);
        let schedule: &[usize] = if room >= threshold / 8 {
            &[4, 2, 1]
        } else {
            &[4]
        };
        let mut compacted = None;
        let mut kept = schedule[0];
        for &preserve_recent_messages in schedule {
            let result = compact_session(
                &self.session,
                CompactionConfig {
                    preserve_recent_messages,
                    max_estimated_tokens: 0,
                },
            );
            if result.removed_message_count == 0 {
                continue;
            }
            let fits = size(&result.compacted_session) <= target;
            compacted = Some(result);
            kept = preserve_recent_messages;
            if fits {
                break;
            }
        }
        let mut result = compacted?;
        // The cut is chosen; now let the model write the summary for it, if
        // the client offers that (one request, only for the chosen cut).
        let api_client = &mut self.api_client;
        let written = compact_session_with(
            &self.session,
            CompactionConfig {
                preserve_recent_messages: kept,
                max_estimated_tokens: 0,
            },
            &mut |previous, removed| {
                api_client.summarize(
                    SUMMARY_SYSTEM_PROMPT,
                    &summary_transcript(previous, removed),
                )
            },
        );
        if written.removed_message_count > 0 {
            result = written;
        }
        if size(&result.compacted_session) > target {
            // The kept messages hold large tool output: cut it to an even
            // share of the room left (~4 characters per token).
            let room_chars = target.saturating_sub(overhead) as usize * 4;
            truncate_large_tool_results(&mut result.compacted_session, (room_chars / 2).max(2_000));
        }

        self.session = result.compacted_session;
        keep_turn_request(&mut self.session, turn_request);
        Some(AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
            pruned_tool_results: 0,
        })
    }

    fn record_turn_started(&self, user_input: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "user_input".to_string(),
            Value::String(user_input.to_string()),
        );
        session_tracer.record("turn_started", attributes);
    }

    fn record_assistant_iteration(
        &self,
        iteration: usize,
        assistant_message: &ConversationMessage,
        pending_tool_use_count: usize,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "assistant_blocks".to_string(),
            Value::from(assistant_message.blocks.len() as u64),
        );
        attributes.insert(
            "pending_tool_use_count".to_string(),
            Value::from(pending_tool_use_count as u64),
        );
        session_tracer.record("assistant_iteration_completed", attributes);
    }

    fn record_tool_started(&self, iteration: usize, tool_name: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        session_tracer.record("tool_execution_started", attributes);
    }

    fn record_tool_finished(&self, iteration: usize, result_message: &ConversationMessage) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let Some(ContentBlock::ToolResult {
            tool_name,
            is_error,
            ..
        }) = result_message.blocks.first()
        else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("tool_name".to_string(), Value::String(tool_name.clone()));
        attributes.insert("is_error".to_string(), Value::Bool(*is_error));
        session_tracer.record("tool_execution_finished", attributes);
    }

    fn record_turn_completed(&self, summary: &TurnSummary) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "iterations".to_string(),
            Value::from(summary.iterations as u64),
        );
        attributes.insert(
            "assistant_messages".to_string(),
            Value::from(summary.assistant_messages.len() as u64),
        );
        attributes.insert(
            "tool_results".to_string(),
            Value::from(summary.tool_results.len() as u64),
        );
        attributes.insert(
            "prompt_cache_events".to_string(),
            Value::from(summary.prompt_cache_events.len() as u64),
        );
        session_tracer.record("turn_completed", attributes);
    }

    fn record_turn_failed(&self, iteration: usize, error: &RuntimeError) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("turn_failed", attributes);
    }
}

/// Adds the running turn's prompt to the summary message when compaction
/// removed it.
fn keep_turn_request(session: &mut Session, turn_request: &str) {
    if turn_request.trim().is_empty() {
        return;
    }
    let still_there = session.messages.iter().any(|message| {
        message.role == MessageRole::User
            && message
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == turn_request))
    });
    if still_there {
        return;
    }
    let Some(ContentBlock::Text { text }) = session
        .messages
        .first_mut()
        .filter(|message| message.role == MessageRole::System)
        .and_then(|message| message.blocks.first_mut())
    else {
        return;
    };
    text.push_str(
        "\n\nThe user's current request, which you are in the middle of and must finish:\n",
    );
    text.push_str(turn_request);
}

static AUTO_COMPACTION_OVERRIDE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// Sets the automatic compaction threshold for runtimes created from now on
/// (`/autocompact`); `None` goes back to the environment or the default.
pub fn set_auto_compaction_threshold_override(tokens: Option<u32>) {
    AUTO_COMPACTION_OVERRIDE.store(tokens.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
}

/// Reads the automatic compaction threshold: the `/autocompact` override,
/// else the environment, else the default.
#[must_use]
pub fn auto_compaction_threshold_from_env() -> u32 {
    configured_auto_compaction_threshold().unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

/// The threshold the user chose (`/autocompact`, or the environment), if
/// any. Without one, a caller that knows the model's window can derive a
/// better threshold than the default.
#[must_use]
pub fn configured_auto_compaction_threshold() -> Option<u32> {
    let set = AUTO_COMPACTION_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
    if set > 0 {
        return Some(set);
    }
    parse_configured_threshold(
        std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR)
            .ok()
            .as_deref(),
    )
}

fn parse_configured_threshold(value: Option<&str>) -> Option<u32> {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|threshold| *threshold > 0)
}

#[cfg(test)]
fn parse_auto_compaction_threshold(value: Option<&str>) -> u32 {
    parse_configured_threshold(value).unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<
    (
        ConversationMessage,
        Option<TokenUsage>,
        Vec<PromptCacheEvent>,
    ),
    RuntimeError,
> {
    let mut text = String::new();
    let mut blocks = Vec::new();
    let mut prompt_cache_events = Vec::new();
    let mut finished = false;
    let mut usage = None;

    for event in events {
        match event {
            AssistantEvent::Thinking {
                thinking,
                signature,
            } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature,
                });
            }
            AssistantEvent::TextDelta(delta) => text.push_str(&delta),
            AssistantEvent::ToolUse { id, name, input } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
            AssistantEvent::Usage(value) => usage = Some(value),
            AssistantEvent::PromptCache(event) => prompt_cache_events.push(event),
            AssistantEvent::OutputLimitReached => {}
            AssistantEvent::MessageStop => {
                finished = true;
            }
        }
    }

    flush_text_block(&mut text, &mut blocks);

    if !finished {
        return Err(RuntimeError::new(
            "assistant stream ended without a message stop event",
        ));
    }
    if blocks.is_empty() {
        return Err(RuntimeError::new("assistant stream produced no content"));
    }

    Ok((
        ConversationMessage::assistant_with_usage(blocks, usage),
        usage,
        prompt_cache_events,
    ))
}

fn flush_text_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text),
        });
    }
}

fn format_hook_message(result: &HookRunResult, fallback: &str) -> String {
    if result.messages().is_empty() {
        fallback.to_string()
    } else {
        result.messages().join("\n")
    }
}

fn merge_hook_feedback(messages: &[String], output: String, is_error: bool) -> String {
    if messages.is_empty() {
        return output;
    }

    let mut sections = Vec::new();
    if !output.trim().is_empty() {
        sections.push(output);
    }
    let label = if is_error {
        "Hook feedback (error)"
    } else {
        "Hook feedback"
    };
    sections.push(format!("{label}:\n{}", messages.join("\n")));
    sections.join("\n\n")
}

type ToolHandler = Box<dyn FnMut(&str) -> Result<String, ToolError>>;

/// Simple in-memory tool executor for tests and lightweight integrations.
#[derive(Default)]
pub struct StaticToolExecutor {
    handlers: BTreeMap<String, ToolHandler>,
}

impl StaticToolExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn register(
        mut self,
        tool_name: impl Into<String>,
        handler: impl FnMut(&str) -> Result<String, ToolError> + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Box::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get_mut(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_assistant_message, parse_auto_compaction_threshold, ApiClient, ApiRequest,
        AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RuntimeError,
        StaticToolExecutor, ToolExecutor, DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPromptBuilder};
    use crate::session::{ContentBlock, MessageRole, Session};
    use crate::usage::TokenUsage;
    use crate::ToolError;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use telemetry::{MemoryTelemetrySink, SessionTracer, TelemetryEvent};

    struct ScriptedApiClient {
        call_count: usize,
    }

    impl ApiClient for ScriptedApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            match self.call_count {
                1 => {
                    assert!(request
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::User));
                    Ok(vec![
                        AssistantEvent::TextDelta("Let me calculate that.".to_string()),
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: "2,2".to_string(),
                        },
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 6,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 2,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                2 => {
                    let last_message = request
                        .messages
                        .last()
                        .expect("tool result should be present");
                    assert_eq!(last_message.role, MessageRole::Tool);
                    Ok(vec![
                        AssistantEvent::TextDelta("The answer is 4.".to_string()),
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 24,
                            output_tokens: 4,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 3,
                        }),
                        AssistantEvent::PromptCache(PromptCacheEvent {
                            unexpected: true,
                            reason:
                                "cache read tokens dropped while prompt fingerprint remained stable"
                                    .to_string(),
                            previous_cache_read_input_tokens: 6_000,
                            current_cache_read_input_tokens: 1_000,
                            token_drop: 5_000,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                _ => unreachable!("extra API call"),
            }
        }
    }

    struct PromptAllowOnce;

    impl PermissionPrompter for PromptAllowOnce {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "add");
            PermissionPromptDecision::Allow
        }
    }

    /// Asks for read, read, write, read in one reply, then finishes.
    struct MixedToolCallsApiClient {
        call_count: usize,
    }

    impl ApiClient for MixedToolCallsApiClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            if self.call_count > 1 {
                return Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]);
            }
            let call = |id: &str, name: &str| AssistantEvent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: id.to_string(),
            };
            Ok(vec![
                call("a", "read"),
                call("b", "read"),
                call("c", "write"),
                call("d", "read"),
                AssistantEvent::MessageStop,
            ])
        }
    }

    /// Records how calls reach the executor: batches versus single calls.
    #[derive(Default)]
    struct RecordingExecutor {
        log: Vec<String>,
    }

    impl ToolExecutor for RecordingExecutor {
        fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
            self.log.push(format!("single {tool_name}:{input}"));
            Ok(format!("{tool_name} {input}"))
        }

        fn can_run_in_parallel(&self, tool_name: &str) -> bool {
            tool_name == "read"
        }

        fn execute_batch(&mut self, calls: &[(String, String)]) -> Vec<Result<String, ToolError>> {
            let ids = calls
                .iter()
                .map(|(_, input)| input.as_str())
                .collect::<Vec<_>>();
            self.log.push(format!("batch {}", ids.join(",")));
            calls
                .iter()
                .map(|(tool_name, input)| Ok(format!("{tool_name} {input}")))
                .collect()
        }
    }

    struct HostCallClient {
        sent: bool,
    }
    impl ApiClient for HostCallClient {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            if std::mem::replace(&mut self.sent, true) {
                return Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]);
            }
            Ok(vec![
                AssistantEvent::ToolUse {
                    id: "host".to_string(),
                    name: "bash".to_string(),
                    input: r#"{"command":"printf host-approved","dangerouslyDisableSandbox":true}"#
                        .to_string(),
                },
                AssistantEvent::MessageStop,
            ])
        }
    }

    struct ApproveHost;
    impl PermissionPrompter for ApproveHost {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "bash");
            assert!(request
                .reason
                .as_ref()
                .unwrap()
                .contains("outside Claw's sandbox"));
            PermissionPromptDecision::Allow
        }
    }

    #[test]
    fn host_executor_only_receives_approved_calls() {
        for approved in [false, true] {
            let executor = StaticToolExecutor::new().register("bash", |input| {
                let input = serde_json::from_str(input).unwrap();
                crate::execute_bash(input)
                    .map(|output| output.stdout)
                    .map_err(|error| ToolError::new(error.to_string()))
            });
            let mut runtime = ConversationRuntime::new(
                Session::new(),
                HostCallClient { sent: false },
                executor,
                PermissionPolicy::new(PermissionMode::Allow),
                vec![],
            );
            let mut prompt = ApproveHost;
            let prompter = if approved {
                Some(&mut prompt as &mut dyn PermissionPrompter)
            } else {
                None
            };
            let summary = runtime.run_turn("test host execution", prompter).unwrap();
            match &summary.tool_results[0].blocks[0] {
                ContentBlock::ToolResult {
                    is_error, output, ..
                } => {
                    assert_eq!(*is_error, !approved);
                    if approved {
                        assert_eq!(output, "host-approved");
                    }
                }
                other => panic!("unexpected result: {other:?}"),
            }
        }
    }

    #[test]
    fn consecutive_parallel_safe_calls_run_as_a_batch_and_keep_their_order() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            MixedToolCallsApiClient { call_count: 0 },
            RecordingExecutor::default(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("go", None)
            .expect("conversation loop should succeed");

        assert_eq!(
            runtime.tool_executor.log,
            vec!["batch a,b", "single write:c", "single read:d"],
            "the write splits the reads; a lone read runs on its own"
        );
        let order = summary
            .tool_results
            .iter()
            .map(|message| match &message.blocks[0] {
                ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
                other => panic!("unexpected block {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(order, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn denied_calls_in_a_parallel_group_are_not_executed() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            MixedToolCallsApiClient { call_count: 0 },
            RecordingExecutor::default(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess).with_permission_rules(
                &crate::config::RuntimePermissionRuleConfig::new(
                    Vec::new(),
                    vec!["read(b)".to_string()],
                    Vec::new(),
                    Vec::new(),
                ),
            ),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("go", None)
            .expect("conversation loop should succeed");

        assert_eq!(
            runtime.tool_executor.log,
            vec!["single read:a", "single write:c", "single read:d"]
        );
        let denied = summary
            .tool_results
            .iter()
            .filter(|message| {
                matches!(
                    &message.blocks[0],
                    ContentBlock::ToolResult { is_error: true, .. }
                )
            })
            .count();
        assert_eq!(denied, 1);
        assert_eq!(summary.tool_results.len(), 4);
    }

    /// Runs one tool, then checks that the user's queued prompt reached the
    /// model before its next step.
    struct SteeredApiClient {
        call_count: usize,
    }

    impl ApiClient for SteeredApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                return Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "t1".to_string(),
                        name: "read".to_string(),
                        input: "x".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ]);
            }
            let last = request.messages.last().expect("messages");
            assert_eq!(last.role, MessageRole::User);
            assert!(matches!(
                &last.blocks[0],
                ContentBlock::Text { text } if text == "also check y"
            ));
            Ok(vec![
                AssistantEvent::TextDelta("checked both".to_string()),
                AssistantEvent::MessageStop,
            ])
        }
    }

    #[test]
    fn prompts_queued_during_a_turn_reach_the_model_between_tool_calls() {
        let _turn = crate::turn_keys::simulate_queued_during_turn("also check y");
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SteeredApiClient { call_count: 0 },
            RecordingExecutor::default(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("check x", None)
            .expect("conversation loop should succeed");

        assert_eq!(runtime.api_client.call_count, 2);
        let roles = runtime
            .session()
            .messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::Tool,
                MessageRole::User,
                MessageRole::Assistant
            ]
        );
    }

    #[test]
    fn runs_user_to_tool_to_result_loop_end_to_end_and_tracks_usage() {
        let api_client = ScriptedApiClient { call_count: 0 };
        let tool_executor = StaticToolExecutor::new().register("add", |input| {
            let total = input
                .split(',')
                .map(|part| part.parse::<i32>().expect("input must be valid integer"))
                .sum::<i32>();
            Ok(total.to_string())
        });
        let permission_policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite);
        let system_prompt = SystemPromptBuilder::new()
            .with_project_context(ProjectContext {
                cwd: PathBuf::from("/tmp/project"),
                current_date: "2026-03-31".to_string(),
                git_status: None,
                git_checked: false,
                git_diff: None,
                git_context: None,
                instruction_files: Vec::new(),
            })
            .with_os("linux", "6.8")
            .build();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
        );

        let summary = runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(summary.prompt_cache_events.len(), 1);
        assert_eq!(runtime.session().messages.len(), 4);
        assert_eq!(summary.usage.output_tokens, 10);
        assert_eq!(summary.auto_compaction, None);
        assert!(matches!(
            runtime.session().messages[1].blocks[1],
            ContentBlock::ToolUse { .. }
        ));
        assert!(matches!(
            runtime.session().messages[2].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn records_runtime_session_trace_events() {
        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-runtime", sink.clone());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        )
        .with_session_tracer(tracer);

        runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        let events = sink.events();
        let trace_names = events
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) => Some(trace.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(trace_names.contains(&"turn_started"));
        assert!(trace_names.contains(&"assistant_iteration_completed"));
        assert!(trace_names.contains(&"tool_execution_started"));
        assert!(trace_names.contains(&"tool_execution_finished"));
        assert!(trace_names.contains(&"turn_completed"));
    }

    #[test]
    fn records_denied_tool_results_when_prompt_rejects() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("I could not use the tool.".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("use the tool", Some(&mut RejectPrompter))
            .expect("conversation should continue after denied tool");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "not now"
        ));
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_blocks() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook denies")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'blocked by hook'; exit 2")],
                Vec::new(),
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook denial");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook denial should produce an error result: {output}"
        );
        assert!(
            output.contains("denied tool") || output.contains("blocked by hook"),
            "unexpected hook denial output: {output:?}"
        );
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_fails() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("failed".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook fails")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'broken hook'; exit 1")],
                Vec::new(),
                Vec::new(),
            )),
        );

        // when
        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook failure");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook failure should produce an error result: {output}"
        );
        assert!(
            output.contains("exited with status 1") || output.contains("broken hook"),
            "unexpected hook failure output: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: r#"{"lhs":2,"rhs":2}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'pre hook ran'")],
                vec![shell_snippet("printf 'post hook ran'")],
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use add", None)
            .expect("tool loop succeeds");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            !*is_error,
            "post hook should preserve non-error result: {output:?}"
        );
        assert!(
            output.contains('4'),
            "tool output missing value: {output:?}"
        );
        assert!(
            output.contains("pre hook ran"),
            "tool output missing pre hook feedback: {output:?}"
        );
        assert!(
            output.contains("post hook ran"),
            "tool output missing post hook feedback: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_use_failure_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "fail".to_string(),
                            input: r#"{"path":"README.md"}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new()
                .register("fail", |_input| Err(ToolError::new("tool exploded"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                Vec::new(),
                vec![shell_snippet("printf 'post hook should not run'")],
                vec![shell_snippet("printf 'failure hook ran'")],
            )),
        );

        // when
        let summary = runtime
            .run_turn("use fail", None)
            .expect("tool loop succeeds");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "failure hook path should preserve error result: {output:?}"
        );
        assert!(
            output.contains("tool exploded"),
            "tool output missing failure reason: {output:?}"
        );
        assert!(
            output.contains("failure hook ran"),
            "tool output missing failure hook feedback: {output:?}"
        );
        assert!(
            !output.contains("post hook should not run"),
            "normal post hook should not run on tool failure: {output:?}"
        );
    }

    #[test]
    fn reconstructs_usage_tracker_from_restored_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session
            .messages
            .push(crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "earlier".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                }),
            ));

        let runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
    }

    /// Replays one scripted reply per request and records the requests.
    struct CutOffApi {
        replies: Vec<Vec<AssistantEvent>>,
        requests: Vec<ApiRequest>,
    }

    impl ApiClient for CutOffApi {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.requests.push(request);
            Ok(self.replies.remove(0))
        }
    }

    #[test]
    fn a_call_that_keeps_returning_the_same_result_stops_running() {
        let call = |id: &str, command: &str| {
            vec![
                AssistantEvent::ToolUse {
                    id: id.to_string(),
                    name: "bash".to_string(),
                    input: format!(r#"{{"command":"{command}"}}"#),
                },
                AssistantEvent::MessageStop,
            ]
        };
        // `ls` four times with the same result; `cargo test` four times with
        // a new result each time (an edit between runs).
        let mut replies: Vec<_> = (0..4).map(|i| call(&format!("l{i}"), "ls")).collect();
        replies.extend((0..4).map(|i| call(&format!("t{i}"), "cargo test")));
        replies.push(vec![
            AssistantEvent::TextDelta("done".to_string()),
            AssistantEvent::MessageStop,
        ]);
        let runs = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = runs.clone();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CutOffApi {
                replies,
                requests: Vec::new(),
            },
            StaticToolExecutor::new().register("bash", move |input| {
                seen.borrow_mut().push(input.to_string());
                let count = seen.borrow().len();
                Ok(if input.contains("ls") {
                    "a b".to_string()
                } else {
                    format!("{count} passed")
                })
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime.run_turn("fix it", None).expect("turn runs");

        let ls_runs = runs.borrow().iter().filter(|i| i.contains("ls")).count();
        assert_eq!(ls_runs, super::REPEATED_CALL_LIMIT as usize);
        assert_eq!(runs.borrow().len(), super::REPEATED_CALL_LIMIT as usize + 4);
        let stopped = summary.tool_results.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { output, is_error: true, .. }
                    if output.contains("Not run: you already made this exact call"))
            })
        });
        assert!(stopped);
    }

    #[test]
    fn continues_a_reply_cut_off_at_the_output_limit() {
        let api = CutOffApi {
            replies: vec![
                vec![
                    AssistantEvent::TextDelta("first half".to_string()),
                    AssistantEvent::OutputLimitReached,
                    AssistantEvent::MessageStop,
                ],
                vec![
                    AssistantEvent::TextDelta(" second half".to_string()),
                    AssistantEvent::MessageStop,
                ],
            ],
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime.run_turn("write it", None).expect("turn runs");

        assert_eq!(summary.assistant_messages.len(), 2);
        let continue_request = &runtime.api_client_mut().requests[1];
        let last = continue_request.messages.last().expect("a message");
        assert_eq!(last.role, MessageRole::User);
        assert!(matches!(
            &last.blocks[0],
            ContentBlock::Text { text } if text.contains("cut off at the output-token limit")
        ));
    }

    #[test]
    fn a_reply_cut_off_while_thinking_is_not_sent_back() {
        let thought = "x".repeat(40_000);
        let api = CutOffApi {
            replies: vec![
                vec![
                    AssistantEvent::Thinking {
                        thinking: thought.clone(),
                        signature: None,
                    },
                    AssistantEvent::OutputLimitReached,
                    AssistantEvent::MessageStop,
                ],
                vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ],
            ],
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime.run_turn("fix it", None).expect("turn runs");

        // Only the tail of the thinking goes back, in the continuation prompt.
        let continue_request = &runtime.api_client_mut().requests[1];
        let sent = continue_request
            .messages
            .iter()
            .flat_map(|message| &message.blocks)
            .map(|block| match block {
                ContentBlock::Thinking { thinking, .. } => thinking.len(),
                ContentBlock::Text { text } => text.len(),
                _ => 0,
            })
            .sum::<usize>();
        assert!(sent < 3_000, "sent {sent} characters");
        assert!(continue_request
            .messages
            .iter()
            .all(|message| message.role != MessageRole::Assistant));
        assert!(continue_request.follows_thinking_cut_off());
        assert!(!runtime.api_client_mut().requests[0].follows_thinking_cut_off());
    }

    #[test]
    fn a_reply_with_only_thinking_is_answered_with_a_nudge() {
        let api = CutOffApi {
            replies: vec![
                vec![
                    AssistantEvent::Thinking {
                        thinking: "the blur pass never clears its buffers".to_string(),
                        signature: None,
                    },
                    AssistantEvent::MessageStop,
                ],
                vec![
                    AssistantEvent::TextDelta("fixed".to_string()),
                    AssistantEvent::MessageStop,
                ],
            ],
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime.run_turn("fix it", None).expect("turn runs");

        // The turn did not end on the thinking: the model was asked to act.
        assert_eq!(summary.assistant_messages.len(), 2);
        let requests = &runtime.api_client_mut().requests;
        assert_eq!(requests.len(), 2);
        assert!(requests[1].follows_thinking_cut_off());
        let ContentBlock::Text { text } = &requests[1].messages.last().unwrap().blocks[0] else {
            panic!("a text nudge");
        };
        assert!(text.contains("never clears its buffers"), "{text}");
    }

    #[test]
    fn reading_the_same_lines_again_gets_a_short_note() {
        let read = |id: &str, path: &str| {
            vec![
                AssistantEvent::ToolUse {
                    id: id.to_string(),
                    name: "read_file".to_string(),
                    input: format!(r#"{{"path":"{path}"}}"#),
                },
                AssistantEvent::MessageStop,
            ]
        };
        let api = CutOffApi {
            replies: vec![
                read("t1", "a"),
                read("t2", "b"),
                read("t3", "a"),
                vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ],
            ],
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api,
            StaticToolExecutor::new().register("read_file", |input| {
                let path = if input.contains("\"a\"") { "a" } else { "b" };
                Ok(format!(
                    "[{path}: lines 1-100 of 100]\n{}",
                    path.repeat(2_000)
                ))
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime.run_turn("read", None).expect("turn runs");

        let outputs = summary
            .tool_results
            .iter()
            .map(|message| match &message.blocks[0] {
                ContentBlock::ToolResult { output, .. } => output.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>();
        assert!(outputs[0].starts_with("[a:"));
        assert!(outputs[1].starts_with("[b:"));
        assert_eq!(outputs[2], super::UNCHANGED_READ_NOTE);
    }

    #[test]
    fn cut_offs_count_only_in_a_row() {
        let cut_off = || {
            vec![
                AssistantEvent::TextDelta("more".to_string()),
                AssistantEvent::OutputLimitReached,
                AssistantEvent::MessageStop,
            ]
        };
        let mut replies: Vec<_> = (0..super::MAX_CUT_OFF_CONTINUATIONS)
            .map(|_| cut_off())
            .collect();
        replies.push(vec![
            AssistantEvent::ToolUse {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                input: r#"{"path":"a"}"#.to_string(),
            },
            AssistantEvent::MessageStop,
        ]);
        replies.extend((0..super::MAX_CUT_OFF_CONTINUATIONS).map(|_| cut_off()));
        replies.push(vec![
            AssistantEvent::TextDelta("fixed".to_string()),
            AssistantEvent::MessageStop,
        ]);
        let api = CutOffApi {
            replies,
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api,
            StaticToolExecutor::new().register("read_file", |_input| Ok("text".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime.run_turn("fix it", None).expect("turn runs");

        // Progress between cut-offs resets the count, so the turn still
        // reaches its answer.
        assert_eq!(
            summary.assistant_messages.len(),
            2 * super::MAX_CUT_OFF_CONTINUATIONS + 2
        );
    }

    #[test]
    fn stops_continuing_cut_off_replies_after_the_limit() {
        let cut_off = || {
            vec![
                AssistantEvent::TextDelta("more".to_string()),
                AssistantEvent::OutputLimitReached,
                AssistantEvent::MessageStop,
            ]
        };
        let api = CutOffApi {
            replies: (0..=super::MAX_CUT_OFF_CONTINUATIONS)
                .map(|_| cut_off())
                .collect(),
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime.run_turn("write it", None).expect("turn ends");

        assert_eq!(
            summary.assistant_messages.len(),
            super::MAX_CUT_OFF_CONTINUATIONS + 1
        );
    }

    #[test]
    fn does_not_run_a_tool_call_cut_off_at_the_output_limit() {
        let ran = std::rc::Rc::new(std::cell::Cell::new(0));
        let ran_in_tool = ran.clone();
        let api = CutOffApi {
            replies: vec![
                vec![
                    AssistantEvent::ToolUse {
                        id: "t1".to_string(),
                        name: "write_file".to_string(),
                        input: r##"{"path":"a.md","content":"# Tit"##.to_string(),
                    },
                    AssistantEvent::OutputLimitReached,
                    AssistantEvent::MessageStop,
                ],
                vec![
                    AssistantEvent::TextDelta("ok".to_string()),
                    AssistantEvent::MessageStop,
                ],
            ],
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api,
            StaticToolExecutor::new().register("write_file", move |_input| {
                ran_in_tool.set(ran_in_tool.get() + 1);
                Ok("written".to_string())
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime.run_turn("write it", None).expect("turn runs");

        assert_eq!(ran.get(), 0);
        assert_eq!(summary.tool_results.len(), 1);
        // The retry goes without thinking, so the call fits this time.
        assert!(runtime.api_client_mut().requests[1].follows_thinking_cut_off());
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { output, is_error: true, .. }
                if output.contains("tool call was cut off")
        ));
    }

    #[test]
    fn auto_compaction_counts_the_reply_tokens() {
        let mut session = Session::new();
        for index in 0..6 {
            session
                .push_message(crate::session::ConversationMessage::user_text(format!(
                    "question {index}"
                )))
                .expect("push");
            session
                .push_message(crate::session::ConversationMessage::assistant(vec![
                    ContentBlock::Text {
                        text: format!("answer {index}"),
                    },
                ]))
                .expect("push");
        }
        let api = CutOffApi {
            replies: vec![vec![
                AssistantEvent::TextDelta("long reply".to_string()),
                AssistantEvent::Usage(TokenUsage {
                    input_tokens: 600,
                    output_tokens: 500,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                }),
                AssistantEvent::MessageStop,
            ]],
            requests: Vec::new(),
        };
        let mut runtime = ConversationRuntime::new(
            session,
            api,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(1_000);

        let summary = runtime.run_turn("next", None).expect("turn runs");

        assert!(summary.auto_compaction.is_some());
    }

    #[test]
    fn compacts_session_after_turns() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        runtime.run_turn("a", None).expect("turn a");
        runtime.run_turn("b", None).expect("turn b");
        runtime.run_turn("c", None).expect("turn c");

        let result = runtime.compact(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        });
        assert!(result.summary.contains("Conversation summary"));
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert_eq!(
            result.compacted_session.session_id,
            runtime.session().session_id
        );
        assert!(result.compacted_session.compaction.is_some());
    }

    #[test]
    fn persists_conversation_turn_messages_to_jsonl_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("persisted-turn");
        let session = Session::new().with_persistence_path(path.clone());
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("persist this turn", None)
            .expect("turn should succeed");

        let restored = Session::load_from_path(&path).expect("persisted session should reload");
        fs::remove_file(&path).expect("temp session file should be removable");

        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].role, MessageRole::User);
        assert_eq!(restored.messages[1].role, MessageRole::Assistant);
        assert_eq!(restored.session_id, runtime.session().session_id);
    }

    #[test]
    fn forks_runtime_session_without_mutating_original() {
        let mut session = Session::new();
        session
            .push_user_text("branch me")
            .expect("message should append");

        let runtime = ConversationRuntime::new(
            session.clone(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let forked = runtime.fork_session(Some("alt-path".to_string()));

        assert_eq!(forked.messages, session.messages);
        assert_ne!(forked.session_id, session.session_id);
        assert_eq!(
            forked
                .fork
                .as_ref()
                .map(|fork| (fork.parent_session_id.as_str(), fork.branch_name.as_deref())),
            Some((session.session_id.as_str(), Some("alt-path")))
        );
        assert!(runtime.session().fork.is_none());
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-conversation-{label}-{nanos}.json"))
    }

    #[cfg(windows)]
    fn shell_snippet(script: &str) -> String {
        script.replace('\'', "\"")
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[test]
    fn auto_compaction_uses_the_summary_the_model_writes() {
        struct SummarizingApi {
            transcript: Option<String>,
        }
        impl ApiClient for SummarizingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 120_000,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }

            fn summarize(&mut self, system: &str, transcript: &str) -> Option<String> {
                assert!(system.contains("## Objective"));
                self.transcript = Some(transcript.to_string());
                Some("## Objective\nAdd a blur option to lightcomp.".to_string())
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("add blur to lightcomp"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "reading config.cpp".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];
        let mut runtime = ConversationRuntime::new(
            session,
            SummarizingApi { transcript: None },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime.run_turn("trigger", None).expect("turn");
        assert!(summary.auto_compaction.is_some());
        let first = match &runtime.session().messages[0].blocks[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("unexpected block {other:?}"),
        };
        assert!(first.contains("Add a blur option to lightcomp."), "{first}");
        // The model saw the dropped messages, not the kept ones.
        let transcript = runtime.api_client_mut().transcript.clone().expect("asked");
        assert!(transcript.contains("User: add blur to lightcomp"));
        assert!(transcript.contains("Assistant: reading config.cpp"));
    }

    #[test]
    fn auto_compacts_when_the_last_request_crosses_the_threshold() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 120_000,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("one"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 2,
                pruned_tool_results: 0,
            })
        );
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    /// Calls a tool for the first `tool_steps` requests, then answers; every
    /// request reports `context` input tokens.
    struct ToolLoopApi {
        tool_steps: usize,
        calls: usize,
        context: u32,
    }

    impl ApiClient for ToolLoopApi {
        fn stream(&mut self, _request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.calls += 1;
            let mut events = if self.calls <= self.tool_steps {
                vec![AssistantEvent::ToolUse {
                    id: format!("tool-{}", self.calls),
                    name: "echo".to_string(),
                    input: "{}".to_string(),
                }]
            } else {
                vec![AssistantEvent::TextDelta("done".to_string())]
            };
            events.push(AssistantEvent::Usage(TokenUsage {
                input_tokens: 1_000,
                output_tokens: 4,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: self.context - 1_000,
            }));
            events.push(AssistantEvent::MessageStop);
            Ok(events)
        }
    }

    #[test]
    fn continue_turn_resumes_without_sending_the_request_again() {
        struct ReplyApiClient;
        impl ApiClient for ReplyApiClient {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("finished".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session
            .push_user_text("list the tools")
            .expect("push request");
        session
            .push_message(crate::session::ConversationMessage::tool_result(
                "tool-1",
                "read_file",
                "contents",
                false,
            ))
            .expect("push tool result");
        let mut runtime = ConversationRuntime::new(
            session,
            ReplyApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .continue_turn("list the tools", None)
            .expect("turn should continue");

        let requests = runtime
            .session()
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count();
        assert_eq!(requests, 1);
        assert_eq!(runtime.session().messages.len(), 3);
    }

    /// Like a local server: each request reports its real size, a fixed
    /// prompt overhead plus the history it was sent.
    struct SizedReadLoopApi {
        tool_steps: usize,
        calls: usize,
        overhead: u32,
    }

    impl ApiClient for SizedReadLoopApi {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.calls += 1;
            let mut sent = Session::new();
            sent.messages = request.messages;
            let context = self.overhead + crate::compact::estimate_session_tokens(&sent) as u32;
            let mut events = if self.calls <= self.tool_steps {
                vec![AssistantEvent::ToolUse {
                    id: format!("tool-{}", self.calls),
                    name: "read_file".to_string(),
                    input: format!(r#"{{"path":"/w/main.cpp","offset":{}}}"#, self.calls),
                }]
            } else {
                vec![AssistantEvent::TextDelta("done".to_string())]
            };
            events.push(AssistantEvent::Usage(TokenUsage {
                input_tokens: context,
                output_tokens: 60,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }));
            events.push(AssistantEvent::MessageStop);
            Ok(events)
        }
    }

    #[test]
    fn auto_compaction_does_not_fire_on_every_step_of_a_small_window() {
        let _queue = crate::turn_keys::exclusive_queue();
        // The wd session: a 32k window, ~13.4k tokens of system prompt and
        // tools, and 12k-character file reads. Keeping a fixed 4 messages
        // stayed over the threshold, so every step compacted again (22
        // times in one turn) and the model kept starting over.
        let steps = 20;
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SizedReadLoopApi {
                tool_steps: steps,
                calls: 0,
                overhead: 13_400,
            },
            // Each offset is a different part of the file.
            StaticToolExecutor::new().register("read_file", |input| {
                Ok(format!("{input}{}", "x".repeat(12_000)))
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(22_528);

        runtime
            .run_turn("make people terms search the people category", None)
            .expect("turn should succeed");
        let compactions = runtime
            .session()
            .compaction
            .as_ref()
            .map_or(0, |compaction| compaction.count);
        assert!(compactions > 0, "the window does fill up");
        assert!(
            (compactions as usize) * 2 <= steps,
            "compacted {compactions} times in {steps} steps"
        );
        let first = &runtime.session().messages[0];
        let ContentBlock::Text { text } = &first.blocks[0] else {
            panic!("summary message should be text");
        };
        assert!(text.contains("read_file /w/main.cpp (offset 1)"), "{text}");
        assert!(
            text.len() < 8_000,
            "summary stays bounded: {} chars",
            text.len()
        );
    }

    #[test]
    fn many_small_steps_do_not_trigger_auto_compaction() {
        let _queue = crate::turn_keys::exclusive_queue();
        // 30 steps of 20k tokens add up to 600k, but the context never
        // gets near the threshold.
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ToolLoopApi {
                tool_steps: 30,
                calls: 0,
                context: 20_000,
            },
            StaticToolExecutor::new().register("echo", |_| Ok("ok".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("commit and push", None)
            .expect("turn should succeed");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 62);
    }

    #[test]
    fn mid_turn_auto_compaction_keeps_the_running_request() {
        let _queue = crate::turn_keys::exclusive_queue();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ToolLoopApi {
                tool_steps: 6,
                calls: 0,
                context: 150_000,
            },
            StaticToolExecutor::new().register("echo", |_| Ok("ok".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("commit the fix and push it to origin", None)
            .expect("turn should succeed");
        assert!(summary.auto_compaction.is_some());
        let first = &runtime.session().messages[0];
        assert_eq!(first.role, MessageRole::System);
        let ContentBlock::Text { text } = &first.blocks[0] else {
            panic!("summary message should be text");
        };
        assert!(
            text.ends_with("must finish:\ncommit the fix and push it to origin"),
            "{text}"
        );
    }

    #[test]
    fn skips_auto_compaction_below_threshold() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 99_999,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[test]
    fn auto_compaction_threshold_defaults_and_parses_values() {
        assert_eq!(
            parse_auto_compaction_threshold(None),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(parse_auto_compaction_threshold(Some("4321")), 4321);
        assert_eq!(
            parse_auto_compaction_threshold(Some("0")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(
            parse_auto_compaction_threshold(Some("not-a-number")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
    }

    #[test]
    fn compaction_health_probe_blocks_turn_when_tool_executor_is_broken() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                panic!("API should not run when health probe fails");
            }
        }

        let mut session = Session::new();
        session.record_compaction("summarized earlier work", 4);
        session
            .push_user_text("previous message")
            .expect("message should append");

        let tool_executor = StaticToolExecutor::new().register("glob_search", |_input| {
            Err(ToolError::new("transport unavailable"))
        });
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            tool_executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn("trigger", None)
            .expect_err("health probe failure should abort the turn");
        assert!(
            error
                .to_string()
                .contains("Session health probe failed after compaction"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("transport unavailable"),
            "expected underlying probe error: {error}"
        );
    }

    #[test]
    fn compaction_health_probe_skips_empty_compacted_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.record_compaction("fresh summary", 2);

        let tool_executor = StaticToolExecutor::new().register("glob_search", |_input| {
            Err(ToolError::new(
                "glob_search should not run for an empty compacted session",
            ))
        });
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            tool_executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("trigger", None)
            .expect("empty compacted session should not fail health probe");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[test]
    fn build_assistant_message_requires_message_stop_event() {
        // given
        let events = vec![AssistantEvent::TextDelta("hello".to_string())];

        // when
        let error = build_assistant_message(events)
            .expect_err("assistant messages should require a stop event");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream ended without a message stop event"));
    }

    #[test]
    fn build_assistant_message_requires_content() {
        // given
        let events = vec![AssistantEvent::MessageStop];

        // when
        let error =
            build_assistant_message(events).expect_err("assistant messages should require content");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream produced no content"));
    }

    #[test]
    fn build_assistant_message_places_thinking_block_before_text_and_tool_use() {
        // given
        let events = vec![
            AssistantEvent::Thinking {
                thinking: "pondering".to_string(),
                signature: Some("sig".to_string()),
            },
            AssistantEvent::TextDelta("hello".to_string()),
            AssistantEvent::ToolUse {
                id: "tool-1".to_string(),
                name: "echo".to_string(),
                input: "payload".to_string(),
            },
            AssistantEvent::MessageStop,
        ];

        // when
        let (message, _, _) = build_assistant_message(events)
            .expect("assistant message should preserve thinking, text, and tool blocks");

        // then
        assert_eq!(
            message.blocks,
            vec![
                ContentBlock::Thinking {
                    thinking: "pondering".to_string(),
                    signature: Some("sig".to_string()),
                },
                ContentBlock::Text {
                    text: "hello".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "echo".to_string(),
                    input: "payload".to_string(),
                },
            ]
        );
    }

    #[test]
    fn static_tool_executor_rejects_unknown_tools() {
        // given
        let mut executor = StaticToolExecutor::new();

        // when
        let error = executor
            .execute("missing", "{}")
            .expect_err("unregistered tools should fail");

        // then
        assert_eq!(error.to_string(), "unknown tool: missing");
    }

    #[test]
    fn run_turn_errors_when_max_iterations_is_exceeded() {
        struct LoopingApi;

        impl ApiClient for LoopingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "echo".to_string(),
                        input: "payload".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            LoopingApi,
            StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_max_iterations(1);

        // when
        let error = runtime
            .run_turn("loop", None)
            .expect_err("conversation loop should stop after the configured limit");

        // then
        assert!(error
            .to_string()
            .contains("conversation loop exceeded the maximum number of iterations"));
    }

    #[test]
    fn run_turn_propagates_api_errors() {
        struct FailingApi;

        impl ApiClient for FailingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        // when
        let error = runtime
            .run_turn("hello", None)
            .expect_err("API failures should propagate");

        // then
        assert_eq!(error.to_string(), "upstream failed");
    }
}
