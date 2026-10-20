use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemIsolationMode {
    Off,
    #[default]
    WorkspaceOnly,
    AllowList,
}

impl FilesystemIsolationMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceOnly => "workspace-only",
            Self::AllowList => "allow-list",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxConfig {
    pub enabled: Option<bool>,
    pub namespace_restrictions: Option<bool>,
    pub network_isolation: Option<bool>,
    pub filesystem_mode: Option<FilesystemIsolationMode>,
    pub allowed_mounts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxRequest {
    pub enabled: bool,
    pub namespace_restrictions: bool,
    pub network_isolation: bool,
    pub filesystem_mode: FilesystemIsolationMode,
    pub allowed_mounts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContainerEnvironment {
    pub in_container: bool,
    pub markers: Vec<String>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxStatus {
    pub enabled: bool,
    pub requested: SandboxRequest,
    pub supported: bool,
    pub active: bool,
    pub namespace_supported: bool,
    pub namespace_active: bool,
    pub network_supported: bool,
    pub network_active: bool,
    pub filesystem_mode: FilesystemIsolationMode,
    pub filesystem_active: bool,
    pub allowed_mounts: Vec<String>,
    pub in_container: bool,
    pub container_markers: Vec<String>,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDetectionInputs<'a> {
    pub env_pairs: Vec<(String, String)>,
    pub dockerenv_exists: bool,
    pub containerenv_exists: bool,
    pub proc_1_cgroup: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl SandboxConfig {
    #[must_use]
    pub fn resolve_request(
        &self,
        enabled_override: Option<bool>,
        namespace_override: Option<bool>,
        network_override: Option<bool>,
        filesystem_mode_override: Option<FilesystemIsolationMode>,
        allowed_mounts_override: Option<Vec<String>>,
    ) -> SandboxRequest {
        SandboxRequest {
            enabled: enabled_override.unwrap_or(self.enabled.unwrap_or(true)),
            namespace_restrictions: namespace_override
                .unwrap_or(self.namespace_restrictions.unwrap_or(true)),
            network_isolation: network_override.unwrap_or(self.network_isolation.unwrap_or(false)),
            filesystem_mode: filesystem_mode_override
                .or(self.filesystem_mode)
                .unwrap_or_default(),
            allowed_mounts: allowed_mounts_override.unwrap_or_else(|| self.allowed_mounts.clone()),
        }
    }
}

#[must_use]
pub fn detect_container_environment() -> ContainerEnvironment {
    let proc_1_cgroup = fs::read_to_string("/proc/1/cgroup").ok();
    detect_container_environment_from(SandboxDetectionInputs {
        env_pairs: env::vars().collect(),
        dockerenv_exists: Path::new("/.dockerenv").exists(),
        containerenv_exists: Path::new("/run/.containerenv").exists(),
        proc_1_cgroup: proc_1_cgroup.as_deref(),
    })
}

#[must_use]
pub fn detect_container_environment_from(
    inputs: SandboxDetectionInputs<'_>,
) -> ContainerEnvironment {
    let mut markers = Vec::new();
    if inputs.dockerenv_exists {
        markers.push("/.dockerenv".to_string());
    }
    if inputs.containerenv_exists {
        markers.push("/run/.containerenv".to_string());
    }
    for (key, value) in inputs.env_pairs {
        let normalized = key.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "container" | "docker" | "podman" | "kubernetes_service_host"
        ) && !value.is_empty()
        {
            markers.push(format!("env:{key}={value}"));
        }
    }
    if let Some(cgroup) = inputs.proc_1_cgroup {
        for needle in ["docker", "containerd", "kubepods", "podman", "libpod"] {
            if cgroup.contains(needle) {
                markers.push(format!("/proc/1/cgroup:{needle}"));
            }
        }
    }
    markers.sort();
    markers.dedup();
    ContainerEnvironment {
        in_container: !markers.is_empty(),
        markers,
    }
}

#[must_use]
pub fn resolve_sandbox_status(config: &SandboxConfig, cwd: &Path) -> SandboxStatus {
    let request = config.resolve_request(None, None, None, None, None);
    resolve_sandbox_status_for_request(&request, cwd)
}

/// Probe the actual requested mount/namespace setup. A failed probe never
/// permits a plain-shell fallback. `active` means this exact setup was verified.
#[must_use]
pub fn resolve_sandbox_status_for_request(request: &SandboxRequest, cwd: &Path) -> SandboxStatus {
    let container = detect_container_environment();
    let verification = verify_bubblewrap(request, cwd);
    let supported = verification.is_ok();
    let network_supported = if request.network_isolation {
        supported
    } else if supported {
        let mut network_request = request.clone();
        network_request.network_isolation = true;
        verify_bubblewrap(&network_request, cwd).is_ok()
    } else {
        false
    };
    let active = request.enabled && supported;
    let allowed_mounts = writable_mounts(request, cwd).unwrap_or_default();
    SandboxStatus {
        enabled: request.enabled,
        requested: request.clone(),
        supported,
        active,
        namespace_supported: supported,
        namespace_active: active,
        network_supported,
        network_active: active && request.network_isolation,
        filesystem_mode: request.filesystem_mode,
        filesystem_active: active && request.filesystem_mode != FilesystemIsolationMode::Off,
        allowed_mounts,
        in_container: container.in_container,
        container_markers: container.markers,
        fallback_reason: verification.err().map(|error| error.to_string()),
    }
}

fn verify_bubblewrap(request: &SandboxRequest, cwd: &Path) -> std::io::Result<()> {
    bubblewrap_command("true", cwd, request).and_then(|launcher| {
        let output = std::process::Command::new(&launcher.program)
            .args(&launcher.args)
            .envs(launcher.env)
            .stdin(std::process::Stdio::null())
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "Bubblewrap could not enforce the requested sandbox: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    })
}

/// A disabled sandbox is the only case where the caller may use a host shell.
/// Enabled but unsupported protection is an error, including filesystem-only
/// requests. No command is ever retried outside the sandbox automatically.
pub fn build_linux_sandbox_command(
    command: &str,
    cwd: &Path,
    status: &SandboxStatus,
) -> std::io::Result<Option<LinuxSandboxCommand>> {
    if !status.enabled {
        return Ok(None);
    }
    if !status.active {
        return Err(std::io::Error::other(format!(
            "sandbox unavailable; command was not executed: {}. Install/enable Bubblewrap, or request explicit approval for host execution with dangerouslyDisableSandbox=true",
            status.fallback_reason.as_deref().unwrap_or("requested protection could not be verified")
        )));
    }
    bubblewrap_command(command, cwd, &status.requested).map(Some)
}

fn writable_mounts(request: &SandboxRequest, cwd: &Path) -> std::io::Result<Vec<String>> {
    if request.filesystem_mode == FilesystemIsolationMode::Off {
        // The private /tmp would otherwise hide a workspace located there.
        let cwd = cwd.canonicalize()?;
        return Ok(if cwd.starts_with("/tmp") {
            vec![cwd.to_string_lossy().into_owned()]
        } else {
            Vec::new()
        });
    }
    if request.filesystem_mode == FilesystemIsolationMode::AllowList
        && request.allowed_mounts.is_empty()
    {
        return Err(std::io::Error::other(
            "filesystem allow-list requires allowedMounts",
        ));
    }
    let mut paths = vec![cwd.to_path_buf()];
    if request.filesystem_mode == FilesystemIsolationMode::AllowList {
        paths.extend(request.allowed_mounts.iter().map(|mount| {
            let path = PathBuf::from(mount);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        }));
    }
    let mut mounts = Vec::new();
    for path in paths {
        // Resolve symlinks and traversal before passing any host path to bwrap.
        // Requiring existing roots prevents a misspelled allow-list failing open.
        let path = path.canonicalize()?;
        if path == Path::new("/")
            || path.starts_with("/proc")
            || path.starts_with("/sys")
            || path.starts_with("/dev")
        {
            return Err(std::io::Error::other(format!(
                "invalid writable sandbox root: {}",
                path.display()
            )));
        }
        mounts.push(path.to_string_lossy().into_owned());
    }
    mounts.sort();
    mounts.dedup();
    Ok(mounts)
}

fn bubblewrap_command(
    command: &str,
    cwd: &Path,
    request: &SandboxRequest,
) -> std::io::Result<LinuxSandboxCommand> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (command, cwd, request);
        Err(std::io::Error::other(
            "sandbox requires Linux and Bubblewrap",
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        if uid == 0 {
            return Err(std::io::Error::other(
                "run Claw as an unprivileged user to use the sandbox",
            ));
        }
        let cwd = cwd.canonicalize()?;
        let mut args: Vec<String> = [
            "--unshare-user",
            "--disable-userns",
            "--unshare-pid",
            "--new-session",
            "--die-with-parent",
            "--cap-drop",
            "ALL",
            "--uid",
            &uid.to_string(),
            "--gid",
            &gid.to_string(),
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        if request.namespace_restrictions {
            args.extend(["--unshare-ipc", "--unshare-uts"].map(str::to_string));
        }
        if request.network_isolation {
            args.push("--unshare-net".to_string());
        }
        let root_bind = if request.filesystem_mode == FilesystemIsolationMode::Off {
            "--bind"
        } else {
            "--ro-bind"
        };
        args.extend(
            [
                root_bind, "/", "/", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
            ]
            .map(str::to_string),
        );
        for mount in writable_mounts(request, &cwd)? {
            args.extend(["--bind".to_string(), mount.clone(), mount]);
        }
        args.extend([
            "--chdir".to_string(),
            cwd.to_string_lossy().into_owned(),
            "--".to_string(),
            crate::bash::shell_program().to_string(),
            "-c".to_string(),
            command.to_string(),
        ]);
        Ok(LinuxSandboxCommand {
            program: "bwrap".to_string(),
            args,
            // Private temporary storage, with no fake root/Portage overrides.
            env: vec![("TMPDIR".to_string(), "/tmp".to_string())],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_linux_sandbox_command, detect_container_environment_from, FilesystemIsolationMode,
        SandboxConfig, SandboxDetectionInputs,
    };
    use std::path::Path;

    #[test]
    fn detects_container_markers_from_multiple_sources() {
        let detected = detect_container_environment_from(SandboxDetectionInputs {
            env_pairs: vec![("container".to_string(), "docker".to_string())],
            dockerenv_exists: true,
            containerenv_exists: false,
            proc_1_cgroup: Some("12:memory:/docker/abc"),
        });

        assert!(detected.in_container);
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "/.dockerenv"));
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "env:container=docker"));
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "/proc/1/cgroup:docker"));
    }

    #[test]
    fn resolves_request_with_overrides() {
        let config = SandboxConfig {
            enabled: Some(true),
            namespace_restrictions: Some(true),
            network_isolation: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: vec!["logs".to_string()],
        };

        let request = config.resolve_request(
            Some(true),
            Some(false),
            Some(true),
            Some(FilesystemIsolationMode::AllowList),
            Some(vec!["tmp".to_string()]),
        );

        assert!(request.enabled);
        assert!(!request.namespace_restrictions);
        assert!(request.network_isolation);
        assert_eq!(request.filesystem_mode, FilesystemIsolationMode::AllowList);
        assert_eq!(request.allowed_mounts, vec!["tmp"]);
    }

    #[test]
    fn unavailable_sandbox_never_falls_back_to_host() {
        let status = super::SandboxStatus {
            enabled: true,
            fallback_reason: Some("test backend unavailable".to_string()),
            ..Default::default()
        };
        assert!(build_linux_sandbox_command(
            "touch /tmp/should-not-run",
            Path::new("/tmp"),
            &status
        )
        .is_err());
        let disabled = super::SandboxStatus::default();
        assert!(
            build_linux_sandbox_command("true", Path::new("/tmp"), &disabled)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_allow_list_is_inactive() {
        let cwd = tempfile::tempdir().unwrap();
        let request = SandboxConfig::default().resolve_request(
            Some(true),
            None,
            None,
            Some(FilesystemIsolationMode::AllowList),
            None,
        );
        let status = super::resolve_sandbox_status_for_request(&request, cwd.path());
        assert!(!status.active);
        assert!(!status.filesystem_active);
        assert!(status.fallback_reason.unwrap().contains("allowedMounts"));
    }

    #[test]
    fn bubblewrap_enforces_writes_identity_and_network() {
        // Not under the current directory: other tests move it into their
        // own temp folders and delete them, which took this one along.
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("protected"), "original").unwrap();
        let request =
            SandboxConfig::default().resolve_request(Some(true), None, Some(true), None, None);
        let status = super::resolve_sandbox_status_for_request(&request, &workspace);
        if !status.active {
            // CI may prohibit namespaces. Still require a closed execution path.
            assert!(build_linux_sandbox_command("true", &workspace, &status).is_err());
            eprintln!(
                "Bubblewrap integration unavailable: {:?}",
                status.fallback_reason
            );
            return;
        }
        let run = |command: &str, status: &super::SandboxStatus| {
            let launcher = build_linux_sandbox_command(command, &workspace, status)
                .unwrap()
                .unwrap();
            std::process::Command::new(launcher.program)
                .args(launcher.args)
                .envs(launcher.env)
                .output()
                .unwrap()
        };
        let output = run(
            "id -u; printf ok > allowed; printf tmp > /tmp/private; readlink /proc/self/ns/net",
            &status,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        #[cfg(target_os = "linux")]
        assert!(String::from_utf8_lossy(&output.stdout)
            .starts_with(&format!("{}\n", rustix::process::geteuid().as_raw())));
        assert_eq!(
            std::fs::read_to_string(workspace.join("allowed")).unwrap(),
            "ok"
        );
        let host_net = std::fs::read_link("/proc/self/ns/net").unwrap();
        assert!(!String::from_utf8_lossy(&output.stdout).contains(host_net.to_str().unwrap()));
        // The sibling under /tmp is hidden by the private tmpfs.
        let output = run(
            &format!("printf changed > '{}'", outside.join("protected").display()),
            &status,
        );
        assert!(!output.status.success());
        assert_eq!(
            std::fs::read_to_string(outside.join("protected")).unwrap(),
            "original"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("protected"), workspace.join("escape"))
                .unwrap();
            assert!(!run("printf escaped > escape", &status).status.success());
            assert_eq!(
                std::fs::read_to_string(outside.join("protected")).unwrap(),
                "original"
            );
        }

        let mut allow = request.clone();
        allow.filesystem_mode = FilesystemIsolationMode::AllowList;
        allow.allowed_mounts = vec![outside.to_string_lossy().into_owned()];
        let status = super::resolve_sandbox_status_for_request(&allow, &workspace);
        let output = run(
            &format!(
                "printf approved > '{}'",
                outside.join("protected").display()
            ),
            &status,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("protected")).unwrap(),
            "approved"
        );
    }
}
