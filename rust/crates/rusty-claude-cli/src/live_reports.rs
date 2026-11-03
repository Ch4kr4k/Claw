//! Read-only reports the REPL prints: `/status`, `/history`, `/sandbox`,
//! `/cost`, `/config`, `/memory`, `/agents`, `/mcp`, `/skills`, `/plugins`,
//! `/diff` and `/version`.

#[allow(clippy::wildcard_imports)]
use crate::*;

impl LiveCli {
    pub(crate) fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        println!(
            "{}",
            format_status_report(
                &self.model,
                StatusUsage {
                    message_count: self.runtime.session().messages.len(),
                    turns: self.runtime.usage().turns(),
                    latest,
                    cumulative,
                    estimated_tokens: self.runtime.estimated_tokens(),
                },
                self.permission_mode.as_str(),
                &status_context(Some(&self.session.path)).expect("status context should load"),
                None, // #148: REPL /status doesn't carry flag provenance
                None,
            )
        );
    }

    pub(crate) fn print_prompt_history(&self, count: Option<&str>) {
        let limit = match parse_history_count(count) {
            Ok(limit) => limit,
            Err(message) => {
                eprintln!("{message}");
                return;
            }
        };
        let session_entries = &self.runtime.session().prompt_history;
        let entries = if session_entries.is_empty() {
            if self.prompt_history.is_empty() {
                collect_session_prompt_history(self.runtime.session())
            } else {
                self.prompt_history
                    .iter()
                    .map(|entry| PromptHistoryEntry {
                        timestamp_ms: entry.timestamp_ms,
                        text: entry.text.clone(),
                    })
                    .collect()
            }
        } else {
            session_entries
                .iter()
                .map(|entry| PromptHistoryEntry {
                    timestamp_ms: entry.timestamp_ms,
                    text: entry.text.clone(),
                })
                .collect()
        };
        println!("{}", render_prompt_history_report(&entries, limit));
    }

    pub(crate) fn print_sandbox_status() {
        let cwd = env::current_dir().expect("current dir");
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader
            .load()
            .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
        println!(
            "{}",
            format_sandbox_report(&resolve_sandbox_status(runtime_config.sandbox(), &cwd))
        );
    }

    pub(crate) fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        println!("{}", format_cost_report(cumulative));
    }

    pub(crate) fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_config_report(section)?);
        Ok(())
    }

    pub(crate) fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_memory_report()?);
        Ok(())
    }

    pub(crate) fn print_agents(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_agents_slash_command(args, &cwd)?),
            CliOutputFormat::Json => {
                let value = handle_agents_slash_command_json(args, &cwd)?;
                // #789: parity with print_mcp/#788 print_skills — exit 1 when envelope
                // reports an error so automation can rely on exit code instead of
                // parsing the JSON status field.
                let is_error = value.get("status").and_then(|v| v.as_str()) == Some("error");
                println!("{}", serde_json::to_string_pretty(&value)?);
                if is_error {
                    std::process::exit(1);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn print_mcp(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // `claw mcp serve` starts a stdio MCP server exposing claw's built-in
        // tools. All other `mcp` subcommands fall through to the existing
        // configured-server reporter (`list`, `status`, ...).
        if matches!(args.map(str::trim), Some("serve")) {
            return run_mcp_serve();
        }
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_mcp_slash_command(args, &cwd)?),
            CliOutputFormat::Json => {
                let value = handle_mcp_slash_command_json(args, &cwd)?;
                // Propagate ok:false → non-zero exit so automation callers
                // can rely on exit code instead of inspecting the envelope.
                // (#68: mcp error envelopes previously always exited 0.)
                let is_error = value.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
                    || value.get("status").and_then(serde_json::Value::as_str) == Some("error");
                println!("{}", serde_json::to_string_pretty(&value)?);
                if is_error {
                    std::process::exit(1);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn print_skills(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_skills_slash_command(args, &cwd)?),
            CliOutputFormat::Json => {
                let result = handle_skills_slash_command_json(args, &cwd)?;
                let is_error = result.get("status").and_then(|v| v.as_str()) == Some("error");
                // #739: action:"help" with unexpected set is a usage response, not a fatal error;
                // don't return Err which would emit a second error envelope from the generic path.
                let is_help_action = result.get("action").and_then(|v| v.as_str()) == Some("help");
                println!("{}", serde_json::to_string_pretty(&result)?);
                if is_error && !is_help_action {
                    // #788: the error JSON is already emitted above; returning Err here
                    // would cause the top-level handler to emit a second error envelope.
                    // Exit directly to signal failure without a duplicate envelope.
                    std::process::exit(1);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn print_plugins(
        action: Option<&str>,
        target: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        // #803: reject flag-shaped tokens in list filter for BOTH text and JSON modes.
        // Previously the guard was JSON-only (#793); text mode silently returned empty success.
        if action == Some("list") {
            if let Some(filter) = target {
                if filter.starts_with('-') {
                    if matches!(output_format, CliOutputFormat::Json) {
                        // ROADMAP #817: this is a handled local inventory parse error.
                        // Keep it on stdout in JSON mode so `plugins list --` matches the
                        // sibling JSON inventory/local surfaces instead of falling through
                        // to the top-level stderr error path.
                        let obj = json!({
                            "type": "error",
                            "kind": "plugin",
                            "action": "list",
                            "status": "error",
                            "error_kind": "cli_parse",
                            "error": format!("unknown option for `claw plugins list`: {filter}"),
                            "message": format!("unknown option for `claw plugins list`: {filter}"),
                            "unexpected": filter,
                            "hint": "Usage: claw plugins list [<filter>]\nFilters are id substrings, not flags.",
                            "exit_code": 1,
                        });
                        println!("{}", serde_json::to_string_pretty(&obj)?);
                        std::process::exit(1);
                    }
                    return Err(format!(
                        "unknown option for `claw plugins list`: {filter}\nUsage: claw plugins list [<filter>]\nFilters are id substrings, not flags."
                    ).into());
                }
            }
        }
        let payload = plugins_command_payload_for(
            &cwd,
            action,
            target,
            match output_format {
                CliOutputFormat::Json => ConfigWarningMode::SuppressStderr,
                CliOutputFormat::Text => ConfigWarningMode::EmitStderr,
            },
        )?;
        match output_format {
            CliOutputFormat::Text => {
                // #806: text-mode show must return error when plugin not found (parity with JSON)
                let action_str = action.unwrap_or("list");
                if matches!(action_str, "show" | "info" | "describe") {
                    if let Some(name) = target {
                        let needle = name.to_lowercase();
                        let found = payload.plugins.iter().any(|p| {
                            p.get("id")
                                .and_then(|v| v.as_str())
                                .map(|id| id.to_lowercase() == needle)
                                .unwrap_or(false)
                        });
                        if !found {
                            return Err(format!(
                                "plugin_not_found: plugin '{}' not found\nRun `claw plugins list` to see available plugins.",
                                name
                            ).into());
                        }
                    }
                }
                println!("{}", payload.message);
            }
            CliOutputFormat::Json => {
                let action_str = action.unwrap_or("list");
                // #743/#420: plugins help must return a usage envelope matching agents/mcp/skills help shape.
                if matches!(action_str, "help" | "-h" | "--help") {
                    let cwd_str = cwd.display().to_string();
                    let obj = json!({
                        "kind": "plugin",
                        "action": "help",
                        "status": "ok",
                        "unexpected": null,
                        "usage": {
                            "direct_cli": "claw plugins [list|show <id>|install <id>|enable <id>|disable <id>|uninstall <id>|update <id>|help]",
                            "slash_command": "/plugins [list|show <id>|install <id>|enable <id>|disable <id>|uninstall <id>|update <id>|help]",
                        },
                        "cwd": cwd_str,
                    });
                    println!("{}", serde_json::to_string_pretty(&obj)?);
                    return Ok(());
                }
                // For show/info/describe, filter to the named plugin (exact match).
                // For list with a target, treat target as a substring filter.
                let is_show_action = matches!(action_str, "show" | "info" | "describe");
                let is_list_action = action_str == "list";
                let filtered_plugins: Vec<_> = if is_show_action {
                    if let Some(name) = target {
                        let needle = name.to_lowercase();
                        payload
                            .plugins
                            .iter()
                            .filter(|p| {
                                p.get("id")
                                    .and_then(|v| v.as_str())
                                    .map(|id| id.to_lowercase() == needle)
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect()
                    } else {
                        payload.plugins.clone()
                    }
                } else if is_list_action {
                    if let Some(filter) = target {
                        let needle = filter.to_lowercase();
                        payload
                            .plugins
                            .iter()
                            .filter(|p| {
                                p.get("id")
                                    .and_then(|v| v.as_str())
                                    .map(|id| id.to_lowercase().contains(&needle))
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect()
                    } else {
                        payload.plugins.clone()
                    }
                } else {
                    payload.plugins.clone()
                };
                // Return not-found error for show with missing target.
                if is_show_action {
                    if let Some(name) = target {
                        if filtered_plugins.is_empty() {
                            let obj = json!({
                                "kind": "plugin",
                                "action": action_str,
                                "status": "error",
                                "error_kind": "plugin_not_found",
                                "requested": name,
                                // #734: parity with skills show which always emits a message field
                                "message": format!("plugin '{}' not found", name),
                                // #760: hint so callers know how to enumerate available plugins
                                "hint": "Run `claw plugins list` to see available plugins.",
                            });
                            println!("{}", serde_json::to_string_pretty(&obj)?);
                            // #789: exit 1 on not-found so automation can rely on exit code
                            std::process::exit(1);
                        }
                    }
                }
                let enabled_count = filtered_plugins
                    .iter()
                    .filter(|p| p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
                    .count();
                let disabled_count = filtered_plugins.len().saturating_sub(enabled_count);
                let mut obj = json!({
                    "kind": "plugin",
                    "action": action_str,
                    "status": payload.status,
                    "summary": {
                        "total": filtered_plugins.len(),
                        "enabled": enabled_count,
                        "disabled": disabled_count,
                        "load_failures": payload.load_failures.len(),
                    },
                    "config_load_error": payload.config_load_error,
                    "mcp_validation": payload.mcp_validation.json_value(),
                    "plugins": filtered_plugins,
                    "load_failures": payload.load_failures,
                });
                // Only include operation-result fields for mutating actions (not list/show)
                if action_str != "list" && !is_show_action {
                    obj["target"] = json!(target);
                    obj["reload_runtime"] = json!(payload.reload_runtime);
                    obj["message"] = json!(payload.message);
                }
                println!("{}", serde_json::to_string_pretty(&obj)?);
            }
        }
        Ok(())
    }

    /// `/diff`: on a terminal, the colored per-file view the diff panel uses
    /// (totals, one row per file, then highlighted hunks; new files
    /// included); piped, or with `NO_COLOR`, the plain `git diff` report.
    pub(crate) fn print_diff(mode: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(mode) = mode {
            println!("{}", render_diff_summary_for(&env::current_dir()?, mode)?);
            return Ok(());
        }
        if io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none() {
            let files = diff_panel::collect(&env::current_dir()?).unwrap_or_default();
            if !files.is_empty() {
                let width = crossterm::terminal::size()
                    .map_or(100, |(cols, _)| usize::from(cols))
                    .max(40);
                let mut out = io::stdout().lock();
                for row in diff_panel::render(&files, width) {
                    writeln!(out, "{}", row.trim_end())?;
                }
                return Ok(());
            }
        }
        println!("{}", render_diff_report()?);
        Ok(())
    }

    pub(crate) fn print_version(output_format: CliOutputFormat) {
        let _ = crate::print_version(output_format);
    }
}
