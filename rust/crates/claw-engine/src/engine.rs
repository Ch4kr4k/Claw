//! [`Engine`]: one claw session on an Infinity local agent system.
//!
//! The system's thread drivers are tokio local tasks, so they run on a
//! [`LocalSet`] inside a multi-threaded runtime: any number of agent threads
//! interleave on it asynchronously, provider streams and slow tools run on
//! the runtime's worker and blocking pools, and nothing waits on a tool.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
use infinity_agent_core::system::local::{LaunchingSystem, ThreadLifecycleState};
use infinity_agent_core::system::{AgentEvent, AgentSystemBuilder, NoRapHttp, StaticModel};
use infinity_provider_protocol::{ModelProvider, Usage};
use runtime::PermissionPolicy;
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tools::GlobalToolRegistry;

use crate::provider::{ClawModelProvider, ClawPrompt};
use crate::tools::{ClawTool, ToolActivity, ToolHost};

type System = LaunchingSystem<InMemoryConversationStore, InMemoryStateStore, NoRapHttp>;

/// Instructions added for the engine: how claw's tools behave on it.
pub const ENGINE_PROMPT: &str = "\
# Engine and threads
You run on an asynchronous engine.
- Fast tools (reads, searches, edits) answer at once; slow ones (bash, \
web fetches, sub-agents) run in the background and their result arrives as \
soon as it is ready. Several calls in one reply run together (read-only \
ones in parallel).
- For independent work, call spawn_thread with your full thread stack in \
child_of (root first, yourself last). A child inherits your conversation up \
to the spawn, or nothing with fresh_context: true (then the instructions \
must be self-contained). You continue at once; do not wait for children.
- Inside a child thread: do only the task you were given, never the \
parent's other plans or spawns. Send results with report_to_parent and \
finish with close_thread (your own thread id, optional final report).
- Reports from children arrive as results of the spawn_thread call that \
created them.";

/// Something that happened in the engine, in the order it was observed.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// An event of one agent thread. `root` is true for the user's thread.
    Agent {
        thread: String,
        root: bool,
        event: AgentEvent,
    },
    /// A tool started or finished (reported from worker threads).
    Tool(ToolActivity),
    /// A thread's driver started (`live`) or went idle.
    Lifecycle { thread: String, live: bool },
}

/// Everything needed to run claw on the engine.
pub struct EngineConfig {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    /// claw's system prompt (all sections, joined).
    pub system_prompt: String,
    pub registry: GlobalToolRegistry,
    pub policy: PermissionPolicy,
    pub allowed_tools: Option<BTreeSet<String>>,
}

impl EngineConfig {
    /// A config for `model` with claw's own provider client for it.
    pub fn for_model(
        model: &str,
        system_prompt: String,
        registry: GlobalToolRegistry,
        policy: PermissionPolicy,
    ) -> Result<Self, api::ApiError> {
        let provider = ClawModelProvider::from_model(model)?;
        let model = provider.entry().model_id.clone();
        Ok(Self {
            provider: Arc::new(provider),
            model,
            system_prompt,
            registry,
            policy,
            allowed_tools: None,
        })
    }
}

/// Totals of one [`Engine::run_prompt`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// The root thread's final reply (the text of its last completion).
    pub final_text: String,
    /// Threads that ran, the root included.
    pub threads: usize,
    /// Model completions across all threads.
    pub completions: usize,
    pub usage: Usage,
}

/// Errors starting or running the engine.
#[derive(Debug)]
pub enum EngineError {
    Model(String),
    Runtime(std::io::Error),
    Send(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(error) => write!(f, "model: {error}"),
            Self::Runtime(error) => write!(f, "async runtime: {error}"),
            Self::Send(error) => write!(f, "engine input queue: {error}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// claw on Infinity.
pub struct Engine {
    config: EngineConfig,
}

impl Engine {
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Runs `prompt` to completion on a fresh multi-threaded runtime,
    /// calling `on_event` for everything that happens, and returns once every
    /// thread (children included) is idle. For synchronous callers.
    pub fn run_prompt_blocking(
        self,
        prompt: &str,
        on_event: impl FnMut(&EngineEvent),
    ) -> Result<RunSummary, EngineError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(EngineError::Runtime)?;
        runtime.block_on(self.run_prompt(prompt, on_event))
    }

    /// Runs `prompt` to completion; see [`Self::run_prompt_blocking`]. Must
    /// be awaited inside a multi-threaded tokio runtime.
    pub async fn run_prompt(
        self,
        prompt: &str,
        on_event: impl FnMut(&EngineEvent),
    ) -> Result<RunSummary, EngineError> {
        LocalSet::new()
            .run_until(self.run_prompt_local(prompt.to_string(), on_event))
            .await
    }

    async fn run_prompt_local(
        self,
        prompt: String,
        mut on_event: impl FnMut(&EngineEvent),
    ) -> Result<RunSummary, EngineError> {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<EngineEvent>();
        let activity_tx = events_tx.clone();
        let host = Arc::new(
            ToolHost::new(self.config.registry, self.config.policy).with_activity(Arc::new(
                move |activity| {
                    // A closed channel means the run is over; drop the report.
                    let _ = activity_tx.send(EngineEvent::Tool(activity));
                },
            )),
        );
        let tools = ClawTool::all(&host, self.config.allowed_tools.as_ref());
        let model = StaticModel::new(
            Arc::new(ClawPrompt(self.config.provider)),
            &self.config.model,
        )
        .await
        .map_err(|error| EngineError::Model(error.to_string()))?;
        let system_prompt = if self.config.system_prompt.is_empty() {
            ENGINE_PROMPT.to_string()
        } else {
            format!("{}\n\n{ENGINE_PROMPT}", self.config.system_prompt)
        };
        let mut system: System = AgentSystemBuilder::new_local(
            InMemoryConversationStore::new(),
            InMemoryStateStore::new(),
            model,
        )
        .tools(tools)
        .extra_system_prompt(system_prompt)
        .with_tokio_sleep_tools()
        .start();

        let root = system.thread_builder().launch().await;
        let root_id = root.thread_id().as_str().to_string();
        forward(root, true, events_tx.clone());
        let mut attached = HashSet::from([root_id.clone()]);

        let first = system.sender();
        infinity_agent_core::traits::InputSender::send_to_input_queue(
            &first,
            infinity_agent_core::message::InputMessage::user_text(
                rap_protocol::ThreadId::from(root_id.clone()),
                prompt,
            ),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .map_err(|error| EngineError::Send(error.to_string()))?;

        let mut summary = RunSummary::default();
        let mut seen_live = false;
        let mut root_text = String::new();
        // When every driver has exited, a message one of them sent last (a
        // child's report) may not have restarted its target yet: the run is
        // only over once the system stays idle for IDLE_GRACE.
        let mut idle_deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                lifecycle = system.next_lifecycle_event() => {
                    let Some(lifecycle) = lifecycle else { break };
                    let thread = lifecycle.thread_id.as_str().to_string();
                    let live = matches!(lifecycle.state, ThreadLifecycleState::Live);
                    if live {
                        seen_live = true;
                        idle_deadline = None;
                        if attached.insert(thread.clone()) {
                            if let Ok(Some(handle)) =
                                system.thread_handle(&lifecycle.thread_id).await
                            {
                                forward(handle, false, events_tx.clone());
                            }
                        }
                    } else if seen_live && system.is_idle() {
                        idle_deadline = Some(tokio::time::Instant::now() + IDLE_GRACE);
                    }
                    on_event(&EngineEvent::Lifecycle { thread, live });
                }
                Some(event) = events_rx.recv() => {
                    record(&event, &root_id, &mut root_text, &mut summary);
                    on_event(&event);
                }
                () = sleep_until_or_never(idle_deadline) => {
                    if system.is_idle() {
                        break;
                    }
                    idle_deadline = None;
                }
            }
        }
        // Events emitted just before the last driver exited.
        while let Ok(event) = events_rx.try_recv() {
            record(&event, &root_id, &mut root_text, &mut summary);
            on_event(&event);
        }
        system.shutdown().await;
        summary.threads = attached.len();
        Ok(summary)
    }
}

/// How long the system must stay idle before a run counts as finished.
const IDLE_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

async fn sleep_until_or_never(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn record(event: &EngineEvent, root_id: &str, root_text: &mut String, summary: &mut RunSummary) {
    let EngineEvent::Agent { thread, event, .. } = event else {
        return;
    };
    let is_root = thread == root_id;
    match event {
        AgentEvent::CompletionStarted if is_root => root_text.clear(),
        AgentEvent::TextChunk { text } if is_root => root_text.push_str(text),
        AgentEvent::CompletionFinished { usage } => {
            summary.completions += 1;
            if let Some(usage) = usage {
                summary.usage.input_tokens += usage.input_tokens;
                summary.usage.output_tokens += usage.output_tokens;
                summary.usage.total_tokens += usage.total_tokens;
                summary.usage.cached_input_tokens += usage.cached_input_tokens;
            }
            if is_root && !root_text.trim().is_empty() {
                summary.final_text.clone_from(root_text);
            }
        }
        _ => {}
    }
}

/// Relays a thread handle's events into the engine's event channel.
fn forward(
    mut handle: infinity_agent_core::system::local::ThreadHandle,
    root: bool,
    events: mpsc::UnboundedSender<EngineEvent>,
) {
    let thread = handle.thread_id().as_str().to_string();
    tokio::task::spawn_local(async move {
        while let Some(event) = handle.recv().await {
            let event = EngineEvent::Agent {
                thread: thread.clone(),
                root,
                event,
            };
            if events.send(event).is_err() {
                break;
            }
        }
    });
}
