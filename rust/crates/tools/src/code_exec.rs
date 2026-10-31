//! `ExecuteCode`: the model writes one Python script that calls claw's
//! read-only tools as functions, instead of making the calls one step at a
//! time. Ten searches, reads and filters that cost ten model steps (each one
//! a full request, slow on a local model) become one, and only what the
//! script prints comes back to the model. Ported from hermes-agent's
//! programmatic tool calling.
//!
//! The script runs like a bash command (sandboxed, Esc stops it, with a
//! timeout), and so is approved like one. Its tool calls come back to claw
//! over a Unix socket in a temporary folder in the working directory (the
//! one place the sandbox can reach), and only the read-only tools in
//! [`RPC_TOOLS`] can be called: anything that changes files the script can
//! do itself, inside the sandbox, as bash would.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use runtime::{execute_bash, BashCommandInput, PermissionMode};
use serde::Deserialize;
use serde_json::{json, Value};

use super::ToolSpec;

/// Tools the script may call, and the Python function for each.
const RPC_TOOLS: [(&str, &str); 12] = [
    ("read_file", "read_file"),
    ("glob_search", "glob_search"),
    ("grep_search", "grep_search"),
    ("CodeSearch", "code_search"),
    ("WebSearch", "web_search"),
    ("WebFetch", "web_fetch"),
    ("GitStatus", "git_status"),
    ("GitDiff", "git_diff"),
    ("GitLog", "git_log"),
    ("GitShow", "git_show"),
    ("GitBlame", "git_blame"),
    ("SessionSearch", "session_search"),
];
const MAX_TOOL_CALLS: usize = 100;
const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MAX_OUTPUT_CHARS: usize = 30_000;

pub(crate) fn specs() -> Vec<ToolSpec> {
    let functions = RPC_TOOLS
        .iter()
        .map(|(_, function)| *function)
        .collect::<Vec<_>>()
        .join(", ");
    let description = format!(
        "Run a Python 3 script that calls claw's read-only tools as functions, to do many \
         searches, reads and filters in one step instead of one tool call each. Only what the \
         script prints comes back to you, so print a compact summary, not whole files. In the \
         script, call {functions} (already imported). Each takes the same \
         arguments as the tool, as keyword arguments (read_file(path='src/main.rs', limit=50), \
         grep_search(pattern='fn main', path='src')), and returns the tool's result, parsed \
         from JSON when it is JSON. A failed call raises ToolError. At most {MAX_TOOL_CALLS} \
         tool calls per script. agent(prompt, subagent_type=None, description=None) runs a \
         sub-agent (Explore, Plan, Verification, fork, or general-purpose when omitted) and \
         returns its report; call it from several threads (concurrent.futures) to run sub-agents \
         in parallel, e.g. one per module, then combine their reports in the script. The script runs in the working directory like a bash command \
         (same sandbox): it can also read files, run subprocesses and write files itself. Use \
         it for multi-step lookups (find every caller, then read each); use bash for single \
         commands."
    );
    vec![ToolSpec {
        name: "ExecuteCode",
        description: Box::leak(description.into_boxed_str()),
        input_schema: json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "The Python 3 script" },
                "timeout": { "type": "integer", "minimum": 1, "description": "Seconds (default 300)" }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::DangerFullAccess,
    }]
}

#[derive(Debug, Deserialize)]
struct ExecuteCodeInput {
    code: String,
    timeout: Option<u64>,
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    (name == "ExecuteCode").then(|| {
        let input: ExecuteCodeInput =
            serde_json::from_value(input.clone()).map_err(|error| error.to_string())?;
        run(&input)
    })
}

fn helper_module() -> String {
    let mut module = String::from(
        r#""""claw's tools, callable from an ExecuteCode script."""
import json as _json
import os as _os
import socket as _socket


class ToolError(Exception):
    pass


def call(tool, **arguments):
    """Calls a claw tool by name; returns its result (parsed if JSON)."""
    sock = _socket.socket(_socket.AF_UNIX, _socket.SOCK_STREAM)
    # Socket paths are limited to 107 bytes: connect from inside its folder.
    here = _os.getcwd()
    _os.chdir(_os.environ["CLAW_RPC_DIR"])
    try:
        sock.connect("rpc.sock")
    finally:
        _os.chdir(here)
    try:
        sock.sendall((_json.dumps({"tool": tool, "input": arguments}) + "\n").encode())
        data = b""
        while not data.endswith(b"\n"):
            chunk = sock.recv(65536)
            if not chunk:
                break
            data += chunk
    finally:
        sock.close()
    reply = _json.loads(data.decode() or '{"ok": false, "error": "no reply"}')
    if not reply.get("ok"):
        raise ToolError(reply.get("error", "tool call failed"))
    result = reply.get("result", "")
    try:
        return _json.loads(result)
    except (ValueError, TypeError):
        return result

"#,
    );
    module.push_str(
        r#"
# Runs a sub-agent on `prompt` and returns its final report (text).
def agent(prompt, subagent_type=None, description=None, model=None):
    arguments = {"prompt": prompt, "description": description or prompt[:40]}
    if subagent_type:
        arguments["subagent_type"] = subagent_type
    if model:
        arguments["model"] = model
    result = call("Agent", **arguments)
    if isinstance(result, dict):
        return result.get("result") or result.get("error") or result
    return result
"#,
    );
    for (tool, function) in RPC_TOOLS {
        module.push_str(&format!(
            "\ndef {function}(**arguments):\n    return call({tool:?}, **arguments)\n"
        ));
    }
    module.push_str(&format!(
        "\n__all__ = ['call', 'ToolError', 'agent', {}]\n",
        RPC_TOOLS
            .iter()
            .map(|(_, function)| format!("'{function}'"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    module
}

/// A folder for the socket, the helper and the script, removed on drop.
struct RunDir(PathBuf);

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(input: &ExecuteCodeInput) -> Result<String, String> {
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let dir = RunDir(cwd.join(format!(
        ".claw-exec-{}-{}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir_all(&dir.0).map_err(|error| error.to_string())?;
    std::fs::write(dir.0.join("claw_tools.py"), helper_module())
        .map_err(|error| error.to_string())?;
    std::fs::write(dir.0.join("script.py"), &input.code).map_err(|error| error.to_string())?;
    // The tools are preloaded, so a script that forgets the import still
    // works; compiled from its own file, its tracebacks keep its lines.
    std::fs::write(
        dir.0.join("run.py"),
        "import sys as _sys\nfrom claw_tools import *\n_path = _sys.argv[1]\n\
         __file__ = _path\nexec(compile(open(_path).read(), _path, 'exec'))\n",
    )
    .map_err(|error| error.to_string())?;
    // A socket path may be at most 107 bytes, which a deep working
    // directory exceeds; both ends use a path relative to their working
    // directory instead (claw's is `cwd`, the helper changes into the
    // folder to connect).
    let folder = dir.0.file_name().map(PathBuf::from).unwrap_or_default();
    let listener =
        UnixListener::bind(folder.join("rpc.sock")).map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;

    let done = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let server = {
        let (done, calls) = (Arc::clone(&done), Arc::clone(&calls));
        std::thread::spawn(move || serve(&listener, &done, &calls))
    };
    let timeout = input.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1);
    let command = format!(
        "cd {} && CLAW_RPC_DIR={} PYTHONPATH={}${{PYTHONPATH:+:$PYTHONPATH}} PYTHONDONTWRITEBYTECODE=1 python3 {} {}",
        quote(&cwd),
        quote(&dir.0),
        quote(&dir.0),
        quote(&dir.0.join("run.py")),
        quote(&dir.0.join("script.py"))
    );
    let output = execute_bash(BashCommandInput {
        command,
        timeout: Some(timeout * 1000),
        description: Some("ExecuteCode".to_string()),
        run_in_background: None,
        dangerously_disable_sandbox: None,
        namespace_restrictions: None,
        isolate_network: None,
        filesystem_mode: None,
        allowed_mounts: None,
    });
    done.store(true, Ordering::Relaxed);
    let _ = server.join();
    let output = output.map_err(|error| error.to_string())?;

    let mut text = output.stdout.trim_end().to_string();
    let stderr = output.stderr.trim_end();
    if !stderr.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(stderr);
    }
    let text = runtime::tool_spill::spill_long(&text, MAX_OUTPUT_CHARS, "execute-code");
    let status = output
        .return_code_interpretation
        .unwrap_or_else(|| "exit code 0".to_string());
    let interrupted = if output.interrupted {
        ", interrupted"
    } else {
        ""
    };
    Ok(format!(
        "{}\n[{status}{interrupted}; {} tool call(s)]",
        if text.is_empty() {
            "(no output)"
        } else {
            &text
        },
        calls.load(Ordering::Relaxed)
    ))
}

fn serve(listener: &UnixListener, done: &AtomicBool, calls: &Arc<AtomicUsize>) {
    while !done.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                // Each call on its own thread, so a script's parallel
                // sub-agents run at the same time.
                let calls = Arc::clone(calls);
                std::thread::spawn(move || {
                    let _ = stream.set_nonblocking(false);
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let reply = answer(&line, &calls);
                    let mut stream = &stream;
                    let _ = stream.write_all(format!("{reply}\n").as_bytes());
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

fn answer(line: &str, calls: &AtomicUsize) -> Value {
    let request = match serde_json::from_str::<Value>(line) {
        Ok(request) => request,
        Err(error) => return json!({"ok": false, "error": format!("bad request: {error}")}),
    };
    let tool = request["tool"].as_str().unwrap_or_default();
    if tool != "Agent" && !RPC_TOOLS.iter().any(|(name, _)| *name == tool) {
        return json!({"ok": false, "error": format!("{tool} cannot be called from a script")});
    }
    if calls.fetch_add(1, Ordering::Relaxed) >= MAX_TOOL_CALLS {
        return json!({"ok": false, "error": format!("more than {MAX_TOOL_CALLS} tool calls")});
    }
    let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    match super::execute_tool(tool, &input) {
        Ok(result) => json!({"ok": true, "result": result}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{answer, helper_module, RPC_TOOLS};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn the_helper_defines_a_function_per_tool() {
        let module = helper_module();
        for (tool, function) in RPC_TOOLS {
            assert!(module.contains(&format!(
                "def {function}(**arguments):\n    return call(\"{tool}\""
            )));
        }
    }

    #[test]
    fn a_script_calls_tools_and_only_its_output_comes_back() {
        let input = super::ExecuteCodeInput {
            code: "r = glob_search(pattern='Cargo.toml')\nprint('found', type(r).__name__)\n\
                   try:\n    call('write_file', path='x', content='y')\n\
                   except ToolError as e:\n    print('refused:', e)\n"
                .to_string(),
            timeout: Some(60),
        };
        let output = super::run(&input).expect("script runs");
        assert!(output.contains("found dict"), "{output}");
        assert!(
            output.contains("refused: write_file cannot be called"),
            "{output}"
        );
        assert!(output.contains("1 tool call(s)"), "{output}");
        let leftovers = std::fs::read_dir(".").unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".claw-exec-")
        });
        assert!(!leftovers, "the run folder is removed");
    }

    #[test]
    fn only_read_only_tools_can_be_called() {
        let calls = AtomicUsize::new(0);
        let reply = answer(
            r#"{"tool":"write_file","input":{"path":"x","content":"y"}}"#,
            &calls,
        );
        assert_eq!(reply["ok"], false);
        assert!(reply["error"]
            .as_str()
            .unwrap()
            .contains("cannot be called"));
        let reply = answer(
            r#"{"tool":"glob_search","input":{"pattern":"Cargo.toml"}}"#,
            &calls,
        );
        assert_eq!(reply["ok"], true, "{reply}");
    }
}
