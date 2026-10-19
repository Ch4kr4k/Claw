//! Developer tools beyond the basics:
//! - `GoogleDork`: builds a search-operator query (site:, filetype:,
//!   intitle:, inurl:, intext:, exclusions, exact phrases, dates) and runs
//!   it through the WebSearch backend, with ready links for Google and Bing.
//! - `Checkpoint`: git snapshots of the work tree (in claw's own snapshot
//!   repository, never the project's history) before risky edits, with
//!   list, diff and restore, so a batch of changes can be undone.
//! - `Diagnostics`: runs the project's type checkers, linters and
//!   formatters (cargo, ruff/pyright/mypy, tsc/eslint/prettier, go vet/gofmt,
//!   shellcheck, clang-tidy), whichever the project uses and are installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use runtime::{GitSnapshots, PermissionMode};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{to_pretty_json, ToolSpec};

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "GoogleDork",
            description: "Advanced web search with search operators (Google dorking) for research and OSINT on targets you are allowed to look at: builds a query from site, filetype/ext, intitle, inurl, intext, exact phrases, excluded words or sites, and a date range, runs it through the WebSearch backend, and returns the results plus Google and Bing links for the same query. Use it to find documentation on one site, files of a type (pdf, xlsx, log), exposed index pages or config files on your own domains, or pages mentioning an exact error message.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Free words to search for." },
                    "site": { "type": "string", "description": "Limit to a domain, e.g. docs.rs or *.example.com." },
                    "filetype": { "type": "string", "description": "File extension: pdf, docx, xlsx, log, sql, env…" },
                    "intitle": { "type": "string" },
                    "inurl": { "type": "string" },
                    "intext": { "type": "string" },
                    "exact": { "type": "array", "items": { "type": "string" }, "description": "Exact phrases." },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Words to leave out." },
                    "exclude_sites": { "type": "array", "items": { "type": "string" } },
                    "after": { "type": "string", "description": "YYYY-MM-DD" },
                    "before": { "type": "string", "description": "YYYY-MM-DD" },
                    "run": { "type": "boolean", "description": "Run the search (default true); false only builds the query." }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Checkpoint",
            description: "Snapshots of the working tree for undo, kept in claw's own git snapshot store (the project's git history and index are not touched; untracked files are included, ignored ones are not). action \"save\" before a risky batch of edits (give a label); \"list\" shows this session's checkpoints; \"diff\" shows what changed since a checkpoint; \"restore\" puts files back to a checkpoint (it first saves the current state, returned as undo_id, so a restore can itself be undone). Restore only reverts files that changed since the checkpoint.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["save", "list", "diff", "restore"] },
                    "label": { "type": "string" },
                    "id": { "type": "string", "description": "Checkpoint id (from save or list)." },
                    "path": { "type": "string", "description": "Work tree (default: current directory)." }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "Diagnostics",
            description: "Run the project's type checkers, linters and formatters after edits and get their findings: Rust (cargo check, cargo clippy, cargo fmt), Python (pyright, mypy, ruff, black), TypeScript/JavaScript (tsc --noEmit, eslint, prettier), Go (go vet, gofmt), shell (shellcheck), C/C++ (clang-tidy on a given file). It detects which apply from the project files and runs only installed ones. checks: any of \"types\", \"lint\", \"format\" (default types and lint). fix=true lets formatters and linters rewrite files. For one file's live diagnostics from a language server, use LSP.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Project directory or a file in it (default: current directory)." },
                    "checks": { "type": "array", "items": { "type": "string", "enum": ["types", "lint", "format"] } },
                    "fix": { "type": "boolean" },
                    "timeout_secs": { "type": "integer", "minimum": 10, "maximum": 1800 }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
    ]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    let parse = |input: &Value| input.clone();
    Some(match name {
        "GoogleDork" => from(parse(input)).and_then(google_dork),
        "Checkpoint" => from(parse(input)).and_then(checkpoint),
        "Diagnostics" => from(parse(input)).and_then(diagnostics),
        _ => return None,
    })
}

fn from<T: for<'de> Deserialize<'de>>(input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|error| error.to_string())
}

// ───────────────────────────── GoogleDork ─────────────────────────────

#[derive(Deserialize, Default)]
struct DorkInput {
    query: Option<String>,
    site: Option<String>,
    filetype: Option<String>,
    intitle: Option<String>,
    inurl: Option<String>,
    intext: Option<String>,
    #[serde(default)]
    exact: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    exclude_sites: Vec<String>,
    after: Option<String>,
    before: Option<String>,
    run: Option<bool>,
}

/// Quotes a value that has spaces, for an operator or a phrase.
fn quoted(value: &str) -> String {
    let value = value.trim().replace('"', "");
    if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value
    }
}

fn build_dork(input: &DorkInput) -> String {
    let mut parts = Vec::new();
    if let Some(query) = input.query.as_deref().filter(|q| !q.trim().is_empty()) {
        parts.push(query.trim().to_string());
    }
    for phrase in &input.exact {
        if !phrase.trim().is_empty() {
            parts.push(format!("\"{}\"", phrase.trim().replace('"', "")));
        }
    }
    let operators = [
        ("site", &input.site),
        ("filetype", &input.filetype),
        ("intitle", &input.intitle),
        ("inurl", &input.inurl),
        ("intext", &input.intext),
        ("after", &input.after),
        ("before", &input.before),
    ];
    for (operator, value) in operators {
        if let Some(value) = value.as_deref().filter(|v| !v.trim().is_empty()) {
            let value = if operator == "filetype" {
                value.trim().trim_start_matches('.').to_string()
            } else {
                quoted(value)
            };
            parts.push(format!("{operator}:{value}"));
        }
    }
    for word in &input.exclude {
        if !word.trim().is_empty() {
            parts.push(format!("-{}", quoted(word)));
        }
    }
    for site in &input.exclude_sites {
        if !site.trim().is_empty() {
            parts.push(format!("-site:{}", site.trim()));
        }
    }
    parts.join(" ")
}

fn google_dork(input: DorkInput) -> Result<String, String> {
    let dork = build_dork(&input);
    if dork.is_empty() {
        return Err(
            "give a query or at least one operator (site, filetype, intitle, …)".to_string(),
        );
    }
    let encode = |base: &str| {
        reqwest::Url::parse_with_params(base, &[("q", dork.as_str())])
            .map(|url| url.to_string())
            .unwrap_or_default()
    };
    let mut out = json!({
        "dork": dork,
        "google_url": encode("https://www.google.com/search"),
        "bing_url": encode("https://www.bing.com/search"),
        "duckduckgo_url": encode("https://duckduckgo.com/"),
    });
    if input.run.unwrap_or(true) {
        let search = super::WebSearchInput {
            query: dork.clone(),
            allowed_domains: None,
            blocked_domains: None,
        };
        match super::execute_web_search(&search) {
            Ok(results) => {
                out["results"] = serde_json::to_value(results).unwrap_or(Value::Null);
            }
            Err(error) => {
                out["search_error"] = json!(error);
                out["note"] = json!("The search backend failed; open google_url instead.");
            }
        }
        out["backend_note"] = json!(
            "Results come from the WebSearch backend (DuckDuckGo unless CLAWD_WEB_SEARCH_BASE_URL is set), which honours site:, filetype:, intitle: and inurl: but not every Google operator; google_url runs the exact query on Google."
        );
    }
    to_pretty_json(out)
}

// ───────────────────────────── Checkpoint ─────────────────────────────

#[derive(Deserialize)]
struct CheckpointInput {
    action: String,
    label: Option<String>,
    id: Option<String>,
    path: Option<String>,
}

struct Saved {
    id: String,
    label: String,
    root: PathBuf,
    at: String,
}

static SAVED: Mutex<Vec<Saved>> = Mutex::new(Vec::new());

fn checkpoint(input: CheckpointInput) -> Result<String, String> {
    let root = std::fs::canonicalize(input.path.as_deref().unwrap_or("."))
        .map_err(|error| format!("cannot open the work tree: {error}"))?;
    let snapshots = GitSnapshots::open(&root);
    if !snapshots.is_available() {
        return Err(format!(
            "snapshots are unavailable here: {}",
            snapshots
                .unavailable_reason()
                .unwrap_or("not a git work tree")
        ));
    }
    let mut saved = SAVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match input.action.as_str() {
        "save" => {
            let label = input
                .label
                .filter(|label| !label.trim().is_empty())
                .unwrap_or_else(|| "checkpoint".to_string());
            let id = snapshots
                .snapshot(&format!("claw checkpoint: {label}"))
                .ok_or("could not take a snapshot")?;
            let at = now();
            saved.push(Saved {
                id: id.clone(),
                label: label.clone(),
                root: root.clone(),
                at: at.clone(),
            });
            to_pretty_json(json!({ "id": id, "label": label, "saved_at": at }))
        }
        "list" => {
            let rows: Vec<Value> = saved
                .iter()
                .rev()
                .filter(|cp| cp.root == root)
                .map(|cp| json!({ "id": cp.id, "label": cp.label, "saved_at": cp.at }))
                .collect();
            to_pretty_json(json!({ "checkpoints": rows }))
        }
        "diff" => {
            let id = resolve_id(&saved, &root, input.id.as_deref())?;
            let diff = snapshots
                .diff(&id, None)
                .unwrap_or_else(|| "no changes since the checkpoint".to_string());
            let limit = 30_000;
            let text = if diff.len() > limit {
                let mut end = limit;
                while !diff.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}\n… diff truncated ({} bytes)", &diff[..end], diff.len())
            } else {
                diff
            };
            to_pretty_json(json!({ "id": id, "diff": text }))
        }
        "restore" => {
            let id = resolve_id(&saved, &root, input.id.as_deref())?;
            let current = snapshots
                .snapshot("claw checkpoint: before restore")
                .ok_or("could not snapshot the current state")?;
            let actions: Vec<String> = snapshots
                .restore(&id, &current)
                .into_iter()
                .map(|action| action.to_string())
                .collect();
            saved.push(Saved {
                id: current.clone(),
                label: format!("before restoring {}", short(&id)),
                root: root.clone(),
                at: now(),
            });
            to_pretty_json(json!({
                "restored_to": id,
                "undo_id": current,
                "actions": if actions.is_empty() { vec!["nothing to restore".to_string()] } else { actions },
            }))
        }
        other => Err(format!("unknown action `{other}`")),
    }
}

fn resolve_id(saved: &[Saved], root: &Path, id: Option<&str>) -> Result<String, String> {
    match id.filter(|id| !id.trim().is_empty()) {
        Some(id) => Ok(saved
            .iter()
            .rev()
            .find(|cp| cp.root == root && cp.id.starts_with(id.trim()))
            .map_or_else(|| id.trim().to_string(), |cp| cp.id.clone())),
        None => saved
            .iter()
            .rev()
            .find(|cp| cp.root == root)
            .map(|cp| cp.id.clone())
            .ok_or_else(|| "no checkpoint saved yet; give an id or save one first".to_string()),
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(10)]
}

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs());
    format!("unix:{secs}")
}

// ───────────────────────────── Diagnostics ─────────────────────────────

#[derive(Deserialize)]
struct DiagnosticsInput {
    path: Option<String>,
    checks: Option<Vec<String>>,
    fix: Option<bool>,
    timeout_secs: Option<u64>,
}

pub(crate) struct Check {
    tool: &'static str,
    pub(crate) kind: &'static str,
    program: String,
    args: Vec<String>,
}

fn installed(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// The nearest directory at or above `start` holding one of `markers`.
fn find_up(start: &Path, markers: &[&str]) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| markers.iter().any(|marker| dir.join(marker).exists()))
        .map(Path::to_path_buf)
}

pub(crate) fn plan_checks(target: &Path, fix: bool) -> (PathBuf, Vec<Check>) {
    let dir = if target.is_file() {
        target.parent().unwrap_or(target).to_path_buf()
    } else {
        target.to_path_buf()
    };
    let file_ext = target.is_file().then(|| {
        target
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string()
    });
    let check = |tool: &'static str, kind: &'static str, program: &str, args: &[&str]| Check {
        tool,
        kind,
        program: program.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
    };
    let mut checks = Vec::new();
    let mut root = dir.clone();
    if let Some(cargo) = find_up(&dir, &["Cargo.toml"]) {
        root = cargo;
        if installed("cargo") {
            checks.push(check(
                "cargo check",
                "types",
                "cargo",
                &["check", "--all-targets", "--message-format", "short"],
            ));
            if fix {
                checks.push(check(
                    "cargo clippy",
                    "lint",
                    "cargo",
                    &[
                        "clippy",
                        "--all-targets",
                        "--fix",
                        "--allow-dirty",
                        "--allow-staged",
                        "--message-format",
                        "short",
                    ],
                ));
                checks.push(check("cargo fmt", "format", "cargo", &["fmt"]));
            } else {
                checks.push(check(
                    "cargo clippy",
                    "lint",
                    "cargo",
                    &["clippy", "--all-targets", "--message-format", "short"],
                ));
                checks.push(check("cargo fmt", "format", "cargo", &["fmt", "--check"]));
            }
        }
    }
    let python = find_up(
        &dir,
        &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
    )
    .or_else(|| (file_ext.as_deref() == Some("py")).then(|| dir.clone()));
    if let Some(python) = python {
        root = python;
        let target_arg = if target.is_file() {
            target.to_string_lossy().into_owned()
        } else {
            ".".to_string()
        };
        if installed("pyright") {
            checks.push(check("pyright", "types", "pyright", &[&target_arg]));
        } else if installed("mypy") {
            checks.push(check("mypy", "types", "mypy", &[&target_arg]));
        }
        if installed("ruff") {
            if fix {
                checks.push(check(
                    "ruff check",
                    "lint",
                    "ruff",
                    &["check", "--fix", &target_arg],
                ));
                checks.push(check(
                    "ruff format",
                    "format",
                    "ruff",
                    &["format", &target_arg],
                ));
            } else {
                checks.push(check("ruff check", "lint", "ruff", &["check", &target_arg]));
                checks.push(check(
                    "ruff format",
                    "format",
                    "ruff",
                    &["format", "--check", &target_arg],
                ));
            }
        } else if installed("black") {
            checks.push(check(
                "black",
                "format",
                "black",
                if fix {
                    &["."][..]
                } else {
                    &["--check", "."][..]
                },
            ));
        }
    }
    if let Some(node) = find_up(&dir, &["package.json", "tsconfig.json"]) {
        root = node.clone();
        let local = |name: &str| node.join("node_modules/.bin").join(name);
        let pick = |name: &str| {
            let local = local(name);
            if local.is_file() {
                Some(local.to_string_lossy().into_owned())
            } else if installed(name) {
                Some(name.to_string())
            } else {
                None
            }
        };
        if node.join("tsconfig.json").exists() {
            if let Some(tsc) = pick("tsc") {
                checks.push(check(
                    "tsc",
                    "types",
                    &tsc,
                    &["--noEmit", "--pretty", "false"],
                ));
            }
        }
        if let Some(eslint) = pick("eslint") {
            checks.push(check(
                "eslint",
                "lint",
                &eslint,
                if fix { &["--fix", "."][..] } else { &["."][..] },
            ));
        }
        if let Some(prettier) = pick("prettier") {
            checks.push(check(
                "prettier",
                "format",
                &prettier,
                if fix {
                    &["--write", "."][..]
                } else {
                    &["--check", "."][..]
                },
            ));
        }
    }
    if let Some(go) = find_up(&dir, &["go.mod"]) {
        root = go;
        if installed("go") {
            checks.push(check("go vet", "lint", "go", &["vet", "./..."]));
            checks.push(check("go build", "types", "go", &["build", "./..."]));
        }
        if installed("gofmt") {
            checks.push(check(
                "gofmt",
                "format",
                "gofmt",
                if fix {
                    &["-w", "."][..]
                } else {
                    &["-l", "."][..]
                },
            ));
        }
    }
    if let Some(ext) = file_ext.as_deref() {
        let file = target.to_string_lossy().into_owned();
        if matches!(ext, "sh" | "bash") && installed("shellcheck") {
            checks.push(check("shellcheck", "lint", "shellcheck", &[&file]));
        }
        if matches!(ext, "c" | "cc" | "cpp" | "cxx" | "cu") && installed("clang-tidy") {
            let mut args = vec![file.clone()];
            if let Some(db) = find_up(
                &dir,
                &["compile_commands.json", "build/compile_commands.json"],
            ) {
                let db_dir = if db.join("compile_commands.json").exists() {
                    db
                } else {
                    db.join("build")
                };
                args.push(format!("-p={}", db_dir.display()));
            }
            checks.push(Check {
                tool: "clang-tidy",
                kind: "lint",
                program: "clang-tidy".to_string(),
                args,
            });
        }
    }
    (root, checks)
}

fn diagnostics(input: DiagnosticsInput) -> Result<String, String> {
    let target = std::fs::canonicalize(input.path.as_deref().unwrap_or("."))
        .map_err(|error| format!("cannot open the path: {error}"))?;
    let fix = input.fix.unwrap_or(false);
    let wanted = input
        .checks
        .unwrap_or_else(|| vec!["types".to_string(), "lint".to_string()]);
    let (root, checks) = plan_checks(&target, fix);
    let checks: Vec<Check> = checks
        .into_iter()
        .filter(|check| {
            wanted.iter().any(|kind| kind == check.kind) || (fix && check.kind == "format")
        })
        .collect();
    if checks.is_empty() {
        return to_pretty_json(json!({
            "root": root.display().to_string(),
            "results": [],
            "note": "No checker applies here or none is installed (looked for cargo, pyright/mypy, ruff/black, tsc/eslint/prettier, go, shellcheck, clang-tidy). Use LSP for a single file's diagnostics.",
        }));
    }
    let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(300));
    let results: Vec<Value> = checks
        .iter()
        .map(|check| run_check(check, &root, timeout))
        .collect();
    let clean = results.iter().all(|result| result["ok"] == true);
    // RepoSignals reads which files the failing checks named.
    let failing_output: String = results
        .iter()
        .filter(|result| result["ok"] != true)
        .filter_map(|result| result["output"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    super::signals::record_failures(&root, "Diagnostics", &failing_output);
    to_pretty_json(json!({
        "root": root.display().to_string(),
        "clean": clean,
        "results": results,
    }))
}

pub(crate) fn run_check(check: &Check, root: &Path, timeout: Duration) -> Value {
    let command_line = format!("{} {}", check.program, check.args.join(" "));
    let child = Command::new(&check.program)
        .args(&check.args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            return json!({ "tool": check.tool, "kind": check.kind, "command": command_line, "ok": false, "output": format!("could not start: {error}") });
        }
    };
    // Read both pipes on threads so a chatty checker cannot block on a full pipe.
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut stream| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut stream, &mut text);
            text
        })
    })
    .collect();
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break None,
        }
    };
    let output: String = readers
        .into_iter()
        .filter_map(|reader| reader.join().ok())
        .collect::<Vec<_>>()
        .join("\n");
    let output = output.trim();
    let limit = 8000;
    let shown = if output.len() > limit {
        let mut start = output.len() - limit;
        while !output.is_char_boundary(start) {
            start += 1;
        }
        format!("…(earlier output cut)\n{}", &output[start..])
    } else {
        output.to_string()
    };
    json!({
        "tool": check.tool,
        "kind": check.kind,
        "command": command_line,
        "ok": status.is_some_and(|status| status.success()),
        "exit_code": status.and_then(|status| status.code()),
        "timed_out": status.is_none(),
        "seconds": started.elapsed().as_secs(),
        "output": shown,
    })
}

#[cfg(test)]
mod tests {
    use super::{build_dork, DorkInput};

    #[test]
    fn builds_dork_queries() {
        let input = DorkInput {
            query: Some("index of".to_string()),
            site: Some("example.com".to_string()),
            filetype: Some(".pdf".to_string()),
            intitle: Some("annual report".to_string()),
            exact: vec!["internal use only".to_string()],
            exclude: vec!["draft".to_string()],
            exclude_sites: vec!["www.example.com".to_string()],
            ..DorkInput::default()
        };
        assert_eq!(
            build_dork(&input),
            "index of \"internal use only\" site:example.com filetype:pdf intitle:\"annual report\" -draft -site:www.example.com"
        );
    }

    #[test]
    fn diagnostics_plans_cargo_checks_for_this_crate() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let (root, checks) = super::plan_checks(here, false);
        assert!(root.join("Cargo.toml").exists());
        if super::installed("cargo") {
            assert!(checks.iter().any(|check| check.tool == "cargo check"));
            assert!(checks
                .iter()
                .any(|check| check.args.contains(&"--check".to_string())));
        }
    }

    #[test]
    fn checkpoint_saves_diffs_and_restores() {
        let _lock = crate::test_env_guard();
        let dir = std::env::temp_dir().join(format!("claw-checkpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let config = dir.join("config-home");
        std::env::set_var("CLAW_CONFIG_HOME", &config);
        let call = |input: serde_json::Value| -> serde_json::Value {
            let text = super::execute("Checkpoint", &input).unwrap().unwrap();
            serde_json::from_str(&text).unwrap()
        };
        let path = dir.display().to_string();
        let saved = call(serde_json::json!({"action": "save", "label": "before", "path": path}));
        if saved.get("id").is_none() {
            return;
        }
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let diff = call(serde_json::json!({"action": "diff", "path": path}));
        assert!(diff["diff"].as_str().unwrap().contains("+two"), "{diff}");
        let restored = call(serde_json::json!({"action": "restore", "path": path}));
        assert!(restored["undo_id"].is_string(), "{restored}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        std::env::remove_var("CLAW_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
