//! Runs the model's tool calls for the CLI: permission and allowed-list
//! checks, deferred-tool loading through ToolSearch, MCP and plugin tools, and
//! the progress and result display of each call.

#[allow(clippy::wildcard_imports)]
use crate::*;

pub(crate) struct CliToolExecutor {
    pub(crate) renderer: TerminalRenderer,
    pub(crate) emit_output: bool,
    pub(crate) allowed_tools: Option<AllowedToolSet>,
    pub(crate) tool_registry: GlobalToolRegistry,
    pub(crate) mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    /// The previous call (name, input, result) and how many times in a row
    /// it came back unchanged, so a model stuck repeating it gets told.
    pub(crate) last_call: Mutex<Option<(String, String, String, usize)>>,
    /// The model the results go to; sizes `read_file` pages to its window.
    pub(crate) model: Option<String>,
}

impl CliToolExecutor {
    pub(crate) fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            allowed_tools,
            tool_registry,
            mcp_state,
            last_call: Mutex::new(None),
            model: None,
        }
    }

    pub(crate) fn with_model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    /// A `read_file` of a file with no `offset`, right after a read of the
    /// same file whose page was cut: the offset of the next page and the
    /// input asking for it. Qwen re-sent the same call six times in a row,
    /// thinking "it always returns lines 1-465, let me read from 465" but
    /// never adding the offset.
    pub(crate) fn next_page_instead(
        &self,
        tool_name: &str,
        input: &str,
    ) -> Option<(usize, String)> {
        if tool_name != "read_file" {
            return None;
        }
        let mut value = serde_json::from_str::<serde_json::Value>(input).ok()?;
        let object = value.as_object_mut()?;
        if ["offset", "limit"]
            .iter()
            .any(|key| object.contains_key(*key))
        {
            return None;
        }
        let path = object.get("path")?.as_str()?.to_string();
        let last = self.last_call.lock().ok()?;
        let (last_name, last_input, last_output, _) = last.as_ref()?;
        let last_path = serde_json::from_str::<serde_json::Value>(last_input)
            .ok()?
            .get("path")?
            .as_str()?
            .to_string();
        if last_name != "read_file" || last_path != path {
            return None;
        }
        let offset = last_output
            .rsplit_once("Continue with read_file offset=")?
            .1
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse::<usize>()
            .ok()?;
        object.insert("offset".to_string(), json!(offset));
        Some((offset, value.to_string()))
    }

    /// Appends a note when this exact call returned this exact result last
    /// time too. Small local models otherwise re-issue a call whose output
    /// they misread (e.g. a truncated file) until the turn runs out.
    pub(crate) fn note_repeated_call(
        &self,
        tool_name: &str,
        input: &str,
        output: String,
    ) -> String {
        let Ok(mut last) = self.last_call.lock() else {
            return output;
        };
        let repeats = match last.as_ref() {
            Some((name, last_input, last_output, count))
                if name == tool_name && last_input == input && *last_output == output =>
            {
                count + 1
            }
            _ => 0,
        };
        *last = Some((
            tool_name.to_string(),
            input.to_string(),
            output.clone(),
            repeats,
        ));
        if repeats == 0 {
            return output;
        }
        // The identical output is already in the conversation, right above:
        // sending it again (a 16k-character file page, six times over) only
        // filled a small window until old tool output was pruned to make
        // room, the file the model needed with it.
        let shown = if output.chars().count() > 600 {
            "[The output is identical to your previous call's, shown above; not repeated.]"
                .to_string()
        } else {
            output
        };
        format!(
            "{shown}\n\n[note: this is the same call with the same result as your previous \
             {} call(s). Repeating it will not return anything new: use what you already have, \
             change the arguments (e.g. offset/limit), or try a different approach.]",
            repeats
        )
    }

    /// [`read_file_outline_for_model`], unless the previous call was this
    /// same read and got the outline: asked again, the model gets the page.
    pub(crate) fn outline_instead(
        &self,
        input: &str,
        output: &str,
        budget: Option<usize>,
    ) -> Option<String> {
        let asked_again = self.last_call.lock().ok().is_some_and(|last| {
            last.as_ref()
                .is_some_and(|(name, last_input, last_output, _)| {
                    matches!(name.as_str(), "read_file" | "Read")
                        && last_input == input
                        && last_output.contains(READ_OUTLINE_NOTE)
                })
        });
        if asked_again {
            return None;
        }
        read_file_outline_for_model(input, output, budget)
    }

    pub(crate) fn execute_search_tool(
        &self,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        let found = self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        );
        // The matches the session may use are loaded: sent in full from the
        // next request on, and described here so they can be called now.
        let usable = found
            .matches
            .iter()
            .map(|name| canonical_allowed_tool_name(name))
            .filter(|name| {
                self.allowed_tools
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(name))
            })
            .collect::<AllowedToolSet>();
        let loaded = if usable.is_empty() {
            Vec::new()
        } else {
            LOADED_TOOLS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(usable.iter().cloned());
            self.tool_registry.definitions(Some(&usable))
        };
        let mut result =
            serde_json::to_value(&found).map_err(|error| ToolError::new(error.to_string()))?;
        if !loaded.is_empty() {
            result["loaded"] = json!(loaded
                .iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }))
                .collect::<Vec<_>>());
        }
        serde_json::to_string_pretty(&result).map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        run_mcp_tool(mcp_state, tool_name, value)
    }
}

/// Runs one MCP runtime tool (a server tool or the MCP wrapper tools) on
/// the session's MCP state.
pub(crate) fn run_mcp_tool(
    mcp_state: &Mutex<RuntimeMcpState>,
    tool_name: &str,
    value: serde_json::Value,
) -> Result<String, ToolError> {
    {
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn can_run_in_parallel(&self, tool_name: &str) -> bool {
        PARALLEL_SAFE_TOOLS.contains(&tool_name)
            && !self.tool_registry.has_runtime_tool(tool_name)
            && self.check_allowed(tool_name).is_ok()
    }

    /// Runs read-only calls on scoped threads. The threads only borrow the
    /// registry immutably and return their results; printing and everything
    /// else that touches shared state happens here, in the original order.
    fn execute_batch(&mut self, calls: &[(String, String)]) -> Vec<Result<String, ToolError>> {
        let _timer = self.emit_output.then(step_stats::ToolTimer::start);
        let calls = calls
            .iter()
            .map(|(tool_name, input)| (self.resolve_tool_name(tool_name), input.clone()))
            .collect::<Vec<_>>();
        let calls = calls.as_slice();
        if calls
            .iter()
            .any(|(tool_name, _)| !self.can_run_in_parallel(tool_name))
        {
            return calls
                .iter()
                .map(|(tool_name, input)| self.execute(tool_name, input))
                .collect();
        }
        if self.emit_output {
            runtime::turn_keys::begin_tool(&format!("Running {} tools in parallel", calls.len()));
            render::start_thinking(&format!("running {} tools in parallel", calls.len()));
        }
        let registry = &self.tool_registry;
        let results = std::thread::scope(|scope| {
            let workers = calls
                .iter()
                .map(|(tool_name, input)| {
                    scope.spawn(move || {
                        let value = serde_json::from_str(input).map_err(|error| {
                            ToolError::new(format!("invalid tool input JSON: {error}"))
                        })?;
                        registry.execute(tool_name, &value).map_err(ToolError::new)
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .unwrap_or_else(|_| Err(ToolError::new("tool panicked while running")))
                })
                .collect::<Vec<_>>()
        });
        render::stop_thinking();
        runtime::turn_keys::end_tool();
        calls
            .iter()
            .zip(results)
            .map(|((tool_name, input), result)| self.report_result(tool_name, input, result))
            .collect()
    }

    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let _timer = self.emit_output.then(step_stats::ToolTimer::start);
        let tool_name = &self.resolve_tool_name(tool_name);
        self.check_allowed(tool_name)?;
        if let Some((offset, next_input)) = self.next_page_instead(tool_name, input) {
            let page = self.execute(tool_name, &next_input)?;
            return Ok(format!(
                "[You asked for this file from the start again, right after reading a page \
                 of it that was cut. This is the next page (offset={offset}); pass `offset` \
                 yourself to read a particular part, or offset=0 for the start.]\n{page}"
            ));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        if tool_name == "AskUserQuestion" && desktop_agent::is_active() {
            return self.report_result(
                tool_name,
                input,
                desktop_agent::ask_user(&value).map_err(ToolError::new),
            );
        }
        if tool_name == "AskUserQuestion" && self.emit_output {
            // The picker shows the question and the answer itself.
            runtime::turn_keys::begin_tool("Waiting for your answer");
            let result = ask::ask_user(&value).map_err(ToolError::new);
            runtime::turn_keys::end_tool();
            return result;
        }
        if self.emit_output {
            runtime::turn_keys::begin_tool(&tool_activity_label(tool_name, input));
            render::start_thinking(&format!("running {tool_name}"));
        }
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute(tool_name, &value)
                .map_err(ToolError::new)
        };
        render::stop_thinking();
        runtime::turn_keys::end_tool();
        self.report_result(tool_name, input, result)
    }
}
