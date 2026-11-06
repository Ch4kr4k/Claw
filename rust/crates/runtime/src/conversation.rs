//! [`ConversationRuntime`]: a session with the client and tool executor
//! that serve it, usage tracking and compaction. Turns themselves run on the
//! Infinity engine (crate `claw-engine`); this type holds what they read and
//! extend.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use crate::compact::{
    compact_session, compact_session_with, estimate_session_tokens, prune_old_tool_results,
    summary_transcript, truncate_large_tool_results, CompactionConfig, CompactionResult,
    SUMMARY_SYSTEM_PROMPT,
};
use crate::permissions::PermissionPolicy;
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::usage::{TokenUsage, UsageTracker};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 100_000;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";

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

/// The model client a session keeps for work outside turns.
pub trait ApiClient {
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

/// Summary of one completed turn, including tool results and usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub prompt_cache_events: Vec<PromptCacheEvent>,
    pub iterations: usize,
    pub usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
}

/// Details about automatic session compaction applied after a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
    /// Old tool results cleared before (or instead of) summarizing.
    pub pruned_tool_results: usize,
}

/// A session with the client and tools that serve it.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    usage_tracker: UsageTracker,
    auto_compaction_input_tokens_threshold: u32,
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
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        Self {
            session,
            api_client,
            tool_executor,
            permission_policy,
            usage_tracker,
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
        }
    }

    /// Switches the permission mode of the policy in use; rules, tool
    /// requirements and everything else stay as they are.
    pub fn set_permission_mode(&mut self, mode: crate::permissions::PermissionMode) {
        self.permission_policy.set_active_mode(mode);
    }

    #[must_use]
    pub fn with_auto_compaction_input_tokens_threshold(mut self, threshold: u32) -> Self {
        self.auto_compaction_input_tokens_threshold = threshold;
        self
    }

    /// Update the auto-compaction threshold after construction, e.g. from a
    /// context window the server named in an error.
    pub fn set_auto_compaction_input_tokens_threshold(&mut self, threshold: u32) {
        self.auto_compaction_input_tokens_threshold = threshold;
    }

    /// Records a finished turn's usage (`/cost`, `/stats`).
    pub fn record_usage(&mut self, usage: TokenUsage) {
        self.usage_tracker.record(usage);
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

    /// The tool executor of the session (its registry, MCP state).
    #[must_use]
    pub fn tool_executor(&self) -> &T {
        &self.tool_executor
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

    /// Compacts the session when the last request of a turn filled the
    /// context past the threshold. `context_tokens` is what that request
    /// sent plus its reply (which the next request carries too).
    /// `turn_request` is the turn's prompt: if it is summarized away, it is
    /// kept word for word in the summary message, so the model still knows
    /// what it was in the middle of doing.
    pub fn auto_compact(
        &mut self,
        turn_request: &str,
        context_tokens: u32,
    ) -> Option<AutoCompactionEvent> {
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
        parse_auto_compaction_threshold, ApiClient, ConversationRuntime, StaticToolExecutor,
        ToolExecutor, DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
    };
    use crate::permissions::{PermissionMode, PermissionPolicy};
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
    use crate::usage::TokenUsage;

    struct NoSummary;
    impl ApiClient for NoSummary {}

    fn runtime(session: Session) -> ConversationRuntime<NoSummary, StaticToolExecutor> {
        ConversationRuntime::new(
            session,
            NoSummary,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
        )
    }

    #[test]
    fn reconstructs_usage_tracker_from_restored_session() {
        let mut session = Session::new();
        session
            .messages
            .push(ConversationMessage::assistant_with_usage(
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
        let mut runtime = runtime(session);
        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
        runtime.record_usage(TokenUsage {
            input_tokens: 4,
            output_tokens: 1,
            ..TokenUsage::default()
        });
        assert_eq!(runtime.usage().turns(), 2);
    }

    #[test]
    fn forks_runtime_session_without_mutating_original() {
        let mut session = Session::new();
        session
            .push_user_text("branch me")
            .expect("message should append");
        let runtime = runtime(session.clone());
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
    fn auto_compact_shrinks_a_full_session_and_keeps_the_request() {
        let mut session = Session::new();
        for round in 0..12 {
            session
                .push_user_text(format!("question {round}"))
                .expect("append");
            session
                .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: format!("answer {round} {}", "x".repeat(4_000)),
                }]))
                .expect("append");
        }
        let mut runtime = runtime(session).with_auto_compaction_input_tokens_threshold(5_000);
        assert_eq!(
            runtime.auto_compact("question 0", 1_000),
            None,
            "under the threshold"
        );
        let event = runtime
            .auto_compact("question 0", 20_000)
            .expect("compacted");
        assert!(event.removed_message_count > 0 || event.pruned_tool_results > 0);
        assert!(runtime.session().messages.len() < 24);
        let summary = &runtime.session().messages[0];
        assert_eq!(summary.role, MessageRole::System);
        assert!(
            matches!(&summary.blocks[0], ContentBlock::Text { text } if text.contains("question 0"))
        );
    }

    #[test]
    fn static_tool_executor_rejects_unknown_tools() {
        let mut executor = StaticToolExecutor::new();
        let error = executor
            .execute("missing", "{}")
            .expect_err("unregistered tools should fail");
        assert_eq!(error.to_string(), "unknown tool: missing");
    }
}
