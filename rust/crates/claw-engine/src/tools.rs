//! [`ClawTool`]: claw's tool registry behind Infinity's [`Tool`] trait.
//!
//! Every call is checked against the session's [`PermissionPolicy`] first; a
//! denied call becomes an `Error: …` tool result the model can react to.
//!
//! *Fast* tools (reads, searches, edits) run inline in the slice through
//! [`Tool::execute_synchronous`]: the result is injected and the completion
//! loops back without yielding. Everything else is *slow*: [`Tool::execute`]
//! starts it on tokio's blocking pool and returns at once, so the thread
//! yields and its driver stays free for user input, child reports and other
//! work. The result re-enters the thread later as an [`InputMessage`].

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use infinity_provider_protocol::{
    Image, ImageMediaType, ImageSource, Text, ToolResult, ToolResultContent, UserContent,
};
use rap_protocol::{ProviderCallId, ToolCallId};
use runtime::{
    HookRunResult, HookRunner, PermissionContext, PermissionOutcome, PermissionPolicy,
    PermissionPrompter,
};
use tools::GlobalToolRegistry;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Tools that run inline: they finish in milliseconds, so yielding for them
/// would only add a round trip through the input queue.
pub const SYNC_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob_search",
    "grep_search",
    "TodoWrite",
    "NotebookEdit",
    "ToolSearch",
    "StructuredOutput",
    "Config",
];

/// Whether calls to `name` run inline rather than being dispatched.
#[must_use]
pub fn runs_inline(name: &str) -> bool {
    SYNC_TOOLS.contains(&name)
}

/// What a tool call did, for front ends that show tool activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolActivity {
    /// A dispatched (slow) tool started on the blocking pool.
    Started { thread: String, name: String },
    /// A tool finished; `output` is what the model receives.
    Finished {
        thread: String,
        name: String,
        /// The call's arguments, as JSON text.
        input: String,
        output: String,
        is_error: bool,
    },
}

/// Callback receiving [`ToolActivity`]; called from worker threads.
pub type ActivitySink = Arc<dyn Fn(ToolActivity) + Send + Sync>;

/// Runs one tool call in place of the registry (for front ends with tools
/// of their own, such as an interactive `AskUserQuestion`). Called from
/// worker threads with the hook-updated input.
pub type Executor = Arc<dyn Fn(&str, &serde_json::Value) -> Result<String, String> + Send + Sync>;

/// Asks the user about calls the policy does not allow by itself. Calls are
/// asked one at a time (the lock is held for the question).
pub type SharedPrompter = Arc<Mutex<Box<dyn PermissionPrompter + Send>>>;

/// State shared by every [`ClawTool`] of an engine.
pub struct ToolHost {
    registry: GlobalToolRegistry,
    policy: PermissionPolicy,
    hooks: Option<HookRunner>,
    prompter: Option<SharedPrompter>,
    executor: Option<Executor>,
    /// Whether the registry's runtime (MCP) tools are offered: only when the
    /// executor can run them.
    runtime_tools: bool,
    activity: Option<ActivitySink>,
}

impl ToolHost {
    #[must_use]
    pub fn new(registry: GlobalToolRegistry, policy: PermissionPolicy) -> Self {
        Self {
            registry,
            policy,
            hooks: None,
            prompter: None,
            executor: None,
            runtime_tools: false,
            activity: None,
        }
    }

    #[must_use]
    pub fn with_activity(mut self, sink: ActivitySink) -> Self {
        self.activity = Some(sink);
        self
    }

    /// Runs the session's PreToolUse / PostToolUse hooks around each call.
    #[must_use]
    pub fn with_hooks(mut self, hooks: HookRunner) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// Asks `prompter` when the policy needs approval for a call.
    #[must_use]
    pub fn with_prompter(mut self, prompter: SharedPrompter) -> Self {
        self.prompter = Some(prompter);
        self
    }

    /// Offers the registry's runtime (MCP) tools too; the executor (see
    /// [`Self::with_executor`]) must run them.
    #[must_use]
    pub fn with_runtime_tools(mut self) -> Self {
        self.runtime_tools = true;
        self
    }

    /// Runs calls through `executor` instead of the registry.
    #[must_use]
    pub fn with_executor(mut self, executor: Executor) -> Self {
        self.executor = Some(executor);
        self
    }

    fn report(&self, activity: ToolActivity) {
        if let Some(sink) = &self.activity {
            sink(activity);
        }
    }

    /// The PreToolUse hook, the permission check, the tool, then the
    /// PostToolUse hook, as the classic loop runs them. Blocking: call it
    /// from a blocking-pool thread. The text is what the model sees.
    pub(crate) fn run(
        &self,
        thread: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> (String, bool) {
        let input_text = serde_json::to_string(input).unwrap_or_default();
        let pre = self
            .hooks
            .as_ref()
            .map(|hooks| hooks.run_pre_tool_use(name, &input_text));
        let effective_input = pre
            .as_ref()
            .and_then(HookRunResult::updated_input)
            .map_or_else(|| input_text.clone(), ToOwned::to_owned);
        let outcome = match pre.as_ref().and_then(hook_stop) {
            Some(verb) => PermissionOutcome::Deny {
                reason: hook_message(
                    pre.as_ref().expect("bug: hook stop without a hook result"),
                    &format!("PreToolUse hook {verb} tool `{name}`"),
                ),
            },
            None => self.authorize(name, &effective_input, pre.as_ref()),
        };
        let (mut output, mut is_error) = match outcome {
            PermissionOutcome::Deny { reason } => (format!("Error: {reason}"), true),
            PermissionOutcome::Allow => {
                let value =
                    serde_json::from_str(&effective_input).unwrap_or_else(|_| input.clone());
                let result = match &self.executor {
                    Some(executor) => executor(name, &value),
                    None => self.registry.execute(name, &value),
                };
                match result {
                    Ok(output) => (output, false),
                    Err(error) => (format!("Error: {error}"), true),
                }
            }
        };
        if let Some(pre) = &pre {
            output = merge_hook_feedback(pre.messages(), output, false);
        }
        if let Some(hooks) = &self.hooks {
            let post = if is_error {
                hooks.run_post_tool_use_failure(name, &effective_input, &output)
            } else {
                hooks.run_post_tool_use(name, &effective_input, &output, false)
            };
            let stopped = hook_stop(&post).is_some();
            is_error |= stopped;
            output = merge_hook_feedback(post.messages(), output, stopped);
        }
        self.report(ToolActivity::Finished {
            thread: thread.to_string(),
            name: name.to_string(),
            input: effective_input,
            output: output.clone(),
            is_error,
        });
        (output, is_error)
    }

    fn authorize(&self, name: &str, input: &str, pre: Option<&HookRunResult>) -> PermissionOutcome {
        let context = pre.map_or_else(PermissionContext::default, |pre| {
            PermissionContext::new(
                pre.permission_override(),
                pre.permission_reason().map(ToOwned::to_owned),
            )
        });
        match &self.prompter {
            Some(prompter) => {
                let mut prompter = prompter
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let prompter: &mut dyn PermissionPrompter = &mut **prompter;
                self.policy
                    .authorize_with_context(name, input, &context, Some(prompter))
            }
            None => self
                .policy
                .authorize_with_context(name, input, &context, None),
        }
    }
}

/// Why a hook stopped the call, if it did.
fn hook_stop(result: &HookRunResult) -> Option<&'static str> {
    if result.is_cancelled() {
        Some("cancelled")
    } else if result.is_failed() {
        Some("failed for")
    } else if result.is_denied() {
        Some("denied")
    } else {
        None
    }
}

fn hook_message(result: &HookRunResult, fallback: &str) -> String {
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

/// One claw tool exposed to the Infinity runtime.
pub struct ClawTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    host: Arc<ToolHost>,
}

impl ClawTool {
    /// One adapter per tool `host`'s registry offers (optionally limited to
    /// `allowed`, canonical names as claw's `--allowedTools` uses them).
    #[must_use]
    pub fn all(
        host: &Arc<ToolHost>,
        allowed: Option<&std::collections::BTreeSet<String>>,
    ) -> Vec<Box<dyn Tool<ChannelSender>>> {
        host.registry
            .definitions(allowed)
            .into_iter()
            // MCP tools need the session's MCP state, which only a front
            // end's executor holds.
            .filter(|definition| {
                host.runtime_tools || !host.registry.has_runtime_tool(&definition.name)
            })
            .map(|definition| {
                Box::new(ClawTool {
                    name: definition.name,
                    description: definition.description.unwrap_or_default(),
                    parameters: definition.input_schema,
                    host: Arc::clone(host),
                }) as Box<dyn Tool<ChannelSender>>
            })
            .chain(std::iter::once(
                Box::new(crate::batch::BatchTool::new(Arc::clone(host)))
                    as Box<dyn Tool<ChannelSender>>,
            ))
            .collect()
    }

    async fn run_blocking(&self, thread: String, args: serde_json::Value) -> (String, bool) {
        let host = Arc::clone(&self.host);
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || host.run(&thread, &name, &args))
            .await
            .unwrap_or_else(|error| (format!("Error: tool panicked: {error}"), true))
    }
}

fn tool_result(id: &str, call_id: Option<String>, output: String, is_error: bool) -> ToolResult {
    let (text, images) = if is_error {
        (output, Vec::new())
    } else {
        with_images(output)
    };
    let mut content = vec![ToolResultContent::Text(Text { text })];
    content.extend(images);
    ToolResult {
        id: id.to_string(),
        call_id,
        content,
    }
}

/// The pictures a tool result names with `claw-image: /path` lines (the X11
/// tool's screenshots, rendered pages), read and size-budgeted as the
/// classic loop does, plus the text with any note about images left out.
#[must_use]
pub fn with_images(output: String) -> (String, Vec<ToolResultContent>) {
    if !output.contains(runtime::tool_images::TOOL_IMAGE_MARKER) {
        return (output, Vec::new());
    }
    let mut message = runtime::ConversationMessage::tool_result("", "", output, false);
    runtime::tool_images::attach_tool_images(&mut message);
    let mut text = String::new();
    let mut images = Vec::new();
    for block in message.blocks {
        match block {
            runtime::ContentBlock::ToolResult { output, .. } => text = output,
            runtime::ContentBlock::Image {
                media_type, data, ..
            } => images.push(ToolResultContent::Image(Image {
                data: ImageSource::Base64(data),
                media_type: ImageMediaType::from_mime_type(&media_type),
            })),
            _ => {}
        }
    }
    (text, images)
}

#[async_trait]
impl Tool<ChannelSender> for ClawTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn supports_sync(&self) -> bool {
        runs_inline(&self.name)
    }

    async fn execute_synchronous(
        &self,
        args: &serde_json::Value,
        id: &ToolCallId<str>,
        call_id: Option<&ProviderCallId<str>>,
        context: &ToolContext<ChannelSender>,
    ) -> Option<ToolResult> {
        if !runs_inline(&self.name) {
            return None;
        }
        let (output, is_error) = self
            .run_blocking(context.group_id.as_str().to_string(), args.clone())
            .await;
        Some(tool_result(
            id.as_str(),
            call_id.map(|call_id| call_id.as_str().to_string()),
            output,
            is_error,
        ))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        id: ToolCallId,
        call_id: Option<ProviderCallId>,
        context: &ToolContext<ChannelSender>,
    ) -> Result<(), BoxError> {
        let thread = context.group_id.as_str().to_string();
        self.host.report(ToolActivity::Started {
            thread: thread.clone(),
            name: self.name.clone(),
        });
        let host = Arc::clone(&self.host);
        let name = self.name.clone();
        let sender = context.message_sender.clone();
        let group_id = context.group_id.clone();
        // Return immediately: the thread yields while the tool runs, and the
        // result comes back through the input queue like any other message.
        tokio::spawn(async move {
            let (output, is_error) =
                tokio::task::spawn_blocking(move || host.run(&thread, &name, &args))
                    .await
                    .unwrap_or_else(|error| (format!("Error: tool panicked: {error}"), true));
            let message = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(tool_result(
                    id.as_str(),
                    call_id.map(ProviderCallId::into_inner),
                    output,
                    is_error,
                ))),
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
