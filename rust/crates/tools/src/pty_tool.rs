//! `Pty`: interactive programs in a pseudo-terminal (a REPL, `ssh`, a TUI,
//! a debugger, anything that asks questions or wants a terminal). Each
//! session runs under util-linux `script`, which gives the command a real
//! terminal and relays our pipe to it, so no unsafe pty code is needed here.
//! The model starts a session, sends input or keys, and reads what the
//! program printed since the last read.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use runtime::PermissionMode;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{to_pretty_json, ToolSpec};

/// Most output kept per session; older output is dropped.
const MAX_BUFFER: usize = 1024 * 1024;
/// Most output returned by one call (the newest part).
const MAX_RETURN: usize = 16 * 1024;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "Pty",
        description: "Run an interactive program in a pseudo-terminal and talk to it: REPLs (python, node, gdb, psql), installers that ask questions, ssh, TUIs, anything that needs a terminal or keeps running. action \"start\" runs `command` and returns its first output and a session id; \"send\" types `input` (Enter appended unless enter=false); \"key\" presses a key (ctrl-c, ctrl-d, ctrl-z, esc, tab, enter, up, down, left, right, backspace); \"read\" returns new output; \"list\" shows sessions; \"kill\" ends one. Each call waits wait_ms (default 800) for output to settle. Output has terminal escape codes removed unless raw=true. Use bash for one-shot commands; use this when the program is interactive or long-running.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["start", "send", "key", "read", "list", "kill"] },
                "command": { "type": "string", "description": "start: shell command to run." },
                "cwd": { "type": "string", "description": "start: working directory." },
                "cols": { "type": "integer", "minimum": 20, "maximum": 400 },
                "rows": { "type": "integer", "minimum": 5, "maximum": 200 },
                "id": { "type": "integer", "description": "Session id from start." },
                "input": { "type": "string", "description": "send: text to type." },
                "enter": { "type": "boolean", "description": "send: press Enter after the text (default true)." },
                "key": { "type": "string" },
                "wait_ms": { "type": "integer", "minimum": 0, "maximum": 60000 },
                "raw": { "type": "boolean" }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::DangerFullAccess,
    }]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    (name == "Pty").then(|| {
        serde_json::from_value::<PtyInput>(input.clone())
            .map_err(|error| error.to_string())
            .and_then(run)
    })
}

#[derive(Deserialize)]
struct PtyInput {
    action: String,
    command: Option<String>,
    cwd: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    id: Option<u32>,
    input: Option<String>,
    enter: Option<bool>,
    key: Option<String>,
    wait_ms: Option<u64>,
    raw: Option<bool>,
}

struct Session {
    command: String,
    child: Child,
    stdin: ChildStdin,
    output: Arc<Mutex<Vec<u8>>>,
    /// How much of `output` was returned already.
    read: usize,
    started: Instant,
}

static SESSIONS: Mutex<BTreeMap<u32, Session>> = Mutex::new(BTreeMap::new());
static NEXT_ID: Mutex<u32> = Mutex::new(1);

fn sessions() -> std::sync::MutexGuard<'static, BTreeMap<u32, Session>> {
    SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn run(input: PtyInput) -> Result<String, String> {
    let wait = Duration::from_millis(input.wait_ms.unwrap_or(800));
    let raw = input.raw.unwrap_or(false);
    match input.action.as_str() {
        "start" => {
            let command = input
                .command
                .filter(|command| !command.trim().is_empty())
                .ok_or("start needs a command")?;
            let id = start(
                &command,
                input.cwd.as_deref(),
                input.cols.unwrap_or(120),
                input.rows.unwrap_or(40),
            )?;
            respond(id, wait, raw)
        }
        "send" => {
            let id = input.id.ok_or("send needs the session id")?;
            let mut text = input.input.unwrap_or_default();
            if input.enter.unwrap_or(true) {
                text.push('\r');
            }
            write_to(id, text.as_bytes())?;
            respond(id, wait, raw)
        }
        "key" => {
            let id = input.id.ok_or("key needs the session id")?;
            let key = input.key.ok_or("key needs a key name")?;
            write_to(id, key_bytes(&key)?)?;
            respond(id, wait, raw)
        }
        "read" => respond(input.id.ok_or("read needs the session id")?, wait, raw),
        "list" => {
            let mut sessions = sessions();
            let rows: Vec<Value> = sessions
                .iter_mut()
                .map(|(id, session)| {
                    json!({
                        "id": id,
                        "command": session.command,
                        "running": matches!(session.child.try_wait(), Ok(None)),
                        "seconds": session.started.elapsed().as_secs(),
                    })
                })
                .collect();
            to_pretty_json(json!({ "sessions": rows }))
        }
        "kill" => {
            let id = input.id.ok_or("kill needs the session id")?;
            let mut session = sessions()
                .remove(&id)
                .ok_or_else(|| format!("no pty session {id}"))?;
            let _ = session.child.kill();
            let _ = session.child.wait();
            to_pretty_json(json!({ "id": id, "killed": true }))
        }
        other => Err(format!("unknown action `{other}`")),
    }
}

fn start(command: &str, cwd: Option<&str>, cols: u16, rows: u16) -> Result<u32, String> {
    // `script` gives the command a terminal; stty sets its size, since our
    // pipe has none to copy. The command comes in through the environment
    // so it needs no quoting.
    let inner = format!("stty cols {cols} rows {rows} 2>/dev/null; eval \"$CLAW_PTY_COMMAND\"");
    let mut process = Command::new("script");
    process
        .args(["-qfec", &inner, "/dev/null"])
        .env("CLAW_PTY_COMMAND", command)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    let mut child = process
        .spawn()
        .map_err(|error| format!("could not start `script` (util-linux) for the pty: {error}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let output = Arc::new(Mutex::new(Vec::new()));
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let output = Arc::clone(&output);
        std::thread::spawn(move || pump(stream, &output));
    }
    let id = {
        let mut next = NEXT_ID
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = *next;
        *next += 1;
        id
    };
    sessions().insert(
        id,
        Session {
            command: command.to_string(),
            child,
            stdin,
            output,
            read: 0,
            started: Instant::now(),
        },
    );
    Ok(id)
}

fn pump(mut stream: Box<dyn Read + Send>, output: &Mutex<Vec<u8>>) {
    let mut buffer = [0_u8; 8192];
    while let Ok(count) = stream.read(&mut buffer) {
        if count == 0 {
            break;
        }
        let mut output = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        output.extend_from_slice(&buffer[..count]);
    }
}

fn write_to(id: u32, bytes: &[u8]) -> Result<(), String> {
    let mut sessions = sessions();
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| format!("no pty session {id}"))?;
    session
        .stdin
        .write_all(bytes)
        .and_then(|()| session.stdin.flush())
        .map_err(|error| format!("session {id} no longer takes input: {error}"))
}

fn key_bytes(key: &str) -> Result<&'static [u8], String> {
    Ok(match key.to_ascii_lowercase().replace('+', "-").as_str() {
        "ctrl-c" => b"\x03",
        "ctrl-d" => b"\x04",
        "ctrl-z" => b"\x1a",
        "ctrl-l" => b"\x0c",
        "ctrl-u" => b"\x15",
        "ctrl-r" => b"\x12",
        "esc" | "escape" => b"\x1b",
        "tab" => b"\t",
        "enter" | "return" => b"\r",
        "backspace" => b"\x7f",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        other => return Err(format!("unknown key `{other}`")),
    })
}

/// Waits until the output stops growing (or `wait` passes), then returns
/// what is new since the last read.
fn respond(id: u32, wait: Duration, raw: bool) -> Result<String, String> {
    let output = {
        let sessions = sessions();
        let session = sessions
            .get(&id)
            .ok_or_else(|| format!("no pty session {id}"))?;
        Arc::clone(&session.output)
    };
    let deadline = Instant::now() + wait;
    let mut last = usize::MAX;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        let len = output.lock().map_or(0, |output| output.len());
        if len == last {
            break;
        }
        last = len;
    }
    let mut sessions = sessions();
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| format!("no pty session {id}"))?;
    let (new, dropped) = {
        let mut buffer = session
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let new = buffer[session.read.min(buffer.len())..].to_vec();
        // Keep memory bounded: forget what was already returned.
        let mut dropped = false;
        if buffer.len() > MAX_BUFFER {
            buffer.clear();
            dropped = true;
        }
        (new, dropped)
    };
    session.read = if dropped { 0 } else { session.read + new.len() };
    let exit = match session.child.try_wait() {
        Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
        _ => None,
    };
    let mut text = String::from_utf8_lossy(&new).into_owned();
    if !raw {
        text = strip_ansi(&text);
    }
    let truncated = text.len() > MAX_RETURN;
    if truncated {
        let mut start = text.len() - MAX_RETURN;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text = text[start..].to_string();
    }
    if exit.is_some() {
        sessions.remove(&id);
    }
    to_pretty_json(json!({
        "id": id,
        "output": text,
        "truncated": truncated,
        "running": exit.is_none(),
        "exit_code": exit,
    }))
}

/// Removes terminal escape sequences and carriage returns, so the model
/// reads the text a person would see.
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    // CSI: parameters, then one final byte in @..~.
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: up to BEL or ESC \.
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('(' | ')') => {
                    chars.next();
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\x07' | '\x08' => {}
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    fn call(input: Value) -> Value {
        let text = super::execute("Pty", &input).expect("Pty").expect("ok");
        serde_json::from_str(&text).expect("json")
    }

    #[test]
    fn strips_escape_codes() {
        assert_eq!(
            super::strip_ansi("\x1b[1;32mok\x1b[0m\r\n\x1b]0;title\x07>>> "),
            "ok\n>>> "
        );
    }

    #[test]
    fn talks_to_an_interactive_program() {
        if std::process::Command::new("script")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let started = call(json!({
            "action": "start",
            "command": "read -r -p 'name? ' name; echo \"hi $name\"; read -r _",
            "wait_ms": 1500
        }));
        let id = started["id"].as_u64().unwrap();
        assert!(
            started["output"].as_str().unwrap().contains("name?"),
            "{started}"
        );
        let reply = call(json!({"action": "send", "id": id, "input": "claw", "wait_ms": 1500}));
        assert!(
            reply["output"].as_str().unwrap().contains("hi claw"),
            "{reply}"
        );
        let killed = call(json!({"action": "kill", "id": id}));
        assert_eq!(killed["killed"], true);
    }
}
