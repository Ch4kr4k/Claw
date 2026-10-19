//! Starts a model server when claw needs it and stops it when the last claw
//! using it exits, per provider. Configured in `settings.json`:
//!
//! ```json
//! "modelServers": {
//!   "local": {
//!     "start": "systemctl --user start llama-gpu",
//!     "stop": "systemctl --user stop llama-gpu",
//!     "health": "http://127.0.0.1:8080/health",
//!     "stopOnExit": true
//!   }
//! }
//! ```
//!
//! The key is the model's provider prefix (`local` for `local/qwen…`,
//! `ollama`, …; `default` matches any model). A wrapper can override it with
//! `CLAW_SERVER_START`, `CLAW_SERVER_STOP`, `CLAW_SERVER_HEALTH` and
//! `CLAW_SERVER_STOP_ON_EXIT` (for whatever provider is in use), or turn it
//! off with `CLAW_MANAGE_SERVER=0`. Cloud providers need no entry.
//!
//! Every claw process using a server holds a lease, a file named after its
//! pid under `~/.claw/run/model-server/<provider>/`. When a claw exits it
//! drops its lease and, if no live lease is left, stops the server. A small
//! detached watcher (`claw __model-server-watch <provider>`) does the same
//! for claws that die without cleaning up (a closed terminal, a killed
//! desktop agent).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

/// First argument that makes `claw` run the watcher.
pub const WATCH_ARG: &str = "__model-server-watch";
/// How often the watcher looks for dead leases.
const WATCH_INTERVAL: Duration = Duration::from_secs(15);
/// How long to wait for a started server to answer its health URL.
const START_WAIT: Duration = Duration::from_secs(20);

/// Providers this process holds a lease for.
static HELD: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub start: Option<String>,
    pub stop: Option<String>,
    pub health: Option<String>,
    pub stop_on_exit: bool,
}

/// The provider key of a model id: `local/qwen` -> `local`.
#[must_use]
pub fn provider_key(model: &str) -> String {
    model
        .split_once('/')
        .map_or("default", |(prefix, _)| prefix)
        .to_ascii_lowercase()
}

/// The server settings for `provider`, from the environment or settings.
#[must_use]
pub fn config_for(provider: &str) -> Option<ServerConfig> {
    if std::env::var("CLAW_MANAGE_SERVER").is_ok_and(|value| value == "0" || value == "false") {
        return None;
    }
    let settings = settings_entry(provider).unwrap_or(Value::Null);
    let text = |env: &str, key: &str| {
        std::env::var(env)
            .ok()
            .or_else(|| {
                settings
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|value| !value.trim().is_empty())
    };
    let start = text("CLAW_SERVER_START", "start");
    let stop = text("CLAW_SERVER_STOP", "stop");
    if start.is_none() && stop.is_none() {
        return None;
    }
    let stop_on_exit = match std::env::var("CLAW_SERVER_STOP_ON_EXIT") {
        Ok(value) => !matches!(value.as_str(), "0" | "false" | "no"),
        Err(_) => settings
            .get("stopOnExit")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    };
    Some(ServerConfig {
        start,
        stop,
        health: text("CLAW_SERVER_HEALTH", "health"),
        stop_on_exit,
    })
}

fn settings_entry(provider: &str) -> Option<Value> {
    let path = crate::config::default_config_home().join("settings.json");
    let root: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let servers = root.get("modelServers")?.as_object()?;
    servers
        .get(provider)
        .or_else(|| servers.get("default"))
        .cloned()
}

/// The endpoint `provider`'s requests go to, when set by the environment:
/// `local/` models use `CLAW_LOCAL_BASE_URL` (else `OPENAI_BASE_URL`),
/// Ollama `OLLAMA_HOST`.
fn endpoint(provider: &str) -> Option<String> {
    let names: &[&str] = match provider {
        "local" => &["CLAW_LOCAL_BASE_URL", "OPENAI_BASE_URL"],
        "ollama" => &["OLLAMA_HOST"],
        _ => &[],
    };
    names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty())
}

/// `host:port` of a URL (default ports filled in, `localhost` as 127.0.0.1).
fn host_port(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://").unwrap_or(("http", url.trim()));
    let authority = rest.split('/').next()?.rsplit('@').next()?;
    if authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => (host, port.to_string()),
        _ => (
            authority,
            if scheme == "https" { "443" } else { "80" }.to_string(),
        ),
    };
    let host = if host.eq_ignore_ascii_case("localhost") {
        "127.0.0.1"
    } else {
        host
    };
    Some(format!("{}:{port}", host.to_ascii_lowercase()))
}

/// Whether requests for `provider` go to the managed server. A provider
/// pointed elsewhere (`local/` models at NVIDIA, Together, a remote vLLM)
/// leaves the server alone; without an endpoint or a health URL to compare,
/// the entry applies.
fn targets_server(provider: &str, config: &ServerConfig) -> bool {
    match (endpoint(provider), &config.health) {
        (Some(endpoint), Some(health)) => host_port(&endpoint) == host_port(health),
        _ => true,
    }
}

fn lease_dir(provider: &str) -> PathBuf {
    crate::config::default_config_home()
        .join("run")
        .join("model-server")
        .join(provider)
}

/// Makes sure the server for `model`'s provider runs, and holds a lease on
/// it for this process. Does nothing for providers without a server entry,
/// or when this process already holds one. Progress goes to stderr.
pub fn ensure_for_model(model: &str) {
    let provider = provider_key(model);
    let Some(config) = config_for(&provider) else {
        return;
    };
    if !targets_server(&provider, &config) {
        return;
    }
    {
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !held.insert(provider.clone()) {
            return;
        }
    }
    let dir = lease_dir(&provider);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(std::process::id().to_string()), "");
    if !is_up(&config) {
        if let Some(start) = &config.start {
            eprintln!("\x1b[2mStarting the {provider} model server: {start}\x1b[0m");
            if !run_shell(start) {
                eprintln!("warning: `{start}` failed; requests to {provider} models may fail");
            }
            wait_until_up(&config);
        }
    }
    if config.stop_on_exit && config.stop.is_some() {
        spawn_watcher(&provider);
    }
}

/// Drops this process's leases and stops servers nobody else uses. Call on
/// the way out.
pub fn release_all() {
    let held: Vec<String> = std::mem::take(
        &mut *HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_iter()
    .collect();
    for provider in held {
        let _ = std::fs::remove_file(lease_dir(&provider).join(std::process::id().to_string()));
        stop_if_unused(&provider);
    }
}

/// Stops `provider`'s server when no live claw holds a lease. Returns
/// whether no lease is left.
fn stop_if_unused(provider: &str) -> bool {
    if live_leases(provider) > 0 {
        return false;
    }
    if let Some(config) = config_for(provider) {
        if config.stop_on_exit {
            if let Some(stop) = &config.stop {
                eprintln!("\x1b[2mStopping the {provider} model server: {stop}\x1b[0m");
                run_shell(stop);
            }
        }
    }
    true
}

/// Counts leases whose process is still a running claw, removing the rest.
fn live_leases(provider: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(lease_dir(provider)) else {
        return 0;
    };
    let mut live = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match name.parse::<u32>() {
            Ok(pid) if is_live_claw(pid) => live += 1,
            _ => {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    live
}

/// Whether `pid` is a running claw (a reused pid of another program does
/// not count).
fn is_live_claw(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains("claw"))
    } else {
        // Without /proc, keep the lease; stale ones only delay a stop.
        true
    }
}

fn is_up(config: &ServerConfig) -> bool {
    let Some(url) = &config.health else {
        return false;
    };
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or(rest);
    let address = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    std::net::ToSocketAddrs::to_socket_addrs(&address)
        .ok()
        .and_then(|mut addresses| addresses.next())
        .is_some_and(|address| {
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
        })
}

fn wait_until_up(config: &ServerConfig) {
    if config.health.is_none() {
        return;
    }
    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline {
        if is_up(config) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn run_shell(command: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn watcher_pid_file(provider: &str) -> PathBuf {
    lease_dir(provider).join(".watcher")
}

/// Starts the watcher for `provider` unless one is running. It runs in its
/// own session, so closing the terminal does not end it.
fn spawn_watcher(provider: &str) {
    // A test binary would take the watcher arguments as a test filter.
    if cfg!(test) {
        return;
    }
    let pid_file = watcher_pid_file(provider);
    if std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .is_some_and(is_live_claw)
    {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new("setsid")
        .arg(exe)
        .args([WATCH_ARG, provider])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// `claw __model-server-watch <provider>`: waits until no live claw holds a
/// lease, stops the server, and exits.
pub fn run_watcher(provider: &str) -> i32 {
    let pid_file = watcher_pid_file(provider);
    let _ = std::fs::write(&pid_file, std::process::id().to_string());
    loop {
        std::thread::sleep(WATCH_INTERVAL);
        // Another watcher took over.
        if std::fs::read_to_string(&pid_file)
            .ok()
            .as_deref()
            .map(str::trim)
            != Some(&std::process::id().to_string())
        {
            return 0;
        }
        if live_leases(provider) == 0 {
            stop_if_unused(provider);
            let _ = std::fs::remove_file(&pid_file);
            return 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{config_for, ensure_for_model, live_leases, provider_key, release_all};

    #[test]
    fn only_manages_the_server_the_provider_points_at() {
        use super::{host_port, targets_server, ServerConfig};
        assert_eq!(
            host_port("http://localhost:8080/v1").as_deref(),
            Some("127.0.0.1:8080")
        );
        assert_eq!(
            host_port("https://integrate.api.nvidia.com/v1").as_deref(),
            Some("integrate.api.nvidia.com:443")
        );
        let config = ServerConfig {
            start: Some("true".to_string()),
            stop: None,
            health: Some("http://127.0.0.1:8080/health".to_string()),
            stop_on_exit: true,
        };
        let _lock = crate::test_env_lock();
        let previous = std::env::var_os("CLAW_LOCAL_BASE_URL");
        std::env::set_var("CLAW_LOCAL_BASE_URL", "http://127.0.0.1:8080/v1");
        assert!(targets_server("local", &config));
        std::env::set_var("CLAW_LOCAL_BASE_URL", "https://integrate.api.nvidia.com/v1");
        assert!(!targets_server("local", &config));
        match previous {
            Some(value) => std::env::set_var("CLAW_LOCAL_BASE_URL", value),
            None => std::env::remove_var("CLAW_LOCAL_BASE_URL"),
        }
    }

    #[test]
    fn provider_keys() {
        assert_eq!(provider_key("local/qwen36-35b-a3b"), "local");
        assert_eq!(provider_key("Ollama/llama3"), "ollama");
        assert_eq!(provider_key("claude-opus-4-7"), "default");
    }

    #[test]
    fn starts_once_and_stops_after_the_last_lease() {
        let _lock = crate::test_env_lock();
        let home = std::env::temp_dir().join(format!("claw-model-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let log = home.join("log");
        std::fs::write(
            home.join("settings.json"),
            serde_json::json!({
                "modelServers": { "local": {
                    "start": format!("echo start >> {}", log.display()),
                    "stop": format!("echo stop >> {}", log.display()),
                    "stopOnExit": true
                }}
            })
            .to_string(),
        )
        .unwrap();
        let previous = std::env::var_os("CLAW_CONFIG_HOME");
        std::env::set_var("CLAW_CONFIG_HOME", &home);
        std::env::set_var("CLAW_SERVER_HEALTH", "");

        assert!(config_for("local").is_some());
        assert!(config_for("openai").is_none());
        ensure_for_model("local/some-model");
        ensure_for_model("local/other-model");
        assert_eq!(live_leases("local"), 1);
        release_all();
        assert_eq!(live_leases("local"), 0);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "start\nstop\n");

        std::env::remove_var("CLAW_SERVER_HEALTH");
        match previous {
            Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
            None => std::env::remove_var("CLAW_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
