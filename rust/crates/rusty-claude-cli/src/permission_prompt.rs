//! The terminal's approval prompt for tool calls that need more than the
//! current permission mode: allow once, always (for this session), or deny.

#[allow(clippy::wildcard_imports)]
use crate::*;

pub(crate) struct CliPermissionPrompter {
    pub(crate) current_mode: PermissionMode,
}

impl CliPermissionPrompter {
    pub(crate) fn new(current_mode: PermissionMode) -> Self {
        Self { current_mode }
    }
}

impl runtime::PermissionPrompter for CliPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        let command = serde_json::from_str::<serde_json::Value>(&request.input)
            .ok()
            .and_then(|input| {
                input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        // Host grants cover the complete input and cwd, never a command prefix.
        // Ordinary `sudo emerge` session approvals cannot authorize host access.
        let approval_key = permission_request_approval_key(request, command.as_deref());
        if SESSION_APPROVALS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&approval_key)
        {
            return runtime::PermissionPromptDecision::Allow;
        }

        // Pause the keyboard watcher before showing the question, so an
        // answer typed right away is read here instead of being queued.
        let _keys_paused = runtime::turn_keys::pause();
        let always_label = if runtime::is_host_execution_request(&request.tool_name, &request.input)
        {
            "this exact host command and working directory"
        } else {
            approval_key.split_once(':').map_or(
                "",
                |(tool, key)| {
                    if key.is_empty() {
                        tool
                    } else {
                        key
                    }
                },
            )
        };
        if io::stdin().is_terminal() && runtime::turn_keys::stdout_is_terminal() {
            render::stop_thinking();
            let prompt = approval_request(request, command.as_deref(), always_label);
            return match approval::ask(prompt) {
                Ok(approval::Decision::Allow) => runtime::PermissionPromptDecision::Allow,
                Ok(approval::Decision::AllowForSession) => {
                    SESSION_APPROVALS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(approval_key);
                    runtime::PermissionPromptDecision::Allow
                }
                Ok(approval::Decision::Deny(None)) => runtime::PermissionPromptDecision::Deny {
                    reason: format!(
                        "tool '{}' denied by user approval prompt",
                        request.tool_name
                    ),
                },
                // The user's words go to the model, so it can change course
                // instead of retrying the same call.
                Ok(approval::Decision::Deny(Some(feedback))) => {
                    runtime::PermissionPromptDecision::Deny {
                        reason: format!(
                            "tool '{}' denied by the user, who said: {feedback}",
                            request.tool_name
                        ),
                    }
                }
                Err(error) => runtime::PermissionPromptDecision::Deny {
                    reason: format!("permission approval failed: {error}"),
                },
            };
        }

        // Not a terminal (piped stdin, tests): the plain typed prompt.
        let subject = command
            .clone()
            .unwrap_or_else(|| truncate_for_summary(&request.input, 300));
        let yellow = "\x1b[33m";
        // The box has its own indent; wrap long lines inside the right margin
        // and keep the `|` gutter on wrapped lines.
        let cols = runtime::turn_keys::stdout_is_terminal()
            .then(|| crossterm::terminal::size().ok())
            .flatten()
            .map(|(cols, _)| usize::from(cols));
        let mut boxed = render::MarginWriter::with_width(io::stdout(), cols, 0);
        let _ = writeln!(boxed);
        let _ = writeln!(
            boxed,
            "{yellow}  +-- Permission required: {}{STYLE_RESET}",
            request.tool_name
        );
        for line in subject.lines().take(20) {
            let _ = writeln!(boxed, "{yellow}  |{STYLE_RESET} {line}");
        }
        if let Some(reason) = &request.reason {
            let _ = writeln!(
                boxed,
                "{yellow}  |{STYLE_RESET} {TOOL_DIM_STYLE}{reason}{STYLE_RESET}"
            );
        }
        let _ = writeln!(boxed, "{yellow}  +--{STYLE_RESET}");
        let _ = boxed.flush();
        print!(
            "{yellow}  Allow? [y]es / [a]lways for `{always_label}` this session / [N]o: {STYLE_RESET}"
        );
        let _ = io::stdout().flush();

        let mut response = String::new();
        match io::stdin().read_line(&mut response) {
            Ok(_) => {
                let normalized = response.trim().to_ascii_lowercase();
                if matches!(normalized.as_str(), "a" | "always") {
                    SESSION_APPROVALS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(approval_key);
                    runtime::PermissionPromptDecision::Allow
                } else if matches!(normalized.as_str(), "y" | "yes") {
                    runtime::PermissionPromptDecision::Allow
                } else {
                    runtime::PermissionPromptDecision::Deny {
                        reason: format!(
                            "tool '{}' denied by user approval prompt",
                            request.tool_name
                        ),
                    }
                }
            }
            Err(error) => runtime::PermissionPromptDecision::Deny {
                reason: format!("permission approval failed: {error}"),
            },
        }
    }
}
