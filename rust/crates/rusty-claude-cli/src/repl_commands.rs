//! REPL slash commands that used to be registered as stubs: checkpoints and
//! undo, plan mode, reasoning effort, themes, usage/context reports, hooks,
//! background jobs, clipboard, session naming, extra directories, retry, and
//! the prompt-backed review commands.
//!
//! Checkpoints, background jobs and themes are ported from `AzureAiHarness`.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use runtime::{
    estimate_session_tokens, global_background_jobs, CheckpointStack, ContentBlock, GitSnapshots,
    MessageRole, UsageTracker,
};

use crate::render::{current_theme, set_current_theme, TerminalRenderer, ThemeName};
use crate::LiveCli;

const PLAN_MODE_PROMPT: &str = "# Plan mode\n\
You are in plan mode: tools are limited to read-only access. Investigate the \
codebase, then reply with a concise, numbered implementation plan and the files \
you would change. Do not attempt to modify anything. The user will leave plan \
mode with /plan off when they want the plan executed.";

type CommandResult = Result<bool, Box<dyn std::error::Error>>;

/// Applies the persisted theme (`CLAW_THEME` wins over the `theme` setting).
pub(crate) fn apply_configured_theme() {
    if env::var_os("CLAW_THEME").is_some() {
        set_current_theme(ThemeName::from_env());
        return;
    }
    if let Some(theme) = env::current_dir()
        .ok()
        .and_then(|cwd| runtime::ConfigLoader::default_for(cwd).load().ok())
        .and_then(|config| {
            config
                .get("theme")
                .and_then(|v| v.as_str())
                .and_then(|name| ThemeName::parse(name).ok())
        })
    {
        set_current_theme(theme);
    }
}

impl LiveCli {
    /// System prompt plus the sections contributed by plan mode and `/add-dir`.
    pub(crate) fn effective_system_prompt(&self) -> Vec<String> {
        let mut prompt = self.system_prompt.clone();
        let added = tools::additional_workspace_roots();
        if !added.is_empty() {
            prompt.push(format!(
                "# Additional working directories\nBesides the current directory, the user \
                 added these directories to the workspace; you may read and edit files in them:\n{}",
                added
                    .iter()
                    .map(|dir| format!("- {}", dir.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if self.plan_mode_previous.is_some() {
            prompt.push(PLAN_MODE_PROMPT.to_string());
        }
        if let Some(style) = &self.output_style {
            prompt.push(crate::claude_commands::output_style_section(style));
        }
        if let Some(model) = &self.advisor {
            prompt.push(crate::claude_commands::advisor_section(model));
        }
        prompt
    }

    // ───────────────────────────── checkpoints ─────────────────────────────

    fn snapshots(&mut self) -> Option<&GitSnapshots> {
        let cwd = env::current_dir().ok()?;
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        if self
            .snapshots
            .as_ref()
            .is_none_or(|snapshots| snapshots.work_tree() != cwd)
        {
            self.snapshots = Some(GitSnapshots::open(&cwd));
        }
        self.snapshots.as_ref().filter(|s| s.is_available())
    }

    /// Takes the "before" snapshot for a REPL request.
    pub(crate) fn begin_checkpoint(&mut self, label: &str) {
        if !self.checkpoints_enabled {
            return;
        }
        let message_index = self.runtime.session().messages.len();
        let generation = compaction_generation(self.runtime.session());
        let Some(before) = self.snapshots().and_then(|s| s.snapshot(label)) else {
            return;
        };
        let mut stack = CheckpointStack::load(&self.session.path);
        stack.begin(label, before, message_index, generation);
        let _ = stack.save(&self.session.path);
    }

    /// Takes the "after" snapshot for the request started by `begin_checkpoint`.
    pub(crate) fn finish_checkpoint(&mut self) {
        if !self.checkpoints_enabled {
            return;
        }
        let mut stack = CheckpointStack::load(&self.session.path);
        let Some(before) = stack
            .last()
            .filter(|cp| cp.after.is_none())
            .map(|cp| cp.before.clone())
        else {
            return;
        };
        let Some(snapshots) = self.snapshots() else {
            return;
        };
        let Some(after) = snapshots.snapshot("after request") else {
            return;
        };
        let changed = snapshots
            .changed_files(&before, &after)
            .into_iter()
            .map(|(_, path)| path)
            .collect();
        stack.finish(after, changed);
        let _ = stack.save(&self.session.path);
    }

    /// Prefixes the prompt with notices about background jobs that exited.
    pub(crate) fn with_job_notices(input: &str) -> String {
        let notices = global_background_jobs().drain_notices();
        if notices.is_empty() {
            input.to_string()
        } else {
            format!(
                "<system-reminder>\n{}\n</system-reminder>\n\n{input}",
                notices.join("\n")
            )
        }
    }

    /// `/undo`: revert the files changed by the last request and rewind the
    /// conversation to before it.
    pub(crate) fn run_undo(&mut self) -> CommandResult {
        self.undo_checkpoints(1)
    }

    /// `/rewind [n]`: list checkpoints, or undo the last `n` requests.
    pub(crate) fn run_rewind(&mut self, steps: Option<&str>) -> CommandResult {
        let Some(steps) = steps.map(str::trim).filter(|s| !s.is_empty()) else {
            self.print_checkpoints();
            return Ok(false);
        };
        let count: usize = steps
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("usage: /rewind [steps]  (got `{steps}`)"))?;
        self.undo_checkpoints(count)
    }

    fn print_checkpoints(&mut self) {
        let stack = CheckpointStack::load(&self.session.path);
        if stack.is_empty() {
            match self.snapshots() {
                Some(_) => println!("No checkpoints yet in this session."),
                None => println!(
                    "Checkpoints are unavailable: {}",
                    self.snapshots
                        .as_ref()
                        .and_then(GitSnapshots::unavailable_reason)
                        .unwrap_or("unknown reason")
                ),
            }
            return;
        }
        println!("Checkpoints (newest first)");
        for (index, cp) in stack.checkpoints.iter().rev().enumerate() {
            let files = match cp.changed_files.len() {
                0 => "no file changes".to_string(),
                1 => "1 file".to_string(),
                n => format!("{n} files"),
            };
            println!("  {:>3}  {:<16} {}", index + 1, files, cp.label);
        }
        println!("Use /undo to revert the newest, or /rewind <n> to revert the newest n.");
    }

    fn undo_checkpoints(&mut self, count: usize) -> CommandResult {
        let mut stack = CheckpointStack::load(&self.session.path);
        if stack.is_empty() {
            println!("Nothing to undo.");
            return Ok(false);
        }
        let mut rewind_to = None;
        let generation = compaction_generation(self.runtime.session());
        for _ in 0..count {
            let Some(cp) = stack.pop() else { break };
            println!("Undoing: {}", cp.label);
            match (cp.after.as_deref(), self.snapshots()) {
                (Some(after), Some(snapshots)) => {
                    let log = snapshots.restore(&cp.before, after);
                    if log.is_empty() {
                        println!("  no file changes in that request");
                    }
                    for action in log {
                        println!("  {action}");
                    }
                }
                (None, _) => println!("  request did not finish; files left as they are"),
                (_, None) => println!("  snapshots unavailable; files left as they are"),
            }
            if cp.compaction_generation == generation {
                rewind_to = Some(cp.message_index);
            } else {
                println!("  conversation not rewound (it was compacted since)");
            }
        }
        stack.save(&self.session.path)?;
        if let Some(index) = rewind_to {
            let messages = &mut self.runtime.session_mut().messages;
            if index <= messages.len() {
                messages.truncate(index);
                println!("Conversation rewound to before that request.");
            }
        }
        Ok(true)
    }

    // ───────────────────────────── modes ─────────────────────────────

    /// `/plan [on|off|<task>]`: read-only planning mode.
    pub(crate) fn run_plan(&mut self, mode: Option<&str>) -> CommandResult {
        let enable = match mode.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => self.plan_mode_previous.is_none(),
            Some("on" | "enter" | "start") => true,
            Some("off" | "exit" | "stop" | "done") => false,
            // `/plan <task>`: plan mode on, and start planning that task.
            Some(_) => {
                let task = mode.unwrap_or_default().trim().to_string();
                if self.plan_mode_previous.is_none() {
                    let previous = self.permission_mode;
                    self.set_permissions(Some("read-only".to_string()))?;
                    self.plan_mode_previous = Some(previous);
                    println!("Plan mode on: read-only tools; the model will propose a plan instead of editing.");
                }
                crate::report_repl_turn_error(self.run_turn(&task));
                return Ok(true);
            }
        };
        match (enable, self.plan_mode_previous) {
            (true, Some(_)) => println!("Plan mode is already on. /plan off to leave it."),
            (true, None) => {
                let previous = self.permission_mode;
                self.set_permissions(Some("read-only".to_string()))?;
                self.plan_mode_previous = Some(previous);
                println!("Plan mode on: read-only tools; the model will propose a plan instead of editing.");
            }
            (false, None) => println!("Plan mode is off."),
            (false, Some(previous)) => {
                self.plan_mode_previous = None;
                self.set_permissions(Some(previous.as_str().to_string()))?;
                println!(
                    "Plan mode off: permissions restored to {}.",
                    previous.as_str()
                );
            }
        }
        Ok(true)
    }

    /// `/effort [low|medium|high|default]`.
    pub(crate) fn run_effort(&mut self, level: Option<&str>) {
        let Some(level) = level.map(|l| l.trim().to_ascii_lowercase()) else {
            println!(
                "Reasoning effort: {}",
                self.reasoning_effort.as_deref().unwrap_or("model default")
            );
            return;
        };
        match level.as_str() {
            "low" | "medium" | "high" => {
                self.set_reasoning_effort(Some(level.clone()));
                println!("Reasoning effort: {level}");
            }
            "default" | "reset" | "off" | "none" => {
                self.set_reasoning_effort(None);
                println!("Reasoning effort: model default");
            }
            other => eprintln!("usage: /effort [low|medium|high|default]  (got `{other}`)"),
        }
    }

    // ───────────────────────────── reports ─────────────────────────────

    /// `/usage`: token usage and cost for the session.
    pub(crate) fn run_usage(&self) {
        let tracker = UsageTracker::from_session(self.runtime.session());
        let total = tracker.cumulative_usage();
        let last = tracker.current_turn_usage();
        let pricing = runtime::pricing_for_model(&self.model);
        let cost = total
            .estimate_cost_usd_with_pricing(
                pricing.unwrap_or_else(runtime::ModelPricing::default_sonnet_tier),
            )
            .total_cost_usd();
        println!("Usage");
        println!("  Model            {}", self.model);
        println!("  API calls        {}", tracker.turns());
        println!("  Input tokens     {}", total.input_tokens);
        println!("  Output tokens    {}", total.output_tokens);
        println!("  Cache create     {}", total.cache_creation_input_tokens);
        println!("  Cache read       {}", total.cache_read_input_tokens);
        if total.input_tokens + total.cache_read_input_tokens > 0 {
            let hit = f64::from(total.cache_read_input_tokens)
                / f64::from(total.input_tokens + total.cache_read_input_tokens)
                * 100.0;
            println!("  Cache hit rate   {hit:.1}%");
        }
        println!(
            "  Last call        {} in / {} out",
            last.input_tokens, last.output_tokens
        );
        println!(
            "  Estimated cost   {}{}",
            runtime::format_usd(cost),
            if pricing.is_none() {
                " (unknown model; priced as Sonnet tier)"
            } else {
                ""
            }
        );
    }

    /// `/context [show|clear]`.
    pub(crate) fn run_context(&mut self, action: Option<&str>) -> CommandResult {
        match action.map(|a| a.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "show") => {
                let session = self.runtime.session();
                let estimated = estimate_session_tokens(session);
                let window =
                    api::model_token_limit(&self.model).map(|limit| limit.context_window_tokens);
                println!("Context");
                println!("  Messages         {}", session.messages.len());
                match window {
                    Some(window) => {
                        #[allow(clippy::cast_precision_loss)]
                        let percent = estimated as f64 / f64::from(window) * 100.0;
                        println!("  Estimated tokens ~{estimated} of {window} ({percent:.1}%)");
                    }
                    None => println!("  Estimated tokens ~{estimated} (context window unknown)"),
                }
                println!("  Compactions      {}", compaction_generation(session));
                println!(
                    "  System prompt    {} section(s)",
                    self.effective_system_prompt().len()
                );
                let added = tools::additional_workspace_roots();
                if !added.is_empty() {
                    println!("  Extra dirs       {}", added.len());
                }
                println!(
                    "Use /compact to summarise older messages, /context clear to start fresh."
                );
                Ok(false)
            }
            Some("clear") => self.clear_session(true),
            Some(other) => Err(format!("usage: /context [show|clear]  (got `{other}`)").into()),
        }
    }

    /// `/hooks`: configured lifecycle hooks.
    pub(crate) fn run_hooks() -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let config = runtime::ConfigLoader::default_for(&cwd).load()?;
        let hooks = config.hooks();
        let groups = [
            ("PreToolUse", hooks.pre_tool_use_entries()),
            ("PostToolUse", hooks.post_tool_use_entries()),
            ("PostToolUseFailure", hooks.post_tool_use_failure_entries()),
        ];
        if groups.iter().all(|(_, entries)| entries.is_empty()) {
            println!(
                "No hooks configured. Add a \"hooks\" section to .claw/settings.json or ~/.claw/settings.json."
            );
        } else {
            println!("Hooks");
            for (event, entries) in groups {
                for entry in entries {
                    println!(
                        "  {event:<19} {:<14} {}",
                        entry.matcher().unwrap_or("*"),
                        entry.command()
                    );
                }
            }
        }
        for invalid in hooks.invalid_hooks() {
            println!("  invalid: {invalid:?}");
        }
        Ok(())
    }

    /// `/tasks [list|get <id>|stop <id>]`: background shell jobs.
    pub(crate) fn run_tasks(args: Option<&str>) {
        let mut parts = args.unwrap_or_default().split_whitespace();
        match (parts.next(), parts.next()) {
            (None | Some("list"), _) => {
                let jobs = global_background_jobs().list();
                if jobs.is_empty() {
                    println!("No background jobs. The model starts them with bash(run_in_background=true).");
                    return;
                }
                println!("Background jobs");
                for job in jobs {
                    let command: String = job.command.chars().take(70).collect();
                    println!(
                        "  {:<5} {:<24} {:>6}s  pid {:<7} {command}",
                        job.id,
                        job.state.to_string(),
                        job.elapsed_secs,
                        job.pid
                    );
                }
            }
            (Some("get" | "output"), Some(id)) => {
                match global_background_jobs().read_new(id, Duration::ZERO) {
                    Ok(read) => {
                        println!("[{}: {}]", read.id, read.state);
                        let output = read.output.trim_end();
                        println!(
                            "{}",
                            if output.is_empty() {
                                "(no new output)"
                            } else {
                                output
                            }
                        );
                    }
                    Err(error) => eprintln!("{error}"),
                }
            }
            (Some("stop" | "kill"), Some(id)) => match global_background_jobs().kill(id) {
                Ok(read) => println!("[{}: {}]", read.id, read.state),
                Err(error) => eprintln!("{error}"),
            },
            _ => eprintln!("usage: /tasks [list|get <id>|stop <id>]"),
        }
    }

    /// `/files`: files the model has read or changed in this conversation.
    pub(crate) fn run_files(&self) {
        let mut seen: Vec<(String, &'static str)> = Vec::new();
        for message in &self.runtime.session().messages {
            for block in &message.blocks {
                let ContentBlock::ToolUse { name, input, .. } = block else {
                    continue;
                };
                let action = match name.as_str() {
                    "read_file" => "read",
                    "write_file" | "write_docx" | "write_pptx" | "write_xlsx" => "written",
                    "edit_file" => "edited",
                    "NotebookEdit" => "edited",
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
                    continue;
                };
                let Some(path) = ["path", "file_path", "notebook_path"]
                    .iter()
                    .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
                else {
                    continue;
                };
                match seen.iter_mut().find(|(p, _)| p == path) {
                    // A write or edit outranks a read.
                    Some(entry) if entry.1 == "read" => entry.1 = action,
                    Some(_) => {}
                    None => seen.push((path.to_string(), action)),
                }
            }
        }
        if seen.is_empty() {
            println!("No files in the conversation yet.");
            return;
        }
        println!("Files in context");
        for (path, action) in seen {
            println!("  {action:<8} {path}");
        }
    }

    // ───────────────────────────── session ─────────────────────────────

    /// `/rename <name>`: give the session a human-readable title.
    pub(crate) fn run_rename(&mut self, name: Option<&str>) -> CommandResult {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            match &self.runtime.session().title {
                Some(title) => println!("Session title: {title}"),
                None => println!("Session has no title. Usage: /rename <name>"),
            }
            return Ok(false);
        };
        self.runtime.session_mut().title = Some(name.to_string());
        println!("Session {} renamed to \"{name}\".", self.session.id);
        Ok(true)
    }

    /// `/add-dir <path>`: allow file tools to work in another directory.
    pub(crate) fn run_add_dir(path: Option<&str>) {
        let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) else {
            let roots = tools::additional_workspace_roots();
            if roots.is_empty() {
                println!("No additional directories. Usage: /add-dir <path>");
            } else {
                println!("Additional directories");
                for root in roots {
                    println!("  {}", root.display());
                }
            }
            return;
        };
        match tools::add_workspace_root(&expand_home(path)) {
            Ok(root) => println!(
                "Added {} to the workspace. File tools can now use it without escalation.",
                root.display()
            ),
            Err(error) => eprintln!("{error}"),
        }
    }

    /// `/copy [last|all]`: copy the last reply or the whole transcript.
    pub(crate) fn run_copy(&self, target: Option<&str>) {
        let session = self.runtime.session();
        let text = match target.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "last") => session
                .messages
                .iter()
                .rev()
                .find(|m| m.role == MessageRole::Assistant && !message_text(m).is_empty())
                .map(message_text)
                .unwrap_or_default(),
            Some("all") => session
                .messages
                .iter()
                .filter(|m| matches!(m.role, MessageRole::User | MessageRole::Assistant))
                .filter(|m| !message_text(m).trim().is_empty())
                .map(|m| {
                    let who = if m.role == MessageRole::User {
                        "User"
                    } else {
                        "Assistant"
                    };
                    format!("## {who}\n\n{}", message_text(m))
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            Some(other) => {
                eprintln!("usage: /copy [last|all]  (got `{other}`)");
                return;
            }
        };
        if text.trim().is_empty() {
            println!("Nothing to copy yet.");
            return;
        }
        match copy_to_clipboard(&text) {
            Ok(method) => println!("Copied {} characters ({method}).", text.chars().count()),
            Err(error) => eprintln!("copy failed: {error}"),
        }
    }

    /// `/theme [name]`: list or switch colour themes (persisted to settings).
    pub(crate) fn run_theme(name: Option<&str>) {
        if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
            match ThemeName::parse(name) {
                Ok(theme) => {
                    set_current_theme(theme);
                    match runtime::save_user_setting("theme", Some(theme.as_str())) {
                        Ok(()) => {
                            println!("Theme: {} (saved to ~/.claw/settings.json)", theme.as_str())
                        }
                        Err(error) => println!("Theme: {} (not saved: {error})", theme.as_str()),
                    }
                }
                Err(error) => {
                    eprintln!("{error}");
                    return;
                }
            }
        }
        let current = current_theme();
        for theme in ThemeName::ALL {
            let marker = if theme == current { "●" } else { " " };
            println!("  {marker} {:<16} {}", theme.as_str(), theme.description());
        }
        let preview = TerminalRenderer::new().render_markdown(
            "**Preview** — *emphasis*, `inline code`, [link](https://example.com)\n\n```rust\nfn main() { let answer = 42; println!(\"{answer}\"); }\n```",
        );
        println!("{preview}");
    }

    // ───────────────────────────── prompts ─────────────────────────────

    /// `/retry`: resend the last prompt (typically after a failed request).
    pub(crate) fn run_retry(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(prompt) = self
            .last_failed_prompt
            .clone()
            .or_else(|| self.prompt_history.last().map(|entry| entry.text.clone()))
        else {
            println!("Nothing to retry yet.");
            return Ok(());
        };
        println!("Retrying: {}", prompt.lines().next().unwrap_or_default());
        self.run_turn(&prompt)
    }

    /// `/paste`: read text from the clipboard and send it as a prompt.
    pub(crate) fn run_paste(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let text = paste_from_clipboard()?;
        if text.trim().is_empty() {
            println!("The clipboard is empty.");
            return Ok(());
        }
        println!("Pasted {} characters.", text.chars().count());
        self.record_prompt_history(&text);
        self.run_turn(&text)
    }

    /// `/image [path]`: queue an image file (or the clipboard image) to be
    /// sent with the next message.
    pub(crate) fn run_image(&mut self, path: Option<&str>) {
        let path = path.map(str::trim).filter(|p| !p.is_empty());
        let loaded = match path {
            Some(path) => {
                let resolved = expand_home(path);
                std::fs::read(&resolved)
                    .map_err(|error| format!("cannot read {}: {error}", resolved.display()))
                    .and_then(|bytes| image_block(&bytes, &resolved.display().to_string()))
            }
            None => clipboard_image().and_then(|bytes| image_block(&bytes, "clipboard")),
        };
        match loaded {
            Ok(block) => {
                self.pending_attachments.push(block);
                println!(
                    "Attached image {} ({} queued); it is sent with your next message.",
                    path.unwrap_or("from clipboard"),
                    self.pending_attachments.len()
                );
            }
            Err(error) => eprintln!("/image: {error}"),
        }
    }
}

/// Anthropic rejects images larger than this.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Media type from the file's magic bytes.
fn sniff_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

fn image_block(bytes: &[u8], source: &str) -> Result<ContentBlock, String> {
    let media_type = sniff_image_media_type(bytes)
        .ok_or_else(|| format!("{source} is not a PNG, JPEG, GIF or WebP image"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{source} is {:.1} MB; the limit is {} MB",
            bytes.len() as f64 / 1_048_576.0,
            MAX_IMAGE_BYTES / 1_048_576
        ));
    }
    Ok(ContentBlock::Image {
        media_type: media_type.to_string(),
        data: base64_encode(bytes),
        source: source.to_string(),
    })
}

/// The images among the desktop app's `[Attached file: …]` lines, to send
/// as vision input with the message; other files are left to the tools.
pub(crate) fn attached_images(text: &str) -> Vec<ContentBlock> {
    text.lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("[Attached file: ")?
                .strip_suffix(']')
        })
        .filter_map(|path| {
            let bytes = std::fs::read(expand_home(path)).ok()?;
            image_block(&bytes, path).ok()
        })
        .collect()
}

/// PNG bytes of the clipboard image (Wayland or X11).
fn clipboard_image() -> Result<Vec<u8>, String> {
    let mut readers: Vec<(&str, &[&str])> = Vec::new();
    if env::var_os("WAYLAND_DISPLAY").is_some() {
        readers.push(("wl-paste", &["--type", "image/png"]));
    }
    readers.push((
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
    ));
    let mut found_reader = false;
    for (program, args) in readers {
        if !command_exists(program) {
            continue;
        }
        found_reader = true;
        if let Ok(output) = Command::new(program)
            .args(args)
            .stderr(Stdio::null())
            .output()
        {
            if output.status.success() && sniff_image_media_type(&output.stdout).is_some() {
                return Ok(output.stdout);
            }
        }
    }
    Err(if found_reader {
        "the clipboard does not contain an image".to_string()
    } else {
        "no clipboard image reader found (install wl-clipboard or xclip)".to_string()
    })
}

/// Prompt sent for a prompt-backed slash command, or `None` if the command
/// is not one of them.
pub(crate) fn prompt_for_command(name: &str, scope: Option<&str>) -> Option<String> {
    let scope = scope.map(str::trim).filter(|s| !s.is_empty());
    let focus = scope.map_or_else(String::new, |s| format!("\n\nFocus: {s}"));
    let prompt = match name {
        "reflect" => format!(
            "Reflect on this session for auto memory before it ends. Do no other work: no code \
             changes, no commands beyond reading.\n\
             Look back over the conversation for durable knowledge that a future session in this \
             project would need and could not get from the code or git history: decisions made \
             and why, facts about the project or setup learned the hard way, mistakes made and how \
             they were fixed, and the user's preferences or corrections. Skip anything about this \
             task only.\n\
             For each item: check MEMORY.md and update an existing memory instead of adding a \
             duplicate; otherwise write a new memory file and its index line, as the memory \
             instructions describe. When it is about specific code, add `anchors:` (path#symbol) \
             so it can be flagged once that code changes. If a stale memory came up and you now \
             know the truth, fix or delete it. Save at most 5. If nothing durable came up, save \
             nothing.\n\
             End with one line listing what you saved or updated (or \"nothing to save\").{focus}"
        ),
        "scope-check" => format!(
            "Act as Scope Creeper, an adversarial second opinion, before any code is written for: \
             {subject}\n\nDo not write or change files. Read just enough of the repository to ground \
             your answer (the README and the files this would touch; CodeSearch and RepoSignals help), \
             then give exactly three sections:\n\
             1. Scan: a delusion score from 0 (clear, small, partly built already) to 10 (vague, \
             huge, or solving a non-problem), a one-line verdict (build / shrink / don't build), and \
             the 3-5 assumptions it rests on, each marked verified or unverified from what you read.\n\
             2. Kill: a one-page autopsy written as if the project already failed: the likeliest \
             causes (scope creep, an existing alternative such as a helper or tool already in this \
             repository or its ecosystem, maintenance cost, no clear user), and what abandoning it \
             costs now versus later.\n\
             3. Shippable: a v0 plan: a one-sentence goal, the user and the single job it does, what \
             is in and what is explicitly out, acceptance checks a script can run, the files it \
             likely touches with rough line counts, and a first step that can be pasted and run \
             (commands or a minimal skeleton). Keep v0 to what one focused session can ship.\n\
             Be blunt and specific; no encouragement.",
            subject = scope.unwrap_or("the task described above")
        ),
        "diverge" => format!(
            "Think divergently about: {subject}\n\nDo not settle on the first good idea, and do not \
             change files. First diagnose the kind of problem (optimization, open design, stuck \
             debugging, naming or framing, strategy) and pick the two techniques that suit it best \
             from: inversion (how to make it worse, then flip it), SCAMPER, an analogy from a distant \
             field, removing or adding a constraint, random-word injection (take an unrelated word \
             and force a link), deliberately bad ideas, reversing an assumption, a shift of \
             perspective (user, attacker, maintainer, newcomer). Say which and why in one line.\n\
             Then give at least 8 ideas that differ in kind, not degree, each labelled with its \
             technique. Audit their diversity: group them into clusters, and if there are fewer \
             than 4 clusters, add ideas in the missing directions. Map blind spots: 3 things nobody \
             close to the problem would think of. Finish with the 2 most promising unusual ideas and \
             a cheap test for each.",
            subject = scope.unwrap_or("the problem described above")
        ),
        "review" => format!(
            "Review the current uncommitted changes in this repository (use `git status` and \
             `git diff`, plus `git diff --cached`; if the tree is clean, review the latest commit). \
             Look for correctness bugs, missing edge cases, error handling gaps, and risky changes. \
             For each finding give the file and line, what goes wrong and when, and a suggested fix. \
             Rank findings by severity and skip pure style nits. Do not modify files.{focus}"
        ),
        "security-review" => format!(
            "Perform a security review of {} Look for injection (shell, SQL, path traversal), \
             unsafe deserialization, secrets in code or logs, missing authentication or \
             authorization checks, unsafe file permissions, and dependency risks. For each finding \
             give the file and line, the attack scenario, severity, and a concrete fix. Report only \
             issues you can point to in the code. Do not modify files.{focus}",
            if scope.is_some() {
                "the scope below."
            } else {
                "the pending changes (`git diff` and `git diff --cached`; if clean, the latest commit)."
            }
        ),
        "release-notes" => format!(
            "Write release notes for the changes since the most recent git tag (use `git describe \
             --tags --abbrev=0` and `git log <tag>..HEAD`; if there is no tag, use the last 30 \
             commits). Group entries under Features, Fixes, and Other, write them for users rather \
             than developers, and mention breaking changes first. Do not modify files.{focus}"
        ),
        "simplify" => format!(
            "Review the code changed in this repository (`git diff` and `git diff --cached`; if the \
             tree is clean, the latest commit) for quality: code that duplicates an existing helper, \
             logic that can be simpler, needless allocations or repeated work, and code that does \
             not match the style around it. Then apply the cleanups you are confident about, keeping \
             behavior the same, and run the relevant tests. This is not a bug hunt. Finish with a \
             short list of what you changed and what you left alone and why.{focus}"
        ),
        "pr-comments" => format!(
            "Fetch the comments on GitHub pull request {} with the `gh` CLI: the conversation \
             (`gh pr view <pr> --comments`) and the review comments on code \
             (`gh api repos/{{owner}}/{{repo}}/pulls/<number>/comments`). Summarize them grouped by \
             file, quoting each comment's author, file and line, and say which ones still look \
             unaddressed in the current code. Do not modify files or post anything.",
            scope.map_or_else(
                || "for the current branch (find it with `gh pr view`)".to_string(),
                |pr| format!("`{pr}`"),
            )
        ),
        "run" => format!(
            "Launch this project's app and drive it to see it working, not only passing tests. \
             Find how it is built and started (README, manifests, scripts, existing run configs), \
             build it, start it (long-running servers in the background), then exercise it the way \
             a user would: a CLI with real arguments, a server with real requests, a UI through \
             whatever automation is available. Report what you ran, what you observed, and anything \
             that did not work. Stop what you started when you are done.{focus}"
        ),
        "verify" => format!(
            "Verify that the recent change actually works: identify what changed (`git diff`, \
             `git diff --cached`, or the last commit), build the app, run it, and exercise the \
             changed behavior directly, including an edge case. Tests and type checks alone do not \
             count. Report what you ran and observed, and say plainly whether the change works, \
             does not, or could not be checked and why.{focus}"
        ),
        "deep-research" => format!(
            "Research this question thoroughly: {}\n\nRun several WebSearch queries from different \
             angles, read the most relevant sources with WebFetch, cross-check claims between \
             independent sources, and note where they disagree. Then write a report: a short answer \
             first, the findings with inline citations (URLs), and the open questions. Prefer \
             primary sources and recent ones.",
            scope.unwrap_or_default()
        ),
        "update-config" => format!(
            "Make this settings change for claw: {}\n\nclaw reads settings from \
             `~/.claw/settings.json` (user), `.claw.json` and `.claw/settings.json` (project, \
             shared) and `.claw/settings.local.json` (project, this machine). Pick the right file \
             for the request (personal preferences in the user file, team rules in the project \
             file, machine-specific ones in the local file), read it, and edit it keeping every \
             other key. Permission rules go under `permissions` (`allow`, `ask`, `deny`), hooks \
             under `hooks`, environment variables under `env`. Show the change you made.",
            scope.unwrap_or_default()
        ),
        "fewer-permission-prompts" => "Reduce permission prompts. Read the recent session \
             transcripts in `.claw/sessions/` (JSONL) and find the read-only bash commands and \
             tools that were run often (e.g. `git status`, `cargo test`, `ls`, `rg`). Propose a \
             prioritized list of `permissions.allow` rules for `.claw/settings.json`, only for \
             commands that cannot change or delete anything, and explain each. Ask before \
             writing them."
            .to_string(),
        "subtask" => format!(
            "Hand this side task to a background subagent so we can keep working here: {}\n\n\
             Use the Agent tool with `run_in_background: true`, giving it everything it needs \
             from this conversation (it does not see it). Tell me in one line that it started; \
             its result will come back when it finishes.",
            scope.unwrap_or_default()
        ),
        "statusline" => format!(
            "Set up a custom status line for claw{}. claw runs the command in the `statusLine.command` \
             setting (user settings: `~/.claw/settings.json`) before each prompt, with JSON about the \
             session on stdin (`model`, `cwd`, `permission_mode`, `context_used`, `context_window`), and \
             shows the first line it prints under the input box (ANSI colors allowed). Write the \
             script to `~/.claw/statusline.sh` (POSIX sh, fast: it runs on every prompt, use `jq` only \
             if installed), make it executable, test it with sample JSON, then set \
             `\"statusLine\": {{\"command\": \"~/.claw/statusline.sh\"}}` keeping the other keys. \
             Without a description, base it on the user's shell prompt (PS1 in ~/.bashrc or the \
             equivalent).",
            scope.map_or_else(String::new, |s| format!(" that shows: {s}"))
        ),
        "batch" => format!(
            "Carry out this large change in parallel: {}\n\n1. Research the codebase and split the \
             work into 5 to 30 independent units that do not touch the same files. 2. Present the \
             plan (units, files, how each is verified) and wait for my approval. 3. Once approved, \
             for each unit create a git worktree on its own branch (`git worktree add`), and start \
             one background Agent per unit (`run_in_background: true`) working only in its worktree, \
             implementing the unit, running the relevant tests and committing. 4. As agents finish, \
             report each result and list the branches to merge.",
            scope.unwrap_or_default()
        ),
        "debug" => format!(
            "Troubleshoot claw itself{}. Look at its configuration (`~/.claw/settings.json`, \
             `.claw.json`, `.claw/settings.json`, `.claw/settings.local.json`), run `claw doctor`, \
             check this session's transcript under `.claw/sessions/` for errors, and the provider \
             setup (environment variables, local server reachable). Explain the cause and the fix; \
             ask before changing settings.",
            scope.map_or_else(String::new, |s| format!(": {s}"))
        ),
        "dataviz" => format!(
            "Design a data visualization{}. Pick the chart form from the data and the question it \
             answers (comparison: bars; trend: lines; distribution: histogram; part-of-whole: \
             stacked bars, rarely pies). Assign color by role, not by series index; use one accent \
             for what matters and grays for context; keep a colorblind-safe palette with enough \
             contrast in light and dark themes. Label directly instead of legends when possible, \
             start bar axes at zero, avoid 3D and dual axes, and add tooltips and keyboard access \
             for interactive charts. Then build it with the tools the project already uses.",
            scope.map_or_else(String::new, |s| format!(": {s}"))
        ),
        "import" => format!(
            "Bring configuration from other coding agents into claw ({}). Look for: OpenAI Codex \
             (`~/.codex/AGENTS.md`, `~/.codex/config.toml` MCP servers, `~/.codex/prompts/`), Gemini \
             CLI (`GEMINI.md`, `~/.gemini/GEMINI.md`, `~/.gemini/settings.json` mcpServers, \
             `.gemini/commands/`), Cursor (`.cursorrules`, `.cursor/rules/`, `.cursor/mcp.json`, \
             `~/.cursor/mcp.json`). Map instructions to CLAUDE.md / `.claw/rules/`, MCP servers to \
             `mcpServers` in `.claw.json` (project) or `~/.claw/settings.json` (user), and commands \
             to skills in `.claw/skills/<name>/SKILL.md`. List what you found and the plan first; \
             with --dry-run stop there, otherwise ask me to confirm before writing anything.",
            scope.unwrap_or("codex, gemini and cursor")
        ),
        "insights" => "Write a report on how I use claw. Read my recent sessions (`.claw/sessions/` \
             here and in other projects you can find under my home directory, plus \
             `~/.claw/history.jsonl`): which projects, what kinds of tasks, which tools and slash \
             commands, where things went wrong (errors, interrupted or retried requests, context \
             overflows), and claw features I am not using that would help. Write it as a \
             self-contained HTML page to `~/.claw/insights/report.html` and tell me the path. Do not \
             include secrets or file contents in the report."
            .to_string(),
        "team-onboarding" => "Write an onboarding guide for a teammate joining this project, \
             from how claw has been used here: read `.claw/sessions/` (recent tasks, commands run, \
             tools and MCP servers used), the instruction files (CLAUDE.md, AGENTS.md, \
             `.claw/rules/`) and settings. Cover setup, how to build and test, the common tasks and \
             how they were done, useful slash commands and skills, and pitfalls. Write it as \
             markdown a teammate can paste as their first message to claw, to `ONBOARDING-claw.md`."
            .to_string(),
        "run-skill-generator" => "Write a project skill that teaches /run and /verify how to \
             build, launch and drive this app from a clean checkout. Find the real commands \
             (README, manifests, scripts, CI config), try them, and note what each needs \
             (services, env vars, ports, test data). Write `.claw/skills/run-app/SKILL.md` with \
             `name` and `description` frontmatter and the exact steps: build, start (in the \
             background for servers), check it is up, drive it (example commands or requests), and \
             stop it."
            .to_string(),
        "summary" => format!(
            "Summarise this conversation so far: the goal, what has been done (with file paths), \
             decisions made, and what remains open. Keep it brief and do not call any tools.{focus}"
        ),
        _ => return None,
    };
    Some(prompt)
}

fn compaction_generation(session: &runtime::Session) -> u32 {
    session.compaction.as_ref().map_or(0, |c| c.count)
}

fn message_text(message: &runtime::ConversationMessage) -> String {
    message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn command_exists(program: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Clipboard writers in preference order: (program, args, label).
fn clipboard_writers() -> Vec<(&'static str, &'static [&'static str])> {
    let mut writers: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if env::var_os("WAYLAND_DISPLAY").is_some() {
        writers.push(("wl-copy", &[]));
    }
    writers.push(("xclip", &["-selection", "clipboard"]));
    writers.push(("xsel", &["--clipboard", "--input"]));
    writers.push(("pbcopy", &[]));
    writers.push(("clip.exe", &[]));
    writers
}

/// Copies a mouse selection from the full-screen view (see
/// `screen::set_copy_handler`).
pub(crate) fn copy_selection(text: &str) -> bool {
    copy_to_clipboard(text).is_ok()
}

fn copy_to_clipboard(text: &str) -> Result<String, String> {
    for (program, args) in clipboard_writers() {
        if !command_exists(program) {
            continue;
        }
        let child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { continue };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                continue;
            }
        }
        if child.wait().is_ok_and(|status| status.success()) {
            return Ok(format!("via {program}"));
        }
    }
    // OSC 52 works over SSH and in most modern terminals.
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    Ok("via terminal OSC 52".to_string())
}

fn paste_from_clipboard() -> Result<String, String> {
    let mut readers: Vec<(&str, &[&str])> = Vec::new();
    if env::var_os("WAYLAND_DISPLAY").is_some() {
        readers.push(("wl-paste", &["--no-newline"]));
    }
    readers.push(("xclip", &["-selection", "clipboard", "-o"]));
    readers.push(("xsel", &["--clipboard", "--output"]));
    readers.push(("pbpaste", &[]));
    readers.push((
        "powershell.exe",
        &["-NoProfile", "-Command", "Get-Clipboard"],
    ));
    for (program, args) in readers {
        if !command_exists(program) {
            continue;
        }
        if let Ok(output) = Command::new(program).args(args).output() {
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
        }
    }
    Err("no clipboard reader found (install wl-clipboard, xclip, or xsel)".to_string())
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn prompt_backed_commands_have_prompts_and_scope() {
        for name in ["review", "security-review", "release-notes", "summary"] {
            let prompt = prompt_for_command(name, Some("src/auth")).expect(name);
            assert!(prompt.contains("Focus: src/auth"), "{name}");
        }
        assert!(prompt_for_command("review", None)
            .unwrap()
            .contains("git diff"));
        assert!(prompt_for_command("bogus", None).is_none());
    }

    #[test]
    fn image_block_sniffs_type_and_rejects_non_images() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let ContentBlock::Image {
            media_type, data, ..
        } = image_block(&png, "x.png").unwrap()
        else {
            panic!("expected image block");
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(data, base64_encode(&png));
        assert!(image_block(b"plain text", "x.txt").is_err());
        let big = [&png[..], &vec![0; MAX_IMAGE_BYTES][..]].concat();
        assert!(image_block(&big, "big.png").unwrap_err().contains("limit"));
    }

    #[test]
    fn attached_images_keeps_only_readable_images() {
        let dir = env::temp_dir().join(format!("claw-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        let text = dir.join("b.txt");
        std::fs::write(&text, "hello").unwrap();
        let message = format!(
            "look\n[Attached file: {}]\n[Attached file: {}]\n[Attached file: /nope.png]",
            png.display(),
            text.display()
        );
        let blocks = attached_images(&message);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], ContentBlock::Image { media_type, .. } if media_type == "image/png")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_home_resolves_tilde() {
        if let Some(home) = env::var_os("HOME") {
            assert_eq!(expand_home("~/x"), Path::new(&home).join("x"));
        }
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}
