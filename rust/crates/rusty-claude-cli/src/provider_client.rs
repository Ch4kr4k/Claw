//! The session's model client outside turns (turns run on the engine; see
//! `live_engine.rs`): it writes compaction summaries. Also builds the
//! provider client a session's turns use ([`turn_client`]).

#[allow(clippy::wildcard_imports)]
use crate::*;

/// The model client a session keeps for compaction summaries.
pub(crate) struct AnthropicRuntimeClient {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) client: ApiProviderClient,
    pub(crate) model: String,
    pub(crate) emit_output: bool,
}

impl AnthropicRuntimeClient {
    pub(crate) fn new(
        session_id: &str,
        model: String,
        emit_output: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let client = turn_client(&model, session_id)?;
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            model,
            emit_output,
        })
    }
}

impl ApiClient for AnthropicRuntimeClient {
    /// A compaction summary written by the model (hosted models only: a
    /// local model's small window is refilled quickly and slowly, and the
    /// summary built from the messages costs it nothing).
    fn summarize(&mut self, system: &str, transcript: &str) -> Option<String> {
        // Without an auxiliary model, a local main model keeps the free
        // summary built from the messages.
        let main_local = self.model.starts_with("local/");
        if main_local && runtime::read_user_setting("auxiliaryModel").is_none() {
            return None;
        }
        let request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model).min(16_000),
            messages: vec![InputMessage::user_text(transcript)],
            system: Some(system.to_string()),
            stream: false,
            ..Default::default()
        };
        if self.emit_output {
            render::start_thinking("summarizing the conversation");
        }
        let fallback = (!main_local).then_some((self.model.as_str(), &self.client));
        let text = side_task_text(&self.runtime, request, fallback);
        if self.emit_output {
            render::stop_thinking();
        }
        text.ok().filter(|text| !text.is_empty())
    }
}

/// The provider client for a session's turns on `model`: Anthropic with
/// claw's base URL and a session-scoped prompt cache, the OpenAI-compatible
/// backends from their env; with a fallback configured, a failing provider
/// is given up on quickly instead of retried for minutes.
pub(crate) fn turn_client(
    model: &str,
    session_id: &str,
) -> Result<ApiProviderClient, Box<dyn std::error::Error>> {
    let resolved_model = api::resolve_model_alias(model);
    let client = match detect_provider_kind(&resolved_model) {
        ProviderKind::Anthropic => {
            let auth = resolve_cli_auth_source()?;
            let inner = AnthropicClient::from_auth(auth)
                .with_base_url(api::read_base_url())
                .with_prompt_cache(PromptCache::new(session_id));
            ApiProviderClient::Anthropic(inner)
        }
        ProviderKind::Xai | ProviderKind::OpenAi => {
            // The api crate's `ProviderClient::from_model_with_anthropic_auth`
            // with `None` for the anthropic auth routes via
            // `detect_provider_kind` and builds an
            // `OpenAiCompatClient::from_env` with the matching
            // `OpenAiCompatConfig` (openai / xai / dashscope).
            // That reads the correct API-key env var and BASE_URL
            // override internally, so this one call covers OpenAI,
            // OpenRouter, xAI, DashScope, Ollama, and any other
            // OpenAI-compat endpoint users configure via
            // `OPENAI_BASE_URL` / `XAI_BASE_URL` / `DASHSCOPE_BASE_URL`.
            ApiProviderClient::from_model_with_anthropic_auth(&resolved_model, None)?
        }
    };
    // With a fallback model, a failing provider is not retried for
    // minutes (8 tries, up to 128s apart): two quick tries, then the
    // fallback takes over.
    let client = if fallback_models_for(&resolved_model).is_empty() {
        client
    } else {
        client.with_retry_policy(2, Duration::from_secs(1), Duration::from_secs(4))
    };
    Ok(client)
}
