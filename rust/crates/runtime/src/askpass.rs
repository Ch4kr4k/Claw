//! Lets commands claw runs ask the user for an SSH key passphrase (or a git
//! password). Tool commands run with stdin closed, so `ssh` cannot prompt on
//! the terminal: it falls back to `SSH_ASKPASS`, and without one it gives up
//! with "Permission denied (publickey)". claw points `SSH_ASKPASS` at a tiny
//! script that runs `claw __askpass PROMPT`; that process hands the prompt to
//! this claw over a private Unix socket and prints what the user typed.

use std::path::PathBuf;
use std::sync::OnceLock;

/// First argument that makes `claw` act as the askpass helper.
pub const ASKPASS_ARG: &str = "__askpass";
/// Where the helper finds the socket of the claw that started it.
const SOCKET_ENV: &str = "CLAW_ASKPASS_SOCKET";

/// Asks the user `prompt`; `None` when they cancel or nobody can be asked.
pub type SecretPrompter = fn(&str) -> Option<String>;

static PROMPTER: OnceLock<SecretPrompter> = OnceLock::new();
/// The helper's environment once the bridge is up (`None`: it failed).
static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();

struct Bridge {
    dir: PathBuf,
    env: Vec<(String, String)>,
}

/// Registers how to ask the user. Only the interactive REPL does this; until
/// then commands get no askpass and behave as before.
pub fn set_prompter(prompter: SecretPrompter) {
    let _ = PROMPTER.set(prompter);
}

/// Environment for a command claw runs, so `ssh` (and git over ssh) asks the
/// user through claw. Empty without a prompter, when the user set their own
/// `SSH_ASKPASS`, or off Unix.
#[must_use]
pub fn child_env() -> Vec<(String, String)> {
    if PROMPTER.get().is_none() || std::env::var_os("SSH_ASKPASS").is_some() {
        return Vec::new();
    }
    BRIDGE
        .get_or_init(start_bridge)
        .as_ref()
        .map(|bridge| bridge.env.clone())
        .unwrap_or_default()
}

/// Removes the socket and helper script; call on the way out.
pub fn cleanup() {
    if let Some(Some(bridge)) = BRIDGE.get() {
        let _ = std::fs::remove_dir_all(&bridge.dir);
    }
}

#[cfg(not(unix))]
fn start_bridge() -> Option<Bridge> {
    None
}

#[cfg(unix)]
fn start_bridge() -> Option<Bridge> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::UnixListener;

    // Not /tmp: the sandbox mounts a private one, hiding the socket there.
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join("claw"))
        })?;
    let dir = base.join(format!("claw-askpass-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    // The directory being the user's alone is what keeps others off the socket.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    let socket = dir.join("sock");
    let listener = UnixListener::bind(&socket).ok()?;

    let exe = std::env::current_exe().ok()?;
    let script = dir.join("askpass");
    let quoted = exe.to_string_lossy().replace('\'', r"'\''");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nexec '{quoted}' {ASKPASS_ARG} \"$@\"\n"),
    )
    .ok()?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).ok()?;

    std::thread::Builder::new()
        .name("claw-askpass".to_string())
        .spawn(move || serve(&listener))
        .ok()?;
    let script = script.to_string_lossy().into_owned();
    Some(Bridge {
        dir,
        env: vec![
            ("SSH_ASKPASS".to_string(), script),
            // Use it even without $DISPLAY, instead of a GUI askpass.
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            (
                SOCKET_ENV.to_string(),
                socket.to_string_lossy().into_owned(),
            ),
        ],
    })
}

/// One prompt per connection: the helper sends the prompt and shuts its
/// side; the reply is the answer, or nothing when the user cancelled.
#[cfg(unix)]
fn serve(listener: &std::os::unix::net::UnixListener) {
    use std::io::{Read, Write};

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut prompt = String::new();
        if (&mut stream)
            .take(16 * 1024)
            .read_to_string(&mut prompt)
            .is_err()
        {
            continue;
        }
        let answer = PROMPTER.get().and_then(|ask| ask(prompt.trim_end()));
        if let Some(answer) = answer {
            let _ = stream.write_all(format!("ok\n{answer}").as_bytes());
        }
    }
}

/// `claw __askpass PROMPT...`: asks the claw named by `CLAW_ASKPASS_SOCKET`
/// and prints the answer, as `SSH_ASKPASS` programs do. Returns the exit
/// code: 1 when cancelled or there is no claw to ask.
#[must_use]
pub fn run_helper(args: &[String]) -> i32 {
    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        use std::net::Shutdown;
        use std::os::unix::net::UnixStream;

        let Some(socket) = std::env::var_os(SOCKET_ENV) else {
            return 1;
        };
        let Ok(mut stream) = UnixStream::connect(socket) else {
            return 1;
        };
        if stream.write_all(args.join(" ").as_bytes()).is_err()
            || stream.shutdown(Shutdown::Write).is_err()
        {
            return 1;
        }
        let mut reply = String::new();
        if stream.read_to_string(&mut reply).is_err() {
            return 1;
        }
        match reply.strip_prefix("ok\n") {
            Some(answer) => {
                println!("{answer}");
                0
            }
            None => 1,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        1
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn helper_relays_prompt_and_answer() {
        fn prompter(prompt: &str) -> Option<String> {
            (prompt == "Enter passphrase for key 'k':").then(|| "s3cret".to_string())
        }
        super::set_prompter(prompter);
        let _lock = crate::test_env_lock();
        let previous = std::env::var_os("SSH_ASKPASS");
        std::env::remove_var("SSH_ASKPASS");
        let env = super::child_env();
        if let Some(previous) = previous {
            std::env::set_var("SSH_ASKPASS", previous);
        }
        let socket = &env
            .iter()
            .find(|(key, _)| key == super::SOCKET_ENV)
            .expect("bridge started")
            .1;
        std::env::set_var(super::SOCKET_ENV, socket);
        let asked = super::run_helper(&["Enter passphrase for key 'k':".to_string()]);
        let cancelled = super::run_helper(&["Enter PIN:".to_string()]);
        std::env::remove_var(super::SOCKET_ENV);
        assert_eq!(asked, 0);
        assert_eq!(cancelled, 1);
        assert!(env
            .iter()
            .any(|(key, value)| key == "SSH_ASKPASS_REQUIRE" && value == "force"));
    }
}
