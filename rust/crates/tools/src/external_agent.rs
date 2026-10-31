//! `ExternalAgent`: hands a self-contained task to another coding-agent CLI
//! installed here, Claude Code (`claude -p`) or Codex (`codex exec`), and
//! returns its final answer (after deepseek-harness's Claude Code and Codex
//! subagent providers). A local model can pass a hard subtask to a
//! stronger one this way.
//!
//! The other agent runs with its own model, quota, sandbox and permission
//! rules, outside claw's sandbox (it would block their own config and
//! cache writes), so the call needs claw's full-access permission: the user
//! approves it unless claw already runs with full access. Esc stops it
//! (its whole process group), as does the timeout.
//!
//! The command lines can be changed in `~/.claw/settings.json`:
//! `"externalAgents": {"claude": {"args": [...]}, "codex": {"args": [...]}}`,
//! where `{prompt}` and `{output}` (Codex's last-message file) are filled in.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use runtime::PermissionMode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::ToolSpec;

const DEFAULT_TIMEOUT_SECS: u64 = 1800;
const MAX_ANSWER_CHARS: usize = 30_000;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "ExternalAgent",
        description: "Hand a self-contained task to another coding agent installed on this machine and get its final answer: `claude` (Claude Code) or `codex` (OpenAI Codex). It works in the current directory with its own model, quota and permissions and can read and edit files and run commands. It does not see this conversation, so give it a complete prompt: the goal, the relevant files, constraints and what to report back. Use it when the user asks to delegate, or for a hard subtask a stronger model should do; not for simple lookups.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "enum": ["claude", "codex"] },
                "prompt": { "type": "string", "description": "The complete task" },
                "model": { "type": "string", "description": "Optional model for that agent" },
                "timeout": { "type": "integer", "minimum": 10, "description": "Seconds (default 1800)" }
            },
            "required": ["agent", "prompt"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::DangerFullAccess,
    }]
}

#[derive(Debug, Deserialize)]
struct ExternalAgentInput {
    agent: String,
    prompt: String,
    model: Option<String>,
    timeout: Option<u64>,
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    (name == "ExternalAgent").then(|| {
        let input: ExternalAgentInput =
            serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
        run(&input)
    })
}

/// The program and arguments for `agent`, with `{prompt}` and `{output}`
/// still to fill in.
fn command_line(agent: &str, model: Option<&str>) -> Result<(String, Vec<String>), String> {
    let configured = runtime::read_user_setting_value("externalAgents")
        .and_then(|agents| {
            agents
                .get(agent)
                .and_then(|entry| entry.get("args"))
                .cloned()
        })
        .and_then(|args| serde_json::from_value::<Vec<String>>(args).ok());
    let (program, mut args) = match agent {
        "claude" => (
            "claude",
            configured.unwrap_or_else(|| {
                [
                    "-p",
                    "{prompt}",
                    "--output-format",
                    "json",
                    "--permission-mode",
                    "acceptEdits",
                ]
                .map(String::from)
                .to_vec()
            }),
        ),
        "codex" => (
            "codex",
            configured.unwrap_or_else(|| {
                [
                    "exec",
                    "--skip-git-repo-check",
                    "--sandbox",
                    "workspace-write",
                    "--output-last-message",
                    "{output}",
                    "{prompt}",
                ]
                .map(String::from)
                .to_vec()
            }),
        ),
        other => return Err(format!("unknown agent `{other}`: use claude or codex")),
    };
    if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
        let at = args
            .iter()
            .position(|arg| arg == "{prompt}")
            .unwrap_or(args.len());
        let flag = if agent == "claude" { "--model" } else { "-m" };
        args.splice(at..at, [flag.to_string(), model.to_string()]);
    }
    Ok((program.to_string(), args))
}

fn run(input: &ExternalAgentInput) -> Result<String, String> {
    let (program, args) = command_line(&input.agent, input.model.as_deref())?;
    let output_file = std::env::temp_dir().join(format!(
        "claw-external-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let args = args
        .into_iter()
        .map(|arg| {
            arg.replace("{prompt}", &input.prompt)
                .replace("{output}", &output_file.display().to_string())
        })
        .collect::<Vec<_>>();
    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        format!("could not start `{program}` ({error}); is it installed and on PATH?")
    })?;
    // Read both pipes on threads, so a chatty agent cannot fill one and stall.
    let pipe = |reader: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut reader) = reader {
                let _ = reader.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = pipe(
        child
            .stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn Read + Send>),
    );
    let stderr = pipe(
        child
            .stderr
            .take()
            .map(|err| Box::new(err) as Box<dyn Read + Send>),
    );
    let limit = Duration::from_secs(input.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(10));
    let started = Instant::now();
    let ended = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break Ok(status);
        }
        if runtime::turn_aborted() {
            break Err("stopped by the user");
        }
        if started.elapsed() > limit {
            break Err("timed out");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if ended.is_err() {
        stop_group(child.id());
        let _ = child.wait();
    }
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let last_message = std::fs::read_to_string(&output_file).ok();
    let _ = std::fs::remove_file(&output_file);
    let status = match ended {
        Ok(status) => status,
        Err(reason) => {
            return Err(format!(
                "{program} {reason} after {}s.{}",
                started.elapsed().as_secs(),
                tail("\nIts last output:\n", &stdout)
            ))
        }
    };
    let answer = answer_from(&input.agent, &stdout, last_message.as_deref());
    if !status.success() && answer.trim().is_empty() {
        return Err(format!(
            "{program} exited with {status}.{}{}",
            tail("\nstderr:\n", &stderr),
            tail("\nstdout:\n", &stdout)
        ));
    }
    let answer = runtime::tool_spill::spill_long(answer.trim(), MAX_ANSWER_CHARS, "external-agent");
    Ok(format!(
        "{answer}\n\n[{} finished in {}s{}]",
        match input.agent.as_str() {
            "claude" => "Claude Code",
            _ => "Codex",
        },
        started.elapsed().as_secs(),
        if status.success() {
            ""
        } else {
            ", with an error status"
        }
    ))
}

/// The agent's final answer: Claude Code's JSON `result`, Codex's last
/// message file, else what it printed.
fn answer_from(agent: &str, stdout: &str, last_message: Option<&str>) -> String {
    if agent == "claude" {
        if let Ok(value) = serde_json::from_str::<Value>(stdout.trim()) {
            if let Some(result) = value.get("result").and_then(Value::as_str) {
                let cost = value
                    .get("total_cost_usd")
                    .and_then(Value::as_f64)
                    .map(|cost| format!("\n\n(cost ${cost:.4})"))
                    .unwrap_or_default();
                return format!("{result}{cost}");
            }
        }
    }
    if let Some(message) = last_message.filter(|message| !message.trim().is_empty()) {
        return message.to_string();
    }
    stdout.to_string()
}

fn tail(label: &str, text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let start = text.char_indices().rev().nth(2_000).map_or(0, |(at, _)| at);
    format!("{label}{}", &text[start..])
}

#[cfg(unix)]
fn stop_group(pid: u32) {
    let group = format!("-{pid}");
    let _ = Command::new("kill").args(["-TERM", "--", &group]).status();
    std::thread::sleep(Duration::from_millis(500));
    let _ = Command::new("kill").args(["-KILL", "--", &group]).status();
}

#[cfg(not(unix))]
fn stop_group(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .status();
}

#[cfg(test)]
mod tests {
    use super::{answer_from, command_line};

    #[test]
    fn command_lines_fill_in_the_model_before_the_prompt() {
        let (program, args) = command_line("claude", Some("opus")).unwrap();
        assert_eq!(program, "claude");
        assert_eq!(
            args,
            [
                "-p",
                "--model",
                "opus",
                "{prompt}",
                "--output-format",
                "json",
                "--permission-mode",
                "acceptEdits"
            ]
        );
        let (_, args) = command_line("codex", None).unwrap();
        assert!(args.contains(&"{output}".to_string()) && args.last().unwrap() == "{prompt}");
        assert!(command_line("gemini", None).is_err());
    }

    #[test]
    fn answers_come_from_claude_json_or_codex_last_message() {
        let json = r#"{"type":"result","result":"Fixed the test.","total_cost_usd":0.0123}"#;
        assert_eq!(
            answer_from("claude", json, None),
            "Fixed the test.\n\n(cost $0.0123)"
        );
        assert_eq!(
            answer_from("codex", "log lines", Some("Done: 2 files")),
            "Done: 2 files"
        );
        assert_eq!(answer_from("codex", "log lines", None), "log lines");
    }
}
