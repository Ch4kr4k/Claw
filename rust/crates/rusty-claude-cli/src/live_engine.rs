//! Turns on the Infinity engine (the default; `CLAW_ENGINE=legacy` opts out).
//!
//! The session stays claw's: each turn seeds the engine's root thread with
//! the session's messages and appends what the turn added, so checkpoints,
//! `/undo`, `--resume`, compaction and learning work as with the classic
//! loop. Within the turn the engine runs slow tools in the background, lets
//! the model fan work out to child threads, sends prompts typed meanwhile
//! straight to the running thread, and stops on Esc / Ctrl+C.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use claw_engine::{AgentEvent, Engine, EngineConfig, EngineEvent, ToolActivity, TurnControl};

#[allow(clippy::wildcard_imports)]
use crate::*;

/// The engine's thread tools: they have no claw tool activity, so their
/// calls are shown when the model makes them.
pub(crate) const ENGINE_TOOLS: &[&str] = &[
    "spawn_thread",
    "report_to_parent",
    "close_thread",
    "sleep",
    "sleep_until",
    "cancel_subscription",
];

impl LiveCli {
    /// One REPL turn on the engine; see the module docs.
    pub(crate) fn run_engine_turn(
        &mut self,
        input: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let config = self.engine_config()?;
        let history = self.runtime.session().messages.clone();
        runtime::turn_keys::set_activity_color(&render::activity_color(render::current_theme()));
        let abort = runtime::HookAbortSignal::new();
        let key_watcher = runtime::turn_keys::TurnKeyWatcher::start(abort.clone());
        let control = TurnControl {
            abort: Some(abort),
            queued_prompts: Some(Box::new(runtime::turn_keys::take_queued_for_running_turn)),
            attachments: self.pending_attachments.clone(),
            // One root thread id per session keeps the system prompt the same
            // from turn to turn, so a local server's prompt cache holds.
            thread_id: Some(self.session.id.clone()),
        };
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        let started = std::time::Instant::now();
        let mut view = TurnView {
            model: self.model.clone(),
            ..TurnView::default()
        };
        let result = Engine::new(config)
            .run_turn_blocking(&history, input, control, |event| view.show(event));
        view.flush();
        runtime::turn_keys::set_live_threads(0);
        drop(key_watcher);
        render::stop_thinking();
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                spinner.fail(
                    "request failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                return Err(Box::new(error));
            }
        };
        for message in outcome.messages.iter().cloned() {
            self.runtime.session_mut().push_message(message)?;
        }
        self.pending_attachments.clear();
        let messages = &self.runtime.session().messages;
        learning::after_turn(
            messages,
            &messages[messages.len().saturating_sub(outcome.messages.len())..],
        );
        let compaction = self.after_engine_turn(input, &outcome);
        let threads = if outcome.summary.threads > 1 {
            format!(", {} threads", outcome.summary.threads)
        } else {
            String::new()
        };
        let label = if outcome.interrupted {
            format!(
                "stopped after {:.1}s{threads}",
                started.elapsed().as_secs_f64()
            )
        } else {
            format!("done in {:.1}s{threads}", started.elapsed().as_secs_f64())
        };
        spinner.finish(&label, TerminalRenderer::new().color_theme(), &mut stdout)?;
        println!();
        if let Some(event) = compaction {
            println!("{}", auto_compaction_notice(&event));
        }
        self.persist_session()?;
        Ok(())
    }

    /// After an engine turn: its usage is recorded (`/cost`), and when its
    /// last request filled the context past the threshold the session is
    /// compacted, as the classic loop did between steps.
    pub(crate) fn after_engine_turn(
        &mut self,
        input: &str,
        outcome: &claw_engine::TurnOutcome,
    ) -> Option<runtime::AutoCompactionEvent> {
        let rt = self.runtime.runtime.as_mut()?;
        if let Some(usage) = outcome
            .messages
            .iter()
            .rev()
            .find_map(|message| message.usage)
        {
            rt.record_usage(usage);
        }
        let context = u32::try_from(outcome.summary.last_context_tokens).unwrap_or(u32::MAX);
        rt.auto_compact(input, context)
    }

    /// One turn on the engine with nothing drawn (`-p --output-format
    /// json`): the session is updated and saved as for a REPL turn, and the
    /// turn comes back as the classic loop's summary, so the JSON output is
    /// built the same way whichever loop ran it.
    pub(crate) fn run_engine_turn_quiet(
        &mut self,
        input: &str,
    ) -> Result<runtime::TurnSummary, Box<dyn std::error::Error>> {
        let config = self.engine_config()?;
        let history = self.runtime.session().messages.clone();
        let control = TurnControl {
            attachments: self.pending_attachments.clone(),
            thread_id: Some(self.session.id.clone()),
            ..TurnControl::default()
        };
        let outcome = Engine::new(config).run_turn_blocking(&history, input, control, |_| {})?;
        for message in outcome.messages.iter().cloned() {
            self.runtime.session_mut().push_message(message)?;
        }
        self.pending_attachments.clear();
        let compaction = self.after_engine_turn(input, &outcome);
        self.persist_session()?;
        let mut summary = turn_summary(&outcome);
        summary.auto_compaction = compaction;
        Ok(summary)
    }

    /// A prompt answered on the engine from a copy of the conversation; the
    /// session is left as it was (side questions, internal prompts).
    /// Without `enable_tools` no tools are offered or run. `progress` gets
    /// the steps of the run, as the classic loop reports them.
    pub(crate) fn run_engine_side_prompt(
        &self,
        prompt: &str,
        enable_tools: bool,
        progress: Option<&InternalPromptProgressReporter>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let mut config = self.engine_config()?;
        if !enable_tools {
            config.offer_tools = false;
            config.allowed_tools = Some(std::collections::BTreeSet::new());
        }
        let history = self.runtime.session().messages.clone();
        let control = TurnControl {
            // The session's root thread id keeps the system prompt (and a
            // local server's prompt cache) the same as the main turns'.
            thread_id: Some(self.session.id.clone()),
            ..TurnControl::default()
        };
        let outcome =
            Engine::new(config).run_turn_blocking(&history, prompt, control, |event| {
                let Some(progress) = progress else { return };
                match event {
                    EngineEvent::Agent {
                        event: AgentEvent::CompletionStarted,
                        ..
                    } => progress.mark_model_phase(),
                    EngineEvent::Tool(ToolActivity::Started { name, .. }) => {
                        progress.mark_tool_phase(name, "");
                    }
                    _ => {}
                }
            })?;
        let text = outcome.summary.final_text.trim().to_string();
        if let Some(progress) = progress {
            progress.mark_text_phase(&text);
        }
        Ok(text)
    }

    pub(crate) fn engine_config(&self) -> Result<EngineConfig, Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let runtime_config = ConfigLoader::default_for(&cwd).load()?;
        let plugin_registry = &self.runtime.plugin_registry;
        let plugin_hooks =
            runtime_hook_config_from_plugin_hooks(plugin_registry.aggregated_hooks()?);
        let feature_config = runtime_config
            .feature_config()
            .clone()
            .with_hooks(runtime_config.hooks().merged(&plugin_hooks));
        // The session's registry: built-ins, plugin tools and the MCP
        // servers' tools, as the classic loop has them.
        let registry = match self.runtime.runtime.as_ref() {
            Some(rt) => rt.tool_executor().tool_registry.clone(),
            None => GlobalToolRegistry::with_plugin_tools(plugin_registry.aggregated_tools()?)?,
        };
        let policy = permission_policy(self.permission_mode, &feature_config, &registry)?;
        let provider = engine_provider(&self.model, &self.session.id)?
            .with_reasoning_effort(self.reasoning_effort.clone())
            // `/btw` side questions are answered from the last request.
            .with_request_observer(Arc::new(
                |client: &api::ProviderClient, request: &api::MessageRequest| {
                    remember_for_side_questions(client, request);
                },
            ));
        // One CLI executor for the turn, output off (the turn view draws):
        // it repairs tool names, enforces --allowedTools, pages and outlines
        // reads to the model's window, flags repeated calls, runs ToolSearch
        // (loading what it finds) and MCP tools, as in the classic loop.
        let cli_executor = Arc::new(
            CliToolExecutor::new(
                self.allowed_tools.clone(),
                false,
                registry.clone(),
                self.runtime.mcp_state.clone(),
            )
            .with_model(&self.model),
        );
        let tool_list = engine_tool_list(&registry, &self.model, self.allowed_tools.clone());
        let mcp_configured = self.runtime.mcp_state.is_some();
        let mut config = EngineConfig::for_provider(
            provider,
            self.effective_system_prompt().join("\n\n"),
            registry,
            policy,
        );
        config.allowed_tools.clone_from(&self.allowed_tools);
        // As the classic loop had them: a turn that edited code is reminded
        // to check it, a directory's AGENTS.md comes as the model first works
        // there, and finished background jobs reach the model between steps.
        config.guards = claw_engine::GuardConfig {
            verify_on_stop: verify_on_stop_enabled(),
            context_hints: Some(session_context_hints()),
            job_notices: true,
        };
        // Sub-agents without an explicit model use this session's model.
        tools::set_default_agent_model(&self.model);
        config.hooks = Some(runtime::HookRunner::from_feature_config(&feature_config));
        config.runtime_tools = mcp_configured;
        config.tool_list = Some(tool_list);
        let resolver = Arc::clone(&cli_executor);
        config.resolve_tool_name =
            Some(Arc::new(move |name: &str| resolver.resolve_tool_name(name)));
        // The desktop app answers approvals itself (and remembers "always").
        let prompter: Box<dyn runtime::PermissionPrompter + Send> = if desktop_agent::is_active() {
            desktop_agent::prompter()
        } else {
            Box::new(CliPermissionPrompter::new(self.permission_mode))
        };
        config.prompter = Some(Arc::new(Mutex::new(prompter)));
        config.executor = Some(Arc::new(move |name: &str, input: &serde_json::Value| {
            // In the desktop app the CLI executor asks through the app.
            if name == "AskUserQuestion" && !desktop_agent::is_active() {
                runtime::turn_keys::begin_tool("Waiting for your answer");
                let answer = ask::ask_user(input);
                runtime::turn_keys::end_tool();
                return answer;
            }
            let input = input.to_string();
            runtime::turn_keys::begin_tool(&tool_activity_label(name, &input));
            let result = cli_executor
                .run_call(name, &input)
                .map_err(|error| error.to_string());
            runtime::turn_keys::end_tool();
            result
        }));
        Ok(config)
    }
}

/// The engine's provider for `model`, set up as claw's turns use it: the
/// session's client (prompt cache, retry policy), the reply budget, image
/// support, a local server's thinking budget, and the fallback models.
pub(crate) fn engine_provider(
    model: &str,
    session_id: &str,
) -> Result<claw_engine::ClawModelProvider, Box<dyn std::error::Error>> {
    let client = provider_client::turn_client(model, session_id)?;
    let words_for = session_id.to_string();
    let mut provider = claw_engine::ClawModelProvider::new(client, model)
        .with_error_text(Arc::new(move |error: &api::ApiError| {
            format_user_visible_api_error(&words_for, error)
        }))
        .with_max_output_tokens(reply_budget(model))
        .with_image_input(sends_images(model))
        .with_extra_body(Arc::new(|model: &str, follows_cut_off: bool| {
            local_thinking_extra_body(model, follows_cut_off)
        }));
    if !fallback_models_for(&api::resolve_model_alias(model)).is_empty() {
        let primary = model.to_string();
        let active_primary = primary.clone();
        provider = provider.with_fallbacks(claw_engine::Fallbacks {
            active: Arc::new(move || active_fallback(&active_primary, true)),
            next: Arc::new(move |error: &str, failed: &[String]| {
                switch_to_fallback(&primary, error, failed, true)
            }),
            worthy: Arc::new(is_fallback_worthy),
        });
    }
    Ok(provider)
}

/// An engine turn as the classic loop reports one: the replies, the tool
/// results, completions as iterations, and the usage of the turn.
pub(crate) fn turn_summary(outcome: &claw_engine::TurnOutcome) -> runtime::TurnSummary {
    let of_role = |role: MessageRole| {
        outcome
            .messages
            .iter()
            .filter(|message| message.role == role)
            .cloned()
            .collect::<Vec<_>>()
    };
    let usage = outcome
        .messages
        .iter()
        .rev()
        .find_map(|message| message.usage)
        .unwrap_or_default();
    runtime::TurnSummary {
        assistant_messages: of_role(MessageRole::Assistant),
        tool_results: of_role(MessageRole::Tool),
        prompt_cache_events: Vec::new(),
        iterations: outcome.summary.completions,
        usage,
        auto_compaction: None,
    }
}

/// The tools a request offers, as the classic loop sends them
/// ([`sent_tool_definitions`]: a local model's core set, rarely used tools
/// behind `ToolSearch` until it loads them), worked out per request.
pub(crate) fn engine_tool_list(
    registry: &GlobalToolRegistry,
    model: &str,
    allowed_tools: Option<AllowedToolSet>,
) -> claw_engine::ToolList {
    let registry = registry.clone();
    let model = model.to_string();
    Arc::new(move || {
        sent_tool_definitions(&registry, &model, allowed_tools.as_ref())
            .into_iter()
            .map(|tool| claw_engine::ToolDefinition {
                name: tool.name,
                description: tool.description.unwrap_or_default(),
                parameters: tool.input_schema,
            })
            .collect()
    })
}

/// Draws a turn's events: the root thread's reply streamed as markdown, one
/// block per finished tool call, child threads' replies under their label.
#[derive(Default)]
struct TurnView {
    markdown: MarkdownStreamState,
    /// Child thread labels (`#1`, `#2`, …) by thread id.
    labels: BTreeMap<String, String>,
    /// Child replies, shown whole when their completion finishes.
    child_text: BTreeMap<String, String>,
    /// Whether the root's current reply has shown text yet.
    streaming: bool,
    /// Child threads whose driver is running, for the status line.
    live_children: std::collections::BTreeSet<String>,
    root: Option<String>,
    /// The turn's model, for `/stats`.
    model: String,
}

impl TurnView {
    fn write(text: &str) {
        let mut out = render::MarginWriter::for_stdout(StopThinkingOnWrite(io::stdout()));
        // A failed write to the terminal only loses display, not the turn.
        let _ = write!(out, "{text}").and_then(|()| out.flush());
    }

    /// Writes out the root reply's buffered markdown.
    fn flush(&mut self) {
        if let Some(rendered) = self.markdown.flush(&TerminalRenderer::new()) {
            Self::write(&rendered);
        }
    }

    fn label(&mut self, thread: &str) -> String {
        let next = self.labels.len() + 1;
        self.labels
            .entry(thread.to_string())
            .or_insert_with(|| format!("#{next}"))
            .clone()
    }

    fn show(&mut self, event: &EngineEvent) {
        match event {
            EngineEvent::Agent {
                thread,
                root: true,
                event,
            } => self.show_root(event),
            EngineEvent::Agent { thread, event, .. } => self.show_child(thread, event),
            EngineEvent::Lifecycle { thread, live } => {
                // The first thread to go live is the root.
                let root = self.root.get_or_insert_with(|| thread.clone());
                if thread != root {
                    if *live {
                        self.live_children.insert(thread.clone());
                    } else {
                        self.live_children.remove(thread);
                    }
                    runtime::turn_keys::set_live_threads(self.live_children.len());
                }
            }
            EngineEvent::Tool(ToolActivity::Started { .. }) => {}
            EngineEvent::Tool(ToolActivity::Finished {
                thread,
                name,
                input,
                output,
                is_error,
            }) => {
                self.flush();
                let child = self.labels.get(thread).cloned();
                let (brief, full) = foldable(|| {
                    let call = format_tool_call_start(name, input);
                    match &child {
                        Some(label) => format!("{TOOL_DIM_STYLE}[{label}]{STYLE_RESET} {call}"),
                        None => call,
                    }
                });
                let mut out = io::stdout();
                let _ =
                    writeln!(out).and_then(|()| write_folded(&mut out, &brief, full.as_deref()));
                if *is_error || !matches!(name.as_str(), "read_file" | "Read") {
                    print_foldable(|| format_tool_result(name, output, *is_error));
                }
                if matches!(
                    name.as_str(),
                    "write_file" | "Write" | "edit_file" | "Edit" | "NotebookEdit"
                ) {
                    diff_panel::refresh();
                }
                // `/btw` answers that came in while the tool ran.
                runtime::turn_keys::print_side_answers();
                render::start_thinking("thinking");
            }
        }
    }

    fn show_root(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::CompletionStarted => {
                step_stats::begin_step();
                runtime::turn_keys::begin_model_request();
                render::start_thinking("thinking");
                self.streaming = false;
            }
            AgentEvent::ThinkingChunk { text } => {
                step_stats::mark_first_token();
                runtime::turn_keys::note_reasoning(text.chars().count());
            }
            AgentEvent::TextChunk { text } => {
                step_stats::mark_first_token();
                runtime::turn_keys::note_text(text.chars().count());
                if !self.streaming {
                    self.streaming = true;
                    render::stop_thinking();
                }
                if let Some(rendered) = self.markdown.push(&TerminalRenderer::new(), text) {
                    Self::write(&rendered);
                }
            }
            AgentEvent::CompletionFinished { usage } => {
                self.flush();
                let usage = usage.unwrap_or_default();
                let clamp = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
                step_stats::end_step(
                    &self.model,
                    runtime::TokenUsage {
                        input_tokens: clamp(
                            usage.input_tokens.saturating_sub(usage.cached_input_tokens),
                        ),
                        output_tokens: clamp(usage.output_tokens),
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: clamp(usage.cached_input_tokens),
                    },
                );
            }
            AgentEvent::ToolCall { name, args, .. } if ENGINE_TOOLS.contains(&name.as_str()) => {
                self.flush();
                print_foldable(|| format_tool_call_start(name, &args.to_string()));
            }
            AgentEvent::SubscriptionEvent { name, text } => {
                self.flush();
                println!(
                    "{TOOL_DIM_STYLE}  ↩ {name}: {}{STYLE_RESET}",
                    first_line(text)
                );
            }
            AgentEvent::Info { text } => {
                self.flush();
                println!("{TOOL_DIM_STYLE}  {text}{STYLE_RESET}");
            }
            _ => {}
        }
    }

    fn show_child(&mut self, thread: &str, event: &AgentEvent) {
        let label = self.label(thread);
        match event {
            AgentEvent::TextChunk { text } => self
                .child_text
                .entry(thread.to_string())
                .or_default()
                .push_str(text),
            AgentEvent::CompletionFinished { .. } => {
                let text = self.child_text.remove(thread).unwrap_or_default();
                if !text.trim().is_empty() {
                    self.flush();
                    let rendered = TerminalRenderer::new().markdown_to_ansi(text.trim());
                    println!("{TOOL_DIM_STYLE}[{label}]{STYLE_RESET}");
                    Self::write(&rendered);
                    println!();
                }
            }
            AgentEvent::ToolCall { name, args, .. } if ENGINE_TOOLS.contains(&name.as_str()) => {
                self.flush();
                print_foldable(|| {
                    format!(
                        "{TOOL_DIM_STYLE}[{label}]{STYLE_RESET} {}",
                        format_tool_call_start(name, &args.to_string())
                    )
                });
            }
            _ => {}
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}
