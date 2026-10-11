//! Working-tree snapshots in a private "shadow" git repository.
//!
//! The shadow repo lives under `<config_home>/checkpoints/<hash>.git` and uses
//! the project folder as its work tree, so the project's own `.git` is never
//! touched. Each user request that changes anything gets a before/after commit,
//! which lets `/undo` and `/rewind` revert edits made by any tool (including
//! shell side effects) and survive restarts, and gives `/diff` a session
//! baseline.
//!
//! Snapshots are disabled for huge folders, the home directory, the filesystem
//! root, when git is not installed, or when `CLAW_SNAPSHOTS=0`.
//!
//! Ported from the `GitSnapshots`/`CheckpointManager` pair in `AzureAiHarness`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_FILES: usize = 20_000;
const MAX_CHECKPOINTS: usize = 50;

/// Directories that are never snapshotted (build output, dependencies, and
/// claw's own session store, which changes on every turn).
const IGNORED_PATTERNS: &[&str] = &[
    ".git/",
    "node_modules/",
    "target/",
    "dist/",
    "coverage/",
    ".build/",
    ".venv/",
    "venv/",
    "__pycache__/",
    ".claw/sessions/",
    "*.tmp",
];

/// Shadow-git snapshot store for one work tree.
#[derive(Debug, Clone)]
pub struct GitSnapshots {
    work_tree: PathBuf,
    git_dir: PathBuf,
    available: bool,
    reason: Option<String>,
}

/// Outcome of a single file during [`GitSnapshots::restore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreAction {
    Restored(String),
    Deleted(String),
    /// The file changed again after the request; it was left alone.
    Skipped(String),
    Failed {
        path: String,
        error: String,
    },
}

impl std::fmt::Display for RestoreAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Restored(path) => write!(f, "restored {path}"),
            Self::Deleted(path) => write!(f, "deleted  {path}"),
            Self::Skipped(path) => write!(f, "skipped  {path} (changed again since; left as is)"),
            Self::Failed { path, error } => write!(f, "FAILED   {path}: {error}"),
        }
    }
}

struct GitOutput {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

impl GitOutput {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

impl GitSnapshots {
    /// Opens (creating if needed) the shadow repo for `work_tree` under the
    /// default config home. Never fails: check [`Self::is_available`].
    #[must_use]
    pub fn open(work_tree: &Path) -> Self {
        Self::open_in(work_tree, &crate::default_config_home())
    }

    /// Like [`Self::open`] but with an explicit config home (used by tests).
    #[must_use]
    pub fn open_in(work_tree: &Path, config_home: &Path) -> Self {
        let work_tree = work_tree
            .canonicalize()
            .unwrap_or_else(|_| work_tree.to_path_buf());
        let digest = Sha256::digest(work_tree.to_string_lossy().as_bytes());
        let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        let git_dir = config_home.join("checkpoints").join(format!("{hash}.git"));
        let mut snapshots = Self {
            work_tree,
            git_dir,
            available: false,
            reason: None,
        };
        if let Err(reason) = snapshots.init() {
            snapshots.reason = Some(reason);
        } else {
            snapshots.available = true;
        }
        snapshots
    }

    #[must_use]
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// Why snapshots are unavailable, if they are.
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    #[must_use]
    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    fn init(&self) -> Result<(), String> {
        if matches!(
            std::env::var("CLAW_SNAPSHOTS").as_deref(),
            Ok("0" | "false" | "off")
        ) {
            return Err("disabled by CLAW_SNAPSHOTS".to_string());
        }
        if self.work_tree.parent().is_none() {
            return Err("the working directory is the filesystem root".to_string());
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            let home = home.canonicalize().unwrap_or(home);
            if self.work_tree == home {
                return Err("the working directory is the home folder".to_string());
            }
        }
        if count_files(&self.work_tree, MAX_FILES + 1) > MAX_FILES {
            return Err(format!(
                "more than {MAX_FILES} files in the working directory"
            ));
        }
        if self.git(&["--version"], false, None).code != 0 {
            return Err("git is not installed".to_string());
        }
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        if let Some(parent) = self.git_dir.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let git_dir = self.git_dir.to_string_lossy().into_owned();
        let init = self.git(&["init", "-q", "--bare", &git_dir], false, None);
        if init.code != 0 {
            return Err(format!("git init failed: {}", init.stderr.trim()));
        }
        for (key, value) in [
            ("core.bare", "false"),
            ("user.name", "claw"),
            ("user.email", "claw@localhost"),
            ("gc.auto", "0"),
        ] {
            self.git(&["config", key, value], true, None);
        }
        let info = self.git_dir.join("info");
        fs::create_dir_all(&info).map_err(|e| e.to_string())?;
        fs::write(info.join("exclude"), IGNORED_PATTERNS.join("\n") + "\n")
            .map_err(|e| e.to_string())?;
        fs::write(
            self.git_dir.join("claw-worktree.txt"),
            self.work_tree.to_string_lossy().as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Commits the current state of the work tree. Returns the commit id.
    #[must_use]
    pub fn snapshot(&self, message: &str) -> Option<String> {
        if !self.available {
            return None;
        }
        let add = self.git(&["add", "-A", "--ignore-errors", "--", "."], true, None);
        if add.code != 0 && add.code != 1 {
            return None;
        }
        let message: String = message
            .replace(['\r', '\n'], " ")
            .chars()
            .take(200)
            .collect();
        let commit = self.git(
            &[
                "commit",
                "-q",
                "--allow-empty",
                "--no-verify",
                "-m",
                &message,
            ],
            true,
            None,
        );
        if commit.code != 0 {
            return None;
        }
        let head = self.git(&["rev-parse", "HEAD"], true, None);
        (head.code == 0).then(|| head.text().trim().to_string())
    }

    /// True when two commits record the same tree (nothing changed between them).
    #[must_use]
    pub fn same_tree(&self, a: &str, b: &str) -> bool {
        let tree = |commit: &str| {
            let out = self.git(&["rev-parse", &format!("{commit}^{{tree}}")], true, None);
            (out.code == 0).then(|| out.text().trim().to_string())
        };
        matches!((tree(a), tree(b)), (Some(x), Some(y)) if x == y)
    }

    /// Paths changed between two commits, with their git status letter.
    #[must_use]
    pub fn changed_files(&self, before: &str, after: &str) -> Vec<(char, String)> {
        let out = self.git(
            &["diff", "--name-status", "--no-renames", "-z", before, after],
            true,
            None,
        );
        if out.code != 0 {
            return Vec::new();
        }
        let text = out.text();
        let parts: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
        parts
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .filter_map(|pair| pair[0].chars().next().map(|c| (c, pair[1].to_string())))
            .collect()
    }

    /// Reverts the changes made between `before` and `after`, but only for
    /// files whose current content still equals the `after` state. Anything
    /// edited since is left alone.
    #[must_use]
    pub fn restore(&self, before: &str, after: &str) -> Vec<RestoreAction> {
        let changes = self.changed_files(before, after);
        if changes.is_empty() {
            return Vec::new();
        }
        let after_tree = self.tree(after);
        let paths: Vec<String> = changes.iter().map(|(_, p)| p.clone()).collect();
        let current = self.hash_work_tree(&paths);
        let mut log = Vec::new();
        for (status, rel) in changes {
            let full = self.work_tree.join(&rel);
            if after_tree.get(&rel) != current.get(&rel) {
                log.push(RestoreAction::Skipped(rel));
                continue;
            }
            if status == 'A' {
                match fs::remove_file(&full) {
                    Ok(()) => log.push(RestoreAction::Deleted(rel)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        log.push(RestoreAction::Deleted(rel));
                    }
                    Err(e) => log.push(RestoreAction::Failed {
                        path: rel,
                        error: e.to_string(),
                    }),
                }
                continue;
            }
            let blob = self.git(
                &["cat-file", "--filters", &format!("{before}:{rel}")],
                true,
                None,
            );
            if blob.code != 0 {
                log.push(RestoreAction::Failed {
                    path: rel,
                    error: "could not read the earlier version".to_string(),
                });
                continue;
            }
            let write = full
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|()| fs::write(&full, &blob.stdout));
            match write {
                Ok(()) => log.push(RestoreAction::Restored(rel)),
                Err(e) => log.push(RestoreAction::Failed {
                    path: rel,
                    error: e.to_string(),
                }),
            }
        }
        log
    }

    /// Unified diff (with stat header) between `from` and `to`, or between
    /// `from` and the current work tree when `to` is `None`.
    #[must_use]
    pub fn diff(&self, from: &str, to: Option<&str>) -> Option<String> {
        let to = match to {
            Some(to) => to.to_string(),
            None => self.snapshot("diff")?,
        };
        let stat = self.git(
            &["-c", "color.ui=never", "diff", "--stat", from, &to],
            true,
            None,
        );
        let diff = self.git(
            &["-c", "color.ui=never", "diff", "--no-ext-diff", from, &to],
            true,
            None,
        );
        Some(
            format!("{}\n\n{}", stat.text().trim_end(), diff.text())
                .trim()
                .to_string(),
        )
    }

    fn tree(&self, commit: &str) -> BTreeMap<String, String> {
        let out = self.git(&["ls-tree", "-r", "-z", "--full-tree", commit], true, None);
        let mut map = BTreeMap::new();
        for entry in out.text().split('\0').filter(|s| !s.is_empty()) {
            // "<mode> blob <sha>\t<path>"
            if let Some((meta, path)) = entry.split_once('\t') {
                if let Some(sha) = meta.split(' ').nth(2) {
                    map.insert(path.to_string(), sha.to_string());
                }
            }
        }
        map
    }

    fn hash_work_tree(&self, paths: &[String]) -> BTreeMap<String, String> {
        let existing: Vec<&String> = paths
            .iter()
            .filter(|p| self.work_tree.join(p).is_file())
            .collect();
        let mut map = BTreeMap::new();
        if existing.is_empty() {
            return map;
        }
        let stdin = existing
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let out = self.git(&["hash-object", "--stdin-paths"], true, Some(&stdin));
        for (path, hash) in existing.iter().zip(out.text().lines()) {
            map.insert((*path).clone(), hash.trim().to_string());
        }
        map
    }

    fn git(&self, args: &[&str], use_repo: bool, stdin: Option<&str>) -> GitOutput {
        let mut command = Command::new("git");
        command.current_dir(&self.work_tree);
        if use_repo {
            command.arg(format!("--git-dir={}", self.git_dir.display()));
            command.arg(format!("--work-tree={}", self.work_tree.display()));
            for config in [
                "core.autocrlf=false",
                "core.quotepath=false",
                "core.fsmonitor=false",
                "commit.gpgsign=false",
            ] {
                command.args(["-c", config]);
            }
        }
        command
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                return GitOutput {
                    code: -1,
                    stdout: Vec::new(),
                    stderr: e.to_string(),
                }
            }
        };
        let mut child = child;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(input.as_bytes());
        }
        match child.wait_with_output() {
            Ok(out) => GitOutput {
                code: out.status.code().unwrap_or(-1),
                stdout: out.stdout,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => GitOutput {
                code: -1,
                stdout: Vec::new(),
                stderr: e.to_string(),
            },
        }
    }
}

fn count_files(root: &Path, limit: usize) -> usize {
    let skip = |name: &str| {
        matches!(
            name,
            ".git"
                | "node_modules"
                | "target"
                | "dist"
                | "coverage"
                | ".build"
                | ".venv"
                | "venv"
                | "__pycache__"
        )
    };
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !entry.file_name().to_str().is_some_and(skip))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .take(limit)
        .count()
}

/// One checkpoint per user request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    /// First line of the user's prompt, truncated.
    pub label: String,
    pub created_at_ms: u64,
    /// Number of session messages before the user message was added.
    pub message_index: usize,
    /// Session compaction count when the request started; rewinding the
    /// conversation is only possible while this is unchanged.
    pub compaction_generation: u32,
    /// Shadow-git commit before the request.
    pub before: String,
    /// Shadow-git commit after the request.
    pub after: Option<String>,
    /// Files changed by the request (filled in when it finishes).
    #[serde(default)]
    pub changed_files: Vec<String>,
}

/// Persisted undo stack for one session, stored next to the session file
/// as `<session>.checkpoints.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointStack {
    pub checkpoints: Vec<CheckpointRecord>,
}

impl CheckpointStack {
    /// Path of the sidecar file for a session file.
    #[must_use]
    pub fn sidecar_path(session_path: &Path) -> PathBuf {
        let mut name = session_path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".checkpoints.json");
        session_path.with_file_name(name)
    }

    /// Loads the stack for a session; a missing or unreadable file is empty.
    #[must_use]
    pub fn load(session_path: &Path) -> Self {
        fs::read_to_string(Self::sidecar_path(session_path))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, session_path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        fs::write(Self::sidecar_path(session_path), text)
    }

    /// Starts a checkpoint for a new request.
    pub fn begin(
        &mut self,
        label: &str,
        before: String,
        message_index: usize,
        compaction_generation: u32,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let label: String = label
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(60)
            .collect();
        self.checkpoints.push(CheckpointRecord {
            id: format!("cp-{now:x}-{}", self.checkpoints.len()),
            label,
            created_at_ms: now,
            message_index,
            compaction_generation,
            before,
            after: None,
            changed_files: Vec::new(),
        });
        if self.checkpoints.len() > MAX_CHECKPOINTS {
            self.checkpoints.remove(0);
        }
    }

    /// Completes the most recent checkpoint.
    pub fn finish(&mut self, after: String, changed_files: Vec<String>) {
        if let Some(last) = self.checkpoints.last_mut() {
            last.after = Some(after);
            last.changed_files = changed_files;
        }
    }

    pub fn pop(&mut self) -> Option<CheckpointRecord> {
        self.checkpoints.pop()
    }

    #[must_use]
    pub fn last(&self) -> Option<&CheckpointRecord> {
        self.checkpoints.last()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    /// The earliest "before" snapshot of the session (baseline for `/diff`).
    #[must_use]
    pub fn session_baseline(&self) -> Option<&str> {
        self.checkpoints.first().map(|c| c.before.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, GitSnapshots) {
        let work = tempfile::tempdir().expect("work dir");
        let home = tempfile::tempdir().expect("home dir");
        fs::write(work.path().join("a.txt"), "one\n").expect("write a");
        let snapshots = GitSnapshots::open_in(work.path(), home.path());
        (work, home, snapshots)
    }

    #[test]
    fn restore_reverts_edits_and_deletes_added_files() {
        if !git_available() {
            return;
        }
        let (work, _home, snapshots) = setup();
        assert!(
            snapshots.is_available(),
            "{:?}",
            snapshots.unavailable_reason()
        );
        let before = snapshots.snapshot("before").expect("before");
        fs::write(work.path().join("a.txt"), "two\n").expect("edit a");
        fs::write(work.path().join("b.txt"), "new\n").expect("add b");
        let after = snapshots.snapshot("after").expect("after");
        assert!(!snapshots.same_tree(&before, &after));

        let log = snapshots.restore(&before, &after);
        assert!(log.contains(&RestoreAction::Restored("a.txt".to_string())));
        assert!(log.contains(&RestoreAction::Deleted("b.txt".to_string())));
        assert_eq!(
            fs::read_to_string(work.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(!work.path().join("b.txt").exists());
    }

    #[test]
    fn restore_skips_files_changed_again_after_the_request() {
        if !git_available() {
            return;
        }
        let (work, _home, snapshots) = setup();
        let before = snapshots.snapshot("before").expect("before");
        fs::write(work.path().join("a.txt"), "two\n").expect("edit a");
        let after = snapshots.snapshot("after").expect("after");
        fs::write(work.path().join("a.txt"), "user edit\n").expect("edit again");

        let log = snapshots.restore(&before, &after);
        assert_eq!(log, vec![RestoreAction::Skipped("a.txt".to_string())]);
        assert_eq!(
            fs::read_to_string(work.path().join("a.txt")).unwrap(),
            "user edit\n"
        );
    }

    #[test]
    fn unchanged_tree_is_detected_and_sessions_are_ignored() {
        if !git_available() {
            return;
        }
        let (work, _home, snapshots) = setup();
        let before = snapshots.snapshot("before").expect("before");
        fs::create_dir_all(work.path().join(".claw/sessions")).unwrap();
        fs::write(work.path().join(".claw/sessions/s.jsonl"), "{}").unwrap();
        let after = snapshots.snapshot("after").expect("after");
        assert!(snapshots.same_tree(&before, &after));
    }

    #[test]
    fn diff_includes_new_files() {
        if !git_available() {
            return;
        }
        let (work, _home, snapshots) = setup();
        let before = snapshots.snapshot("before").expect("before");
        fs::write(work.path().join("c.txt"), "hello\n").unwrap();
        let diff = snapshots.diff(&before, None).expect("diff");
        assert!(diff.contains("c.txt"));
        assert!(diff.contains("+hello"));
    }

    #[test]
    fn checkpoint_stack_round_trips_through_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let mut stack = CheckpointStack::default();
        stack.begin("fix the bug\nmore", "abc".to_string(), 3, 0);
        stack.finish("def".to_string(), vec!["a.rs".to_string()]);
        stack.save(&session).unwrap();

        let loaded = CheckpointStack::load(&session);
        assert_eq!(loaded, stack);
        assert_eq!(loaded.last().unwrap().label, "fix the bug");
        assert_eq!(loaded.session_baseline(), Some("abc"));
        assert!(CheckpointStack::sidecar_path(&session)
            .to_string_lossy()
            .ends_with("session.jsonl.checkpoints.json"));
    }
}
