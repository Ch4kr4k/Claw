use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{
    allowed_tools_for_subagent, build_agent_system_prompt, classify_lane_failure,
    derive_agent_state, execute_agent_with_spawn, execute_tool, extract_recovery_outcome,
    global_cron_registry, maybe_commit_provenance, mvp_tool_specs, permission_mode_from_plugin,
    persist_agent_terminal_state, run_subagent_tool, run_task_packet, AgentInput, AgentJob,
    GlobalToolRegistry, LaneEventName, LaneFailureClass,
};
use runtime::{
    permission_enforcer::PermissionEnforcer, PermissionMode, PermissionPolicy, Session, TaskPacket,
};
use serde_json::json;

fn env_lock() -> &'static Mutex<()> {
    &crate::TEST_ENV_LOCK
}

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn env_guard_recovers_after_poisoning() {
    let poisoned = std::thread::spawn(|| {
        let _guard = env_guard();
        panic!("poison env lock");
    })
    .join();
    assert!(poisoned.is_err(), "poisoning thread should panic");

    let _guard = env_guard();
}

#[test]
fn existing_files_must_be_read_before_they_are_changed() {
    let root = temp_path("read-before-write");
    fs::create_dir_all(&root).expect("create root");
    let file = root.join("main.rs");
    let path = file.to_str().expect("utf-8 path");
    fs::write(&file, "fn main() {}\n").expect("seed file");

    let unread = execute_tool("write_file", &json!({ "path": path, "content": "x\n" }))
        .expect_err("an unread file is refused");
    assert!(
        unread.contains("have not read it with read_file"),
        "{unread}"
    );
    let unread_edit = execute_tool(
        "edit_file",
        &json!({ "path": path, "old_string": "main", "new_string": "start" }),
    )
    .expect_err("edit_file is refused too");
    assert!(unread_edit.contains("have not read it"), "{unread_edit}");

    execute_tool("read_file", &json!({ "path": path })).expect("read");
    execute_tool(
        "edit_file",
        &json!({ "path": path, "old_string": "main", "new_string": "start" }),
    )
    .expect("edit after read");
    // Its own change counts as read: a second edit needs no re-read.
    execute_tool(
        "edit_file",
        &json!({ "path": path, "old_string": "start", "new_string": "run" }),
    )
    .expect("second edit");

    // Someone else changes it: read it again first.
    fs::write(&file, "fn main() { changed(); }\n").expect("outside change");
    let stale = execute_tool("write_file", &json!({ "path": path, "content": "x\n" }))
        .expect_err("a file changed since the read is refused");
    assert!(stale.contains("changed since you last read it"), "{stale}");

    // New files need no read.
    let new_file = root.join("new.rs");
    execute_tool(
        "write_file",
        &json!({ "path": new_file.to_str().expect("utf-8"), "content": "x\n" }),
    )
    .expect("a new file is created without a read");

    fs::remove_dir_all(root).expect("cleanup");
}

fn temp_path(name: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!("clawd-tools-{unique}-{name}"))
}

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|error| panic!("git {} failed: {error}", args.join(" ")));
    assert!(
        status.success(),
        "git {} exited with {status}",
        args.join(" ")
    );
}

fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo");
    run_git(path, &["init", "--quiet", "-b", "main"]);
    run_git(path, &["config", "user.email", "tests@example.com"]);
    run_git(path, &["config", "user.name", "Tools Tests"]);
    std::fs::write(path.join("README.md"), "initial\n").expect("write readme");
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "-m", "initial commit", "--quiet"]);
}

fn commit_file(path: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(path.join(file), contents).expect("write file");
    run_git(path, &["add", file]);
    run_git(path, &["commit", "-m", message, "--quiet"]);
}

fn permission_policy_for_mode(mode: PermissionMode) -> PermissionPolicy {
    mvp_tool_specs()
        .into_iter()
        .fold(PermissionPolicy::new(mode), |policy, spec| {
            policy.with_tool_requirement(spec.name, spec.required_permission)
        })
}

#[test]
fn exposes_mvp_tools() {
    let names = mvp_tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"WebFetch"));
    assert!(names.contains(&"WebSearch"));
    assert!(names.contains(&"TodoWrite"));
    assert!(names.contains(&"Skill"));
    assert!(names.contains(&"Agent"));
    assert!(names.contains(&"ToolSearch"));
    assert!(names.contains(&"NotebookEdit"));
    assert!(names.contains(&"Sleep"));
    assert!(names.contains(&"SendUserMessage"));
    assert!(names.contains(&"Config"));
    assert!(names.contains(&"EnterPlanMode"));
    assert!(names.contains(&"ExitPlanMode"));
    assert!(names.contains(&"StructuredOutput"));
    assert!(names.contains(&"REPL"));
    assert!(names.contains(&"PowerShell"));
    assert!(names.contains(&"WorkerCreate"));
    assert!(names.contains(&"WorkerObserve"));
    assert!(names.contains(&"WorkerAwaitReady"));
    assert!(names.contains(&"WorkerSendPrompt"));
}

#[test]
fn git_show_schema_exposes_format_enum() {
    let spec = mvp_tool_specs()
        .into_iter()
        .find(|spec| spec.name == "GitShow")
        .expect("GitShow spec");
    assert_eq!(
        spec.input_schema["properties"]["format"]["enum"],
        json!(["patch", "stat", "metadata"])
    );
}

#[test]
fn git_show_supports_patch_stat_metadata_and_rejects_metadata_path() {
    let _guard = env_guard();
    let root = temp_path("git-show-format");
    init_git_repo(&root);
    commit_file(&root, "README.md", "initial\nupdated\n", "update readme");
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let patch = execute_tool("GitShow", &json!({"commit": "HEAD", "format": "patch"}))
        .expect("patch git show");
    let patch: serde_json::Value = serde_json::from_str(&patch).expect("patch json");
    assert!(patch["output"]
        .as_str()
        .expect("patch output")
        .contains("diff --git"));

    let stat = execute_tool("GitShow", &json!({"commit": "HEAD", "format": "stat"}))
        .expect("stat git show");
    let stat: serde_json::Value = serde_json::from_str(&stat).expect("stat json");
    assert!(stat["output"]
        .as_str()
        .expect("stat output")
        .contains("README.md"));

    let legacy_stat = execute_tool("GitShow", &json!({"commit": "HEAD", "stat": true}))
        .expect("legacy stat git show");
    let legacy_stat: serde_json::Value =
        serde_json::from_str(&legacy_stat).expect("legacy stat json");
    assert!(legacy_stat["output"]
        .as_str()
        .expect("legacy stat output")
        .contains("README.md"));

    let metadata = execute_tool("GitShow", &json!({"commit": "HEAD", "format": "metadata"}))
        .expect("metadata git show");
    let metadata: serde_json::Value = serde_json::from_str(&metadata).expect("metadata json");
    let metadata_output = metadata["output"].as_str().expect("metadata output");
    assert!(metadata_output.contains("commit "));
    assert!(metadata_output.contains("update readme"));
    assert!(!metadata_output.contains("diff --git"));

    let file_patch = execute_tool(
        "GitShow",
        &json!({"commit": "HEAD", "path": "README.md", "format": "patch"}),
    )
    .expect("file patch git show");
    let file_patch: serde_json::Value = serde_json::from_str(&file_patch).expect("file patch json");
    assert_eq!(
        file_patch["output"].as_str().expect("file patch output"),
        "initial\nupdated"
    );

    let metadata_path = execute_tool(
        "GitShow",
        &json!({"commit": "HEAD", "path": "README.md", "format": "metadata"}),
    )
    .expect_err("metadata with path should be rejected");
    assert!(metadata_path.contains("cannot be combined with path"));

    let invalid = execute_tool("GitShow", &json!({"commit": "HEAD", "format": "bogus"}))
        .expect_err("invalid format should be rejected");
    assert!(invalid.contains("unknown GitShow format"));

    std::env::set_current_dir(&previous).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn rejects_unknown_tool_names() {
    let error = execute_tool("nope", &json!({})).expect_err("tool should be rejected");
    assert!(error.contains("unsupported tool"));
}

#[test]
fn worker_tools_gate_prompt_delivery_until_ready_and_support_auto_trust() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({
            "cwd": "/tmp/worktree/repo",
            "trusted_roots": ["/tmp/worktree"]
        }),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker id")
        .to_string();
    assert_eq!(created_output["status"], "spawning");
    assert_eq!(created_output["trust_auto_resolve"], true);

    let gated = execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": worker_id,
            "prompt": "ship the change"
        }),
    )
    .expect_err("prompt delivery before ready should fail");
    assert!(gated.contains("not ready for prompt delivery"));

    let observed = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": created_output["worker_id"],
            "screen_text": "Do you trust the files in this folder?\n1. Yes, proceed\n2. No"
        }),
    )
    .expect("WorkerObserve should auto-resolve trust");
    let observed_output: serde_json::Value = serde_json::from_str(&observed).expect("json");
    assert_eq!(observed_output["status"], "spawning");
    assert_eq!(observed_output["trust_gate_cleared"], true);
    assert_eq!(
        observed_output["events"][1]["payload"]["type"],
        "trust_prompt"
    );
    assert_eq!(
        observed_output["events"][2]["payload"]["resolution"],
        "auto_allowlisted"
    );

    let ready = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": created_output["worker_id"],
            "screen_text": "Ready for your input\n>"
        }),
    )
    .expect("WorkerObserve should mark worker ready");
    let ready_output: serde_json::Value = serde_json::from_str(&ready).expect("json");
    assert_eq!(ready_output["status"], "ready_for_prompt");

    let await_ready = execute_tool(
        "WorkerAwaitReady",
        &json!({
            "worker_id": created_output["worker_id"]
        }),
    )
    .expect("WorkerAwaitReady should succeed");
    let await_ready_output: serde_json::Value = serde_json::from_str(&await_ready).expect("json");
    assert_eq!(await_ready_output["ready"], true);

    let accepted = execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": created_output["worker_id"],
            "prompt": "ship the change"
        }),
    )
    .expect("WorkerSendPrompt should succeed after ready");
    let accepted_output: serde_json::Value = serde_json::from_str(&accepted).expect("json");
    assert_eq!(accepted_output["status"], "running");
    assert_eq!(accepted_output["prompt_delivery_attempts"], 1);
    assert_eq!(accepted_output["prompt_in_flight"], true);
}

#[test]
fn worker_create_merges_config_trusted_roots_without_per_call_override() {
    use std::fs;
    // Write a .claw/settings.json in a temp dir with trustedRoots
    let worktree = temp_path("config-trust-worktree");
    let claw_dir = worktree.join(".claw");
    fs::create_dir_all(&claw_dir).expect("create .claw dir");
    // Use the actual OS temp dir so the worktree path matches the allowlist
    let tmp_root = std::env::temp_dir().to_str().expect("utf-8").to_string();
    let settings = format!("{{\"trustedRoots\": [\"{tmp_root}\"]}}");
    fs::write(claw_dir.join("settings.json"), settings).expect("write settings");
    // The user has trusted this project file (`claw trust`).
    let _env = crate::test_env_guard();
    let config_home = temp_path("config-trust-home");
    let loader = runtime::ConfigLoader::new(&worktree, &config_home);
    for file in loader.untrusted_files() {
        loader.trust(&file).expect("trust project config");
    }
    let previous_home = std::env::var_os("CLAW_CONFIG_HOME");
    std::env::set_var("CLAW_CONFIG_HOME", &config_home);

    // WorkerCreate with no per-call trusted_roots — config should supply them
    let cwd = worktree.to_str().expect("valid utf-8").to_string();
    let created = execute_tool(
        "WorkerCreate",
        &json!({
            "cwd": cwd
            // trusted_roots intentionally omitted
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    let created = created.expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");

    // worktree is under /tmp, so config roots auto-resolve trust
    assert_eq!(
        output["trust_auto_resolve"], true,
        "config-level trustedRoots should auto-resolve trust without per-call override"
    );

    fs::remove_dir_all(&worktree).ok();
}

#[test]
fn worker_create_merges_config_trusted_roots_with_per_call_roots() {
    use std::fs;

    let worktree = temp_path("config-and-call-trust-worktree");
    let claw_dir = worktree.join(".claw");
    fs::create_dir_all(&claw_dir).expect("create .claw dir");
    fs::write(
        claw_dir.join("settings.json"),
        r#"{"trustedRoots": ["/definitely/not/this/worktree"]}"#,
    )
    .expect("write settings");

    let cwd = worktree.to_str().expect("valid utf-8").to_string();
    let parent = worktree
        .parent()
        .expect("temp path has parent")
        .to_str()
        .expect("valid parent utf-8")
        .to_string();

    let created = execute_tool(
        "WorkerCreate",
        &json!({
            "cwd": cwd,
            "trusted_roots": [parent]
        }),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");

    assert_eq!(
        output["trust_auto_resolve"], true,
        "per-call trusted_roots must extend config defaults for this create request"
    );

    fs::remove_dir_all(&worktree).ok();
}

#[test]
fn worker_terminate_sets_finished_status() {
    // Create a worker in running state
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/terminate-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    // Terminate
    let terminated = execute_tool("WorkerTerminate", &json!({"worker_id": worker_id}))
        .expect("WorkerTerminate should succeed");
    let term_output: serde_json::Value = serde_json::from_str(&terminated).expect("json");
    assert_eq!(
        term_output["status"], "finished",
        "terminated worker should be finished"
    );
    assert_eq!(
        term_output["prompt_in_flight"], false,
        "prompt_in_flight should be cleared on termination"
    );
}

#[test]
fn worker_restart_resets_to_spawning() {
    // Create and advance worker to ready_for_prompt
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/restart-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    // Advance to ready_for_prompt via observe
    execute_tool(
        "WorkerObserve",
        &json!({"worker_id": worker_id, "screen_text": "Ready for input\n>"}),
    )
    .expect("WorkerObserve should succeed");

    // Restart
    let restarted = execute_tool("WorkerRestart", &json!({"worker_id": worker_id}))
        .expect("WorkerRestart should succeed");
    let restart_output: serde_json::Value = serde_json::from_str(&restarted).expect("json");
    assert_eq!(
        restart_output["status"], "spawning",
        "restarted worker should return to spawning"
    );
    assert_eq!(
        restart_output["prompt_in_flight"], false,
        "prompt_in_flight should be cleared on restart"
    );
    assert_eq!(
        restart_output["trust_gate_cleared"], false,
        "trust_gate_cleared should be reset on restart (re-trust required)"
    );
}

#[test]
fn worker_get_returns_worker_state() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/worker-get-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"].as_str().expect("worker_id");

    let fetched = execute_tool("WorkerGet", &json!({"worker_id": worker_id}))
        .expect("WorkerGet should succeed");
    let fetched_output: serde_json::Value = serde_json::from_str(&fetched).expect("json");
    assert_eq!(fetched_output["worker_id"], worker_id);
    assert_eq!(fetched_output["status"], "spawning");
    assert_eq!(fetched_output["cwd"], "/tmp/worker-get-test");
}

#[test]
fn worker_get_on_unknown_id_returns_error() {
    let result = execute_tool(
        "WorkerGet",
        &json!({"worker_id": "worker_nonexistent_get_00000000"}),
    );
    assert!(
        result.is_err(),
        "WorkerGet on unknown id should return error"
    );
    assert!(
        result.unwrap_err().contains("worker not found"),
        "error should mention worker not found"
    );
}

#[test]
fn worker_await_ready_on_spawning_worker_returns_not_ready() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/worker-await-not-ready"}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"].as_str().expect("worker_id");

    // Worker is still in spawning — await_ready should return not-ready snapshot
    let snapshot = execute_tool("WorkerAwaitReady", &json!({"worker_id": worker_id}))
        .expect("WorkerAwaitReady should succeed even when not ready");
    let snap_output: serde_json::Value = serde_json::from_str(&snapshot).expect("json");
    assert_eq!(
        snap_output["ready"], false,
        "WorkerAwaitReady on a spawning worker must return ready=false"
    );
    assert_eq!(snap_output["worker_id"], worker_id);
}

#[test]
fn worker_send_prompt_on_non_ready_worker_returns_error() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/worker-send-not-ready"}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"].as_str().expect("worker_id");

    let result = execute_tool(
        "WorkerSendPrompt",
        &json!({"worker_id": worker_id, "prompt": "too early"}),
    );
    assert!(
        result.is_err(),
        "WorkerSendPrompt on a non-ready worker should fail"
    );
}

#[test]
fn recovery_loop_state_file_reflects_transitions() {
    // End-to-end proof: .claw/worker-state.json reflects every transition
    // through the stall-detect -> resolve-trust -> ready loop.
    use std::fs;

    // Use a real temp CWD so state file can be written
    let worktree = temp_path("recovery-loop-state");
    fs::create_dir_all(&worktree).expect("create worktree");
    let cwd = worktree.to_str().expect("utf-8").to_string();
    let state_path = worktree.join(".claw").join("worker-state.json");

    // 1. Create worker WITHOUT trusted_roots
    let created =
        execute_tool("WorkerCreate", &json!({"cwd": cwd})).expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker_id")
        .to_string();
    // State file should exist after create
    assert!(
        state_path.exists(),
        "state file should be written after WorkerCreate"
    );
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(state["status"], "spawning");
    assert_eq!(state["is_ready"], false);
    assert!(
        state["seconds_since_update"].is_number(),
        "seconds_since_update must be present"
    );

    // 2. Force trust_required via observe
    execute_tool(
        "WorkerObserve",
        &json!({"worker_id": worker_id, "screen_text": "Do you trust the files in this folder?"}),
    )
    .expect("WorkerObserve should succeed");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(
        state["status"], "trust_required",
        "state file must reflect trust_required stall"
    );
    assert_eq!(state["is_ready"], false);
    assert_eq!(state["trust_gate_cleared"], false);
    assert!(state["seconds_since_update"].is_number());

    // 3. WorkerResolveTrust -> state file reflects recovery
    execute_tool("WorkerResolveTrust", &json!({"worker_id": worker_id}))
        .expect("WorkerResolveTrust should succeed");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(
        state["status"], "spawning",
        "state file must show spawning after trust resolved"
    );
    assert_eq!(state["trust_gate_cleared"], true);

    // 4. Observe ready screen -> state file shows ready_for_prompt
    execute_tool(
        "WorkerObserve",
        &json!({"worker_id": worker_id, "screen_text": "Ready for input\n>"}),
    )
    .expect("WorkerObserve ready should succeed");
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(
        state["status"], "ready_for_prompt",
        "state file must show ready_for_prompt after ready screen"
    );
    assert_eq!(
        state["is_ready"], true,
        "is_ready must be true in state file at ready_for_prompt"
    );

    fs::remove_dir_all(&worktree).ok();
}

#[test]
fn stall_detect_and_resolve_trust_end_to_end() {
    // 1. Create worker WITHOUT trusted_roots so trust won't auto-resolve
    let created = execute_tool("WorkerCreate", &json!({"cwd": "/no/trusted/root/here"}))
        .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker_id")
        .to_string();
    assert_eq!(created_output["trust_auto_resolve"], false);

    // 2. Observe trust prompt screen text -> worker stalls at trust_required
    let stalled = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "Do you trust the files in this folder?\n[Allow] [Deny]"
        }),
    )
    .expect("WorkerObserve should succeed");
    let stalled_output: serde_json::Value = serde_json::from_str(&stalled).expect("json");
    assert_eq!(
        stalled_output["status"], "trust_required",
        "worker should stall at trust_required when trust prompt seen without allowlist"
    );
    assert_eq!(stalled_output["trust_gate_cleared"], false);
    // 3. Clawhip calls WorkerResolveTrust to unblock
    let resolved = execute_tool("WorkerResolveTrust", &json!({"worker_id": worker_id}))
        .expect("WorkerResolveTrust should succeed");
    let resolved_output: serde_json::Value = serde_json::from_str(&resolved).expect("json");
    assert_eq!(
        resolved_output["status"], "spawning",
        "worker should return to spawning after trust resolved"
    );
    assert_eq!(resolved_output["trust_gate_cleared"], true);

    // 4. Ready screen text now advances worker normally
    let ready = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "Ready for input\n>"
        }),
    )
    .expect("WorkerObserve should succeed after trust resolved");
    let ready_output: serde_json::Value = serde_json::from_str(&ready).expect("json");
    assert_eq!(
        ready_output["status"], "ready_for_prompt",
        "worker should reach ready_for_prompt after trust resolved and ready screen seen"
    );
}

#[test]
fn stall_detect_and_restart_recovery_end_to_end() {
    // Worker stalls at trust_required, clawhip restarts instead of resolving
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/no/trusted/root/restart-test"}),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker_id")
        .to_string();

    // Force trust_required
    let stalled = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "trust this folder? [Yes] [No]"
        }),
    )
    .expect("WorkerObserve should succeed");
    let stalled_output: serde_json::Value = serde_json::from_str(&stalled).expect("json");
    assert_eq!(stalled_output["status"], "trust_required");

    // WorkerRestart resets the worker
    let restarted = execute_tool("WorkerRestart", &json!({"worker_id": worker_id}))
        .expect("WorkerRestart should succeed");
    let restarted_output: serde_json::Value = serde_json::from_str(&restarted).expect("json");
    assert_eq!(
        restarted_output["status"], "spawning",
        "restarted worker should be back at spawning"
    );
    assert_eq!(
        restarted_output["trust_gate_cleared"], false,
        "restart clears trust — next observe loop must re-acquire trust"
    );
}

#[test]
fn worker_terminate_on_unknown_id_returns_error() {
    let result = execute_tool(
        "WorkerTerminate",
        &json!({"worker_id": "worker_nonexistent_00000000"}),
    );
    assert!(result.is_err(), "terminating unknown worker should fail");
    assert!(
        result.unwrap_err().contains("worker not found"),
        "error should mention worker not found"
    );
}

#[test]
fn worker_restart_on_unknown_id_returns_error() {
    let result = execute_tool(
        "WorkerRestart",
        &json!({"worker_id": "worker_nonexistent_00000001"}),
    );
    assert!(result.is_err(), "restarting unknown worker should fail");
    assert!(
        result.unwrap_err().contains("worker not found"),
        "error should mention worker not found"
    );
}

#[test]
fn worker_observe_completion_success_finish_sets_finished_status() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/observe-completion-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    let completed = execute_tool(
        "WorkerObserveCompletion",
        &json!({
            "worker_id": worker_id,
            "finish_reason": "end_turn",
            "tokens_output": 512
        }),
    )
    .expect("WorkerObserveCompletion should succeed");
    let completed_output: serde_json::Value = serde_json::from_str(&completed).expect("json");
    assert_eq!(completed_output["status"], "finished");
    assert_eq!(completed_output["prompt_in_flight"], false);
}

#[test]
fn worker_observe_completion_degraded_provider_sets_failed_status() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({"cwd": "/tmp/observe-degraded-test", "trusted_roots": ["/tmp"]}),
    )
    .expect("WorkerCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = output["worker_id"].as_str().expect("worker_id").to_string();

    // finish=unknown + 0 tokens = degraded provider classification
    let failed = execute_tool(
        "WorkerObserveCompletion",
        &json!({
            "worker_id": worker_id,
            "finish_reason": "unknown",
            "tokens_output": 0
        }),
    )
    .expect("WorkerObserveCompletion should succeed");
    let failed_output: serde_json::Value = serde_json::from_str(&failed).expect("json");
    assert_eq!(
        failed_output["status"], "failed",
        "finish=unknown + 0 tokens should classify as provider failure"
    );
    assert_eq!(failed_output["prompt_in_flight"], false);
    // last_error should be set with provider failure message
    assert!(
        !failed_output["last_error"].is_null(),
        "last_error should be populated for provider failure"
    );
}

#[test]
fn worker_tools_detect_misdelivery_and_arm_prompt_replay() {
    let created = execute_tool(
        "WorkerCreate",
        &json!({
            "cwd": "/tmp/repo/worker-misdelivery"
        }),
    )
    .expect("WorkerCreate should succeed");
    let created_output: serde_json::Value = serde_json::from_str(&created).expect("json");
    let worker_id = created_output["worker_id"]
        .as_str()
        .expect("worker id")
        .to_string();

    execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "Ready for input\n>"
        }),
    )
    .expect("worker should become ready");

    execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": worker_id,
            "prompt": "Investigate flaky boot"
        }),
    )
    .expect("prompt send should succeed");

    let recovered = execute_tool(
        "WorkerObserve",
        &json!({
            "worker_id": worker_id,
            "screen_text": "% Investigate flaky boot\nzsh: command not found: Investigate"
        }),
    )
    .expect("misdelivery observe should succeed");
    let recovered_output: serde_json::Value = serde_json::from_str(&recovered).expect("json");
    assert_eq!(recovered_output["status"], "ready_for_prompt");
    assert_eq!(recovered_output["last_error"]["kind"], "prompt_delivery");
    assert_eq!(recovered_output["replay_prompt"], "Investigate flaky boot");
    assert_eq!(
        recovered_output["events"][3]["payload"]["observed_target"],
        "shell"
    );
    assert_eq!(
        recovered_output["events"][4]["payload"]["recovery_armed"],
        true
    );

    let replayed = execute_tool(
        "WorkerSendPrompt",
        &json!({
            "worker_id": worker_id
        }),
    )
    .expect("WorkerSendPrompt should replay recovered prompt");
    let replayed_output: serde_json::Value = serde_json::from_str(&replayed).expect("json");
    assert_eq!(replayed_output["status"], "running");
    assert_eq!(replayed_output["prompt_delivery_attempts"], 2);
    assert_eq!(replayed_output["prompt_in_flight"], true);
}

#[test]
fn global_tool_registry_denies_blocked_tool_before_dispatch() {
    // given
    let policy = permission_policy_for_mode(PermissionMode::ReadOnly);
    let registry = GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy));

    // when
    let error = registry
        .execute(
            "write_file",
            &json!({
                "path": "blocked.txt",
                "content": "blocked"
            }),
        )
        .expect_err("write tool should be denied before dispatch");

    // then
    assert!(error.contains("requires 'workspace-write' permission"));
}

#[test]
fn a_subagent_tool_outside_its_set_is_refused() {
    let allowed = BTreeSet::from([String::from("read_file")]);
    let error = run_subagent_tool(
        &allowed,
        "write_file",
        &json!({ "path": "blocked.txt", "content": "blocked" }),
    )
    .expect_err("write_file is not this agent's");
    assert!(error.contains("not enabled for this sub-agent"));
}

#[test]
fn permission_mode_from_plugin_rejects_invalid_inputs() {
    let unknown_permission =
        permission_mode_from_plugin("admin").expect_err("unknown plugin permission should fail");
    assert!(unknown_permission.contains("unsupported plugin permission: admin"));

    let empty_permission =
        permission_mode_from_plugin("").expect_err("empty plugin permission should fail");
    assert!(empty_permission.contains("unsupported plugin permission: "));
}

#[test]
fn allowed_tools_rejects_empty_token_lists() {
    let registry = GlobalToolRegistry::builtin();

    for raw in ["", ",,", "   "] {
        let err = registry
            .normalize_allowed_tools(&[raw.to_string()])
            .expect_err("empty allow-list input should be rejected");
        assert!(
            err.contains("--allowedTools was provided with no usable tool names"),
            "unexpected error for {raw:?}: {err}"
        );
    }
}

#[test]
fn allowed_tools_normalize_to_canonical_snake_case_and_aliases_432() {
    let registry = GlobalToolRegistry::builtin();
    let allowed = registry
        .normalize_allowed_tools(&["Read,WebFetch,MCP".to_string()])
        .expect("aliases and legacy names should normalize")
        .expect("allow-list should be populated");
    assert!(allowed.contains("read_file"));
    assert!(allowed.contains("web_fetch"));
    assert!(allowed.contains("mcp"));
    assert!(!allowed.contains("Read"));
    assert!(!allowed.contains("WebFetch"));

    let canonical = registry.canonical_allowed_tool_names();
    assert!(canonical.contains(&"web_fetch".to_string()));
    assert!(canonical.contains(&"todo_write".to_string()));
    assert!(!canonical.contains(&"WebFetch".to_string()));
    assert_eq!(
        registry.allowed_tool_aliases().get("WebFetch"),
        Some(&"web_fetch".to_string())
    );
}

#[test]
fn runtime_tools_extend_registry_definitions_permissions_and_search() {
    let registry = GlobalToolRegistry::builtin()
        .with_runtime_tools(vec![super::RuntimeToolDefinition {
            name: "mcp__demo__echo".to_string(),
            description: Some("Echo text from the demo MCP server".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "additionalProperties": false
            }),
            required_permission: runtime::PermissionMode::ReadOnly,
        }])
        .expect("runtime tools should register");

    let allowed = registry
        .normalize_allowed_tools(&["mcp__demo__echo".to_string()])
        .expect("runtime tool should be allow-listable")
        .expect("allow-list should be populated");
    assert!(allowed.contains("mcp__demo__echo"));

    let definitions = registry.definitions(Some(&allowed));
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "mcp__demo__echo");

    let permissions = registry
        .permission_specs(Some(&allowed))
        .expect("runtime tool permissions should resolve");
    assert_eq!(
        permissions,
        vec![(
            "mcp__demo__echo".to_string(),
            runtime::PermissionMode::ReadOnly
        )]
    );

    let search = registry.search(
        "demo echo",
        5,
        Some(vec!["pending-server".to_string()]),
        Some(runtime::McpDegradedReport::new(
            vec!["demo".to_string()],
            vec![runtime::McpFailedServer {
                server_name: "pending-server".to_string(),
                phase: runtime::McpLifecyclePhase::ToolDiscovery,
                error: runtime::McpErrorSurface::new(
                    runtime::McpLifecyclePhase::ToolDiscovery,
                    Some("pending-server".to_string()),
                    "tool discovery failed",
                    BTreeMap::new(),
                    true,
                ),
            }],
            vec!["mcp__demo__echo".to_string()],
            vec!["mcp__demo__echo".to_string()],
        )),
    );
    let output = serde_json::to_value(search).expect("search output should serialize");
    assert_eq!(output["matches"][0], "mcp__demo__echo");
    assert_eq!(output["pending_mcp_servers"][0], "pending-server");
    assert_eq!(
        output["mcp_degraded"]["failed_servers"][0]["phase"],
        "tool_discovery"
    );
}

#[test]
fn web_fetch_returns_prompt_aware_summary() {
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.starts_with("GET /page "));
        HttpResponse::html(
                200,
                "OK",
                "<html><head><title>Ignored</title></head><body><h1>Test Page</h1><p>Hello <b>world</b> from local server.</p></body></html>",
            )
    }));

    let result = execute_tool(
        "WebFetch",
        &json!({
            "url": format!("http://{}/page", server.addr()),
            "prompt": "Summarize this page"
        }),
    )
    .expect("WebFetch should succeed");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["code"], 200);
    let summary = output["result"].as_str().expect("result string");
    assert!(summary.contains("Fetched"));
    assert!(summary.contains("Test Page"));
    assert!(summary.contains("Hello world from local server"));

    let titled = execute_tool(
        "WebFetch",
        &json!({
            "url": format!("http://{}/page", server.addr()),
            "prompt": "What is the page title?"
        }),
    )
    .expect("WebFetch title query should succeed");
    let titled_output: serde_json::Value = serde_json::from_str(&titled).expect("valid json");
    let titled_summary = titled_output["result"].as_str().expect("result string");
    assert!(titled_summary.contains("Title: Ignored"));
}

#[test]
fn private_hosts_are_recognised_for_the_redirect_check() {
    for host in [
        "localhost",
        "api.localhost",
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "[::1]",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
    ] {
        assert!(super::is_private_host(host), "{host}");
    }
    for host in ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "100.128.0.1"] {
        assert!(!super::is_private_host(host), "{host}");
    }
}

#[test]
fn readable_text_skips_head_scripts_and_navigation() {
    let head_junk = format!(
        "<script>const guestTheme = document.cookie;</script>\
             <script type=\"application/ld+json\">{{\"@context\":\"https://schema.org\"}}</script>\
             <style>body {{ color: red; }}</style>{}",
        "<meta name=\"x\" content=\"y\">".repeat(200)
    );
    let html = format!(
        "<html><head><title>Uncensor &amp; more</title>{head_junk}</head><body>\
             <nav><a href=\"/\">Home</a> <a href=\"/blog\">Blog</a></nav>\
             <article><h1>Abliteration</h1>\
             <p>It finds a direction in the residual stream &mdash; then removes it.</p>\
             <ul><li>Collect activations</li><li>Compute the mean difference</li></ul>\
             <p>{}</p></article><footer>Copyright</footer></body></html>",
        "More body text. ".repeat(20)
    );
    let text = super::html_to_readable_text(&html);
    assert!(text.starts_with("Abliteration\n"), "{text}");
    assert_eq!(
        super::html_to_readable_text(r#"<div class="[&>a]:hidden">kept</div>"#),
        "kept"
    );
    assert!(text.contains("It finds a direction in the residual stream \u{2014} then removes it."));
    assert!(text.contains("- Collect activations\n- Compute the mean difference"));
    for junk in [
        "guestTheme",
        "schema.org",
        "color: red",
        "Home",
        "Copyright",
    ] {
        assert!(!text.contains(junk), "{junk} leaked into: {text}");
    }
    assert_eq!(
        super::decode_html_entities("it&#8217;s &#x41; &bogus; a&b"),
        "it\u{2019}s A &bogus; a&b"
    );
}

#[test]
fn web_fetch_supports_plain_text_and_rejects_invalid_url() {
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.starts_with("GET /plain "));
        HttpResponse::text(200, "OK", "plain text response")
    }));

    let result = execute_tool(
        "WebFetch",
        &json!({
            "url": format!("http://{}/plain", server.addr()),
            "prompt": "Show me the content"
        }),
    )
    .expect("WebFetch should succeed for text content");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["url"], format!("http://{}/plain", server.addr()));
    assert!(output["result"]
        .as_str()
        .expect("result")
        .contains("plain text response"));

    let error = execute_tool(
        "WebFetch",
        &json!({
            "url": "not a url",
            "prompt": "Summarize"
        }),
    )
    .expect_err("invalid URL should fail");
    assert!(error.contains("relative URL without a base") || error.contains("invalid"));
}

#[test]
fn web_search_extracts_and_filters_results() {
    // Serialize env-var mutation so this test cannot race with the sibling
    // web_search_handles_generic_links_and_invalid_base_url test that also
    // sets CLAWD_WEB_SEARCH_BASE_URL. Without the lock, parallel test
    // runners can interleave the set/remove calls and cause assertion
    // failures on the wrong port.
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.contains("GET /search?q=rust+web+search "));
        HttpResponse::html(
            200,
            "OK",
            r#"
                <html><body>
                  <a class="result__a" href="https://docs.rs/reqwest">Reqwest docs</a>
                  <a class="result__snippet" href="https://docs.rs/reqwest">An ergonomic, <b>batteries-included</b> HTTP Client.</a>
                  <a class="result__a" href="https://example.com/blocked">Blocked result</a>
                </body></html>
                "#,
        )
    }));

    std::env::set_var(
        "CLAWD_WEB_SEARCH_BASE_URL",
        format!("http://{}/search", server.addr()),
    );
    let result = execute_tool(
        "WebSearch",
        &json!({
            "query": "rust web search",
            "allowed_domains": ["https://DOCS.rs/"],
            "blocked_domains": ["HTTPS://EXAMPLE.COM"]
        }),
    )
    .expect("WebSearch should succeed");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["query"], "rust web search");
    let results = output["results"].as_array().expect("results array");
    let search_result = results
        .iter()
        .find(|item| item.get("content").is_some())
        .expect("search result block present");
    let content = search_result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["title"], "Reqwest docs");
    assert_eq!(content[0]["url"], "https://docs.rs/reqwest");
    let text = results
        .iter()
        .find_map(serde_json::Value::as_str)
        .expect("text block");
    assert!(
            text.contains(
                "- [Reqwest docs](https://docs.rs/reqwest)\n  An ergonomic, batteries-included HTTP Client."
            ),
            "{text}"
        );
}

#[test]
fn web_search_handles_generic_links_and_invalid_base_url() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.contains("GET /fallback?q=generic+links "));
        HttpResponse::html(
            200,
            "OK",
            r#"
                <html><body>
                  <a href="https://example.com/one">Example One</a>
                  <a href="https://example.com/one">Duplicate Example One</a>
                  <a href="https://docs.rs/tokio">Tokio Docs</a>
                </body></html>
                "#,
        )
    }));

    std::env::set_var(
        "CLAWD_WEB_SEARCH_BASE_URL",
        format!("http://{}/fallback", server.addr()),
    );
    let result = execute_tool(
        "WebSearch",
        &json!({
            "query": "generic links"
        }),
    )
    .expect("WebSearch fallback parsing should succeed");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    let results = output["results"].as_array().expect("results array");
    let search_result = results
        .iter()
        .find(|item| item.get("content").is_some())
        .expect("search result block present");
    let content = search_result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["url"], "https://example.com/one");
    assert_eq!(content[1]["url"], "https://docs.rs/tokio");

    std::env::set_var("CLAWD_WEB_SEARCH_BASE_URL", "://bad-base-url");
    let error = execute_tool("WebSearch", &json!({ "query": "generic links" }))
        .expect_err("invalid base URL should fail");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");
    assert!(error.contains("relative URL without a base") || error.contains("empty host"));
}

#[test]
fn web_search_decodes_absolute_duckduckgo_redirect_urls() {
    // given
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.contains("GET /search?q=duckduckgo+redirects "));
        HttpResponse::html(
            200,
            "OK",
            r#"
                <html><body>
                  <a rel="nofollow" class="result__a" href="https://duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Freqwest&amp;rut=abc">Reqwest docs</a>
                </body></html>
                "#,
        )
    }));

    // when
    std::env::set_var(
        "CLAWD_WEB_SEARCH_BASE_URL",
        format!("http://{}/search", server.addr()),
    );
    let result = execute_tool(
        "WebSearch",
        &json!({
            "query": "duckduckgo redirects"
        }),
    )
    .expect("WebSearch should succeed");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");

    // then
    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    let results = output["results"].as_array().expect("results array");
    let search_result = results
        .iter()
        .find(|item| item.get("content").is_some())
        .expect("search result block present");
    let content = search_result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["title"], "Reqwest docs");
    assert_eq!(content[0]["url"], "https://docs.rs/reqwest");
}

#[test]
fn web_search_decodes_protocol_relative_duckduckgo_redirect_urls() {
    // given
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let server = TestServer::spawn(Arc::new(|request_line: &str| {
        assert!(request_line.contains("GET /search?q=duckduckgo+protocol+relative "));
        HttpResponse::html(
            200,
            "OK",
            r#"
                <html><body>
                  <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&amp;rut=xyz">Tokio Docs</a>
                </body></html>
                "#,
        )
    }));

    // when
    std::env::set_var(
        "CLAWD_WEB_SEARCH_BASE_URL",
        format!("http://{}/search", server.addr()),
    );
    let result = execute_tool(
        "WebSearch",
        &json!({
            "query": "duckduckgo protocol relative"
        }),
    )
    .expect("WebSearch should succeed");
    std::env::remove_var("CLAWD_WEB_SEARCH_BASE_URL");

    // then
    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    let results = output["results"].as_array().expect("results array");
    let search_result = results
        .iter()
        .find(|item| item.get("content").is_some())
        .expect("search result block present");
    let content = search_result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["title"], "Tokio Docs");
    assert_eq!(content[0]["url"], "https://docs.rs/tokio");
}

#[test]
fn todo_list_renders_progress_and_marks() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temp_path("todos-render.json");
    std::env::set_var("CLAWD_TODO_STORE", &path);
    let empty = super::render_todo_list();
    execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "Add tool", "activeForm": "Adding tool", "status": "completed"},
                {"content": "Run tests", "activeForm": "Running tests", "status": "in_progress"},
                {"content": "Ship", "activeForm": "Shipping", "status": "pending"}
            ]
        }),
    )
    .expect("TodoWrite should succeed");
    let rendered = super::render_todo_list();
    std::env::remove_var("CLAWD_TODO_STORE");
    let _ = std::fs::remove_file(crate::todo_events::log_path(&path));
    let _ = std::fs::remove_file(path);

    assert!(empty.starts_with("No todos"));
    assert_eq!(
        rendered,
        "Todos (1/3 done)\n  ✔ Add tool\n  ◼ Running tests\n  ◻ Ship"
    );
}

#[test]
fn todo_write_persists_and_returns_previous_state() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temp_path("todos.json");
    std::env::set_var("CLAWD_TODO_STORE", &path);

    let first = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "Add tool", "activeForm": "Adding tool", "status": "in_progress"},
                {"content": "Run tests", "activeForm": "Running tests", "status": "pending"}
            ]
        }),
    )
    .expect("TodoWrite should succeed");
    let first_output: serde_json::Value = serde_json::from_str(&first).expect("valid json");
    assert_eq!(first_output["oldTodos"].as_array().expect("array").len(), 0);

    let second = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "Add tool", "activeForm": "Adding tool", "status": "completed"},
                {"content": "Run tests", "activeForm": "Running tests", "status": "completed"},
                {"content": "Verify", "activeForm": "Verifying", "status": "completed"}
            ]
        }),
    )
    .expect("TodoWrite should succeed");
    std::env::remove_var("CLAWD_TODO_STORE");
    let _ = std::fs::remove_file(crate::todo_events::log_path(&path));
    let _ = std::fs::remove_file(path);

    let second_output: serde_json::Value = serde_json::from_str(&second).expect("valid json");
    assert_eq!(
        second_output["oldTodos"].as_array().expect("array").len(),
        2
    );
    assert_eq!(
        second_output["newTodos"].as_array().expect("array").len(),
        3
    );
    assert!(second_output["verificationNudgeNeeded"].is_null());
}

#[test]
fn todo_write_rejects_invalid_payloads_and_sets_verification_nudge() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = temp_path("todos-errors.json");
    std::env::set_var("CLAWD_TODO_STORE", &path);

    let empty =
        execute_tool("TodoWrite", &json!({ "todos": [] })).expect_err("empty todos should fail");
    assert!(empty.contains("todos must not be empty"));

    // Multiple in_progress items are now allowed for parallel workflows
    let _multi_active = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "One", "activeForm": "Doing one", "status": "in_progress"},
                {"content": "Two", "activeForm": "Doing two", "status": "in_progress"}
            ]
        }),
    )
    .expect("multiple in-progress todos should succeed");

    let blank_content = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "   ", "activeForm": "Doing it", "status": "pending"}
            ]
        }),
    )
    .expect_err("blank content should fail");
    assert!(blank_content.contains("todo content must not be empty"));

    let nudge = execute_tool(
        "TodoWrite",
        &json!({
            "todos": [
                {"content": "Write tests", "activeForm": "Writing tests", "status": "completed"},
                {"content": "Fix errors", "activeForm": "Fixing errors", "status": "completed"},
                {"content": "Ship branch", "activeForm": "Shipping branch", "status": "completed"}
            ]
        }),
    )
    .expect("completed todos should succeed");
    std::env::remove_var("CLAWD_TODO_STORE");
    let _ = fs::remove_file(crate::todo_events::log_path(&path));
    let _ = fs::remove_file(path);

    let output: serde_json::Value = serde_json::from_str(&nudge).expect("valid json");
    assert_eq!(output["verificationNudgeNeeded"], true);
}

#[test]
fn skill_loads_local_skill_prompt() {
    let _guard = env_guard();
    let home = temp_path("skills-home");
    let skill_dir = home.join(".agents").join("skills").join("help");
    fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "# help\n\nGuide on using oh-my-codex plugin\n",
    )
    .expect("skill file should exist");
    let original_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);

    let result = execute_tool(
        "Skill",
        &json!({
            "skill": "help",
            "args": "overview"
        }),
    )
    .expect("Skill should succeed");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(output["skill"], "help");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with("/help/SKILL.md"));
    assert!(output["prompt"]
        .as_str()
        .expect("prompt")
        .contains("Guide on using oh-my-codex plugin"));

    let dollar_result = execute_tool(
        "Skill",
        &json!({
            "skill": "$help"
        }),
    )
    .expect("Skill should accept $skill invocation form");
    let dollar_output: serde_json::Value =
        serde_json::from_str(&dollar_result).expect("valid json");
    assert_eq!(dollar_output["skill"], "$help");
    assert!(dollar_output["path"]
        .as_str()
        .expect("path")
        .ends_with("/help/SKILL.md"));

    if let Some(home) = original_home {
        std::env::set_var("HOME", home);
    } else {
        std::env::remove_var("HOME");
    }
    fs::remove_dir_all(home).expect("temp home should clean up");
}

#[test]
fn skill_resolves_project_local_skills_and_legacy_commands() {
    let _guard = env_guard();
    let root = temp_path("project-skills");
    let skill_dir = root.join(".claw").join("skills").join("plan");
    let command_dir = root.join(".claw").join("commands");
    fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    fs::create_dir_all(&command_dir).expect("command dir should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: plan\ndescription: Project planning guidance\n---\n\n# plan\n",
    )
    .expect("skill file should exist");
    fs::write(
        command_dir.join("handoff.md"),
        "---\nname: handoff\ndescription: Legacy handoff guidance\n---\n\n# handoff\n",
    )
    .expect("command file should exist");

    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let skill_result = execute_tool("Skill", &json!({ "skill": "$plan" }))
        .expect("project-local skill should resolve");
    let skill_output: serde_json::Value = serde_json::from_str(&skill_result).expect("valid json");
    assert!(skill_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claw/skills/plan/SKILL.md"));

    let command_result = execute_tool("Skill", &json!({ "skill": "/handoff" }))
        .expect("legacy command should resolve");
    let command_output: serde_json::Value =
        serde_json::from_str(&command_result).expect("valid json");
    assert!(command_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claw/commands/handoff.md"));

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    fs::remove_dir_all(root).expect("temp project should clean up");
}

#[test]
fn skill_loads_project_local_claude_skill_prompt() {
    let _guard = env_guard();
    let root = temp_path("project-skills");
    let home = root.join("home");
    let workspace = root.join("workspace");
    let nested = workspace.join("nested");
    let skill_dir = workspace.join(".claude").join("skills").join("trace");
    fs::create_dir_all(&skill_dir).expect("skill dir should exist");
    fs::create_dir_all(&nested).expect("nested cwd should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: trace\ndescription: Project-local trace helper\n---\n# trace\n",
    )
    .expect("skill file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_current_dir(&nested).expect("set cwd");

    let result = execute_tool("Skill", &json!({ "skill": "trace" }))
        .expect("project-local skill should resolve");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claude/skills/trace/SKILL.md"));
    assert_eq!(output["description"], "Project-local trace helper");

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_project_local_omc_and_agents_skill_prompts() {
    let _guard = env_guard();
    let root = temp_path("project-omc-skills");
    let home = root.join("home");
    let workspace = root.join("workspace");
    let nested = workspace.join("nested");
    let omc_skill_dir = workspace.join(".omc").join("skills").join("hud");
    let agents_skill_dir = workspace.join(".agents").join("skills").join("trace");
    fs::create_dir_all(&omc_skill_dir).expect("omc skill dir should exist");
    fs::create_dir_all(&agents_skill_dir).expect("agents skill dir should exist");
    fs::create_dir_all(&nested).expect("nested cwd should exist");
    fs::write(
        omc_skill_dir.join("SKILL.md"),
        "---\nname: hud\ndescription: Project-local OMC HUD helper\n---\n# hud\n",
    )
    .expect("omc skill file should exist");
    fs::write(
        agents_skill_dir.join("SKILL.md"),
        "---\nname: trace\ndescription: Project-local agents compatibility helper\n---\n# trace\n",
    )
    .expect("agents skill file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_current_dir(&nested).expect("set cwd");

    let omc_result =
        execute_tool("Skill", &json!({ "skill": "hud" })).expect("omc skill should resolve");
    let agents_result =
        execute_tool("Skill", &json!({ "skill": "trace" })).expect("agents skill should resolve");

    let omc_output: serde_json::Value = serde_json::from_str(&omc_result).expect("valid json");
    let agents_output: serde_json::Value =
        serde_json::from_str(&agents_result).expect("valid json");
    assert!(omc_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".omc/skills/hud/SKILL.md"));
    assert_eq!(omc_output["description"], "Project-local OMC HUD helper");
    assert!(agents_output["path"]
        .as_str()
        .expect("path")
        .ends_with(".agents/skills/trace/SKILL.md"));
    assert_eq!(
        agents_output["description"],
        "Project-local agents compatibility helper"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_learned_skill_from_claude_config_dir() {
    let _guard = env_guard();
    let root = temp_path("claude-config-learned-skill");
    let home = root.join("home");
    let claude_config_dir = root.join("claude-config");
    let learned_skill_dir = claude_config_dir
        .join("skills")
        .join("omc-learned")
        .join("learned");
    fs::create_dir_all(&learned_skill_dir).expect("learned skill dir should exist");
    fs::write(
        learned_skill_dir.join("SKILL.md"),
        "---\nname: learned\ndescription: Learned OMC skill\n---\n# learned\n",
    )
    .expect("learned skill file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

    let result = execute_tool("Skill", &json!({ "skill": "learned" }))
        .expect("learned skill should resolve");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with("skills/omc-learned/learned/SKILL.md"));
    assert_eq!(output["description"], "Learned OMC skill");

    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    match original_claude_config_dir {
        Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
        None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_direct_skill_and_legacy_command_from_claude_config_dir() {
    let _guard = env_guard();
    let root = temp_path("claude-config-direct-skill");
    let home = root.join("home");
    let claude_config_dir = root.join("claude-config");
    let skill_dir = claude_config_dir.join("skills").join("statusline");
    let command_dir = claude_config_dir.join("commands");
    fs::create_dir_all(&skill_dir).expect("direct skill dir should exist");
    fs::create_dir_all(&command_dir).expect("command dir should exist");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: statusline\ndescription: Claude config skill\n---\n# statusline\n",
    )
    .expect("direct skill file should exist");
    fs::write(
        command_dir.join("doctor-check.md"),
        "---\nname: doctor-check\ndescription: Claude config command\n---\n# doctor-check\n",
    )
    .expect("direct command file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

    let direct_skill =
        execute_tool("Skill", &json!({ "skill": "statusline" })).expect("direct skill");
    let direct_skill_output: serde_json::Value =
        serde_json::from_str(&direct_skill).expect("valid skill json");
    assert!(direct_skill_output["path"]
        .as_str()
        .expect("path")
        .ends_with("skills/statusline/SKILL.md"));
    assert_eq!(direct_skill_output["description"], "Claude config skill");

    let legacy_command =
        execute_tool("Skill", &json!({ "skill": "doctor-check" })).expect("direct command");
    let legacy_command_output: serde_json::Value =
        serde_json::from_str(&legacy_command).expect("valid command json");
    assert!(legacy_command_output["path"]
        .as_str()
        .expect("path")
        .ends_with("commands/doctor-check.md"));
    assert_eq!(
        legacy_command_output["description"],
        "Claude config command"
    );

    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    match original_claude_config_dir {
        Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
        None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn skill_loads_project_local_legacy_command_markdown() {
    let _guard = env_guard();
    let root = temp_path("project-legacy-command");
    let home = root.join("home");
    let workspace = root.join("workspace");
    let nested = workspace.join("nested");
    let command_dir = workspace.join(".claude").join("commands");
    fs::create_dir_all(&command_dir).expect("legacy command dir should exist");
    fs::create_dir_all(&nested).expect("nested cwd should exist");
    fs::write(
        command_dir.join("team.md"),
        "---\nname: team\ndescription: Legacy team workflow\n---\n# team\n",
    )
    .expect("legacy command file should exist");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_codex_home = std::env::var("CODEX_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::remove_var("CODEX_HOME");
    std::env::set_current_dir(&nested).expect("set cwd");

    let result = execute_tool("Skill", &json!({ "skill": "team" }))
        .expect("legacy command markdown should resolve");

    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert!(output["path"]
        .as_str()
        .expect("path")
        .ends_with(".claude/commands/team.md"));
    assert_eq!(output["description"], "Legacy team workflow");

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    match original_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    fs::remove_dir_all(root).expect("temp tree should clean up");
}

#[test]
fn tool_search_supports_keyword_and_select_queries() {
    let keyword = execute_tool(
        "ToolSearch",
        &json!({"query": "web current", "max_results": 3}),
    )
    .expect("ToolSearch should succeed");
    let keyword_output: serde_json::Value = serde_json::from_str(&keyword).expect("valid json");
    let matches = keyword_output["matches"].as_array().expect("matches");
    assert!(matches.iter().any(|value| value == "WebSearch"));

    let selected = execute_tool("ToolSearch", &json!({"query": "select:Agent,Skill"}))
        .expect("ToolSearch should succeed");
    let selected_output: serde_json::Value = serde_json::from_str(&selected).expect("valid json");
    assert_eq!(selected_output["matches"][0], "Agent");
    assert_eq!(selected_output["matches"][1], "Skill");

    let aliased = execute_tool("ToolSearch", &json!({"query": "AgentTool"}))
        .expect("ToolSearch should support tool aliases");
    let aliased_output: serde_json::Value = serde_json::from_str(&aliased).expect("valid json");
    assert_eq!(aliased_output["matches"][0], "Agent");
    assert_eq!(aliased_output["normalized_query"], "agent");

    let selected_with_alias =
        execute_tool("ToolSearch", &json!({"query": "select:AgentTool,Skill"}))
            .expect("ToolSearch alias select should succeed");
    let selected_with_alias_output: serde_json::Value =
        serde_json::from_str(&selected_with_alias).expect("valid json");
    assert_eq!(selected_with_alias_output["matches"][0], "Agent");
    assert_eq!(selected_with_alias_output["matches"][1], "Skill");
}

#[test]
fn a_custom_agent_runs_with_its_own_instructions_and_tools() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("custom-agent");
    let agents = root.join(".claw").join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
            agents.join("rust-reviewer.md"),
            "---\nname: rust-reviewer\ndescription: Reviews Rust code\ntools: Read, Grep, Glob, Bash\nmodel: sonnet\n---\n\nYou review Rust for ownership bugs.\n",
        )
        .unwrap();
    let store = root.join("store");
    std::env::set_var("CLAWD_AGENT_STORE", &store);
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");
    let captured = Arc::new(Mutex::new(None::<AgentJob>));
    let captured_for_spawn = Arc::clone(&captured);
    let spawn = move |job| {
        *captured_for_spawn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
        Ok(())
    };
    let input = |kind: &str| AgentInput {
        description: "Review".to_string(),
        prompt: "Review src/lib.rs".to_string(),
        subagent_type: Some(kind.to_string()),
        name: None,
        model: None,
        run_in_background: false,
    };
    let result = execute_agent_with_spawn(input("rust-reviewer"), spawn);
    let unknown = execute_agent_with_spawn(input("no-such-agent"), |_| Ok(()));
    std::env::set_current_dir(&original_dir).expect("restore cwd");
    std::env::remove_var("CLAWD_AGENT_STORE");
    result.expect("custom agent spawns");
    let error = unknown.expect_err("unknown agents are refused");
    assert!(error.contains("rust-reviewer"), "{error}");
    let job = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("job");
    assert!(job
        .system_prompt
        .last()
        .is_some_and(|section| section.contains("You review Rust for ownership bugs.")));
    let tools = job.allowed_tools.iter().cloned().collect::<Vec<_>>();
    assert!(tools.contains(&"read_file".to_string()) && tools.contains(&"bash".to_string()));
    assert!(!tools.contains(&"write_file".to_string()), "{tools:?}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_fork_starts_from_the_parents_conversation_and_system_prompt() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-fork");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);
    let mut parent = Session::new();
    parent
        .push_message(runtime::ConversationMessage::user_text(
            "the parent's request",
        ))
        .unwrap();
    super::set_fork_seed(parent, vec!["parent system prompt".to_string()]);
    let captured = Arc::new(Mutex::new(None::<AgentJob>));
    let captured_for_spawn = Arc::clone(&captured);
    execute_agent_with_spawn(
        AgentInput {
            description: "Check the other file".to_string(),
            prompt: "Look at b.rs the same way.".to_string(),
            subagent_type: Some("fork".to_string()),
            name: None,
            model: None,
            run_in_background: false,
        },
        move |job| {
            *captured_for_spawn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
            Ok(())
        },
    )
    .expect("fork spawns");
    std::env::remove_var("CLAWD_AGENT_STORE");
    let job = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("job");
    assert_eq!(job.system_prompt, vec!["parent system prompt".to_string()]);
    assert_eq!(job.seed.expect("seeded").messages.len(), 1);
    assert!(job
        .prompt
        .starts_with("<system-reminder>You are now a fork"));
    assert!(job.prompt.ends_with("Look at b.rs the same way."));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn agent_persists_handoff_metadata() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-store");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);
    let captured = Arc::new(Mutex::new(None::<AgentJob>));
    let captured_for_spawn = Arc::clone(&captured);

    let manifest = execute_agent_with_spawn(
        AgentInput {
            description: "Audit the branch".to_string(),
            prompt: "Check tests and outstanding work.".to_string(),
            subagent_type: Some("Explore".to_string()),
            name: Some("ship-audit".to_string()),
            model: None,
            run_in_background: false,
        },
        move |job| {
            *captured_for_spawn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
            Ok(())
        },
    )
    .expect("Agent should succeed");
    std::env::remove_var("CLAWD_AGENT_STORE");

    assert_eq!(manifest.name, "ship-audit");
    assert_eq!(manifest.subagent_type.as_deref(), Some("Explore"));
    assert_eq!(manifest.status, "running");
    assert!(!manifest.created_at.is_empty());
    assert!(manifest.started_at.is_some());
    assert!(manifest.completed_at.is_none());
    let contents = std::fs::read_to_string(&manifest.output_file).expect("agent file exists");
    let manifest_contents =
        std::fs::read_to_string(&manifest.manifest_file).expect("manifest file exists");
    let manifest_json: serde_json::Value =
        serde_json::from_str(&manifest_contents).expect("manifest should be valid json");
    assert!(contents.contains("Audit the branch"));
    assert!(contents.contains("Check tests and outstanding work."));
    assert!(manifest_contents.contains("\"subagentType\": \"Explore\""));
    assert!(manifest_contents.contains("\"status\": \"running\""));
    assert_eq!(manifest_json["laneEvents"][0]["event"], "lane.started");
    assert_eq!(manifest_json["laneEvents"][0]["status"], "running");
    assert!(manifest_json["currentBlocker"].is_null());
    let captured_job = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("spawn job should be captured");
    assert_eq!(captured_job.prompt, "Check tests and outstanding work.");
    assert!(captured_job.allowed_tools.contains("read_file"));
    assert!(!captured_job.allowed_tools.contains("agent"));

    let normalized = execute_tool(
        "Agent",
        &json!({
            "run_in_background": true,
            "description": "Verify the branch",
            "prompt": "Check tests.",
            "subagent_type": "explorer"
        }),
    )
    .expect("Agent should normalize built-in aliases");
    let normalized_output: serde_json::Value =
        serde_json::from_str(&normalized).expect("valid json");
    assert_eq!(normalized_output["subagentType"], "Explore");

    let named = execute_tool(
        "Agent",
        &json!({
            "run_in_background": true,
            "description": "Review the branch",
            "prompt": "Inspect diff.",
            "name": "Ship Audit!!!"
        }),
    )
    .expect("Agent should normalize explicit names");
    let named_output: serde_json::Value = serde_json::from_str(&named).expect("valid json");
    assert_eq!(named_output["name"], "ship-audit");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[allow(clippy::too_many_lines)]
fn agent_fake_runner_can_persist_completion_and_failure() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = temp_path("agent-runner");
    std::env::set_var("CLAWD_AGENT_STORE", &dir);

    let completed = execute_agent_with_spawn(
        AgentInput {
            description: "Complete the task".to_string(),
            prompt: "Do the work".to_string(),
            subagent_type: Some("Explore".to_string()),
            name: Some("complete-task".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            run_in_background: false,
        },
        |job| {
            persist_agent_terminal_state(
                &job.manifest,
                "completed",
                Some("Finished successfully in commit abc1234"),
                None,
            )
        },
    )
    .expect("completed agent should succeed");

    let completed_manifest =
        std::fs::read_to_string(&completed.manifest_file).expect("completed manifest should exist");
    let completed_manifest_json: serde_json::Value =
        serde_json::from_str(&completed_manifest).expect("completed manifest json");
    let completed_output =
        std::fs::read_to_string(&completed.output_file).expect("completed output should exist");
    assert!(completed_manifest.contains("\"status\": \"completed\""));
    assert!(completed_output.contains("Finished successfully"));
    assert_eq!(
        completed_manifest_json["laneEvents"][0]["event"],
        "lane.started"
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][1]["event"],
        "lane.finished"
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
        false
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][1]["detail"],
        "Finished successfully in commit abc1234"
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][2]["event"],
        "lane.commit.created"
    );
    assert_eq!(
        completed_manifest_json["laneEvents"][2]["data"]["commit"],
        "abc1234"
    );
    assert!(completed_manifest_json["currentBlocker"].is_null());
    assert_eq!(
        completed_manifest_json["derivedState"],
        "finished_cleanable"
    );

    let failed = execute_agent_with_spawn(
        AgentInput {
            description: "Fail the task".to_string(),
            prompt: "Do the failing work".to_string(),
            subagent_type: Some("Verification".to_string()),
            name: Some("fail-task".to_string()),
            model: None,
            run_in_background: false,
        },
        |job| {
            persist_agent_terminal_state(
                &job.manifest,
                "failed",
                None,
                Some(String::from("tool failed: simulated failure")),
            )
        },
    )
    .expect("failed agent should still spawn");

    let failed_manifest =
        std::fs::read_to_string(&failed.manifest_file).expect("failed manifest should exist");
    let failed_manifest_json: serde_json::Value =
        serde_json::from_str(&failed_manifest).expect("failed manifest json");
    let failed_output =
        std::fs::read_to_string(&failed.output_file).expect("failed output should exist");
    assert!(failed_manifest.contains("\"status\": \"failed\""));
    assert!(failed_manifest.contains("simulated failure"));
    assert!(failed_output.contains("simulated failure"));
    assert!(failed_output.contains("failure_class: tool_runtime"));
    assert_eq!(
        failed_manifest_json["currentBlocker"]["failureClass"],
        "tool_runtime"
    );
    assert_eq!(
        failed_manifest_json["laneEvents"][1]["event"],
        "lane.blocked"
    );
    assert_eq!(
        failed_manifest_json["laneEvents"][2]["event"],
        "lane.failed"
    );
    assert_eq!(
        failed_manifest_json["laneEvents"][2]["failureClass"],
        "tool_runtime"
    );
    assert_eq!(failed_manifest_json["derivedState"], "truly_idle");

    let normalized = execute_agent_with_spawn(
        AgentInput {
            description: "Sweep the next backlog item".to_string(),
            prompt: "Produce a low-signal stop summary".to_string(),
            subagent_type: Some("Explore".to_string()),
            name: Some("summary-floor".to_string()),
            model: None,
            run_in_background: false,
        },
        |job| {
            persist_agent_terminal_state(
                &job.manifest,
                "completed",
                Some("commit push everyting, keep sweeping $ralph"),
                None,
            )
        },
    )
    .expect("normalized agent should succeed");

    let normalized_manifest = std::fs::read_to_string(&normalized.manifest_file)
        .expect("normalized manifest should exist");
    let normalized_manifest_json: serde_json::Value =
        serde_json::from_str(&normalized_manifest).expect("normalized manifest json");
    assert_eq!(
        normalized_manifest_json["laneEvents"][1]["event"],
        "lane.finished"
    );
    let normalized_detail = normalized_manifest_json["laneEvents"][1]["detail"]
        .as_str()
        .expect("normalized detail");
    assert!(normalized_detail.contains("Completed lane `summary-floor`"));
    assert!(normalized_detail.contains("Sweep the next backlog item"));
    assert_eq!(
        normalized_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
        true
    );
    assert_eq!(
        normalized_manifest_json["laneEvents"][1]["data"]["rawSummary"],
        "commit push everyting, keep sweeping $ralph"
    );
    assert_eq!(
        normalized_manifest_json["laneEvents"][1]["data"]["reasons"][0],
        "control_only"
    );

    let recovery = execute_agent_with_spawn(
            AgentInput {
                description: "Recover the stalled audit lane".to_string(),
                prompt: "Normalize OMX reinjection control prose".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("recovery-lane".to_string()),
                model: None,
                run_in_background: false,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some(
                        "Team read-only-audit-only-for-roadm: worker panes stalled, no progress 2m30s. Next: omx team status read-only-audit-only-for-roadm; read worker messages; unblock/reassign or shutdown. [OMX_TMUX_INJECT]",
                    ),
                    None,
                )
            },
        )
        .expect("recovery agent should succeed");

    let recovery_manifest =
        std::fs::read_to_string(&recovery.manifest_file).expect("recovery manifest should exist");
    let recovery_manifest_json: serde_json::Value =
        serde_json::from_str(&recovery_manifest).expect("recovery manifest json");
    let recovery_detail = recovery_manifest_json["laneEvents"][1]["detail"]
        .as_str()
        .expect("recovery detail");
    assert!(recovery_detail.contains("Recovery handoff observed via tmux reinjection"));
    assert!(recovery_detail.contains("read-only-audit-only-for-roadm"));
    assert!(!recovery_detail.contains("OMX_TMUX_INJECT"));
    assert_eq!(
        recovery_manifest_json["laneEvents"][1]["data"]["recoveryOutcome"]["cause"],
        "tmux_reinject_after_idle"
    );
    assert_eq!(
        recovery_manifest_json["laneEvents"][1]["data"]["recoveryOutcome"]["targetLane"],
        "read-only-audit-only-for-roadm"
    );
    assert_eq!(
        recovery_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
        true
    );
    assert_eq!(
        recovery_manifest_json["laneEvents"][1]["data"]["reasons"][0],
        "recovery_control_prose"
    );

    let review = execute_agent_with_spawn(
        AgentInput {
            description: "Review commit 1234abcd for ROADMAP #67".to_string(),
            prompt: "Review the scoped diff".to_string(),
            subagent_type: Some("Verification".to_string()),
            name: Some("review-lane".to_string()),
            model: None,
            run_in_background: false,
        },
        |job| {
            persist_agent_terminal_state(
                &job.manifest,
                "completed",
                Some("APPROVE\n\nTarget: commit 1234abcd\nRationale: scoped diff is safe."),
                None,
            )
        },
    )
    .expect("review agent should succeed");

    let review_manifest =
        std::fs::read_to_string(&review.manifest_file).expect("review manifest should exist");
    let review_manifest_json: serde_json::Value =
        serde_json::from_str(&review_manifest).expect("review manifest json");
    assert_eq!(
        review_manifest_json["laneEvents"][1]["data"]["reviewVerdict"],
        "approve"
    );
    assert_eq!(
        review_manifest_json["laneEvents"][1]["data"]["reviewTarget"],
        "Review commit 1234abcd for ROADMAP #67"
    );
    assert_eq!(
        review_manifest_json["laneEvents"][1]["data"]["reviewRationale"],
        "Target: commit 1234abcd Rationale: scoped diff is safe."
    );
    assert_eq!(
        review_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
        false
    );

    let selection = execute_agent_with_spawn(
            AgentInput {
                description: "Scan ROADMAP Immediate Backlog for the next repo-local item".to_string(),
                prompt: "Choose the next backlog target".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("backlog-scan".to_string()),
                model: None,
                run_in_background: false,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some(
                        "Selected next backlog target.\nChosen: ROADMAP #65\nSkipped: ROADMAP #63, ROADMAP #64\nAction: execute\nRationale: #65 is the next repo-local lane-finished metadata task.",
                    ),
                    None,
                )
            },
        )
        .expect("selection agent should succeed");

    let selection_manifest =
        std::fs::read_to_string(&selection.manifest_file).expect("selection manifest should exist");
    let selection_manifest_json: serde_json::Value =
        serde_json::from_str(&selection_manifest).expect("selection manifest json");
    assert_eq!(
        selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["chosenItems"][0],
        "ROADMAP #65"
    );
    assert_eq!(
        selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["skippedItems"][0],
        "ROADMAP #63"
    );
    assert_eq!(
        selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["skippedItems"][1],
        "ROADMAP #64"
    );
    assert_eq!(
        selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["action"],
        "execute"
    );
    assert_eq!(
        selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["rationale"],
        "#65 is the next repo-local lane-finished metadata task."
    );

    let artifact = execute_agent_with_spawn(
            AgentInput {
                description: "Land ROADMAP #64 provenance hardening".to_string(),
                prompt: "Ship structured artifact provenance".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("artifact-lane".to_string()),
                model: None,
                run_in_background: false,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some(
                        "Completed ROADMAP #64. Files: rust/crates/tools/src/lib.rs ROADMAP.md. Diff stat: 2 files, +12/-1. Tested, committed, pushed as commit deadbee.",
                    ),
                    None,
                )
            },
        )
        .expect("artifact agent should succeed");

    let artifact_manifest =
        std::fs::read_to_string(&artifact.manifest_file).expect("artifact manifest should exist");
    let artifact_manifest_json: serde_json::Value =
        serde_json::from_str(&artifact_manifest).expect("artifact manifest json");
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["sourceLanes"][0],
        "artifact-lane"
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["roadmapIds"][0],
        "ROADMAP #64"
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["files"][0],
        "ROADMAP.md"
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["files"][1],
        "rust/crates/tools/src/lib.rs"
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["diffStat"],
        "2 files, +12/-1."
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["verification"][0],
        "tested"
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["verification"][1],
        "committed"
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["verification"][2],
        "pushed"
    );
    assert_eq!(
        artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["commitSha"],
        "deadbee"
    );

    let cron = global_cron_registry().create(
        "*/10 * * * *",
        "roadmap-nudge-10min for ROADMAP #66",
        Some("ROADMAP #66 reminder"),
    );
    let reminder = execute_agent_with_spawn(
        AgentInput {
            description: "Close ROADMAP #66 reminder shutdown".to_string(),
            prompt: "Finish the cron shutdown fix".to_string(),
            subagent_type: Some("Explore".to_string()),
            name: Some("cron-closeout".to_string()),
            model: None,
            run_in_background: false,
        },
        |job| {
            persist_agent_terminal_state(
                &job.manifest,
                "completed",
                Some("Completed ROADMAP #66 after verification."),
                None,
            )
        },
    )
    .expect("reminder agent should succeed");

    let reminder_manifest =
        std::fs::read_to_string(&reminder.manifest_file).expect("reminder manifest should exist");
    let reminder_manifest_json: serde_json::Value =
        serde_json::from_str(&reminder_manifest).expect("reminder manifest json");
    assert_eq!(
        reminder_manifest_json["laneEvents"][1]["data"]["disabledCronIds"][0],
        cron.cron_id
    );
    let disabled_entry = global_cron_registry()
        .get(&cron.cron_id)
        .expect("cron should still exist");
    assert!(!disabled_entry.enabled);

    let resume_outcome =
        extract_recovery_outcome("Continue from current mode state. [OMX_TMUX_INJECT]")
            .expect("resume outcome should be detected");
    assert_eq!(resume_outcome.cause, "resume_after_stop");
    assert_eq!(
        resume_outcome.preserved_state.as_deref(),
        Some("current mode state")
    );

    let spawn_error = execute_agent_with_spawn(
        AgentInput {
            description: "Spawn error task".to_string(),
            prompt: "Never starts".to_string(),
            subagent_type: None,
            name: Some("spawn-error".to_string()),
            model: None,
            run_in_background: false,
        },
        |_| Err(String::from("thread creation failed")),
    )
    .expect_err("spawn errors should surface");
    assert!(spawn_error.contains("failed to spawn sub-agent"));
    let spawn_error_manifest = std::fs::read_dir(&dir)
        .expect("agent dir should exist")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .find_map(|path| {
            let contents = std::fs::read_to_string(&path).ok()?;
            contents
                .contains("\"name\": \"spawn-error\"")
                .then_some(contents)
        })
        .expect("failed manifest should still be written");
    let spawn_error_manifest_json: serde_json::Value =
        serde_json::from_str(&spawn_error_manifest).expect("spawn error manifest json");
    assert!(spawn_error_manifest.contains("\"status\": \"failed\""));
    assert!(spawn_error_manifest.contains("thread creation failed"));
    assert_eq!(
        spawn_error_manifest_json["currentBlocker"]["failureClass"],
        "infra"
    );
    assert_eq!(spawn_error_manifest_json["derivedState"], "truly_idle");

    std::env::remove_var("CLAWD_AGENT_STORE");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn agent_state_classification_covers_finished_and_specific_blockers() {
    assert_eq!(derive_agent_state("running", None, None, None), "working");
    assert_eq!(
        derive_agent_state("completed", Some("done"), None, None),
        "finished_cleanable"
    );
    assert_eq!(
        derive_agent_state("completed", None, None, None),
        "finished_pending_report"
    );
    assert_eq!(
        derive_agent_state("failed", None, Some("mcp handshake timed out"), None),
        "degraded_mcp"
    );
    assert_eq!(
        derive_agent_state(
            "failed",
            None,
            Some("background terminal still running"),
            None
        ),
        "blocked_background_job"
    );
    assert_eq!(
        derive_agent_state("failed", None, Some("merge conflict while rebasing"), None),
        "blocked_merge_conflict"
    );
    assert_eq!(
        derive_agent_state(
            "failed",
            None,
            Some("transport interrupted after partial progress"),
            None
        ),
        "interrupted_transport"
    );
}

#[test]
fn commit_provenance_is_extracted_from_agent_results() {
    let provenance = maybe_commit_provenance(Some("landed as commit deadbee with clean push"))
        .expect("commit provenance");
    assert_eq!(provenance.commit, "deadbee");
    assert_eq!(provenance.canonical_commit.as_deref(), Some("deadbee"));
    assert_eq!(provenance.lineage, vec!["deadbee".to_string()]);
}
#[test]
fn lane_failure_taxonomy_normalizes_common_blockers() {
    let cases = [
        (
            "prompt delivery failed in tmux pane",
            LaneFailureClass::PromptDelivery,
        ),
        (
            "trust prompt is still blocking startup",
            LaneFailureClass::TrustGate,
        ),
        (
            "branch stale against main after divergence",
            LaneFailureClass::BranchDivergence,
        ),
        (
            "compile failed after cargo check",
            LaneFailureClass::Compile,
        ),
        ("targeted tests failed", LaneFailureClass::Test),
        ("plugin bootstrap failed", LaneFailureClass::PluginStartup),
        ("mcp handshake timed out", LaneFailureClass::McpHandshake),
        (
            "mcp startup failed before listing tools",
            LaneFailureClass::McpStartup,
        ),
        (
            "gateway routing rejected the request",
            LaneFailureClass::GatewayRouting,
        ),
        (
            "tool failed: denied tool execution from hook",
            LaneFailureClass::ToolRuntime,
        ),
        (
            "workspace mismatch while resuming the managed session",
            LaneFailureClass::WorkspaceMismatch,
        ),
        ("thread creation failed", LaneFailureClass::Infra),
    ];

    for (message, expected) in cases {
        assert_eq!(classify_lane_failure(message), expected, "{message}");
    }
}

#[test]
fn lane_event_schema_serializes_to_canonical_names() {
    let cases = [
        (LaneEventName::Started, "lane.started"),
        (LaneEventName::Ready, "lane.ready"),
        (LaneEventName::PromptMisdelivery, "lane.prompt_misdelivery"),
        (LaneEventName::Blocked, "lane.blocked"),
        (LaneEventName::Red, "lane.red"),
        (LaneEventName::Green, "lane.green"),
        (LaneEventName::CommitCreated, "lane.commit.created"),
        (LaneEventName::PrOpened, "lane.pr.opened"),
        (LaneEventName::MergeReady, "lane.merge.ready"),
        (LaneEventName::Finished, "lane.finished"),
        (LaneEventName::Failed, "lane.failed"),
        (
            LaneEventName::BranchStaleAgainstMain,
            "branch.stale_against_main",
        ),
        (
            LaneEventName::BranchWorkspaceMismatch,
            "branch.workspace_mismatch",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(
            serde_json::to_value(event).expect("serialize lane event"),
            json!(expected)
        );
    }
}

#[test]
fn agent_tool_subset_mapping_is_expected() {
    let general = allowed_tools_for_subagent("general-purpose");
    assert!(general.contains("bash"));
    assert!(general.contains("write_file"));
    assert!(!general.contains("agent"));

    let explore = allowed_tools_for_subagent("Explore");
    assert!(explore.contains("read_file"));
    assert!(explore.contains("grep_search"));
    assert!(!explore.contains("bash"));

    let plan = allowed_tools_for_subagent("Plan");
    assert!(plan.contains("todo_write"));
    assert!(plan.contains("structured_output"));
    assert!(!plan.contains("agent"));

    let verification = allowed_tools_for_subagent("Verification");
    assert!(verification.contains("bash"));
    assert!(verification.contains("power_shell"));
    assert!(!verification.contains("write_file"));
}

#[test]
fn subagent_system_prompt_uses_resolved_model_identity() {
    // given: a temporary workspace and an OpenAI-compatible subagent model
    let _guard = env_guard();
    let root = temp_path("subagent-prompt-identity");
    fs::create_dir_all(&root).expect("create temp workspace");
    let previous = std::env::current_dir().expect("current dir");
    std::env::set_current_dir(&root).expect("enter temp workspace");

    // when: building the subagent system prompt
    let prompt = build_agent_system_prompt("Explore", "openai/gpt-4.1-mini")
        .expect("subagent system prompt should build")
        .join("\n");
    std::env::set_current_dir(previous).expect("restore current dir");

    // then: the prompt renders a generic model family identity
    assert!(prompt.contains("Model family: an AI assistant"));
    assert!(!prompt.contains("Model family: Claude Opus 4.6"));

    fs::remove_dir_all(root).expect("cleanup temp workspace");
}

#[test]
fn agent_rejects_blank_required_fields() {
    let missing_description = execute_tool(
        "Agent",
        &json!({
            "run_in_background": true,
            "description": "  ",
            "prompt": "Inspect"
        }),
    )
    .expect_err("blank description should fail");
    assert!(missing_description.contains("description must not be empty"));

    let missing_prompt = execute_tool(
        "Agent",
        &json!({
            "run_in_background": true,
            "description": "Inspect branch",
            "prompt": " "
        }),
    )
    .expect_err("blank prompt should fail");
    assert!(missing_prompt.contains("prompt must not be empty"));
}

#[test]
fn notebook_edit_replaces_inserts_and_deletes_cells() {
    let path = temp_path("notebook.ipynb");
    std::fs::write(
            &path,
            r#"{
  "cells": [
    {"cell_type": "code", "id": "cell-a", "metadata": {}, "source": ["print(1)\n"], "outputs": [], "execution_count": null}
  ],
  "metadata": {"kernelspec": {"language": "python"}},
  "nbformat": 4,
  "nbformat_minor": 5
}"#,
        )
        .expect("write notebook");

    let replaced = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "cell_id": "cell-a",
            "new_source": "print(2)\n",
            "edit_mode": "replace"
        }),
    )
    .expect("NotebookEdit replace should succeed");
    let replaced_output: serde_json::Value = serde_json::from_str(&replaced).expect("json");
    assert_eq!(replaced_output["cell_id"], "cell-a");
    assert_eq!(replaced_output["cell_type"], "code");

    let inserted = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "cell_id": "cell-a",
            "new_source": "# heading\n",
            "cell_type": "markdown",
            "edit_mode": "insert"
        }),
    )
    .expect("NotebookEdit insert should succeed");
    let inserted_output: serde_json::Value = serde_json::from_str(&inserted).expect("json");
    assert_eq!(inserted_output["cell_type"], "markdown");
    let appended = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "new_source": "print(3)\n",
            "edit_mode": "insert"
        }),
    )
    .expect("NotebookEdit append should succeed");
    let appended_output: serde_json::Value = serde_json::from_str(&appended).expect("json");
    assert_eq!(appended_output["cell_type"], "code");

    let deleted = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": path.display().to_string(),
            "cell_id": "cell-a",
            "edit_mode": "delete"
        }),
    )
    .expect("NotebookEdit delete should succeed without new_source");
    let deleted_output: serde_json::Value = serde_json::from_str(&deleted).expect("json");
    assert!(deleted_output["cell_type"].is_null());
    assert_eq!(deleted_output["new_source"], "");

    let final_notebook: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read notebook"))
            .expect("valid notebook json");
    let cells = final_notebook["cells"].as_array().expect("cells array");
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0]["cell_type"], "markdown");
    assert!(cells[0].get("outputs").is_none());
    assert_eq!(cells[1]["cell_type"], "code");
    assert_eq!(cells[1]["source"][0], "print(3)\n");
    let _ = std::fs::remove_file(crate::todo_events::log_path(&path));
    let _ = std::fs::remove_file(path);
}

#[test]
fn notebook_edit_rejects_invalid_inputs() {
    let text_path = temp_path("notebook.txt");
    fs::write(&text_path, "not a notebook").expect("write text file");
    let wrong_extension = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": text_path.display().to_string(),
            "new_source": "print(1)\n"
        }),
    )
    .expect_err("non-ipynb file should fail");
    assert!(wrong_extension.contains("Jupyter notebook"));
    let _ = fs::remove_file(&text_path);

    let empty_notebook = temp_path("empty.ipynb");
    fs::write(
            &empty_notebook,
            r#"{"cells":[],"metadata":{"kernelspec":{"language":"python"}},"nbformat":4,"nbformat_minor":5}"#,
        )
        .expect("write empty notebook");

    let missing_source = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": empty_notebook.display().to_string(),
            "edit_mode": "insert"
        }),
    )
    .expect_err("insert without source should fail");
    assert!(missing_source.contains("new_source is required"));

    let missing_cell = execute_tool(
        "NotebookEdit",
        &json!({
            "notebook_path": empty_notebook.display().to_string(),
            "edit_mode": "delete"
        }),
    )
    .expect_err("delete on empty notebook should fail");
    assert!(missing_cell.contains("Notebook has no cells to edit"));
    let _ = fs::remove_file(empty_notebook);
}

#[test]
fn bash_tool_reports_success_exit_failure_timeout_and_background() {
    let success =
        execute_tool("bash", &json!({ "command": "printf 'hello'" })).expect("bash should succeed");
    let success_output: serde_json::Value = serde_json::from_str(&success).expect("json");
    assert_eq!(success_output["stdout"], "hello");
    assert_eq!(success_output["interrupted"], false);

    let failure = execute_tool("bash", &json!({ "command": "printf 'oops' >&2; exit 7" }))
        .expect("bash failure should still return structured output");
    let failure_output: serde_json::Value = serde_json::from_str(&failure).expect("json");
    assert_eq!(failure_output["returnCodeInterpretation"], "exit_code:7");
    assert!(failure_output["stderr"]
        .as_str()
        .expect("stderr")
        .contains("oops"));

    let timeout = execute_tool("bash", &json!({ "command": "sleep 1", "timeout": 10 }))
        .expect("bash timeout should return output");
    let timeout_output: serde_json::Value = serde_json::from_str(&timeout).expect("json");
    assert_eq!(timeout_output["interrupted"], true);
    assert_eq!(timeout_output["returnCodeInterpretation"], "timeout");
    assert!(timeout_output["stderr"]
        .as_str()
        .expect("stderr")
        .contains("Command exceeded timeout"));

    let background = execute_tool(
        "bash",
        &json!({ "command": "sleep 1", "run_in_background": true }),
    )
    .expect("bash background should succeed");
    let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
    assert!(background_output["backgroundTaskId"].as_str().is_some());
    assert_eq!(background_output["noOutputExpected"], true);
}

#[test]
fn bash_tool_classifies_test_timeout_as_hung_with_provenance() {
    let timeout = execute_tool(
        "bash",
        &json!({ "command": "sleep 1 # cargo test slow_case", "timeout": 10 }),
    )
    .expect("bash timeout should return output");
    let timeout_output: serde_json::Value = serde_json::from_str(&timeout).expect("json");
    assert_eq!(timeout_output["interrupted"], true);
    assert_eq!(timeout_output["returnCodeInterpretation"], "test.hung");
    assert_eq!(timeout_output["structuredContent"][0]["event"], "test.hung");
    assert_eq!(
        timeout_output["structuredContent"][0]["data"]["provenance"],
        "bash.timeout"
    );
}

#[test]
fn bash_workspace_tests_are_blocked_when_branch_is_behind_main() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("workspace-test-preflight");
    let original_dir = std::env::current_dir().expect("cwd");
    init_git_repo(&root);
    run_git(&root, &["checkout", "-b", "feature/stale-tests"]);
    run_git(&root, &["checkout", "main"]);
    commit_file(
        &root,
        "hotfix.txt",
        "fix from main\n",
        "fix: unblock workspace tests",
    );
    run_git(&root, &["checkout", "feature/stale-tests"]);
    std::env::set_current_dir(&root).expect("set cwd");

    let output = execute_tool(
        "bash",
        &json!({ "command": "cargo test --workspace --all-targets" }),
    )
    .expect("preflight should return structured output");
    let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_eq!(
        output_json["returnCodeInterpretation"],
        "preflight_blocked:branch_divergence"
    );
    assert!(output_json["stderr"]
        .as_str()
        .expect("stderr")
        .contains("branch divergence detected before workspace tests"));
    assert_eq!(
        output_json["structuredContent"][0]["event"],
        "branch.stale_against_main"
    );
    assert_eq!(
        output_json["structuredContent"][0]["failureClass"],
        "branch_divergence"
    );
    assert_eq!(
        output_json["structuredContent"][0]["data"]["missingCommits"][0],
        "fix: unblock workspace tests"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bash_targeted_tests_skip_branch_preflight() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("targeted-test-no-preflight");
    let original_dir = std::env::current_dir().expect("cwd");
    init_git_repo(&root);
    run_git(&root, &["checkout", "-b", "feature/targeted-tests"]);
    run_git(&root, &["checkout", "main"]);
    commit_file(
        &root,
        "hotfix.txt",
        "fix from main\n",
        "fix: only broad tests should block",
    );
    run_git(&root, &["checkout", "feature/targeted-tests"]);
    std::env::set_current_dir(&root).expect("set cwd");

    let output = execute_tool(
        "bash",
        &json!({ "command": "printf 'targeted ok'; cargo test -p runtime stale_branch" }),
    )
    .expect("targeted commands should still execute");
    let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_ne!(
        output_json["returnCodeInterpretation"],
        "preflight_blocked:branch_divergence"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn file_tools_cover_read_write_and_edit_behaviors() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("fs-suite");
    fs::create_dir_all(&root).expect("create root");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let write_create = execute_tool(
        "write_file",
        &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
    )
    .expect("write create should succeed");
    let write_create_output: serde_json::Value = serde_json::from_str(&write_create).expect("json");
    assert_eq!(write_create_output["type"], "create");
    assert!(root.join("nested/demo.txt").exists());

    let write_update = execute_tool(
        "write_file",
        &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\ngamma\n" }),
    )
    .expect("write update should succeed");
    let write_update_output: serde_json::Value = serde_json::from_str(&write_update).expect("json");
    assert_eq!(write_update_output["type"], "update");
    assert_eq!(write_update_output["originalFile"], "alpha\nbeta\nalpha\n");

    let read_full = execute_tool("read_file", &json!({ "path": "nested/demo.txt" }))
        .expect("read full should succeed");
    let read_full_output: serde_json::Value = serde_json::from_str(&read_full).expect("json");
    assert_eq!(read_full_output["file"]["content"], "alpha\nbeta\ngamma");
    assert_eq!(read_full_output["file"]["startLine"], 1);

    let read_slice = execute_tool(
        "read_file",
        &json!({ "path": "nested/demo.txt", "offset": 1, "limit": 1 }),
    )
    .expect("read slice should succeed");
    let read_slice_output: serde_json::Value = serde_json::from_str(&read_slice).expect("json");
    assert_eq!(read_slice_output["file"]["content"], "beta");
    assert_eq!(read_slice_output["file"]["startLine"], 2);

    let read_past_end = execute_tool(
        "read_file",
        &json!({ "path": "nested/demo.txt", "offset": 50 }),
    )
    .expect("read past EOF should succeed");
    let read_past_end_output: serde_json::Value =
        serde_json::from_str(&read_past_end).expect("json");
    assert_eq!(read_past_end_output["file"]["content"], "");
    assert_eq!(read_past_end_output["file"]["startLine"], 4);

    let read_error = execute_tool("read_file", &json!({ "path": "missing.txt" }))
        .expect_err("missing file should fail");
    assert!(!read_error.is_empty());
    // Errors reach the model as plain text, not a Debug-printed wrapper.
    assert!(!read_error.contains("ExecutionError"), "{read_error}");
    // Other agents' argument names are accepted.
    let read_alias = execute_tool("read_file", &json!({ "file_path": "nested/demo.txt" }))
        .expect("file_path alias should read");
    assert!(read_alias.contains("alpha"));

    let edit_once = execute_tool(
        "edit_file",
        &json!({ "path": "nested/demo.txt", "old_string": "alpha", "new_string": "omega" }),
    )
    .expect("single edit should succeed");
    let edit_once_output: serde_json::Value = serde_json::from_str(&edit_once).expect("json");
    assert_eq!(edit_once_output["replaceAll"], false);
    assert_eq!(
        fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
        "omega\nbeta\ngamma\n"
    );

    execute_tool(
        "write_file",
        &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
    )
    .expect("reset file");
    let edit_all = execute_tool(
        "edit_file",
        &json!({
            "path": "nested/demo.txt",
            "old_string": "alpha",
            "new_string": "omega",
            "replace_all": true
        }),
    )
    .expect("replace all should succeed");
    let edit_all_output: serde_json::Value = serde_json::from_str(&edit_all).expect("json");
    assert_eq!(edit_all_output["replaceAll"], true);
    assert_eq!(
        fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
        "omega\nbeta\nomega\n"
    );

    let edit_same = execute_tool(
        "edit_file",
        &json!({ "path": "nested/demo.txt", "old_string": "omega", "new_string": "omega" }),
    )
    .expect_err("identical old/new should fail");
    assert!(edit_same.contains("must differ"));

    let edit_missing = execute_tool(
        "edit_file",
        &json!({ "path": "nested/demo.txt", "old_string": "missing", "new_string": "omega" }),
    )
    .expect_err("missing substring should fail");
    assert!(edit_missing.contains("old_string not found"));

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn glob_and_grep_tools_cover_success_and_errors() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("search-suite");
    fs::create_dir_all(root.join("nested")).expect("create root");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    fs::write(
        root.join("nested/lib.rs"),
        "fn main() {}\nlet alpha = 1;\nlet alpha = 2;\n",
    )
    .expect("write rust file");
    fs::write(root.join("nested/notes.txt"), "alpha\nbeta\n").expect("write txt file");

    let globbed = execute_tool("glob_search", &json!({ "pattern": "nested/*.rs" }))
        .expect("glob should succeed");
    let globbed_output: serde_json::Value = serde_json::from_str(&globbed).expect("json");
    assert_eq!(globbed_output["numFiles"], 1);
    assert!(globbed_output["filenames"][0]
        .as_str()
        .expect("filename")
        .ends_with("nested/lib.rs"));

    let glob_error = execute_tool("glob_search", &json!({ "pattern": "[" }))
        .expect_err("invalid glob should fail");
    assert!(!glob_error.is_empty());

    let grep_content = execute_tool(
        "grep_search",
        &json!({
            "pattern": "alpha",
            "path": "nested",
            "glob": "*.rs",
            "output_mode": "content",
            "-n": true,
            "head_limit": 1,
            "offset": 1
        }),
    )
    .expect("grep content should succeed");
    let grep_content_output: serde_json::Value = serde_json::from_str(&grep_content).expect("json");
    assert_eq!(grep_content_output["numFiles"], 0);
    assert!(grep_content_output["appliedLimit"].is_null());
    assert_eq!(grep_content_output["appliedOffset"], 1);
    assert!(grep_content_output["content"]
        .as_str()
        .expect("content")
        .contains("let alpha = 2;"));

    let grep_count = execute_tool(
        "grep_search",
        &json!({ "pattern": "alpha", "path": "nested", "output_mode": "count" }),
    )
    .expect("grep count should succeed");
    let grep_count_output: serde_json::Value = serde_json::from_str(&grep_count).expect("json");
    assert_eq!(grep_count_output["numMatches"], 3);

    let grep_error = execute_tool(
        "grep_search",
        &json!({ "pattern": "(alpha", "path": "nested" }),
    )
    .expect_err("invalid regex should fail");
    assert!(!grep_error.is_empty());

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
}

/// Paths outside the workspace are allowed by the tools themselves;
/// whether a call may run is the permission policy's decision (writes
/// outside the current directory need approval in the default mode).
#[test]
fn file_tools_reach_paths_outside_the_workspace() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("workspace-scope");
    let outside = temp_path("workspace-scope-outside");
    fs::create_dir_all(&root).expect("create root");
    fs::create_dir_all(&outside).expect("create outside");
    fs::write(outside.join("notes.txt"), "notes\n").expect("outside fixture");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let read = execute_tool(
        "read_file",
        &json!({ "path": outside.join("notes.txt").display().to_string() }),
    )
    .expect("read outside the workspace");
    assert!(read.contains("notes"));

    execute_tool(
        "write_file",
        &json!({ "path": outside.join("created.txt").display().to_string(), "content": "yes" }),
    )
    .expect("write outside the workspace");
    assert_eq!(
        fs::read_to_string(outside.join("created.txt")).expect("read back"),
        "yes"
    );

    let glob = execute_tool(
        "glob_search",
        &json!({ "pattern": outside.join("*.txt").display().to_string() }),
    )
    .expect("glob outside the workspace");
    assert!(glob.contains("notes.txt"));

    let grep = execute_tool(
        "grep_search",
        &json!({ "pattern": "notes", "path": outside.display().to_string() }),
    )
    .expect("grep outside the workspace");
    assert!(grep.contains("notes.txt"));

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

/// Checks the expansion without changing `HOME`: other tests run at the
/// same time and may depend on it (a `python3` shim, for example).
#[test]
fn file_tools_expand_home_in_paths() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    let expanded = super::expand_home_in_paths(
        &json!({ "path": "~/notes/todo.txt", "pattern": "~/*.md", "other": "~/kept" }),
    );

    assert_eq!(
        expanded["path"],
        home.join("notes/todo.txt").display().to_string()
    );
    assert_eq!(expanded["pattern"], home.join("*.md").display().to_string());
    assert_eq!(expanded["other"], "~/kept", "only path-like fields change");
    assert_eq!(
        super::expand_home_in_paths(&json!({ "path": "a~/b" }))["path"],
        "a~/b"
    );
}

#[test]
#[cfg(unix)]
fn file_tools_follow_symlinks_out_of_the_workspace() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("workspace-symlink-scope");
    let outside = temp_path("workspace-symlink-outside");
    fs::create_dir_all(&root).expect("create root");
    fs::create_dir_all(&outside).expect("create outside");
    fs::write(outside.join("notes.txt"), "linked\n").expect("outside fixture");
    std::os::unix::fs::symlink(outside.join("notes.txt"), root.join("link.txt"))
        .expect("create symlink");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let read = execute_tool("read_file", &json!({ "path": "link.txt" }))
        .expect("read through the symlink");
    assert!(read.contains("linked"));

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn sleep_waits_and_reports_duration() {
    let started = std::time::Instant::now();
    let result = execute_tool("Sleep", &json!({"duration_ms": 20})).expect("Sleep should succeed");
    let elapsed = started.elapsed();
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["duration_ms"], 20);
    assert!(output["message"]
        .as_str()
        .expect("message")
        .contains("Slept for 20ms"));
    assert!(elapsed >= Duration::from_millis(15));
}

#[test]
fn given_excessive_duration_when_sleep_then_rejects_with_error() {
    let result = execute_tool("Sleep", &json!({"duration_ms": 999_999_999_u64}));
    let error = result.expect_err("excessive sleep should fail");
    assert!(error.contains("exceeds maximum allowed sleep"));
}

#[test]
fn given_zero_duration_when_sleep_then_succeeds() {
    let result =
        execute_tool("Sleep", &json!({"duration_ms": 0})).expect("0ms sleep should succeed");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["duration_ms"], 0);
}

#[test]
fn brief_returns_sent_message_and_attachment_metadata() {
    let attachment = std::env::temp_dir().join(format!(
        "clawd-brief-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::write(&attachment, b"png-data").expect("write attachment");

    let result = execute_tool(
        "SendUserMessage",
        &json!({
            "message": "hello user",
            "attachments": [attachment.display().to_string()],
            "status": "normal"
        }),
    )
    .expect("SendUserMessage should succeed");

    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["message"], "hello user");
    assert!(output["sentAt"].as_str().is_some());
    assert_eq!(output["attachments"][0]["isImage"], true);
    let _ = std::fs::remove_file(attachment);
}

#[test]
fn config_reads_and_writes_supported_values() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = std::env::temp_dir().join(format!(
        "clawd-config-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(home.join(".claw")).expect("home dir");
    std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");
    std::fs::write(
        home.join(".claw").join("settings.json"),
        r#"{"verbose":false}"#,
    )
    .expect("write global settings");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::set_current_dir(&cwd).expect("set cwd");

    let get = execute_tool("Config", &json!({"setting": "verbose"})).expect("get config");
    let get_output: serde_json::Value = serde_json::from_str(&get).expect("json");
    assert_eq!(get_output["value"], false);

    let set = execute_tool(
        "Config",
        &json!({"setting": "permissions.defaultMode", "value": "plan"}),
    )
    .expect("set config");
    let set_output: serde_json::Value = serde_json::from_str(&set).expect("json");
    assert_eq!(set_output["operation"], "set");
    assert_eq!(set_output["newValue"], "plan");

    let invalid = execute_tool(
        "Config",
        &json!({"setting": "permissions.defaultMode", "value": "bogus"}),
    )
    .expect_err("invalid config value should error");
    assert!(invalid.contains("Invalid value"));

    let unknown =
        execute_tool("Config", &json!({"setting": "nope"})).expect("unknown setting result");
    let unknown_output: serde_json::Value = serde_json::from_str(&unknown).expect("json");
    assert_eq!(unknown_output["success"], false);

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn enter_and_exit_plan_mode_round_trip_existing_local_override() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = std::env::temp_dir().join(format!(
        "clawd-plan-mode-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(home.join(".claw")).expect("home dir");
    std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");
    std::fs::write(
        cwd.join(".claw").join("settings.local.json"),
        r#"{"permissions":{"defaultMode":"acceptEdits"}}"#,
    )
    .expect("write local settings");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::set_current_dir(&cwd).expect("set cwd");

    let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
    let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
    assert_eq!(enter_output["changed"], true);
    assert_eq!(enter_output["managed"], true);
    assert_eq!(enter_output["previousLocalMode"], "acceptEdits");
    assert_eq!(enter_output["currentLocalMode"], "plan");

    let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
        .expect("local settings after enter");
    assert!(local_settings.contains(r#""defaultMode": "plan""#));
    let state =
        std::fs::read_to_string(cwd.join(".claw").join("tool-state").join("plan-mode.json"))
            .expect("plan mode state");
    assert!(state.contains(r#""hadLocalOverride": true"#));
    assert!(state.contains(r#""previousLocalMode": "acceptEdits""#));

    let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
    let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
    assert_eq!(exit_output["changed"], true);
    assert_eq!(exit_output["managed"], false);
    assert_eq!(exit_output["previousLocalMode"], "acceptEdits");
    assert_eq!(exit_output["currentLocalMode"], "acceptEdits");

    let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
        .expect("local settings after exit");
    assert!(local_settings.contains(r#""defaultMode": "acceptEdits""#));
    assert!(!cwd
        .join(".claw")
        .join("tool-state")
        .join("plan-mode.json")
        .exists());

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn exit_plan_mode_clears_override_when_enter_created_it_from_empty_local_state() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = std::env::temp_dir().join(format!(
        "clawd-plan-mode-empty-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let home = root.join("home");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(home.join(".claw")).expect("home dir");
    std::fs::create_dir_all(cwd.join(".claw")).expect("cwd dir");

    let original_home = std::env::var("HOME").ok();
    let original_config_home = std::env::var("CLAW_CONFIG_HOME").ok();
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAW_CONFIG_HOME");
    std::env::set_current_dir(&cwd).expect("set cwd");

    let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
    let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
    assert_eq!(enter_output["previousLocalMode"], serde_json::Value::Null);
    assert_eq!(enter_output["currentLocalMode"], "plan");

    let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
    let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
    assert_eq!(exit_output["changed"], true);
    assert_eq!(exit_output["currentLocalMode"], serde_json::Value::Null);

    let local_settings = std::fs::read_to_string(cwd.join(".claw").join("settings.local.json"))
        .expect("local settings after exit");
    let local_settings_json: serde_json::Value =
        serde_json::from_str(&local_settings).expect("valid settings json");
    assert_eq!(
        local_settings_json.get("permissions"),
        None,
        "permissions override should be removed on exit"
    );
    assert!(!cwd
        .join(".claw")
        .join("tool-state")
        .join("plan-mode.json")
        .exists());

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_config_home {
        Some(value) => std::env::set_var("CLAW_CONFIG_HOME", value),
        None => std::env::remove_var("CLAW_CONFIG_HOME"),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn structured_output_echoes_input_payload() {
    let result = execute_tool("StructuredOutput", &json!({"ok": true, "items": [1, 2, 3]}))
        .expect("StructuredOutput should succeed");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["data"], "Structured output provided successfully");
    assert_eq!(output["structured_output"]["ok"], true);
    assert_eq!(output["structured_output"]["items"][1], 2);
}

#[test]
fn given_empty_payload_when_structured_output_then_rejects_with_error() {
    let result = execute_tool("StructuredOutput", &json!({}));
    let error = result.expect_err("empty payload should fail");
    assert!(error.contains("must not be empty"));
}

#[test]
fn repl_executes_python_code() {
    // Another test points PATH at an empty directory under this lock.
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = execute_tool(
        "REPL",
        &json!({"language": "python", "code": "print(1 + 1)", "timeout_ms": 30_000}),
    )
    .expect("REPL should succeed");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["language"], "python");
    assert_eq!(output["exitCode"], 0);
    assert!(output["stdout"].as_str().expect("stdout").contains('2'));
}

#[test]
fn repl_with_timeout_drains_large_output_without_deadlock() {
    // Another test points PATH at an empty directory under this lock.
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let result = execute_tool(
        "REPL",
        &json!({"language": "python", "code": "print('x' * 200000)", "timeout_ms": 30_000}),
    )
    .expect("large output must not be mistaken for a timeout");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["exitCode"], 0);
    assert!(output["stdout"].as_str().expect("stdout").len() >= 200_000);
}

#[test]
fn given_empty_code_when_repl_then_rejects_with_error() {
    let result = execute_tool("REPL", &json!({"language": "python", "code": "   "}));

    let error = result.expect_err("empty REPL code should fail");
    assert!(error.contains("code must not be empty"));
}

#[test]
fn given_unsupported_language_when_repl_then_rejects_with_error() {
    let result = execute_tool("REPL", &json!({"language": "ruby", "code": "puts 1"}));

    let error = result.expect_err("unsupported REPL language should fail");
    assert!(error.contains("unsupported REPL language: ruby"));
}

#[test]
fn given_timeout_ms_when_repl_blocks_then_returns_timeout_error() {
    let result = execute_tool(
        "REPL",
        &json!({
            "language": "python",
            "code": "import time\ntime.sleep(1)",
            "timeout_ms": 10
        }),
    );

    let error = result.expect_err("timed out REPL execution should fail");
    assert!(error.contains("REPL execution exceeded timeout of 10 ms"));
}

#[test]
fn powershell_runs_via_stub_shell() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!(
        "clawd-pwsh-bin-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create dir");
    let script = dir.join("pwsh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while [ "$1" != "-Command" ] && [ $# -gt 0 ]; do shift; done
shift
printf 'pwsh:%s' "$1"
"#,
    )
    .expect("write script");
    std::process::Command::new("/bin/chmod")
        .arg("+x")
        .arg(&script)
        .status()
        .expect("chmod");
    let original_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{}", dir.display(), original_path));

    let result = execute_tool(
        "PowerShell",
        &json!({"command": "Write-Output hello", "timeout": 1000}),
    )
    .expect("PowerShell should succeed");

    let background = execute_tool(
        "PowerShell",
        &json!({"command": "Write-Output hello", "run_in_background": true}),
    )
    .expect("PowerShell background should succeed");

    std::env::set_var("PATH", original_path);
    let _ = std::fs::remove_dir_all(dir);

    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["stdout"], "pwsh:Write-Output hello");
    assert!(output["stderr"].as_str().expect("stderr").is_empty());

    let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
    assert!(background_output["backgroundTaskId"].as_str().is_some());
    assert_eq!(background_output["backgroundedByUser"], true);
    assert_eq!(background_output["assistantAutoBackgrounded"], false);
}

#[test]
fn powershell_errors_when_shell_is_missing() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let original_path = std::env::var("PATH").unwrap_or_default();
    let empty_dir = std::env::temp_dir().join(format!(
        "clawd-empty-bin-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&empty_dir).expect("create empty dir");
    std::env::set_var("PATH", empty_dir.display().to_string());

    let err = execute_tool("PowerShell", &json!({"command": "Write-Output hello"}))
        .expect_err("PowerShell should fail when shell is missing");

    std::env::set_var("PATH", original_path);
    let _ = std::fs::remove_dir_all(empty_dir);

    assert!(err.contains("PowerShell executable not found"));
}

fn read_only_registry() -> super::GlobalToolRegistry {
    use runtime::permission_enforcer::PermissionEnforcer;
    use runtime::PermissionPolicy;

    let policy = mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(runtime::PermissionMode::ReadOnly),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    );
    let mut registry = super::GlobalToolRegistry::builtin();
    registry.set_enforcer(PermissionEnforcer::new(policy));
    registry
}

fn workspace_write_registry() -> super::GlobalToolRegistry {
    use runtime::permission_enforcer::PermissionEnforcer;
    use runtime::PermissionPolicy;

    let policy = mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(runtime::PermissionMode::WorkspaceWrite),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    );
    let mut registry = super::GlobalToolRegistry::builtin();
    registry.set_enforcer(PermissionEnforcer::new(policy));
    registry
}

#[test]
fn given_read_only_enforcer_when_bash_then_denied() {
    let registry = read_only_registry();
    // Use a command that requires DangerFullAccess (rm) to ensure it's blocked in read-only mode
    let err = registry
        .execute("bash", &json!({ "command": "rm -rf /" }))
        .expect_err("bash should be denied in read-only mode");
    assert!(
        err.contains("current mode is 'read-only'"),
        "should cite active mode: {err}"
    );
}

#[test]
fn given_workspace_write_enforcer_when_web_tools_then_denied() {
    let registry = workspace_write_registry();
    for (tool, input) in [
        (
            "WebFetch",
            json!({"url":"https://example.com", "prompt":"summarize"}),
        ),
        ("WebSearch", json!({"query":"rust language"})),
    ] {
        let err = registry
            .execute(tool, &input)
            .expect_err("network tools should require explicit full access");
        assert!(
            err.contains("requires 'danger-full-access'"),
            "{tool} should require elevated mode: {err}"
        );
    }
}

#[test]
fn given_workspace_write_enforcer_when_bash_uses_shell_expansion_then_denied() {
    let registry = workspace_write_registry();
    let err = registry
        .execute("bash", &json!({ "command": "cat $HOME/.ssh/config" }))
        .expect_err("shell-expanded path should require elevated permission");
    assert!(
        err.contains("requires 'danger-full-access'"),
        "should require elevated mode: {err}"
    );
}

#[test]
fn given_workspace_write_enforcer_when_bash_uses_windows_absolute_path_then_denied() {
    let registry = workspace_write_registry();
    let err = registry
        .execute(
            "bash",
            &json!({ "command": r"cat C:\\Users\\alice\\.ssh\\config" }),
        )
        .expect_err("Windows absolute path should require elevated permission");
    assert!(
        err.contains("requires 'danger-full-access'"),
        "should require elevated mode: {err}"
    );
}

#[test]
#[cfg(unix)]
fn given_workspace_write_enforcer_when_bash_reads_symlink_escape_then_denied() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("bash-symlink-scope");
    let outside = temp_path("bash-symlink-outside");
    fs::create_dir_all(&root).expect("create root");
    fs::create_dir_all(&outside).expect("create outside");
    fs::write(outside.join("secret.txt"), "secret\n").expect("outside fixture");
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt"))
        .expect("create symlink");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let registry = workspace_write_registry();
    let err = registry
        .execute("bash", &json!({ "command": "cat link.txt" }))
        .expect_err("symlink escape should require elevated permission");
    assert!(
        err.contains("requires 'danger-full-access'"),
        "should require elevated mode: {err}"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn given_read_only_enforcer_when_write_file_then_denied() {
    let registry = read_only_registry();
    let err = registry
        .execute(
            "write_file",
            &json!({ "path": "/tmp/x.txt", "content": "x" }),
        )
        .expect_err("write_file should be denied in read-only mode");
    assert!(
        err.contains("current mode is 'read-only'"),
        "should cite active mode: {err}"
    );
}

#[test]
fn given_read_only_enforcer_when_edit_file_then_denied() {
    let registry = read_only_registry();
    let err = registry
        .execute(
            "edit_file",
            &json!({ "path": "/tmp/x.txt", "old_string": "a", "new_string": "b" }),
        )
        .expect_err("edit_file should be denied in read-only mode");
    assert!(
        err.contains("current mode is 'read-only'"),
        "should cite active mode: {err}"
    );
}

#[test]
fn given_read_only_enforcer_when_read_file_then_not_permission_denied() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_path("perm-read");
    fs::create_dir_all(&root).expect("create root");
    let file = root.join("readable.txt");
    fs::write(&file, "content\n").expect("write test file");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let registry = read_only_registry();
    let result = registry.execute("read_file", &json!({ "path": file.display().to_string() }));
    assert!(result.is_ok(), "read_file should be allowed: {result:?}");

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn given_read_only_enforcer_when_glob_search_then_not_permission_denied() {
    let registry = read_only_registry();
    let result = registry.execute("glob_search", &json!({ "pattern": "*.rs" }));
    assert!(
        result.is_ok(),
        "glob_search should be allowed in read-only mode: {result:?}"
    );
}

#[test]
fn given_no_enforcer_when_bash_then_executes_normally() {
    let _guard = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registry = super::GlobalToolRegistry::builtin();
    let result = registry
        .execute("bash", &json!({ "command": "printf 'ok'" }))
        .expect("bash should succeed without enforcer");
    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["stdout"], "ok");
}

#[test]
fn run_task_packet_creates_packet_backed_task() {
    use runtime::task_packet::TaskScope;
    let result = run_task_packet(TaskPacket {
        objective: "Ship packetized runtime task".to_string(),
        scope: TaskScope::Module,
        scope_path: Some("runtime/task system".to_string()),
        worktree: Some("/tmp/wt-packet".to_string()),
        repo: "claw-code-parity".to_string(),
        branch_policy: "origin/main only".to_string(),
        acceptance_tests: vec![
            "cargo build --workspace".to_string(),
            "cargo test --workspace".to_string(),
        ],
        acceptance_criteria: vec!["task packet is accepted".to_string()],
        resources: vec![runtime::TaskResource {
            kind: "module".to_string(),
            value: "runtime/task system".to_string(),
        }],
        model: Some("gpt-5.5".to_string()),
        provider: Some("openai".to_string()),
        permission_profile: Some("workspace-write".to_string()),
        commit_policy: "single commit".to_string(),
        reporting_contract: "print build/test result and sha".to_string(),
        reporting_targets: vec!["leader".to_string()],
        escalation_policy: "manual escalation".to_string(),
        recovery_policy: Some("retry once".to_string()),
        verification_plan: vec!["cargo test --workspace".to_string()],
    })
    .expect("task packet should create a task");

    let output: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(output["status"], "created");
    assert_eq!(output["prompt"], "Ship packetized runtime task");
    assert_eq!(output["description"], "runtime/task system");
    assert_eq!(output["task_packet"]["repo"], "claw-code-parity");
    assert_eq!(output["task_packet"]["resources"][0]["kind"], "module");
    assert_eq!(
        output["task_packet"]["resources"][0]["value"],
        "runtime/task system"
    );
    assert_eq!(
        output["task_packet"]["acceptance_criteria"][0],
        "task packet is accepted"
    );
    assert_eq!(output["task_packet"]["model"], "gpt-5.5");
    assert_eq!(output["task_packet"]["provider"], "openai");
    assert_eq!(
        output["task_packet"]["permission_profile"],
        "workspace-write"
    );
    assert_eq!(
        output["task_packet"]["verification_plan"][0],
        "cargo test --workspace"
    );
    assert_eq!(output["task_packet"]["reporting_targets"][0], "leader");
    assert_eq!(
        output["task_packet"]["acceptance_tests"][1],
        "cargo test --workspace"
    );
}

struct TestServer {
    addr: SocketAddr,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn spawn(handler: Arc<dyn Fn(&str) -> HttpResponse + Send + Sync + 'static>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        let handle = thread::spawn(move || loop {
            if rx.try_recv().is_ok() {
                break;
            }

            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buffer = [0_u8; 4096];
                    let size = stream.read(&mut buffer).expect("read request");
                    let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    let response = handler(&request_line);
                    stream
                        .write_all(response.to_bytes().as_slice())
                        .expect("write response");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("server accept failed: {error}"),
            }
        });

        Self {
            addr,
            shutdown: Some(tx),
            handle: Some(handle),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join test server");
        }
    }
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: String,
}

impl HttpResponse {
    fn html(status: u16, reason: &'static str, body: &str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/html; charset=utf-8",
            body: body.to_string(),
        }
    }

    fn text(status: u16, reason: &'static str, body: &str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: body.to_string(),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                self.status,
                self.reason,
                self.content_type,
                self.body.len(),
                self.body
            )
            .into_bytes()
    }
}

#[test]
fn document_tools_write_and_read_back_within_the_workspace() {
    let _guard = env_guard();
    let root = temp_path("documents");
    fs::create_dir_all(&root).expect("create root");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let created = execute_tool(
            "write_xlsx",
            &json!({ "path": "out/book.xlsx", "rows": [["Item", "Qty"], ["nails", 12]], "bold_first_row": true }),
        )
        .expect("write_xlsx should succeed");
    let created: serde_json::Value = serde_json::from_str(&created).expect("json");
    assert_eq!(created["type"], "create");

    let read = execute_tool(
        "read_file",
        &json!({ "path": "out/book.xlsx", "range": "A1:B2" }),
    )
    .expect("read_file should render the workbook");
    let read: serde_json::Value = serde_json::from_str(&read).expect("json");
    assert_eq!(read["type"], "document");
    let content = read["file"]["content"].as_str().expect("content");
    assert!(content.contains("| 2 | nails | 12 |"), "{content}");

    execute_tool(
        "write_docx",
        &json!({ "path": "notes.docx", "markdown": "# Plan\n\n- ship it", "page_size": "Letter" }),
    )
    .expect("write_docx should succeed");
    let read = execute_tool("read_file", &json!({ "path": "notes.docx", "offset": 1 }))
        .expect("read docx");
    let read: serde_json::Value = serde_json::from_str(&read).expect("json");
    assert_eq!(read["file"]["content"], "- ship it");
    assert_eq!(read["file"]["totalLines"], 2);

    let escaped = execute_tool(
        "write_pptx",
        &json!({ "path": "deck.pptx", "slides": [{ "title": "x", "image": "/etc/hostname" }] }),
    )
    .expect_err("non-image files are rejected as slide images");
    assert!(escaped.contains("image"), "{escaped}");
    assert!(
        execute_tool("write_docx", &json!({ "path": "x.txt", "markdown": "" }))
            .expect_err("wrong extension")
            .contains(".docx")
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn file_tools_work_inside_added_workspace_roots() {
    let _guard = env_guard();
    let root = temp_path("addroot-cwd");
    let extra = temp_path("addroot-extra");
    fs::create_dir_all(&root).expect("create root");
    fs::create_dir_all(&extra).expect("create extra");
    fs::write(extra.join("data.txt"), "hello\n").expect("write");
    let original_dir = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("set cwd");

    let file = extra.join("data.txt").display().to_string();
    super::add_workspace_root(&extra).expect("add root");
    let read = execute_tool("read_file", &json!({ "path": file })).expect("read after /add-dir");
    assert!(read.contains("hello"), "{read}");
    execute_tool(
        "write_file",
        &json!({ "path": extra.join("new.txt").display().to_string(), "content": "x" }),
    )
    .expect("write after /add-dir");
    assert_eq!(
        fs::read_to_string(extra.join("new.txt")).expect("read"),
        "x"
    );

    std::env::set_current_dir(&original_dir).expect("restore cwd");
    fs::remove_dir_all(root).expect("cleanup");
    fs::remove_dir_all(extra).expect("cleanup");
}
