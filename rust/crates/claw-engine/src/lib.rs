//! claw on the [Infinity](https://github.com/hydro-project/infinity) runtime:
//! asynchronous, multi-threaded agents.
//!
//! - [`provider::ClawModelProvider`] serves claw's provider clients as an
//!   Infinity `ModelProvider`.
//! - [`tools::ClawTool`] serves claw's tool registry as Infinity tools, with
//!   permission checks; slow tools yield instead of blocking the thread.
//! - [`engine::Engine`] runs a prompt on a local agent system, child threads
//!   included, and streams [`engine::EngineEvent`]s.
//!
//! Design and phase plan: `docs/infinity-engine.md`.

// `api::ApiError` is large; boxing it at this boundary only adds noise (same as `api`).
#![allow(clippy::result_large_err)]

pub mod batch;
pub mod engine;
pub mod history;
pub mod provider;
pub mod tools;

pub use engine::{
    Engine, EngineConfig, EngineError, EngineEvent, RunSummary, TurnControl, TurnOutcome,
    ENGINE_PROMPT,
};
pub use infinity_agent_core::system::AgentEvent;
pub use provider::ClawModelProvider;
pub use tools::{ClawTool, Executor, SharedPrompter, ToolActivity, ToolHost};

/// Whether the user asked for the Infinity engine (`CLAW_ENGINE=infinity`).
#[must_use]
pub fn requested() -> bool {
    std::env::var("CLAW_ENGINE").is_ok_and(|value| value.eq_ignore_ascii_case("infinity"))
}
