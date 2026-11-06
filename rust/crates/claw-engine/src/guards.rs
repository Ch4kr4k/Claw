//! [`TurnGuards`]: the checks claw's classic loop made on what the model
//! does, for a turn on the engine. One value is shared by the turn's tool
//! host, its provider wrapper and its control loop; state is kept per
//! thread, so a child thread's calls don't count against the root's.
//!
//! - **Before a call** ([`TurnGuards::before_call`]): a call that keeps
//!   returning the same result is no longer run, nor one that keeps failing
//!   the same way; a broad filesystem search gets one warning instead.
//! - **After a call** ([`TurnGuards::after_call`]): notes after repeated
//!   failures, warnings about instructions injected into tool output, a
//!   newly entered directory's instruction files, a short note in place of
//!   a read the conversation already holds, and (on the root) background
//!   jobs that finished meanwhile.
//! - **After a reply** ([`TurnGuards::after_reply`]): a reply cut off at
//!   the output limit or mid-thought is continued (a few times in a row at
//!   most), and a turn that edited code and is about to end without having
//!   run anything is reminded to check it. These come back as follow-up
//!   prompts the engine sends once the reply is finished.
//! - **Stuck**: a model that keeps sending a call already blocked as a
//!   repeat ends the turn ([`TurnGuards::stuck`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use runtime::context_hints::ContextHints;
use runtime::turn_guards::{
    self, CallVerdict, FailureGuard, RepeatGuard, VerifyGuard, CUT_OFF_CONTINUE_PROMPT,
    CUT_OFF_TOOL_CALL_RESULT, MAX_CUT_OFF_CONTINUATIONS, UNCHANGED_READ_MIN_CHARS,
    UNCHANGED_READ_NOTE,
};
use runtime::{ContentBlock, ConversationMessage};

/// What the guards do for a turn.
#[derive(Clone, Default)]
pub struct GuardConfig {
    /// Remind a turn that edited code to check it before it ends.
    pub verify_on_stop: bool,
    /// Load a directory's instruction files (`AGENTS.md`, …) as the model
    /// first works in it; kept by the caller across turns.
    pub context_hints: Option<Arc<Mutex<ContextHints>>>,
    /// Report background jobs that finished to the root thread, appended to
    /// its next tool result.
    pub job_notices: bool,
}

/// How one model reply ended, as the provider wrapper saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplyReport {
    /// Tool calls the reply makes (after any cut-off call was dropped).
    pub tool_calls: usize,
    /// The reply stopped at the output-token limit.
    pub cut_off: bool,
    /// The end of the reply's thinking when it has no text and no call.
    pub thinking_tail: Option<String>,
    /// The reply's text is a repetition loop.
    pub looped: bool,
    /// The reply's last call was cut off mid-input and was not run.
    pub dropped_cut_off_call: bool,
}

#[derive(Default)]
struct ThreadGuards {
    repeat: RepeatGuard,
    failures: FailureGuard,
    verify: VerifyGuard,
    broad_searches_warned: BTreeSet<String>,
    /// Cut-off replies in a row.
    cut_offs: usize,
    followups: Vec<String>,
}

/// See the module docs.
#[derive(Default)]
pub struct TurnGuards {
    config: GuardConfig,
    root: Mutex<Option<String>>,
    threads: Mutex<HashMap<String, ThreadGuards>>,
    /// Hashes of long tool outputs the conversation already holds.
    seen_outputs: Mutex<HashSet<u64>>,
}

impl TurnGuards {
    #[must_use]
    pub fn new(config: GuardConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// The root thread's id, and the conversation it starts from (whose
    /// reads count as already seen).
    pub fn begin(&self, root: &str, history: &[ConversationMessage]) {
        *lock(&self.root) = Some(root.to_string());
        let mut seen = lock(&self.seen_outputs);
        for message in history {
            for block in &message.blocks {
                if let ContentBlock::ToolResult {
                    output,
                    is_error: false,
                    ..
                } = block
                {
                    if output.len() >= UNCHANGED_READ_MIN_CHARS {
                        seen.insert(hash(output));
                    }
                }
            }
        }
    }

    fn is_root(&self, thread: &str) -> bool {
        lock(&self.root).as_deref() == Some(thread)
    }

    fn with_thread<R>(&self, thread: &str, f: impl FnOnce(&mut ThreadGuards) -> R) -> R {
        let mut threads = lock(&self.threads);
        f(threads.entry(thread.to_string()).or_default())
    }

    /// Why a call the permission check allowed is not run, if it is not.
    #[must_use]
    pub fn before_call(&self, thread: &str, tool: &str, input: &str) -> Option<String> {
        self.with_thread(thread, |guards| {
            if let Some(reason) = guards.repeat.check(tool, input) {
                return Some(reason);
            }
            if let CallVerdict::Block(reason) = guards.failures.check(tool, input) {
                return Some(reason);
            }
            if tool.eq_ignore_ascii_case("bash") {
                let target = serde_json::from_str::<serde_json::Value>(input)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .and_then(turn_guards::broad_search_target)
                    });
                if let Some(target) = target {
                    if guards.broad_searches_warned.insert(input.to_string()) {
                        return Some(turn_guards::broad_search_note(&target));
                    }
                }
            }
            None
        })
    }

    /// Adds the guards' notes to the result of a call that ran.
    pub fn after_call(
        &self,
        thread: &str,
        tool: &str,
        input: &str,
        output: &mut String,
        is_error: bool,
    ) {
        let mut notes = Vec::new();
        self.with_thread(thread, |guards| {
            guards.repeat.record(tool, input, output, is_error);
            if let Some(note) = guards.failures.record(tool, input, is_error) {
                notes.push(note);
            }
            if !is_error {
                guards.verify.record(tool, input);
            }
        });
        if !is_error {
            if matches!(tool, "read_file" | "Read") && output.len() >= UNCHANGED_READ_MIN_CHARS {
                if !lock(&self.seen_outputs).insert(hash(output)) {
                    *output = UNCHANGED_READ_NOTE.to_string();
                }
            } else if output.len() >= UNCHANGED_READ_MIN_CHARS {
                lock(&self.seen_outputs).insert(hash(output));
            }
            if let Some(warning) = runtime::threats::tool_result_warning(tool, output) {
                notes.push(warning);
            }
            if let Some(hints) = &self.config.context_hints {
                if let Some(hint) = lock(hints).for_call(tool, input) {
                    notes.push(hint);
                }
            }
        }
        // Background shells or agents that finished while the tools ran
        // reach the model with this result, before its next step.
        if self.config.job_notices && self.is_root(thread) {
            let finished = runtime::global_background_jobs().drain_notices();
            if !finished.is_empty() {
                notes.push(format!(
                    "<system-reminder>\n{}\n</system-reminder>",
                    finished.join("\n")
                ));
            }
        }
        for note in notes {
            output.push_str("\n\n");
            output.push_str(&note);
        }
    }

    /// Records how a reply ended; what the thread should be sent next is
    /// kept for [`Self::take_followups`].
    pub fn after_reply(&self, thread: &str, report: &ReplyReport) {
        let root = self.is_root(thread);
        let verify_on_stop = self.config.verify_on_stop;
        self.with_thread(thread, |guards| {
            if report.dropped_cut_off_call && report.tool_calls == 0 {
                guards.followups.push(CUT_OFF_TOOL_CALL_RESULT.to_string());
                return;
            }
            if report.tool_calls > 0 {
                // The limit is on cut-offs in a row: a long turn that keeps
                // making progress between them is not ended without an answer.
                guards.cut_offs = 0;
                return;
            }
            if report.looped {
                // Continuing a loop only feeds it.
                return;
            }
            let cut_off = report.cut_off || report.thinking_tail.is_some();
            if cut_off {
                if guards.cut_offs < MAX_CUT_OFF_CONTINUATIONS {
                    guards.cut_offs += 1;
                    guards
                        .followups
                        .push(report.thinking_tail.as_deref().map_or_else(
                            || CUT_OFF_CONTINUE_PROMPT.to_string(),
                            turn_guards::thinking_cut_off_prompt,
                        ));
                }
                return;
            }
            if root && verify_on_stop {
                if let Some(reminder) = guards.verify.reminder() {
                    guards.followups.push(reminder);
                }
            }
        });
    }

    /// Prompts to send `thread` now that its reply is finished.
    #[must_use]
    pub fn take_followups(&self, thread: &str) -> Vec<String> {
        lock(&self.threads)
            .get_mut(thread)
            .map(|guards| std::mem::take(&mut guards.followups))
            .unwrap_or_default()
    }

    /// The tool a thread keeps calling although every call is blocked: the
    /// turn should end.
    #[must_use]
    pub fn stuck(&self) -> Option<String> {
        lock(&self.threads)
            .values()
            .find_map(|guards| guards.repeat.stuck().map(ToOwned::to_owned))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_off_reply_is_continued_a_few_times_in_a_row() {
        let guards = TurnGuards::default();
        guards.begin("root", &[]);
        let cut = ReplyReport {
            cut_off: true,
            ..ReplyReport::default()
        };
        for _ in 0..MAX_CUT_OFF_CONTINUATIONS {
            guards.after_reply("root", &cut);
            assert_eq!(guards.take_followups("root"), [CUT_OFF_CONTINUE_PROMPT]);
        }
        guards.after_reply("root", &cut);
        assert!(guards.take_followups("root").is_empty());
        // Progress (a tool call) resets the count.
        guards.after_reply(
            "root",
            &ReplyReport {
                tool_calls: 1,
                ..ReplyReport::default()
            },
        );
        guards.after_reply("root", &cut);
        assert_eq!(guards.take_followups("root").len(), 1);
    }

    #[test]
    fn a_reply_with_only_thinking_quotes_its_tail() {
        let guards = TurnGuards::default();
        guards.after_reply(
            "t",
            &ReplyReport {
                thinking_tail: Some("so I will edit main.rs".into()),
                ..ReplyReport::default()
            },
        );
        let followups = guards.take_followups("t");
        assert!(turn_guards::follows_thinking_cut_off(&followups[0]));
        assert!(followups[0].contains("edit main.rs"));
    }

    #[test]
    fn a_looped_reply_is_not_continued() {
        let guards = TurnGuards::default();
        guards.after_reply(
            "t",
            &ReplyReport {
                cut_off: true,
                looped: true,
                ..ReplyReport::default()
            },
        );
        assert!(guards.take_followups("t").is_empty());
    }

    #[test]
    fn edited_code_gets_one_reminder_before_the_root_finishes() {
        let guards = TurnGuards::new(GuardConfig {
            verify_on_stop: true,
            ..GuardConfig::default()
        });
        guards.begin("root", &[]);
        let mut output = "ok".to_string();
        guards.after_call(
            "root",
            "edit_file",
            r#"{"path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
            &mut output,
            false,
        );
        guards.after_reply("root", &ReplyReport::default());
        assert_eq!(guards.take_followups("root").len(), 1);
        guards.after_reply("root", &ReplyReport::default());
        assert!(guards.take_followups("root").is_empty());
    }

    #[test]
    fn repeated_calls_are_blocked_then_the_turn_is_stuck() {
        let guards = TurnGuards::default();
        let input = r#"{"bash_id":"1"}"#;
        for _ in 0..turn_guards::REPEATED_CALL_LIMIT {
            assert_eq!(guards.before_call("t", "BashOutput", input), None);
            let mut output = "nothing".to_string();
            guards.after_call("t", "BashOutput", input, &mut output, false);
        }
        assert!(guards.before_call("t", "BashOutput", input).is_some());
        assert!(guards.stuck().is_none());
        for _ in 0..turn_guards::BLOCKED_REPEAT_STEPS_LIMIT {
            assert!(guards.before_call("t", "BashOutput", input).is_some());
        }
        assert_eq!(guards.stuck().as_deref(), Some("BashOutput"));
        // Another thread's calls are its own.
        assert_eq!(guards.before_call("other", "BashOutput", input), None);
    }

    #[test]
    fn a_read_already_in_the_conversation_gets_a_short_note() {
        let guards = TurnGuards::default();
        let content = "line\n".repeat(200);
        guards.begin(
            "root",
            &[ConversationMessage::tool_result(
                "1",
                "read_file",
                content.clone(),
                false,
            )],
        );
        let mut output = content.clone();
        guards.after_call("root", "read_file", "{}", &mut output, false);
        assert_eq!(output, UNCHANGED_READ_NOTE);
        // A new read is kept, and noted for next time.
        let other = "other\n".repeat(200);
        let mut output = other.clone();
        guards.after_call("root", "read_file", "{}", &mut output, false);
        assert_eq!(output, other);
        let mut again = other;
        guards.after_call("root", "read_file", "{}", &mut again, false);
        assert_eq!(again, UNCHANGED_READ_NOTE);
    }

    #[test]
    fn a_broad_search_is_warned_about_once() {
        let guards = TurnGuards::default();
        let input = r#"{"command":"find / -name foo"}"#;
        assert!(guards.before_call("t", "bash", input).is_some());
        assert_eq!(guards.before_call("t", "bash", input), None);
    }
}
