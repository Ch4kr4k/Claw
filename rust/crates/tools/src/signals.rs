//! Signals in and around a repository, for sensing before acting.
//!
//! - `RepoSignals` (after "Physarum"): the repository as a pheromone field.
//!   Churn (files many recent commits touched: contested ground), fan-in
//!   (modules many files import: load-bearing tubes), failures (files named
//!   in the last failing type check, test run or Diagnostics: negative
//!   pheromone), coverage (from an lcov report: strong trails) and age give
//!   each file a reading; the summary names load-bearing, contested,
//!   failing, protected and cleanup-candidate files.
//! - `Trail` (after "Stigmergy"): a shared, decaying signal layer for agents
//!   working on the same repository (sub-agents, parallel chats,
//!   worktrees). An agent leaves a typed trace on a path; others sense it
//!   instead of reading each other's conversations. Traces fade
//!   exponentially (a half-life per trace), so stale ones disappear.

use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::PermissionMode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::code_search;
use super::{to_pretty_json, ToolSpec};

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "RepoSignals",
            description: "Sense a repository before changing it: which files are load-bearing (imported by many others), contested (changed by many recent commits), failing (named in the last failing type check, test or Diagnostics run, or in the output of test_command), protected (high test coverage from an lcov report) and cleanup candidates (nothing imports them and nobody touched them for a year). Give `file` for one file's full reading (its importers, churn, last change, coverage, failures) before editing it; without it you get the repository summary.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repository (default: current directory)." },
                    "file": { "type": "string", "description": "One file to read in detail." },
                    "days": { "type": "integer", "minimum": 1, "maximum": 3650, "description": "Churn window (default 90)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "test_command": { "type": "string", "description": "Optional command whose failures mark files (e.g. `cargo test`)." }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Trail",
            description: "Shared, fading signals between agents on the same repository (sub-agents, parallel chats, worktrees), instead of passing whole conversations. action \"leave\" puts a trace on a path (a file or folder): kind \"claim\" (I am working here, keep off), \"danger\" (broken or fragile, careful), \"info\" (a fact others need) or \"attraction\" (worth doing next); \"sense\" returns the traces on, above or below a path (all of them without a path), strongest first; \"clear\" removes your traces on a path. Traces decay by half every half_life_minutes (default 60; claims 30). Sense before editing files other agents may touch; leave a claim on what you edit and danger/info for what you learned.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["leave", "sense", "clear"] },
                    "path": { "type": "string" },
                    "kind": { "type": "string", "enum": ["claim", "danger", "info", "attraction"] },
                    "note": { "type": "string", "description": "One line: what and why." },
                    "strength": { "type": "number", "minimum": 0.05, "maximum": 1 },
                    "half_life_minutes": { "type": "number", "minimum": 1, "maximum": 10080 },
                    "by": { "type": "string", "description": "Who leaves it (agent name); defaults to this process." },
                    "repo": { "type": "string", "description": "Repository (default: current directory)." }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    let parse = |input: &Value| input.clone();
    Some(match name {
        "RepoSignals" => from(parse(input)).and_then(repo_signals),
        "Trail" => from(parse(input)).and_then(trail),
        _ => return None,
    })
}

fn from<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|error| error.to_string())
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The repository's work-tree root (or the folder itself outside git).
fn repo_root(path: &str) -> Result<PathBuf, String> {
    let path =
        std::fs::canonicalize(path).map_err(|error| format!("cannot open {path}: {error}"))?;
    let dir = if path.is_file() {
        path.parent().map(Path::to_path_buf).unwrap_or(path)
    } else {
        path
    };
    Ok(git(&dir, &["rev-parse", "--show-toplevel"])
        .map(|top| PathBuf::from(top.trim()))
        .unwrap_or(dir))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

fn config_dir(name: &str) -> PathBuf {
    runtime::default_config_home().join(name)
}

/// FNV-1a, for naming per-repository files.
fn fnv(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ───────────────────────────── failures memory ─────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct Failures {
    at: u64,
    source: String,
    /// Repository-relative paths named in the failing output.
    files: Vec<String>,
}

fn failures_path(root: &Path) -> PathBuf {
    config_dir("index").join(format!("{}-failures.json", fnv(&root.to_string_lossy())))
}

/// Files of `files` (repository-relative) that `output` names.
fn files_named_in(output: &str, files: &[String]) -> Vec<String> {
    let mut named: BTreeSet<String> = BTreeSet::new();
    let error_lines: Vec<&str> = output
        .lines()
        .filter(|line| {
            let lower = line.to_lowercase();
            lower.contains("error") || lower.contains("fail") || lower.contains("panicked")
        })
        .collect();
    for file in files {
        let name = Path::new(file)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.len() < 3 {
            continue;
        }
        if error_lines.iter().any(|line| {
            line.contains(file.as_str())
                || line.contains(&format!("/{name}"))
                || line.contains(&format!(" {name}"))
        }) {
            named.insert(file.clone());
        }
    }
    named.into_iter().collect()
}

/// Remembers which files a failing check named (Diagnostics, LeanGate), for
/// `RepoSignals`. An empty `output` (a clean run) clears the memory.
pub(crate) fn record_failures(root: &Path, source: &str, output: &str) {
    let files = code_search::list_files(root);
    let record = Failures {
        at: now_secs(),
        source: source.to_string(),
        files: if output.trim().is_empty() {
            Vec::new()
        } else {
            files_named_in(output, &files)
        },
    };
    let path = failures_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(&record) {
        let _ = std::fs::write(path, text);
    }
}

// ───────────────────────────── RepoSignals ─────────────────────────────

#[derive(Deserialize)]
struct SignalsInput {
    path: Option<String>,
    file: Option<String>,
    days: Option<u64>,
    limit: Option<usize>,
    test_command: Option<String>,
}

/// Stems that name a folder's module rather than themselves.
const ENTRY_STEMS: &[&str] = &[
    "mod", "lib", "main", "index", "__init__", "build", "setup", "conftest", "app",
];

/// The key importers use for `file`: its language family and the name it
/// is imported by (`rs:bash`, `py:loader`, `js:session-store`,
/// `c:llama-context.h`, `go:server`).
fn import_key(file: &str) -> Option<String> {
    let path = Path::new(file);
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => format!("rs:{}", import_name(file)?),
        "py" | "pyi" => format!("py:{}", import_name(file)?),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => format!("js:{}", import_name(file)?),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "cu" | "cuh" | "m" | "mm" => {
            format!("c:{}", path.file_name()?.to_string_lossy())
        }
        "go" => format!("go:{}", path.parent()?.file_name()?.to_string_lossy()),
        _ => return None,
    })
}

/// What a file is called by those who import it: its stem, or its folder's
/// name for a package's entry file (Rust `mod.rs`/`lib.rs`/`main.rs`,
/// Python `__init__.py`, JS `index.*`).
fn import_name(file: &str) -> Option<String> {
    let path = Path::new(file);
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    let entry = match extension {
        "rs" => matches!(stem.as_str(), "mod" | "lib" | "main"),
        "py" | "pyi" => stem == "__init__",
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => stem == "index",
        _ => false,
    };
    if entry {
        path.parent()?
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    } else {
        Some(stem)
    }
}

/// The module names a file's import lines refer to, read per language so
/// ordinary words (`use serde_json::{json, …}`, `use std::fmt`) do not count:
/// Rust `mod x;`, `crate::x`, `super::x`, `self::x`; quoted paths in
/// JS/TS `import`/`require`, C `#include "…"` and Go imports (last
/// segment, extension dropped); Python dotted names after `import`/`from`.
fn import_words(text: &str) -> BTreeSet<String> {
    let mut words: BTreeSet<String> = BTreeSet::new();
    let ident = |rest: &str| -> String {
        rest.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    };
    for line in text.lines() {
        let line = line.trim_start();
        // Rust.
        for marker in ["crate::", "super::", "self::"] {
            for (at, _) in line.match_indices(marker) {
                let name = ident(&line[at + marker.len()..]);
                if !name.is_empty() {
                    words.insert(format!("rs:{name}"));
                }
            }
        }
        for prefix in ["mod ", "pub mod ", "pub(crate) mod ", "pub(super) mod "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                if rest.trim_end().ends_with(';') {
                    words.insert(format!("rs:{}", ident(rest)));
                }
            }
        }
        // Quoted module paths: JS/TS, C/C++, Go.
        let quoted = |line: &str| -> Option<String> {
            for quote in ['"', '\''] {
                let mut parts = line.split(quote);
                parts.next();
                if let Some(path) = parts.next().filter(|path| !path.is_empty()) {
                    return Some(path.to_string());
                }
            }
            None
        };
        if line.starts_with("#include \"") {
            if let Some(path) = quoted(line) {
                words.insert(format!("c:{}", path.rsplit('/').next().unwrap_or(&path)));
            }
        } else if line.starts_with("import ")
            || line.contains(" from \"")
            || line.contains(" from '")
            || line.contains("require(")
        {
            if let Some(path) = quoted(line) {
                let last = path.rsplit('/').next().unwrap_or(&path);
                let stem = last.split('.').next().unwrap_or(last);
                if !stem.is_empty() {
                    // JS/TS paths, and Go package paths in single-line imports.
                    words.insert(format!("js:{stem}"));
                    words.insert(format!("go:{last}"));
                }
            }
        } else if line.starts_with('"') && line.ends_with('"') {
            // A line of a Go import block.
            if let Some(path) = quoted(line) {
                words.insert(format!("go:{}", path.rsplit('/').next().unwrap_or(&path)));
            }
        }
        // Python: `import a.b.c`, `from a.b import c, d`.
        if let Some(rest) = line.strip_prefix("from ") {
            let (module, names) = rest.split_once(" import ").unwrap_or((rest, ""));
            words.extend(
                module
                    .split('.')
                    .map(str::trim)
                    .filter(|w| !w.is_empty())
                    .map(|w| format!("py:{w}")),
            );
            words.extend(
                names
                    .split(',')
                    .filter_map(|name| name.split_whitespace().next())
                    .map(|name| name.trim_matches(|c| c == '(' || c == ')').to_string())
                    .filter(|name| !name.is_empty())
                    .map(|name| format!("py:{name}")),
            );
        } else if let Some(rest) = line
            .strip_prefix("import ")
            .filter(|rest| !rest.contains('"') && !rest.contains('\''))
        {
            for module in rest.split(',') {
                let module = module.split_whitespace().next().unwrap_or_default();
                words.extend(
                    module
                        .split('.')
                        .filter(|w| !w.is_empty())
                        .map(|w| format!("py:{w}")),
                );
            }
        }
    }
    words.retain(|word| {
        word.split_once(':')
            .is_some_and(|(_, name)| name.len() >= 2)
    });
    words
}

/// Of several files sharing an import key, the one nearest to `importer`
/// (the longest common folder prefix).
fn nearest<'a>(importer: &str, targets: &'a [String]) -> Option<&'a String> {
    let common = |target: &str| {
        importer
            .split('/')
            .zip(target.split('/'))
            .take_while(|(a, b)| a == b)
            .count()
    };
    targets.iter().max_by_key(|target| common(target))
}

fn is_test_file(file: &str) -> bool {
    let lower = file.to_lowercase();
    lower.contains("test") || lower.contains("spec") || lower.contains("bench")
}

/// Per-file coverage (percent of lines hit) from an lcov report.
fn lcov_coverage(root: &Path) -> HashMap<String, f64> {
    let mut coverage = HashMap::new();
    for candidate in [
        "lcov.info",
        "coverage/lcov.info",
        "target/coverage/lcov.info",
        "target/lcov.info",
    ] {
        let Ok(text) = std::fs::read_to_string(root.join(candidate)) else {
            continue;
        };
        let (mut file, mut found, mut hit) = (String::new(), 0_u64, 0_u64);
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("SF:") {
                let path = Path::new(path);
                file = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
            } else if let Some(n) = line.strip_prefix("LF:") {
                found = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = line.strip_prefix("LH:") {
                hit = n.trim().parse().unwrap_or(0);
            } else if line == "end_of_record" && found > 0 {
                #[allow(clippy::cast_precision_loss)]
                coverage.insert(std::mem::take(&mut file), hit as f64 * 100.0 / found as f64);
            }
        }
        break;
    }
    coverage
}

struct Field {
    files: Vec<String>,
    churn: HashMap<String, usize>,
    last_change: HashMap<String, u64>,
    importers: HashMap<String, Vec<String>>,
    coverage: HashMap<String, f64>,
    failing: BTreeSet<String>,
    failing_source: Option<String>,
}

fn read_field(root: &Path, days: u64, test_command: Option<&str>) -> Field {
    let files: Vec<String> = code_search::list_files(root)
        .into_iter()
        .filter(|file| code_search::language(file).is_some())
        .collect();
    // Churn: commits in the window touching each file.
    let mut churn: HashMap<String, usize> = HashMap::new();
    if let Some(log) = git(
        root,
        &[
            "log",
            &format!("--since={days}.days"),
            "--name-only",
            "--format=",
        ],
    ) {
        for line in log.lines().filter(|line| !line.is_empty()) {
            *churn.entry(line.to_string()).or_default() += 1;
        }
    }
    // Last change (within two years) of each file.
    let mut last_change: HashMap<String, u64> = HashMap::new();
    if let Some(log) = git(
        root,
        &["log", "--since=2.years", "--name-only", "--format=@%ct"],
    ) {
        let mut when = 0_u64;
        for line in log.lines().filter(|line| !line.is_empty()) {
            if let Some(time) = line.strip_prefix('@') {
                when = time.parse().unwrap_or(0);
            } else {
                last_change.entry(line.to_string()).or_insert(when);
            }
        }
    }
    // Files git has no recent record of (new or untracked): their mtime.
    for file in &files {
        if !last_change.contains_key(file) {
            let tracked_long_ago = git(root, &["log", "-1", "--format=%ct", "--", file])
                .and_then(|out| out.trim().parse::<u64>().ok());
            let when = tracked_long_ago.or_else(|| {
                std::fs::metadata(root.join(file))
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|age| age.as_secs())
            });
            if let Some(when) = when {
                last_change.insert(file.clone(), when);
            }
        }
    }
    // Fan-in: who imports whom (by the name a file is imported as).
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    for file in &files {
        if let Some(key) = import_key(file) {
            by_name.entry(key).or_default().push(file.clone());
        }
    }
    let mut importers: HashMap<String, Vec<String>> = HashMap::new();
    for file in &files {
        let full = root.join(file);
        if std::fs::metadata(&full).map_or(true, |meta| meta.len() > 2 * 1024 * 1024) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(full) else {
            continue;
        };
        for word in import_words(&text) {
            if let Some(targets) = by_name.get(&word) {
                if let Some(target) = nearest(file, targets) {
                    if target != file {
                        importers
                            .entry(target.clone())
                            .or_default()
                            .push(file.clone());
                    }
                }
            }
        }
    }
    // Failures: the remembered ones, or a fresh test run.
    let mut failing = BTreeSet::new();
    let mut failing_source = None;
    if let Some(command) = test_command.filter(|command| !command.trim().is_empty()) {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(root)
            .output();
        if let Ok(output) = output {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if !output.status.success() {
                failing.extend(files_named_in(&text, &files));
            }
            record_failures(
                root,
                command,
                if output.status.success() { "" } else { &text },
            );
            failing_source = Some(format!("`{command}` just now"));
        }
    } else if let Some(record) = std::fs::read_to_string(failures_path(root))
        .ok()
        .and_then(|text| serde_json::from_str::<Failures>(&text).ok())
    {
        failing.extend(record.files);
        let age_minutes = now_secs().saturating_sub(record.at) / 60;
        failing_source = Some(format!("{} ({age_minutes} min ago)", record.source));
    }
    Field {
        coverage: lcov_coverage(root),
        files,
        churn,
        last_change,
        importers,
        failing,
        failing_source,
    }
}

fn days_since(when: Option<&u64>) -> Option<u64> {
    when.filter(|when| **when > 0)
        .map(|when| now_secs().saturating_sub(*when) / 86_400)
}

fn repo_signals(input: SignalsInput) -> Result<String, String> {
    let root = repo_root(input.path.as_deref().unwrap_or("."))?;
    let days = input.days.unwrap_or(90);
    let limit = input.limit.unwrap_or(10);
    let field = read_field(&root, days, input.test_command.as_deref());
    if let Some(file) = &input.file {
        let absolute = std::fs::canonicalize(file).unwrap_or_else(|_| root.join(file));
        let relative = absolute
            .strip_prefix(&root)
            .map_or_else(|_| file.clone(), |rel| rel.to_string_lossy().into_owned());
        let importers = field.importers.get(&relative).cloned().unwrap_or_default();
        let churn = field.churn.get(&relative).copied().unwrap_or(0);
        let coverage = field.coverage.get(&relative).copied();
        let failing = field.failing.contains(&relative);
        let mut reading = Vec::new();
        if failing {
            reading.push(
                "failing: named in the last failing check; fix or understand that first"
                    .to_string(),
            );
        }
        if importers.len() >= 5 {
            reading.push(format!(
                "load-bearing: {} files import it; keep its interface stable",
                importers.len()
            ));
        }
        if churn >= 5 {
            reading.push(format!(
                "contested: {churn} commits in {days} days; others are working here"
            ));
        }
        if coverage.is_some_and(|c| c >= 80.0) {
            reading
                .push("protected: well covered by tests; run them after changing it".to_string());
        }
        if importers.is_empty()
            && !is_test_file(&relative)
            && days_since(field.last_change.get(&relative)).is_none_or(|age| age > 365)
        {
            reading.push("quiet: nothing imports it and it has not changed for a year; check it is still used".to_string());
        }
        return to_pretty_json(json!({
            "root": root.display().to_string(),
            "file": relative,
            "reading": reading,
            "importers": importers.iter().take(20).collect::<Vec<_>>(),
            "importer_count": importers.len(),
            "commits_in_window": churn,
            "days_since_change": days_since(field.last_change.get(&relative)),
            "coverage_percent": coverage.map(|c| (c * 10.0).round() / 10.0),
            "failing": failing,
            "failures_from": field.failing_source,
        }));
    }
    let mut load_bearing: Vec<(&String, usize)> = field
        .importers
        .iter()
        .map(|(file, who)| (file, who.len()))
        .filter(|(_, count)| *count >= 3)
        .collect();
    load_bearing.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut contested: Vec<(&String, usize)> = field
        .churn
        .iter()
        .filter(|(file, count)| **count >= 3 && field.files.contains(file))
        .map(|(file, count)| (file, *count))
        .collect();
    contested.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let mut protected: Vec<(&String, f64)> = field
        .coverage
        .iter()
        .filter(|(_, cov)| **cov >= 80.0)
        .map(|(file, cov)| (file, *cov))
        .collect();
    protected.sort_by(|a, b| b.1.total_cmp(&a.1));
    let cleanup: Vec<&String> = field
        .files
        .iter()
        .filter(|file| {
            !is_test_file(file)
                && !field.importers.contains_key(*file)
                && import_name(file).is_some()
                && !ENTRY_STEMS.contains(
                    &Path::new(file)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                        .as_str(),
                )
                && days_since(field.last_change.get(*file)).is_none_or(|age| age > 365)
        })
        .take(limit)
        .collect();
    let row = |(file, value): (&String, usize), unit: &str| format!("{file} ({value} {unit})");
    to_pretty_json(json!({
        "root": root.display().to_string(),
        "code_files": field.files.len(),
        "load_bearing": load_bearing.into_iter().take(limit).map(|r| row(r, "importers")).collect::<Vec<_>>(),
        "contested": contested.into_iter().take(limit).map(|r| row(r, &format!("commits in {days} days"))).collect::<Vec<_>>(),
        "failing": field.failing.iter().take(limit).collect::<Vec<_>>(),
        "failures_from": field.failing_source,
        "protected": protected.into_iter().take(limit).map(|(file, cov)| format!("{file} ({cov:.0}% covered)")).collect::<Vec<_>>(),
        "coverage_report": if field.coverage.is_empty() { "none found (lcov.info)" } else { "lcov" },
        "cleanup_candidates": cleanup,
        "note": "Load-bearing: change interfaces carefully. Contested: others are active here. Failing: deal with first. Cleanup candidates: nothing imports them and they have not changed in a year; check before deleting (entry points, scripts and dynamic loading are not seen).",
    }))
}

// ───────────────────────────── Trail ─────────────────────────────

#[derive(Deserialize)]
struct TrailInput {
    action: String,
    path: Option<String>,
    kind: Option<String>,
    note: Option<String>,
    strength: Option<f64>,
    half_life_minutes: Option<f64>,
    by: Option<String>,
    repo: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Trace {
    path: String,
    kind: String,
    note: String,
    by: String,
    strength: f64,
    half_life_minutes: f64,
    at: u64,
}

impl Trace {
    #[allow(clippy::cast_precision_loss)]
    fn current(&self, now: u64) -> f64 {
        let minutes = now.saturating_sub(self.at) as f64 / 60.0;
        self.strength * 0.5_f64.powf(minutes / self.half_life_minutes.max(1.0))
    }
}

/// Traces below this strength are gone.
const FADED: f64 = 0.05;

/// One trail file per repository, shared by its worktrees.
fn trail_file(root: &Path) -> PathBuf {
    let common = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_or_else(
        || root.to_string_lossy().into_owned(),
        |dir| dir.trim().to_string(),
    );
    config_dir("trails").join(format!("{}.json", fnv(&common)))
}

/// Runs `f` on the repository's traces under a lock file, saving the result.
fn with_traces<T>(file: &Path, f: impl FnOnce(&mut Vec<Trace>) -> T) -> Result<T, String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let lock = file.with_extension("lock");
    let mut attempts = 0;
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => break,
            Err(_) if attempts < 100 => {
                // A lock older than five seconds was left by a crash.
                if std::fs::metadata(&lock)
                    .and_then(|meta| meta.modified())
                    .is_ok_and(|time| time.elapsed().unwrap_or_default() > Duration::from_secs(5))
                {
                    let _ = std::fs::remove_file(&lock);
                }
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("trail is locked: {error}")),
        }
    }
    let mut traces: Vec<Trace> = std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let now = now_secs();
    traces.retain(|trace| trace.current(now) >= FADED);
    let result = f(&mut traces);
    let saved = serde_json::to_string(&traces)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            let temp = file.with_extension("tmp");
            std::fs::write(&temp, text)
                .and_then(|()| std::fs::rename(&temp, file))
                .map_err(|error| error.to_string())
        });
    let _ = std::fs::remove_file(&lock);
    saved.map(|()| result)
}

/// `path` relative to the repository root, `""` for the whole repository.
fn relative_path(root: &Path, path: Option<&str>) -> String {
    let Some(path) = path
        .map(str::trim)
        .filter(|path| !path.is_empty() && *path != ".")
    else {
        return String::new();
    };
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| {
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(candidate)
        }
    });
    absolute
        .strip_prefix(root)
        .map_or_else(
            |_| path.to_string(),
            |rel| rel.to_string_lossy().into_owned(),
        )
        .trim_end_matches('/')
        .to_string()
}

/// Whether two repository paths are the same, or one contains the other.
fn related(a: &str, b: &str) -> bool {
    a.is_empty()
        || b.is_empty()
        || a == b
        || a.starts_with(&format!("{b}/"))
        || b.starts_with(&format!("{a}/"))
}

fn trail(input: TrailInput) -> Result<String, String> {
    let root = repo_root(input.repo.as_deref().unwrap_or("."))?;
    let file = trail_file(&root);
    let path = relative_path(&root, input.path.as_deref());
    let by = input
        .by
        .clone()
        .or_else(|| std::env::var("CLAW_AGENT_NAME").ok())
        .unwrap_or_else(|| format!("claw-{}", std::process::id()));
    let now = now_secs();
    match input.action.as_str() {
        "leave" => {
            let kind = input.kind.clone().unwrap_or_else(|| "info".to_string());
            if !matches!(kind.as_str(), "claim" | "danger" | "info" | "attraction") {
                return Err(format!("unknown kind `{kind}`"));
            }
            let trace = Trace {
                path: path.clone(),
                half_life_minutes: input.half_life_minutes.unwrap_or(if kind == "claim" {
                    30.0
                } else {
                    60.0
                }),
                kind,
                note: input.note.clone().unwrap_or_default(),
                by: by.clone(),
                strength: input.strength.unwrap_or(1.0).clamp(FADED, 1.0),
                at: now,
            };
            let left = with_traces(&file, |traces| {
                // A new trace of the same kind by the same agent replaces the old.
                traces.retain(|old| {
                    !(old.path == trace.path && old.kind == trace.kind && old.by == trace.by)
                });
                traces.push(trace.clone());
                traces.len()
            })?;
            to_pretty_json(
                json!({ "left": { "path": path, "kind": trace.kind, "by": by }, "traces_in_repo": left }),
            )
        }
        "sense" => {
            let found = with_traces(&file, |traces| {
                let mut found: Vec<(f64, Trace)> = traces
                    .iter()
                    .filter(|trace| related(&trace.path, &path))
                    .map(|trace| (trace.current(now), trace.clone()))
                    .collect();
                found.sort_by(|a, b| b.0.total_cmp(&a.0));
                found
            })?;
            let rows: Vec<Value> = found
                .iter()
                .take(30)
                .map(|(strength, trace)| {
                    json!({
                        "path": if trace.path.is_empty() { "(whole repository)" } else { trace.path.as_str() },
                        "kind": trace.kind,
                        "strength": (strength * 100.0).round() / 100.0,
                        "note": trace.note,
                        "by": trace.by,
                        "minutes_ago": now.saturating_sub(trace.at) / 60,
                        "mine": trace.by == by,
                    })
                })
                .collect();
            let others_claim = found.iter().any(|(_, trace)| {
                trace.kind == "claim" && trace.by != by && !trace.path.is_empty()
            });
            to_pretty_json(json!({
                "path": if path.is_empty() { "(whole repository)" } else { path.as_str() },
                "traces": rows,
                "advice": if others_claim { "Another agent claims part of this area: coordinate (leave an info trace) or work elsewhere." } else { "" },
            }))
        }
        "clear" => {
            let removed = with_traces(&file, |traces| {
                let before = traces.len();
                traces.retain(|trace| !(trace.by == by && related(&trace.path, &path)));
                before - traces.len()
            })?;
            to_pretty_json(json!({ "cleared": removed, "by": by }))
        }
        other => Err(format!("unknown action `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::{import_name, import_words, related, Trace};

    #[test]
    fn traces_fade_by_half_each_half_life() {
        let trace = Trace {
            path: "src/a.rs".to_string(),
            kind: "claim".to_string(),
            note: String::new(),
            by: "x".to_string(),
            strength: 1.0,
            half_life_minutes: 30.0,
            at: 1_000,
        };
        assert!((trace.current(1_000) - 1.0).abs() < 1e-9);
        assert!((trace.current(1_000 + 30 * 60) - 0.5).abs() < 1e-9);
        assert!((trace.current(1_000 + 60 * 60) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn paths_relate_to_their_folders() {
        assert!(related("src/a.rs", "src"));
        assert!(related("src", "src/a.rs"));
        assert!(related("", "anything"));
        assert!(!related("src/a.rs", "src/ab.rs"));
        assert!(!related("srcx/a.rs", "src"));
    }

    #[test]
    fn reads_import_names() {
        assert_eq!(
            import_name("crates/runtime/src/bash.rs").as_deref(),
            Some("bash")
        );
        assert_eq!(
            import_name("crates/runtime/src/turn_keys/mod.rs").as_deref(),
            Some("turn_keys")
        );
        let words = import_words(
            "use crate::bash::execute;\nuse serde_json::{json, Value};\nuse std::fmt;\nmod leangate;\n#include \"llama-context.h\"\nimport { x } from './session-store';\nfrom pkg.loader import read, write\nlet y = bash;",
        );
        for expected in [
            "rs:bash",
            "rs:leangate",
            "c:llama-context.h",
            "js:session-store",
            "py:loader",
            "py:read",
        ] {
            assert!(words.contains(expected), "{expected}: {words:?}");
        }
        for unexpected in ["rs:json", "rs:fmt", "rs:Value", "rs:y", "js:json"] {
            assert!(!words.contains(unexpected), "{unexpected}: {words:?}");
        }
        assert_eq!(
            super::import_key("src/llama.h").as_deref(),
            Some("c:llama.h")
        );
        assert_eq!(
            super::import_key("src/llama.cpp").as_deref(),
            Some("c:llama.cpp")
        );
        assert_eq!(
            super::import_key("app/models/index.ts").as_deref(),
            Some("js:models")
        );
        let targets = vec!["a/x/m.h".to_string(), "b/y/m.h".to_string()];
        assert_eq!(
            super::nearest("b/y/z.cpp", &targets).map(String::as_str),
            Some("b/y/m.h")
        );
    }

    #[test]
    fn senses_what_others_left() {
        let _lock = crate::test_env_guard();
        let home = std::env::temp_dir().join(format!("claw-trail-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("CLAW_CONFIG_HOME", &home);
        let repo = env!("CARGO_MANIFEST_DIR");
        let call = |input: serde_json::Value| -> serde_json::Value {
            serde_json::from_str(&super::execute("Trail", &input).unwrap().unwrap()).unwrap()
        };
        call(
            serde_json::json!({"action": "leave", "path": format!("{repo}/src/lib.rs"), "kind": "claim", "note": "refactoring dispatch", "by": "agent-a", "repo": repo}),
        );
        let sensed = call(
            serde_json::json!({"action": "sense", "path": format!("{repo}/src"), "by": "agent-b", "repo": repo}),
        );
        assert_eq!(sensed["traces"][0]["kind"], "claim", "{sensed}");
        assert_eq!(sensed["traces"][0]["by"], "agent-a");
        assert!(
            sensed["advice"].as_str().unwrap().contains("Another agent"),
            "{sensed}"
        );
        let cleared = call(
            serde_json::json!({"action": "clear", "path": format!("{repo}/src"), "by": "agent-a", "repo": repo}),
        );
        assert_eq!(cleared["cleared"], 1);
        std::env::remove_var("CLAW_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
