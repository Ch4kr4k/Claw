//! [`Engine`]: one claw session on an Infinity local agent system.
//!
//! The system's thread drivers are tokio local tasks, so they run on a
//! [`LocalSet`] inside a multi-threaded runtime: any number of agent threads
//! interleave on it asynchronously, provider streams and slow tools run on
//! the runtime's worker and blocking pools, and nothing waits on a tool.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use infinity_agent_core::message::InputMessage;
use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
use infinity_agent_core::system::local::{ChannelSender, LaunchingSystem, ThreadLifecycleState};
use infinity_agent_core::system::{AgentEvent, AgentSystemBuilder, NoRapHttp, StaticModel};
use infinity_agent_core::traits::{ConversationStore, InputSender};
use infinity_agent_core::ThreadId;
use infinity_provider_protocol::{ModelProvider, Usage};
use runtime::{ConversationMessage, HookAbortSignal, HookRunner, PermissionPolicy};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tools::GlobalToolRegistry;

use crate::guards::{GuardConfig, TurnGuards};
use crate::history;
use crate::provider::{ClawModelProvider, ClawPrompt, NameResolver, ToolList};
use crate::tools::{ClawTool, Executor, SharedPrompter, ToolActivity, ToolHost};

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
    /// PreToolUse / PostToolUse hooks run around every call.
    pub hooks: Option<HookRunner>,
    /// Asked when the policy needs approval; without one such calls are denied.
    pub prompter: Option<SharedPrompter>,
    /// Runs calls in place of the registry.
    pub executor: Option<Executor>,
    /// Offer the registry's runtime (MCP) tools; `executor` must run them.
    pub runtime_tools: bool,
    /// The claw tools each request offers (see [`ClawPrompt::tool_list`]);
    /// `None` offers every registered tool.
    pub tool_list: Option<ToolList>,
    /// Repairs tool names a model gets wrong (see [`ClawPrompt::resolve_name`]).
    pub resolve_tool_name: Option<NameResolver>,
    /// Offer tools at all; `false` asks for a plain answer (see
    /// [`ClawPrompt::offer_tools`]).
    pub offer_tools: bool,
    /// Model calls the turn may make, child threads included; past it the
    /// turn ends as interrupted (see [`ClawPrompt::max_completions`]).
    pub max_completions: Option<usize>,
    /// What the turn's guards do beyond the always-on checks (see
    /// [`crate::guards`]).
    pub guards: GuardConfig,
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
            hooks: None,
            prompter: None,
            executor: None,
            runtime_tools: false,
            tool_list: None,
            resolve_tool_name: None,
            offer_tools: true,
            max_completions: None,
            guards: GuardConfig::default(),
        })
    }

    /// A config served by `provider` (built with claw's options: effort,
    /// reply budget, image support).
    #[must_use]
    pub fn for_provider(
        provider: ClawModelProvider,
        system_prompt: String,
        registry: GlobalToolRegistry,
        policy: PermissionPolicy,
    ) -> Self {
        let model = provider.entry().model_id.clone();
        Self {
            provider: Arc::new(provider),
            model,
            system_prompt,
            registry,
            policy,
            allowed_tools: None,
            hooks: None,
            prompter: None,
            executor: None,
            runtime_tools: false,
            tool_list: None,
            resolve_tool_name: None,
            offer_tools: true,
            max_completions: None,
            guards: GuardConfig::default(),
        }
    }
}

/// Totals of one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// The root thread's final reply (the text of its last completion).
    pub final_text: String,
    /// Threads that ran, the root included.
    pub threads: usize,
    /// Model completions across all threads.
    pub completions: usize,
    pub usage: Usage,
    /// What the root's last request sent plus its reply, in tokens: how full
    /// the context is for the next request.
    pub last_context_tokens: u64,
}

/// How the caller steers a running turn.
#[derive(Default)]
pub struct TurnControl {
    /// Set (Esc, Ctrl+C) to stop the turn: running completions are
    /// interrupted, their partial turns kept, and the run returns.
    pub abort: Option<HookAbortSignal>,
    /// Polled while the turn runs: prompts the user typed meanwhile. Each
    /// goes to the root thread at once (interrupting a streaming reply, as
    /// user text does on the engine).
    pub queued_prompts: Option<Box<dyn FnMut() -> Vec<String>>>,
    /// Blocks sent with the prompt (`@image`, `/image`): they reach the
    /// model just before the prompt's text, and are not part of
    /// [`TurnOutcome::messages`].
    pub attachments: Vec<runtime::ContentBlock>,
    /// The root thread's id. Give the same one for every turn of a session:
    /// the id is in the system prompt, so a new one each turn would change
    /// the prompt and cost a local server its prompt cache. `None` makes one.
    pub thread_id: Option<String>,
}

/// What one turn did.
#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub summary: RunSummary,
    /// The messages the turn added to the root conversation (the prompt
    /// included), as claw session messages.
    pub messages: Vec<ConversationMessage>,
    /// Whether the turn was stopped through [`TurnControl::abort`].
    pub interrupted: bool,
    /// The tool the model kept calling although every call was blocked as
    /// a repeat: the turn was ended for it (a note says so in `messages`).
    pub stopped_looping: Option<String>,
}

/// Errors starting or running the engine.
#[derive(Debug)]
pub enum EngineError {
    Model(String),
    Runtime(std::io::Error),
    Send(String),
    Store(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(error) => write!(f, "model: {error}"),
            Self::Runtime(error) => write!(f, "async runtime: {error}"),
            Self::Send(error) => write!(f, "engine input queue: {error}"),
            Self::Store(error) => write!(f, "conversation store: {error}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// How often a running turn checks its [`TurnControl`].
const CONTROL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

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
        self.run_turn_blocking(&[], prompt, TurnControl::default(), on_event)
            .map(|outcome| outcome.summary)
    }

    /// Runs one turn of a conversation: the root thread starts from
    /// `history` (claw's session so far), gets `prompt`, and runs until
    /// every thread is idle or `control` stops it. On a fresh
    /// multi-threaded runtime; for synchronous callers.
    pub fn run_turn_blocking(
        self,
        history: &[ConversationMessage],
        prompt: &str,
        control: TurnControl,
        on_event: impl FnMut(&EngineEvent),
    ) -> Result<TurnOutcome, EngineError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(EngineError::Runtime)?;
        let outcome = runtime.block_on(self.run_turn(history, prompt, control, on_event));
        // Dropping the runtime would wait for tools still running on its
        // blocking pool (a stopped `cargo build`); they finish on their own
        // and their results are dropped.
        runtime.shutdown_background();
        outcome
    }

    /// See [`Self::run_turn_blocking`]. Must be awaited inside a
    /// multi-threaded tokio runtime.
    pub async fn run_turn(
        self,
        history: &[ConversationMessage],
        prompt: &str,
        control: TurnControl,
        on_event: impl FnMut(&EngineEvent),
    ) -> Result<TurnOutcome, EngineError> {
        LocalSet::new()
            .run_until(self.run_turn_local(history, prompt.to_string(), control, on_event))
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn run_turn_local(
        self,
        history: &[ConversationMessage],
        prompt: String,
        mut control: TurnControl,
        mut on_event: impl FnMut(&EngineEvent),
    ) -> Result<TurnOutcome, EngineError> {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<EngineEvent>();
        let activity_tx = events_tx.clone();
        let mut host = ToolHost::new(self.config.registry, self.config.policy).with_activity(
            Arc::new(move |activity| {
                // A closed channel means the run is over; drop the report.
                let _ = activity_tx.send(EngineEvent::Tool(activity));
            }),
        );
        if let Some(hooks) = self.config.hooks {
            host = host.with_hooks(hooks);
        }
        if let Some(prompter) = self.config.prompter {
            host = host.with_prompter(prompter);
        }
        if let Some(executor) = self.config.executor {
            host = host.with_executor(executor);
        }
        if self.config.runtime_tools {
            host = host.with_runtime_tools();
        }
        let guards = Arc::new(TurnGuards::new(self.config.guards));
        host = host.with_guards(Arc::clone(&guards));
        if let Some(abort) = &control.abort {
            host = host.with_abort(abort.clone());
        }
        let rewrites = crate::reply::Rewrites::default();
        let host = Arc::new(host);
        let tools = ClawTool::all(&host, self.config.allowed_tools.as_ref());
        let completions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let model = StaticModel::new(
            Arc::new(ClawPrompt {
                inner: self.config.provider,
                tool_list: self.config.tool_list,
                resolve_name: self.config.resolve_tool_name,
                offer_tools: self.config.offer_tools,
                max_completions: self.config.max_completions,
                completions: Arc::clone(&completions),
                guards: Some(Arc::clone(&guards)),
                rewrites: Arc::clone(&rewrites),
            }),
            &self.config.model,
        )
        .await
        .map_err(|error| EngineError::Model(error.to_string()))?;
        let system_prompt = if self.config.system_prompt.is_empty() {
            ENGINE_PROMPT.to_string()
        } else {
            format!("{}\n\n{ENGINE_PROMPT}", self.config.system_prompt)
        };
        let store = InMemoryConversationStore::new();
        let mut system: System =
            AgentSystemBuilder::new_local(store.clone(), InMemoryStateStore::new(), model)
                .tools(tools)
                .extra_system_prompt(system_prompt)
                .with_tokio_sleep_tools()
                .start();

        let root = match control.thread_id.take() {
            Some(id) => {
                let id = ThreadId::from(id);
                store
                    .ensure_root_thread(&id)
                    .await
                    .map_err(|error| EngineError::Store(error.to_string()))?;
                system
                    .thread_handle(&id)
                    .await
                    .map_err(|error| EngineError::Store(error.to_string()))?
                    .ok_or_else(|| {
                        EngineError::Store(format!("thread {} not found", id.as_str()))
                    })?
            }
            None => system.thread_builder().launch().await,
        };
        let root_id = root.thread_id().to_owned();
        let root_name = root_id.as_str().to_string();
        guards.begin(&root_name, history);
        // The conversation so far, before the thread's first slice loads it.
        let attachments = runtime::ConversationMessage {
            role: runtime::MessageRole::User,
            blocks: std::mem::take(&mut control.attachments),
            usage: None,
        };
        let seed = history::to_infinity(history)
            .into_iter()
            .chain(history::to_infinity(std::slice::from_ref(&attachments)))
            .map(|entry| (entry, uuid::Uuid::new_v4().to_string()))
            .collect::<Vec<_>>();
        let seeded = seed.len();
        store
            .append_messages(&root_id, seed)
            .await
            .map_err(|error| EngineError::Store(error.to_string()))?;
        forward(root, true, events_tx.clone());
        let mut attached = HashSet::from([root_name.clone()]);

        let sender = system.sender();
        send_user_text(&sender, &root_id, prompt).await?;

        let mut summary = RunSummary::default();
        let mut seen_live = false;
        let mut interrupted = false;
        let mut stopped_looping = None;
        let mut root_text = String::new();
        // When every driver has exited, a message one of them sent last (a
        // child's report) may not have restarted its target yet: the run is
        // only over once the system stays idle for IDLE_GRACE.
        let mut idle_deadline: Option<tokio::time::Instant> = None;
        let mut poll = tokio::time::interval(CONTROL_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                    record(&event, &root_name, &mut root_text, &mut summary);
                    on_event(&event);
                    // What the guards want a thread sent once its reply is
                    // done: a continuation of a cut-off reply, a reminder.
                    if let EngineEvent::Agent {
                        thread,
                        event: AgentEvent::CompletionFinished { .. },
                        ..
                    } = &event
                    {
                        for text in guards.take_followups(thread) {
                            idle_deadline = None;
                            let target = ThreadId::from(thread.clone());
                            send_user_text(&sender, &target, text).await?;
                        }
                    }
                }
                () = sleep_until_or_never(idle_deadline) => {
                    if system.is_idle() {
                        break;
                    }
                    idle_deadline = None;
                }
                _ = poll.tick() => {
                    if control.abort.as_ref().is_some_and(HookAbortSignal::is_aborted) {
                        interrupted = true;
                        break;
                    }
                    // A model that keeps sending a call already blocked as a
                    // repeat is not going to stop by itself.
                    if let Some(tool) = guards.stuck() {
                        stopped_looping = Some(tool);
                        break;
                    }
                    if let Some(queued) = control.queued_prompts.as_mut() {
                        for text in queued() {
                            idle_deadline = None;
                            send_user_text(&sender, &root_id, text).await?;
                        }
                    }
                }
            }
        }
        // Interrupts running completions and flushes their partial turns.
        system.shutdown().await;
        if stopped_looping.is_none() {
            stopped_looping = guards.stuck();
        }
        if self
            .config
            .max_completions
            .is_some_and(|max| completions.load(std::sync::atomic::Ordering::Relaxed) > max)
        {
            interrupted = true;
        }
        // Events emitted just before the last driver exited.
        while let Ok(event) = events_rx.try_recv() {
            record(&event, &root_name, &mut root_text, &mut summary);
            on_event(&event);
        }
        // Infinity reports one finished completion per slice, and a slice
        // loops over inline tools: count the model calls themselves.
        let requested = completions.load(std::sync::atomic::Ordering::Relaxed);
        summary.completions = summary.completions.max(match self.config.max_completions {
            Some(max) => requested.min(max),
            None => requested,
        });
        summary.threads = attached.len();
        let entries = store
            .load_history_up_to(&root_id, None, None)
            .await
            .map_err(|error| EngineError::Store(error.to_string()))?;
        let added = entries.get(seeded..).unwrap_or_default();
        let mut messages = history::from_infinity(added);
        crate::reply::rewrite_messages(&mut messages, &rewrites);
        summary.final_text = crate::reply::rewrite_text(&summary.final_text, &rewrites);
        if let Some(tool) = &stopped_looping {
            eprintln!(
                "\n\x1b[2m(stopped: the model kept repeating a blocked `{tool}` call)\x1b[0m"
            );
            messages.push(ConversationMessage::assistant(vec![
                runtime::ContentBlock::Text {
                    text: runtime::turn_guards::stopped_looping_note(tool),
                },
            ]));
        }
        if let Some(last) = messages
            .iter_mut()
            .rev()
            .find(|message| message.role == runtime::MessageRole::Assistant)
        {
            last.usage = Some(token_usage(&summary.usage));
        }
        Ok(TurnOutcome {
            summary,
            messages,
            interrupted,
            stopped_looping,
        })
    }
}

async fn send_user_text(
    sender: &ChannelSender,
    thread: &ThreadId<str>,
    text: String,
) -> Result<(), EngineError> {
    sender
        .send_to_input_queue(
            InputMessage::user_text(thread.to_owned(), text),
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .map_err(|error| EngineError::Send(error.to_string()))
}

/// The run's usage in claw's form (input here includes cached input).
fn token_usage(usage: &Usage) -> runtime::TokenUsage {
    let clamp = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
    runtime::TokenUsage {
        input_tokens: clamp(usage.input_tokens.saturating_sub(usage.cached_input_tokens)),
        output_tokens: clamp(usage.output_tokens),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: clamp(usage.cached_input_tokens),
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
            if let (true, Some(usage)) = (is_root, usage) {
                summary.last_context_tokens = usage.input_tokens + usage.output_tokens;
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
