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

use std::sync::Arc;

use async_trait::async_trait;
use infinity_agent_core::message::{InputMessage, InputMessageContent};
use infinity_agent_core::system::local::ChannelSender;
use infinity_agent_core::tools::{Tool, ToolContext};
use infinity_agent_core::traits::InputSender;
use infinity_provider_protocol::{Text, ToolResult, ToolResultContent, UserContent};
use rap_protocol::{ProviderCallId, ToolCallId};
use runtime::{PermissionOutcome, PermissionPolicy};
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

/// State shared by every [`ClawTool`] of an engine.
pub struct ToolHost {
    registry: GlobalToolRegistry,
    policy: PermissionPolicy,
    activity: Option<ActivitySink>,
}

impl ToolHost {
    #[must_use]
    pub fn new(registry: GlobalToolRegistry, policy: PermissionPolicy) -> Self {
        Self {
            registry,
            policy,
            activity: None,
        }
    }

    #[must_use]
    pub fn with_activity(mut self, sink: ActivitySink) -> Self {
        self.activity = Some(sink);
        self
    }

    fn report(&self, activity: ToolActivity) {
        if let Some(sink) = &self.activity {
            sink(activity);
        }
    }

    /// Checks the policy, then runs the tool. Blocking: call it from a
    /// blocking-pool thread. The text is what the model sees.
    pub(crate) fn run(
        &self,
        thread: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> (String, bool) {
        let input_text = serde_json::to_string(input).unwrap_or_default();
        let (output, is_error) = match self.policy.authorize(name, &input_text, None) {
            PermissionOutcome::Deny { reason } => (format!("Error: {reason}"), true),
            PermissionOutcome::Allow => match self.registry.execute(name, input) {
                Ok(output) => (output, false),
                Err(error) => (format!("Error: {error}"), true),
            },
        };
        self.report(ToolActivity::Finished {
            thread: thread.to_string(),
            name: name.to_string(),
            input: input_text,
            output: output.clone(),
            is_error,
        });
        (output, is_error)
    }
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
            // MCP tools need the session's MCP state, which the engine does
            // not hold yet (docs/infinity-engine.md, phase 3).
            .filter(|definition| !host.registry.has_runtime_tool(&definition.name))
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

fn tool_result(id: &str, call_id: Option<String>, text: String) -> ToolResult {
    ToolResult {
        id: id.to_string(),
        call_id,
        content: vec![ToolResultContent::Text(Text { text })],
    }
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
        let (output, _) = self
            .run_blocking(context.group_id.as_str().to_string(), args.clone())
            .await;
        Some(tool_result(
            id.as_str(),
            call_id.map(|call_id| call_id.as_str().to_string()),
            output,
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
            let (output, _) = tokio::task::spawn_blocking(move || host.run(&thread, &name, &args))
                .await
                .unwrap_or_else(|error| (format!("Error: tool panicked: {error}"), true));
            let message = InputMessage {
                content: InputMessageContent::User(UserContent::ToolResult(tool_result(
                    id.as_str(),
                    call_id.map(ProviderCallId::into_inner),
                    output,
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
