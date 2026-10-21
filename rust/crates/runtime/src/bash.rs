use std::env;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command as TokioCommand;
use tokio::runtime::Builder;
use tokio::time::timeout;

use crate::lane_events::{LaneEvent, ShipMergeMethod, ShipProvenance};
use crate::sandbox::{
    build_linux_sandbox_command, resolve_sandbox_status_for_request, FilesystemIsolationMode,
    SandboxStatus,
};
use crate::ConfigLoader;

/// Input schema for the built-in bash execution tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BashCommandInput {
    pub command: String,
    pub timeout: Option<u64>,
    pub description: Option<String>,
    #[serde(rename = "run_in_background")]
    pub run_in_background: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "namespaceRestrictions")]
    pub namespace_restrictions: Option<bool>,
    #[serde(rename = "isolateNetwork")]
    pub isolate_network: Option<bool>,
    #[serde(rename = "filesystemMode")]
    pub filesystem_mode: Option<FilesystemIsolationMode>,
    #[serde(rename = "allowedMounts")]
    pub allowed_mounts: Option<Vec<String>>,
}

/// Output returned from a bash tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BashCommandOutput {
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "rawOutputPath")]
    pub raw_output_path: Option<String>,
    pub interrupted: bool,
    #[serde(rename = "isImage")]
    pub is_image: Option<bool>,
    #[serde(rename = "backgroundTaskId")]
    pub background_task_id: Option<String>,
    #[serde(rename = "backgroundedByUser")]
    pub backgrounded_by_user: Option<bool>,
    #[serde(rename = "assistantAutoBackgrounded")]
    pub assistant_auto_backgrounded: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "returnCodeInterpretation")]
    pub return_code_interpretation: Option<String>,
    #[serde(rename = "noOutputExpected")]
    pub no_output_expected: Option<bool>,
    #[serde(rename = "structuredContent")]
    pub structured_content: Option<Vec<serde_json::Value>>,
    #[serde(rename = "persistedOutputPath")]
    pub persisted_output_path: Option<String>,
    #[serde(rename = "persistedOutputSize")]
    pub persisted_output_size: Option<u64>,
    #[serde(rename = "sandboxStatus")]
    pub sandbox_status: Option<SandboxStatus>,
}

thread_local! {
    // The model cannot supply this authorization in JSON. ConversationRuntime
    // installs it only after the explicit host permission check, and the shell
    // consumes it once before spawning, on the same execution thread.
    static HOST_APPROVAL: std::cell::RefCell<Option<(BashCommandInput, std::path::PathBuf)>> = const { std::cell::RefCell::new(None) };
}

pub(crate) struct HostExecutionScope;

impl HostExecutionScope {
    pub(crate) fn for_authorized_call(tool_name: &str, input: &str) -> Self {
        let grant = if crate::permissions::is_host_execution_request(tool_name, input) {
            serde_json::from_str::<BashCommandInput>(input)
                .ok()
                .zip(env::current_dir().ok())
        } else {
            None
        };
        HOST_APPROVAL.with(|approval| *approval.borrow_mut() = grant);
        Self
    }
}

impl Drop for HostExecutionScope {
    fn drop(&mut self) {
        HOST_APPROVAL.with(|approval| *approval.borrow_mut() = None);
    }
}

fn consume_host_approval(input: &BashCommandInput, cwd: &std::path::Path) -> bool {
    HOST_APPROVAL.with(|approval| {
        approval
            .borrow_mut()
            .take()
            .is_some_and(|(approved, approved_cwd)| approved == *input && approved_cwd == cwd)
    })
}

/// Executes a shell command with the requested sandbox settings.
pub fn execute_bash(input: BashCommandInput) -> io::Result<BashCommandOutput> {
    let cwd = env::current_dir()?;
    // With the sandbox switched off in settings every command already runs on
    // the host, so asking to leave the sandbox needs no separate approval.
    if input.dangerously_disable_sandbox == Some(true)
        && configured_sandbox_enabled(&cwd)
        && !consume_host_approval(&input, &cwd)
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "host execution requires explicit approval for this exact command; ordinary Bash approvals do not authorize disabling the sandbox"));
    }
    let sandbox_status = sandbox_status_for_input(&input, &cwd)?;

    if input.run_in_background.unwrap_or(false) {
        let command = prepare_command(&input.command, &cwd, &sandbox_status)?;
        let job_id =
            crate::background_jobs::global_background_jobs().start(command, &input.command)?;

        return Ok(BashCommandOutput {
            stdout: format!(
                "Started background job {job_id}. Use BashOutput(bash_id=\"{job_id}\") to read its output and KillShell(shell_id=\"{job_id}\") to stop it."
            ),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(job_id),
            backgrounded_by_user: Some(false),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: input.dangerously_disable_sandbox,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: Some(sandbox_status),
        });
    }

    let abort = TURN_ABORT.with(|signal| signal.borrow().clone());
    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(execute_bash_async(input, sandbox_status, cwd, abort))
}

thread_local! {
    /// The abort signal of the turn running tools on this thread.
    static TURN_ABORT: std::cell::RefCell<Option<crate::HookAbortSignal>> =
        const { std::cell::RefCell::new(None) };
}

/// While it lives, shell commands run on this thread stop when `signal`
/// is aborted (Esc or Ctrl+C in the REPL, Stop in the desktop app).
pub struct TurnAbortScope(Option<crate::HookAbortSignal>);

impl TurnAbortScope {
    #[must_use]
    pub fn enter(signal: &crate::HookAbortSignal) -> Self {
        Self(TURN_ABORT.with(|current| current.replace(Some(signal.clone()))))
    }
}

impl Drop for TurnAbortScope {
    fn drop(&mut self) {
        let previous = self.0.take();
        TURN_ABORT.with(|current| *current.borrow_mut() = previous);
    }
}

/// Why a command was ended early.
enum Ended {
    Timeout,
    Stopped,
}

/// Resolves when `abort` is set; never without one.
async fn aborted(abort: Option<crate::HookAbortSignal>) {
    let Some(abort) = abort else {
        return std::future::pending().await;
    };
    while !abort.is_aborted() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Signals the command's whole process group (the shell and everything it
/// started), so a stopped command does not leave children running.
#[cfg(unix)]
fn signal_group(pid: Option<u32>, kill: bool) {
    let Some(pid) = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    let signal = if kill {
        rustix::process::Signal::KILL
    } else {
        rustix::process::Signal::TERM
    };
    let _ = rustix::process::kill_process_group(pid, signal);
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _kill: bool) {}

/// Detect git push to main and emit ship provenance event
fn detect_and_emit_ship_prepared(command: &str) {
    let trimmed = command.trim();
    // Simple detection: git push with main/master
    if trimmed.contains("git push") && (trimmed.contains("main") || trimmed.contains("master")) {
        // Emit ship.prepared event
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let provenance = ShipProvenance {
            source_branch: get_current_branch().unwrap_or_else(|| "unknown".to_string()),
            base_commit: get_head_commit().unwrap_or_default(),
            commit_count: 0, // Would need to calculate from range
            commit_range: "unknown..HEAD".to_string(),
            merge_method: ShipMergeMethod::DirectPush,
            actor: get_git_actor().unwrap_or_else(|| "unknown".to_string()),
            pr_number: None,
        };
        let _event = LaneEvent::ship_prepared(format!("{now}"), &provenance);
        // Log to stderr as interim routing before event stream integration
        eprintln!(
            "[ship.prepared] branch={} -> main, commits={}, actor={}",
            provenance.source_branch, provenance.commit_count, provenance.actor
        );
    }
}

fn get_current_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_head_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_git_actor() -> Option<String> {
    let name = Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    Some(name)
}

async fn execute_bash_async(
    input: BashCommandInput,
    sandbox_status: SandboxStatus,
    cwd: std::path::PathBuf,
    abort: Option<crate::HookAbortSignal>,
) -> io::Result<BashCommandOutput> {
    // Detect and emit ship provenance for git push operations
    detect_and_emit_ship_prepared(&input.command);

    let mut command = prepare_tokio_command(&input.command, &cwd, &sandbox_status)?;
    // Its own process group, so stopping it reaches everything it started.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    // The output is read on its own tasks, and the command counts as done
    // when the shell exits, not when its output closes: a program started
    // in the background (`./server &`) keeps the pipes open for as long as
    // it runs, and waiting for them hung the turn for hours.
    let (stdout_buf, stdout_reader) = drain(child.stdout.take());
    let (stderr_buf, stderr_reader) = drain(child.stderr.take());
    let deadline = async {
        match input.timeout {
            Some(timeout_ms) => tokio::time::sleep(Duration::from_millis(timeout_ms)).await,
            None => std::future::pending().await,
        }
    };
    let (status, ended) = tokio::select! {
        status = child.wait() => (status?, None),
        () = deadline => (stop_group(pid, &mut child).await?, Some(Ended::Timeout)),
        () = aborted(abort) => (stop_group(pid, &mut child).await?, Some(Ended::Stopped)),
    };
    if let (Some(Ended::Timeout), Some(timeout_ms)) = (&ended, input.timeout) {
        return Ok(timeout_output(&input, timeout_ms, sandbox_status));
    }
    let output_closed = timeout(OUTPUT_GRACE, async {
        let _ = stdout_reader.await;
        let _ = stderr_reader.await;
    })
    .await
    .is_ok();
    let interrupted = ended.is_some();
    let stdout = truncate_output(&String::from_utf8_lossy(&taken(&stdout_buf)));
    let mut stderr = truncate_output(&String::from_utf8_lossy(&taken(&stderr_buf)));
    if !output_closed {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(BACKGROUND_HOLDS_OUTPUT_NOTE);
    }
    if interrupted {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str("[claw] The user stopped this command.");
    }
    if sandbox_status.active {
        if let Some(note) = hidden_device_note(&input.command) {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(&note);
        }
    }
    if sandbox_status.active && status.code() != Some(0) {
        if let Some(note) = sandbox_block_note(&stderr) {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(note);
        }
    }
    if status.code() != Some(0) {
        if let Some(note) = no_terminal_note(&stdout, &stderr) {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(note);
        }
    }
    let no_output_expected = Some(stdout.trim().is_empty() && stderr.trim().is_empty());
    let return_code_interpretation = status.code().and_then(|code| {
        if code == 0 {
            None
        } else {
            Some(format!("exit_code:{code}"))
        }
    });

    Ok(BashCommandOutput {
        stdout,
        stderr,
        raw_output_path: None,
        interrupted,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation,
        no_output_expected,
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
    })
}

/// Asks the command's process group to end, then kills it if it has not
/// within three seconds, and returns how the shell exited.
async fn stop_group(
    pid: Option<u32>,
    child: &mut tokio::process::Child,
) -> io::Result<std::process::ExitStatus> {
    signal_group(pid, false);
    if let Ok(result) = timeout(Duration::from_secs(3), child.wait()).await {
        return result;
    }
    signal_group(pid, true);
    child.wait().await
}

/// How long the output may stay open after the shell has exited.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);

const BACKGROUND_HOLDS_OUTPUT_NOTE: &str = "[claw] The command finished, but a program it started in the background (`&`) still holds its output open, so claw stopped reading it; the program keeps running. To start a long-running program, use run_in_background, or redirect its output: `prog > /tmp/prog.log 2>&1 &`.";

type SharedBuf = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

/// Reads `pipe` to its end on a task, into a buffer that can be taken
/// before the end comes.
fn drain<R>(pipe: Option<R>) -> (SharedBuf, tokio::task::JoinHandle<()>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let buf = SharedBuf::default();
    let sink = buf.clone();
    let reader = tokio::spawn(async move {
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            if let Ok(mut sink) = sink.lock() {
                sink.extend_from_slice(&chunk[..n]);
            }
        }
    });
    (buf, reader)
}

fn taken(buf: &SharedBuf) -> Vec<u8> {
    buf.lock()
        .map(|mut buf| std::mem::take(&mut *buf))
        .unwrap_or_default()
}

fn timeout_output(
    input: &BashCommandInput,
    timeout_ms: u64,
    sandbox_status: SandboxStatus,
) -> BashCommandOutput {
    let is_test = is_test_command(&input.command);
    let return_code_interpretation = if is_test { "test.hung" } else { "timeout" };
    BashCommandOutput {
        stdout: String::new(),
        stderr: format!("Command exceeded timeout of {timeout_ms} ms"),
        raw_output_path: None,
        interrupted: true,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation: Some(String::from(return_code_interpretation)),
        no_output_expected: Some(true),
        structured_content: Some(vec![test_timeout_provenance(
            &input.command,
            timeout_ms,
            is_test,
        )]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
    }
}

fn is_test_command(command: &str) -> bool {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    normalized.contains("cargo test")
        || normalized.contains("cargo nextest")
        || normalized.contains("npm test")
        || normalized.contains("pnpm test")
        || normalized.contains("yarn test")
        || normalized.contains("pytest")
}

fn test_timeout_provenance(
    command: &str,
    timeout_ms: u64,
    classified_as_test_hang: bool,
) -> serde_json::Value {
    json!({
        "event": if classified_as_test_hang { "test.hung" } else { "command.timeout" },
        "failureClass": if classified_as_test_hang { "test_hang" } else { "timeout" },
        "data": {
            "command": command,
            "timeoutMs": timeout_ms,
            "provenance": "bash.timeout",
            "classification": if classified_as_test_hang { "test.hung" } else { "timeout" }
        }
    })
}

/// Whether the settings ask for the shell sandbox (the default). Settings
/// that cannot be read count as enabled, so an error never skips approval.
pub fn configured_sandbox_enabled(cwd: &std::path::Path) -> bool {
    ConfigLoader::default_for(cwd)
        .load()
        .map_or(true, |config| {
            config
                .sandbox()
                .resolve_request(None, None, None, None, None)
                .enabled
        })
}

fn sandbox_status_for_input(
    input: &BashCommandInput,
    cwd: &std::path::Path,
) -> io::Result<SandboxStatus> {
    let config = ConfigLoader::default_for(cwd)
        .load()
        .map_err(|error| io::Error::other(format!("cannot load sandbox settings: {error}")))?
        .sandbox()
        .clone();
    let baseline = config.resolve_request(None, None, None, None, None);
    validate_sandbox_overrides(input, &baseline)?;
    // `sandbox.excludedCommands` run on the host, as the user configured;
    // ordinary Bash permissions still apply to them.
    let excluded = config.excludes(&input.command).then_some(false);
    let request = config.resolve_request(
        excluded.or(input.dangerously_disable_sandbox.map(|disabled| !disabled)),
        input.namespace_restrictions,
        input.isolate_network,
        input.filesystem_mode,
        input.allowed_mounts.clone(),
    );
    Ok(resolve_sandbox_status_for_request(&request, cwd))
}

fn validate_sandbox_overrides(
    input: &BashCommandInput,
    baseline: &crate::sandbox::SandboxRequest,
) -> io::Result<()> {
    if !baseline.enabled || input.dangerously_disable_sandbox == Some(true) {
        return Ok(());
    }
    let weaker_filesystem = match input.filesystem_mode {
        Some(FilesystemIsolationMode::Off) => {
            baseline.filesystem_mode != FilesystemIsolationMode::Off
        }
        Some(FilesystemIsolationMode::AllowList) => {
            baseline.filesystem_mode == FilesystemIsolationMode::WorkspaceOnly
        }
        _ => false,
    };
    if (input.namespace_restrictions == Some(false) && baseline.namespace_restrictions)
        || (input.isolate_network == Some(false) && baseline.network_isolation)
        || weaker_filesystem
        || input
            .allowed_mounts
            .as_ref()
            .is_some_and(|mounts| *mounts != baseline.allowed_mounts)
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            "tool arguments cannot weaken configured sandbox protections or change allowedMounts; request explicit host approval with dangerouslyDisableSandbox=true"));
    }
    Ok(())
}

/// The shell the model's commands run in: bash when it is installed, as
/// the tool's name promises (Claude Code runs bash too), else `sh`. On
/// systems where `sh` is dash, models' bash syntax (`<(...)`, `[[ ]]`,
/// arrays, `{a,b}`) failed: Qwen's `diff <(…)` got "Syntax error: \"(\"
/// unexpected".
#[must_use]
pub fn shell_program() -> &'static str {
    static SHELL: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    SHELL.get_or_init(|| {
        let on_path = std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("bash").is_file()));
        if on_path || std::path::Path::new("/bin/bash").is_file() {
            "bash"
        } else {
            "sh"
        }
    })
}

fn prepare_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
) -> io::Result<Command> {
    let mut prepared =
        if let Some(launcher) = build_linux_sandbox_command(command, cwd, sandbox_status)? {
            let mut prepared = Command::new(launcher.program);
            prepared.args(launcher.args).envs(launcher.env);
            prepared
        } else {
            let mut prepared = Command::new(shell_program());
            prepared.arg("-c").arg(command);
            prepared
        };
    // No terminal to type an SSH key passphrase on: ssh asks through claw.
    prepared
        .current_dir(cwd)
        .stdin(Stdio::null())
        .envs(crate::askpass::child_env());
    Ok(prepared)
}

fn prepare_tokio_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
) -> io::Result<TokioCommand> {
    let prepared = prepare_command(command, cwd, sandbox_status)?;
    let mut command = TokioCommand::from(prepared);
    command.kill_on_drop(true);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::BashCommandInput;

    fn execute_bash(input: BashCommandInput) -> std::io::Result<super::BashCommandOutput> {
        let _scope = super::HostExecutionScope::for_authorized_call(
            "bash",
            &serde_json::to_string(&input).unwrap(),
        );
        super::execute_bash(input)
    }
    use crate::sandbox::FilesystemIsolationMode;

    fn host_input() -> BashCommandInput {
        serde_json::from_value(
            serde_json::json!({"command":"printf approved", "dangerouslyDisableSandbox":true}),
        )
        .unwrap()
    }

    /// Points `CLAW_CONFIG_HOME` at an empty folder for the test's lifetime,
    /// so the user's own settings (say, the sandbox switched off) cannot leak in.
    struct IsolatedConfigHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
        dir: std::path::PathBuf,
    }

    impl IsolatedConfigHome {
        fn new() -> Self {
            let lock = crate::test_env_lock();
            let dir = std::env::temp_dir().join(format!(
                "claw-bash-config-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_nanos())
            ));
            std::fs::create_dir_all(dir.join(".claw")).unwrap();
            let previous = std::env::var_os("CLAW_CONFIG_HOME");
            std::env::set_var("CLAW_CONFIG_HOME", dir.join(".claw"));
            Self {
                _lock: lock,
                previous,
                dir,
            }
        }
    }

    impl Drop for IsolatedConfigHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
                None => std::env::remove_var("CLAW_CONFIG_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn stopping_the_turn_ends_the_command_and_its_children() {
        let _config = IsolatedConfigHome::new();
        let marker = std::env::temp_dir().join(format!("claw-stop-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let input: BashCommandInput = serde_json::from_value(serde_json::json!({
            "command": format!("(sleep 3; touch {}) & sleep 30", marker.display()),
            "dangerouslyDisableSandbox": true
        }))
        .unwrap();
        let signal = crate::HookAbortSignal::new();
        let stopper = signal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            stopper.abort();
        });
        let started = std::time::Instant::now();
        let output = {
            let _scope = super::TurnAbortScope::enter(&signal);
            execute_bash(input).expect("runs")
        };
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(output.interrupted);
        assert!(
            output.stderr.contains("The user stopped this command"),
            "{}",
            output.stderr
        );
        // The background child was in the same group and was stopped too.
        std::thread::sleep(std::time::Duration::from_secs(4));
        assert!(!marker.exists(), "child kept running after Stop");
    }

    #[test]
    fn a_program_left_running_in_the_background_does_not_hang_the_command() {
        let _config = IsolatedConfigHome::new();
        let input: BashCommandInput = serde_json::from_value(serde_json::json!({
            "command": "sleep 30 & echo started",
            "dangerouslyDisableSandbox": true
        }))
        .unwrap();
        let started = std::time::Instant::now();
        let output = execute_bash(input).expect("runs");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(output.stdout.trim(), "started");
        assert!(!output.interrupted);
        assert!(
            output.stderr.contains("still holds its output open"),
            "{}",
            output.stderr
        );
    }

    #[test]
    fn commands_run_in_bash_when_it_is_installed() {
        if super::shell_program() != "bash" {
            return;
        }
        let _config = IsolatedConfigHome::new();
        let input: BashCommandInput = serde_json::from_value(serde_json::json!({
            "command": "diff <(echo a) <(echo a) && [[ -n x ]] && echo {one,two}",
            "dangerouslyDisableSandbox": true
        }))
        .unwrap();
        let output = execute_bash(input).expect("runs");
        assert_eq!(output.stdout.trim(), "one two", "{}", output.stderr);
    }

    #[test]
    fn host_execution_requires_exact_single_use_approval() {
        let _config = IsolatedConfigHome::new();
        let input = host_input();
        assert_eq!(
            super::execute_bash(input.clone()).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let cwd = std::env::current_dir().unwrap();
        {
            let _scope = super::HostExecutionScope::for_authorized_call(
                "bash",
                &serde_json::to_string(&input).unwrap(),
            );
            let mut different = input.clone();
            different.command.push_str("; printf extra");
            assert!(!super::consume_host_approval(&different, &cwd));
            assert!(!super::consume_host_approval(&input, &cwd));
        }
        {
            let _scope = super::HostExecutionScope::for_authorized_call(
                "bash",
                &serde_json::to_string(&input).unwrap(),
            );
            assert!(!super::consume_host_approval(
                &input,
                &cwd.join("different")
            ));
        }
        {
            let _scope = super::HostExecutionScope::for_authorized_call(
                "bash",
                &serde_json::to_string(&input).unwrap(),
            );
            let output = super::execute_bash(input.clone()).unwrap();
            assert_eq!(output.stdout, "approved");
            assert!(!output.sandbox_status.unwrap().enabled);
            assert!(super::execute_bash(input.clone()).is_err());
        }
        {
            let _scope = super::HostExecutionScope::for_authorized_call(
                "bash",
                &serde_json::to_string(&input).unwrap(),
            );
        }
        assert!(super::execute_bash(input).is_err());
    }

    #[test]
    fn model_cannot_weaken_sandbox_with_tool_arguments() {
        let mut baseline =
            crate::sandbox::SandboxConfig::default().resolve_request(None, None, None, None, None);
        baseline.network_isolation = true;
        for change in [
            serde_json::json!({"namespaceRestrictions":false}),
            serde_json::json!({"isolateNetwork":false}),
            serde_json::json!({"filesystemMode":"off"}),
            serde_json::json!({"filesystemMode":"allow-list"}),
            serde_json::json!({"allowedMounts":["/"]}),
        ] {
            let mut value = serde_json::json!({"command":"true"});
            value
                .as_object_mut()
                .unwrap()
                .extend(change.as_object().unwrap().clone());
            let input = serde_json::from_value(value).unwrap();
            assert!(super::validate_sandbox_overrides(&input, &baseline).is_err());
        }
    }

    #[test]
    fn executes_simple_command() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: None,
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        });
        let output = match output {
            Ok(output) => output,
            Err(error) if error.to_string().contains("sandbox unavailable") => {
                // Hosts without Bubblewrap/namespaces must fail closed.
                return;
            }
            Err(error) => panic!("bash command failed: {error}"),
        };

        assert_eq!(output.stdout, "hello");
        assert!(!output.interrupted);
        assert!(output.sandbox_status.is_some());
    }

    #[test]
    fn disables_sandbox_when_requested() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        })
        .expect("bash command should execute");

        assert!(!output.sandbox_status.expect("sandbox status").enabled);
    }

    #[test]
    fn timed_out_test_command_is_classified_as_hung_test_with_provenance() {
        let output = execute_bash(BashCommandInput {
            command: String::from("sleep 1 # cargo test slow_case"),
            timeout: Some(1),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash command should return structured timeout");

        assert!(output.interrupted);
        assert_eq!(
            output.return_code_interpretation.as_deref(),
            Some("test.hung")
        );
        let structured = output.structured_content.expect("structured content");
        assert_eq!(structured[0]["event"], "test.hung");
        assert_eq!(structured[0]["data"]["provenance"], "bash.timeout");
    }

    #[test]
    fn prevents_stdin_hangs_by_redirecting_to_null() {
        let output = execute_bash(BashCommandInput {
            command: String::from("cat"),
            timeout: Some(2_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        })
        .expect("bash command should execute cleanly");

        assert!(
            !output.interrupted,
            "Command hung and was cut off by the timeout!"
        );
    }
}

/// Maximum output bytes before truncation (16 KiB, matching upstream).
/// What the sandbox's minimal `/dev` (bwrap `--dev`) provides; every other
/// device node, such as `/dev/i2c-*`, `/dev/video*` or `/dev/ttyUSB*`, is
/// absent inside it even when it exists on the host.
const SANDBOX_DEV_ENTRIES: &[&str] = &[
    "null",
    "zero",
    "full",
    "random",
    "urandom",
    "tty",
    "console",
    "pts",
    "ptmx",
    "shm",
    "fd",
    "stdin",
    "stdout",
    "stderr",
    "core",
    "mqueue",
    "hugepages",
];

/// A note for a sandboxed command that names a host device the sandbox
/// hides, so the model does not conclude from `No such file or directory`
/// (or an empty glob) that the device is missing on the machine.
fn hidden_device_note(command: &str) -> Option<String> {
    let mut hidden: Vec<&str> = Vec::new();
    for (start, _) in command.match_indices("/dev/") {
        let rest = &command[start + "/dev/".len()..];
        let end = rest
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '*' | '?' | '/'))
            })
            .unwrap_or(rest.len());
        let path = &rest[..end];
        // `/dev/pts/0` is provided, `/dev/i2c-1`, `/dev/i2c*` and `/dev/` are not.
        let first = path.split('/').next().unwrap_or("");
        let provided = SANDBOX_DEV_ENTRIES.contains(&first);
        if !provided {
            let shown = &command[start..start + "/dev/".len() + end];
            if !hidden.contains(&shown) {
                hidden.push(shown);
            }
        }
    }
    if hidden.is_empty() {
        return None;
    }
    Some(format!(
        "[sandbox] This command ran in claw's sandbox, whose /dev holds only basic devices (null, zero, random, tty, pts, shm). Host devices such as {} are hidden here even if they exist on the machine, so \"No such file or directory\" or an empty listing says nothing about the host. To check or use them, rerun the command with dangerouslyDisableSandbox=true (the user is asked to approve it).",
        hidden.join(", ")
    ))
}

/// A note for a sandboxed command that failed on a write the sandbox
/// refuses, so the model asks to rerun it on the host (as Claude Code does)
/// instead of working around it or giving up.
fn sandbox_block_note(stderr: &str) -> Option<&'static str> {
    let lower = stderr.to_lowercase();
    let blocked = lower.contains("read-only file system")
        || lower.contains("operation not permitted")
        || (lower.contains("permission denied") && !lower.contains("permission denied (publickey"));
    blocked.then_some(
        "[sandbox] This command ran in claw's sandbox, where only the workspace (and /tmp) is writable and .git/hooks, .git/config, .envrc and claw's settings files are read-only, so this failure may come from the sandbox rather than the machine. If the command is needed, rerun the same command with dangerouslyDisableSandbox=true: the user is asked to approve running it on the host.",
    )
}

/// A note for a command that failed because it wanted a terminal to ask
/// the user something (`emerge --ask`, `sudo` asking for a password, ...).
/// Commands run with stdin closed and no terminal, so the model should
/// rerun them non-interactively; claw does the asking itself.
fn no_terminal_note(stdout: &str, stderr: &str) -> Option<&'static str> {
    const SIGNS: [&str; 7] = [
        "should only be used in a terminal",
        "not a terminal",
        "no tty present",
        "a terminal is required",
        "inappropriate ioctl for device",
        "input is not a terminal",
        "a password is required",
    ];
    let lower = format!("{stdout}\n{stderr}").to_lowercase();
    if lower.contains("permission denied (publickey") {
        return Some(
            "[claw] ssh could not use a key. When a key has a passphrase, claw asks the user for it on the terminal as ssh needs it, so a refusal here means they cancelled, mistyped it, or the key is not on the server. Do not start ssh-agent, pass the passphrase to ssh-add or write askpass scripts; ask the user to rerun it or to run `! ssh-add` themselves.",
        );
    }
    SIGNS.iter().any(|sign| lower.contains(sign)).then_some(
        "[claw] Commands run without a terminal, so they cannot prompt or read answers. Rerun it non-interactively: drop `--ask`/`-a` (emerge), add `-y`/`--yes`/`--noconfirm`, or use `sudo -n`. claw already asks the user before risky commands, so no extra confirmation step is needed.",
    )
}

const MAX_OUTPUT_BYTES: usize = 16_384;

/// Truncate output to `MAX_OUTPUT_BYTES`, appending a marker when trimmed.
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    // Find the last valid UTF-8 boundary at or before MAX_OUTPUT_BYTES
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str("\n\n[output truncated — exceeded 16384 bytes]");
    truncated
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn notes_host_devices_the_sandbox_hides() {
        let note = super::hidden_device_note("ls -la /dev/i2c* && cat /dev/i2c-3 2>/dev/null")
            .expect("i2c devices are hidden in the sandbox");
        assert!(note.contains("/dev/i2c*, /dev/i2c-3"), "{note}");
        assert!(note.contains("dangerouslyDisableSandbox=true"));
        assert!(super::hidden_device_note("ls /dev/").is_some());
    }

    #[test]
    fn notes_commands_that_wanted_a_terminal() {
        let note = super::no_terminal_note(
            "",
            "!!! \"--ask\" should only be used in a terminal. Exiting.",
        )
        .expect("emerge --ask needs a terminal");
        assert!(note.contains("--ask"));
        assert_eq!(super::no_terminal_note("ok", "file not found"), None);
        let note = super::no_terminal_note("", "git@github.com: Permission denied (publickey).")
            .expect("ssh key refusals get a note");
        assert!(note.contains("Do not start ssh-agent"), "{note}");
    }

    #[test]
    fn no_device_note_for_devices_the_sandbox_provides() {
        assert_eq!(super::hidden_device_note("cargo build 2>/dev/null"), None);
        assert!(super::sandbox_block_note(
            "error: could not lock config file .git/config: Read-only file system"
        )
        .is_some());
        assert!(
            super::sandbox_block_note("touch: cannot touch '/etc/x': Permission denied").is_some()
        );
        assert!(
            super::sandbox_block_note("git@github.com: Permission denied (publickey).").is_none()
        );
        assert!(super::sandbox_block_note("error[E0425]: cannot find value").is_none());
        assert_eq!(
            super::hidden_device_note("head -c8 /dev/urandom > /dev/stderr; tty </dev/pts/0"),
            None
        );
        assert_eq!(super::hidden_device_note("echo devices"), None);
    }

    #[test]
    fn short_output_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_output(s), s);
    }

    #[test]
    fn long_output_truncated() {
        let s = "x".repeat(20_000);
        let result = truncate_output(&s);
        assert!(result.len() < 20_000);
        assert!(result.ends_with("[output truncated — exceeded 16384 bytes]"));
    }

    #[test]
    fn exact_boundary_unchanged() {
        let s = "a".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(truncate_output(&s), s);
    }

    #[test]
    fn one_over_boundary_truncated() {
        let s = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let result = truncate_output(&s);
        assert!(result.contains("[output truncated"));
    }
}
