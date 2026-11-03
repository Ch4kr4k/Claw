//! Command-line parsing: the flags and subcommands of `claw` into a
//! [`CliAction`].

#[allow(clippy::wildcard_imports)]
use crate::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CliAction {
    /// `claw desktop-agent`: one session for the desktop app over JSON lines.
    DesktopAgent {
        model: String,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        resume: Option<String>,
        /// Keep nothing on disk: the session file is removed and never saved.
        incognito: bool,
        allow_broad_cwd: bool,
    },
    /// `claw desktop`: start the desktop app.
    Desktop,
    DumpManifests {
        output_format: CliOutputFormat,
        manifests_dir: Option<PathBuf>,
    },
    BootstrapPlan {
        output_format: CliOutputFormat,
    },
    Agents {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Mcp {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Skills {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Plugins {
        action: Option<String>,
        target: Option<String>,
        output_format: CliOutputFormat,
    },
    PrintSystemPrompt {
        cwd: PathBuf,
        date: String,
        model: String,
        output_format: CliOutputFormat,
    },
    Version {
        output_format: CliOutputFormat,
    },
    SessionList {
        output_format: CliOutputFormat,
    },
    ResumeSession {
        session_path: PathBuf,
        commands: Vec<String>,
        output_format: CliOutputFormat,
        allow_broad_cwd: bool,
    },
    Status {
        model: String,
        // #148: raw `--model` flag input (pre-alias-resolution), if any.
        // None means no flag was supplied; env/config/default fallback is
        // resolved inside `print_status_snapshot`.
        model_flag_raw: Option<String>,
        permission_mode: PermissionModeProvenance,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
    },
    Sandbox {
        output_format: CliOutputFormat,
    },
    Prompt {
        prompt: String,
        model: String,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        compact: bool,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
    },
    Doctor {
        output_format: CliOutputFormat,
        permission_mode: PermissionModeProvenance,
    },
    Acp {
        output_format: CliOutputFormat,
    },
    State {
        output_format: CliOutputFormat,
    },
    Init {
        output_format: CliOutputFormat,
    },
    /// `claw trust`: review and approve the project config files in the
    /// current directory, whose settings are ignored until then.
    Trust,
    Setup {
        output_format: CliOutputFormat,
    },
    // #146: `claw config` and `claw diff` are pure-local read-only
    // introspection commands; wire them as standalone CLI subcommands.
    Config {
        section: Option<String>,
        output_format: CliOutputFormat,
    },
    Models {
        action: Option<String>,
        output_format: CliOutputFormat,
    },
    Diff {
        output_format: CliOutputFormat,
    },
    Export {
        session_reference: String,
        output_path: Option<PathBuf>,
        output_format: CliOutputFormat,
    },
    Repl {
        model: String,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        /// `claw --resume [session]`: continue this session.
        resume: Option<String>,
    },
    HelpTopic {
        topic: LocalHelpTopic,
        output_format: CliOutputFormat,
    },
    // prompt-mode formatting is only supported for non-interactive runs
    Help {
        output_format: CliOutputFormat,
    },
}

#[allow(clippy::too_many_lines)]
pub(crate) fn parse_args(args: &[String]) -> Result<CliAction, String> {
    let mut model = DEFAULT_MODEL.to_string();
    // #148: when user passes --model/--model=, capture the raw input so we
    // can attribute source: "flag" later. None means no flag was supplied.
    let mut model_flag_raw: Option<String> = None;
    let mut output_format_selection = if cli_has_output_format_flag(args) {
        OutputFormatSelection::default()
    } else {
        output_format_selection_from_env()?
    };
    set_current_output_format_selection(&output_format_selection);
    let mut output_format = output_format_selection.format;
    let mut permission_mode_override = None;
    let mut wants_help = false;
    let mut wants_version = false;
    let mut allowed_tool_values = Vec::new();
    let mut compact = false;
    let mut base_commit: Option<String> = None;
    let mut reasoning_effort: Option<String> = None;
    let mut allow_broad_cwd = false;

    // #755: -p prompt text captured as single token; remaining args continue
    // flag parsing. None until `-p <text>` is seen.
    let mut short_p_prompt: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut positional_after_separator = false;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" if rest.is_empty() => {
                wants_help = true;
                index += 1;
            }
            "--help" | "-h"
                if !rest.is_empty()
                    && matches!(rest[0].as_str(), "prompt" | "commit" | "pr" | "issue") =>
            {
                // `--help` following a subcommand that would otherwise forward
                // the arg to the API (e.g. `claw prompt --help`) should show
                // top-level help instead. Subcommands that consume their own
                // args (agents, mcp, plugins, skills) and local help-topic
                // subcommands (status, sandbox, doctor, init, state, export,
                // version, system-prompt, dump-manifests, bootstrap-plan) must
                // NOT be intercepted here — they handle --help in their own
                // dispatch paths via parse_local_help_action(). See #141.
                wants_help = true;
                index += 1;
            }
            "--version" | "-V" => {
                wants_version = true;
                index += 1;
            }
            "--model" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing_flag_value: missing value for --model.\nUsage: --model <provider/model>  e.g. --model anthropic/claude-opus-4-7".to_string())?;
                // #468: track duplicate --model flags
                if model_flag_raw.is_some() {
                    push_duplicate_flag(&format!(
                        "--model (previous: {}, new: {})",
                        model_flag_raw.as_deref().unwrap_or(""),
                        value
                    ));
                }
                let resolved = resolve_model_alias_with_config(value);
                debug!("Resolved --model '{}' -> '{}'", value, resolved);
                validate_model_syntax(&resolved)?;
                model = resolved;
                model_flag_raw = Some(value.clone()); // #148
                index += 2;
            }

            flag if flag.starts_with("--model=") => {
                let value = &flag[8..];
                let resolved = resolve_model_alias_with_config(value);
                debug!("Resolved --model='{}' -> '{}'", value, resolved);
                validate_model_syntax(&resolved)?;
                model = resolved;
                model_flag_raw = Some(value.to_string()); // #148
                index += 1;
            }
            "--output-format" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing_flag_value: missing value for --output-format.\nUsage: --output-format text  or  --output-format json".to_string())?;
                // #468: track duplicate --output-format flags
                if output_format != CliOutputFormat::Text
                    || output_format_selection.format != CliOutputFormat::Text
                {
                    push_duplicate_flag("--output-format (overwriting previous value)");
                }
                output_format = apply_output_format_flag(&mut output_format_selection, value)?;
                index += 2;
            }
            "--permission-mode" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing_flag_value: missing value for --permission-mode.\nUsage: --permission-mode read-only|workspace-write|danger-full-access".to_string())?;
                // #468: track duplicate --permission-mode flags
                if permission_mode_override.is_some() {
                    push_duplicate_flag("--permission-mode (overwriting previous value)");
                }
                permission_mode_override = Some(parse_permission_mode_arg(value)?);
                index += 2;
            }

            flag if flag.starts_with("--output-format=") => {
                output_format =
                    apply_output_format_flag(&mut output_format_selection, &flag[16..])?;
                index += 1;
            }
            flag if flag.starts_with("--permission-mode=") => {
                permission_mode_override = Some(parse_permission_mode_arg(&flag[18..])?);
                index += 1;
            }
            "--dangerously-skip-permissions" | "--skip-permissions" => {
                permission_mode_override = Some(PermissionMode::DangerFullAccess);
                index += 1;
            }
            // Plan / dry run: read-only tools, and the model is told to
            // reply with the exact commands and edits it would make.
            "--plan" | "--dry-run" => {
                if permission_mode_override.is_some() {
                    push_duplicate_flag("--plan (overwriting previous permission mode)");
                }
                permission_mode_override = Some(PermissionMode::ReadOnly);
                index += 1;
            }
            "--compact" => {
                compact = true;
                index += 1;
            }
            "--base-commit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing_flag_value: missing value for --base-commit.\nUsage: --base-commit <git-sha>".to_string())?;
                // #122: validate that base-commit looks like a git SHA (hex, 7-64 chars)
                if value.len() < 7
                    || value.len() > 64
                    || !value.chars().all(|c| c.is_ascii_hexdigit())
                {
                    return Err(format!(
                        "invalid_flag_value: --base-commit expects a hex SHA (7-64 chars), got '{}'.\nUsage: --base-commit <git-sha>",
                        value
                    ));
                }
                base_commit = Some(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--base-commit=") => {
                base_commit = Some(flag[14..].to_string());
                index += 1;
            }
            "--reasoning-effort" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing_flag_value: missing value for --reasoning-effort.\nUsage: --reasoning-effort low|medium|high".to_string())?;
                if !matches!(value.as_str(), "low" | "medium" | "high") {
                    return Err(format!(
                        "invalid_flag_value: invalid value for --reasoning-effort: '{value}'.\nUsage: --reasoning-effort low|medium|high"
                    ));
                }
                reasoning_effort = Some(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--reasoning-effort=") => {
                let value = &flag[19..];
                if !matches!(value, "low" | "medium" | "high") {
                    return Err(format!(
                        "invalid_flag_value: invalid value for --reasoning-effort: '{value}'.\nUsage: --reasoning-effort low|medium|high"
                    ));
                }
                reasoning_effort = Some(value.to_string());
                index += 1;
            }
            "--allow-broad-cwd" => {
                allow_broad_cwd = true;
                index += 1;
            }
            "--" => {
                if rest.is_empty() {
                    positional_after_separator = true;
                    rest.extend(args[index + 1..].iter().cloned());
                } else {
                    rest.push("--".to_string());
                    rest.extend(args[index + 1..].iter().cloned());
                }
                break;
            }
            "-p" => {
                // Claw Code compat: -p "prompt" = one-shot prompt.
                // #755: consume exactly one token so subsequent flags like
                // --model/--output-format are parsed normally instead of
                // being swallowed into the prompt string (#117).
                let next = args.get(index + 1).map(|s| s.as_str());
                match next {
                    None | Some("") => {
                        return Err("missing_prompt: -p requires a prompt string.\nUsage: claw -p <text>  or  claw prompt <text>".to_string());
                    }
                    Some(tok) if tok.starts_with('-') && tok != "--" => {
                        // Looks like a flag, not a prompt. Reject so the user
                        // knows to quote the literal text or use `--`.
                        return Err(format!(
                            "missing_prompt: -p requires a prompt string before flags; got `{tok}`.\nUsage: claw -p <text> --model sonnet  or  claw -p -- {tok} (literal)"
                        ));
                    }
                    Some(tok) => {
                        // `--` sentinel: skip it and take the token after as literal
                        let (prompt_text, skip) = if tok == "--" {
                            match args.get(index + 2) {
                                Some(t) => (t.as_str(), 3usize),
                                None => return Err("missing_prompt: -p -- requires a prompt string after `--`.\nUsage: claw -p -- <text>".to_string()),
                            }
                        } else {
                            (tok, 2usize)
                        };
                        if prompt_text.trim().is_empty() {
                            return Err("missing_prompt: -p requires a non-empty prompt string.\nUsage: claw -p <text>  or  claw prompt <text>".to_string());
                        }
                        short_p_prompt = Some(prompt_text.to_string());
                        index += skip;
                        continue;
                    }
                }
            }
            "--print" => {
                // Claw Code compat: --print makes output non-interactive
                output_format = CliOutputFormat::Text;
                index += 1;
            }
            "--resume" if rest.is_empty() => {
                rest.push("--resume".to_string());
                index += 1;
            }
            // #457: --help after --resume should show resume help, not be consumed as session-id
            "--help" | "-h" if rest.first().map(String::as_str) == Some("--resume") => {
                wants_help = true;
                index += 1;
            }
            flag if rest.is_empty() && flag.starts_with("--resume=") => {
                rest.push("--resume".to_string());
                rest.push(flag[9..].to_string());
                index += 1;
            }
            "--acp" | "-acp" => {
                rest.push("acp".to_string());
                index += 1;
            }
            "--allowedTools" | "--allowed-tools" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(allowed_tools_missing_error)?;
                if value.starts_with('-') || is_known_top_level_subcommand(value) {
                    return Err(allowed_tools_missing_error());
                }
                allowed_tool_values.push(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--allowedTools=") => {
                let value = flag[15..].to_string();
                if value.trim().is_empty() {
                    return Err(allowed_tools_missing_error());
                }
                allowed_tool_values.push(value);
                index += 1;
            }
            flag if flag.starts_with("--allowed-tools=") => {
                let value = flag[16..].to_string();
                if value.trim().is_empty() {
                    return Err(allowed_tools_missing_error());
                }
                allowed_tool_values.push(value);
                index += 1;
            }
            other if rest.is_empty() && other.starts_with('-') => {
                if should_reject_unknown_option_like(other) {
                    return Err(format_unknown_option(other));
                }
                rest.push(other.to_string());
                index += 1;
            }
            other => {
                rest.push(other.to_string());
                index += 1;
            }
        }
    }

    if wants_help {
        // #684: --help before subcommand should still route to subcommand-specific
        // help when the subcommand is one of the local-help-topic commands.
        if let Some(action) = parse_local_help_action(&rest, output_format) {
            return action;
        }
        // When --help was consumed before the subcommand, rest has no help flag.
        // If rest is a simple local-help subcommand with no extra args, route there.
        if !rest.is_empty() && rest[1..].iter().all(|a| is_help_flag(a)) {
            let topic = match rest[0].as_str() {
                "status" => Some(LocalHelpTopic::Status),
                "sandbox" => Some(LocalHelpTopic::Sandbox),
                "doctor" => Some(LocalHelpTopic::Doctor),
                "acp" => Some(LocalHelpTopic::Acp),
                "init" => Some(LocalHelpTopic::Init),
                "setup" => Some(LocalHelpTopic::Setup),
                "state" => Some(LocalHelpTopic::State),
                "resume" => Some(LocalHelpTopic::Resume),
                "session" => Some(LocalHelpTopic::Session),
                "compact" => Some(LocalHelpTopic::Compact),
                "--resume" => Some(LocalHelpTopic::Resume),
                "export" => Some(LocalHelpTopic::Export),
                "version" => Some(LocalHelpTopic::Version),
                "system-prompt" => Some(LocalHelpTopic::SystemPrompt),
                "dump-manifests" => Some(LocalHelpTopic::DumpManifests),
                "bootstrap-plan" => Some(LocalHelpTopic::BootstrapPlan),
                "agents" | "agent" => Some(LocalHelpTopic::Agents),
                "skills" | "skill" => Some(LocalHelpTopic::Skills),
                "plugins" | "plugin" | "marketplace" => Some(LocalHelpTopic::Plugins),
                "mcp" => Some(LocalHelpTopic::Mcp),
                "config" => Some(LocalHelpTopic::Config),
                "model" | "models" => Some(LocalHelpTopic::Model),
                "settings" => Some(LocalHelpTopic::Settings),
                "diff" => Some(LocalHelpTopic::Diff),
                _ => None,
            };
            if let Some(topic) = topic {
                return Ok(CliAction::HelpTopic {
                    topic,
                    output_format,
                });
            }
        }
        return Ok(CliAction::Help { output_format });
    }

    if wants_version {
        return Ok(CliAction::Version { output_format });
    }

    let allowed_tools = normalize_allowed_tools(&allowed_tool_values)?;

    // #755: -p consumed exactly one token; dispatch now that all flags are parsed
    if let Some(prompt) = short_p_prompt {
        return Ok(CliAction::Prompt {
            prompt,
            model: resolve_model_alias_with_config(&model),
            output_format,
            allowed_tools,
            permission_mode: permission_mode_override.unwrap_or_else(default_permission_mode),
            compact,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
        });
    }

    if positional_after_separator && !rest.is_empty() {
        let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
        return Ok(CliAction::Prompt {
            prompt: rest.join(" "),
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort: reasoning_effort.clone(),
            allow_broad_cwd,
        });
    }

    if rest.is_empty() {
        let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
        let stdin_is_terminal = std::io::stdin().is_terminal();
        if compact && stdin_is_terminal {
            return Err(compact_missing_argument_error());
        }
        // When stdin is not a terminal (pipe/redirect) and no prompt is given on the
        // command line, read stdin as the prompt and dispatch as a one-shot Prompt
        // rather than starting the interactive REPL (which would consume the pipe and
        // print the startup banner, then exit without sending anything to the API).
        // (Not in tests: see `read_piped_stdin`.)
        if !stdin_is_terminal && !cfg!(test) {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
            let piped = buf.trim().to_string();
            if !piped.is_empty() {
                return Ok(CliAction::Prompt {
                    model,
                    prompt: piped,
                    allowed_tools,
                    permission_mode,
                    output_format,
                    compact,
                    base_commit,
                    reasoning_effort,
                    allow_broad_cwd,
                });
            }
            if compact {
                return Err(compact_missing_argument_error());
            }
            // Non-TTY stdin with no piped content: refuse to start the interactive
            // REPL (it would block forever waiting for input that will never arrive).
            // (#696: emit a typed error instead of hanging indefinitely)
            // Skip this guard in test builds (parse_args tests run in non-TTY context).
            #[cfg(not(test))]
            // #746: newline before remediation so split_error_hint populates hint field
            return Err("interactive_only: claw requires an interactive terminal.\nStdin is not a TTY and no prompt was provided — pipe a prompt with `echo 'task' | claw` or run `claw` in an interactive terminal.".into());
        }
        return Ok(CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            base_commit,
            reasoning_effort: reasoning_effort.clone(),
            allow_broad_cwd,
            resume: None,
        });
    }
    if let Some(action) = parse_local_help_action(&rest, output_format) {
        return action;
    }
    if matches!(
        rest.first().map(String::as_str),
        Some("--resume" | "resume")
    ) {
        let action = parse_resume_args(&rest[1..], output_format, allow_broad_cwd)?;
        // `claw --resume [session]` with nothing to run on it continues the
        // session in the REPL, like `claude --resume`.
        if let CliAction::ResumeSession {
            session_path,
            commands,
            output_format: CliOutputFormat::Text,
            allow_broad_cwd,
        } = &action
        {
            if commands.is_empty() && resume_opens_repl() {
                return Ok(CliAction::Repl {
                    model,
                    allowed_tools,
                    permission_mode: permission_mode_override
                        .unwrap_or_else(default_permission_mode),
                    base_commit,
                    reasoning_effort,
                    allow_broad_cwd: *allow_broad_cwd,
                    resume: Some(session_path.display().to_string()),
                });
            }
        }
        return Ok(action);
    }
    // #696: `claw compact` is the bare name of the interactive `/compact`
    // slash command, not a prompt. When extra args such as `--help` appear
    // after the word `compact`, the generic prompt fallback used to send
    // `compact --help` to provider startup and could hang under closed stdin /
    // JSON output. Fail closed before any provider, prompt, TUI, or spinner
    // startup. `claw --resume SESSION.jsonl /compact` remains the supported
    // non-interactive session compaction path.
    if rest.first().map(String::as_str) == Some("compact") {
        return Err(compact_interactive_only_error());
    }
    if let Some(action) = parse_single_word_command_alias(
        &rest,
        &model,
        model_flag_raw.as_deref(),
        permission_mode_override,
        output_format,
        allowed_tools.clone(),
    ) {
        return action;
    }

    // Keep config-backed defaults lazy so pure-local JSON surfaces (notably
    // `claw --output-format json config`) can report config warnings
    // structurally without an earlier default-resolution load writing prose
    // warnings to stderr.
    let permission_mode = || permission_mode_override.unwrap_or_else(default_permission_mode);
    let permission_mode_provenance = || {
        permission_mode_override
            .map(PermissionModeProvenance::from_flag)
            .unwrap_or_else(permission_mode_provenance_for_current_dir)
    };

    // #98: --compact is only meaningful for prompt mode. When a known non-prompt
    // subcommand is being dispatched, reject --compact so callers don't silently
    // lose the flag.
    if compact
        && rest
            .first()
            .map(|s| s.as_str())
            .is_some_and(|s| s != "prompt")
    {
        // Allow compact for the default prompt fallback (unknown tokens).
        // Only reject for known top-level subcommands that don't use compact.
        let first = rest[0].as_str();
        if is_known_top_level_subcommand(first) && first != "prompt" {
            return Err("invalid_flag_value: --compact is only supported with prompt mode.\nUsage: claw --compact \"<prompt>\" or echo \"<prompt>\" | claw --compact".to_string());
        }
    }

    match rest[0].as_str() {
        "dump-manifests" => parse_dump_manifests_args(&rest[1..], output_format),
        "bootstrap-plan" => Ok(CliAction::BootstrapPlan { output_format }),
        "agents" => Ok(CliAction::Agents {
            args: join_optional_args(&rest[1..]),
            output_format,
        }),
        "mcp" => Ok(CliAction::Mcp {
            args: join_optional_args(&rest[1..]),
            output_format,
        }),
        // #145: `plugins` was routed through the prompt fallback because no
        // top-level parser arm produced CliAction::Plugins. That made `claw
        // plugins` (and `claw plugins --help`, `claw plugins list`, ...)
        // attempt an Anthropic network call, surfacing the misleading error
        // `missing Anthropic credentials` even though the command is purely
        // local introspection. Mirror `agents`/`mcp`/`skills`: action is the
        // first positional arg, target is the second.
        // `plugin` (singular) and `marketplace` are aliases for `plugins`.
        // All three must route to the same local handler so that no form
        // falls through to the LLM/prompt path.
        "plugins" | "plugin" | "marketplace" => {
            let tail = &rest[1..];
            let action = tail.first().cloned();
            let target = tail.get(1).cloned();
            if tail.len() > 2 {
                // #797: append \n usage hint so split_error_hint extracts it (parity with #791 config fix)
                return Err(format!(
                    "unexpected extra arguments after `claw {} {}`: {}\nUsage: claw plugins [list|show <id>|install <id>|enable <id>|disable <id>|uninstall <id>|update <id>|help]",
                    rest[0],
                    tail[..2].join(" "),
                    tail[2..].join(" ")
                ));
            }
            Ok(CliAction::Plugins {
                action,
                target,
                output_format,
            })
        }
        // #146: `config` is pure-local read-only introspection (merges
        // `.claw.json` + `.claw/settings.json` from disk, no network, no
        // state mutation). Previously callers had to spin up a session with
        // `claw --resume SESSION.jsonl /config` to see their own config,
        // which is synthetic friction. Accepts an optional section name
        // (env|hooks|model|plugins) matching the slash command shape.
        "config" => {
            let tail = &rest[1..];
            let section = tail.first().cloned();
            if tail.len() > 1 {
                // #791: append \n hint so split_error_hint extracts it and hint is non-null
                return Err(format!(
                    "unexpected extra arguments after `claw config {}`: {}\nUsage: claw config [env|hooks|model|plugins|mcp|settings]",
                    tail[0],
                    tail[1..].join(" ")
                ));
            }
            Ok(CliAction::Config {
                section,
                output_format,
            })
        }
        // #146: `diff` is pure-local (shells out to `git diff --cached` +
        // `git diff`). No session needed to inspect the working tree.
        "diff" => {
            if rest.len() > 1 {
                // #3129: keep malformed `diff ... --output-format json` on the
                // parser/error path, not the prompt/TUI fallback. The newline
                // before Usage is part of the JSON hint contract.
                return Err(unexpected_diff_args_error(&rest[1..]));
            }
            Ok(CliAction::Diff { output_format })
        }
        // `claw permissions <mode>` falls through to the LLM when called
        // with a subcommand argument because parse_single_word_command_alias
        // only intercepts the bare single-word form. Catch all multi-word
        // forms here and return a structured guidance error so no network
        // call or session is created.
        "permissions" => Err(
            "`claw permissions` is a slash command. Start `claw` and run `/permissions` inside the REPL.\n  Usage  /permissions [read-only|workspace-write|danger-full-access]"
                .to_string(),
        ),
        // #767: `claw session bogus` bypassed parse_single_word_command_alias (rest.len()>1),
        // had no match arm, and fell to CliAction::Prompt — reaching the credential gate
        // instead of a structured error. Mirror the guard on `permissions`.
        "session" => {
            // #449: `claw session list` is a pure local filesystem read that
            // requires no API credentials. Route directly to SessionList instead
            // of falling through to the resume/auth path.
            if rest.get(1).map(|s| s.as_str()) == Some("list") {
                Ok(CliAction::SessionList { output_format })
            } else {
                let action_hint = rest.get(1).map_or(String::new(), |a| format!(" (got: `{a}`)" ));
                Err(format!(
                    "interactive_only: `claw session` is a slash command{action_hint}.\nUse `claw --resume SESSION.jsonl /session <action>` or start `claw` and run `/session [list|exists|switch|fork|delete]`."
                ))
            }
        }
        // #770: same fallthrough gap as #767 — these slash commands had no multi-arg match arm
        // and fell to CliAction::Prompt reaching the credential gate when called with args.
        "cost" => Err(
            "interactive_only: `claw cost` is a slash command.\nUse `claw --resume SESSION.jsonl /cost` or start `claw` and run `/cost`."
                .to_string(),
        ),
        "clear" => Err(
            "interactive_only: `claw clear` is a slash command.\nUse `claw --resume SESSION.jsonl /clear [--confirm]` or start `claw` and run `/clear`."
                .to_string(),
        ),
        "memory" => Err(
            "interactive_only: `claw memory` is a slash command.\nStart `claw` and run `/memory` inside the REPL."
                .to_string(),
        ),
        "ultraplan" => Err(
            "interactive_only: `claw ultraplan` is a slash command.\nStart `claw` and run `/ultraplan` inside the REPL."
                .to_string(),
        ),
        "model" | "models" => {
            let tail = &rest[1..];
            let action = tail.first().cloned();
            if tail.len() > 1 {
                return Err(format!(
                    "unexpected extra arguments after `claw {} {}`: {}\nUsage: claw {} [help] [--output-format json]",
                    rest[0],
                    tail[0],
                    tail[1..].join(" "),
                    rest[0]
                ));
            }
            Ok(CliAction::Models {
                action,
                output_format,
            })
        }
        // #771: usage/stats/fork are slash-only verbs with no multi-arg match arms
        "usage" => Err(
            "interactive_only: `claw usage` is a slash command.\nUse `claw --resume SESSION.jsonl /usage` or start `claw` and run `/usage`."
                .to_string(),
        ),
        "stats" => Err(
            "interactive_only: `claw stats` is a slash command.\nUse `claw --resume SESSION.jsonl /stats` or start `claw` and run `/stats`."
                .to_string(),
        ),
        "fork" => Err(
            "interactive_only: `claw fork` is a slash command.\nStart `claw` and run `/session fork [branch-name]` inside the REPL."
                .to_string(),
        ),
        "skills" => {
            let args = join_optional_args(&rest[1..]);
            if let Some(action) = args.as_deref() {
                let first_word = action.split_whitespace().next().unwrap_or(action);
                if matches!(first_word, "add") {
                    return Err(format!(
                        "unsupported skills action: {first_word}. Supported actions: list, show <name>, install <path>, uninstall <name>, help, or <skill> [args]"
                    ));
                }
            }
            match classify_skills_slash_command(args.as_deref()) {
                SkillSlashDispatch::Invoke(prompt) => Ok(CliAction::Prompt {
                    prompt,
                    model,
                    output_format,
                    allowed_tools,
                    permission_mode: permission_mode(),
                    compact,
                    base_commit,
                    reasoning_effort: reasoning_effort.clone(),
                    allow_broad_cwd,
                }),
                SkillSlashDispatch::Local => Ok(CliAction::Skills {
                    args,
                    output_format,
                }),
            }
        }
        "settings" => {
            let tail = &rest[1..];
            if tail.is_empty() {
                Ok(CliAction::Config {
                    section: Some("settings".to_string()),
                    output_format,
                })
            } else if tail.len() == 1 && matches!(tail[0].as_str(), "help" | "--help" | "-h") {
                Ok(CliAction::HelpTopic {
                    topic: LocalHelpTopic::Settings,
                    output_format,
                })
            } else {
                Err(format!(
                    "unexpected extra arguments after `claw settings`: {}\nUsage: claw settings [help] [--output-format json]",
                    tail.join(" ")
                ))
            }
        }
        "system-prompt" => parse_system_prompt_args(&rest[1..], model, output_format),
        "acp" => parse_acp_args(&rest[1..], output_format),
        "desktop" => Ok(CliAction::Desktop),
        "desktop-agent" => {
            let mut resume = None;
            let mut incognito = false;
            let mut args = rest[1..].iter();
            while let Some(arg) = args.next() {
                match (arg.as_str(), args.as_slice().first()) {
                    ("--resume", Some(path)) => {
                        resume = Some(path.clone());
                        args.next();
                    }
                    ("--incognito", _) => incognito = true,
                    _ => {
                        return Err(format!(
                            "unexpected arguments after `claw desktop-agent`: {}\nUsage: claw desktop-agent [--resume <session.jsonl>] [--incognito]",
                            rest[1..].join(" ")
                        ))
                    }
                }
            }
            Ok(CliAction::DesktopAgent {
                model,
                allowed_tools,
                permission_mode: permission_mode(),
                resume,
                incognito,
                allow_broad_cwd,
            })
        }
        "login" | "logout" => Err(removed_auth_surface_error(rest[0].as_str())),
        "trust" => {
            if rest.len() > 1 {
                return Err(format!(
                    "unexpected extra arguments after `claw trust`: {}\nUsage: claw trust (reviews the project config files in the current directory)",
                    rest[1..].join(" ")
                ));
            }
            Ok(CliAction::Trust)
        }
        "init" => {
            // #771: extra positional args to `init` were silently ignored — now rejected
            if rest.len() > 1 {
                let extra = rest[1..].join(" ");
                return Err(format!(
                    "unexpected extra arguments after `claw init`: {extra}\nUsage: claw init [--cwd <dir>] [--date <date>] [--session <session-id>]"
                ));
            }
            Ok(CliAction::Init { output_format })
        }
        "setup" => {
            if rest.len() > 1 {
                let extra = rest[1..].join(" ");
                return Err(format!(
                    "unexpected extra arguments after `claw setup`: {extra}\nUsage: claw setup"
                ));
            }
            Ok(CliAction::Setup { output_format })
        }
        "export" => parse_export_args(&rest[1..], output_format),
        "prompt" => {
            let mut read_stdin = false;
            let prompt_parts = rest[1..]
                .iter()
                .filter_map(|arg| {
                    if matches!(arg.as_str(), "--stdin" | "--prompt-stdin") {
                        read_stdin = true;
                        None
                    } else {
                        Some(arg.as_str())
                    }
                })
                .collect::<Vec<_>>();
            let positional_prompt = prompt_parts.join(" ");
            let stdin_prompt = if read_stdin || positional_prompt.trim().is_empty() {
                read_piped_stdin()
            } else {
                None
            };
            let prompt = if read_stdin {
                merge_prompt_with_stdin(&positional_prompt, stdin_prompt.as_deref())
            } else {
                stdin_prompt
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or(&positional_prompt)
                    .to_string()
            };
            if prompt.trim().is_empty() {
                // #750/#823/#423: provide error_kind-compatible prefix + \n for hint extraction.
                return Err("missing_prompt: prompt subcommand requires a prompt string.
Usage: claw prompt <text>  or  echo '<text>' | claw prompt".to_string());
            }
            Ok(CliAction::Prompt {
                prompt,
                model,
                output_format,
                allowed_tools,
                permission_mode: permission_mode(),
                compact,
                base_commit: base_commit.clone(),
                reasoning_effort: reasoning_effort.clone(),
                allow_broad_cwd,
            })
        }
        other if other.starts_with('/') => parse_direct_slash_cli_action(
            &rest,
            model,
            output_format,
            allowed_tools,
            permission_mode_provenance(),
            compact,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
        ),
        other => {
            if !compact
                && !other.starts_with('-')
                && looks_like_subcommand_typo(other)
                && (rest.len() == 1
                    || (output_format == CliOutputFormat::Json && model_flag_raw.is_none()))
            {
                // #825/#826: emit command_not_found before provider startup for
                // command-shaped tokens that do not match known subcommands.
                // Text-mode multi-word prompt shorthand remains available, but
                // JSON-mode automation must not turn an unknown command into a
                // credential-gated prompt request.
                let mut message = format!("command_not_found: unknown subcommand: {other}.");
                if let Some(suggestions) = suggest_similar_subcommand(other) {
                    if let Some(line) = render_suggestion_line("Did you mean", &suggestions) {
                        message.push('\n');
                        message.push_str(&line);
                    }
                }
                message.push_str(
                    "\nRun `claw --help` for the full list. If you meant to send a prompt literally, use `claw prompt <text>`.",
                );
                return Err(message);
            }
            // #147: guard empty/whitespace-only prompts at the fallthrough
            // path the same way `"prompt"` arm above does. Without this,
            // `claw ""`, `claw "   "`, and `claw "" ""` silently route to
            // the Anthropic call and surface a misleading
            // `missing Anthropic credentials` error (or burn API tokens on
            // an empty prompt when credentials are present).
            let joined = rest.join(" ");
            if joined.trim().is_empty() {
                // #798: add \n hint so split_error_hint extracts it (was empty_prompt + null)
                return Err(
                    "empty prompt: provide a subcommand or a non-empty prompt string.\nUsage: claw <subcommand> or claw -p <prompt>. Run `claw --help` for the full list."
                        .to_string(),
                );
            }
            Ok(CliAction::Prompt {
                prompt: joined,
                model,
                output_format,
                allowed_tools,
                permission_mode: permission_mode(),
                compact,
                base_commit,
                reasoning_effort: reasoning_effort.clone(),
                allow_broad_cwd,
            })
        }
    }
}
