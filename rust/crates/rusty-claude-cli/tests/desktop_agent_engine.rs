//! `claw desktop-agent` against the mock Anthropic service: the app's event
//! stream for text, a two-call batch, and bash approved and denied.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::{json, Value};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Agent {
    child: Child,
    stdin: ChildStdin,
    events: mpsc::Receiver<Value>,
}

impl Agent {
    fn start(root: &Path, base_url: &str, mode: &str, tools: &str) -> Self {
        let config_home = root.join("config-home");
        let home = root.join("home");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(
            config_home.join("settings.json"),
            r#"{"permissions":{"bashApproval":"always"}}"#,
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_claw"))
            .current_dir(root)
            .env_clear()
            .env("ANTHROPIC_API_KEY", "test-desktop-key")
            .env("ANTHROPIC_BASE_URL", base_url)
            .env("CLAW_CONFIG_HOME", &config_home)
            .env("HOME", &home)
            .env("NO_COLOR", "1")
            .env("PATH", "/usr/bin:/bin")
            .args([
                "--model",
                "sonnet",
                "--permission-mode",
                mode,
                "--allowedTools",
                tools,
                "desktop-agent",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("claw desktop-agent should launch");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Lines that are not JSON events are claw's own notices.
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if value.get("ev").is_some() && tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        let mut agent = Self {
            child,
            stdin,
            events,
        };
        agent.until("ready");
        agent
    }

    fn send(&mut self, command: &Value) {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Events up to and including the first `ev` == `last`.
    fn until(&mut self, last: &str) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let event = self
                .events
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no `{last}` event; saw {seen:#?}"));
            let done = event["ev"] == last;
            seen.push(event);
            if done {
                return seen;
            }
        }
    }

    /// One prompt's turn, permission requests answered with `decision`.
    fn turn(&mut self, scenario: &str, decision: &str) -> Vec<Value> {
        self.send(&json!({"cmd": "prompt", "text": format!("{SCENARIO_PREFIX}{scenario}")}));
        let mut seen = Vec::new();
        loop {
            let event = self
                .events
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no turn_end; saw {seen:#?}"));
            if event["ev"] == "permission" {
                self.send(&json!({"cmd": "permission", "id": event["id"], "decision": decision}));
            }
            let done = event["ev"] == "turn_end";
            seen.push(event);
            if done {
                return seen;
            }
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn of(events: &[Value], ev: &str) -> Vec<Value> {
    events.iter().filter(|e| e["ev"] == ev).cloned().collect()
}

fn text(events: &[Value]) -> String {
    of(events, "text")
        .iter()
        .filter_map(|e| e["text"].as_str())
        .collect()
}

fn names(events: &[Value], ev: &str) -> Vec<String> {
    of(events, ev)
        .iter()
        .map(|e| e["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("claw-desktop-{label}-{nanos}-{n}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Saved session lines of `root`'s agent: the messages' roles.
fn saved_roles(root: &Path) -> Vec<String> {
    let mut roles = Vec::new();
    for entry in walk(root) {
        if entry.extension().is_some_and(|ext| ext == "jsonl") {
            for line in fs::read_to_string(&entry).unwrap_or_default().lines() {
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    if let Some(role) = value["message"]["role"].as_str() {
                        roles.push(role.to_string());
                    }
                }
            }
        }
    }
    roles
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn desktop_agent_streams_turn_events_to_the_app() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockAnthropicService::spawn()).unwrap();
    let base_url = server.base_url();

    {
        let engine = "engine";
        // Plain streamed text.
        let root = temp_dir("text");
        let mut agent = Agent::start(&root, &base_url, "read-only", "read_file");
        let events = agent.turn("streaming_text", "deny");
        assert_eq!(of(&events, "turn_start").len(), 1, "{engine}");
        assert_eq!(of(&events, "turn_end")[0]["status"], "done", "{engine}");
        assert!(!text(&events).is_empty(), "{engine}: {events:#?}");
        assert_eq!(of(&events, "usage").len(), 1, "{engine}");
        drop(agent);

        // Two calls in one reply (a batch on the engine): each is listed
        // and gets its result, once.
        let root = temp_dir("multi");
        fs::write(
            root.join("fixture.txt"),
            "alpha parity line\nbeta line\ngamma parity line\n",
        )
        .unwrap();
        let mut agent = Agent::start(&root, &base_url, "read-only", "read_file,grep_search");
        let events = agent.turn("multi_tool_turn_roundtrip", "deny");
        assert_eq!(
            names(&events, "tool_call"),
            ["read_file", "grep_search"],
            "{engine}: {events:#?}"
        );
        let mut results = names(&events, "tool_result");
        results.sort();
        assert_eq!(results, ["grep_search", "read_file"], "{engine}");
        assert!(text(&events).contains("2 occurrences"), "{engine}");
        drop(agent);

        // A bash call the app approves, then one it denies.
        for (scenario, decision, expect) in [
            (
                "bash_permission_prompt_approved",
                "allow",
                "approved via prompt",
            ),
            ("bash_permission_prompt_denied", "deny", "declined"),
        ] {
            let root = temp_dir("bash");
            let mut agent = Agent::start(&root, &base_url, "workspace-write", "bash");
            let events = agent.turn(scenario, decision);
            assert_eq!(of(&events, "permission").len(), 1, "{engine} {scenario}");
            assert_eq!(names(&events, "tool_call"), ["bash"], "{engine} {scenario}");
            let results = of(&events, "tool_result");
            assert_eq!(results.len(), 1, "{engine} {scenario}: {events:#?}");
            assert!(
                results[0]["output"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(expect),
                "{engine} {scenario}: {results:#?}"
            );
            assert_eq!(
                results[0]["is_error"],
                decision == "deny",
                "{engine} {scenario}"
            );
            assert_eq!(of(&events, "turn_end")[0]["status"], "done");
            drop(agent);
            // The turn is saved: prompt, call, result, answer.
            let roles = saved_roles(&root);
            assert!(roles.len() >= 4, "{engine} {scenario}: saved {roles:?}");
        }
    }
}
