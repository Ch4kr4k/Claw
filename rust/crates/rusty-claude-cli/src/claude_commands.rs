//! Slash commands ported from Claude Code's command list: side questions
//! (`/btw`, `/recap`), goals and loops, `/autocompact`, `/cd`, `/branch`,
//! output styles, the view (`/tui`, `/scroll-speed`) and the prompt-backed
//! commands (`/run`, `/verify`, `/subtask`, …).

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal;
use runtime::turn_keys::screen::{self, ScreenGuard};
use runtime::{ContentBlock, MessageRole};

use crate::render::TerminalRenderer;
use crate::{
    build_runtime, build_system_prompt, effective_auto_compaction_threshold, final_assistant_text,
    report_repl_turn_error,
};
use crate::{CliPermissionPrompter, LiveCli};

type CommandResult = Result<bool, Box<dyn std::error::Error>>;

/// Continuation rounds `/goal` runs before handing back to the user.
const MAX_GOAL_ROUNDS: usize = 20;
/// The first line of a reply that ends a goal.
const GOAL_MET: &str = "GOAL MET";
/// `/loop` without an interval.
const DEFAULT_LOOP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// What a turn stopped with Esc leaves as the last message.
const INTERRUPTED: &str = "[Request interrupted by user]";

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// The full-screen window, kept here so `/tui` can switch it at runtime.
static FULLSCREEN: Mutex<Option<ScreenGuard>> = Mutex::new(None);

/// Holds the full-screen window for the REPL; gives it back when dropped.
pub(crate) struct Fullscreen(());

impl Fullscreen {
    pub(crate) fn start() -> Self {
        *lock_fullscreen() = screen::enter();
        crate::diff_panel::refresh();
        Self(())
    }
}

impl Drop for Fullscreen {
    fn drop(&mut self) {
        drop(lock_fullscreen().take());
    }
}

/// Restore the same rendering mode after an external editor exits.
pub(crate) struct ExternalTerminal(bool);
impl Drop for ExternalTerminal {
    fn drop(&mut self) {
        if self.0 {
            *lock_fullscreen() = screen::enter();
            screen::set_copy_handler(crate::repl_commands::copy_selection);
            // An editor may have changed files too.
            crate::diff_panel::refresh();
        }
    }
}
pub(crate) fn external_terminal() -> ExternalTerminal {
    let active = screen::active();
    drop(lock_fullscreen().take());
    ExternalTerminal(active)
}

fn lock_fullscreen() -> std::sync::MutexGuard<'static, Option<ScreenGuard>> {
    FULLSCREEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Applies saved view and compaction settings (`/scroll-speed`,
/// `/autocompact`) at startup.
/// Announces background sessions that finished since the last prompt.
pub(crate) fn announce_finished_background() {
    for entry in crate::background::take_finished() {
        println!(
            "{DIM}Background session {} {} — /resume {} to see it{RESET}",
            entry.id, entry.status, entry.session_id
        );
    }
}

pub(crate) fn apply_saved_settings() {
    if let Some(rows) = runtime::read_user_setting("scrollSpeed").and_then(|v| v.parse().ok()) {
        runtime::turn_keys::set_wheel_rows(rows);
    }
    if let Some(tokens) =
        runtime::read_user_setting("autoCompactWindow").and_then(|v| parse_token_count(&v))
    {
        runtime::set_auto_compaction_threshold_override(Some(tokens));
    }
}

/// `500k`, `1.5m`, `120000` → tokens.
pub(crate) fn parse_token_count(value: &str) -> Option<u32> {
    let value = value.trim().to_ascii_lowercase().replace('_', "");
    let (number, scale) = match value.chars().last()? {
        'k' => (&value[..value.len() - 1], 1_000.0),
        'm' => (&value[..value.len() - 1], 1_000_000.0),
        _ => (value.as_str(), 1.0),
    };
    let tokens = number.parse::<f64>().ok()? * scale;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (tokens >= 1_000.0 && tokens <= f64::from(u32::MAX)).then(|| tokens.round() as u32)
}

/// `30s`, `5m`, `2h`, `1d` → a duration.
fn parse_interval(token: &str) -> Option<Duration> {
    let unit = token.chars().last()?;
    let count: u64 = token[..token.len() - unit.len_utf8()].parse().ok()?;
    let seconds = match unit {
        's' => count,
        'm' => count * 60,
        'h' => count * 3_600,
        'd' => count * 86_400,
        _ => return None,
    };
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

fn format_interval(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds.is_multiple_of(3_600) {
        format!("{}h", seconds / 3_600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// An output style: extra instructions for how replies are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputStyle {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) prompt: String,
}

fn builtin_styles() -> Vec<OutputStyle> {
    let style = |name: &str, description: &str, prompt: &str| OutputStyle {
        name: name.to_string(),
        description: description.to_string(),
        prompt: prompt.to_string(),
    };
    vec![
        style("default", "claw's normal replies", ""),
        style(
            "explanatory",
            "Explains implementation choices and codebase patterns as it works",
            "While you work, add short \"Insight\" notes (2-3 bullets) that explain why you chose an approach and what patterns in this codebase it follows. Keep the notes specific to the code at hand, not general programming advice.",
        ),
        style(
            "learning",
            "Pauses to let you write small pieces yourself, to learn by doing",
            "Teach while you work. For a meaningful but small piece of the change (a function body, a condition, a test case), leave a `TODO(human)` marker and ask the user to write it, explaining what it needs to do and why. Do everything else yourself, and add short insight notes about the decisions involved.",
        ),
        style(
            "concise",
            "Short, direct replies",
            "Be brief. Answer in as few sentences as the content allows, skip preambles and summaries, and show only the code that changed.",
        ),
    ]
}

/// Custom styles: markdown files with `name`/`description` frontmatter in
/// `.claw/output-styles/` or `.claude/output-styles/`, in the project or the
/// user folder (Claude Code's format).
fn custom_styles(cwd: &Path) -> Vec<OutputStyle> {
    let mut dirs = vec![
        cwd.join(".claw").join("output-styles"),
        cwd.join(".claude").join("output-styles"),
        runtime::default_config_home().join("output-styles"),
    ];
    if let Some(claude) = runtime::claude_config_home() {
        dirs.push(claude.join("output-styles"));
    }
    let mut styles = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        paths.sort();
        for path in paths {
            if let Some(style) = parse_style_file(&path) {
                if !styles
                    .iter()
                    .any(|known: &OutputStyle| known.name == style.name)
                {
                    styles.push(style);
                }
            }
        }
    }
    styles
}

fn parse_style_file(path: &Path) -> Option<OutputStyle> {
    let text = fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let (mut name, mut description, mut body) = (stem, String::new(), text.as_str());
    if let Some(rest) = text.strip_prefix("---") {
        if let Some((front, after)) = rest.split_once("\n---") {
            for line in front.lines() {
                if let Some((key, value)) = line.split_once(':') {
                    let value = value.trim().trim_matches('"').to_string();
                    match key.trim() {
                        "name" if !value.is_empty() => name = value,
                        "description" => description = value,
                        _ => {}
                    }
                }
            }
            body = after;
        }
    }
    Some(OutputStyle {
        name,
        description,
        prompt: body.trim().to_string(),
    })
}

/// Every style that can be chosen, built-in first.
fn all_styles() -> Vec<OutputStyle> {
    let cwd = env::current_dir().unwrap_or_default();
    let mut styles = builtin_styles();
    for style in custom_styles(&cwd) {
        if let Some(existing) = styles.iter_mut().find(|known| known.name == style.name) {
            *existing = style;
        } else {
            styles.push(style);
        }
    }
    styles
}

/// The style saved with `/output-style`, if it is not the default.
pub(crate) fn saved_output_style() -> Option<OutputStyle> {
    let name = runtime::read_user_setting("outputStyle")?;
    all_styles()
        .into_iter()
        .find(|style| style.name.eq_ignore_ascii_case(&name))
        .filter(|style| !style.prompt.is_empty())
}

/// The system prompt section for a style.
pub(crate) fn output_style_section(style: &OutputStyle) -> String {
    format!("# Output Style: {}\n{}", style.name, style.prompt)
}

/// The text of the last assistant message.
fn last_assistant_text(cli: &LiveCli) -> String {
    cli.runtime
        .session()
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Whether the last turn was stopped with Esc.
fn last_turn_interrupted(cli: &LiveCli) -> bool {
    cli.runtime
        .session()
        .messages
        .last()
        .is_some_and(|message| {
            message.role == MessageRole::User
                && message.blocks.iter().any(
                    |block| matches!(block, ContentBlock::Text { text } if text == INTERRUPTED),
                )
        })
}

impl LiveCli {
    /// Answers `prompt` from the conversation without tools, on a copy of
    /// the session that is not saved, and returns the answer.
    fn side_answer(&self, prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
        // A side-task model (`auxiliaryModel`) answers when one is set, so
        // the main model's quota is left alone.
        if let Some(answer) = crate::auxiliary_side_answer(
            self.runtime.session(),
            &self.effective_system_prompt(),
            prompt,
        ) {
            return Ok(answer);
        }
        let session = self.runtime.session().clone().without_persistence();
        let mut runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.effective_system_prompt(),
            false,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(prompt, Some(&mut permission_prompter));
        runtime.shutdown_plugins()?;
        Ok(final_assistant_text(&result?).trim().to_string())
    }

    fn print_side_answer(question: &str, answer: &str) -> io::Result<()> {
        let mut out = io::stdout();
        TerminalRenderer::new().stream_markdown(answer, &mut out)?;
        writeln!(
            out,
            "{DIM}(side question, not added to the conversation){RESET}"
        )?;
        let _ = question;
        writeln!(out)
    }

    /// `/btw [question]`: a side question; without one, shows the last.
    pub(crate) fn run_btw(&mut self, question: Option<&str>) -> CommandResult {
        let Some(question) = question.map(str::trim).filter(|q| !q.is_empty()) else {
            match &self.last_btw {
                Some((question, answer)) => {
                    println!("{DIM}/btw {question}{RESET}");
                    Self::print_side_answer(question, answer)?;
                }
                None => println!("usage: /btw <question>   (a quick side question; the answer is not added to the conversation)"),
            }
            return Ok(false);
        };
        println!("\n{DIM}/btw {question}{RESET}");
        io::stdout().flush()?;
        let answer = self.side_answer(&btw_prompt(question))?;
        Self::print_side_answer(question, &answer)?;
        self.last_btw = Some((question.to_string(), answer));
        Ok(false)
    }

    /// `/recap`: a one-line summary of the session so far.
    pub(crate) fn run_recap(&self) -> CommandResult {
        if self.runtime.session().messages.is_empty() {
            println!("Nothing to recap yet.");
            return Ok(false);
        }
        let answer = self.side_answer(
            "<system-reminder>Summarize this whole session in one line of at most 25 words: what the user is working on and where it stands. Reply with only that line. This exchange is not kept in the conversation.</system-reminder>",
        )?;
        println!("{DIM}Recap:{RESET} {answer}");
        Ok(false)
    }

    /// `/goal [condition|clear]`.
    pub(crate) fn run_goal(&mut self, condition: Option<&str>) -> CommandResult {
        match condition.map(str::trim).filter(|c| !c.is_empty()) {
            None => {
                match (&self.goal, &self.last_goal_met) {
                    (Some(goal), _) => println!("Goal: {goal}\n  /goal clear to drop it."),
                    (None, Some(met)) => println!("No active goal. Last goal met: {met}"),
                    (None, None) => println!(
                        "No goal set. /goal <condition> keeps claw working until it holds."
                    ),
                }
                Ok(false)
            }
            Some("clear" | "stop" | "off" | "reset" | "none" | "cancel") => {
                match self.goal.take() {
                    Some(goal) => println!("Goal cleared: {goal}"),
                    None => println!("No goal was set."),
                }
                Ok(false)
            }
            Some(condition) => {
                self.goal = Some(condition.to_string());
                println!("Goal set: {condition}\n  claw keeps working until it holds (up to {MAX_GOAL_ROUNDS} rounds; Esc pauses, /goal clear drops it).");
                self.pursue_goal();
                Ok(true)
            }
        }
    }

    /// Keeps running turns toward the active goal until the model reports
    /// it met, the user stops a turn, a turn fails, or the rounds run out.
    pub(crate) fn pursue_goal(&mut self) {
        for _ in 0..MAX_GOAL_ROUNDS {
            let Some(goal) = self.goal.clone() else {
                return;
            };
            let prompt = format!(
                "<system-reminder>Active goal, set by the user with /goal: {goal}\n\
                 If the goal is fully met now, reply with `{GOAL_MET}` on its own first line, then one line saying how you know, and do nothing else. \
                 Otherwise keep working toward it; you will be prompted again after this turn.</system-reminder>"
            );
            if self.run_turn(&prompt).is_err() {
                println!("{DIM}Goal paused: the request failed. Send a prompt or /goal to continue.{RESET}");
                return;
            }
            if last_turn_interrupted(self) {
                println!("{DIM}Goal paused. /goal clear drops it; any prompt continues it.{RESET}");
                return;
            }
            if last_assistant_text(self).trim_start().starts_with(GOAL_MET) {
                println!("{DIM}Goal met: {goal}{RESET}");
                self.last_goal_met = self.goal.take();
                return;
            }
        }
        println!("{DIM}Goal still open after {MAX_GOAL_ROUNDS} rounds; send a prompt to keep going or /goal clear.{RESET}");
    }

    /// `/loop [interval] <prompt>`: runs the prompt, waits, runs it again,
    /// until a key is pressed.
    pub(crate) fn run_loop(&mut self, args: &str) -> CommandResult {
        let args = args.trim();
        if args == "stop" {
            let scheduler = runtime::scheduler::global_scheduler();
            if scheduler.cancel_wakeup() {
                println!("Self-paced loop stopped: its next run was cancelled.");
            } else {
                println!("No self-paced loop is waiting to run.");
            }
            return Ok(false);
        }
        let (interval, prompt) = match args.split_once(char::is_whitespace) {
            Some((first, rest)) if parse_interval(first).is_some() => (
                parse_interval(first).unwrap_or(DEFAULT_LOOP_INTERVAL),
                rest.trim(),
            ),
            _ => (Duration::ZERO, args),
        };
        if prompt.is_empty() {
            println!(
                "usage: /loop [interval] <prompt>   e.g. /loop 5m check if the deploy finished\n       /loop <prompt>   the agent picks when to run again (self-paced)"
            );
            return Ok(false);
        }
        if interval.is_zero() {
            // Self-paced, like Claude Code: each iteration ends with
            // ScheduleWakeup, and the wake-up runs the next one while the
            // prompt is idle (you can type in between).
            println!(
                "Self-paced loop: {prompt}\n  {DIM}The agent schedules each next run; it ends when it calls ScheduleWakeup with stop, or /loop stop.{RESET}"
            );
            report_repl_turn_error(self.run_turn(&self_paced_loop_prompt(prompt)));
            self.persist_session()?;
            return Ok(true);
        }
        println!(
            "Looping every {}: {prompt}\n  {DIM}Press any key while it waits to stop.{RESET}",
            format_interval(interval)
        );
        let mut runs = 0;
        loop {
            runs += 1;
            report_repl_turn_error(self.run_turn(prompt));
            self.persist_session()?;
            if last_turn_interrupted(self) {
                break;
            }
            println!(
                "{DIM}  next run in {} (run {runs} done) — press any key to stop{RESET}",
                format_interval(interval)
            );
            if !wait_unless_key(interval)? {
                break;
            }
        }
        println!("{DIM}Loop stopped after {runs} run(s).{RESET}");
        Ok(true)
    }

    /// `/autocompact [auto|<tokens>]`.
    pub(crate) fn run_autocompact(&mut self, value: Option<&str>) -> CommandResult {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            None => println!(
                "Auto-compact window: {} tokens (the context size at which the session is compacted).\n  /autocompact <tokens> to change it (e.g. 150k), /autocompact auto for the default.",
                effective_auto_compaction_threshold(&self.model)
            ),
            Some("auto" | "default") => {
                runtime::set_auto_compaction_threshold_override(None);
                runtime::save_user_setting("autoCompactWindow", None)?;
                println!(
                    "Auto-compact window: {} tokens (default).",
                    effective_auto_compaction_threshold(&self.model)
                );
            }
            Some(value) => {
                let Some(tokens) = parse_token_count(value) else {
                    println!("usage: /autocompact [auto|<tokens>]   e.g. 150k, 1m, 90000 (at least 1k)");
                    return Ok(false);
                };
                runtime::set_auto_compaction_threshold_override(Some(tokens));
                runtime::save_user_setting("autoCompactWindow", Some(&tokens.to_string()))?;
                println!("Auto-compact window: {tokens} tokens (saved; applies from the next message).");
            }
        }
        Ok(false)
    }

    /// `/cd <path>`: moves the session; instructions and memory are
    /// reloaded for the new directory.
    pub(crate) fn run_cd(&mut self, path: &str) -> CommandResult {
        let target = runtime::expand_home(path.trim());
        let target = if target.is_absolute() {
            target
        } else {
            env::current_dir()?.join(target)
        };
        let target = match target.canonicalize() {
            Ok(target) if target.is_dir() => target,
            _ => {
                println!("not a directory: {}", target.display());
                return Ok(false);
            }
        };
        env::set_current_dir(&target)?;
        self.system_prompt = build_system_prompt(&self.model)?;
        self.snapshots = None;
        self.rebuild_runtime()?;
        println!(
            "Working directory: {}\n  {DIM}instructions and memory reloaded for it; the conversation continues{RESET}",
            target.display()
        );
        Ok(true)
    }

    /// `/branch [name]`: forks the conversation and switches to the fork.
    pub(crate) fn run_branch(&mut self, name: Option<&str>) -> CommandResult {
        let parent = self.session.id.clone();
        let saved = self.handle_session_command(Some("fork"), name)?;
        println!("  {DIM}Return to the original with /resume {parent}{RESET}");
        Ok(saved)
    }

    /// `/output-style [style]`.
    pub(crate) fn run_output_style(&mut self, style: Option<&str>) -> CommandResult {
        let styles = all_styles();
        let current = self
            .output_style
            .as_ref()
            .map_or("default", |style| style.name.as_str())
            .to_string();
        let Some(wanted) = style.map(str::trim).filter(|s| !s.is_empty()) else {
            println!("Output styles (current: {current})");
            for style in &styles {
                let mark = if style.name == current { "●" } else { " " };
                println!("  {mark} {:<12} {}", style.name, style.description);
            }
            println!("  {DIM}Add your own as .claude/output-styles/<name>.md or .claw/output-styles/<name>.md{RESET}");
            return Ok(false);
        };
        let Some(style) = styles
            .into_iter()
            .find(|style| style.name.eq_ignore_ascii_case(wanted))
        else {
            println!("unknown output style `{wanted}`; /output-style lists them");
            return Ok(false);
        };
        let name = style.name.clone();
        self.output_style = (!style.prompt.is_empty()).then_some(style);
        runtime::save_user_setting("outputStyle", (name != "default").then_some(name.as_str()))?;
        println!("Output style: {name} (saved; applies from the next message)");
        Ok(false)
    }

    /// `/tui [fullscreen|default]`: switches the view now and saves it.
    pub(crate) fn run_tui(mode: Option<&str>) -> CommandResult {
        let active = screen::active();
        let current = if active { "fullscreen" } else { "default" };
        match mode.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => println!(
                "View: {current}\n  /tui fullscreen: claw draws the window, the input stays at the bottom\n  /tui default: inline, output stays in the terminal's history"
            ),
            Some("fullscreen" | "full") => {
                runtime::save_user_setting("tui", None)?;
                if !active {
                    *lock_fullscreen() = screen::enter();
                    screen::set_copy_handler(crate::repl_commands::copy_selection);
                    crate::diff_panel::refresh();
                }
                println!("View: fullscreen (saved)");
            }
            Some("default" | "inline") => {
                runtime::save_user_setting("tui", Some("default"))?;
                drop(lock_fullscreen().take());
                println!("View: default (saved)");
            }
            Some(other) => println!("usage: /tui [fullscreen|default]  (got `{other}`)"),
        }
        Ok(false)
    }

    /// `/scroll-speed [rows]`.
    pub(crate) fn run_scroll_speed(rows: Option<&str>) -> CommandResult {
        match rows.map(str::trim).filter(|r| !r.is_empty()) {
            None => println!(
                "Mouse wheel: {} rows a step. /scroll-speed <1-20> to change it.",
                runtime::turn_keys::wheel_rows()
            ),
            Some(value) => match value.parse::<usize>() {
                Ok(rows) if (1..=20).contains(&rows) => {
                    runtime::turn_keys::set_wheel_rows(rows);
                    runtime::save_user_setting("scrollSpeed", Some(&rows.to_string()))?;
                    println!("Mouse wheel: {rows} rows a step (saved)");
                }
                _ => println!("usage: /scroll-speed <1-20>"),
            },
        }
        Ok(false)
    }

    /// `/color [name|default]`: the input box color; no name picks one.
    pub(crate) fn run_color(color: Option<&str>) -> CommandResult {
        use runtime::turn_keys::{prompt_color_name, set_prompt_color, PROMPT_COLORS};
        let name = match color.map(str::trim).filter(|c| !c.is_empty()) {
            Some(name) => name.to_string(),
            None => {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |time| time.subsec_nanos() as usize);
                let current = prompt_color_name();
                let others: Vec<&str> = PROMPT_COLORS
                    .iter()
                    .map(|(name, _)| *name)
                    .filter(|name| *name != current)
                    .collect();
                others[nanos % others.len()].to_string()
            }
        };
        if set_prompt_color(&name) {
            println!("Session color set to: {}", prompt_color_name());
        } else {
            let names: Vec<&str> = PROMPT_COLORS.iter().map(|(name, _)| *name).collect();
            println!(
                "unknown color `{name}`; choose {} or default",
                names.join(", ")
            );
        }
        Ok(false)
    }

    /// `/advisor [model|off]`.
    pub(crate) fn run_advisor(&mut self, model: Option<&str>) -> CommandResult {
        match model.map(str::trim).filter(|m| !m.is_empty()) {
            None => match &self.advisor {
                Some(model) => println!("Advisor: {model}. /advisor off turns it off."),
                None => println!("No advisor. /advisor <model> lets the model consult a second model at key moments."),
            },
            Some("off" | "none") => {
                self.advisor = None;
                println!("Advisor off.");
            }
            Some(model) => {
                self.advisor = Some(model.to_string());
                println!("Advisor: {model} (consulted through a sub-agent at key moments; applies from the next message)");
            }
        }
        Ok(false)
    }

    /// `/fork [prompt]`.
    pub(crate) fn run_fork(&mut self, prompt: Option<&str>) -> CommandResult {
        if let Some(prompt) = prompt.map(str::trim).filter(|p| !p.is_empty()) {
            return self.run_prompted("subtask", Some(prompt));
        }
        let forked = self.runtime.fork_session(None);
        let handle = crate::create_managed_session_handle(&forked.session_id)?;
        let forked = forked.with_persistence_path(handle.path.clone());
        forked.save_to_path(&handle.path)?;
        println!(
            "Forked this conversation into session {}; you are still here.\n  {DIM}/resume {} switches to the copy.{RESET}",
            handle.id, handle.id
        );
        Ok(false)
    }

    /// `/bug [report]` and `/feedback [report]`: a local bundle (claw has
    /// nowhere to send it) with the report, the setup and the session.
    pub(crate) fn run_report(&mut self, kind: &str, text: Option<&str>) -> CommandResult {
        self.persist_session()?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_secs());
        let dir = runtime::default_config_home()
            .join("feedback-bundles")
            .join(format!("{kind}-{stamp}"));
        fs::create_dir_all(&dir)?;
        let report = format!(
            "# claw {kind} report\n\n{}\n\n## Setup\n- claw {}\n- model: {}\n- permission mode: {}\n- OS: {} {}\n- terminal: {}\n- working directory: {}\n- session: {} ({} messages)\n",
            text.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("(no description given)"),
            env!("CARGO_PKG_VERSION"),
            self.model,
            self.permission_mode.as_str(),
            env::consts::OS,
            env::consts::ARCH,
            env::var("TERM").unwrap_or_default(),
            env::current_dir().unwrap_or_default().display(),
            self.session.id,
            self.runtime.session().messages.len(),
        );
        fs::write(dir.join("report.md"), report)?;
        if self.session.path.is_file() {
            fs::copy(&self.session.path, dir.join("session.jsonl"))?;
        }
        println!(
            "Saved the {kind} report to {}\n  {DIM}It includes this conversation (session.jsonl); read it before sending it to anyone.{RESET}",
            dir.display()
        );
        Ok(false)
    }

    /// `/reload-skills`.
    pub(crate) fn run_reload_skills() -> CommandResult {
        let cwd = env::current_dir()?;
        let skills = commands::skill_inventory(&cwd)?;
        println!(
            "{} skills available (claw reads skill folders each time, so new and changed skills are already picked up).",
            skills.len()
        );
        for (name, description, _) in skills.iter().take(30) {
            println!("  {name:<24} {}", description.lines().next().unwrap_or(""));
        }
        Ok(false)
    }

    /// `/reload-plugins`: rebuilds the runtime, which reloads plugins and
    /// restarts MCP servers.
    pub(crate) fn run_reload_plugins(&mut self) -> CommandResult {
        self.system_prompt = build_system_prompt(&self.model)?;
        self.rebuild_runtime()?;
        println!("Plugins, MCP servers and instructions reloaded.");
        Ok(false)
    }

    /// `/skill-doctor`: what each skill costs when used and how often it was.
    pub(crate) fn run_skill_doctor() -> CommandResult {
        let cwd = env::current_dir()?;
        let skills = commands::skill_inventory(&cwd)?;
        if skills.is_empty() {
            println!("No skills found.");
            return Ok(false);
        }
        let uses = skill_use_counts(&cwd);
        let mut rows: Vec<(String, usize, usize)> = skills
            .into_iter()
            .map(|(name, _, path)| {
                let chars = path
                    .and_then(|path| fs::read_to_string(path).ok())
                    .map_or(0, |text| text.chars().count());
                let used = uses.get(&name).copied().unwrap_or(0);
                (name, chars / 4, used)
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1));
        println!("Skills (tokens are an estimate of what loading one costs; uses are from this project's sessions)");
        println!("  {:<28} {:>8} {:>6}", "skill", "tokens", "uses");
        for (name, tokens, used) in &rows {
            println!("  {name:<28} {tokens:>8} {used:>6}");
        }
        let unused = rows.iter().filter(|(_, _, used)| *used == 0).count();
        if unused > 0 {
            println!(
                "  {DIM}{unused} never used here; remove or move the ones you do not need.{RESET}"
            );
        }
        Ok(false)
    }

    /// `/terminal-setup`.
    pub(crate) fn run_terminal_setup() -> CommandResult {
        println!("{}", terminal_report());
        Ok(false)
    }

    /// `/powerup`.
    pub(crate) fn run_powerup() -> CommandResult {
        println!("{POWERUP}");
        Ok(false)
    }

    /// `/keybindings`.
    pub(crate) fn run_keybindings() -> CommandResult {
        println!("{}", crate::keybindings::report()?);
        Ok(false)
    }

    /// `/focus [on|off]`: the compact view; saved like Claude Code's
    /// `viewMode` setting.
    /// `/diffpanel [on|off]`: the uncommitted changes, right of the
    /// conversation. Saved, like `/focus`.
    pub(crate) fn run_diff_panel(mode: Option<&str>) -> CommandResult {
        if !screen::active() {
            println!("The diff panel needs the full-screen view: /tui fullscreen");
            return Ok(false);
        }
        let shown = crate::diff_panel::enabled() && !screen::side_panel_closed();
        let on = match mode.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => !shown,
            Some("on") => true,
            Some("off") => false,
            Some(other) => {
                println!("usage: /diffpanel [on|off]  (got `{other}`)");
                return Ok(false);
            }
        };
        runtime::save_user_setting("diffPanel", (!on).then_some("off"))?;
        screen::set_side_panel_closed(!on);
        if on {
            crate::diff_panel::refresh();
            println!(
                "Diff panel on (saved): uncommitted changes, shown when the window is at least {} columns wide and something changed.",
                screen::SIDE_PANEL_MIN_COLS
            );
        } else {
            screen::set_side_panel("diff", None);
            println!("Diff panel off (saved).");
        }
        Ok(false)
    }

    pub(crate) fn run_focus(mode: Option<&str>) -> CommandResult {
        if !screen::active() {
            println!("The focus view needs the full-screen view: /tui fullscreen");
            return Ok(false);
        }
        let on = match mode.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => !screen::focus(),
            Some("on") => true,
            Some("off") => false,
            Some(other) => {
                println!("usage: /focus [on|off]  (got `{other}`)");
                return Ok(false);
            }
        };
        screen::set_focus(on);
        runtime::save_user_setting("viewMode", on.then_some("focus"))?;
        println!(
            "Focus view {} (saved).",
            if on {
                "on: last prompt, tool calls as one line each, final reply"
            } else {
                "off"
            }
        );
        Ok(false)
    }

    /// `/background [prompt]`: a background claw process continues this
    /// conversation; this terminal starts a fresh one.
    pub(crate) fn run_background(&mut self, prompt: Option<&str>) -> CommandResult {
        if self.runtime.session().messages.is_empty() && prompt.is_none() {
            println!(
                "Nothing to move yet: /background <prompt> starts a background session on a task."
            );
            return Ok(false);
        }
        self.persist_session()?;
        let prompt = prompt
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or(crate::background::CONTINUE_PROMPT);
        let entry = crate::background::launch(&crate::background::Launch {
            session_id: &self.session.id,
            session_path: &self.session.path,
            prompt,
            model: &self.model,
            permission_mode: self.permission_mode.as_str(),
            allowed_tools: self
                .allowed_tools
                .as_ref()
                .map(|tools| tools.iter().cloned().collect::<Vec<_>>().join(",")),
        })?;
        let moved = self.session.id.clone();
        self.start_fresh_session()?;
        println!(
            "Moved to the background as {} (session {moved}).\n  {DIM}It cannot ask for permission, so actions that would ask are denied. /list-agents shows it, /stop {} ends it, /resume {moved} opens it. Log: {}{RESET}",
            entry.id,
            entry.id,
            entry.log.display()
        );
        Ok(true)
    }

    /// `/list-agents`: background sessions, then shells and jobs.
    pub(crate) fn run_list_agents() -> CommandResult {
        let sessions = crate::background::entries();
        if sessions.is_empty() {
            println!("No background sessions. /background moves a conversation to one.");
        } else {
            println!("Background sessions");
            for line in crate::background::render(&sessions) {
                println!("{line}");
            }
        }
        Self::run_tasks(None);
        Ok(false)
    }

    /// `/stop [id]`: a background session, or a background job.
    pub(crate) fn run_stop(id: Option<&str>) -> CommandResult {
        let running: Vec<_> = crate::background::entries()
            .into_iter()
            .filter(|entry| entry.status == "running")
            .collect();
        let id = match id.map(str::trim).filter(|i| !i.is_empty()) {
            Some(id) => id.to_string(),
            None if running.len() == 1 => running[0].id.clone(),
            None => {
                println!("usage: /stop <id>   (/list-agents shows the ids)");
                return Ok(false);
            }
        };
        match crate::background::stop(&id) {
            Ok(entry) => println!(
                "Stopped {} (session {}; /resume {} opens it).",
                entry.id, entry.session_id, entry.session_id
            ),
            Err(session_error) => match runtime::global_background_jobs().kill(&id) {
                Ok(read) => println!("[{}: {}]", read.id, read.state),
                Err(_) => println!("{session_error}"),
            },
        }
        Ok(false)
    }

    /// The prompt-backed commands: send their prompt as a turn.
    pub(crate) fn run_prompted(&mut self, name: &str, args: Option<&str>) -> CommandResult {
        let Some(prompt) = crate::repl_commands::prompt_for_command(name, args) else {
            return Ok(false);
        };
        report_repl_turn_error(self.run_turn(&prompt));
        Ok(true)
    }
}

/// One iteration of a self-paced `/loop`: the task and how to pace it.
pub(crate) fn self_paced_loop_prompt(prompt: &str) -> String {
    format!(
        "{prompt}\n\n<system-reminder>\nThis is an iteration of a self-paced /loop. When this \
         iteration's work is done, call ScheduleWakeup with delaySeconds (60-3600, matched to \
         what you are waiting for), prompt set to exactly the loop's task above (without this \
         reminder), and a one-sentence reason. When the task is complete or nothing is left to \
         watch, call ScheduleWakeup with stop: true instead.\n</system-reminder>"
    )
}

/// The section `/advisor` adds to the system prompt.
pub(crate) fn advisor_section(model: &str) -> String {
    format!(
        "# Advisor\nThe user set `{model}` as your advisor. At key moments (before committing to an \
         approach on a non-trivial task, when stuck after a couple of failed attempts, and before \
         declaring a multi-step task done) consult it with the Agent tool, `model: \"{model}\"`, \
         giving it the goal, what you know and the decision you face, and asking for a short \
         recommendation without editing anything. Weigh its advice; you make the call. Do not \
         consult it for routine steps."
    )
}

/// What `/terminal-setup` says about the terminal claw runs in.
fn terminal_report() -> String {
    let var = |name: &str| env::var(name).unwrap_or_default();
    let term = var("TERM");
    let program = var("TERM_PROGRAM");
    let (name, native) = if !var("KITTY_WINDOW_ID").is_empty() || term == "xterm-kitty" {
        ("kitty", true)
    } else if !var("WEZTERM_PANE").is_empty() || program == "WezTerm" {
        ("WezTerm", true)
    } else if program == "ghostty" || term == "xterm-ghostty" {
        ("Ghostty", true)
    } else if term.starts_with("foot") {
        ("foot", true)
    } else if !var("ALACRITTY_WINDOW_ID").is_empty() || !var("ALACRITTY_SOCKET").is_empty() {
        ("Alacritty", true)
    } else if program == "vscode" {
        ("VS Code / Cursor", false)
    } else if !var("ZED_TERM").is_empty() || program == "zed" {
        ("Zed", false)
    } else if program == "iTerm.app" {
        ("iTerm2", false)
    } else if program == "Apple_Terminal" {
        ("Apple Terminal", false)
    } else if !var("KONSOLE_VERSION").is_empty() {
        ("Konsole", false)
    } else if !var("VTE_VERSION").is_empty() {
        ("a VTE terminal (GNOME Terminal, Tilix, …)", false)
    } else {
        ("an unrecognized terminal", false)
    };
    let mut lines = vec![format!("Terminal: {name}")];
    if native {
        lines.push(
            "  Shift+Enter inserts a newline already (kitty keyboard protocol); nothing to set up."
                .to_string(),
        );
    } else {
        lines.push("  Shift+Enter may arrive as plain Enter here. Alt+Enter, Ctrl+J and a trailing `\\` always insert a newline.".to_string());
        match name {
            "VS Code / Cursor" => lines.push(
                "  To make Shift+Enter work, add to keybindings.json (Ctrl+Shift+P → \"Open Keyboard Shortcuts (JSON)\"):\n    { \"key\": \"shift+enter\", \"command\": \"workbench.action.terminal.sendSequence\",\n      \"args\": { \"text\": \"\\u001b\\r\" }, \"when\": \"terminalFocus\" }".to_string(),
            ),
            "Zed" => lines.push(
                "  To make Shift+Enter work, add to Zed's keymap.json:\n    { \"context\": \"Terminal\", \"bindings\": { \"shift-enter\": [\"terminal::SendText\", \"\\u001b\\r\"] } }".to_string(),
            ),
            "iTerm2" => lines.push(
                "  To make Shift+Enter work: Settings → Profiles → Keys → Key Mappings → + → Shift+Enter, \"Send Escape Sequence\", `\\r`. Turn on Settings → General → Selection → \"Applications in terminal may access clipboard\" for copying.".to_string(),
            ),
            "Apple Terminal" => lines.push(
                "  Settings → Profiles → Keyboard → \"Use Option as Meta key\" makes Option+Enter insert a newline.".to_string(),
            ),
            _ => {}
        }
    }
    lines.join("\n")
}

/// How long a `statusLine.command` may take before it is skipped.
const STATUS_LINE_TIMEOUT: Duration = Duration::from_millis(800);

/// Runs the `statusLine.command` setting, if there is one, with session
/// JSON on stdin (like Claude Code's status line), and returns the first
/// line it prints.
pub(crate) fn custom_status_line(
    model: &str,
    permission_mode: &str,
    used: usize,
    window: Option<usize>,
) -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let config = runtime::ConfigLoader::default_for(&cwd).load().ok()?;
    let command = config
        .get("statusLine")?
        .as_object()?
        .get("command")?
        .as_str()?
        .trim()
        .to_string();
    if command.is_empty() {
        return None;
    }
    let input = serde_json::json!({
        "model": model,
        "cwd": cwd.display().to_string(),
        "permission_mode": permission_mode,
        "context_used": used,
        "context_window": window,
    })
    .to_string();
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(runtime::expand_home(&command))
        .current_dir(&cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let deadline = Instant::now() + STATUS_LINE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut output).ok()?;
    let line = output.lines().next()?.trim_end().to_string();
    (!line.is_empty()).then_some(line)
}

/// Counts `Skill` tool uses per skill name in this project's sessions.
fn skill_use_counts(cwd: &Path) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    let mut stack = vec![cwd.join(".claw").join("sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for piece in text.split("\\\"skill\\\":\\\"").skip(1) {
                    if let Some(name) = piece.split('\\').next() {
                        *counts
                            .entry(name.trim_start_matches('/').to_string())
                            .or_insert(0) += 1;
                    }
                }
            }
        }
    }
    counts
}

const POWERUP: &str = "A quick tour of claw

Input
  Shift/Alt+Enter or Ctrl+J   newline; paste keeps newlines
  Up/Down, Ctrl+R            history; reverse search
  !cmd                       run a shell command, its output becomes context
  # note                     save a note to your AGENTS.md
  @path                      attach a file to the prompt
  Shift+Tab                  cycle permission modes (also during a reply)

While it works
  Esc                        stop the reply; type ahead and press Enter to queue a message
  /btw <question>            side question that does not enter the conversation
  /goal <condition>          keep going until the condition holds

Full-screen view
  wheel, PgUp/PgDn           scroll; Ctrl+End jumps back
  drag                       select and copy; Shift+drag for the terminal's own selection
  /color, /scroll-speed, /tui

Memory and instructions
  CLAUDE.md, AGENTS.md, .claude/rules/, @imports, and ~/.claude/CLAUDE.md are loaded
  claw keeps its own memories across sessions; /memory lists them

Sessions
  /undo, /rewind             roll back files and conversation
  /branch, /fork, /resume    try another direction, come back
  /compact, /autocompact     keep the context small

More: /help, MANUAL.md";

/// Waits `interval`; returns false as soon as a key is pressed.
fn wait_unless_key(interval: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + interval;
    terminal::enable_raw_mode()?;
    let result = (|| {
        while Instant::now() < deadline {
            let step = (deadline - Instant::now()).min(Duration::from_millis(200));
            if event::poll(step)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => return Ok(false),
                    Event::Mouse(mouse) => {
                        if let crossterm::event::MouseEventKind::ScrollUp = mouse.kind {
                            screen::scroll(runtime::turn_keys::Scroll::Up(
                                runtime::turn_keys::wheel_rows(),
                            ));
                        } else if let crossterm::event::MouseEventKind::ScrollDown = mouse.kind {
                            screen::scroll(runtime::turn_keys::Scroll::Down(
                                runtime::turn_keys::wheel_rows(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(true)
    })();
    terminal::disable_raw_mode()?;
    result
}

/// The message for `/btw`: a quick answer from what is already known.
pub(crate) fn btw_prompt(question: &str) -> String {
    format!(
        "<system-reminder>The user is asking a quick side question (/btw). Answer it briefly from what this conversation already contains. You have no tools for this answer. If the conversation does not contain the answer, say so in one sentence. This exchange is not kept in the conversation.</system-reminder>\n\n{question}"
    )
}

#[cfg(test)]
mod tests {
    use super::{format_interval, parse_interval, parse_style_file, parse_token_count};
    use std::time::Duration;

    #[test]
    fn skill_uses_are_counted_from_transcripts() {
        let root = std::env::temp_dir().join(format!("claw-skill-uses-{}", std::process::id()));
        let sessions = root.join(".claw").join("sessions").join("abc");
        std::fs::create_dir_all(&sessions).expect("dir");
        std::fs::write(
            sessions.join("s.jsonl"),
            r#"{"input":"{\"skill\":\"run-app\"}"}
{"input":"{\"skill\":\"/run-app\",\"args\":\"x\"}"}
{"input":"{\"skill\":\"other\"}"}
"#,
        )
        .expect("write");
        let counts = super::skill_use_counts(&root);
        std::fs::remove_dir_all(root).ok();
        assert_eq!(counts.get("run-app"), Some(&2));
        assert_eq!(counts.get("other"), Some(&1));
    }

    #[test]
    fn token_counts_take_suffixes() {
        assert_eq!(parse_token_count("150k"), Some(150_000));
        assert_eq!(parse_token_count("1.5m"), Some(1_500_000));
        assert_eq!(parse_token_count("90000"), Some(90_000));
        assert_eq!(parse_token_count("12"), None, "too small to be meant");
        assert_eq!(parse_token_count("lots"), None);
    }

    #[test]
    fn intervals_parse_and_print() {
        assert_eq!(parse_interval("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_interval("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_interval("2h"), Some(Duration::from_secs(7_200)));
        assert_eq!(parse_interval("0m"), None);
        assert_eq!(parse_interval("check"), None);
        assert_eq!(format_interval(Duration::from_secs(300)), "5m");
        assert_eq!(format_interval(Duration::from_secs(7_200)), "2h");
        assert_eq!(format_interval(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn style_files_use_claude_code_frontmatter() {
        let dir = std::env::temp_dir().join(format!("claw-style-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("pirate.md");
        std::fs::write(
            &path,
            "---\nname: Pirate\ndescription: Talks like a pirate\n---\n\nAnswer like a pirate.\n",
        )
        .expect("write");
        let style = parse_style_file(&path).expect("style");
        std::fs::remove_dir_all(dir).ok();
        assert_eq!(style.name, "Pirate");
        assert_eq!(style.description, "Talks like a pirate");
        assert_eq!(style.prompt, "Answer like a pirate.");
    }
}
