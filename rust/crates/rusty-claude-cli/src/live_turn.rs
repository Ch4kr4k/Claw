//! Running turns: preparing the runtime for a prompt, the watched attempt
//! and its retries (stop, LeanGate, compaction on overflow), one-shot
//! prompt modes (`-p`, `--compact`, JSON), and the prompt-backed commands
//! (`/bughunter`, `/ultraplan`, `/commit`, `/pr`, …).

#[allow(clippy::wildcard_imports)]
use crate::*;

impl LiveCli {
    pub(crate) fn prepare_turn_runtime(
        &self,
        emit_output: bool,
    ) -> Result<(BuiltRuntime, HookAbortMonitor), Box<dyn std::error::Error>> {
        let hook_abort_signal = runtime::HookAbortSignal::new();
        let mut runtime = build_runtime(
            self.runtime.session().clone(),
            &self.session.id,
            self.model.clone(),
            self.effective_system_prompt(),
            true,
            emit_output,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?
        .with_hook_abort_signal(hook_abort_signal.clone());
        // Each turn gets a fresh runtime, so the effort must be re-applied here;
        // setting it only on the startup runtime silently dropped it.
        if let Some(rt) = runtime.runtime.as_mut() {
            // Finished background shells and agents are reported to the
            // model between steps of this turn; sub-agents without an
            // explicit model use this session's model.
            rt.set_job_notices(true);
            rt.set_context_hints(session_context_hints());
            rt.set_verify_on_stop(verify_on_stop_enabled());
            tools::set_default_agent_model(&self.model);
            rt.api_client_mut()
                .set_reasoning_effort(self.reasoning_effort.clone());
            // Ctrl+C stops the reply too, not only running hooks.
            rt.api_client_mut()
                .set_abort_signal(hook_abort_signal.clone());
        }
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal);

        Ok((runtime, hook_abort_monitor))
    }

    pub(crate) fn replace_runtime(
        &mut self,
        runtime: BuiltRuntime,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_plugins()?;
        self.runtime = runtime;
        Ok(())
    }

    /// Runs one request, wrapped in a checkpoint (REPL only) and prefixed
    /// with notices about background jobs that exited since the last turn.
    pub(crate) fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        // A learning review still running would hold a one-slot local
        // server; the user's turn comes first.
        learning::cancel();
        learning::print_notices();
        step_stats::begin_turn();
        // What a `fork` sub-agent starts from: the finished turns.
        tools::set_fork_seed(
            self.runtime.session().clone(),
            self.effective_system_prompt(),
        );
        let prompt = Self::with_job_notices(input);
        self.begin_checkpoint(input);
        let _ = tools::leangate::take_edited_files();
        let mut result = self.run_turn_inner(&prompt);
        if result.is_ok() {
            result = self.lean_gate_rounds();
        }
        if result.is_ok() && !self.last_turn_was_stopped() {
            if let Some(report) = image_check(self.runtime.session()) {
                println!(
                    "  {TOOL_DIM_STYLE}⎿ Some images in the reply do not load; claw asks for working ones{STYLE_RESET}"
                );
                result = self.run_turn_inner(&report);
            }
        }
        self.finish_checkpoint();
        // Shell commands and sub-agents change files too.
        diff_panel::refresh();
        // Shift+Tab pressed while the turn ran: switch modes now it is over.
        if let Err(error) = self.cycle_permission_mode_by(runtime::turn_keys::take_mode_presses()) {
            eprintln!("error: {error}");
        }
        self.last_failed_prompt = result.is_err().then(|| input.to_string());
        result
    }

    /// Runs one attempt at a turn with the key watcher up, so the activity
    /// block and status line show and Esc, the mouse wheel and selection
    /// work while it runs. Without `attachments` the session already holds
    /// `input` and the turn is continued rather than sent again.
    pub(crate) fn run_watched_attempt(
        &self,
        runtime: &mut BuiltRuntime,
        hook_abort_monitor: HookAbortMonitor,
        input: &str,
        attachments: Option<Vec<ContentBlock>>,
    ) -> Result<runtime::TurnSummary, RuntimeError> {
        // Esc (like Ctrl+C) stops the reply; keys typed meanwhile become type-ahead.
        let key_watcher = runtime::turn_keys::TurnKeyWatcher::start(hook_abort_monitor.signal());
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = match attachments {
            Some(attachments) => runtime.run_turn_with_attachments(
                input,
                attachments,
                Some(&mut permission_prompter),
            ),
            None => runtime.continue_turn(input, Some(&mut permission_prompter)),
        };
        drop(key_watcher);
        hook_abort_monitor.stop();
        result
    }

    /// Ends a turn the user stopped (Esc, Ctrl+C), keeping what it did.
    pub(crate) fn finish_stopped_turn(
        &mut self,
        mut runtime: BuiltRuntime,
        spinner: &mut Spinner,
        stdout: &mut io::Stdout,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Stopping a reply hands control back: queued prompts are put
        // in the input box instead of running.
        runtime::turn_keys::unqueue_into_type_ahead();
        // Keep what the turn did (the prompt, finished tool calls and
        // their results) instead of dropping it, so the next prompt can
        // refer to it. The stop only lands during a model request, after
        // every tool_use already has its tool_result, so the history
        // stays well-formed; the note tells the model it was cut off.
        if let Some(rt) = runtime.runtime.as_mut() {
            let _ = rt
                .session_mut()
                .push_user_text("[Request interrupted by user]");
        }
        self.pending_attachments.clear();
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        spinner.fail("stopped", TerminalRenderer::new().color_theme(), stdout)?;
        println!();
        Ok(())
    }

    /// LeanGate after a turn: while the files the turn edited have type
    /// errors, the agent gets them back and continues, a few rounds at most.
    /// Skipped when the user stopped the turn.
    pub(crate) fn lean_gate_rounds(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for _ in 0..LEAN_GATE_ROUNDS {
            let edited = tools::leangate::take_edited_files();
            if edited.is_empty() || self.last_turn_was_stopped() {
                break;
            }
            let Some(report) = tools::leangate::completion_check(&edited) else {
                break;
            };
            println!(
                "  {TOOL_DIM_STYLE}⎿ LeanGate: the edited files have type errors; claw fixes them before finishing{STYLE_RESET}"
            );
            self.run_turn_inner(&report)?;
        }
        let _ = tools::leangate::take_edited_files();
        Ok(())
    }

    /// On the way out of a substantial session, one turn that saves what it
    /// learned (decisions, facts, mistakes, preferences) to auto memory.
    /// Esc skips it; `"memoryReflectOnExit": false` or `CLAW_MEMORY_REFLECT=0`
    /// turns it off.
    pub(crate) fn reflect_before_exit(&mut self) {
        if self.runtime.usage().turns() < REFLECT_MIN_TURNS
            || std::env::var("CLAW_MEMORY_REFLECT")
                .is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off"))
        {
            return;
        }
        let cwd = env::current_dir().unwrap_or_default();
        let config = runtime::ConfigLoader::default_for(&cwd).load().ok();
        let memory_on = config.as_ref().is_none_or(runtime::memory_enabled);
        let reflect_on = config
            .as_ref()
            .and_then(|config| config.get("memoryReflectOnExit"))
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        if !memory_on || !reflect_on {
            return;
        }
        let Some(prompt) = repl_commands::prompt_for_command("reflect", None) else {
            return;
        };
        println!(
            "  {TOOL_DIM_STYLE}⎿ Saving what this session learned to memory (Esc skips)…{STYLE_RESET}"
        );
        if let Err(error) = self.run_turn(&prompt) {
            eprintln!("reflection skipped: {error}");
        }
    }

    pub(crate) fn last_turn_was_stopped(&self) -> bool {
        self.runtime
            .session()
            .messages
            .last()
            .is_some_and(|message| {
                message.blocks.iter().any(|block| {
                    matches!(block, runtime::ContentBlock::Text { text } if text == "[Request interrupted by user]")
                })
            })
    }

    pub(crate) fn run_turn_inner(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        if claw_engine::requested() {
            return self.run_engine_turn(input);
        }
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(true)?;
        runtime::turn_keys::set_activity_color(&render::activity_color(render::current_theme()));
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        let turn_started = std::time::Instant::now();
        let attachments = self.pending_attachments.clone();
        let result = self.run_watched_attempt(
            &mut runtime,
            hook_abort_monitor,
            input,
            Some(attachments.clone()),
        );
        match result {
            Ok(summary) => {
                // Providers that stream text have already shown the reply;
                // only print it here for ones whose stream showed nothing.
                let streamed_text = runtime
                    .runtime
                    .as_mut()
                    .is_some_and(|rt| rt.api_client_mut().streamed_text());
                self.pending_attachments.clear();
                self.replace_runtime(runtime)?;
                // Decide in the background whether this turn taught
                // anything worth keeping (memories, skills).
                let added = 1 + summary.assistant_messages.len() + summary.tool_results.len();
                let messages = &self.runtime.session().messages;
                learning::after_turn(messages, &messages[messages.len().saturating_sub(added)..]);
                spinner.finish(
                    &format!("done in {:.1}s", turn_started.elapsed().as_secs_f64()),
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                let final_text = final_assistant_text(&summary);
                if !streamed_text && !final_text.is_empty() {
                    println!("{final_text}");
                }
                if !final_text.is_empty() {
                    let rendered = TerminalRenderer::new().markdown_to_ansi(&final_text);
                    runtime::turn_keys::screen::focus_write(
                        format!("\r\n{}\r\n", rendered.replace('\n', "\r\n")).as_bytes(),
                    );
                }
                println!();
                if let Some(event) = summary.auto_compaction {
                    println!("{}", auto_compaction_notice(&event));
                }
                self.persist_session()?;
                Ok(())
            }
            Err(error) if error.to_string().contains(INTERRUPTED_BY_USER) => {
                self.finish_stopped_turn(runtime, &mut spinner, &mut stdout)
            }
            Err(error) => {
                runtime.shutdown_plugins()?;
                spinner.fail(
                    "request failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;

                // ============================================================================
                // Auto-compact retry on context window errors
                // ============================================================================
                // When the model API returns a context_window_blocked error (because the request
                // exceeds the model's context window), we automatically:
                // 1. Compact the session (remove old messages to free up space)
                // 2. Retry the original request with the compacted session
                // 3. Report results to the user
                //
                // This eliminates the need for users to manually run /compact when they
                // hit context limits - the recovery happens automatically.
                //
                // Detection: We look for "context_window" or "Context window" in the error
                // message, which covers error types like:
                // - "context_window_blocked"
                // - "Context window blocked"
                // - "This model's maximum context length is X tokens..."
                // ============================================================================

                let error_str = error.to_string();
                // Detect context window overflow. Some providers (e.g. OpenAI-compat backends)
                // return 400 with "no parseable body" instead of a proper context_length_exceeded
                // error when the request is too large to even parse — treat that as context overflow too.
                // Also detect model-specific context error markers (e.g. llama.cpp returns
                // "Context size has been exceeded." / "exceed_context_size_error" / "exceeds the available context size").
                let is_context_window = error_str.contains("context_window")
                    || error_str.contains("Context window")
                    || error_str.contains("no parseable body")
                    || error_str.contains("exceed_context_size")
                    || error_str.contains("exceeds the available context size")
                    || error_str
                        .to_ascii_lowercase()
                        .contains("context size has been exceeded");

                // Also treat "assistant stream produced no content" and reqwest decode failures
                // as recoverable errors that may benefit from auto-compaction. Some backends (e.g.
                // llama.cpp) return a non-SSE HTTP 500 body when context overflows, causing
                // reqwest to fail with "error decoding response body" — treat that as context overflow too.
                // A timed-out body read surfaces with the same "error decoding response body"
                // message, but compacting the session cannot fix it.
                let is_no_content = error_str.contains("assistant stream produced no content")
                    || error_str.contains("Failed to parse input at pos")
                    || (error_str.contains("error decoding response body")
                        && !error_str.contains("timed out"));

                if is_context_window || is_no_content {
                    // If the error tells us the server's actual context window, adapt our
                    // auto-compaction threshold so future auto-compact-trigger checks are accurate.
                    if let Some(window) = extract_context_window_tokens_from_error(&error_str) {
                        // Same share of the window as a known window gets up front.
                        let threshold = auto_compaction_threshold_for_window(
                            window,
                            max_tokens_for_model(&self.model),
                        );
                        println!(
                            "  Server context window: {} tokens — setting auto-compaction threshold to {}",
                            window, threshold
                        );
                        runtime.set_auto_compaction_input_tokens_threshold(threshold);
                        // Each turn builds a fresh runtime: remember the window
                        // so later turns compact (and budget replies) in time.
                        if self.model.starts_with("local/") {
                            api::note_local_context_window(&self.model, window);
                        }
                    }

                    // A single compaction pass may not free enough context space.
                    // Progressive retry: each round preserves fewer recent messages (4→2→1→0),
                    // trading conversation continuity for a smaller payload until it fits.
                    // Max 4 rounds before giving up and surfacing the error to the user.
                    let max_compact_rounds = 4;
                    let preserve_schedule = [4, 2, 1, 0];
                    // The latest overflow error: it may name the window.
                    let mut overflow_error = error_str.clone();

                    for (round, &preserve) in preserve_schedule.iter().enumerate() {
                        // Kept messages go word for word, so one huge tool
                        // result (a whole large file) can overflow the
                        // window by itself. Worked out each round, from
                        // the request that just overflowed.
                        let max_tool_result_chars = overflow_tool_result_limit(
                            &overflow_error,
                            &self.model,
                            runtime.session(),
                        );
                        println!(
                            "  Auto-compacting session (round {}/{}, preserving {} recent messages)...",
                            round + 1,
                            max_compact_rounds,
                            preserve
                        );

                        // Run Trident pipeline then summary-based compaction
                        let result = runtime::trident::trident_compact_session(
                            runtime.session(),
                            CompactionConfig {
                                preserve_recent_messages: preserve,
                                max_estimated_tokens: 0,
                            },
                            &runtime::trident::TridentConfig::default(),
                        );
                        let removed = result.removed_message_count;
                        let mut compacted_session = result.compacted_session;
                        let cut = max_tool_result_chars.map_or(0, |limit| {
                            runtime::truncate_large_tool_results(&mut compacted_session, limit)
                        });

                        if removed == 0 && cut == 0 {
                            // Nothing changed: the same request would fail
                            // the same way, so go straight to a harder round.
                            if round + 1 < max_compact_rounds {
                                continue;
                            }
                            println!("  No further compaction possible.");
                            break;
                        }
                        if cut > 0 {
                            println!(
                                "  Cut {cut} oversized tool result(s) to fit the context window."
                            );
                        }

                        if removed > 0 {
                            println!(
                                "{}",
                                format_compact_report(
                                    removed,
                                    compacted_session.messages.len(),
                                    false
                                )
                            );
                        }

                        // The compacted session holds this turn's prompt and
                        // the work done so far: continue from there. Sending
                        // the prompt again started the work over each round,
                        // so it overflowed the same way every time. Only when
                        // compaction removed the prompt is it sent again.
                        let continues = compacted_session.messages.last().is_some_and(|message| {
                            matches!(message.role, MessageRole::User | MessageRole::Tool)
                        });

                        // Without this, prepare_turn_runtime() reads from self.runtime.session()
                        // which still holds the ORIGINAL un-compacted session, so every retry round
                        // would send the same bloated request — compaction was wasted.
                        *self.runtime.session_mut() = compacted_session;

                        // Build a new runtime with the compacted session and retry
                        let (mut new_runtime, hook_abort_monitor) =
                            self.prepare_turn_runtime(true)?;

                        // The retry is a whole turn of its own (often many
                        // tool calls): it needs the key watcher too, or the
                        // activity block, Esc and scrolling are gone for it.
                        match self.run_watched_attempt(
                            &mut new_runtime,
                            hook_abort_monitor,
                            input,
                            (!continues).then(|| attachments.clone()),
                        ) {
                            Ok(summary) => {
                                self.pending_attachments.clear();
                                self.replace_runtime(new_runtime)?;
                                spinner.finish(
                                    if round == 0 {
                                        "done (after auto-compact)"
                                    } else {
                                        "done (after aggressive auto-compact)"
                                    },
                                    TerminalRenderer::new().color_theme(),
                                    &mut stdout,
                                )?;
                                println!();
                                if let Some(event) = summary.auto_compaction {
                                    println!("{}", auto_compaction_notice(&event));
                                }
                                self.persist_session()?;
                                return Ok(());
                            }
                            Err(retry_error)
                                if retry_error.to_string().contains(INTERRUPTED_BY_USER) =>
                            {
                                return self.finish_stopped_turn(
                                    new_runtime,
                                    &mut spinner,
                                    &mut stdout,
                                );
                            }
                            Err(retry_error) => {
                                let retry_str = retry_error.to_string();
                                let still_context_window = retry_str.contains("context_window")
                                    || retry_str.contains("Context window")
                                    || retry_str.contains("no parseable body")
                                    || retry_str.contains("exceed_context_size")
                                    || retry_str.contains("exceeds the available context size")
                                    || retry_str
                                        .to_ascii_lowercase()
                                        .contains("context size has been exceeded");
                                let still_no_content = retry_str
                                    .contains("assistant stream produced no content")
                                    || retry_str.contains("Failed to parse input at pos")
                                    || (retry_str.contains("error decoding response body")
                                        && !retry_str.contains("timed out"));

                                if (still_context_window || still_no_content)
                                    && round + 1 < max_compact_rounds
                                {
                                    // If the retry error reveals the context window, adapt threshold.
                                    if let Some(window) =
                                        extract_context_window_tokens_from_error(&retry_str)
                                    {
                                        let threshold: u32 = (window as f64 * 0.7).round() as u32;
                                        new_runtime
                                            .set_auto_compaction_input_tokens_threshold(threshold);
                                    }

                                    // The compacted session was still too large for the model's context.
                                    // Shut down the old runtime, adopt the partially-compacted one,
                                    // and loop — the next round will compact more aggressively.
                                    runtime.shutdown_plugins()?;
                                    runtime = new_runtime;
                                    overflow_error = retry_str;
                                    continue;
                                }

                                // Not a context window error, or out of rounds
                                return Err(Box::new(retry_error));
                            }
                        }
                    }
                }

                // If not a context window error, return original error
                Err(Box::new(error))
            }
        }
    }

    pub(crate) fn run_turn_with_output(
        &mut self,
        input: &str,
        output_format: CliOutputFormat,
        compact: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match output_format {
            CliOutputFormat::Json if compact => self.run_prompt_compact_json(input),
            CliOutputFormat::Text if compact => self.run_prompt_compact(input),
            CliOutputFormat::Text => self.run_turn(input),
            CliOutputFormat::Json => self.run_prompt_json(input),
        }
    }

    pub(crate) fn run_prompt_compact(
        &mut self,
        input: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        let final_text = final_assistant_text(&summary);
        println!("{final_text}");
        Ok(())
    }

    pub(crate) fn run_prompt_compact_json(
        &mut self,
        input: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "compact": true,
                "model": self.model,
                "usage": {
                    "input_tokens": summary.usage.input_tokens,
                    "output_tokens": summary.usage.output_tokens,
                    "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                },
            })
        );
        Ok(())
    }

    pub(crate) fn run_prompt_json(
        &mut self,
        input: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "model": self.model,
                "iterations": summary.iterations,
                "auto_compaction": summary.auto_compaction.map(|event| json!({
                    "removed_messages": event.removed_message_count,
                    "notice": auto_compaction_notice(&event),
                })),
                "tool_uses": collect_tool_uses(&summary),
                "tool_results": collect_tool_results(&summary),
                "prompt_cache_events": collect_prompt_cache_events(&summary),
                "usage": {
                    "input_tokens": summary.usage.input_tokens,
                    "output_tokens": summary.usage.output_tokens,
                    "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                },
                "estimated_cost": format_usd(
                    summary.usage.estimate_cost_usd_with_pricing(
                        pricing_for_model(&self.model)
                            .unwrap_or_else(runtime::ModelPricing::default_sonnet_tier)
                    ).total_cost_usd()
                )
            })
        );
        Ok(())
    }

    pub(crate) fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self
            .runtime
            .compact_with_model_summary(CompactionConfig::default());
        let removed = result.removed_message_count;
        let kept = result.compacted_session.messages.len();
        let skipped = removed == 0;
        let runtime = build_runtime(
            result.compacted_session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!("{}", format_compact_report(removed, kept, skipped));
        Ok(())
    }

    pub(crate) fn run_internal_prompt_text_with_progress(
        &self,
        prompt: &str,
        enable_tools: bool,
        progress: Option<InternalPromptProgressReporter>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let mut runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            enable_tools,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            progress,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let summary = runtime.run_turn(prompt, Some(&mut permission_prompter))?;
        let text = final_assistant_text(&summary).trim().to_string();
        runtime.shutdown_plugins()?;
        Ok(text)
    }

    pub(crate) fn run_internal_prompt_text(
        &self,
        prompt: &str,
        enable_tools: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.run_internal_prompt_text_with_progress(prompt, enable_tools, None)
    }

    pub(crate) fn run_bughunter(
        &self,
        scope: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_bughunter_report(scope));
        Ok(())
    }

    /// `/ultraplan <task>`: Claude Code drafted plans in the cloud; here a
    /// read-only planning turn researches with parallel explorers and
    /// writes the plan to `.claw/plans/`, without changing code.
    pub(crate) fn run_ultraplan(
        &mut self,
        task: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(task) = task.map(str::trim).filter(|task| !task.is_empty()) else {
            println!("usage: /ultraplan <task>   e.g. /ultraplan move the session store to SQLite");
            return Ok(false);
        };
        println!("{}", format_ultraplan_report(Some(task)));
        report_repl_turn_error(self.run_turn(&ultraplan_prompt(task)));
        Ok(true)
    }

    pub(crate) fn run_teleport(target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
            println!("Usage: /teleport <symbol-or-path>");
            return Ok(());
        };

        println!("{}", render_teleport_report(target)?);
        Ok(())
    }

    pub(crate) fn run_debug_tool_call(
        &self,
        args: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/debug-tool-call", args)?;
        println!("{}", render_last_tool_debug_report(self.runtime.session())?);
        Ok(())
    }

    pub(crate) fn run_commit(
        &mut self,
        args: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/commit", args)?;
        let status = git_output(&["status", "--short", "--branch"])?;
        let summary = parse_git_workspace_summary(Some(&status));
        let branch = parse_git_status_branch(Some(&status));
        if summary.is_clean() {
            println!("{}", format_commit_skipped_report());
            return Ok(());
        }

        println!(
            "{}",
            format_commit_preflight_report(branch.as_deref(), summary)
        );
        Ok(())
    }

    pub(crate) fn run_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let branch =
            resolve_git_branch_for(&env::current_dir()?).unwrap_or_else(|| "unknown".to_string());
        println!("{}", format_pr_report(&branch, context));
        Ok(())
    }

    pub(crate) fn run_issue(
        &self,
        context: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_issue_report(context));
        Ok(())
    }
}
