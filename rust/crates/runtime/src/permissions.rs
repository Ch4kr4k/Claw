use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::RuntimePermissionRuleConfig;

/// Permission level assigned to a tool invocation or runtime session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    Prompt,
    Allow,
}

impl PermissionMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
            Self::Prompt => "prompt",
            Self::Allow => "allow",
        }
    }
}

/// Hook-provided override applied before standard permission evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOverride {
    Allow,
    Deny,
    Ask,
}

/// Additional permission context supplied by hooks or higher-level orchestration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PermissionContext {
    override_decision: Option<PermissionOverride>,
    override_reason: Option<String>,
}

impl PermissionContext {
    #[must_use]
    pub fn new(
        override_decision: Option<PermissionOverride>,
        override_reason: Option<String>,
    ) -> Self {
        Self {
            override_decision,
            override_reason,
        }
    }

    #[must_use]
    pub fn override_decision(&self) -> Option<PermissionOverride> {
        self.override_decision
    }

    #[must_use]
    pub fn override_reason(&self) -> Option<&str> {
        self.override_reason.as_deref()
    }
}

/// Full authorization request presented to a permission prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub input: String,
    pub current_mode: PermissionMode,
    pub required_mode: PermissionMode,
    pub reason: Option<String>,
}

/// User-facing decision returned by a [`PermissionPrompter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionPromptDecision {
    Allow,
    Deny { reason: String },
}

/// Prompting interface used when policy requires interactive approval.
pub trait PermissionPrompter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision;
}

/// Final authorization result after evaluating static rules and prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allow,
    Deny { reason: String },
}

/// Evaluates permission mode requirements plus allow/deny/ask rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPolicy {
    active_mode: PermissionMode,
    tool_requirements: BTreeMap<String, PermissionMode>,
    allow_rules: Vec<PermissionRule>,
    deny_rules: Vec<PermissionRule>,
    ask_rules: Vec<PermissionRule>,
    /// Judge bash commands by content: approval only for privileged or
    /// destructive ones (`bashApproval: "risky"`).
    risky_bash_only: bool,
    /// Risky bash commands ask in every mode, full access included
    /// (`bashApproval: "sudo-and-destructive"` / `"destructive"`).
    risky_bash_asks_in_every_mode: bool,
    /// Running as root counts as risky; false for `bashApproval: "destructive"`.
    root_bash_is_risky: bool,
    /// The shell sandbox is enabled in settings. When it is off, a request to
    /// run outside it is an ordinary command and needs no extra approval.
    sandbox_enabled: bool,
    /// #159: simple tool-name denials. Tools in this list are unconditionally
    /// denied regardless of permission mode, checked before the rule-based
    /// deny/allow/ask evaluation.
    denied_tools: Vec<String>,
}

impl PermissionPolicy {
    #[must_use]
    pub fn new(active_mode: PermissionMode) -> Self {
        Self {
            active_mode,
            tool_requirements: BTreeMap::new(),
            allow_rules: Vec::new(),
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            risky_bash_only: false,
            risky_bash_asks_in_every_mode: false,
            root_bash_is_risky: true,
            sandbox_enabled: true,
            denied_tools: Vec::new(),
        }
    }

    /// Whether the shell sandbox is enabled in settings (the default).
    #[must_use]
    pub fn with_sandbox_enabled(mut self, enabled: bool) -> Self {
        self.sandbox_enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_tool_requirement(
        mut self,
        tool_name: impl Into<String>,
        required_mode: PermissionMode,
    ) -> Self {
        self.tool_requirements
            .insert(tool_name.into(), required_mode);
        self
    }

    #[must_use]
    pub fn with_permission_rules(mut self, config: &RuntimePermissionRuleConfig) -> Self {
        self.risky_bash_only = config.risky_bash_only();
        self.risky_bash_asks_in_every_mode = config.risky_bash_asks_in_every_mode();
        self.root_bash_is_risky = config.root_bash_is_risky();
        self.allow_rules = config
            .allow()
            .iter()
            .map(|rule| PermissionRule::parse(rule))
            .collect();
        self.deny_rules = config
            .deny()
            .iter()
            .map(|rule| PermissionRule::parse(rule))
            .collect();
        self.ask_rules = config
            .ask()
            .iter()
            .map(|rule| PermissionRule::parse(rule))
            .collect();
        // #94: normalize denied tool names to lowercase to match runtime convention
        self.denied_tools = config
            .denied_tools()
            .iter()
            .map(|t| t.to_lowercase())
            .collect();
        self
    }

    #[must_use]
    pub fn active_mode(&self) -> PermissionMode {
        self.active_mode
    }

    #[must_use]
    pub fn required_mode_for(&self, tool_name: &str) -> PermissionMode {
        self.tool_requirements
            .get(tool_name)
            .copied()
            .unwrap_or(PermissionMode::DangerFullAccess)
    }

    /// The mode a call needs: the tool's own requirement, raised to full
    /// access for file-writing tools aimed outside the current directory.
    #[must_use]
    pub fn required_mode_for_input(&self, tool_name: &str, input: &str) -> PermissionMode {
        let base = self.required_mode_for(tool_name);
        if tool_name == "bash" && self.risky_bash_only {
            if let Some(command) = bash_command_of(input) {
                return bash_command_requirement(&command, self.root_bash_is_risky);
            }
        }
        // Outside the current directory, only changing a file that already
        // exists asks: creating a new file destroys nothing. The project's
        // memory folder counts as inside, so keeping memories never asks.
        if FILE_WRITING_TOOLS.contains(&tool_name)
            && extract_permission_subject(input).is_some_and(|path| {
                !path_inside_cwd(&path)
                    && !crate::memory::is_memory_path(&path)
                    && path_exists_from_cwd(&path)
            })
        {
            return base.max(PermissionMode::DangerFullAccess);
        }
        base
    }

    #[must_use]
    pub fn authorize(
        &self,
        tool_name: &str,
        input: &str,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        self.authorize_with_context(tool_name, input, &PermissionContext::default(), prompter)
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn authorize_with_context(
        &self,
        tool_name: &str,
        input: &str,
        context: &PermissionContext,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        // #159: check denied_tools before rule-based evaluation. Tools listed
        // in the denied_tools config are unconditionally denied regardless of
        // permission mode.
        if self.denied_tools.iter().any(|t| t == tool_name) {
            return PermissionOutcome::Deny {
                reason: format!("tool '{tool_name}' has been denied by denied_tools configuration"),
            };
        }

        if let Some(rule) = Self::find_matching_restriction(&self.deny_rules, tool_name, input) {
            return PermissionOutcome::Deny {
                reason: format!(
                    "Permission to use {tool_name} has been denied by rule '{}'",
                    rule.raw
                ),
            };
        }

        if self.sandbox_enabled && is_host_execution_request(tool_name, input) {
            if context.override_decision() == Some(PermissionOverride::Deny) {
                return PermissionOutcome::Deny {
                    reason: "host execution denied by hook".to_string(),
                };
            }
            if self.active_mode() == PermissionMode::ReadOnly {
                return PermissionOutcome::Deny {
                    reason: "host execution is unavailable in read-only mode".to_string(),
                };
            }
            return Self::prompt_or_deny(tool_name, input, self.active_mode(),
                PermissionMode::DangerFullAccess,
                Some("Run this exact command on the host, outside Claw's sandbox. This approval is separate from ordinary Bash permissions.".to_string()), prompter);
        }

        let current_mode = self.active_mode();
        let required_mode = self.required_mode_for_input(tool_name, input);
        let ask_rule = Self::find_matching_restriction(&self.ask_rules, tool_name, input);
        // A bash allow rule only counts when it also covers every risky
        // command in a chain: `bash(git:*)` must not approve `git status; rm -rf ~`.
        let allow_rule =
            Self::find_matching_rule(&self.allow_rules, tool_name, input).filter(|_| {
                tool_name != "bash"
                    || bash_command_of(input)
                        .is_none_or(|command| self.uncovered_bash_risk(&command).is_none())
            });

        match context.override_decision() {
            Some(PermissionOverride::Deny) => {
                return PermissionOutcome::Deny {
                    reason: context.override_reason().map_or_else(
                        || format!("tool '{tool_name}' denied by hook"),
                        ToOwned::to_owned,
                    ),
                };
            }
            Some(PermissionOverride::Ask) => {
                let reason = context.override_reason().map_or_else(
                    || format!("tool '{tool_name}' requires approval due to hook guidance"),
                    ToOwned::to_owned,
                );
                return Self::prompt_or_deny(
                    tool_name,
                    input,
                    current_mode,
                    required_mode,
                    Some(reason),
                    prompter,
                );
            }
            Some(PermissionOverride::Allow) => {
                if let Some(rule) = ask_rule {
                    let reason = format!(
                        "tool '{tool_name}' requires approval due to ask rule '{}'",
                        rule.raw
                    );
                    return Self::prompt_or_deny(
                        tool_name,
                        input,
                        current_mode,
                        required_mode,
                        Some(reason),
                        prompter,
                    );
                }
                if let Some(reason) = self.risky_bash_reason(tool_name, input) {
                    return Self::prompt_or_deny(
                        tool_name,
                        input,
                        current_mode,
                        required_mode,
                        Some(reason),
                        prompter,
                    );
                }
                if allow_rule.is_some()
                    || current_mode == PermissionMode::Allow
                    || current_mode >= required_mode
                {
                    return PermissionOutcome::Allow;
                }
            }
            None => {}
        }

        if let Some(rule) = ask_rule {
            let reason = format!(
                "tool '{tool_name}' requires approval due to ask rule '{}'",
                rule.raw
            );
            return Self::prompt_or_deny(
                tool_name,
                input,
                current_mode,
                required_mode,
                Some(reason),
                prompter,
            );
        }

        if let Some(reason) = self.risky_bash_reason(tool_name, input) {
            return Self::prompt_or_deny(
                tool_name,
                input,
                current_mode,
                required_mode,
                Some(reason),
                prompter,
            );
        }

        if allow_rule.is_some()
            || current_mode == PermissionMode::Allow
            || current_mode >= required_mode
        {
            return PermissionOutcome::Allow;
        }

        if current_mode == PermissionMode::Prompt
            || (current_mode == PermissionMode::WorkspaceWrite
                && required_mode == PermissionMode::DangerFullAccess)
        {
            let risk = (tool_name == "bash")
                .then(|| bash_command_of(input))
                .flatten()
                .and_then(|command| bash_risk(&command, self.root_bash_is_risky));
            let reason = Some(risk.unwrap_or_else(|| {
                format!(
                    "tool '{tool_name}' requires approval to escalate from {} to {}",
                    current_mode.as_str(),
                    required_mode.as_str()
                )
            }));
            return Self::prompt_or_deny(
                tool_name,
                input,
                current_mode,
                required_mode,
                reason,
                prompter,
            );
        }

        PermissionOutcome::Deny {
            reason: format!(
                "tool '{tool_name}' requires {} permission; current mode is {}",
                required_mode.as_str(),
                current_mode.as_str()
            ),
        }
    }

    /// With `bashApproval: "sudo-and-destructive"` or `"destructive"`, why a
    /// bash call must be approved whatever the mode: it runs as root (unless
    /// `"destructive"`) or destroys data, disks, processes or git history.
    /// Allow rules covering each risky command still pre-approve it.
    fn risky_bash_reason(&self, tool_name: &str, input: &str) -> Option<String> {
        if !self.risky_bash_asks_in_every_mode || tool_name != "bash" {
            return None;
        }
        bash_command_of(input).and_then(|command| self.uncovered_bash_risk(&command))
    }

    /// Why `command` is risky despite the bash allow rules, or `None` when
    /// they cover it. Each risky command in a chain needs a rule matching
    /// that command (`bash(shutdown:*)` covers `shutdown -h +5`, not
    /// `shutdown now; rm -rf ~`); a pattern spanning the whole command
    /// (`rm -rf /`, a fork bomb) needs a rule matching the whole command.
    fn uncovered_bash_risk(&self, command: &str) -> Option<String> {
        let covered = |subject: &str| {
            self.allow_rules.iter().any(|rule| {
                rule.tool_name.eq_ignore_ascii_case("bash") && rule.matches_subject(subject)
            })
        };
        if let crate::bash_validation::ValidationResult::Warn { message }
        | crate::bash_validation::ValidationResult::Block { reason: message } =
            crate::bash_validation::check_destructive(command)
        {
            if !covered(command.trim()) {
                return Some(message);
            }
        }
        risk_segments(command).into_iter().find_map(|segment| {
            bash_risk(&segment, self.root_bash_is_risky).filter(|_| !covered(&segment))
        })
    }

    fn prompt_or_deny(
        tool_name: &str,
        input: &str,
        current_mode: PermissionMode,
        required_mode: PermissionMode,
        reason: Option<String>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> PermissionOutcome {
        let request = PermissionRequest {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            current_mode,
            required_mode,
            reason: reason.clone(),
        };

        match prompter.as_mut() {
            Some(prompter) => match prompter.decide(&request) {
                PermissionPromptDecision::Allow => PermissionOutcome::Allow,
                PermissionPromptDecision::Deny { reason } => PermissionOutcome::Deny { reason },
            },
            None => PermissionOutcome::Deny {
                reason: reason.unwrap_or_else(|| {
                    format!(
                        "tool '{tool_name}' requires approval to run while mode is {}",
                        current_mode.as_str()
                    )
                }),
            },
        }
    }

    fn find_matching_rule<'a>(
        rules: &'a [PermissionRule],
        tool_name: &str,
        input: &str,
    ) -> Option<&'a PermissionRule> {
        rules.iter().find(|rule| rule.matches(tool_name, input))
    }

    /// Like [`Self::find_matching_rule`], but a `bash` rule also matches any
    /// command inside a chain (`cd x && sudo y`, `echo | sudo tee f`,
    /// `$(sudo ...)`). Used for `deny` and `ask` rules only, where matching
    /// more broadly can only make the policy stricter.
    fn find_matching_restriction<'a>(
        rules: &'a [PermissionRule],
        tool_name: &str,
        input: &str,
    ) -> Option<&'a PermissionRule> {
        rules.iter().find(|rule| {
            rule.matches(tool_name, input)
                || (tool_name == "bash"
                    && rule.tool_name.eq_ignore_ascii_case("bash")
                    && extract_permission_subject(input).is_some_and(|command| {
                        shell_command_segments(&command)
                            .iter()
                            .any(|segment| rule.matches_subject(segment))
                    }))
        })
    }
}

/// An explicit request for the host executor, never inferred from `sudo`.
#[must_use]
pub fn is_host_execution_request(tool_name: &str, input: &str) -> bool {
    tool_name.eq_ignore_ascii_case("bash")
        && serde_json::from_str::<Value>(input)
            .ok()
            .is_some_and(|value| {
                value
                    .get("dangerouslyDisableSandbox")
                    .and_then(Value::as_bool)
                    == Some(true)
            })
}

/// The `command` field of a bash tool input.
fn bash_command_of(input: &str) -> Option<String> {
    serde_json::from_str::<Value>(input)
        .ok()?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

/// Commands that run something as root.
const PRIVILEGE_COMMANDS: &[&str] = &["sudo", "su", "doas", "pkexec", "run0"];

/// Commands that delete data, change disks or stop the system or processes.
const DESTRUCTIVE_COMMANDS: &[&str] = &[
    "rm", "rmdir", "unlink", "shred", "wipefs", "dd", "fdisk", "sfdisk", "cfdisk", "gdisk",
    "sgdisk", "parted", "mkswap", "truncate", "kill", "killall", "pkill", "reboot", "shutdown",
    "poweroff", "halt",
];

/// Why a bash command needs the user's approval, or `None` when it can run
/// without asking: it runs as root, or it destroys data, disks, processes or
/// git history. Every command in a chain or pipeline is checked.
#[must_use]
pub fn bash_command_risk(command: &str) -> Option<String> {
    bash_risk(command, true)
}

/// Like [`bash_command_risk`], but running as root alone is not a risk: only
/// destructive commands are, also behind `sudo`, `doas` or `pkexec`.
#[must_use]
pub fn bash_command_destructive_risk(command: &str) -> Option<String> {
    bash_risk(command, false)
}

/// Options of `sudo`/`doas` that take a value as the next word.
const PRIVILEGE_OPTIONS_WITH_VALUE: &[&str] =
    &["-u", "-g", "-C", "-D", "-p", "-r", "-t", "-U", "-h"];

fn bash_risk(command: &str, root_is_risky: bool) -> Option<String> {
    if let crate::bash_validation::ValidationResult::Warn { message }
    | crate::bash_validation::ValidationResult::Block { reason: message } =
        crate::bash_validation::check_destructive(command)
    {
        return Some(message);
    }
    for segment in risk_segments(command) {
        let mut words = segment.split_whitespace().collect::<Vec<_>>();
        let base_name = |word: &str| word.rsplit('/').next().unwrap_or(word).to_string();
        if !root_is_risky {
            // Judge what runs as root, not the act of running as root.
            while words
                .first()
                .is_some_and(|word| PRIVILEGE_COMMANDS.contains(&base_name(word).as_str()))
            {
                let mut index = 1;
                while index < words.len() && words[index].starts_with('-') {
                    if PRIVILEGE_OPTIONS_WITH_VALUE.contains(&words[index]) {
                        index += 1;
                    }
                    index += 1;
                }
                words.drain(..index.min(words.len()));
            }
        }
        let Some(first) = words
            .first()
            .map(|word| base_name(word.trim_start_matches(['\'', '"'])))
        else {
            continue;
        };
        let first = first.as_str();
        let has = |flag: &str| words.contains(&flag);
        let has_short = |letter: char| {
            words.iter().any(|word| {
                word.starts_with('-') && !word.starts_with("--") && word.contains(letter)
            })
        };
        if root_is_risky && PRIVILEGE_COMMANDS.contains(&first) {
            return Some(format!("runs as root ({first}): {segment}"));
        }
        if DESTRUCTIVE_COMMANDS.contains(&first) || first.starts_with("mkfs") {
            return Some(format!(
                "may destroy data or stop things ({first}): {segment}"
            ));
        }
        let risky = match first {
            "find" => has("-delete") || has("-exec") && words.contains(&"rm"),
            "chmod" | "chown" | "chgrp" => has_short('R') || has("--recursive"),
            "git" => {
                let sub = words.iter().skip(1).find(|word| !word.starts_with('-'));
                match sub.copied() {
                    Some("reset") => has("--hard"),
                    Some("clean") => has_short('f') || has("--force"),
                    Some("push") => has("--force") || has("-f") || has("--force-with-lease"),
                    Some("branch") => has("-D") || has("--delete") && has("--force"),
                    Some("checkout" | "restore") => has("--") || has(".") || has("--force"),
                    Some("stash") => has("drop") || has("clear"),
                    _ => false,
                }
            }
            "crontab" => has("-r"),
            "emerge" => has("-C") || has("--unmerge") || has("--depclean") || has("-c"),
            _ => false,
        };
        if risky {
            return Some(format!("may destroy data ({first}): {segment}"));
        }
    }
    None
}

/// The commands of a chain or pipeline that [`bash_risk`] judges one by one.
/// Split at quotes too (finds `bash -c 'sudo ...'`), and without splitting
/// at quotes (keeps `find . -name '*.o' -delete` in one piece).
fn risk_segments(command: &str) -> Vec<String> {
    let unquoted_segments = command
        .split([';', '&', '|', '\n', '(', ')', '`'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string);
    shell_command_segments(command)
        .into_iter()
        .chain(unquoted_segments)
        .collect()
}

/// Programs whose arguments only name what they act on (a process, a
/// time): "always" covers the program, whatever the target.
const TARGET_ONLY_PROGRAMS: &[&str] = &[
    "kill", "pkill", "killall", "shutdown", "reboot", "poweroff", "halt",
];

/// What "always allow in this session" remembers for a bash command: the
/// first two words of the command that made it need approval (`sudo emerge`,
/// `rm -rf`), so approving `sudo emerge` does not also approve `sudo rm`.
/// A second word that is only a target is left out, or the key would never
/// match again: `kill 1234`, `pkill llama-server` and `rm build/` are
/// remembered as `kill`, `pkill` and `rm`.
#[must_use]
pub fn bash_approval_key(command: &str) -> String {
    let segments = shell_command_segments(command);
    let risky = segments
        .iter()
        .find(|segment| bash_command_risk(segment).is_some())
        .or_else(|| segments.first());
    risky
        .map(|segment| {
            let words: Vec<&str> = segment.split_whitespace().take(2).collect();
            match words.as_slice() {
                [program, second]
                    if TARGET_ONLY_PROGRAMS
                        .contains(&program.rsplit('/').next().unwrap_or(program))
                        || second.chars().all(|c| c.is_ascii_digit())
                        || second.contains('/') =>
                {
                    (*program).to_string()
                }
                _ => words.join(" "),
            }
        })
        .unwrap_or_else(|| command.trim().to_string())
}

/// The mode a bash command needs: full access (so the user is asked in the
/// default mode) when [`bash_command_risk`] flags it, read-only when every
/// command in it only reads, workspace-write otherwise.
fn bash_command_requirement(command: &str, root_is_risky: bool) -> PermissionMode {
    if bash_risk(command, root_is_risky).is_some() {
        return PermissionMode::DangerFullAccess;
    }
    let read_only = shell_command_segments(command).iter().all(|segment| {
        crate::bash_validation::classify_command(segment)
            == crate::bash_validation::CommandIntent::ReadOnly
    }) && !command.contains('>');
    if read_only {
        PermissionMode::ReadOnly
    } else {
        PermissionMode::WorkspaceWrite
    }
}

/// Tools that write files; writing outside the current directory needs
/// full access, so the user is asked first in the default mode.
const FILE_WRITING_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "notebook_edit",
    "write_docx",
    "write_pptx",
    "write_xlsx",
];

/// Whether `path` (`~` expanded, relative to the current directory) exists.
/// A dangling symlink counts: writing through it would change something.
fn path_exists_from_cwd(path: &str) -> bool {
    let expanded = crate::expand_home(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(expanded),
            Err(_) => return true,
        }
    };
    absolute.symlink_metadata().is_ok()
}

/// Whether `path` (`~` expanded, relative to the current directory) is inside
/// the current directory, following symlinks for the parts that exist.
fn path_inside_cwd(path: &str) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let expanded = crate::expand_home(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    // Canonicalize the deepest existing ancestor; the rest may not exist yet.
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => break,
        }
    }
    let mut resolved = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    for name in rest.into_iter().rev() {
        if name == ".." {
            resolved.pop();
        } else if name != "." {
            resolved.push(name);
        }
    }
    resolved.starts_with(&cwd)
}

/// The simple commands in a shell line: split at `;`, `&`, `|`, newlines,
/// subshells and quotes, with leading `VAR=value` assignments and wrappers
/// such as `env`, `time` and `nohup` removed.
fn shell_command_segments(command: &str) -> Vec<String> {
    const WRAPPERS: &[&str] = &[
        "env", "time", "nohup", "nice", "exec", "command", "builtin", "xargs", "then", "do", "else",
    ];
    command
        .split(|ch: char| {
            matches!(
                ch,
                ';' | '&' | '|' | '\n' | '(' | ')' | '`' | '\'' | '"' | '{' | '}'
            )
        })
        .filter_map(|segment| {
            let mut words = segment.split_whitespace().peekable();
            while let Some(word) = words.peek() {
                let assignment = word
                    .split_once('=')
                    .is_some_and(|(name, _)| !name.is_empty() && !name.starts_with('-'));
                if assignment || WRAPPERS.contains(word) || word.starts_with('$') {
                    words.next();
                } else {
                    break;
                }
            }
            let rest = words.collect::<Vec<_>>().join(" ");
            (!rest.is_empty()).then_some(rest)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionRule {
    raw: String,
    tool_name: String,
    matcher: PermissionRuleMatcher,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PermissionRuleMatcher {
    Any,
    Exact(String),
    Prefix(String),
}

impl PermissionRule {
    fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let open = find_first_unescaped(trimmed, '(');
        let close = find_last_unescaped(trimmed, ')');

        if let (Some(open), Some(close)) = (open, close) {
            if close == trimmed.len() - 1 && open < close {
                let tool_name = trimmed[..open].trim();
                let content = &trimmed[open + 1..close];
                if !tool_name.is_empty() {
                    let matcher = parse_rule_matcher(content);
                    return Self {
                        raw: trimmed.to_string(),
                        // #94: normalize tool name to lowercase to match runtime convention
                        tool_name: tool_name.to_lowercase(),
                        matcher,
                    };
                }
            }
        }

        Self {
            raw: trimmed.to_string(),
            // #94: normalize tool name to lowercase to match runtime convention
            tool_name: trimmed.to_lowercase(),
            matcher: PermissionRuleMatcher::Any,
        }
    }

    fn matches(&self, tool_name: &str, input: &str) -> bool {
        // `parse` lowercases the rule's tool name, so compare without case:
        // otherwise a rule like "WebSearch" never matched the `WebSearch` tool.
        if !self.tool_name.eq_ignore_ascii_case(tool_name) {
            return false;
        }

        match &self.matcher {
            PermissionRuleMatcher::Any => true,
            PermissionRuleMatcher::Exact(_) | PermissionRuleMatcher::Prefix(_) => {
                extract_permission_subject(input)
                    .is_some_and(|candidate| self.matches_subject(&candidate))
            }
        }
    }

    fn matches_subject(&self, subject: &str) -> bool {
        match &self.matcher {
            PermissionRuleMatcher::Any => true,
            PermissionRuleMatcher::Exact(expected) => subject == expected,
            PermissionRuleMatcher::Prefix(prefix) => subject.starts_with(prefix.as_str()),
        }
    }
}

fn parse_rule_matcher(content: &str) -> PermissionRuleMatcher {
    let unescaped = unescape_rule_content(content.trim());
    if unescaped.is_empty() || unescaped == "*" {
        PermissionRuleMatcher::Any
    } else if let Some(prefix) = unescaped.strip_suffix(":*") {
        PermissionRuleMatcher::Prefix(prefix.to_string())
    } else {
        PermissionRuleMatcher::Exact(unescaped)
    }
}

fn unescape_rule_content(content: &str) -> String {
    content
        .replace(r"\(", "(")
        .replace(r"\)", ")")
        .replace(r"\\", r"\")
}

fn find_first_unescaped(value: &str, needle: char) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in value.char_indices() {
        if ch == '\\' {
            escaped = !escaped;
            continue;
        }
        if ch == needle && !escaped {
            return Some(idx);
        }
        escaped = false;
    }
    None
}

fn find_last_unescaped(value: &str, needle: char) -> Option<usize> {
    let chars = value.char_indices().collect::<Vec<_>>();
    for (pos, (idx, ch)) in chars.iter().enumerate().rev() {
        if *ch != needle {
            continue;
        }
        let mut backslashes = 0;
        for (_, prev) in chars[..pos].iter().rev() {
            if *prev == '\\' {
                backslashes += 1;
            } else {
                break;
            }
        }
        if backslashes % 2 == 0 {
            return Some(*idx);
        }
    }
    None
}

fn extract_permission_subject(input: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(input).ok();
    if let Some(Value::Object(object)) = parsed {
        for key in [
            "command",
            "path",
            "file_path",
            "filePath",
            "notebook_path",
            "notebookPath",
            "url",
            "pattern",
            "code",
            "message",
        ] {
            if let Some(value) = object.get(key).and_then(Value::as_str) {
                return Some(value.to_string());
            }
        }
    }

    (!input.trim().is_empty()).then(|| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        PermissionContext, PermissionMode, PermissionOutcome, PermissionOverride, PermissionPolicy,
        PermissionPromptDecision, PermissionPrompter, PermissionRequest,
    };
    use crate::config::RuntimePermissionRuleConfig;

    struct RecordingPrompter {
        seen: Vec<PermissionRequest>,
        allow: bool,
    }

    impl PermissionPrompter for RecordingPrompter {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            self.seen.push(request.clone());
            if self.allow {
                PermissionPromptDecision::Allow
            } else {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }
    }

    #[test]
    fn host_execution_always_needs_explicit_permission_and_respects_denials() {
        let input =
            r#"{"command":"sudo emerge --pretend app-misc/test","dangerouslyDisableSandbox":true}"#;
        let rules = RuntimePermissionRuleConfig::new(
            vec!["bash(*)".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        for mode in [
            PermissionMode::WorkspaceWrite,
            PermissionMode::Prompt,
            PermissionMode::Allow,
            PermissionMode::DangerFullAccess,
        ] {
            let policy = PermissionPolicy::new(mode).with_permission_rules(&rules);
            let context = PermissionContext::new(Some(PermissionOverride::Allow), None);
            assert!(matches!(
                policy.authorize_with_context("bash", input, &context, None),
                PermissionOutcome::Deny { .. }
            ));
            let mut prompter = RecordingPrompter {
                seen: Vec::new(),
                allow: true,
            };
            assert_eq!(
                policy.authorize_with_context("bash", input, &context, Some(&mut prompter)),
                PermissionOutcome::Allow
            );
            assert_eq!(prompter.seen.len(), 1);
            assert!(prompter.seen[0]
                .reason
                .as_ref()
                .unwrap()
                .contains("outside Claw's sandbox"));
            prompter.allow = false;
            assert!(matches!(
                policy.authorize("bash", input, Some(&mut prompter)),
                PermissionOutcome::Deny { .. }
            ));
        }
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            vec!["bash(sudo:*)".to_string()],
            Vec::new(),
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::Allow).with_permission_rules(&rules);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };
        assert!(matches!(
            policy.authorize("bash", input, Some(&mut prompter)),
            PermissionOutcome::Deny { .. }
        ));
        assert!(prompter.seen.is_empty());
        assert!(matches!(
            PermissionPolicy::new(PermissionMode::ReadOnly).authorize(
                "bash",
                input,
                Some(&mut prompter)
            ),
            PermissionOutcome::Deny { .. }
        ));
    }

    #[test]
    fn allows_tools_when_active_mode_meets_requirement() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);

        assert_eq!(
            policy.authorize("read_file", "{}", None),
            PermissionOutcome::Allow
        );
        assert_eq!(
            policy.authorize("write_file", "{}", None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn denies_read_only_escalations_without_prompt() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);

        assert!(matches!(
            policy.authorize("write_file", "{}", None),
            PermissionOutcome::Deny { reason } if reason.contains("requires workspace-write permission")
        ));
        assert!(matches!(
            policy.authorize("bash", "{}", None),
            PermissionOutcome::Deny { reason } if reason.contains("requires danger-full-access permission")
        ));
    }

    #[test]
    fn prompts_for_workspace_write_to_danger_full_access_escalation() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize("bash", "echo hi", Some(&mut prompter));

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
        assert_eq!(prompter.seen[0].tool_name, "bash");
        assert_eq!(
            prompter.seen[0].current_mode,
            PermissionMode::WorkspaceWrite
        );
        assert_eq!(
            prompter.seen[0].required_mode,
            PermissionMode::DangerFullAccess
        );
    }

    #[test]
    fn honors_prompt_rejection_reason() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: false,
        };

        assert!(matches!(
            policy.authorize("bash", "echo hi", Some(&mut prompter)),
            PermissionOutcome::Deny { reason } if reason == "not now"
        ));
    }

    #[test]
    fn applies_rule_based_denials_and_allows() {
        let rules = RuntimePermissionRuleConfig::new(
            vec!["bash(git:*)".to_string()],
            vec!["bash(rm -rf:*)".to_string()],
            Vec::new(),
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);

        assert_eq!(
            policy.authorize("bash", r#"{"command":"git status"}"#, None),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("bash", r#"{"command":"rm -rf /tmp/x"}"#, None),
            PermissionOutcome::Deny { reason } if reason.contains("denied by rule")
        ));
    }

    #[test]
    fn whole_tool_deny_rule_does_not_block_bash() {
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            vec!["WebSearch".to_string()],
            Vec::new(),
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::Allow).with_permission_rules(&rules);
        assert_eq!(
            policy.authorize("bash", r#"{"command":"echo hi"}"#, None),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("WebSearch", r#"{"query":"x"}"#, None),
            PermissionOutcome::Deny { .. }
        ));
    }

    #[test]
    fn denied_tools_denies_listed_tools_unconditionally() {
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec!["bash".to_string(), "write_file".to_string()],
        );
        let policy = PermissionPolicy::new(PermissionMode::Allow).with_permission_rules(&rules);

        let result = policy.authorize("bash", "echo hello", None);
        assert!(matches!(
            result,
            PermissionOutcome::Deny { reason } if reason.contains("denied_tools")
        ));

        let result = policy.authorize("write_file", "{}", None);
        assert!(matches!(
            result,
            PermissionOutcome::Deny { reason } if reason.contains("denied_tools")
        ));

        let result = policy.authorize("read_file", "{}", None);
        assert_eq!(result, PermissionOutcome::Allow);
    }

    #[test]
    fn ask_rules_force_prompt_even_when_mode_allows() {
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            Vec::new(),
            vec!["bash(git:*)".to_string()],
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize("bash", r#"{"command":"git status"}"#, Some(&mut prompter));

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
        assert!(prompter.seen[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("ask rule")));
    }

    #[test]
    fn hook_allow_still_respects_ask_rules() {
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            Vec::new(),
            vec!["bash(git:*)".to_string()],
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let context = PermissionContext::new(
            Some(PermissionOverride::Allow),
            Some("hook approved".to_string()),
        );
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize_with_context(
            "bash",
            r#"{"command":"git status"}"#,
            &context,
            Some(&mut prompter),
        );

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
    }

    #[test]
    fn hook_deny_short_circuits_permission_flow() {
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let context = PermissionContext::new(
            Some(PermissionOverride::Deny),
            Some("blocked by hook".to_string()),
        );

        assert_eq!(
            policy.authorize_with_context("bash", "{}", &context, None),
            PermissionOutcome::Deny {
                reason: "blocked by hook".to_string(),
            }
        );
    }

    #[test]
    fn hook_ask_forces_prompt() {
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let context = PermissionContext::new(
            Some(PermissionOverride::Ask),
            Some("hook requested confirmation".to_string()),
        );
        let mut prompter = RecordingPrompter {
            seen: Vec::new(),
            allow: true,
        };

        let outcome = policy.authorize_with_context("bash", "{}", &context, Some(&mut prompter));

        assert_eq!(outcome, PermissionOutcome::Allow);
        assert_eq!(prompter.seen.len(), 1);
        assert_eq!(
            prompter.seen[0].reason.as_deref(),
            Some("hook requested confirmation")
        );
    }

    #[test]
    fn ask_rules_for_bash_match_commands_anywhere_in_a_chain() {
        let rules = RuntimePermissionRuleConfig::new(
            Vec::new(),
            Vec::new(),
            vec!["bash(sudo:*)".to_string()],
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let asks = |command: &str| {
            let input = serde_json::json!({ "command": command }).to_string();
            matches!(
                policy.authorize("bash", &input, None),
                PermissionOutcome::Deny { ref reason } if reason.contains("ask rule")
            )
        };

        assert!(asks("sudo emerge --sync"));
        assert!(asks("cd /etc && sudo vim hosts"));
        assert!(asks("echo 1 | sudo tee /proc/sys/vm/drop_caches"));
        assert!(asks("FOO=1 time sudo ls"));
        assert!(asks("bash -c 'sudo id'"));
        assert!(asks("echo $(sudo cat /etc/shadow)"));
        assert!(!asks("ls -la /etc"));
        assert!(!asks("echo pseudo-terminal"));
        assert!(!asks("cat sudoers.txt"));
    }

    #[test]
    fn allow_rules_still_match_only_the_whole_command() {
        let rules = RuntimePermissionRuleConfig::new(
            vec!["bash(git:*)".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let allowed = |command: &str| {
            let input = serde_json::json!({ "command": command }).to_string();
            policy.authorize("bash", &input, None) == PermissionOutcome::Allow
        };

        assert!(allowed("git status"));
        assert!(!allowed("ls; git status"));
    }

    #[test]
    fn prefix_allow_rules_cover_only_the_risky_commands_they_match() {
        let rules = RuntimePermissionRuleConfig::new(
            vec!["bash(shutdown:*)".to_string(), "bash(git:*)".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .with_risky_bash_asking_in_every_mode(true);
        let full_access = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let read_only = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let allowed = |policy: &PermissionPolicy, command: &str| {
            let input = serde_json::json!({ "command": command }).to_string();
            policy.authorize("bash", &input, None) == PermissionOutcome::Allow
        };

        assert!(allowed(&full_access, "shutdown -h +5"));
        assert!(allowed(&full_access, "shutdown -r now"));
        assert!(allowed(&full_access, "sync && shutdown now"));
        assert!(!allowed(&full_access, "shutdown now; rm -rf ~"));
        assert!(!allowed(&full_access, "shutdown now $(pkill sshd)"));
        assert!(!allowed(&full_access, "sudo shutdown now"));
        assert!(allowed(&read_only, "git status"));
        assert!(!allowed(&read_only, "git status; rm -rf build"));
    }

    #[test]
    fn rules_match_tool_names_with_capitals() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("WebSearch", PermissionMode::DangerFullAccess)
            .with_tool_requirement("WebFetch", PermissionMode::DangerFullAccess)
            .with_permission_rules(&RuntimePermissionRuleConfig::new(
                vec!["WebSearch".to_string()],
                vec!["WebFetch".to_string()],
                Vec::new(),
                Vec::new(),
            ));
        let input = r#"{"query":"slock"}"#;
        assert_eq!(
            policy.authorize("WebSearch", input, None),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("WebFetch", r#"{"url":"https://x"}"#, None),
            PermissionOutcome::Deny { ref reason } if reason.contains("denied by rule")
        ));
    }

    #[test]
    fn changing_existing_files_outside_the_current_directory_needs_approval() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly);
        let input = |path: &str| serde_json::json!({ "path": path, "content": "x" }).to_string();

        assert_eq!(
            policy.authorize("write_file", &input("notes/inside.txt"), None),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("write_file", &input("/etc/passwd"), None),
            PermissionOutcome::Deny { ref reason } if reason.contains("requires approval")
        ));
        // A new file outside the directory destroys nothing, so no question.
        assert_eq!(
            policy.authorize(
                "write_file",
                &input("/tmp/claw-permission-test-does-not-exist/new.txt"),
                None
            ),
            PermissionOutcome::Allow
        );
        assert_eq!(
            policy.authorize("read_file", &input("/etc/hostname"), None),
            PermissionOutcome::Allow,
            "reading anywhere is allowed"
        );
    }

    #[test]
    fn updating_memory_files_never_asks() {
        let _env = crate::test_env_lock();
        let home =
            std::env::temp_dir().join(format!("claw-permission-memory-{}", std::process::id()));
        let original = std::env::var_os("CLAW_CONFIG_HOME");
        let original_claude = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAW_CONFIG_HOME", &home);
        // Not the real ~/.claude: memory would go there when it exists.
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join("no-claude"));
        let dir = crate::memory::memory_dir(&std::env::current_dir().expect("cwd"));
        std::fs::create_dir_all(&dir).expect("create memory dir");
        let index = dir.join(crate::memory::MEMORY_INDEX);
        std::fs::write(&index, "- [x](x.md)\n").expect("write index");

        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("edit_file", PermissionMode::WorkspaceWrite);
        let input = |path: &str| serde_json::json!({ "path": path }).to_string();
        let outcome = policy.authorize("edit_file", &input(&index.display().to_string()), None);
        let escape = dir.join("..").join("..").join("..").join("CLAUDE.md");
        std::fs::write(home.join("CLAUDE.md"), "x").expect("write file");
        let escape_outcome =
            policy.authorize("edit_file", &input(&escape.display().to_string()), None);

        match original {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        match original_claude {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(outcome, PermissionOutcome::Allow);
        assert!(matches!(escape_outcome, PermissionOutcome::Deny { .. }));
    }

    #[test]
    fn bash_commands_need_approval_only_when_privileged_or_destructive() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(
                &RuntimePermissionRuleConfig::new(Vec::new(), Vec::new(), Vec::new(), Vec::new())
                    .with_risky_bash_only(true),
            );
        let asks = |command: &str| {
            let input = serde_json::json!({ "command": command }).to_string();
            matches!(
                policy.authorize("bash", &input, None),
                PermissionOutcome::Deny { .. }
            )
        };

        for command in [
            "ls -la",
            "cd rust && grep -r tokio crates/ | head -20",
            "cargo build --release",
            "git status && git commit -m wip",
            "emerge -pv firefox",
            "python3 script.py > out.txt",
            "mkdir -p build && cp a.txt build/",
            "systemctl status sshd",
        ] {
            assert!(!asks(command), "should run without asking: {command}");
        }
        for command in [
            "sudo emerge --sync",
            "cd /etc && sudo vim hosts",
            "rm -rf build",
            "rm notes.txt",
            "find . -name '*.o' -delete",
            "git reset --hard HEAD~1",
            "git push --force origin main",
            "git clean -fdx",
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sdb1",
            "chmod -R 777 /srv",
            "pkill firefox",
            "echo x | doas tee /etc/motd",
        ] {
            assert!(asks(command), "should ask first: {command}");
        }
    }

    #[test]
    fn destructive_only_approval_asks_for_destruction_in_every_mode_but_not_for_root() {
        let rules =
            RuntimePermissionRuleConfig::new(Vec::new(), Vec::new(), Vec::new(), Vec::new())
                .with_risky_bash_asking_in_every_mode(false);
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite)
            .with_permission_rules(&rules);
        let asks = |command: &str| {
            let input = serde_json::json!({ "command": command }).to_string();
            matches!(
                policy.authorize("bash", &input, None),
                PermissionOutcome::Deny { .. }
            )
        };

        for command in [
            "sudo emerge --sync",
            "cd /etc && sudo vim hosts",
            "echo x | doas tee /etc/motd",
            "sudo -u portage ls /var/db/repos",
            "sudo systemctl restart sshd",
            "cargo build --release",
        ] {
            assert!(!asks(command), "should run without asking: {command}");
        }
        for command in [
            "rm -rf build",
            "sudo rm -rf /var/tmp/portage",
            "sudo -u root rm notes.txt",
            "doas shred secret.key",
            "find . -name '*.o' -delete",
            "git reset --hard HEAD~1",
            "git push --force origin main",
            "sudo dd if=/dev/zero of=/dev/sda",
            "sudo mkfs.ext4 /dev/sdb1",
            "sudo emerge --depclean",
            "pkill firefox",
            "su -c 'rm -rf /opt/old'",
        ] {
            assert!(asks(command), "should ask first: {command}");
        }

        // Writing files anywhere needs no approval in full-access mode.
        let outside = serde_json::json!({ "path": "/etc/hosts", "content": "x" }).to_string();
        assert_eq!(
            policy.authorize("write_file", &outside, None),
            PermissionOutcome::Allow
        );
        // A whole-command allow rule still pre-approves a destructive command.
        let allowed = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(
                &RuntimePermissionRuleConfig::new(
                    vec!["bash(rm -rf target:*)".to_string()],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )
                .with_risky_bash_asking_in_every_mode(false),
            );
        let input = serde_json::json!({ "command": "rm -rf target" }).to_string();
        assert_eq!(
            allowed.authorize("bash", &input, None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn sudo_and_destructive_approval_asks_for_root_and_destruction_in_full_access() {
        let rules =
            RuntimePermissionRuleConfig::new(Vec::new(), Vec::new(), Vec::new(), Vec::new())
                .with_risky_bash_asking_in_every_mode(true);
        let policy = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        let asks = |command: &str| {
            let input = serde_json::json!({ "command": command }).to_string();
            matches!(
                policy.authorize("bash", &input, None),
                PermissionOutcome::Deny { .. }
            )
        };
        for command in ["cargo build --release", "emerge -pv vim", "cp a /tmp/b"] {
            assert!(!asks(command), "should run without asking: {command}");
        }
        for command in [
            "sudo id -u",
            "cd /etc && sudo vim hosts",
            "echo x | doas tee /etc/motd",
            "rm notes.txt",
            "git push --force origin main",
        ] {
            assert!(asks(command), "should ask first: {command}");
        }
    }

    #[test]
    fn host_execution_needs_no_extra_approval_when_the_sandbox_is_off() {
        let input = r#"{"command":"sudo emerge --pretend vim","dangerouslyDisableSandbox":true}"#;
        let rules =
            RuntimePermissionRuleConfig::new(Vec::new(), Vec::new(), Vec::new(), Vec::new())
                .with_risky_bash_asking_in_every_mode(false);
        let sandboxed = PermissionPolicy::new(PermissionMode::DangerFullAccess)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess)
            .with_permission_rules(&rules);
        assert!(matches!(
            sandboxed.authorize("bash", input, None),
            PermissionOutcome::Deny { .. }
        ));
        let unsandboxed = sandboxed.with_sandbox_enabled(false);
        assert_eq!(
            unsandboxed.authorize("bash", input, None),
            PermissionOutcome::Allow
        );
    }

    #[test]
    fn bash_risk_explains_why_approval_is_needed() {
        let reason = super::bash_command_risk("cd /tmp && sudo emerge -av vim").expect("risky");
        assert!(reason.contains("root"), "{reason}");
        assert!(reason.contains("sudo emerge"), "{reason}");
        assert_eq!(super::bash_command_risk("cat /etc/hostname"), None);
    }

    #[test]
    fn every_bash_command_needs_approval_unless_risky_only_is_enabled() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("bash", PermissionMode::DangerFullAccess);
        let input = serde_json::json!({ "command": "ls -la" }).to_string();
        assert!(matches!(
            policy.authorize("bash", &input, None),
            PermissionOutcome::Deny { .. }
        ));
    }

    #[test]
    fn approval_keys_name_the_risky_command() {
        assert_eq!(
            super::bash_approval_key("cd /etc && sudo emerge -av vim"),
            "sudo emerge"
        );
        assert_eq!(super::bash_approval_key("rm -rf build/"), "rm -rf");
        assert_eq!(super::bash_approval_key("ls -la"), "ls -la");
        // Targets are not part of the key, so "always" keeps matching.
        assert_eq!(super::bash_approval_key("kill 1234"), "kill");
        assert_eq!(super::bash_approval_key("kill -9 5678"), "kill");
        assert_eq!(super::bash_approval_key("pkill llama-server"), "pkill");
        assert_eq!(
            super::bash_approval_key("pkill -f 'python app.py'"),
            "pkill"
        );
        assert_eq!(super::bash_approval_key("killall firefox"), "killall");
        assert_eq!(super::bash_approval_key("rm build/old.o"), "rm");
        // A privilege wrapper still keeps the program it runs.
        assert_eq!(super::bash_approval_key("sudo kill 1234"), "sudo kill");
        assert_eq!(super::bash_approval_key("sudo rm -rf /tmp/x"), "sudo rm");
    }
}
