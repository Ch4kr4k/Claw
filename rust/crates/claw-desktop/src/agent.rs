//! One `claw desktop-agent` process per open chat. Its JSON-line events are
//! forwarded to the UI through a single subscription; commands are written
//! to its stdin.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use iced::futures::SinkExt;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// An event from the agent of chat `chat`.
#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub chat: u64,
    pub event: Value,
}

struct Bus {
    sender: UnboundedSender<AgentEvent>,
    receiver: Mutex<Option<UnboundedReceiver<AgentEvent>>>,
}

fn bus() -> &'static Bus {
    static BUS: OnceLock<Bus> = OnceLock::new();
    BUS.get_or_init(|| {
        let (sender, receiver) = unbounded_channel();
        Bus {
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    })
}

/// The stream behind the app's subscription: every agent event, in order.
pub fn events() -> impl iced::futures::Stream<Item = AgentEvent> {
    iced::stream::channel(100, async |mut output| {
        let receiver = bus()
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(mut receiver) = receiver else {
            return;
        };
        while let Some(event) = receiver.recv().await {
            if output.send(event).await.is_err() {
                break;
            }
        }
    })
}

/// The claw launcher to run: `$CLAW_DESKTOP_CLAW`, else `~/.local/bin/claw`
/// (the wrapper that points `local/` models at llama-gpu), else `claw`.
fn claw_program() -> PathBuf {
    if let Some(program) = std::env::var_os("CLAW_DESKTOP_CLAW") {
        return PathBuf::from(program);
    }
    let wrapper = crate::sessions::home_dir().join(".local/bin/claw");
    if wrapper.is_file() {
        wrapper
    } else {
        PathBuf::from("claw")
    }
}

pub struct Agent {
    child: Child,
    stdin: ChildStdin,
}

impl Agent {
    /// Starts an agent in `cwd`, continuing `resume` if given. An incognito
    /// agent saves nothing.
    pub fn spawn(
        chat: u64,
        cwd: &Path,
        resume: Option<&Path>,
        model: Option<&str>,
        incognito: bool,
    ) -> std::io::Result<Self> {
        let mut command = Command::new(claw_program());
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        command.arg("desktop-agent");
        if let Some(path) = resume {
            command.arg("--resume").arg(path);
        }
        if incognito {
            command.arg("--incognito");
        }
        let log = crate::sessions::config_home().join("desktop-agent.log");
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_or_else(|_| Stdio::null(), Stdio::from);
        let mut child = command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let sender = bus().sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // claw's own notices are plain text; events are JSON objects.
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if event.get("ev").is_none() {
                    continue;
                }
                if sender.send(AgentEvent { chat, event }).is_err() {
                    return;
                }
            }
            let _ = sender.send(AgentEvent {
                chat,
                event: json!({"ev": "exit"}),
            });
        });
        Ok(Self { child, stdin })
    }

    fn send(&mut self, command: &Value) {
        let _ = writeln!(self.stdin, "{command}");
        let _ = self.stdin.flush();
    }

    pub fn prompt(&mut self, text: &str) {
        self.send(&json!({"cmd": "prompt", "text": text}));
    }

    pub fn cancel(&mut self) {
        self.send(&json!({"cmd": "cancel"}));
    }

    pub fn permission(&mut self, id: u64, decision: &str) {
        self.send(&json!({"cmd": "permission", "id": id, "decision": decision}));
    }

    pub fn answer(&mut self, id: u64, text: &str) {
        self.send(&json!({"cmd": "answer", "id": id, "text": text}));
    }

    /// Drops the conversation from the `prompt`-th typed prompt (0-based)
    /// on, before an edited version of it is sent.
    pub fn rewind(&mut self, prompt: usize) {
        self.send(&json!({"cmd": "rewind", "prompt": prompt}));
    }

    /// Stops a background shell or monitor by id, or all of them.
    pub fn stop_background(&mut self, id: Option<&str>) {
        self.send(&json!({"cmd": "stop_background", "id": id}));
    }

    /// Asks the agent to end (it finishes when its stdin closes too).
    pub fn shutdown(&mut self) {
        self.send(&json!({"cmd": "shutdown"}));
    }

    pub fn set_model(&mut self, model: &str) {
        self.send(&json!({"cmd": "model", "model": model}));
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // Ask it to finish, so it lets go of the model server it started
        // (which stops when no claw uses it); kill it if it lingers.
        self.send(&json!({"cmd": "shutdown"}));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
