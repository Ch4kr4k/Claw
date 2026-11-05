//! End-to-end runs of the engine against a scripted model: plain replies,
//! inline (fast) tools, dispatched (slow) tools, permission denials and
//! child threads reporting back to the root.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use claw_engine::{Engine, EngineConfig, EngineEvent, ToolActivity};
use futures_util::stream;
use infinity_provider_protocol::{
    CompletionError, CompletionModel, CompletionRequest, FinalResponse, Message, ModelEntry,
    ModelStream, SingleModelProvider, StreamChunk, ToolCall, ToolResultContent, UserContent,
};
use runtime::{PermissionMode, PermissionPolicy};
use serde_json::json;
use tools::GlobalToolRegistry;

type Script = dyn Fn(&CompletionRequest) -> Vec<StreamChunk> + Send + Sync;

struct ScriptedModel {
    script: Box<Script>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

#[async_trait]
impl CompletionModel for ScriptedModel {
    async fn stream(&self, request: CompletionRequest) -> Result<ModelStream, CompletionError> {
        let mut chunks = (self.script)(&request);
        chunks.push(StreamChunk::Final(FinalResponse::default()));
        self.requests.lock().expect("requests lock").push(request);
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }
}

/// The newest input of the request: user text, or the text of a tool result.
fn last_input(request: &CompletionRequest) -> String {
    let Some(Message::User { content }) = request.chat_history.last() else {
        return String::new();
    };
    content
        .iter()
        .map(|item| match item {
            UserContent::Text(text) => text.text.clone(),
            UserContent::ToolResult(result) => result
                .content
                .iter()
                .filter_map(|part| match part {
                    ToolResultContent::Text(text) => Some(text.text.as_str()),
                    ToolResultContent::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            UserContent::Image(_) => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// This thread's id, from the line Infinity adds to the preamble.
fn own_thread_id(request: &CompletionRequest) -> String {
    let preamble = request.preamble.as_deref().unwrap_or_default();
    let start = preamble
        .find("Your current thread ID is `")
        .expect("preamble names the thread")
        + "Your current thread ID is `".len();
    preamble[start..]
        .split('`')
        .next()
        .expect("thread id")
        .to_string()
}

fn text(reply: &str) -> Vec<StreamChunk> {
    vec![StreamChunk::Text(reply.to_string())]
}

fn call(name: &str, args: serde_json::Value) -> Vec<StreamChunk> {
    vec![StreamChunk::ToolCall(ToolCall::new(
        format!("call-{name}-{}", uuid_like()),
        name,
        args,
    ))]
}

fn uuid_like() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn policy(mode: PermissionMode, registry: &GlobalToolRegistry) -> PermissionPolicy {
    registry
        .permission_specs(None)
        .expect("permission specs")
        .into_iter()
        .fold(PermissionPolicy::new(mode), |policy, (name, required)| {
            policy.with_tool_requirement(name, required)
        })
}

struct Run {
    summary: claw_engine::RunSummary,
    events: Vec<EngineEvent>,
    requests: Vec<CompletionRequest>,
}

fn run(
    mode: PermissionMode,
    prompt: &str,
    script: impl Fn(&CompletionRequest) -> Vec<StreamChunk> + Send + Sync + 'static,
) -> Run {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        script: Box::new(script),
        requests: Arc::clone(&requests),
    };
    let entry = ModelEntry {
        model_id: "scripted".to_string(),
        display_name: "scripted".to_string(),
        context_window: 200_000,
        max_output_tokens: None,
        supports_image_input: false,
    };
    let registry = GlobalToolRegistry::builtin();
    let config = EngineConfig {
        provider: Arc::new(SingleModelProvider::new(entry, model)),
        model: "scripted".to_string(),
        system_prompt: "claw test prompt".to_string(),
        policy: policy(mode, &registry),
        registry,
        allowed_tools: None,
    };
    let mut events = Vec::new();
    let summary = Engine::new(config)
        .run_prompt_blocking(prompt, |event| events.push(event.clone()))
        .expect("engine run");
    let requests = requests.lock().expect("requests lock").clone();
    Run {
        summary,
        events,
        requests,
    }
}

#[test]
fn plain_reply_finishes_with_its_text() {
    let run = run(PermissionMode::ReadOnly, "hello", |_| text("hi there"));
    assert_eq!(run.summary.final_text, "hi there");
    assert_eq!(run.summary.threads, 1);
    assert_eq!(run.summary.completions, 1);
    let preamble = run.requests[0].preamble.as_deref().expect("preamble");
    assert!(preamble.contains("claw test prompt"));
    assert!(preamble.contains("# Engine and threads"));
    assert!(
        !preamble.contains("squash_sandbox"),
        "Infinity Code's base prompt is stripped: {preamble}"
    );
}

#[test]
fn inline_tool_result_loops_back_into_the_same_slice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "the secret is 42\n").expect("write note");
    let path_text = path.display().to_string();
    let run = run(PermissionMode::ReadOnly, "read it", move |request| {
        let input = last_input(request);
        if input == "read it" {
            call("read_file", json!({ "path": path_text }))
        } else if input.contains("the secret is 42") {
            text("it says 42")
        } else {
            text(&format!("unexpected: {input}"))
        }
    });
    assert_eq!(run.summary.final_text, "it says 42");
    assert!(
        !run.events
            .iter()
            .any(|event| matches!(event, EngineEvent::Tool(ToolActivity::Started { .. }))),
        "an inline tool is not dispatched"
    );
}

#[test]
fn slow_tool_is_dispatched_and_its_result_resumes_the_thread() {
    let run = run(PermissionMode::DangerFullAccess, "run it", |request| {
        let input = last_input(request);
        if input == "run it" {
            call("bash", json!({ "command": "echo engine-$((40+2))" }))
        } else if input.contains("engine-42") {
            text("bash said 42")
        } else {
            text(&format!("unexpected: {input}"))
        }
    });
    assert_eq!(run.summary.final_text, "bash said 42");
    assert!(run.events.iter().any(|event| matches!(
        event,
        EngineEvent::Tool(ToolActivity::Started { name, .. }) if name == "bash"
    )));
}

#[test]
fn denied_tool_returns_an_error_result_to_the_model() {
    let run = run(PermissionMode::ReadOnly, "write it", |request| {
        let input = last_input(request);
        if input == "write it" {
            call("bash", json!({ "command": "touch /tmp/should-not-exist" }))
        } else if input.starts_with("Error:") {
            text("denied")
        } else {
            text(&format!("unexpected: {input}"))
        }
    });
    assert_eq!(run.summary.final_text, "denied");
    assert!(run.events.iter().any(|event| matches!(
        event,
        EngineEvent::Tool(ToolActivity::Finished { name, is_error: true, .. }) if name == "bash"
    )));
}

#[test]
fn child_thread_reports_back_to_the_root() {
    let run = run(PermissionMode::ReadOnly, "fan out", |request| {
        let input = last_input(request);
        let me = own_thread_id(request);
        if input == "fan out" {
            call(
                "spawn_thread",
                json!({ "instructions": "say child-report", "child_of": [me] }),
            )
        } else if input.contains("You are now INSIDE") {
            call(
                "close_thread",
                json!({ "thread_id": me, "report_to_parent": "child-report" }),
            )
        } else if input.contains("child-report") {
            text("all done")
        } else {
            // The spawn result in the root, and anything else: wait.
            text("waiting")
        }
    });
    assert_eq!(run.summary.threads, 2, "events: {:#?}", run.events);
    assert_eq!(run.summary.final_text, "all done");
    let child_events = run
        .events
        .iter()
        .filter(|event| matches!(event, EngineEvent::Lifecycle { live: true, .. }))
        .count();
    assert!(child_events >= 2, "root and child both went live");
}

#[test]
fn several_calls_in_one_reply_all_run_and_all_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, "alpha-content\n").expect("write a");
    std::fs::write(&b, "beta-content\n").expect("write b");
    let (a, b) = (a.display().to_string(), b.display().to_string());
    let run = run(
        PermissionMode::DangerFullAccess,
        "read both",
        move |request| {
            let input = last_input(request);
            if input == "read both" {
                let mut chunks = call("read_file", json!({ "path": a }));
                chunks.extend(call("read_file", json!({ "path": b })));
                chunks.extend(call("bash", json!({ "command": "echo gamma-$((1+1))" })));
                chunks
            } else if input.contains("alpha-content")
                && input.contains("beta-content")
                && input.contains("gamma-2")
            {
                text("got all three")
            } else {
                text(&format!("unexpected: {input}"))
            }
        },
    );
    assert_eq!(run.summary.final_text, "got all three");
    let finished = run
        .events
        .iter()
        .filter(|event| matches!(event, EngineEvent::Tool(ToolActivity::Finished { .. })))
        .count();
    assert_eq!(finished, 3, "every call ran");
}
