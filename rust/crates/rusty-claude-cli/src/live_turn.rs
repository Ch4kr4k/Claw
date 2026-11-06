//! Running turns: preparing the runtime for a prompt, the watched attempt
//! and its retries (stop, LeanGate, compaction on overflow), one-shot
//! prompt modes (`-p`, `--compact`, JSON), and the prompt-backed commands
//! (`/bughunter`, `/ultraplan`, `/commit`, `/pr`, …).

#[allow(clippy::wildcard_imports)]
use crate::*;

impl LiveCli {
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

    /// One turn on the engine (see `live_engine.rs`).
    pub(crate) fn run_turn_inner(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.run_engine_turn(input)
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
        let summary = self.run_engine_turn_quiet(input)?;
        let final_text = final_assistant_text(&summary);
        println!("{final_text}");
        Ok(())
    }

    pub(crate) fn run_prompt_compact_json(
        &mut self,
        input: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let summary = self.run_engine_turn_quiet(input)?;
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
        let summary = self.run_engine_turn_quiet(input)?;
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
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
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
        self.run_engine_side_prompt(prompt, enable_tools, progress.as_ref())
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
