//! Background sessions: `/background` hands the current conversation to a
//! separate claw process that keeps working on it, and this terminal starts
//! a fresh one. `/list-agents` shows them (with background shells), `/stop`
//! ends one, and `/resume <session>` opens one when it is done.
//!
//! The process is `claw -p <prompt>` with `CLAW_BACKGROUND_SESSION` naming
//! the session file to continue and `CLAW_BACKGROUND_ID` its entry under
//! `~/.claw/background/`. It runs in its own process group, so Ctrl+C here
//! does not reach it, with output in a log file next to the entry. Without
//! a terminal it cannot ask for permission, so tool calls that would ask are
//! denied.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Names the session file a background process continues.
pub(crate) const SESSION_ENV: &str = "CLAW_BACKGROUND_SESSION";
/// Names the background entry a process reports to.
pub(crate) const ID_ENV: &str = "CLAW_BACKGROUND_ID";

/// The prompt used when `/background` gets none.
pub(crate) const CONTINUE_PROMPT: &str = "You are now running as a background session: the user moved this conversation to the background and is not watching. Continue the task you were working on and finish it. Nobody can answer questions or approve actions, so work with what you are allowed to do, and end with a short summary of what you did and anything left open.";

fn dir() -> PathBuf {
    runtime::default_config_home().join("background")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

/// A background session's entry.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) pid: u32,
    pub(crate) session_id: String,
    pub(crate) prompt: String,
    pub(crate) cwd: String,
    pub(crate) log: PathBuf,
    pub(crate) started: u64,
    /// `running`, `done`, `failed` or `stopped`.
    pub(crate) status: String,
    pub(crate) notified: bool,
}

impl Entry {
    fn path(id: &str) -> PathBuf {
        dir().join(format!("{id}.json"))
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "pid": self.pid,
            "session_id": self.session_id,
            "prompt": self.prompt,
            "cwd": self.cwd,
            "log": self.log.display().to_string(),
            "started": self.started,
            "status": self.status,
            "notified": self.notified,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            id: text("id")?,
            pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
            session_id: text("session_id").unwrap_or_default(),
            prompt: text("prompt").unwrap_or_default(),
            cwd: text("cwd").unwrap_or_default(),
            log: PathBuf::from(text("log").unwrap_or_default()),
            started: value.get("started").and_then(Value::as_u64).unwrap_or(0),
            status: text("status").unwrap_or_else(|| "running".to_string()),
            notified: value
                .get("notified")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn save(&self) -> std::io::Result<()> {
        fs::create_dir_all(dir())?;
        fs::write(
            Self::path(&self.id),
            serde_json::to_string_pretty(&self.to_json()).unwrap_or_default(),
        )
    }

    /// The status, noticing a process that went away without reporting.
    fn refreshed(mut self) -> Self {
        if self.status == "running" && !process_alive(self.pid) {
            self.status = "failed".to_string();
            let _ = self.save();
        }
        self
    }
}

fn process_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        // A zombie has exited: it only waits to be collected.
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z")
        })
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

/// All entries, newest first.
pub(crate) fn entries() -> Vec<Entry> {
    let Ok(read) = fs::read_dir(dir()) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| fs::read_to_string(path).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter_map(|value| Entry::from_json(&value))
        .map(Entry::refreshed)
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.started));
    entries
}

/// What starting a background session needs from the current one.
pub(crate) struct Launch<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) session_path: &'a Path,
    pub(crate) prompt: &'a str,
    pub(crate) model: &'a str,
    pub(crate) permission_mode: &'a str,
    pub(crate) allowed_tools: Option<String>,
}

/// Starts a background claw process continuing the session.
pub(crate) fn launch(launch: &Launch<'_>) -> Result<Entry, Box<dyn std::error::Error>> {
    let id = format!("bg-{}", now() % 1_000_000);
    fs::create_dir_all(dir())?;
    let log = dir().join(format!("{id}.log"));
    let log_file = fs::File::create(&log)?;
    let mut command = Command::new(std::env::current_exe()?);
    command.args([
        "--allow-broad-cwd",
        "--model",
        launch.model,
        "--permission-mode",
        launch.permission_mode,
    ]);
    if let Some(tools) = &launch.allowed_tools {
        command.args(["--allowedTools", tools]);
    }
    command
        .args(["-p", launch.prompt])
        .env(SESSION_ENV, launch.session_path)
        .env(ID_ENV, &id)
        .env("CLAW_FULLSCREEN", "0")
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn()?;
    let entry = Entry {
        id,
        pid: child.id(),
        session_id: launch.session_id.to_string(),
        prompt: launch.prompt.chars().take(200).collect(),
        cwd: std::env::current_dir()?.display().to_string(),
        log,
        started: now(),
        status: "running".to_string(),
        notified: false,
    };
    entry.save()?;
    // Collect the process when it exits so it does not linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(entry)
}

/// Called by the background process when its turn ends.
pub(crate) fn report_finished(ok: bool) {
    let Ok(id) = std::env::var(ID_ENV) else {
        return;
    };
    let Some(mut entry) = entries().into_iter().find(|entry| entry.id == id) else {
        return;
    };
    if entry.status == "running" {
        entry.status = if ok { "done" } else { "failed" }.to_string();
        let _ = entry.save();
    }
}

/// Finished sessions not announced yet; marks them announced.
pub(crate) fn take_finished() -> Vec<Entry> {
    entries()
        .into_iter()
        .filter(|entry| entry.status != "running" && !entry.notified)
        .map(|mut entry| {
            entry.notified = true;
            let _ = entry.save();
            entry
        })
        .collect()
}

/// Stops a running background session (its whole process group).
pub(crate) fn stop(id: &str) -> Result<Entry, String> {
    let mut entry = entries()
        .into_iter()
        .find(|entry| entry.id == id || entry.session_id == id)
        .ok_or_else(|| format!("no background session `{id}`"))?;
    if entry.status != "running" {
        return Err(format!("{} is already {}", entry.id, entry.status));
    }
    let signal = Command::new("kill")
        .args(["-TERM", &format!("-{}", entry.pid)])
        .stderr(Stdio::null())
        .status();
    if !signal.is_ok_and(|status| status.success()) {
        let _ = Command::new("kill")
            .args(["-TERM", &entry.pid.to_string()])
            .stderr(Stdio::null())
            .status();
    }
    entry.status = "stopped".to_string();
    entry.notified = true;
    entry.save().map_err(|error| error.to_string())?;
    Ok(entry)
}

fn age(started: u64) -> String {
    let seconds = now().saturating_sub(started);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        _ => format!("{}h", seconds / 3_600),
    }
}

/// The `/list-agents` lines for background sessions.
pub(crate) fn render(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .take(20)
        .map(|entry| {
            let prompt: String = entry
                .prompt
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(50)
                .collect();
            format!(
                "  {:<10} {:<8} {:>4}  session {}  {prompt}",
                entry.id,
                entry.status,
                age(entry.started),
                entry.session_id
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{process_alive, Entry};
    use std::path::PathBuf;

    #[test]
    fn entries_round_trip_through_json() {
        let entry = Entry {
            id: "bg-1".to_string(),
            pid: 42,
            session_id: "s-1".to_string(),
            prompt: "finish it".to_string(),
            cwd: "/tmp".to_string(),
            log: PathBuf::from("/tmp/bg-1.log"),
            started: 7,
            status: "running".to_string(),
            notified: false,
        };
        let back = Entry::from_json(&entry.to_json()).expect("entry");
        assert_eq!(back.id, "bg-1");
        assert_eq!(back.pid, 42);
        assert_eq!(back.session_id, "s-1");
        assert_eq!(back.log, PathBuf::from("/tmp/bg-1.log"));
        assert_eq!(back.status, "running");
    }

    #[test]
    fn own_process_is_alive() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(u32::MAX - 1));
    }
}
