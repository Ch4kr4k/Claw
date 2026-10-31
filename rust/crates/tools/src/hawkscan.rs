//! `HawkScan`: StackHawk's DAST scanner, as the HawkScan plugin gives it to
//! Claude Code. It runs `hawk` (or the `stackhawk/hawkscan` image through
//! docker or podman) and turns the `--json-output` report into findings
//! sorted by severity. The plugin's playbook (how to write `stackhawk.yml`,
//! auth recipes, triage) is served by the `guide` action from the Claude
//! Code plugin or a `hawkscan` skill, when one is installed.
//!
//! Commands go through the Bash executor, so they run in claw's sandbox,
//! stop on Esc and have a timeout like any shell command; when the sandbox
//! blocks the scanner the result says how to rerun it on the host.

use std::fs;
use std::path::{Path, PathBuf};

use runtime::{execute_bash, BashCommandInput, PermissionMode};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{to_pretty_json, ToolSpec};

const IMAGE: &str = "docker.io/stackhawk/hawkscan:latest";
const DEFAULT_TIMEOUT_SECS: u64 = 1800;
const MAX_PATHS_PER_FINDING: usize = 10;
const OUTPUT_TAIL: usize = 8000;

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "HawkScan",
        description: "StackHawk HawkScan DAST: scans a running web app or API for vulnerabilities (SQL injection, XSS, auth and header issues…) and returns findings sorted by severity. Use it when the user asks for a security/DAST scan, to test an app or API for vulnerabilities, or to verify a fix. Actions: \"status\" (is hawk or docker/podman there, is an API key set, which stackhawk*.yml configs exist); \"guide\" (the HawkScan playbook: read it before writing stackhawk.yml or a first scan; topic picks one reference, e.g. \"cli-reference\", \"auth-config\", \"findings-and-fixes\"); \"validate\" (hawk validate config|api|auth); \"scan\" (full scan; the app must be running); \"rescan\" (re-runs only the checks that fired on scan_id, fast fix verification). Needs a StackHawk API key (HAWK_API_KEY, or `hawk init --browser`). Configs are file names relative to path, never absolute.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["status", "guide", "validate", "scan", "rescan"] },
                "path": { "type": "string", "description": "Project directory holding the config (default: current directory)." },
                "configs": { "type": "array", "items": { "type": "string" }, "description": "Config files, merged in order (default stackhawk.yml)." },
                "kind": { "type": "string", "enum": ["config", "api", "auth"], "description": "What validate checks (default config)." },
                "scan_id": { "type": "string", "description": "For rescan: scan.id of the earlier scan." },
                "topic": { "type": "string", "description": "For guide: a reference name; omit for the main playbook and the list." },
                "hawk_mem": { "type": "string", "description": "Scanner heap, e.g. 12g (default 9g)." },
                "timeout_secs": { "type": "integer", "minimum": 30, "maximum": 14400 }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        required_permission: PermissionMode::DangerFullAccess,
    }]
}

pub(crate) fn execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    (name == "HawkScan").then(|| {
        serde_json::from_value::<HawkScanInput>(input.clone())
            .map_err(|error| error.to_string())
            .and_then(hawkscan)
    })
}

#[derive(Deserialize)]
struct HawkScanInput {
    action: String,
    path: Option<String>,
    #[serde(default)]
    configs: Vec<String>,
    kind: Option<String>,
    scan_id: Option<String>,
    topic: Option<String>,
    hawk_mem: Option<String>,
    timeout_secs: Option<u64>,
}

fn hawkscan(input: HawkScanInput) -> Result<String, String> {
    let dir = match input.path.as_deref() {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    let result = match input.action.as_str() {
        "status" => status(&dir),
        "guide" => return guide(input.topic.as_deref()),
        "validate" => validate(&dir, &input)?,
        "scan" | "rescan" => scan(&dir, &input)?,
        other => return Err(format!("unknown action: {other}")),
    };
    to_pretty_json(result)
}

// ───────────────────────────── environment ─────────────────────────────

/// How the scanner can be started.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Runner {
    Cli,
    /// `docker` or `podman`.
    Container(&'static str),
}

fn installed(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

fn runner() -> Option<Runner> {
    if installed("hawk") {
        Some(Runner::Cli)
    } else if installed("docker") {
        Some(Runner::Container("docker"))
    } else if installed("podman") {
        Some(Runner::Container("podman"))
    } else {
        None
    }
}

fn env_key() -> bool {
    std::env::var("HAWK_API_KEY").is_ok_and(|key| !key.trim().is_empty())
}

fn stored_key() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let file = Path::new(&home).join(".hawk").join("hawk.properties");
    file.is_file().then_some(file)
}

fn configs_in(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.starts_with("stackhawk") && (name.ends_with(".yml") || name.ends_with(".yaml"))
        })
        .collect();
    found.sort();
    found
}

fn status(dir: &Path) -> Value {
    let runner = runner();
    let version = match &runner {
        Some(Runner::Cli) => run("hawk version", 60).ok().map(|out| out.text),
        _ => None,
    };
    let playbook = playbook_dir();
    let mut problems = Vec::new();
    if runner.is_none() {
        problems.push("neither hawk nor docker/podman is installed: install hawk (https://docs.stackhawk.com/download.html) or docker".to_string());
    }
    if !env_key() && stored_key().is_none() {
        problems
            .push("no API key: set HAWK_API_KEY, or run `hawk init --browser` once".to_string());
    }
    if matches!(runner, Some(Runner::Container(_))) && !env_key() {
        problems.push("the container needs HAWK_API_KEY in the environment".to_string());
    }
    let configs = configs_in(dir);
    if configs.is_empty() {
        problems.push(format!(
            "no stackhawk*.yml in {}: write one first (HawkScan action \"guide\")",
            dir.display()
        ));
    }
    json!({
        "runner": match &runner {
            Some(Runner::Cli) => "hawk".to_string(),
            Some(Runner::Container(program)) => format!("{program} ({IMAGE})"),
            None => "none".to_string(),
        },
        "hawk_version": version.map(|text| text.trim().to_string()),
        "api_key": if env_key() { "HAWK_API_KEY" } else if stored_key().is_some() { "~/.hawk/hawk.properties" } else { "missing" },
        "configs": configs,
        "playbook": playbook.map(|dir| dir.display().to_string()),
        "ready": problems.is_empty(),
        "problems": problems,
    })
}

// ───────────────────────────── guide ─────────────────────────────

/// The newest `hawkscan` skill directory: claw's or Claude Code's skills,
/// or a Claude Code plugin (installed or synced).
fn playbook_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let direct = [
        home.join(".claw/skills/hawkscan"),
        home.join(".claude/skills/hawkscan"),
    ];
    if let Some(dir) = direct
        .into_iter()
        .find(|dir| dir.join("SKILL.md").is_file())
    {
        return Some(dir);
    }
    let mut found = Vec::new();
    find_skill_dirs(&home.join(".claude/plugins"), 0, &mut found);
    found
        .into_iter()
        .filter(|dir| !dir.components().any(|part| part.as_os_str() == ".trash"))
        .max_by_key(|dir| {
            fs::metadata(dir.join("SKILL.md"))
                .and_then(|meta| meta.modified())
                .ok()
        })
}

/// `…/skills/hawkscan` directories with a SKILL.md, at most a few levels down.
fn find_skill_dirs(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if path.ends_with("skills/hawkscan") && path.join("SKILL.md").is_file() {
            found.push(path);
        } else {
            find_skill_dirs(&path, depth + 1, found);
        }
    }
}

const FALLBACK_GUIDE: &str = "No HawkScan playbook is installed (the Claude Code HawkScan plugin, or a `hawkscan` skill in ~/.claw/skills). The loop without it:\n\
1. Start the app and note its URL.\n\
2. Write stackhawk.yml: `app.applicationId` (`hawk create app --name <name> --env Development` prints one), `app.env`, `app.host`; add `app.openApiConf` for an API, `app.authentication` for logins (`hawk config show app.authentication --text`).\n\
3. HawkScan action \"validate\" (kind config, then auth if there is a login).\n\
4. Action \"scan\"; fix every finding, worst first.\n\
5. Action \"rescan\" with the scan id to verify the fixes.\n\
The full playbook: https://github.com/stackhawk/agent-skills (copy skills/hawkscan to ~/.claw/skills/hawkscan).";

fn guide(topic: Option<&str>) -> Result<String, String> {
    let Some(dir) = playbook_dir() else {
        return Ok(FALLBACK_GUIDE.to_string());
    };
    let references = dir.join("references");
    let mut topics: Vec<String> = fs::read_dir(&references)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".md"))
                .map(str::to_string)
        })
        .collect();
    topics.sort();
    let file = match topic.map(|topic| topic.trim().trim_end_matches(".md")) {
        None | Some("") => dir.join("SKILL.md"),
        Some(topic) if topics.iter().any(|name| name == topic) => {
            references.join(format!("{topic}.md"))
        }
        Some(topic) => {
            return Err(format!(
                "unknown topic {topic:?}; topics: {}",
                topics.join(", ")
            ))
        }
    };
    let text = fs::read_to_string(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    // The playbook is written for Claude Code: its `references/x.md` links
    // are this tool's topics here.
    Ok(format!(
        "[{}]\n[In claw, run the hawk commands below with the HawkScan tool where it has an action (status, validate, scan, rescan) and with Bash otherwise; read a references/<name>.md file with HawkScan action \"guide\", topic \"<name>\". Topics: {}]\n\n{text}",
        file.display(),
        topics.join(", ")
    ))
}

// ───────────────────────────── running ─────────────────────────────

struct RunOutput {
    text: String,
    exit_code: i32,
    timed_out: bool,
    interrupted: bool,
}

fn run(command: &str, timeout_secs: u64) -> Result<RunOutput, String> {
    let output = execute_bash(BashCommandInput {
        command: command.to_string(),
        timeout: Some(timeout_secs * 1000),
        description: Some("HawkScan".to_string()),
        run_in_background: None,
        dangerously_disable_sandbox: None,
        namespace_restrictions: None,
        isolate_network: None,
        filesystem_mode: None,
        allowed_mounts: None,
    })
    .map_err(|error| error.to_string())?;
    let code = output.return_code_interpretation.as_deref();
    let exit_code = code
        .and_then(|code| code.strip_prefix("exit_code:"))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let mut text = output.stdout;
    if !output.stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&output.stderr);
    }
    Ok(RunOutput {
        text,
        exit_code,
        timed_out: matches!(code, Some("timeout" | "test.hung")),
        interrupted: output.interrupted,
    })
}

/// `value` in single quotes for the shell.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn checked_configs(configs: &[String]) -> Result<Vec<String>, String> {
    for config in configs {
        if config.starts_with('/') {
            return Err(format!(
                "config {config:?} is absolute: give file names relative to path (hawk prepends the project directory)"
            ));
        }
    }
    Ok(configs.iter().map(|config| quote(config)).collect())
}

/// The API key from the environment, for the scanner (named, not
/// expanded here, so it never shows in the command line).
fn key_prefix() -> &'static str {
    if env_key() {
        "API_KEY=\"$HAWK_API_KEY\" "
    } else {
        ""
    }
}

fn validate(dir: &Path, input: &HawkScanInput) -> Result<Value, String> {
    if runner() != Some(Runner::Cli) {
        return Err("validate needs the hawk CLI (https://docs.stackhawk.com/download.html); with docker only, run a scan and read its errors".to_string());
    }
    let kind = input.kind.as_deref().unwrap_or("config");
    if !matches!(kind, "config" | "api" | "auth") {
        return Err(format!("unknown kind: {kind}"));
    }
    let mut configs = checked_configs(&input.configs)?;
    if configs.is_empty() {
        configs.push(quote("stackhawk.yml"));
    }
    let command = format!(
        "cd {} && {}hawk --no-color validate {kind} {}",
        quote(&dir.display().to_string()),
        key_prefix(),
        configs.join(" ")
    );
    let out = run(&command, input.timeout_secs.unwrap_or(300))?;
    Ok(json!({
        "command": command,
        "ok": out.exit_code == 0 && !out.timed_out && !out.interrupted,
        "exit_code": out.exit_code,
        "output": tail(&out.text),
        "sandbox_note": sandbox_note(&out),
    }))
}

fn scan(dir: &Path, input: &HawkScanInput) -> Result<Value, String> {
    let runner = runner().ok_or(
        "neither hawk nor docker/podman is installed: install hawk (https://docs.stackhawk.com/download.html) or docker",
    )?;
    let rescan = input.action == "rescan";
    let configs = checked_configs(&input.configs)?;
    let memory = match input.hawk_mem.as_deref() {
        Some(mem) if mem.chars().all(|ch| ch.is_ascii_alphanumeric()) => {
            format!(" --hawk-mem={mem}")
        }
        Some(mem) => return Err(format!("bad hawk_mem: {mem:?}")),
        None => String::new(),
    };
    let dir_arg = quote(&dir.display().to_string());
    let command = match (&runner, rescan) {
        (Runner::Cli, false) => format!(
            "cd {dir_arg} && {}hawk --no-color scan --json-output{memory} {}",
            key_prefix(),
            configs.join(" ")
        ),
        (Runner::Cli, true) => {
            let id = input
                .scan_id
                .as_deref()
                .filter(|id| id.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-'))
                .ok_or("rescan needs scan_id (scan.id from the earlier scan)")?;
            format!(
                "cd {dir_arg} && {}hawk --no-color rescan --scan-id {id} --json-output",
                key_prefix()
            )
        }
        (Runner::Container(_), true) => {
            return Err(
                "rescan needs the hawk CLI; run a full scan with docker instead".to_string(),
            )
        }
        (Runner::Container(program), false) => {
            if !env_key() {
                return Err(
                    "the scanner container needs HAWK_API_KEY in the environment".to_string(),
                );
            }
            // Host networking so `localhost` in stackhawk.yml is the host.
            format!(
                "{program} run --rm -v {dir_arg}:/hawk:rw --network host -e API_KEY=\"$HAWK_API_KEY\" {IMAGE} --json-output {}",
                configs.join(" ")
            )
        }
    };
    let out = run(&command, input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS))?;
    let mut result = summarize(&out.text).unwrap_or_else(|| json!({ "output": tail(&out.text) }));
    let object = result.as_object_mut().expect("summary is an object");
    object.insert("command".to_string(), json!(command));
    object.insert("exit_code".to_string(), json!(out.exit_code));
    object.insert(
        "outcome".to_string(),
        json!(match (out.timed_out, out.interrupted, out.exit_code) {
            (true, _, _) => "timed out",
            (_, true, _) => "stopped",
            (_, _, 0) => "complete: nothing at or above the failure threshold",
            (_, _, 42) => "complete: findings at or above the failure threshold",
            _ => "failed (config error, app unreachable or auth failure: see errors/output)",
        }),
    );
    if let Some(note) = sandbox_note(&out) {
        object.insert("sandbox_note".to_string(), json!(note));
    }
    Ok(result)
}

/// Findings worst first, each with its first few paths.
fn summarize(output: &str) -> Option<Value> {
    let start = output.find('{')?;
    let end = output.rfind('}')?;
    let report: Value = serde_json::from_str(output.get(start..=end)?).ok()?;
    let rank = |severity: &str| match severity.to_ascii_uppercase().as_str() {
        "CRITICAL" => 0,
        "HIGH" => 1,
        "MEDIUM" => 2,
        "LOW" => 3,
        _ => 4,
    };
    let mut findings: Vec<Value> = report
        .get("findings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    findings.sort_by_key(|finding| {
        rank(
            finding
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )
    });
    let mut counts = serde_json::Map::new();
    for finding in &mut findings {
        let severity = finding
            .get("severity")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_ascii_uppercase();
        let entry = counts.entry(severity).or_insert(json!(0));
        *entry = json!(entry.as_u64().unwrap_or(0) + 1);
        if let Some(paths) = finding.get_mut("paths").and_then(Value::as_array_mut) {
            if paths.len() > MAX_PATHS_PER_FINDING {
                let more = paths.len() - MAX_PATHS_PER_FINDING;
                paths.truncate(MAX_PATHS_PER_FINDING);
                paths.push(json!(format!("… {more} more paths")));
            }
        }
    }
    Some(json!({
        "scan": report.get("scan"),
        "threshold_result": report.get("thresholdResult"),
        "counts_by_severity": counts,
        "findings": findings,
        "errors": report.get("errors"),
        "warnings": report.get("warnings"),
        "next": "Fix every finding, worst first (HawkScan guide topic \"findings-and-fixes\"), then verify with action \"rescan\" and scan_id = scan.id.",
    }))
}

/// Says how to leave the sandbox when it looks like it stopped the scanner.
fn sandbox_note(out: &RunOutput) -> Option<String> {
    let text = out.text.to_ascii_lowercase();
    let blocked = out.exit_code != 0
        && (text.contains("[sandbox]")
            || text.contains("read-only file system")
            || text.contains("permission denied")
            || text.contains("docker.sock"));
    blocked.then(|| {
        "claw's sandbox may have blocked the scanner (it writes ~/.hawk and needs the container socket and the network). Rerun the command above with Bash and dangerouslyDisableSandbox=true; the user is asked to approve it.".to_string()
    })
}

fn tail(text: &str) -> String {
    let text = text.trim();
    if text.len() <= OUTPUT_TAIL {
        return text.to_string();
    }
    let mut start = text.len() - OUTPUT_TAIL;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…(earlier output cut)\n{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::{checked_configs, quote, summarize};

    #[test]
    fn findings_come_worst_first_with_counts() {
        let output = r#"pulling image…
{
  "scan": { "id": "1234-abcd", "status": "COMPLETED" },
  "findings": [
    { "name": "Missing header", "severity": "LOW", "paths": [] },
    { "name": "SQL Injection", "severity": "HIGH", "paths": [{ "path": "/a" }] },
    { "name": "Cookie", "severity": "LOW", "paths": [] }
  ],
  "errors": [],
  "warnings": [],
  "thresholdResult": "FAIL"
}"#;
        let summary = summarize(output).expect("report");
        assert_eq!(summary["findings"][0]["name"], "SQL Injection");
        assert_eq!(summary["counts_by_severity"]["LOW"], 2);
        assert_eq!(summary["counts_by_severity"]["HIGH"], 1);
        assert_eq!(summary["scan"]["id"], "1234-abcd");
        assert!(summarize("Error: app unreachable").is_none());
    }

    #[test]
    fn configs_must_be_relative_and_are_quoted() {
        assert!(checked_configs(&["/abs/stackhawk.yml".to_string()]).is_err());
        assert_eq!(
            checked_configs(&["it's.yml".to_string()]).expect("relative"),
            vec![r"'it'\''s.yml'".to_string()]
        );
        assert_eq!(quote("a b"), "'a b'");
    }
}
