//! `claw -p` on the Infinity engine (`CLAW_ENGINE=infinity`): the prompt runs
//! as a root thread of an asynchronous agent system, slow tools yield
//! instead of blocking, and the model can fan work out to child threads.
//! See `docs/infinity-engine.md`.
//!
//! Output: the root thread's reply streams to stdout; tool activity and the
//! child threads' replies go to stderr, so stdout stays the answer. With
//! `--output-format json` only a summary object is printed.

use std::collections::BTreeMap;
use std::env;
use std::io::{self, Write};

use claw_engine::{AgentEvent, Engine, EngineConfig, EngineEvent, ToolActivity};
use runtime::{ConfigLoader, PermissionMode};

use crate::{
    build_system_prompt, current_tool_registry, permission_policy, AllowedToolSet, CliOutputFormat,
};

/// The engine's built-in tools (threads, timers), shown when called.
const ENGINE_TOOLS: &[&str] = &[
    "spawn_thread",
    "report_to_parent",
    "close_thread",
    "sleep",
    "sleep_until",
    "cancel_subscription",
];

/// Longest tool argument / result preview printed on a tool line.
const PREVIEW_CHARS: usize = 80;

pub(crate) fn run(
    prompt: &str,
    model: &str,
    output_format: CliOutputFormat,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let feature_config = ConfigLoader::default_for(&cwd)
        .load()?
        .feature_config()
        .clone();
    let registry = current_tool_registry()?;
    let policy = permission_policy(permission_mode, &feature_config, &registry)?;
    let system_prompt = build_system_prompt(model)?.join("\n\n");
    let mut config = EngineConfig::for_model(model, system_prompt, registry, policy)?;
    config.allowed_tools = allowed_tools;

    let json = matches!(output_format, CliOutputFormat::Json);
    let mut printer = Printer::default();
    let summary = Engine::new(config).run_prompt_blocking(prompt, |event| {
        if !json {
            printer.print(event);
        }
    })?;
    if json {
        let value = serde_json::json!({
            "engine": "infinity",
            "model": model,
            "message": summary.final_text,
            "threads": summary.threads,
            "completions": summary.completions,
            "usage": {
                "input_tokens": summary.usage.input_tokens,
                "output_tokens": summary.usage.output_tokens,
                "cached_input_tokens": summary.usage.cached_input_tokens,
            },
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        printer.finish_line();
        eprintln!(
            "\x1b[2m[infinity engine: {} thread(s), {} completion(s), {} in / {} out tokens]\x1b[0m",
            summary.threads,
            summary.completions,
            summary.usage.input_tokens,
            summary.usage.output_tokens
        );
    }
    Ok(())
}

/// Prints engine events as they arrive.
#[derive(Default)]
struct Printer {
    /// Short labels of child threads (`#1`, `#2`, …) by thread id.
    labels: BTreeMap<String, String>,
    /// Child replies, printed whole when their completion finishes.
    child_text: BTreeMap<String, String>,
    /// Whether stdout's last byte was not a newline.
    mid_line: bool,
}

impl Printer {
    fn label(&mut self, thread: &str) -> String {
        let next = self.labels.len() + 1;
        self.labels
            .entry(thread.to_string())
            .or_insert_with(|| format!("#{next}"))
            .clone()
    }

    fn finish_line(&mut self) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
    }

    fn print(&mut self, event: &EngineEvent) {
        match event {
            EngineEvent::Agent {
                thread,
                root,
                event,
            } => self.print_agent(thread, *root, event),
            EngineEvent::Tool(ToolActivity::Started { .. }) | EngineEvent::Lifecycle { .. } => {}
            EngineEvent::Tool(ToolActivity::Finished {
                thread,
                name,
                input,
                output,
                is_error,
            }) => {
                let who = self.who(thread);
                let mark = if *is_error {
                    "\x1b[31m✗"
                } else {
                    "\x1b[32m✓"
                };
                self.finish_line();
                eprintln!(
                    "\x1b[2m{who}\x1b[0m{mark}\x1b[0m {name} \x1b[2m{} → {}\x1b[0m",
                    preview(input),
                    preview(output)
                );
            }
        }
    }

    /// `[#n] ` for a child thread, nothing for the root.
    fn who(&mut self, thread: &str) -> String {
        if self.labels.contains_key(thread) {
            format!("[{}] ", self.label(thread))
        } else {
            String::new()
        }
    }

    fn print_agent(&mut self, thread: &str, root: bool, event: &AgentEvent) {
        if !root {
            self.label(thread);
        }
        match event {
            AgentEvent::TextChunk { text } if root => {
                print!("{text}");
                // A failed flush only delays output; the text is not lost.
                let _ = io::stdout().flush();
                self.mid_line = !text.ends_with('\n');
            }
            AgentEvent::TextChunk { text } => {
                self.child_text
                    .entry(thread.to_string())
                    .or_default()
                    .push_str(text);
            }
            // claw's own tools are shown when they finish (ToolActivity);
            // the engine's thread tools have no activity, so show the call.
            AgentEvent::ToolCall { name, args, .. } if ENGINE_TOOLS.contains(&name.as_str()) => {
                let who = self.who(thread);
                self.finish_line();
                let args = args.to_string();
                eprintln!(
                    "\x1b[2m{who}\x1b[0m\x1b[36m⏺\x1b[0m {name} \x1b[2m{}\x1b[0m",
                    preview(&args)
                );
            }
            AgentEvent::CompletionFinished { .. } if !root => {
                if let Some(text) = self.child_text.remove(thread) {
                    let text = text.trim();
                    if !text.is_empty() {
                        let who = self.who(thread);
                        self.finish_line();
                        eprintln!("\x1b[2m{who}{text}\x1b[0m");
                    }
                }
            }
            AgentEvent::SubscriptionEvent { name, text } => {
                let who = self.who(thread);
                self.finish_line();
                eprintln!(
                    "\x1b[2m{who}\x1b[0m\x1b[35m↩\x1b[0m {name} \x1b[2m{}\x1b[0m",
                    preview(text)
                );
            }
            AgentEvent::Info { text } => {
                let who = self.who(thread);
                self.finish_line();
                eprintln!("\x1b[2m{who}{text}\x1b[0m");
            }
            _ => {}
        }
    }
}

/// The first line of `text`, cut to [`PREVIEW_CHARS`].
fn preview(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut cut = line.chars().take(PREVIEW_CHARS).collect::<String>();
    if line.chars().count() > PREVIEW_CHARS || text.lines().nth(1).is_some() {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::preview;

    #[test]
    fn preview_keeps_first_line_and_marks_cuts() {
        assert_eq!(preview("one"), "one");
        assert_eq!(preview("one\ntwo"), "one…");
        assert_eq!(preview(&"x".repeat(200)).chars().count(), 81);
    }
}
