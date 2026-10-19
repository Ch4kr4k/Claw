//! The app's own state on disk (`~/.claw/desktop.json`): projects, the
//! folders known to hold sessions, and small preferences. Also reads the
//! model list from claw's settings and the folders claw itself recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sessions::config_home;

/// A named working folder with its own instructions, like a Claude project.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
}

/// A prompt the app runs on a schedule, in a new chat, while it is open.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTask {
    pub name: String,
    /// Five-field cron expression (`0 9 * * *`) or `@daily`, `@hourly`, ...
    pub schedule: String,
    pub prompt: String,
    pub folder: PathBuf,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// When it last ran, in milliseconds since the epoch.
    #[serde(default)]
    pub last_run_ms: Option<u64>,
}

fn yes() -> bool {
    true
}

fn default_text_size() -> u16 {
    15
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub projects: Vec<Project>,
    /// Folders that hold (or held) claw sessions.
    #[serde(default)]
    pub workspaces: BTreeSet<PathBuf>,
    /// The folder new chats start in.
    #[serde(default)]
    pub last_cwd: Option<PathBuf>,
    #[serde(default)]
    pub light_theme: bool,
    /// Whether the home folder was scanned for terminal sessions yet.
    #[serde(default)]
    pub scanned_home: bool,
    /// Titles the user gave chats, by session file.
    #[serde(default)]
    pub titles: BTreeMap<PathBuf, String>,
    /// The folder new chats start in (`~/ws/tmp` unless changed).
    #[serde(default)]
    pub default_cwd: Option<PathBuf>,
    /// Sessions started in this app, as opposed to the terminal.
    #[serde(default)]
    pub app_sessions: BTreeSet<PathBuf>,
    #[serde(default)]
    pub sidebar_hidden: bool,
    /// Desktop notification when a reply finishes while the window is not
    /// in front.
    #[serde(default = "yes")]
    pub notify_done: bool,
    /// Show the model's thinking expanded.
    #[serde(default)]
    pub show_thinking: bool,
    #[serde(default = "default_text_size")]
    pub text_size: u16,
    #[serde(default)]
    pub tasks: Vec<ScheduledTask>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            workspaces: BTreeSet::new(),
            last_cwd: None,
            light_theme: false,
            scanned_home: false,
            titles: BTreeMap::new(),
            default_cwd: None,
            app_sessions: BTreeSet::new(),
            sidebar_hidden: false,
            notify_done: true,
            show_thinking: false,
            text_size: default_text_size(),
            tasks: Vec::new(),
        }
    }
}

fn store_path() -> PathBuf {
    config_home().join("desktop.json")
}

impl Store {
    pub fn load() -> Self {
        let mut store: Self = fs::read_to_string(store_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        store.workspaces.extend(claw_recorded_workspaces());
        for project in &store.projects {
            store.workspaces.insert(project.path.clone());
        }
        store
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let path = store_path();
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            let _ = fs::write(path, text + "\n");
        }
    }

    /// Where new chats start: the chosen default folder, else `~/ws/tmp`
    /// when it exists, else home.
    pub fn new_chat_folder(&self) -> PathBuf {
        if let Some(folder) = self.default_cwd.as_ref().filter(|path| path.is_dir()) {
            return folder.clone();
        }
        let scratch = crate::sessions::home_dir().join("ws").join("tmp");
        if scratch.is_dir() {
            scratch
        } else {
            crate::sessions::home_dir()
        }
    }

    /// The project a folder belongs to (the deepest project containing it).
    pub fn project_for(&self, folder: &Path) -> Option<&Project> {
        self.projects
            .iter()
            .filter(|project| folder.starts_with(&project.path))
            .max_by_key(|project| project.path.components().count())
    }
}

/// Folders claw recorded when it started a session there
/// (`~/.claw/workspaces.txt`, one path per line).
fn claw_recorded_workspaces() -> Vec<PathBuf> {
    fs::read_to_string(config_home().join("workspaces.txt"))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// The project's instructions file, which claw loads into every session
/// started in that folder (terminal ones too).
pub fn instructions_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".claw").join("instructions.md")
}

pub fn read_instructions(project_dir: &Path) -> String {
    fs::read_to_string(instructions_path(project_dir)).unwrap_or_default()
}

pub fn write_instructions(project_dir: &Path, text: &str) -> std::io::Result<()> {
    let path = instructions_path(project_dir);
    if text.trim().is_empty() {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text.trim_end().to_string() + "\n")
}

/// Models to offer: what the local llama.cpp server serves (as
/// `local/<id>`), claw's aliases shown with the model each points to
/// (`coder → local/qwen36-35b-a3b`), and the Anthropic models only when
/// there is a key or login for them. Aliases to local models the server no
/// longer has are left out. [`model_id`] turns a choice back into the id.
pub fn model_choices() -> (Option<String>, Vec<String>) {
    let settings: Value = fs::read_to_string(config_home().join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let default = settings
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let served = local_models();
    let available = |model: &str| match (model.strip_prefix("local/"), &served) {
        (Some(id), Some(served)) => served.iter().any(|name| name == id),
        _ => true,
    };
    let mut choices = Vec::new();
    if let Some(model) = &default {
        choices.push(model.clone());
    }
    if let Some(served) = &served {
        choices.extend(served.iter().map(|id| format!("local/{id}")));
    }
    if let Some(aliases) = settings.get("aliases").and_then(Value::as_object) {
        for (alias, target) in aliases {
            match target.as_str() {
                Some(target) if available(target) => {
                    choices.push(format!("{alias} → {target}"));
                }
                Some(_) => {}
                None => choices.push(alias.clone()),
            }
        }
    }
    if anthropic_configured() {
        choices.extend(["sonnet", "opus", "haiku"].into_iter().map(str::to_string));
    }
    let mut seen = BTreeSet::new();
    choices.retain(|choice| seen.insert(choice.clone()));
    (default, choices)
}

/// The model id behind a choice from [`model_choices`]: an alias shown as
/// `coder → local/x` is selected as `coder`.
pub fn model_id(choice: &str) -> String {
    choice
        .split(" → ")
        .next()
        .unwrap_or(choice)
        .trim()
        .to_string()
}

/// Whether claw can reach Anthropic: an API key in the environment or a
/// saved `claw login`.
fn anthropic_configured() -> bool {
    ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
        || runtime::load_oauth_credentials().ok().flatten().is_some()
}

/// Model ids the local OpenAI-compatible server lists at `GET /models`
/// (`CLAW_LOCAL_BASE_URL`, default llama-gpu's `http://127.0.0.1:8080/v1`).
/// `None` when it does not answer within a moment.
fn local_models() -> Option<Vec<String>> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let base = std::env::var("CLAW_LOCAL_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080/v1".to_string());
    let rest = base.strip_prefix("http://")?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let address = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    let address = address.to_socket_addrs().ok()?.next()?;
    let timeout = Duration::from_millis(700);
    let mut stream = TcpStream::connect_timeout(&address, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    let path = format!("/{}/models", path.trim_end_matches('/')).replace("//", "/");
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nAccept: application/json\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (_, body) = response.split_once("\r\n\r\n")?;
    let body: Value = serde_json::from_str(body).ok()?;
    Some(
        body.get("data")?
            .as_array()?
            .iter()
            .filter_map(|model| model.get("id")?.as_str().map(str::to_string))
            .collect(),
    )
}
