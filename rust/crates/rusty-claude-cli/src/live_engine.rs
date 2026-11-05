//! REPL turns on the Infinity engine (`CLAW_ENGINE=infinity`).
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
const ENGINE_TOOLS: &[&str] = &[
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
        };
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        let started = std::time::Instant::now();
        let mut view = TurnView::default();
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
        self.persist_session()?;
        Ok(())
    }

    fn engine_config(&self) -> Result<EngineConfig, Box<dyn std::error::Error>> {
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
        let provider = claw_engine::ClawModelProvider::from_model(&self.model)?
            .with_reasoning_effort(self.reasoning_effort.clone())
            .with_max_output_tokens(reply_budget(&self.model))
            .with_image_input(sends_images(&self.model));
        let executor_registry = registry.clone();
        let mcp_state = self.runtime.mcp_state.clone();
        let mut config = EngineConfig::for_provider(
            provider,
            self.effective_system_prompt().join("\n\n"),
            registry,
            policy,
        );
        config.allowed_tools.clone_from(&self.allowed_tools);
        config.hooks = Some(runtime::HookRunner::from_feature_config(&feature_config));
        config.runtime_tools = mcp_state.is_some();
        let prompter: Box<dyn runtime::PermissionPrompter + Send> =
            Box::new(CliPermissionPrompter::new(self.permission_mode));
        config.prompter = Some(Arc::new(Mutex::new(prompter)));
        config.executor = Some(Arc::new(move |name: &str, input: &serde_json::Value| {
            if name == "AskUserQuestion" {
                runtime::turn_keys::begin_tool("Waiting for your answer");
                let answer = ask::ask_user(input);
                runtime::turn_keys::end_tool();
                return answer;
            }
            runtime::turn_keys::begin_tool(&tool_activity_label(name, &input.to_string()));
            let result = match &mcp_state {
                Some(mcp_state) if executor_registry.has_runtime_tool(name) => {
                    tool_executor::run_mcp_tool(mcp_state, name, input.clone())
                        .map_err(|error| error.to_string())
                }
                _ => executor_registry.execute(name, input),
            };
            runtime::turn_keys::end_tool();
            result
        }));
        Ok(config)
    }
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
                render::start_thinking("thinking");
            }
        }
    }

    fn show_root(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::CompletionStarted => {
                runtime::turn_keys::begin_model_request();
                render::start_thinking("thinking");
                self.streaming = false;
            }
            AgentEvent::ThinkingChunk { text } => {
                runtime::turn_keys::note_reasoning(text.chars().count());
            }
            AgentEvent::TextChunk { text } => {
                runtime::turn_keys::note_text(text.chars().count());
                if !self.streaming {
                    self.streaming = true;
                    render::stop_thinking();
                }
                if let Some(rendered) = self.markdown.push(&TerminalRenderer::new(), text) {
                    Self::write(&rendered);
                }
            }
            AgentEvent::CompletionFinished { .. } => self.flush(),
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
