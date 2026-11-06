//! `Agent` sub-agents on the engine.
//!
//! The `tools` crate owns sub-agents (manifests, background jobs,
//! `SendMessage` resumes) and runs each turn through a
//! [`tools::SubagentRunner`] when one is installed. [`runner`] is that
//! runner: each turn runs on its own engine, so a sub-agent gets the same
//! asynchronous tools, parallel batches and child threads as the main
//! conversation, limited to the agent's tools.

use std::sync::Arc;

use runtime::Session;
use tools::{GlobalToolRegistry, SubagentRunner, SubagentTurn};

use crate::engine::{Engine, EngineConfig, TurnControl};
use crate::provider::ClawModelProvider;

/// Builds the provider for a sub-agent's model; the second argument is the
/// agent's id (its session, for a prompt cache).
pub type ProviderFactory =
    Arc<dyn Fn(&str, &str) -> Result<ClawModelProvider, String> + Send + Sync>;

/// The runner the CLI installs with [`tools::set_subagent_runner`]: the
/// agent's model through `provider` (claw's client, thinking budget and
/// fallback chain), the built-in tools, and the sub-agent permission policy
/// (no one to ask, so calls needing approval are denied).
#[must_use]
pub fn runner(provider: ProviderFactory) -> SubagentRunner {
    Arc::new(move |turn: SubagentTurn| {
        let registry = GlobalToolRegistry::builtin();
        let provider = match provider(&turn.model, &turn.agent_id) {
            Ok(provider) => provider,
            Err(error) => return (Err(error), turn.session),
        };
        let config = EngineConfig::for_provider(
            provider,
            turn.system_prompt.join("\n\n"),
            registry,
            tools::subagent_permission_policy(),
        );
        run_turn(config, turn)
    })
}

/// One sub-agent turn on `config`: only the agent's tools are offered and
/// run, and the turn stops after `max_iterations` model replies.
pub fn run_turn(mut config: EngineConfig, turn: SubagentTurn) -> (Result<String, String>, Session) {
    let SubagentTurn {
        agent_id,
        allowed_tools,
        max_iterations,
        mut session,
        prompt,
        ..
    } = turn;
    config.allowed_tools = Some(allowed_tools.clone());
    if config.executor.is_none() {
        let allowed = allowed_tools;
        config.executor = Some(Arc::new(move |name: &str, input: &serde_json::Value| {
            tools::run_subagent_tool(&allowed, name, input)
        }));
    }
    config.max_completions = Some(max_iterations);
    let control = TurnControl {
        // A resumed agent keeps its system prompt, and a prompt cache.
        thread_id: Some(agent_id),
        ..TurnControl::default()
    };
    let result = Engine::new(config).run_turn_blocking(&session.messages, &prompt, control, |_| {});
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => return (Err(error.to_string()), session),
    };
    for message in outcome.messages {
        if let Err(error) = session.push_message(message) {
            return (Err(error.to_string()), session);
        }
    }
    if outcome.interrupted {
        return (
            Err(format!(
                "sub-agent stopped after {max_iterations} model replies without finishing"
            )),
            session,
        );
    }
    (Ok(outcome.summary.final_text), session)
}
