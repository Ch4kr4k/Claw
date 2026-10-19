//! Tools from Claude Code's tool set that claw implements locally:
//! `Monitor`, `EnterWorktree`/`ExitWorktree`, `ScheduleWakeup`,
//! `ListAgents`/`SendMessage` (sub-agents of this session),
//! `PushNotification`, `ReportFindings`, `SendFeedback`, `SendUserFile`,
//! `EndConversation` and `WaitForMcpServers`. Where Claude Code reaches a
//! hosted service (Remote Control, phone push, claude.ai), these do the
//! local part only and say so in their descriptions.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use runtime::PermissionMode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{to_pretty_json, ToolSpec};

/// The local tools' definitions.
#[allow(clippy::too_many_lines)]
pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "Monitor",
            description: "Run a command in the background and get each batch of new output lines back as a notice, so you can react to log lines, file changes or polled status while the conversation goes on (even while the user's prompt is idle). Filter the command's output to the lines you would act on (grep --line-buffered), and cover failures as well as success: silence looks like still-running. It stops after timeout_ms (default 5 minutes, at most 1 hour), on exit, or when it prints more than 600 lines a minute. Stop it early with TaskStop or KillShell.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command; each stdout/stderr line is an event." },
                    "description": { "type": "string", "description": "What is being watched, shown with every event." },
                    "timeout_ms": { "type": "integer", "minimum": 1000, "maximum": 3_600_000 }
                },
                "required": ["command", "description"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "EnterWorktree",
            description: "Create an isolated git worktree under .claw/worktrees/ (on a new branch claw/<name>) and switch the session into it, or switch into an existing worktree of this repository by path. ExitWorktree returns to the original directory.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name for a new worktree (letters, digits, -, _)." },
                    "path": { "type": "string", "description": "An existing worktree to switch into instead." }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "ExitWorktree",
            description: "Leave the worktree entered with EnterWorktree and return to the original directory. action \"keep\" (default) leaves the worktree on disk; \"remove\" deletes it and its branch, refusing when it has uncommitted changes unless discard_changes is true.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["keep", "remove"] },
                    "discard_changes": { "type": "boolean" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "ScheduleWakeup",
            description: "Pick when the next iteration of a self-paced /loop runs: delaySeconds from now (60-3600), with the same /loop prompt. Call it at the end of each iteration; call it with stop: true to end the loop. Match the delay to what you wait for (a CI run: one check when it should be done, not many short polls).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "delaySeconds": { "type": "number" },
                    "prompt": { "type": "string" },
                    "reason": { "type": "string", "description": "One short sentence on why this delay." },
                    "noop": { "type": "boolean" },
                    "stop": { "type": "boolean" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ListAgents",
            description: "List the agents you can message with SendMessage (sub-agents started in this session, with their id, name and status), plus background claw sessions and background shells for reference.",
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SendMessage",
            description: "Send a message to a sub-agent of this session by its id or name. A finished agent is resumed with its earlier conversation and its reply arrives as a notice; a running agent gets the message when its current run ends. Other claw sessions cannot be messaged.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Agent id or name (ListAgents shows them)." },
                    "message": { "type": "string" },
                    "summary": { "type": "string", "description": "5-10 word preview." }
                },
                "required": ["to", "message"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "PushNotification",
            description: "Send a desktop notification (notify-send, osascript, or the terminal's own notification sequence) so a long task can reach the user when they step away. No phone push: that needs Anthropic's hosted Remote Control.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "title": { "type": "string" }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ReportFindings",
            description: "Report code-review findings as a structured list so they are shown as one: file, summary and a concrete failure scenario per finding, most severe first. Use it only when the active review instructions say so.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "findings": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file": { "type": "string" },
                                "line": { "type": "integer" },
                                "summary": { "type": "string" },
                                "short_summary": { "type": "string" },
                                "failure_scenario": { "type": "string" },
                                "category": { "type": "string" },
                                "verdict": { "type": "string", "enum": ["CONFIRMED", "PLAUSIBLE"] },
                                "outcome": { "type": "string", "enum": ["fixed", "skipped", "no_change_needed"] }
                            },
                            "required": ["file", "summary", "failure_scenario"]
                        }
                    },
                    "level": { "type": "string" }
                },
                "required": ["findings"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SendFeedback",
            description: "Draft a feedback report about claw (a product problem, or your own behaviour in this session) and save it under ~/.claw/feedback/drafts for the user to review. Nothing is sent. details: short labelled bullets (What happened, What the user said, Repro, Evidence).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "type": { "type": "string", "enum": ["bug", "idea", "missing_capability"] },
                    "title": { "type": "string" },
                    "details": { "type": "string" },
                    "area": { "type": "string" },
                    "failure_mode": { "type": "string" },
                    "task_category": { "type": "string" }
                },
                "required": ["type", "title", "details"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SendUserFile",
            description: "Hand files from the session to the user with an optional caption: they are listed for the user, and with display \"render\" (or unset, for images, PDFs and HTML) opened in the desktop's viewer.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "files": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                    "caption": { "type": "string" },
                    "display": { "type": "string", "enum": ["render", "attach"] }
                },
                "required": ["files"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "EndConversation",
            description: "End the session. Only for sustained abusive input directed at you, or when the user asks to see it demonstrated.",
            input_schema: json!({
                "type": "object",
                "properties": { "reason": { "type": "string" } },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WaitForMcpServers",
            description: "Wait for MCP servers that are still connecting, so a request can use their tools, and report each server's status.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "servers": { "type": "array", "items": { "type": "string" } },
                    "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 120_000 }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}

/// Runs a local tool; `None` when `name` is not one.
pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    let parse = |input: &Value| input.clone();
    Some(match name {
        "Monitor" => from(parse(input)).and_then(monitor),
        "EnterWorktree" => from(parse(input)).and_then(worktree::enter),
        "ExitWorktree" => from(parse(input)).and_then(worktree::exit),
        "ScheduleWakeup" => from(parse(input)).and_then(schedule_wakeup),
        "ListAgents" => list_agents(),
        "SendMessage" => from(parse(input)).and_then(agents::send),
        "PushNotification" => from(parse(input)).and_then(push_notification),
        "ReportFindings" => from(parse(input)).and_then(report_findings),
        "SendFeedback" => from(parse(input)).and_then(send_feedback),
        "SendUserFile" => from(parse(input)).and_then(send_user_file),
        "EndConversation" => from(parse(input)).and_then(end_conversation),
        "WaitForMcpServers" => from(parse(input)).and_then(wait_for_mcp_servers),
        _ => return None,
    })
}

fn from<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|error| error.to_string())
}

// ───────────────────────────── Monitor ─────────────────────────────

#[derive(Deserialize)]
struct MonitorInput {
    command: String,
    description: String,
    timeout_ms: Option<u64>,
}

fn monitor(input: MonitorInput) -> Result<String, String> {
    if input.command.trim().is_empty() {
        return Err("command must not be empty".to_string());
    }
    let timeout =
        Duration::from_millis(input.timeout_ms.unwrap_or(300_000).clamp(1_000, 3_600_000));
    let mut command = Command::new("sh");
    command.arg("-c").arg(&input.command);
    let id = runtime::global_background_jobs()
        .start_monitor(command, &input.command, &input.description, timeout)
        .map_err(|error| format!("could not start the monitor: {error}"))?;
    to_pretty_json(json!({
        "monitor_id": id,
        "description": input.description,
        "timeout_secs": timeout.as_secs(),
        "message": format!("Monitor {id} started. New output arrives as notices; stop it with TaskStop(\"{id}\")."),
    }))
}

// ───────────────────────────── worktrees ─────────────────────────────

pub(crate) mod worktree {
    use super::{git, json, to_pretty_json, Deserialize, Mutex, Path, PathBuf};

    /// The worktree session: where it started and the worktree entered.
    struct Entered {
        original: PathBuf,
        worktree: PathBuf,
        /// Branch created for it (removed with the worktree).
        branch: Option<String>,
    }

    static ENTERED: Mutex<Option<Entered>> = Mutex::new(None);

    #[derive(Deserialize)]
    pub(crate) struct EnterInput {
        name: Option<String>,
        path: Option<String>,
    }

    #[derive(Deserialize)]
    pub(crate) struct ExitInput {
        action: Option<String>,
        #[serde(default)]
        discard_changes: bool,
    }

    fn top_level(dir: &Path) -> Result<PathBuf, String> {
        git(dir, &["rev-parse", "--show-toplevel"])
            .map(|out| PathBuf::from(out.trim()))
            .map_err(|_| format!("{} is not inside a git repository", dir.display()))
    }

    fn common_dir(dir: &Path) -> Option<PathBuf> {
        let out = git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .ok()?;
        PathBuf::from(out.trim()).canonicalize().ok()
    }

    pub(crate) fn enter(input: EnterInput) -> Result<String, String> {
        let mut entered = ENTERED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let original = entered
            .as_ref()
            .map_or_else(|| cwd.clone(), |e| e.original.clone());
        let repo = top_level(&original)?;
        let worktrees_dir = repo.join(".claw").join("worktrees");
        let (worktree, branch) = if let Some(path) = input.path.filter(|p| !p.trim().is_empty()) {
            let path = runtime::expand_home(path.trim());
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            let path = path
                .canonicalize()
                .map_err(|_| format!("no such directory: {}", path.display()))?;
            if common_dir(&path).is_none() || common_dir(&path) != common_dir(&repo) {
                return Err(format!(
                    "{} is not a worktree of this repository (git worktree list shows them)",
                    path.display()
                ));
            }
            if entered.is_some() && !path.starts_with(&worktrees_dir) {
                return Err(
                    "from a worktree session only worktrees under .claw/worktrees/ can be entered"
                        .to_string(),
                );
            }
            (path, None)
        } else {
            if entered.is_some() {
                return Err("already in a worktree: ExitWorktree first, or pass a path".to_string());
            }
            let name = input
                .name
                .map(|name| {
                    name.chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                c
                            } else {
                                '-'
                            }
                        })
                        .collect::<String>()
                })
                .filter(|name| !name.trim_matches('-').is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "wt-{}",
                        super::super::iso8601_now().replace([':', '-', 'T', 'Z', '.'], "")
                    )
                });
            let path = worktrees_dir.join(&name);
            if path.exists() {
                return Err(format!(
                    "{} already exists; pass it as path to enter it",
                    path.display()
                ));
            }
            let branch = format!("claw/{name}");
            std::fs::create_dir_all(&worktrees_dir).map_err(|error| error.to_string())?;
            let added = git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    &path.display().to_string(),
                    "HEAD",
                ],
            )
            .map_err(|error| format!("git worktree add failed: {error}"))?;
            let _ = added;
            (path, Some(branch))
        };
        std::env::set_current_dir(&worktree).map_err(|error| error.to_string())?;
        let shown_branch = git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map(|b| b.trim().to_string())
            .unwrap_or_default();
        *entered = Some(Entered {
            original: original.clone(),
            worktree: worktree.clone(),
            branch: branch
                .clone()
                .or_else(|| entered.as_ref().and_then(|e| e.branch.clone())),
        });
        to_pretty_json(json!({
            "worktree": worktree.display().to_string(),
            "branch": shown_branch,
            "original_directory": original.display().to_string(),
            "message": format!(
                "Working directory is now {} (branch {shown_branch}). File tools and shell commands work there; ExitWorktree goes back.",
                worktree.display()
            ),
        }))
    }

    pub(crate) fn exit(input: ExitInput) -> Result<String, String> {
        let mut entered = ENTERED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(session) = entered.as_ref() else {
            return Err("not in a worktree session (EnterWorktree starts one)".to_string());
        };
        let remove = input.action.as_deref() == Some("remove");
        if remove && !input.discard_changes {
            let status = git(&session.worktree, &["status", "--porcelain"]).unwrap_or_default();
            if !status.trim().is_empty() {
                return Err(format!(
                    "the worktree has uncommitted changes; commit them, or pass discard_changes: true to delete them:\n{}",
                    status.trim_end()
                ));
            }
        }
        std::env::set_current_dir(&session.original).map_err(|error| error.to_string())?;
        let mut message = format!("Back in {}.", session.original.display());
        if remove {
            let repo = top_level(&session.original)?;
            git(
                &repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &session.worktree.display().to_string(),
                ],
            )
            .map_err(|error| format!("git worktree remove failed: {error}"))?;
            if let Some(branch) = &session.branch {
                let _ = git(&repo, &["branch", "-D", branch]);
            }
            message.push_str(&format!(" Removed {}.", session.worktree.display()));
        } else {
            message.push_str(&format!(
                " The worktree stays at {}.",
                session.worktree.display()
            ));
        }
        let original = session.original.display().to_string();
        *entered = None;
        to_pretty_json(json!({ "directory": original, "removed": remove, "message": message }))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

// ───────────────────────────── ScheduleWakeup ─────────────────────────────

#[derive(Deserialize)]
struct WakeupInput {
    #[serde(rename = "delaySeconds")]
    delay_seconds: Option<f64>,
    prompt: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    stop: bool,
}

fn schedule_wakeup(input: WakeupInput) -> Result<String, String> {
    let scheduler = runtime::scheduler::global_scheduler();
    if input.stop {
        let cancelled = scheduler.cancel_wakeup();
        return to_pretty_json(json!({
            "stopped": true,
            "message": if cancelled { "Loop ended; the pending wake-up was cancelled." } else { "Loop ended." },
        }));
    }
    let prompt = input
        .prompt
        .filter(|p| !p.trim().is_empty())
        .ok_or("prompt is required unless stop is true")?;
    let seconds = input
        .delay_seconds
        .ok_or("delaySeconds is required unless stop is true")?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let seconds = seconds.clamp(60.0, 3600.0) as u64;
    scheduler.set_wakeup(
        Duration::from_secs(seconds),
        &prompt,
        input.reason.as_deref(),
    );
    to_pretty_json(json!({
        "delay_seconds": seconds,
        "message": format!("Next iteration in {seconds}s{}.", input.reason.map(|r| format!(" ({r})")).unwrap_or_default()),
    }))
}

// ───────────────────────────── agents ─────────────────────────────

/// Rows for background claw sessions, supplied by the CLI.
type SessionRows = fn() -> Vec<Value>;
static BACKGROUND_SESSIONS: Mutex<Option<SessionRows>> = Mutex::new(None);

/// Lets `ListAgents` show the CLI's background sessions.
pub fn set_background_sessions_provider(provider: SessionRows) {
    *BACKGROUND_SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(provider);
}

fn list_agents() -> Result<String, String> {
    let sessions = BACKGROUND_SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .map(|provider| provider())
        .unwrap_or_default();
    let shells: Vec<Value> = runtime::global_background_jobs()
        .list()
        .into_iter()
        .map(|job| json!({ "id": job.id, "command": job.command, "state": job.state.to_string() }))
        .collect();
    let agents = agents::rows();
    to_pretty_json(json!({
        "agents": agents,
        "background_sessions": sessions,
        "background_shells": shells,
        "note": "SendMessage reaches the agents listed under `agents` (by id or name).",
    }))
}

pub(crate) mod agents {
    use std::collections::BTreeMap;

    use runtime::Session;
    use serde::Deserialize;
    use serde_json::{json, Value};

    use super::super::{run_agent_conversation, to_pretty_json, truncate_for_notice, AgentJob};
    use super::Mutex;

    struct Record {
        job: AgentJob,
        session: Option<Session>,
        status: &'static str,
        /// Messages that arrived while it ran.
        inbox: Vec<String>,
    }

    static AGENTS: Mutex<BTreeMap<String, Record>> = Mutex::new(BTreeMap::new());

    fn agents() -> std::sync::MutexGuard<'static, BTreeMap<String, Record>> {
        AGENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A sub-agent started (or was resumed).
    pub(crate) fn started(job: &AgentJob) {
        let mut agents = agents();
        let record = agents
            .entry(job.manifest.agent_id.clone())
            .or_insert_with(|| Record {
                job: job.clone(),
                session: None,
                status: "running",
                inbox: Vec::new(),
            });
        record.status = "running";
    }

    /// A run of a sub-agent ended: keeps its conversation for SendMessage
    /// and, when messages arrived meanwhile, resumes it with them.
    pub(crate) fn finished(
        job: &AgentJob,
        (result, session): (Result<String, String>, Session),
    ) -> Result<String, String> {
        let inbox = {
            let mut agents = agents();
            let record = agents
                .entry(job.manifest.agent_id.clone())
                .or_insert_with(|| Record {
                    job: job.clone(),
                    session: None,
                    status: "running",
                    inbox: Vec::new(),
                });
            record.session = Some(session);
            record.status = if result.is_ok() {
                "completed"
            } else {
                "failed"
            };
            std::mem::take(&mut record.inbox)
        };
        if !inbox.is_empty() {
            let _ = resume(&job.manifest.agent_id, inbox.join("\n\n"));
        }
        result
    }

    pub(crate) fn rows() -> Vec<Value> {
        agents()
            .iter()
            .map(|(id, record)| {
                json!({
                    "id": id,
                    "name": record.job.manifest.name,
                    "description": record.job.manifest.description,
                    "status": record.status,
                    "queued_messages": record.inbox.len(),
                })
            })
            .collect()
    }

    #[derive(Deserialize)]
    pub(crate) struct SendInput {
        to: String,
        message: String,
    }

    pub(crate) fn send(input: SendInput) -> Result<String, String> {
        if input.message.trim().is_empty() {
            return Err("message must not be empty".to_string());
        }
        let id = {
            let mut agents = agents();
            let to = input.to.trim();
            let id = agents
                .iter()
                .rev()
                .find(|(id, record)| id.as_str() == to || record.job.manifest.name == to)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| {
                    let known: Vec<String> = agents
                        .iter()
                        .map(|(id, r)| format!("{id} ({})", r.job.manifest.name))
                        .collect();
                    if known.is_empty() {
                        format!("no agent `{to}`: no sub-agents have run in this session")
                    } else {
                        format!("no agent `{to}`; known: {}", known.join(", "))
                    }
                })?;
            if let Some(record) = agents.get_mut(&id) {
                if record.status == "running" {
                    record.inbox.push(input.message.clone());
                    return to_pretty_json(json!({
                        "to": id,
                        "status": "queued",
                        "message": "The agent is running; it gets the message when its current run ends, and its reply arrives as a notice.",
                    }));
                }
            }
            id
        };
        resume(&id, input.message)?;
        to_pretty_json(json!({
            "to": id,
            "status": "resumed",
            "message": "The agent was resumed with its earlier conversation; its reply arrives as a notice.",
        }))
    }

    fn resume(id: &str, message: String) -> Result<(), String> {
        let (job, session) = {
            let mut agents = agents();
            let record = agents
                .get_mut(id)
                .ok_or_else(|| format!("no agent `{id}`"))?;
            record.status = "running";
            (
                record.job.clone(),
                record.session.take().unwrap_or_default(),
            )
        };
        std::thread::Builder::new()
            .name(format!("clawd-agent-{id}-resume"))
            .spawn(move || {
                let outcome = run_agent_conversation(&job, session, message);
                let notice = match finished(&job, outcome) {
                    Ok(text) => format!(
                        "Agent {} ({}) replied:\n{}",
                        job.manifest.agent_id,
                        job.manifest.name,
                        truncate_for_notice(&text, 4000)
                    ),
                    Err(error) => format!(
                        "Agent {} ({}) failed on the message: {error}",
                        job.manifest.agent_id, job.manifest.name
                    ),
                };
                runtime::global_background_jobs().push_notice(notice);
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

// ───────────────────────────── notifications, files, feedback ─────────────────────────────

#[derive(Deserialize)]
struct NotificationInput {
    message: String,
    title: Option<String>,
}

fn push_notification(input: NotificationInput) -> Result<String, String> {
    let title = input.title.unwrap_or_else(|| "claw".to_string());
    let mut delivered = Vec::new();
    let ran = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if cfg!(target_os = "macos") {
        let script = format!(
            "display notification {} with title {}",
            applescript_string(&input.message),
            applescript_string(&title)
        );
        if ran("osascript", &["-e", &script]) {
            delivered.push("osascript");
        }
    } else if ran("notify-send", &["--app-name=claw", &title, &input.message]) {
        delivered.push("notify-send");
    }
    // Terminals that support it (kitty, WezTerm, iTerm2, foot) show OSC 9.
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        use std::io::Write;
        let text: String = input.message.chars().filter(|c| !c.is_control()).collect();
        if write!(tty, "\x1b]9;{title}: {text}\x07\x07").is_ok() {
            delivered.push("terminal");
        }
    }
    if delivered.is_empty() {
        return Err(
            "no way to show a notification here (no notify-send/osascript, no terminal)"
                .to_string(),
        );
    }
    to_pretty_json(json!({ "delivered_via": delivered, "phone_push": false }))
}

fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Deserialize)]
struct Finding {
    file: String,
    line: Option<i64>,
    summary: String,
    short_summary: Option<String>,
    failure_scenario: String,
    category: Option<String>,
    verdict: Option<String>,
    outcome: Option<String>,
}

#[derive(Deserialize)]
struct FindingsInput {
    findings: Vec<Finding>,
    level: Option<String>,
}

fn report_findings(input: FindingsInput) -> Result<String, String> {
    for (index, finding) in input.findings.iter().enumerate() {
        if finding.file.trim().is_empty() || finding.summary.trim().is_empty() {
            return Err(format!("finding {} needs a file and a summary", index + 1));
        }
    }
    let findings: Vec<Value> = input
        .findings
        .iter()
        .map(|f| {
            json!({
                "file": f.file,
                "line": f.line,
                "summary": f.summary,
                "short_summary": f.short_summary,
                "failure_scenario": f.failure_scenario,
                "category": f.category,
                "verdict": f.verdict,
                "outcome": f.outcome,
            })
        })
        .collect();
    to_pretty_json(json!({
        "count": findings.len(),
        "level": input.level,
        "findings": findings,
        "message": "Findings shown to the user; do not repeat them as text.",
    }))
}

#[derive(Deserialize)]
struct FeedbackInput {
    #[serde(rename = "type")]
    kind: String,
    title: String,
    details: String,
    area: Option<String>,
    failure_mode: Option<String>,
    task_category: Option<String>,
}

fn send_feedback(input: FeedbackInput) -> Result<String, String> {
    let dir = runtime::default_config_home()
        .join("feedback")
        .join("drafts");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let stamp = super::iso8601_now().replace(':', "-");
    let path = dir.join(format!("{stamp}.json"));
    let draft = json!({
        "type": input.kind,
        "title": input.title,
        "details": input.details,
        "area": input.area,
        "failure_mode": input.failure_mode,
        "task_category": input.task_category,
        "created_at": super::iso8601_now(),
        "sent": false,
    });
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&draft).unwrap_or_default(),
    )
    .map_err(|error| error.to_string())?;
    to_pretty_json(json!({
        "draft": path.display().to_string(),
        "message": "Saved as a local draft; nothing was sent.",
    }))
}

#[derive(Deserialize)]
struct UserFileInput {
    files: Vec<String>,
    caption: Option<String>,
    display: Option<String>,
}

fn send_user_file(input: UserFileInput) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    for file in &input.files {
        let path = runtime::expand_home(file);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let meta =
            std::fs::metadata(&path).map_err(|_| format!("no such file: {}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let renderable = matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "pdf" | "html" | "htm")
        );
        let render = match input.display.as_deref() {
            Some("render") => true,
            Some("attach") => false,
            _ => renderable,
        };
        let opened = render && open_in_viewer(&path);
        files.push(json!({
            "path": path.display().to_string(),
            "bytes": meta.len(),
            "opened": opened,
        }));
    }
    to_pretty_json(json!({ "files": files, "caption": input.caption }))
}

fn open_in_viewer(path: &Path) -> bool {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(program)
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

// ───────────────────────────── EndConversation ─────────────────────────────

static END_REQUESTED: Mutex<Option<String>> = Mutex::new(None);

#[derive(Deserialize)]
struct EndInput {
    reason: Option<String>,
}

fn end_conversation(input: EndInput) -> Result<String, String> {
    let reason = input
        .reason
        .unwrap_or_else(|| "ended by the assistant".to_string());
    *END_REQUESTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    to_pretty_json(json!({ "ended": true, "message": "The session ends after this reply." }))
}

/// Takes the reason `EndConversation` gave, if it was called; the REPL ends
/// the session after the turn.
pub fn take_end_conversation() -> Option<String> {
    END_REQUESTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

// ───────────────────────────── WaitForMcpServers ─────────────────────────────

/// Reports MCP servers' status, waiting up to the duration for the named
/// (or all) servers to finish connecting; supplied by the CLI.
type McpWaiter = fn(&[String], Duration) -> Value;
static MCP_WAITER: Mutex<Option<McpWaiter>> = Mutex::new(None);

/// Lets `WaitForMcpServers` see the CLI's MCP servers.
pub fn set_mcp_waiter(waiter: McpWaiter) {
    *MCP_WAITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waiter);
}

#[derive(Deserialize)]
struct WaitInput {
    #[serde(default)]
    servers: Vec<String>,
    timeout_ms: Option<u64>,
}

fn wait_for_mcp_servers(input: WaitInput) -> Result<String, String> {
    let waiter = *MCP_WAITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(waiter) = waiter else {
        return to_pretty_json(
            json!({ "servers": [], "message": "No MCP servers are configured." }),
        );
    };
    let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(30_000).min(120_000));
    to_pretty_json(waiter(&input.servers, timeout))
}

#[cfg(test)]
mod tests {
    use super::{execute, take_end_conversation};
    use serde_json::json;

    #[test]
    fn every_local_tool_has_a_spec_and_a_handler() {
        for spec in super::specs() {
            // Bad input must reach the handler (an error), not "unknown tool".
            assert!(
                execute(spec.name, &json!({"__bad": 1})).is_some(),
                "{}",
                spec.name
            );
        }
        assert!(execute("NoSuchTool", &json!({})).is_none());
    }

    #[test]
    fn findings_are_validated_and_end_conversation_is_taken_once() {
        let ok = execute(
            "ReportFindings",
            &json!({"findings": [{"file": "a.rs", "line": 3, "summary": "x", "failure_scenario": "y"}]}),
        )
        .unwrap()
        .unwrap();
        assert!(ok.contains("\"count\": 1"));
        assert!(execute(
            "ReportFindings",
            &json!({"findings": [{"file": "", "summary": "", "failure_scenario": ""}]})
        )
        .unwrap()
        .is_err());
        execute("EndConversation", &json!({"reason": "demo"}))
            .unwrap()
            .unwrap();
        assert_eq!(take_end_conversation().as_deref(), Some("demo"));
        assert_eq!(take_end_conversation(), None);
    }

    #[test]
    fn send_message_to_an_unknown_agent_says_so() {
        let error = execute("SendMessage", &json!({"to": "nobody", "message": "hi"}))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("no agent `nobody`"));
    }

    #[test]
    fn schedule_wakeup_clamps_and_stops() {
        let out = execute(
            "ScheduleWakeup",
            &json!({"delaySeconds": 5, "prompt": "again"}),
        )
        .unwrap()
        .unwrap();
        assert!(out.contains("\"delay_seconds\": 60"));
        assert!(runtime::scheduler::global_scheduler().wakeup().is_some());
        execute("ScheduleWakeup", &json!({"stop": true}))
            .unwrap()
            .unwrap();
        assert!(runtime::scheduler::global_scheduler().wakeup().is_none());
    }
}
