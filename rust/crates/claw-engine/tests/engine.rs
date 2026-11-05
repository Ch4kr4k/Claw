//! End-to-end runs of the engine against a scripted model: plain replies,
//! inline (fast) tools, dispatched (slow) tools, permission denials and
//! child threads reporting back to the root.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use std::time::{Duration, Instant};

use claw_engine::{Engine, EngineConfig, EngineEvent, ToolActivity, TurnControl};
use futures_util::stream;
use infinity_provider_protocol::{
    CompletionError, CompletionModel, CompletionRequest, FinalResponse, Message, ModelEntry,
    ModelStream, SingleModelProvider, StreamChunk, ToolCall, ToolResultContent, UserContent,
};
use runtime::{
    ContentBlock, ConversationMessage, HookAbortSignal, HookRunner, MessageRole, PermissionMode,
    PermissionPolicy, PermissionPromptDecision, PermissionPrompter, PermissionRequest,
    RuntimeHookConfig,
};
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

type Requests = Arc<Mutex<Vec<CompletionRequest>>>;

fn config(
    mode: PermissionMode,
    script: impl Fn(&CompletionRequest) -> Vec<StreamChunk> + Send + Sync + 'static,
) -> (EngineConfig, Requests) {
    config_with_images(mode, false, script)
}

fn config_with_images(
    mode: PermissionMode,
    images: bool,
    script: impl Fn(&CompletionRequest) -> Vec<StreamChunk> + Send + Sync + 'static,
) -> (EngineConfig, Requests) {
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
        supports_image_input: images,
    };
    let registry = GlobalToolRegistry::builtin();
    let config = EngineConfig {
        provider: Arc::new(SingleModelProvider::new(entry, model)),
        model: "scripted".to_string(),
        system_prompt: "claw test prompt".to_string(),
        policy: policy(mode, &registry),
        registry,
        allowed_tools: None,
        hooks: None,
        prompter: None,
        executor: None,
        runtime_tools: false,
        tool_list: None,
        resolve_tool_name: None,
    };
    (config, requests)
}

fn run(
    mode: PermissionMode,
    prompt: &str,
    script: impl Fn(&CompletionRequest) -> Vec<StreamChunk> + Send + Sync + 'static,
) -> Run {
    let (config, requests) = config(mode, script);
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

fn history_text(request: &CompletionRequest) -> String {
    format!("{:?}", request.chat_history)
}

#[test]
fn turn_continues_the_session_and_returns_what_it_added() {
    let history = vec![
        ConversationMessage::user_text("remember the word pelican"),
        ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "noted".into(),
        }]),
    ];
    let (config, requests) = config(PermissionMode::ReadOnly, |request| {
        if history_text(request).contains("pelican") {
            text("the word was pelican")
        } else {
            text("no memory")
        }
    });
    let outcome = Engine::new(config)
        .run_turn_blocking(
            &history,
            "what was the word?",
            TurnControl::default(),
            |_| {},
        )
        .expect("turn");
    assert_eq!(outcome.summary.final_text, "the word was pelican");
    assert!(!outcome.interrupted);
    assert_eq!(outcome.messages.len(), 2, "{:#?}", outcome.messages);
    assert_eq!(outcome.messages[0].role, MessageRole::User);
    assert_eq!(outcome.messages[1].role, MessageRole::Assistant);
    assert!(
        outcome.messages[1].usage.is_some(),
        "usage lands on the reply"
    );
    let first = &requests.lock().expect("requests")[0];
    assert_eq!(first.chat_history.len(), 3, "seed plus the new prompt");
}

#[test]
fn abort_stops_a_running_tool_turn() {
    let (config, _) = config(PermissionMode::DangerFullAccess, |request| {
        if last_input(request) == "wait" {
            call("bash", json!({ "command": "sleep 3" }))
        } else {
            text("finished")
        }
    });
    let abort = HookAbortSignal::new();
    let trigger = abort.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        trigger.abort();
    });
    let started = Instant::now();
    let outcome = Engine::new(config)
        .run_turn_blocking(
            &[],
            "wait",
            TurnControl {
                abort: Some(abort),
                ..TurnControl::default()
            },
            |_| {},
        )
        .expect("turn");
    assert!(outcome.interrupted);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "returned after {:?}",
        started.elapsed()
    );
    assert!(outcome.messages.iter().any(|message| message
        .blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { name, .. } if name == "bash"))));
}

#[test]
fn prompts_typed_during_the_turn_reach_the_thread() {
    let (config, requests) = config(PermissionMode::DangerFullAccess, |request| {
        let input = last_input(request);
        if input == "start" {
            call("bash", json!({ "command": "sleep 1; echo slept" }))
        } else if input.contains("also check the tests") {
            text("will do")
        } else {
            text("ok")
        }
    });
    let mut sent = false;
    let outcome = Engine::new(config)
        .run_turn_blocking(
            &[],
            "start",
            TurnControl {
                queued_prompts: Some(Box::new(move || {
                    if sent {
                        Vec::new()
                    } else {
                        sent = true;
                        vec!["also check the tests".to_string()]
                    }
                })),
                ..TurnControl::default()
            },
            |_| {},
        )
        .expect("turn");
    assert!(requests
        .lock()
        .expect("requests")
        .iter()
        .any(|request| history_text(request).contains("also check the tests")));
    assert!(outcome
        .messages
        .iter()
        .any(|message| message.blocks.iter().any(
            |block| matches!(block, ContentBlock::Text { text } if text == "also check the tests")
        )));
}

struct Recorder {
    asked: Arc<Mutex<Vec<String>>>,
    allow: bool,
}

impl PermissionPrompter for Recorder {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
        self.asked
            .lock()
            .expect("asked")
            .push(request.tool_name.clone());
        if self.allow {
            PermissionPromptDecision::Allow
        } else {
            PermissionPromptDecision::Deny {
                reason: "user said no".into(),
            }
        }
    }
}

#[test]
fn calls_needing_approval_go_to_the_prompter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("out.txt");
    let target_text = target.display().to_string();
    for allow in [true, false] {
        let path = target_text.clone();
        let (mut config, _) = config(PermissionMode::WorkspaceWrite, move |request| {
            let input = last_input(request);
            if input == "write" {
                call("bash", json!({ "command": format!("touch {path}") }))
            } else if input.contains("user said no") {
                text("denied")
            } else {
                text("written")
            }
        });
        let asked = Arc::new(Mutex::new(Vec::new()));
        let prompter: Box<dyn PermissionPrompter + Send> = Box::new(Recorder {
            asked: Arc::clone(&asked),
            allow,
        });
        config.prompter = Some(Arc::new(Mutex::new(prompter)));
        let outcome = Engine::new(config)
            .run_turn_blocking(&[], "write", TurnControl::default(), |_| {})
            .expect("turn");
        assert_eq!(asked.lock().expect("asked").as_slice(), ["bash"]);
        assert_eq!(
            outcome.summary.final_text,
            if allow { "written" } else { "denied" }
        );
        assert_eq!(target.exists(), allow);
        let _ = std::fs::remove_file(&target);
    }
}

#[test]
fn pre_tool_hook_can_deny_a_call() {
    let (mut config, _) = config(PermissionMode::DangerFullAccess, |request| {
        let input = last_input(request);
        if input == "run" {
            call("bash", json!({ "command": "echo hi" }))
        } else if input.contains("blocked by policy hook") {
            text("hook said no")
        } else {
            text(&format!("unexpected: {input}"))
        }
    });
    config.hooks = Some(HookRunner::new(RuntimeHookConfig::new(
        vec!["echo 'blocked by policy hook'; exit 2".to_string()],
        Vec::new(),
        Vec::new(),
    )));
    let outcome = Engine::new(config)
        .run_turn_blocking(&[], "run", TurnControl::default(), |_| {})
        .expect("turn");
    assert_eq!(outcome.summary.final_text, "hook said no");
}

/// A 1×1 PNG.
const PIXEL: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
    0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[test]
fn tool_images_reach_a_model_that_reads_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = dir.path().join("shot.png");
    std::fs::write(&png, PIXEL).expect("write png");
    let png_text = png.display().to_string();
    let (mut config, requests) =
        config_with_images(PermissionMode::DangerFullAccess, true, |request| {
            if last_input(request) == "look" {
                call("bash", json!({ "command": "screenshot" }))
            } else {
                text("seen")
            }
        });
    // Like the X11 tool: a plain-text result naming the picture.
    config.executor = Some(Arc::new(move |_: &str, _: &serde_json::Value| {
        Ok(format!("took a screenshot\nclaw-image: {png_text}"))
    }));
    let outcome = Engine::new(config)
        .run_turn_blocking(&[], "look", TurnControl::default(), |_| {})
        .expect("turn");
    assert_eq!(outcome.summary.final_text, "seen");
    let requests = requests.lock().expect("requests");
    let with_image = requests.iter().any(|request| {
        request.chat_history.iter().any(|message| match message {
            Message::User { content } => content.iter().any(|item| match item {
                UserContent::ToolResult(result) => result
                    .content
                    .iter()
                    .any(|part| matches!(part, ToolResultContent::Image(_))),
                _ => false,
            }),
            Message::Assistant { .. } => false,
        })
    });
    assert!(with_image, "the screenshot was attached to the bash result");
}

#[test]
fn attachments_go_just_before_the_prompt_and_stay_out_of_the_outcome() {
    let (config, requests) = config(PermissionMode::ReadOnly, |request| {
        if history_text(request).contains("attached-note") {
            text("got the attachment")
        } else {
            text("nothing attached")
        }
    });
    let outcome = Engine::new(config)
        .run_turn_blocking(
            &[],
            "see attachment",
            TurnControl {
                attachments: vec![ContentBlock::Text {
                    text: "attached-note".into(),
                }],
                ..TurnControl::default()
            },
            |_| {},
        )
        .expect("turn");
    assert_eq!(outcome.summary.final_text, "got the attachment");
    let first = &requests.lock().expect("requests")[0];
    assert_eq!(first.chat_history.len(), 2, "attachment, then the prompt");
    assert_eq!(outcome.messages.len(), 2, "prompt and reply only");
}

#[test]
fn a_session_thread_id_keeps_the_system_prompt_identical_across_turns() {
    let mut preambles = Vec::new();
    for _ in 0..2 {
        let (config, requests) = config(PermissionMode::ReadOnly, |_| text("hi"));
        Engine::new(config)
            .run_turn_blocking(
                &[],
                "hello",
                TurnControl {
                    thread_id: Some("session-42".into()),
                    ..TurnControl::default()
                },
                |_| {},
            )
            .expect("turn");
        let request = requests.lock().expect("requests")[0].clone();
        preambles.push(request.preamble.expect("preamble"));
    }
    assert!(preambles[0].contains("session-42"));
    assert_eq!(preambles[0], preambles[1]);
}

#[test]
fn a_tool_list_limits_what_is_offered_but_not_what_runs() {
    let (mut config, requests) = config(PermissionMode::DangerFullAccess, |request| {
        if last_input(request) == "go" {
            call("bash", json!({ "command": "echo hidden-ran" }))
        } else if last_input(request).contains("hidden-ran") {
            text("ran")
        } else {
            text("no")
        }
    });
    config.tool_list = Some(Arc::new(|| {
        vec![infinity_provider_protocol::ToolDefinition {
            name: "read_file".into(),
            description: "read".into(),
            parameters: json!({ "type": "object" }),
        }]
    }));
    let outcome = Engine::new(config)
        .run_turn_blocking(&[], "go", TurnControl::default(), |_| {})
        .expect("turn");
    assert_eq!(outcome.summary.final_text, "ran");
    let names = requests.lock().expect("requests")[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "read_file",
            "spawn_thread",
            "report_to_parent",
            "close_thread",
            "send_message_to_child"
        ]
    );
}

#[test]
fn tool_names_from_other_agents_are_repaired_before_the_engine_checks_them() {
    let (mut config, _) = config(PermissionMode::DangerFullAccess, |request| {
        let input = last_input(request);
        if input == "go" {
            call("Bash", json!({ "command": "echo repaired" }))
        } else if input.contains("repaired") {
            text("ran")
        } else {
            text(&format!("unexpected: {input}"))
        }
    });
    config.resolve_tool_name = Some(Arc::new(|name: &str| {
        if name == "Bash" {
            "bash".to_string()
        } else {
            name.to_string()
        }
    }));
    let outcome = Engine::new(config)
        .run_turn_blocking(&[], "go", TurnControl::default(), |_| {})
        .expect("turn");
    assert_eq!(outcome.summary.final_text, "ran");
}
