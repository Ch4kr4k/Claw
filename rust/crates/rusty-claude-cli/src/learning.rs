//! Runs the learning review ([`runtime::learning`]) in the background after
//! a turn that taught something, and reports what was saved.
//!
//! The review is the conversation plus one instruction, sent to the
//! auxiliary model when one is set, else to the main model with the turn's
//! own request settings (tools included), so a local server reuses the
//! prompt it already processed and only reads the instruction. A new
//! prompt cancels a review still running: on a one-slot local server it
//! would otherwise make the user's next turn wait.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use api::{InputContentBlock, InputMessage, MessageRequest, OutputContentBlock};
use runtime::learning::{self, TurnSignals};
use runtime::ConversationMessage;

static RUNNING: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);
/// Turns since the last review; a review needs at least two between, so
/// a long task is not reviewed after every step of the conversation.
static TURNS_SINCE: AtomicUsize = AtomicUsize::new(usize::MAX / 2);
const MIN_TURNS_BETWEEN: usize = 2;
const REVIEW_TIMEOUT: Duration = Duration::from_secs(600);

/// Stops a review that is still running (a new prompt was sent).
pub(crate) fn cancel() {
    if RUNNING.load(Ordering::SeqCst) {
        CANCEL.store(true, Ordering::SeqCst);
    }
}

/// After a turn made of `turn` (its messages): starts a review in the
/// background when the turn is worth one. `conversation` is the whole
/// session, which the review reads.
pub(crate) fn after_turn(conversation: &[ConversationMessage], turn: &[ConversationMessage]) {
    let turns = TURNS_SINCE.fetch_add(1, Ordering::SeqCst) + 1;
    if !learning::learning_enabled() {
        let _ = learning::take_skills_used();
        return;
    }
    let signals = TurnSignals::from_messages(turn, learning::take_skills_used());
    let due =
        learning::should_review(&signals) && (turns >= MIN_TURNS_BETWEEN || signals.correction);
    if due {
        start(conversation.to_vec(), signals, false);
    }
}

/// `/learn now`: reviews the conversation whatever the turn did.
pub(crate) fn review_now(conversation: &[ConversationMessage]) -> String {
    if RUNNING.load(Ordering::SeqCst) {
        return "A learning review is already running.".to_string();
    }
    let signals = TurnSignals::from_messages(conversation, learning::take_skills_used());
    if start(conversation.to_vec(), signals, true) {
        "Reviewing the conversation in the background; what is saved is shown when it is done."
            .to_string()
    } else {
        "Nothing has been sent to the model yet, so there is nothing to review.".to_string()
    }
}

/// Starts the review thread; false when there is no request to build on.
fn start(conversation: Vec<ConversationMessage>, signals: TurnSignals, asked: bool) -> bool {
    let Some((client, mut request)) = crate::SIDE_CONTEXT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    else {
        return false;
    };
    if RUNNING.swap(true, Ordering::SeqCst) {
        return false;
    }
    CANCEL.store(false, Ordering::SeqCst);
    TURNS_SINCE.store(0, Ordering::SeqCst);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    std::thread::spawn(move || {
        let prompt = learning::review_prompt(&cwd, &signals);
        let mut messages = crate::convert_messages(&conversation);
        let instruction = InputContentBlock::Text { text: prompt };
        match messages.last_mut() {
            Some(last) if last.role == "user" => last.content.push(instruction),
            _ => messages.push(InputMessage {
                role: "user".to_string(),
                content: vec![instruction],
            }),
        }
        request.messages = messages;
        request.stream = false;
        request.max_tokens = request.max_tokens.min(8_192);
        let outcome = run_review(&client, request);
        RUNNING.store(false, Ordering::SeqCst);
        let why = if asked {
            "asked".to_string()
        } else {
            signals_summary(&signals)
        };
        let notice = match outcome {
            Ok(Some(reply)) => {
                let (notice, logged) = report(&cwd, &reply, asked);
                learning::record_review(&format!("({why}) {logged}"));
                notice
            }
            Ok(None) => {
                learning::record_review(&format!("({why}) cancelled: a new prompt came first"));
                None
            }
            Err(error) => {
                learning::record_review(&format!("({why}) failed: {error}"));
                asked.then(|| format!("learning review failed: {error}"))
            }
        };
        if let Some(notice) = notice {
            learning::push_notice(notice);
            if runtime::turn_keys::screen::active() {
                print_notices();
            }
        }
    });
    true
}

/// The review model's reply; `None` when cancelled.
fn run_review(
    main_client: &api::ProviderClient,
    request: MessageRequest,
) -> Result<Option<String>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let send = |client: &api::ProviderClient, request: MessageRequest| {
        runtime.block_on(async {
            tokio::select! {
                response = tokio::time::timeout(REVIEW_TIMEOUT, client.send_message(&request)) => {
                    let response = response
                        .map_err(|_| "timed out".to_string())?
                        .map_err(|error| error.to_string())?;
                    Ok(Some(response.content.into_iter().filter_map(|block| match block {
                        OutputContentBlock::Text { text } => Some(text),
                        _ => None,
                    }).collect::<String>()))
                }
                () = cancelled() => Ok(None),
            }
        })
    };
    if let Some((model, client)) = crate::auxiliary_model() {
        let mut aux = request.clone();
        aux.model.clone_from(&model);
        aux.max_tokens = aux.max_tokens.min(crate::max_tokens_for_model(&model));
        aux.tools = None;
        aux.tool_choice = None;
        if let Ok(reply) = send(&client, aux) {
            return Ok(reply);
        }
    }
    send(main_client, request)
}

async fn cancelled() {
    while !CANCEL.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Why a turn was reviewed, for the review log.
fn signals_summary(signals: &TurnSignals) -> String {
    let mut parts = vec![format!("{} tool calls", signals.tool_calls)];
    if signals.correction {
        parts.push("correction".to_string());
    }
    if signals.recovered {
        parts.push("recovered from a failure".to_string());
    }
    if !signals.skills_used.is_empty() {
        parts.push(format!("used {}", signals.skills_used.join(", ")));
    }
    parts.join(", ")
}

/// Applies the review in `reply`: the line to show the user, if any, and
/// the line for the review log.
fn report(cwd: &std::path::Path, reply: &str, asked: bool) -> (Option<String>, String) {
    let reply = crate::without_tool_call_markup(reply);
    let review = match learning::parse_review(&reply) {
        Ok(review) => review,
        Err(error) => {
            return (
                asked.then(|| format!("learning review: {error}")),
                format!("unreadable reply: {error}"),
            )
        }
    };
    let applied = learning::apply_review(cwd, &review);
    let saved = applied
        .iter()
        .filter(|change| change.ok)
        .map(|change| change.summary.clone())
        .collect::<Vec<_>>();
    let refused = applied
        .iter()
        .filter(|change| !change.ok && !change.summary.ends_with(": no change"))
        .map(|change| change.summary.clone())
        .collect::<Vec<_>>();
    let mut parts = Vec::new();
    if !saved.is_empty() {
        parts.push(format!("learned: {}", saved.join(", ")));
    }
    if !refused.is_empty() {
        parts.push(refused.join("; "));
    }
    if parts.is_empty() {
        return (
            asked.then(|| "learning review: nothing worth saving".to_string()),
            "nothing worth saving".to_string(),
        );
    }
    let logged = parts.join(" · ");
    if !saved.is_empty() {
        parts.push("/learn to see, /learn undo to revert".to_string());
    }
    (Some(parts.join(" · ")), logged)
}

/// Prints what the learning loop saved since the last call.
pub(crate) fn print_notices() {
    for notice in learning::take_notices() {
        println!("\x1b[2m✎ {notice}\x1b[0m");
    }
}

/// `/learn [now|undo|pin <skill>|unpin <skill>|on|off]`.
pub(crate) fn run_command(args: Option<&str>, conversation: &[ConversationMessage]) -> String {
    let mut words = args.unwrap_or_default().split_whitespace();
    match (words.next(), words.next()) {
        (None | Some("status" | "list"), _) => learning::status_report(),
        (Some("now"), _) => review_now(conversation),
        (Some("undo"), _) => learning::undo_last().unwrap_or_else(|error| error),
        (Some("pin"), Some(name)) => learning::set_pinned(name, true).unwrap_or_else(|error| error),
        (Some("unpin"), Some(name)) => {
            learning::set_pinned(name, false).unwrap_or_else(|error| error)
        }
        (Some(state @ ("on" | "off")), _) => {
            let on = state == "on";
            match runtime::save_user_setting_value("learningLoop", serde_json::Value::Bool(on)) {
                Ok(()) => format!(
                    "Learning loop {}.",
                    if on {
                        "on"
                    } else {
                        "off: turns are no longer reviewed"
                    }
                ),
                Err(error) => format!("could not save the setting: {error}"),
            }
        }
        _ => "Usage: /learn [now|undo|pin <skill>|unpin <skill>|on|off]".to_string(),
    }
}
