//! `claw desktop-agent`: runs one claw session for the desktop app
//! (`claw-desktop`) over JSON lines instead of a terminal.
//!
//! The app starts one agent per open chat, in that chat's working directory,
//! optionally with `--resume <session.jsonl>`, or with `--incognito` to keep
//! nothing on disk. Commands arrive on stdin, one
//! JSON object per line:
//!
//! - `{"cmd":"prompt","text":"..."}` runs a turn
//! - `{"cmd":"cancel"}` stops the running turn (like Esc in the REPL)
//! - `{"cmd":"permission","id":N,"decision":"allow"|"always"|"deny"}`
//! - `{"cmd":"answer","id":N,"text":"..."}` answers an `AskUserQuestion`
//! - `{"cmd":"model","model":"..."}` switches the model for later turns
//!
//! Events go to stdout, one JSON object per line with an `ev` field:
//! `ready`, `turn_start`, `text`, `thinking`, `tool_call`, `tool_result`,
//! `permission`, `question`, `turn_end`, `context`, `model`, `error`. Other
//! stdout lines (claw's own notices) are not JSON events; the app ignores
//! them. Everything else (settings, sandbox, permission rules, sessions,
//! `/model` aliases) is the same as in the terminal REPL.

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::{
    final_assistant_text, known_context_window, permission_request_approval_key,
    resolve_model_alias_with_config, resolve_repl_model, tool_result_for_model, AllowedToolSet,
    LiveCli, PermissionMode, INTERRUPTED_BY_USER, SESSION_APPROVALS,
};

/// Set while this process runs as a desktop agent; the stream and tool hooks
/// below do nothing otherwise.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether the current turn has sent any reply text.
static TEXT_SENT: AtomicBool = AtomicBool::new(false);
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
/// An incognito chat: nothing is saved to disk.
static INCOGNITO: AtomicBool = AtomicBool::new(false);
/// Serializes event lines so concurrent writers never interleave.
static OUT: Mutex<()> = Mutex::new(());
/// Stops the running turn; set while a turn runs.
static CURRENT_ABORT: Mutex<Option<runtime::HookAbortSignal>> = Mutex::new(None);
/// Answers to permission requests and questions, read by the turn thread.
static REPLIES: Mutex<Option<mpsc::Receiver<Reply>>> = Mutex::new(None);

/// What the model should know about the app it is talking through.
const DESKTOP_PROMPT: &str = "# Desktop app\nThe user is chatting through claw's desktop app, which renders your replies as Markdown with syntax-highlighted code blocks (always tag them with a language). It shows images: to show a picture, put it in the reply as Markdown, `![description](https://...)` for a web image or `![description](/absolute/path.png)` for a local file. To find pictures on the web, use WebSearch, then WebFetch on the result pages to get direct image URLs (ending in .png, .jpg, .gif, .webp or .svg) and show them that way. When the user wants a file saved (an image you showed, a document you found), use the Download tool; it saves into ~/Downloads unless told otherwise. Images the user attaches are sent to you with their message, so you can see them. Commands run without a terminal: never use interactive flags such as `emerge --ask`; the app asks the user before risky commands.";

/// Longest tool output sent to the app for display.
const MAX_TOOL_OUTPUT_CHARS: usize = 20_000;

enum Command {
    Prompt(String),
    Model(String),
    /// Edit-and-resend: drop the conversation from the n-th typed prompt
    /// (0-based) on; the edited prompt follows as a new `prompt`.
    Rewind(usize),
}

enum Reply {
    Permission { id: u64, decision: String },
    Answer { id: u64, text: String },
    Cancel,
}

pub(crate) fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

fn emit(event: &Value) {
    let line = event.to_string();
    let _guard = OUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// A piece of the reply as it streams.
pub(crate) fn emit_text(text: &str) {
    if is_active() && !text.is_empty() {
        TEXT_SENT.store(true, Ordering::Relaxed);
        emit(&json!({"ev": "text", "text": text}));
    }
}

pub(crate) fn emit_thinking(text: &str) {
    if is_active() && !text.is_empty() {
        emit(&json!({"ev": "thinking", "text": text}));
    }
}

/// A tool call the model made, once its input is complete.
pub(crate) fn emit_tool_call(id: &str, name: &str, input: &str) {
    if is_active() {
        emit(&json!({"ev": "tool_call", "id": id, "name": name, "input": input}));
    }
}

/// A finished tool call, as the user would read it (bash output as plain
/// text, not the JSON envelope). Results arrive in the order of the calls.
pub(crate) fn emit_tool_result(name: &str, output: &str, is_error: bool) {
    if !is_active() {
        return;
    }
    let text = if is_error {
        output.to_string()
    } else {
        tool_result_for_model(name, output.to_string())
    };
    let text = if text.chars().count() > MAX_TOOL_OUTPUT_CHARS {
        let cut: String = text.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
        format!("{cut}\n… (output cut for display)")
    } else {
        text
    };
    emit(&json!({"ev": "tool_result", "name": name, "output": text, "is_error": is_error}));
}

/// Waits for the reply to request `id`; `None` when the turn was cancelled
/// or the app went away.
fn wait_reply(id: u64) -> Option<Reply> {
    let guard = REPLIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let receiver = guard.as_ref()?;
    loop {
        match receiver.recv() {
            Ok(Reply::Cancel) | Err(_) => return None,
            Ok(reply @ Reply::Permission { id: reply_id, .. })
            | Ok(reply @ Reply::Answer { id: reply_id, .. })
                if reply_id == id =>
            {
                return Some(reply)
            }
            // A late answer to an older request.
            Ok(_) => {}
        }
    }
}

/// Drops replies left over from an earlier turn (a cancel sent while idle).
fn drain_replies() {
    let guard = REPLIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(receiver) = guard.as_ref() {
        while receiver.try_recv().is_ok() {}
    }
}

/// `AskUserQuestion` in the app: shows the question with its options and
/// returns the answer in the same shape as the terminal picker.
pub(crate) fn ask_user(input: &Value) -> Result<String, String> {
    let question = input
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let options = input
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    option
                        .as_str()
                        .or_else(|| option.get("label").and_then(Value::as_str))
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    emit(&json!({
        "ev": "question",
        "id": id,
        "question": question,
        "header": input.get("header"),
        "options": options,
        "multi_select": input.get("multiSelect").or_else(|| input.get("multi_select")),
    }));
    let value = match wait_reply(id) {
        Some(Reply::Answer { text, .. }) => json!({
            "question": question,
            "answer": text,
            "status": "answered",
        }),
        _ => json!({
            "question": question,
            "answer": null,
            "status": "cancelled",
            "note": "The user dismissed the question.",
        }),
    };
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

/// Asks the app to approve a tool call, honoring "always" answers given
/// earlier in this session.
struct DesktopPermissionPrompter;

impl runtime::PermissionPrompter for DesktopPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        let command = serde_json::from_str::<Value>(&request.input)
            .ok()
            .and_then(|input| {
                input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let approval_key = permission_request_approval_key(request, command.as_deref());
        let approvals = |f: &mut dyn FnMut(&mut BTreeSet<String>) -> bool| {
            f(&mut SESSION_APPROVALS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner))
        };
        if approvals(&mut |set| set.contains(&approval_key)) {
            return runtime::PermissionPromptDecision::Allow;
        }
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        emit(&json!({
            "ev": "permission",
            "id": id,
            "tool": request.tool_name,
            "input": request.input,
            "command": command,
            "reason": request.reason,
            "host": runtime::is_host_execution_request(&request.tool_name, &request.input),
        }));
        match wait_reply(id) {
            Some(Reply::Permission { decision, .. }) if decision == "always" => {
                approvals(&mut |set| set.insert(approval_key.clone()));
                runtime::PermissionPromptDecision::Allow
            }
            Some(Reply::Permission { decision, .. }) if decision == "allow" => {
                runtime::PermissionPromptDecision::Allow
            }
            _ => runtime::PermissionPromptDecision::Deny {
                reason: "The user declined this in the claw desktop app.".to_string(),
            },
        }
    }
}

/// Reads commands from stdin until it closes. `cancel` and replies are
/// handled here, while a turn runs; prompts and model switches go to the
/// turn loop.
fn spawn_command_reader(commands: mpsc::Sender<Command>, replies: mpsc::Sender<Reply>) {
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let field = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
            let id = value.get("id").and_then(Value::as_u64).unwrap_or_default();
            let sent = match field("cmd") {
                "prompt" => commands
                    .send(Command::Prompt(field("text").to_string()))
                    .is_ok(),
                "model" => commands
                    .send(Command::Model(field("model").to_string()))
                    .is_ok(),
                "rewind" => commands
                    .send(Command::Rewind(
                        usize::try_from(value.get("prompt").and_then(Value::as_u64).unwrap_or(0))
                            .unwrap_or(usize::MAX),
                    ))
                    .is_ok(),
                "cancel" => {
                    if let Some(signal) = CURRENT_ABORT
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                    {
                        signal.abort();
                    }
                    replies.send(Reply::Cancel).is_ok()
                }
                "permission" => replies
                    .send(Reply::Permission {
                        id,
                        decision: field("decision").to_string(),
                    })
                    .is_ok(),
                "answer" => replies
                    .send(Reply::Answer {
                        id,
                        text: field("text").to_string(),
                    })
                    .is_ok(),
                // Background shells and monitors run outside any turn, so
                // the app stops them directly, even while a turn runs.
                "stop_background" => {
                    let jobs = runtime::global_background_jobs();
                    match value.get("id").and_then(Value::as_str) {
                        Some(id) => {
                            let _ = jobs.kill(id);
                        }
                        None => jobs.kill_all(),
                    }
                    true
                }
                "shutdown" => false,
                _ => true,
            };
            if !sent {
                break;
            }
        }
        // The app is gone: stop whatever runs.
        if let Some(signal) = CURRENT_ABORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            signal.abort();
        }
        let _ = replies.send(Reply::Cancel);
    });
}

/// Runs the agent until stdin closes.
pub(crate) fn run(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    resume: Option<String>,
    incognito: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    ACTIVE.store(true, Ordering::Relaxed);
    INCOGNITO.store(incognito, Ordering::Relaxed);
    let resolved_model = resolve_repl_model(model)?;
    let mut cli = LiveCli::new(resolved_model, true, allowed_tools, permission_mode)?;
    if incognito {
        // LiveCli::new saved an empty session file; drop it and stop saving.
        if let Some(rt) = cli.runtime.runtime.as_mut() {
            rt.session_mut().clear_persistence();
        }
        let _ = std::fs::remove_file(&cli.session.path);
    }
    if let Some(session) = resume {
        let fresh = cli.session.path.clone();
        cli.resume_session(Some(session))?;
        // LiveCli::new saved an empty session; resuming made it unused.
        if cli.session.path != fresh {
            let _ = std::fs::remove_file(&fresh);
        }
    }
    cli.system_prompt.push(DESKTOP_PROMPT.to_string());
    let (command_tx, command_rx) = mpsc::channel();
    let (reply_tx, reply_rx) = mpsc::channel();
    *REPLIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reply_rx);
    spawn_command_reader(command_tx, reply_tx);
    emit(&json!({
        "ev": "ready",
        "session_id": cli.session.id,
        "session_path": (!incognito).then_some(&cli.session.path),
        "incognito": incognito,
        "model": cli.model,
        "cwd": std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        "permission_mode": cli.permission_mode.as_str(),
    }));
    cli.emit_context();
    spawn_background_reporter();
    loop {
        let command = match command_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // A background job finished or a monitor saw something while
                // the chat was idle: let the agent react, as the REPL does.
                if runtime::global_background_jobs().has_notices() {
                    emit(&json!({"ev": "wake"}));
                    cli.run_desktop_turn(crate::BACKGROUND_WAKE_PROMPT);
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match command {
            Command::Prompt(text) => cli.run_desktop_turn(&text),
            Command::Model(model) => {
                cli.model = resolve_model_alias_with_config(&model);
                runtime::model_server::ensure_for_model(&cli.model);
                emit(&json!({"ev": "model", "model": cli.model}));
                cli.emit_context();
            }
            Command::Rewind(prompt) => cli.rewind_to_prompt(prompt),
        }
    }
    // Closing the chat ends what it left running in the background.
    runtime::global_background_jobs().kill_all();
    cli.runtime.shutdown_plugins()?;
    Ok(())
}

/// Tells the app which background shells and monitors are running, each
/// time that changes, so it can show them with a way to stop them.
fn spawn_background_reporter() {
    std::thread::spawn(|| {
        let mut last: Option<Vec<String>> = None;
        loop {
            let running: Vec<runtime::JobSummary> = runtime::global_background_jobs()
                .list()
                .into_iter()
                .filter(|job| job.state == runtime::JobState::Running)
                .collect();
            let ids: Vec<String> = running.iter().map(|job| job.id.clone()).collect();
            if last.as_ref() != Some(&ids) {
                let jobs: Vec<Value> = running
                    .iter()
                    .map(|job| json!({"id": job.id, "command": job.command, "seconds": job.elapsed_secs}))
                    .collect();
                emit(&json!({"ev": "background", "jobs": jobs}));
                last = Some(ids);
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

/// A prompt the user typed, as the app lists them: a user message with
/// text that is not the "stopped" marker (tool results are user messages
/// too, without text).
fn is_typed_prompt(message: &runtime::ConversationMessage) -> bool {
    message.role == runtime::MessageRole::User
        && message.blocks.iter().any(|block| {
            matches!(block, runtime::ContentBlock::Text { text }
                if !text.trim().is_empty() && text.trim() != "[Request interrupted by user]")
        })
}

impl LiveCli {
    /// Cuts the conversation just before the `prompt`-th typed prompt and
    /// saves it, so an edited prompt replaces that one and what followed.
    fn rewind_to_prompt(&mut self, prompt: usize) {
        let cut = self
            .runtime
            .session()
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| is_typed_prompt(message))
            .nth(prompt)
            .map(|(index, _)| index);
        let Some(cut) = cut else {
            emit(&json!({"ev": "error", "message": "could not find that message to edit"}));
            return;
        };
        self.runtime.session_mut().messages.truncate(cut);
        if !INCOGNITO.load(Ordering::Relaxed) {
            if let Err(error) = self.persist_session() {
                emit(
                    &json!({"ev": "error", "message": format!("could not save the session: {error}")}),
                );
            }
        }
        self.emit_context();
    }

    /// One turn for the app: the same runtime as a REPL turn, with approvals
    /// and questions answered in the app and the reply streamed as events.
    /// A turn from the app, then LeanGate: while the files it edited have
    /// type errors, the agent is asked to fix them (a few rounds at most).
    fn run_desktop_turn(&mut self, input: &str) {
        let _ = tools::leangate::take_edited_files();
        let mut status = self.run_desktop_turn_once(input);
        for _ in 0..crate::LEAN_GATE_ROUNDS {
            let edited = tools::leangate::take_edited_files();
            if status != "done" || edited.is_empty() {
                break;
            }
            let Some(report) = tools::leangate::completion_check(&edited) else {
                break;
            };
            emit(
                &json!({"ev": "notice", "text": "LeanGate: the edited files have type errors; claw fixes them before finishing"}),
            );
            status = self.run_desktop_turn_once(&report);
        }
    }

    fn run_desktop_turn_once(&mut self, input: &str) -> &'static str {
        drain_replies();
        TEXT_SENT.store(false, Ordering::Relaxed);
        emit(&json!({"ev": "turn_start"}));
        let (mut runtime, monitor) = match self.prepare_turn_runtime(false) {
            Ok(prepared) => prepared,
            Err(error) => {
                emit(&json!({"ev": "error", "message": error.to_string()}));
                emit(&json!({"ev": "turn_end", "status": "error"}));
                return "error";
            }
        };
        *CURRENT_ABORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(monitor.signal());
        let prompt = Self::with_job_notices(input);
        let mut prompter = DesktopPermissionPrompter;
        let attachments = crate::repl_commands::attached_images(input);
        let result = runtime.run_turn_with_attachments(&prompt, attachments, Some(&mut prompter));
        monitor.stop();
        *CURRENT_ABORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let status = match result {
            Ok(summary) => {
                if !TEXT_SENT.load(Ordering::Relaxed) {
                    // Providers that do not stream text: send the reply whole.
                    emit_text(&final_assistant_text(&summary));
                }
                let usage = summary.usage;
                if let Err(error) = self.replace_runtime(runtime) {
                    emit(&json!({"ev": "error", "message": error.to_string()}));
                }
                emit(&json!({
                    "ev": "usage",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                }));
                "done"
            }
            Err(error) if error.to_string().contains(INTERRUPTED_BY_USER) => {
                // Keep what the turn did, as the REPL does after Esc.
                if let Some(rt) = runtime.runtime.as_mut() {
                    let _ = rt
                        .session_mut()
                        .push_user_text("[Request interrupted by user]");
                }
                if let Err(error) = self.replace_runtime(runtime) {
                    emit(&json!({"ev": "error", "message": error.to_string()}));
                }
                "stopped"
            }
            Err(error) => {
                let _ = runtime.shutdown_plugins();
                emit(&json!({"ev": "error", "message": error.to_string()}));
                "error"
            }
        };
        if INCOGNITO.load(Ordering::Relaxed) {
            // Nothing is saved.
        } else if let Err(error) = self.persist_session() {
            emit(
                &json!({"ev": "error", "message": format!("could not save the session: {error}")}),
            );
        }
        emit(&json!({"ev": "turn_end", "status": status}));
        self.emit_context();
        status
    }

    /// How full the context window is, for the app's status bar.
    fn emit_context(&self) {
        let tokens = runtime::estimate_session_tokens(self.runtime.session());
        emit(&json!({
            "ev": "context",
            "tokens": tokens,
            "window": known_context_window(&self.model),
        }));
    }
}
