//! The model client of a turn: builds each request (messages, tools as sent,
//! local-model controls), streams the provider's events into assistant events
//! with live rendering, and handles fallback models and retries.

#[allow(clippy::wildcard_imports)]
use crate::*;

// NOTE: Despite the historical name `AnthropicRuntimeClient`, this struct
// now holds an `ApiProviderClient` which dispatches to Anthropic, xAI,
// OpenAI, or DashScope at construction time based on
// `detect_provider_kind(&model)`. The struct name is kept to avoid
// churning `BuiltRuntime` and every Deref/DerefMut site that references
// it. See ROADMAP #29 for the provider-dispatch routing fix.
pub(crate) struct AnthropicRuntimeClient {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) client: ApiProviderClient,
    pub(crate) session_id: String,
    pub(crate) model: String,
    pub(crate) enable_tools: bool,
    pub(crate) emit_output: bool,
    pub(crate) allowed_tools: Option<AllowedToolSet>,
    pub(crate) tool_registry: GlobalToolRegistry,
    pub(crate) progress_reporter: Option<InternalPromptProgressReporter>,
    pub(crate) reasoning_effort: Option<String>,
    /// Whether any assistant text was streamed to stdout, so the REPL does
    /// not print the reply a second time.
    pub(crate) streamed_text: std::sync::atomic::AtomicBool,
    /// Set by Esc during a REPL turn; cancels the in-flight model request.
    pub(crate) abort_signal: Option<runtime::HookAbortSignal>,
}

impl AnthropicRuntimeClient {
    pub(crate) fn new(
        session_id: &str,
        model: String,
        enable_tools: bool,
        emit_output: bool,
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        progress_reporter: Option<InternalPromptProgressReporter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Dispatch to the correct provider at construction time.
        // `ApiProviderClient` (exposed by the api crate as
        // `ProviderClient`) is an enum over Anthropic / xAI / OpenAI
        // variants, where xAI and OpenAI both use the OpenAI-compat
        // wire format under the hood. We consult
        // `detect_provider_kind(&resolved_model)` so model-name prefix
        // routing (`openai/`, `gpt-`, `grok`, `qwen/`) wins over
        // env-var presence.
        //
        // For Anthropic we build the client directly instead of going
        // through `ApiProviderClient::from_model_with_anthropic_auth`
        // so we can explicitly apply `api::read_base_url()` — that
        // reads `ANTHROPIC_BASE_URL` and is required for the local
        // mock-server test harness
        // (`crates/rusty-claude-cli/tests/compact_output.rs`) to point
        // claw at its fake Anthropic endpoint. We also attach a
        // session-scoped prompt cache on the Anthropic path; the
        // prompt cache is Anthropic-only so non-Anthropic variants
        // skip it.
        let resolved_model = api::resolve_model_alias(&model);
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
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            session_id: session_id.to_string(),
            model,
            enable_tools,
            emit_output,
            allowed_tools,
            tool_registry,
            progress_reporter,
            reasoning_effort: None,
            streamed_text: std::sync::atomic::AtomicBool::new(false),
            abort_signal: None,
        })
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    pub(crate) fn set_abort_signal(&mut self, abort_signal: runtime::HookAbortSignal) {
        self.abort_signal = Some(abort_signal);
    }

    pub(crate) fn streamed_text(&self) -> bool {
        self.streamed_text
            .load(std::sync::atomic::Ordering::Relaxed)
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

    /// One model request, falling back to another model (settings
    /// `providerFallbacks.fallbacks`, e.g. the local server) when this one is
    /// rate-limited, out of credit, failing, silent, or replies with nothing
    /// twice. The switch lasts [`FALLBACK_COOLDOWN`], then the next request
    /// tries the configured model again.
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if let Some((model, client)) = active_fallback(&self.model, self.emit_output) {
            return self.stream_through_fallbacks(model, client, &request);
        }
        let mut empty_replies = 0;
        let mut lowered_reply_budget = false;
        loop {
            let error = match self.stream_once(request.clone()) {
                Ok(events) if is_empty_reply(&events) => {
                    empty_replies += 1;
                    if empty_replies < 2 {
                        continue;
                    }
                    "the model replied with nothing twice".to_string()
                }
                Ok(events) => return Ok(events),
                // "requires more credits, or fewer max_tokens … can only
                // afford N": the same request with a reply budget of N goes
                // through, instead of the turn moving to the fallback model.
                Err(error)
                    if !lowered_reply_budget
                        && note_affordable_reply_budget(&self.model, &error.to_string()) =>
                {
                    lowered_reply_budget = true;
                    continue;
                }
                // The model refused the attached image: send it again with
                // the images as text, and from now on.
                Err(error) if sends_images(&self.model) && is_image_refusal(&error.to_string()) => {
                    note_no_images(&self.model);
                    continue;
                }
                Err(error) if is_fallback_worthy(&error.to_string()) => error.to_string(),
                Err(error) => return Err(error),
            };
            let Some((model, client)) =
                switch_to_fallback(&self.model, &error, &[], self.emit_output)
            else {
                return if empty_replies >= 2 {
                    Err(RuntimeError::new("assistant stream produced no content"))
                } else {
                    Err(RuntimeError::new(error))
                };
            };
            return self.stream_through_fallbacks(model, client, &request);
        }
    }
}

impl AnthropicRuntimeClient {
    /// Sends the request to fallback `model`; when that one fails the same
    /// way (rate limit, error, stall, empty reply), moves on to the next
    /// fallback in the list instead of ending the turn (as hermes walks its
    /// chain). Fails only once every fallback has.
    pub(crate) fn stream_through_fallbacks(
        &mut self,
        mut model: String,
        mut client: ApiProviderClient,
        request: &ApiRequest,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut failed = Vec::new();
        loop {
            let error = match self.stream_as(&model, client, request) {
                Ok(events) if is_empty_reply(&events) => {
                    "the model replied with nothing".to_string()
                }
                Ok(events) => return Ok(events),
                Err(error) if sends_images(&model) && is_image_refusal(&error.to_string()) => {
                    note_no_images(&model);
                    // The client went with the request; the same model again.
                    client = ApiProviderClient::from_model_with_anthropic_auth(&model, None)
                        .map_err(|_| error)?;
                    continue;
                }
                Err(error) if is_fallback_worthy(&error.to_string()) => error.to_string(),
                Err(error) => return Err(error),
            };
            failed.push(model.clone());
            let primary = self.model.clone();
            let Some(next) = switch_to_fallback(
                &primary,
                &format!("{model}: {error}"),
                &failed,
                self.emit_output,
            ) else {
                return Err(RuntimeError::new(error));
            };
            (model, client) = next;
        }
    }

    /// [`Self::stream_once`] with another model and client in place of this
    /// one's for the request.
    pub(crate) fn stream_as(
        &mut self,
        model: &str,
        client: ApiProviderClient,
        request: &ApiRequest,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let saved_client = std::mem::replace(&mut self.client, client);
        let saved_model = std::mem::replace(&mut self.model, model.to_string());
        let result = self.stream_once(request.clone());
        self.client = saved_client;
        self.model = saved_model;
        result
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn stream_once(
        &mut self,
        request: ApiRequest,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        if self.emit_output {
            step_stats::begin_step();
        }
        // With a fallback model a silent provider is given up on quickly, so
        // the first reply is timed too, not only the one after a tool.
        let is_post_tool =
            request_ends_with_tool_result(&request) || !fallback_models_for(&self.model).is_empty();
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: reply_budget(&self.model),
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n")),
            tools: self.enable_tools.then(|| {
                sent_tool_definitions(
                    &self.tool_registry,
                    &self.model,
                    self.allowed_tools.as_ref(),
                )
            }),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            reasoning_effort: self.reasoning_effort.clone(),
            extra_body: local_thinking_controls(&self.model, &request),
            ..Default::default()
        };
        let mut message_request = message_request;
        if !sends_images(&self.model) {
            images_as_text(&self.runtime, &mut message_request.messages, &self.model);
        }
        if self.emit_output || desktop_agent::is_active() {
            remember_for_side_questions(&self.client, &message_request);
        }

        let abort_signal = self.abort_signal.clone();
        if self.emit_output {
            runtime::turn_keys::begin_model_request();
            render::start_thinking("thinking");
        }
        let request = async {
            // When resuming after tool execution, apply a stall timeout on the
            // first stream event.  If the model does not respond within the
            // deadline we drop the stalled connection and re-send the request as
            // a continuation nudge (one retry only).
            // With a fallback model a stall is not re-sent: the fallback takes over.
            let max_attempts: usize = if is_post_tool && fallback_models_for(&self.model).is_empty()
            {
                2
            } else {
                1
            };

            for attempt in 1..=max_attempts {
                let result = self
                    .consume_stream(&message_request, is_post_tool && attempt == 1)
                    .await;
                match result {
                    Ok(events) => return Ok(events),
                    Err(error)
                        if error.to_string().contains("post-tool stall")
                            && attempt < max_attempts =>
                    {
                        // Stalled after tool completion — nudge the model by
                        // re-sending the same request.
                    }
                    Err(error) => return Err(error),
                }
            }

            Err(RuntimeError::new("post-tool continuation nudge exhausted"))
        };
        // Dropping `request` on Esc closes the connection, which also stops
        // generation on the server. Ctrl+Enter does the same, but the turn
        // goes on: the reply ends there and the queued messages follow it.
        let watch_send_now = self.emit_output;
        let result = self.runtime.block_on(async {
            tokio::select! {
                result = request => result,
                () = wait_for_abort(abort_signal) => Err(RuntimeError::new(INTERRUPTED_BY_USER)),
                () = wait_for_send_now(watch_send_now) => {
                    runtime::turn_keys::clear_send_now();
                    eprintln!("\n\x1b[2m(reply cut off to send your message)\x1b[0m");
                    Ok(vec![
                        AssistantEvent::TextDelta(SENT_NOW_REPLY.to_string()),
                        AssistantEvent::MessageStop,
                    ])
                }
            }
        });
        render::stop_thinking();
        if let Ok(events) = &result {
            step_stats::end_step(&self.model, events);
        }
        result
    }
}

impl AnthropicRuntimeClient {
    /// Consume a single streaming response, optionally applying a stall
    /// timeout on the first event for post-tool continuations.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn consume_stream(
        &self,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut stream = self
            .client
            .stream_message(message_request)
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        // Replies keep a margin on both sides and wrap inside it.
        let mut stdout = render::MarginWriter::for_stdout(StopThinkingOnWrite(io::stdout()));
        let mut sink = io::sink();
        let out: &mut dyn Write = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        let renderer = TerminalRenderer::new();
        let mut markdown_stream = MarkdownStreamState::default();
        let mut events = Vec::new();
        let mut pending_tool: Option<(String, String, String)> = None;
        // 累积 reasoning_content 到 Thinking 块（修复 DeepSeek V4 reasoning_content 协议 bug）
        let mut pending_thinking: Option<(String, Option<String>)> = None;
        let mut block_has_thinking_summary = false;
        let mut saw_stop = false;
        let mut received_any_event = false;

        loop {
            let next = if apply_stall_timeout && !received_any_event {
                let stall_timeout = if message_request.model.starts_with("local/") {
                    LOCAL_POST_TOOL_STALL_TIMEOUT
                } else {
                    POST_TOOL_STALL_TIMEOUT
                };
                match tokio::time::timeout(stall_timeout, stream.next_event()).await {
                    Ok(inner) => inner.map_err(|error| {
                        RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                    })?,
                    Err(_elapsed) => {
                        return Err(RuntimeError::new(
                            "post-tool stall: model did not respond within timeout",
                        ));
                    }
                }
            } else {
                stream.next_event().await.map_err(|error| {
                    RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                })?
            };

            let Some(event) = next else {
                break;
            };
            received_any_event = true;
            if matches!(event, ApiStreamEvent::ContentBlockDelta(_)) {
                step_stats::mark_first_token();
            }

            match event {
                ApiStreamEvent::MessageStart(start) => {
                    for block in start.message.content {
                        push_output_block(
                            block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                            &mut block_has_thinking_summary,
                        )?;
                    }
                }
                ApiStreamEvent::ContentBlockStart(start) => {
                    // 特判 Thinking 块：初始化 pending_thinking（用于累积后续 ThinkingDelta）
                    if let OutputContentBlock::Thinking {
                        thinking,
                        signature,
                    } = &start.content_block
                    {
                        // The block is recorded once, whole, when it stops;
                        // recording its (empty) start as well saved an empty
                        // thinking block before every real one, sent back
                        // with the turn's later steps.
                        pending_thinking = Some((thinking.clone(), signature.clone()));
                        render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
                        block_has_thinking_summary = true;
                    } else {
                        push_output_block(
                            start.content_block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                            &mut block_has_thinking_summary,
                        )?;
                    }
                    if let Some((_, name, _)) = &pending_tool {
                        runtime::turn_keys::begin_tool(&format!("Preparing {name}"));
                    }
                }
                ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                    ContentBlockDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            runtime::turn_keys::note_text(text.chars().count());
                            if let Some(progress_reporter) = &self.progress_reporter {
                                progress_reporter.mark_text_phase(&text);
                            }
                            if let Some(rendered) = markdown_stream.push(&renderer, &text) {
                                write!(out, "{rendered}")
                                    .and_then(|()| out.flush())
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                                if self.emit_output {
                                    self.streamed_text
                                        .store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            desktop_agent::emit_text(&text);
                            events.push(AssistantEvent::TextDelta(text));
                        }
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        runtime::turn_keys::note_tool_input(partial_json.chars().count());
                        if let Some((_, _, input)) = &mut pending_tool {
                            input.push_str(&partial_json);
                        }
                    }
                    ContentBlockDelta::ThinkingDelta { thinking } => {
                        runtime::turn_keys::note_reasoning(thinking.chars().count());
                        desktop_agent::emit_thinking(&thinking);
                        if !block_has_thinking_summary {
                            // Full-screen mode prints the block, collapsible,
                            // once it is complete (see ContentBlockStop).
                            if !runtime::turn_keys::screen::active() {
                                render_thinking_block_summary(out, None, false)?;
                            }
                            block_has_thinking_summary = true;
                        }
                        // 累积 thinking 文本到 pending_thinking（让 session 持久化能拿到）
                        if let Some((t, _)) = &mut pending_thinking {
                            t.push_str(&thinking);
                        }
                    }
                    ContentBlockDelta::SignatureDelta { signature } => {
                        // 累积 signature 到 pending_thinking
                        if let Some((_, sig)) = &mut pending_thinking {
                            sig.get_or_insert_with(String::new).push_str(&signature);
                        }
                    }
                },
                ApiStreamEvent::ContentBlockStop(_) => {
                    block_has_thinking_summary = false;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        if self.emit_output {
                            self.streamed_text
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    // 把累积的 thinking 转成 AssistantEvent::Thinking（让 build_assistant_message 写入 session）
                    if let Some((thinking, signature)) = pending_thinking.take() {
                        if runtime::turn_keys::screen::active() {
                            render_thinking_fold(out, &thinking)?;
                        }
                        events.push(AssistantEvent::Thinking {
                            thinking,
                            signature,
                        });
                    }
                    if let Some((id, name, input)) = pending_tool.take() {
                        let input = repaired_tool_input(input);
                        runtime::turn_keys::end_tool();
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_tool_phase(&name, &input);
                        }
                        desktop_agent::emit_tool_call(&id, &name, &input);
                        // Display tool call now that input is fully accumulated
                        let (brief, full) = foldable(|| format_tool_call_start(&name, &input));
                        writeln!(out)
                            .and_then(|()| write_folded(out, &brief, full.as_deref()))
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        events.push(AssistantEvent::ToolUse { id, name, input });
                    }
                }
                ApiStreamEvent::MessageDelta(delta) => {
                    if delta
                        .delta
                        .stop_reason
                        .as_deref()
                        .is_some_and(runtime::is_output_limit_stop_reason)
                    {
                        if let Some(rendered) = markdown_stream.flush(&renderer) {
                            write!(out, "{rendered}")
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                        }
                        writeln!(
                            out,
                            "\n\x1b[2m(reply cut off at the {}-token limit; asking the model to continue)\x1b[0m",
                            message_request.max_tokens
                        )
                        .and_then(|()| out.flush())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                        events.push(AssistantEvent::OutputLimitReached);
                    }
                    events.push(AssistantEvent::Usage(delta.usage.token_usage()));
                }
                ApiStreamEvent::MessageStop(_) => {
                    saw_stop = true;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        if self.emit_output {
                            self.streamed_text
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    events.push(AssistantEvent::MessageStop);
                }
            }
        }

        push_prompt_cache_record(&self.client, &mut events);

        if !saw_stop
            && events.iter().any(|event| {
                matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                    || matches!(event, AssistantEvent::ToolUse { .. })
            })
        {
            events.push(AssistantEvent::MessageStop);
        }

        if events
            .iter()
            .any(|event| matches!(event, AssistantEvent::MessageStop))
        {
            return Ok(events);
        }

        let response = self
            .client
            .send_message(&MessageRequest {
                stream: false,
                ..message_request.clone()
            })
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut events = response_to_events(response, out)?;
        push_prompt_cache_record(&self.client, &mut events);
        Ok(events)
    }
}
