//! Session state of the REPL: persisting and resuming sessions, switching
//! model, fallback and permission mode (rebuilding the runtime), the status
//! line, memory notes, `/session` and `/plugins` actions.

#[allow(clippy::wildcard_imports)]
use crate::*;

impl LiveCli {
    pub(crate) fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    pub(crate) fn record_prompt_history(&mut self, prompt: &str) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map_or(self.runtime.session().updated_at_ms, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        let entry = PromptHistoryEntry {
            timestamp_ms,
            text: prompt.to_string(),
        };
        self.prompt_history.push(entry);
        if let Err(error) = self.runtime.session_mut().push_prompt_entry(prompt) {
            eprintln!("warning: failed to persist prompt history: {error}");
        }
    }

    pub(crate) fn set_model(
        &mut self,
        model: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let model = match model {
            None if io::stdin().is_terminal() && runtime::turn_keys::stdout_is_terminal() => {
                pick_model(&self.model)?
            }
            model => model,
        };
        let Some(model) = model else {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        };

        let model = resolve_model_alias_with_config(&model);
        runtime::model_server::ensure_for_model(&model);

        if model == self.model {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        }

        let previous = self.model.clone();
        let session = self.runtime.session().clone();
        let message_count = session.messages.len();
        let runtime = build_runtime(
            session,
            &self.session.id,
            model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.model.clone_from(&model);
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        // The model picked last is the one claw starts with next time.
        match setup_wizard::save_settings_field("model", &model) {
            Ok(()) => println!("  Default          saved to ~/.claw/settings.json"),
            Err(error) => eprintln!("warning: could not save the default model: {error}"),
        }
        Ok(true)
    }

    /// `/fallback [model|off]`: the model the session switches to when the
    /// current one fails (`providerFallbacks.fallbacks` in settings). Without
    /// an argument, a list to pick from (like `/model`), or the current
    /// setting when there is no terminal.
    pub(crate) fn set_fallback(
        &mut self,
        model: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let current = fallback_models_for("").into_iter().next();
        let choice = match model {
            None if io::stdin().is_terminal() && runtime::turn_keys::stdout_is_terminal() => {
                pick_fallback_model(current.as_deref())?
            }
            None => {
                println!(
                    "Fallback\n  Current          {}\n\nUsage\n  /fallback <model>   use it when the current model fails\n  /fallback off       no fallback",
                    current.as_deref().unwrap_or("off")
                );
                return Ok(false);
            }
            Some(model) => Some(model.trim().to_string()),
        };
        let Some(choice) = choice else {
            return Ok(false);
        };
        println!("{}", apply_fallback_setting(&choice)?);
        Ok(false)
    }

    pub(crate) fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            println!(
                "{}",
                format_permissions_report(self.permission_mode.as_str())
            );
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "invalid_flag_value: unsupported permission mode '{mode}'.\nUsage: --permission-mode read-only|workspace-write|danger-full-access"
            )
        })?;

        if normalized == self.permission_mode.as_str() {
            println!("{}", format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = self.permission_mode.as_str().to_string();
        self.apply_permission_mode(permission_mode_from_label(normalized))?;
        println!(
            "{}",
            format_permissions_switch_report(&previous, normalized)
        );
        Ok(true)
    }

    /// Rebuilds the runtime so `mode` applies to the next tool calls.
    pub(crate) fn apply_permission_mode(
        &mut self,
        mode: PermissionMode,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.permission_mode = mode;
        self.rebuild_runtime()
    }

    /// Rebuilds the runtime from the current session, system prompt and
    /// permission mode.
    pub(crate) fn rebuild_runtime(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)
    }

    /// `# note`: appends the note to the user's AGENTS.md (like Claude
    /// Code's `#` memory shortcut) and reloads the system prompt so it
    /// applies from the next message on.
    pub(crate) fn add_memory_note(&mut self, note: &str) -> Result<(), Box<dyn std::error::Error>> {
        if note.is_empty() {
            println!("  usage: # <note>   (saved to the AGENTS.md loaded in every session)");
            return Ok(());
        }
        let home = runtime::default_config_home();
        fs::create_dir_all(&home)?;
        let path = home.join("AGENTS.md");
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let mut addition = String::new();
        if !existing.is_empty() && !existing.ends_with('\n') {
            addition.push('\n');
        }
        if !existing.contains(MEMORY_NOTES_HEADING) {
            addition.push_str(&format!("\n{MEMORY_NOTES_HEADING}\n"));
        }
        addition.push_str(&format!("- {note}\n"));
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?
            .write_all(addition.as_bytes())?;
        self.system_prompt = build_system_prompt(&self.model)?;
        self.rebuild_runtime()?;
        println!(
            "  {TOOL_DIM_STYLE}saved to{STYLE_RESET} {} {TOOL_DIM_STYLE}(applies from the next message){STYLE_RESET}",
            path.display()
        );
        Ok(())
    }

    /// Fills the status line under the input box: the permission mode, the
    /// model and how much of the context window the conversation uses.
    pub(crate) fn refresh_footer_status(&self) {
        let (mode, mode_sgr) = permission_mode_indicator(self.permission_mode.as_str());
        let usage = self.runtime.usage().current_turn_usage();
        let last_request = usage.input_tokens
            + usage.cache_creation_input_tokens
            + usage.cache_read_input_tokens
            + usage.output_tokens;
        let used = if last_request > 0 {
            last_request as usize
        } else {
            self.runtime.estimated_tokens()
        };
        // Unknown hosted models show only the tokens used.
        let window = known_context_window(&self.model).map(|window| window as usize);
        let short = |tokens: usize| {
            if tokens >= 1000 {
                #[allow(clippy::cast_precision_loss)]
                let thousands = tokens as f64 / 1000.0;
                format!("{thousands:.1}k")
            } else {
                tokens.to_string()
            }
        };
        let context = match window {
            Some(window) if window > 0 => format!(
                "ctx {}/{} ({}%)",
                short(used),
                short(window),
                used * 100 / window
            ),
            _ => format!("ctx {}", short(used)),
        };
        // Sub-agents without an explicit model use the session's model.
        tools::set_default_agent_model(&self.model);
        runtime::turn_keys::set_footer_status(runtime::turn_keys::FooterStatus {
            mode: format!("{mode} (shift+tab to cycle)"),
            mode_sgr: if render::current_theme() == render::ThemeName::None {
                String::new()
            } else {
                mode_sgr.to_string()
            },
            // A `statusLine.command` from settings replaces model and context.
            details: claude_commands::custom_status_line(
                &self.model,
                self.permission_mode.as_str(),
                used,
                window,
            )
            .map_or_else(
                || {
                    vec![
                        self.model.clone(),
                        context,
                        footer_session_label(&self.session.id),
                    ]
                },
                |line| vec![line],
            ),
            // What Shift+Tab presses during a turn will switch to.
            cycle: std::iter::successors(
                Some(next_permission_mode(self.permission_mode.as_str())),
                |mode| Some(next_permission_mode(mode)),
            )
            .take(3)
            .map(|mode| {
                let (label, sgr) = permission_mode_indicator(mode);
                let sgr = if render::current_theme() == render::ThemeName::None {
                    ""
                } else {
                    sgr
                };
                (label.to_string(), sgr.to_string())
            })
            .collect(),
        });
    }

    /// Shift+Tab: read-only -> workspace-write -> danger-full-access -> ...
    /// Applies `presses` presses at once: those made at the prompt (when it
    /// returns) or during a turn (when the turn ends).
    pub(crate) fn cycle_permission_mode_by(
        &mut self,
        presses: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if presses == 0 {
            return Ok(());
        }
        let mut next = self.permission_mode.as_str();
        for _ in 0..presses {
            next = next_permission_mode(next);
        }
        // The status line under the input box shows the new mode. Only the
        // policy changes: no runtime rebuild (plugins, MCP servers) and no
        // session write, so the switch is instant.
        let mode = permission_mode_from_label(next);
        self.permission_mode = mode;
        self.runtime.set_permission_mode(mode);
        Ok(())
    }

    /// Switches to a new, empty session (the old one stays resumable).
    pub(crate) fn start_fresh_session(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let session_state = new_cli_session()?;
        self.session = create_managed_session_handle(&session_state.session_id)?;
        let runtime = build_runtime(
            session_state.with_persistence_path(self.session.path.clone()),
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
        runtime::turn_keys::screen::clear_conversation();
        Ok(())
    }

    /// Like `start_fresh_session`, for a fresh `/goal` round: the screen
    /// keeps the earlier rounds. Returns the previous session's id.
    pub(crate) fn start_goal_round_session(
        &mut self,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let previous = self.session.id.clone();
        self.persist_session()?;
        let session_state = new_cli_session()?;
        self.session = create_managed_session_handle(&session_state.session_id)?;
        let runtime = build_runtime(
            session_state.with_persistence_path(self.session.path.clone()),
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
        Ok(previous)
    }

    pub(crate) fn clear_session(
        &mut self,
        confirm: bool,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            println!(
                "clear: confirmation required; run /clear --confirm to start a fresh session."
            );
            return Ok(false);
        }

        let previous_session = self.session.clone();
        let session_state = new_cli_session()?;
        self.session = create_managed_session_handle(&session_state.session_id)?;
        let runtime = build_runtime(
            session_state.with_persistence_path(self.session.path.clone()),
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
        println!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            self.model,
            self.permission_mode.as_str(),
            self.session.id,
            self.session.path.display(),
        );
        Ok(true)
    }

    pub(crate) fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let session_path = match session_path {
            None if io::stdin().is_terminal() && runtime::turn_keys::stdout_is_terminal() => {
                pick_session_to_resume(&self.session.id)?
            }
            path => path,
        };
        let Some(session_ref) = session_path else {
            println!("{}", render_resume_usage());
            return Ok(false);
        };

        let (handle, session) =
            load_session_reference_excluding(&session_ref, Some(&self.session.id))?;
        let message_count = session.messages.len();
        let session_id = session.session_id.clone();
        let runtime = build_runtime(
            session,
            &handle.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.session = SessionHandle {
            id: session_id,
            path: handle.path,
        };
        println!(
            "{}",
            format_resume_report(
                &self.session.path.display().to_string(),
                message_count,
                self.runtime.usage().turns(),
            )
        );
        Ok(true)
    }

    pub(crate) fn export_session(
        &self,
        requested_path: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let export_path = resolve_export_path(requested_path, self.runtime.session())?;
        fs::write(&export_path, render_export_text(self.runtime.session()))?;
        println!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn handle_session_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None | Some("list") => {
                println!("{}", render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("exists") => {
                let Some(target) = target else {
                    println!("Usage: /session exists <session-id>");
                    return Ok(false);
                };
                let exists = session_reference_exists(target)?;
                let handle = resolve_session_reference(target).ok();
                println!(
                    "Session exists\n  Session          {target}\n  Exists           {exists}{}",
                    handle
                        .as_ref()
                        .map(|handle| format!("\n  File             {}", handle.path.display()))
                        .unwrap_or_default()
                );
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    println!("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let (handle, session) = load_session_reference(target)?;
                let message_count = session.messages.len();
                let session_id = session.session_id.clone();
                let runtime = build_runtime(
                    session,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = SessionHandle {
                    id: session_id,
                    path: handle.path,
                };
                println!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("fork") => {
                let forked = self.runtime.fork_session(target.map(ToOwned::to_owned));
                let parent_session_id = self.session.id.clone();
                let handle = create_managed_session_handle(&forked.session_id)?;
                let branch_name = forked
                    .fork
                    .as_ref()
                    .and_then(|fork| fork.branch_name.clone());
                let forked = forked.with_persistence_path(handle.path.clone());
                let message_count = forked.messages.len();
                forked.save_to_path(&handle.path)?;
                let runtime = build_runtime(
                    forked,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = handle;
                println!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    self.session.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("delete") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                if !confirm_session_deletion(&handle.id) {
                    println!("delete: cancelled.");
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some("delete-force") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some(other) => {
                println!(
                    "Unknown /session action '{other}'. Use /session list, /session exists <session-id>, /session switch <session-id>, /session fork [branch-name], or /session delete <session-id> [--force]."
                );
                Ok(false)
            }
        }
    }

    pub(crate) fn handle_plugins_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let payload =
            plugins_command_payload_for(&cwd, action, target, ConfigWarningMode::EmitStderr)?;
        println!("{}", payload.message);
        if payload.reload_runtime {
            self.reload_runtime_features()?;
        }
        Ok(false)
    }

    pub(crate) fn reload_runtime_features(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = build_runtime(
            self.runtime.session().clone(),
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
        self.persist_session()
    }
}
