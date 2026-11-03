use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use api::{
    max_tokens_for_model, model_family_identity_for, resolve_model_alias, ApiError,
    ContentBlockDelta, ImageSource, InputContentBlock, InputMessage, MessageRequest,
    MessageResponse, OutputContentBlock, ProviderClient, StreamEvent as ApiStreamEvent, ToolChoice,
    ToolDefinition, ToolResultContentBlock,
};
use plugins::PluginTool;
use reqwest::blocking::Client;
use runtime::{
    check_freshness, dedupe_superseded_commit_events, edit_file_in_workspace, execute_bash,
    glob_search_in_workspace, grep_search_in_workspace, load_system_prompt,
    mcp_tool_bridge::McpToolRegistry,
    permission_enforcer::{EnforcementResult, PermissionEnforcer},
    read_file_in_workspace,
    summary_compression::compress_summary_text,
    task_registry::TaskRegistry,
    team_cron_registry::{CronRegistry, TeamRegistry},
    worker_boot::{WorkerReadySnapshot, WorkerRegistry, WorkerTaskReceipt},
    write_file_in_workspace, ApiClient, ApiRequest, AssistantEvent, BashCommandInput,
    BashCommandOutput, BranchFreshness, ConfigLoader, ContentBlock, ConversationMessage,
    ConversationRuntime, GrepSearchInput, LaneCommitProvenance, LaneEvent, LaneEventBlocker,
    LaneEventName, LaneEventStatus, LaneFailureClass, McpDegradedReport, MessageRole,
    PermissionMode, PermissionPolicy, PromptCacheEvent, ProviderFallbackConfig, RuntimeError,
    Session, TaskPacket, ToolError, ToolExecutor,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

fn global_mcp_registry() -> &'static McpToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<McpToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(McpToolRegistry::new)
}

fn global_team_registry() -> &'static TeamRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<TeamRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TeamRegistry::new)
}

fn global_cron_registry() -> &'static CronRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<CronRegistry> = OnceLock::new();
    REGISTRY.get_or_init(CronRegistry::new)
}

fn global_task_registry() -> &'static TaskRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<TaskRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TaskRegistry::new)
}

fn global_worker_registry() -> &'static WorkerRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<WorkerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(WorkerRegistry::new)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifestEntry {
    pub name: String,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Base,
    Conditional,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRegistry {
    entries: Vec<ToolManifestEntry>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(entries: Vec<ToolManifestEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[ToolManifestEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub required_permission: PermissionMode,
}

#[derive(Debug, Clone)]
pub struct GlobalToolRegistry {
    plugin_tools: Vec<PluginTool>,
    runtime_tools: Vec<RuntimeToolDefinition>,
    enforcer: Option<PermissionEnforcer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub required_permission: PermissionMode,
}

impl GlobalToolRegistry {
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            plugin_tools: Vec::new(),
            runtime_tools: Vec::new(),
            enforcer: None,
        }
    }

    pub fn with_plugin_tools(plugin_tools: Vec<PluginTool>) -> Result<Self, String> {
        let builtin_names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .collect::<BTreeSet<_>>();
        let mut seen_plugin_names = BTreeSet::new();

        for tool in &plugin_tools {
            let name = tool.definition().name.clone();
            if builtin_names.contains(&name) {
                return Err(format!(
                    "plugin tool `{name}` conflicts with a built-in tool name"
                ));
            }
            if !seen_plugin_names.insert(name.clone()) {
                return Err(format!("duplicate plugin tool name `{name}`"));
            }
        }

        Ok(Self {
            plugin_tools,
            runtime_tools: Vec::new(),
            enforcer: None,
        })
    }

    pub fn with_runtime_tools(
        mut self,
        runtime_tools: Vec<RuntimeToolDefinition>,
    ) -> Result<Self, String> {
        let mut seen_names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.definition().name.clone()),
            )
            .collect::<BTreeSet<_>>();

        for tool in &runtime_tools {
            if !seen_names.insert(tool.name.clone()) {
                return Err(format!(
                    "runtime tool `{}` conflicts with an existing tool name",
                    tool.name
                ));
            }
        }

        self.runtime_tools = runtime_tools;
        Ok(self)
    }

    #[must_use]
    pub fn with_enforcer(mut self, enforcer: PermissionEnforcer) -> Self {
        self.set_enforcer(enforcer);
        self
    }

    pub fn normalize_allowed_tools(
        &self,
        values: &[String],
    ) -> Result<Option<BTreeSet<String>>, String> {
        if values.is_empty() {
            return Ok(None);
        }

        let actual_names = self.actual_tool_names();
        let canonical_names = self.canonical_allowed_tool_names();
        let canonical_name_set = canonical_names.iter().cloned().collect::<BTreeSet<_>>();
        let mut name_map = BTreeMap::new();
        for actual in &actual_names {
            let canonical = canonical_allowed_tool_name(actual);
            name_map.insert(allowed_tool_lookup_key(actual), canonical.clone());
            name_map.insert(allowed_tool_lookup_key(&canonical), canonical);
        }

        for (alias, canonical) in self.allowed_tool_aliases() {
            if canonical_name_set.contains(&canonical) {
                name_map.insert(allowed_tool_lookup_key(&alias), canonical);
            }
        }

        let mut allowed = BTreeSet::new();
        for value in values {
            for token in value
                .split(|ch: char| ch == ',' || ch.is_whitespace())
                .filter(|token| !token.is_empty())
            {
                let canonical = name_map.get(&allowed_tool_lookup_key(token)).ok_or_else(|| {
                    format!(
                        "invalid_tool_name: unsupported tool in --allowedTools: {token}\nAvailable: {}\nAliases: {}\nHint: Use canonical snake_case tool names from Available or aliases from Aliases.",
                        canonical_names.join(", "),
                        format_allowed_tool_aliases(&self.allowed_tool_aliases())
                    )
                })?;
                allowed.insert(canonical.clone());
            }
        }

        if allowed.is_empty() {
            return Err(format!(
                "--allowedTools was provided with no usable tool names (got `{}`). Omit the flag to allow all tools.",
                values.join(" ")
            ));
        }

        Ok(Some(allowed))
    }

    #[must_use]
    pub fn definitions(&self, allowed_tools: Option<&BTreeSet<String>>) -> Vec<ToolDefinition> {
        let builtin = mvp_tool_specs()
            .into_iter()
            .filter(|spec| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(&canonical_allowed_tool_name(spec.name)))
            })
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            });
        let runtime = self
            .runtime_tools
            .iter()
            .filter(|tool| {
                allowed_tools.is_none_or(|allowed| {
                    allowed.contains(&canonical_allowed_tool_name(&tool.name))
                })
            })
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            });
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools.is_none_or(|allowed| {
                    allowed.contains(&canonical_allowed_tool_name(
                        tool.definition().name.as_str(),
                    ))
                })
            })
            .map(|tool| ToolDefinition {
                name: tool.definition().name.clone(),
                description: tool.definition().description.clone(),
                input_schema: tool.definition().input_schema.clone(),
            });
        builtin.chain(runtime).chain(plugin).collect()
    }

    pub fn permission_specs(
        &self,
        allowed_tools: Option<&BTreeSet<String>>,
    ) -> Result<Vec<(String, PermissionMode)>, String> {
        let builtin = mvp_tool_specs()
            .into_iter()
            .filter(|spec| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(&canonical_allowed_tool_name(spec.name)))
            })
            .map(|spec| (spec.name.to_string(), spec.required_permission));
        let runtime = self
            .runtime_tools
            .iter()
            .filter(|tool| {
                allowed_tools.is_none_or(|allowed| {
                    allowed.contains(&canonical_allowed_tool_name(&tool.name))
                })
            })
            .map(|tool| (tool.name.clone(), tool.required_permission));
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools.is_none_or(|allowed| {
                    allowed.contains(&canonical_allowed_tool_name(
                        tool.definition().name.as_str(),
                    ))
                })
            })
            .map(|tool| {
                permission_mode_from_plugin(tool.required_permission())
                    .map(|permission| (tool.definition().name.clone(), permission))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(builtin.chain(runtime).chain(plugin).collect())
    }

    #[must_use]
    pub fn actual_tool_names(&self) -> Vec<String> {
        mvp_tool_specs()
            .iter()
            .map(|spec| spec.name.to_string())
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.definition().name.clone()),
            )
            .chain(self.runtime_tools.iter().map(|tool| tool.name.clone()))
            .collect()
    }

    #[must_use]
    pub fn canonical_allowed_tool_names(&self) -> Vec<String> {
        self.actual_tool_names()
            .into_iter()
            .map(|name| canonical_allowed_tool_name(&name))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub fn allowed_tool_aliases(&self) -> BTreeMap<String, String> {
        let mut aliases = BTreeMap::from([
            ("read".to_string(), "read_file".to_string()),
            ("Read".to_string(), "read_file".to_string()),
            ("write".to_string(), "write_file".to_string()),
            ("Write".to_string(), "write_file".to_string()),
            ("edit".to_string(), "edit_file".to_string()),
            ("Edit".to_string(), "edit_file".to_string()),
            ("glob".to_string(), "glob_search".to_string()),
            ("Glob".to_string(), "glob_search".to_string()),
            ("grep".to_string(), "grep_search".to_string()),
            ("Grep".to_string(), "grep_search".to_string()),
        ]);
        for actual in self.actual_tool_names() {
            let canonical = canonical_allowed_tool_name(&actual);
            if actual != canonical {
                aliases.insert(actual, canonical);
            }
        }
        aliases
    }
    #[must_use]
    pub fn has_runtime_tool(&self, name: &str) -> bool {
        self.runtime_tools.iter().any(|tool| tool.name == name)
    }

    #[must_use]
    pub fn search(
        &self,
        query: &str,
        max_results: usize,
        pending_mcp_servers: Option<Vec<String>>,
        mcp_degraded: Option<McpDegradedReport>,
    ) -> ToolSearchOutput {
        let query = query.trim().to_string();
        let normalized_query = normalize_tool_search_query(&query);
        let matches = search_tool_specs(&query, max_results.max(1), &self.searchable_tool_specs());

        ToolSearchOutput {
            matches,
            query,
            normalized_query,
            total_deferred_tools: self.searchable_tool_specs().len(),
            pending_mcp_servers,
            mcp_degraded,
        }
    }

    pub fn set_enforcer(&mut self, enforcer: PermissionEnforcer) {
        self.enforcer = Some(enforcer);
    }

    pub fn execute(&self, name: &str, input: &Value) -> Result<String, String> {
        if mvp_tool_specs().iter().any(|spec| spec.name == name) {
            return execute_tool_with_enforcer(self.enforcer.as_ref(), name, input);
        }
        self.plugin_tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .ok_or_else(|| format!("unsupported tool: {name}"))?
            .execute(input)
            .map_err(|error| error.to_string())
    }

    fn searchable_tool_specs(&self) -> Vec<SearchableToolSpec> {
        let builtin = deferred_tool_specs()
            .into_iter()
            .map(|spec| SearchableToolSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
            });
        let runtime = self.runtime_tools.iter().map(|tool| SearchableToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
        });
        let plugin = self.plugin_tools.iter().map(|tool| SearchableToolSpec {
            name: tool.definition().name.clone(),
            description: tool.definition().description.clone().unwrap_or_default(),
        });
        builtin.chain(runtime).chain(plugin).collect()
    }
}

pub fn canonical_allowed_tool_name(value: &str) -> String {
    let trimmed = value.trim().replace('-', "_");
    let mut output = String::new();
    let chars = trimmed.chars().collect::<Vec<_>>();
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch == '_' || ch.is_whitespace() {
            output.push('_');
            continue;
        }
        let previous = index.checked_sub(1).and_then(|i| chars.get(i)).copied();
        let next = chars.get(index + 1).copied();
        if ch.is_ascii_uppercase()
            && index > 0
            && !output.ends_with('_')
            && (previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
                || next.is_some_and(|n| n.is_ascii_lowercase()))
        {
            output.push('_');
        }
        output.push(ch.to_ascii_lowercase());
    }
    output.trim_matches('_').to_string()
}

fn allowed_tool_lookup_key(value: &str) -> String {
    canonical_allowed_tool_name(value).replace('_', "")
}

fn format_allowed_tool_aliases(aliases: &BTreeMap<String, String>) -> String {
    aliases
        .iter()
        .map(|(alias, canonical)| format!("{alias}={canonical}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn permission_mode_from_plugin(value: &str) -> Result<PermissionMode, String> {
    match value {
        "read-only" => Ok(PermissionMode::ReadOnly),
        "workspace-write" => Ok(PermissionMode::WorkspaceWrite),
        "danger-full-access" => Ok(PermissionMode::DangerFullAccess),
        other => Err(format!("unsupported plugin permission: {other}")),
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn mvp_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash",
            description: "Execute a shell command in the current workspace. `~` and $HOME are the user's real home directory. Output comes back as plain text ending with [exit code N]. Commands run without a terminal (stdin is closed), so use non-interactive flags: no `emerge --ask`, add `-y`/`--noconfirm` where a tool would prompt; claw asks the user itself before risky commands. Set run_in_background to true for long-running commands (servers, builds, watchers): it returns a job id at once, you are notified when the job exits, and BashOutput reads its output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Timeout in MILLISECONDS (120000 = 2 minutes). Omit it unless the command may hang."
                    },
                    "description": { "type": "string" },
                    "run_in_background": { "type": "boolean" },
                    "dangerouslyDisableSandbox": { "type": "boolean", "description": "Request explicit approval to execute this exact command on the host outside the sandbox. Use for necessary host administration: sudo, su, doas and pkexec cannot gain root inside the sandbox, and only the workspace is writable there, so set it for commands that need root (installing or updating packages, editing /etc, system services). Never set it for package searches. Ordinary Bash approval does not grant host access." },
                    "namespaceRestrictions": { "type": "boolean" },
                    "isolateNetwork": { "type": "boolean" },
                    "filesystemMode": { "type": "string", "enum": ["off", "workspace-only", "allow-list"] },
                    "allowedMounts": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "BashOutput",
            description: "Read new output from a background shell job started with bash(run_in_background=true), plus its status. Optionally wait up to wait_seconds for output or exit.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "bash_id": { "type": "string", "description": "Job id returned by bash, e.g. bg1" },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 120 }
                },
                "required": ["bash_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "KillShell",
            description: "Stop a background shell job (and its child processes) started with bash(run_in_background=true).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "shell_id": { "type": "string", "description": "Job id returned by bash, e.g. bg1" }
                },
                "required": ["shell_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "read_file",
            description: "Read a file from the workspace. Text files are returned as-is (use offset/limit for long files). Also reads documents as text: Excel and other spreadsheets (.xlsx/.xlsm/.xls/.xlsb/.ods) as tables with cell addresses (sheet, range like \"A1:D50\", formulas=true to show formulas), Word (.docx) and PowerPoint (.pptx) as markdown-style text, PDF by page (pages like \"1-10\"), and Jupyter notebooks (.ipynb) as cells with outputs.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0, "description": "Line to start after (0 = from the top); a line number, not a character position" },
                    "limit": { "type": "integer", "minimum": 1, "description": "Number of lines to read" },
                    "sheet": { "type": "string", "description": "Spreadsheets: only this sheet" },
                    "range": { "type": "string", "description": "Spreadsheets: A1 range, e.g. A1:D50" },
                    "formulas": { "type": "boolean", "description": "Spreadsheets: append [=FORMULA] to formula cells" },
                    "pages": { "type": "string", "description": "PDF: page or range, e.g. 3 or 1-10 (default first 50)" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "write_file",
            description: "Create a text file in the workspace, or replace one completely. To change part of an existing file use edit_file: it sends only the change, while rewriting resends the whole file and can drop code that was there. An existing file must be read with read_file first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "write_docx",
            description: "Create a Word document (.docx) from markdown: # headings, paragraphs, - bullets and 1. numbered lists (indent 2 spaces per level), **bold**, *italic*, `code`, [links](url), | tables |, ``` code blocks ```, > quotes and --- rules. Produces real Word styles (Title, Heading 1-6, List Paragraph). Overwrites the file if it exists (read it first).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "markdown": { "type": "string", "description": "Document content in markdown" },
                    "title": { "type": "string", "description": "Optional document title (Title style, before the content)" },
                    "page_size": { "type": "string", "enum": ["A4", "Letter"], "description": "Default A4" }
                },
                "required": ["path", "markdown"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "write_pptx",
            description: "Create a PowerPoint deck (.pptx, 16:9). Optional title slide (title/subtitle), then content slides with a title and any of: bullets (prefix with 2 spaces per sub-level; **bold** spans allowed), a table (table_headers + table_rows), and an image (path to png/jpg/gif). Keep bullets short (max ~6 per slide). Overwrites the file if it exists (read it first).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "title": { "type": "string", "description": "Title slide heading (omit for no title slide)" },
                    "subtitle": { "type": "string" },
                    "slides": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "bullets": { "type": "array", "items": { "type": "string" } },
                                "table_headers": { "type": "array", "items": { "type": "string" } },
                                "table_rows": { "type": "array", "items": { "type": "array", "items": { "type": "string" } } },
                                "image": { "type": "string", "description": "Image file path" }
                            },
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "slides"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "write_xlsx",
            description: "Create or update an Excel workbook (.xlsx). Opens the file if it exists (otherwise creates it), writes values to one sheet (created if missing), and saves, keeping existing formatting. Use 'cells' for individual cells ({\"A1\": \"Name\", \"B2\": 42, \"C2\": \"=SUM(B2:B10)\"}) and/or 'rows' for a block of rows starting at start_cell. Strings starting with '=' are formulas. null clears a cell. Read the workbook first if it exists.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "sheet": { "type": "string", "description": "Sheet name (default: first sheet, or Sheet1 for new files)" },
                    "cells": { "type": "object", "description": "Map of cell address to value", "additionalProperties": {} },
                    "rows": { "type": "array", "items": { "type": "array", "items": {} }, "description": "2D array of values" },
                    "start_cell": { "type": "string", "description": "Top-left cell for rows (default A1)" },
                    "bold_first_row": { "type": "boolean", "description": "Bold the first written row (header)" },
                    "delete_sheet": { "type": "string", "description": "Name of a sheet to delete" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace old_string with new_string in a workspace file: the way to change an existing file, including fixing compiler errors one at a time. new_string replaces only the matched text, not the whole file. Use write_file only for new files or when nearly every line changes. Read the file with read_file first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "glob_search",
            description: "Find files by glob pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "grep_search",
            description: "Search file contents with a regex pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "glob": { "type": "string" },
                    "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content: matching lines; files_with_matches (default): file names only; count: matches per file" },
                    "-B": { "type": "integer", "minimum": 0 },
                    "-A": { "type": "integer", "minimum": 0 },
                    "-C": { "type": "integer", "minimum": 0 },
                    "context": { "type": "integer", "minimum": 0 },
                    "-n": { "type": "boolean" },
                    "-i": { "type": "boolean" },
                    "type": { "type": "string" },
                    "head_limit": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "multiline": { "type": "boolean" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WebFetch",
            description:
                "Fetch a URL, convert it into readable text, and answer a prompt about it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "format": "uri" },
                    "prompt": { "type": "string" }
                },
                "required": ["url", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "X11",
            description: "Control the user's Linux desktop (X11) like a person: `windows` lists windows (id, title, class, geometry); `screen` gives screen size, mouse position and the active window; `screenshot` captures the screen, a `window` or a region (x, y, width, height) and shows it to you; `focus`, `close`, `move_resize` a window; `launch` a program (`command`, optionally `wait_for` its window title); `wait_window`; `type` text and `key` presses (xdotool names: \"ctrl+s\", \"Return\", \"alt+Tab\", several separated by spaces) into the focused window, or into `window` (refused when that window is hidden or can't get focus, so input never lands elsewhere); `click` (button 1 left, 3 right; count 2 = double), `move_mouse`, `drag` (x, y to to_x, to_y), `scroll` (direction, count) at screen coordinates, or relative to `window`'s top-left corner when `window` is given; `clipboard_get`, `clipboard_set`. `window` is an id or part of a title. Take a screenshot to check what happened before acting further. Cheaper than screenshots: `ocr` returns the text of a `window`/region (or `image` file) with screen boxes; the accessibility actions work on named controls of GTK/Qt/Electron apps: `ui_apps`, `ui_find` (app, name, role e.g. \"push button\"), `ui_tree` (app or element, depth), `ui_click`, `ui_read`, `ui_set_text` (element id from ui_find, text). Prefer the `Browser` tool for web pages.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["windows", "screen", "screenshot", "focus", "type", "key", "click", "move_mouse", "drag", "scroll", "close", "move_resize", "launch", "wait_window", "clipboard_get", "clipboard_set", "ocr", "ui_apps", "ui_find", "ui_tree", "ui_click", "ui_read", "ui_set_text"] },
                    "element": { "type": "string", "description": "Element id from ui_find/ui_tree" },
                    "app": { "type": "string" },
                    "name": { "type": "string" },
                    "role": { "type": "string" },
                    "depth": { "type": "integer", "minimum": 0 },
                    "image": { "type": "string" },
                    "window": { "type": "string" },
                    "text": { "type": "string" },
                    "keys": { "type": "string" },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "to_x": { "type": "integer" },
                    "to_y": { "type": "integer" },
                    "width": { "type": "integer" },
                    "height": { "type": "integer" },
                    "button": { "type": "integer", "minimum": 1, "maximum": 9 },
                    "count": { "type": "integer", "minimum": 1 },
                    "direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "command": { "type": "string" },
                    "wait_for": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1 }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "Browser",
            description: "Use a real Chrome window (claw's own profile, logins kept) by page elements, not pixels; prefer it over X11 for anything on the web. Actions: `open` (url) in the current tab; `elements` lists visible links, buttons and fields with numbers; `click` (id or selector); `type` (text into id or selector, submit to press Enter, clear default true); `key` (Enter, Tab, Escape, ArrowDown, ctrl+a); `read` page text (selector, max_chars); `scroll` (direction); `wait` (text or selector); `back`, `forward`, `reload`; `eval` (script, JavaScript, returns its value); `screenshot` (full_page) shows you the page; `tabs`, `new_tab` (url), `switch_tab`/`close_tab` (tab); `start` (headless). Numbers from `elements` change when the page changes: list again after navigating.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["start", "tabs", "open", "new_tab", "switch_tab", "close_tab", "read", "elements", "click", "type", "key", "scroll", "back", "forward", "reload", "wait", "eval", "screenshot"] },
                    "url": { "type": "string" },
                    "id": { "type": "integer", "description": "Element number from `elements`" },
                    "selector": { "type": "string", "description": "CSS selector, instead of id" },
                    "text": { "type": "string" },
                    "keys": { "type": "string" },
                    "submit": { "type": "boolean" },
                    "clear": { "type": "boolean" },
                    "script": { "type": "string" },
                    "direction": { "type": "string", "enum": ["up", "down"] },
                    "tab": { "type": "string" },
                    "full_page": { "type": "boolean" },
                    "max_chars": { "type": "integer", "minimum": 200 },
                    "headless": { "type": "boolean" },
                    "timeout": { "type": "integer", "minimum": 1 }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "COM",
            description: "Automate Windows applications through COM (Windows or WSL only): Word, Excel, PowerPoint, Outlook, Access, Visio, and anything with a ProgID (Shell.Application, WScript.Shell, WbemScripting.SWbemLocator, Schedule.Service, …). Objects live in one PowerShell session under names you choose. Actions: `create` (object, prog_id, visible default true) starts an app; `attach` (object, prog_id) connects to a running one; `get` (object, path) reads a property, e.g. path `ActiveWorkbook.Name` or `ActiveSheet.Range(\"A1:C3\").Value2` (2-D values come back as rows); `set` (object, path, value; rows like [[1,\"a\"],[2,\"b\"]] become a 2-D array for ranges); `call` (object, path of a method, args), e.g. `Documents.Open` with [\"C:\\\\x.docx\"]; `inspect` (object, path) lists members and signatures, use it to learn an API; `objects`; `release` (object, quit); `progids` (filter) lists registered ProgIDs; `run` (script) runs PowerShell in the session, objects are $global:<name>. Use collection.Item(1) for indexes. Windows paths only.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["create", "attach", "get", "set", "call", "inspect", "objects", "release", "progids", "run"] },
                    "object": { "type": "string", "description": "Name of the object in the session, e.g. excel" },
                    "prog_id": { "type": "string" },
                    "path": { "type": "string" },
                    "value": {},
                    "args": { "type": "array" },
                    "visible": { "type": "boolean" },
                    "quit": { "type": "boolean" },
                    "script": { "type": "string" },
                    "filter": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1, "description": "Seconds (default 120)" }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "Download",
            description: "Save a file to the user's computer: an image or document from the web (https URL), or a copy of a local file (path or file://). By default it goes into ~/Downloads under the name the server or URL gives, never overwriting (a ` (1)` suffix is added). Use it when the user asks to download or save something you showed or found. Returns the saved path.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "https URL, file:// URL or local path of the file" },
                    "save_to": { "type": "string", "description": "Directory (ending in /) or file path to save to; default ~/Downloads" },
                    "name": { "type": "string", "description": "File name to use inside save_to" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "SocialMedia",
            description: "Save the images and videos of an Instagram, Facebook or Pinterest post, reel, video or pin, given its link (share and short links such as pin.it and fb.watch work), or of an Instagram profile's latest posts (its link, or `@username`; `count` posts, default 6). Uses each site's own web API, so there is no need to dig image addresses out of the page. Saves into ~/Downloads/claw-social/<platform> and returns the saved paths plus `show` lines (`![…](path)`) that display the images in your reply. Public posts work without login; for others a Netscape cookie file at ~/.claw/cookies/<platform>.txt is used when present.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Link to the post, reel, video or pin" },
                    "dest": { "type": "string", "description": "Directory to save into; once given, later calls in this session use it too. Default ~/Downloads/claw-social/<platform>" },
                    "count": { "type": "integer", "minimum": 1, "maximum": 24, "description": "Instagram profile: how many of its latest posts (default 6)" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "WebSearch",
            description: "Search the web for current information and return cited results.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2 },
                    "allowed_domains": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "blocked_domains": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TodoWrite",
            description: "Update the structured task list for the current session.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "activeForm": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["content", "activeForm", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "Skill",
            description: "Load a local skill definition and its instructions.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill": { "type": "string" },
                    "args": { "type": "string" }
                },
                "required": ["skill"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Agent",
            description: "Launch a sub-agent that works on a delegated task with its own tools and context, and return its final result. Give it a complete, self-contained prompt. subagent_type: Explore (read-only search), Plan (read-only planning), Verification (can run commands), fork (a copy of you that starts with this whole conversation, so its prompt only needs the task; for side work that needs the context), the name of a custom agent listed in the system prompt, or omit for a general-purpose agent that can edit files and run commands. Set run_in_background to true to keep working while it runs: you are notified with its result when it finishes, so do not poll or sleep for it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "Short (3-5 word) label for the task" },
                    "prompt": { "type": "string", "description": "The full task for the sub-agent" },
                    "subagent_type": { "type": "string" },
                    "name": { "type": "string" },
                    "model": { "type": "string", "description": "Model for the sub-agent; defaults to the current session's model" },
                    "run_in_background": { "type": "boolean", "description": "Return immediately; the result arrives as a notice when the agent finishes" }
                },
                "required": ["description", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "ToolSearch",
            description: "Search for deferred or specialized tools by exact name or keywords.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "NotebookEdit",
            description: "Replace, insert, or delete a cell in a Jupyter notebook.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "notebook_path": { "type": "string" },
                    "cell_id": { "type": "string" },
                    "new_source": { "type": "string" },
                    "cell_type": { "type": "string", "enum": ["code", "markdown"] },
                    "edit_mode": { "type": "string", "enum": ["replace", "insert", "delete"] }
                },
                "required": ["notebook_path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "Sleep",
            description: "Wait for a specified duration without holding a shell process.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "duration_ms": { "type": "integer", "minimum": 0 }
                },
                "required": ["duration_ms"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SendUserMessage",
            description: "Send a message to the user.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "attachments": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "status": {
                        "type": "string",
                        "enum": ["normal", "proactive"]
                    }
                },
                "required": ["message", "status"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Config",
            description: "Get or set Claude Code settings.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "setting": { "type": "string" },
                    "value": {
                        "type": ["string", "boolean", "number"]
                    }
                },
                "required": ["setting"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "EnterPlanMode",
            description: "Enable a worktree-local planning mode override and remember the previous local setting for ExitPlanMode.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "ExitPlanMode",
            description: "Restore or clear the worktree-local planning mode override created by EnterPlanMode.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "StructuredOutput",
            description: "Return structured output in the requested format.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": true
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "REPL",
            description: "Execute code in a REPL-like subprocess.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string" },
                    "language": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1 }
                },
                "required": ["code", "language"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "PowerShell",
            description: "Execute a PowerShell command with optional timeout.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Timeout in MILLISECONDS (120000 = 2 minutes). Omit it unless the command may hang."
                    },
                    "description": { "type": "string" },
                    "run_in_background": { "type": "boolean" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "AskUserQuestion",
            description: "Ask the user a question and wait for their answer. Use it when a decision is genuinely the user's (a choice between valid approaches, a preference, missing information) instead of guessing; don't use it for things you can find out yourself. Give 2-4 short options, put the one you recommend first and set `recommended: true` on it, and add a one-line `description` with the trade-off. The user can always type their own answer instead, so don't add an \"Other\" option. Set `multi_select: true` when several options can be chosen together (shown as checkboxes). Omit `options` to ask for free text.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The complete question, ending with a question mark."
                    },
                    "header": {
                        "type": "string",
                        "description": "Very short topic label shown before the question, e.g. \"Database\"."
                    },
                    "options": {
                        "type": "array",
                        "items": {
                            "anyOf": [
                                { "type": "string" },
                                {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string" },
                                        "description": { "type": "string" },
                                        "recommended": { "type": "boolean" }
                                    },
                                    "required": ["label"]
                                }
                            ]
                        }
                    },
                    "multi_select": {
                        "type": "boolean",
                        "description": "Let the user choose several options (checkboxes)."
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskCreate",
            description: "Create a background task that runs in a separate subprocess.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "description": { "type": "string" }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "RunTaskPacket",
            description: "Create a background task from a structured task packet.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "objective": { "type": "string" },
                    "scope": { "type": "string" },
                    "repo": { "type": "string" },
                    "branch_policy": { "type": "string" },
                    "acceptance_tests": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "commit_policy": { "type": "string" },
                    "reporting_contract": { "type": "string" },
                    "escalation_policy": { "type": "string" }
                },
                "required": [
                    "objective",
                    "scope",
                    "repo",
                    "branch_policy",
                    "acceptance_tests",
                    "commit_policy",
                    "reporting_contract",
                    "escalation_policy"
                ],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskGet",
            description: "Get the status and details of a background task by ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskList",
            description: "List everything running in the background with its status: tasks (TaskCreate), background shells and monitors (bash run_in_background, Monitor) and sub-agents (Agent run_in_background). Stop any of them with TaskStop, read output with TaskOutput, message an agent with SendMessage.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskStop",
            description: "Stop a running background task by ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskUpdate",
            description: "Send a message or update to a running background task.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["task_id", "message"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskOutput",
            description: "Retrieve the output produced by a background task.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WorkerCreate",
            description: "Create a coding worker boot session with trust-gate and prompt-delivery guards.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cwd": { "type": "string" },
                    "trusted_roots": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "auto_recover_prompt_misdelivery": { "type": "boolean" }
                },
                "required": ["cwd"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "WorkerGet",
            description: "Fetch the current worker boot state, last error, and event history.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" }
                },
                "required": ["worker_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WorkerObserve",
            description: "Feed a terminal snapshot into worker boot detection to resolve trust gates, ready handshakes, and prompt misdelivery.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" },
                    "screen_text": { "type": "string" }
                },
                "required": ["worker_id", "screen_text"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WorkerResolveTrust",
            description: "Resolve a detected trust prompt so worker boot can continue.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" }
                },
                "required": ["worker_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "WorkerAwaitReady",
            description: "Return the current ready-handshake verdict for a coding worker.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" }
                },
                "required": ["worker_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WorkerSendPrompt",
            description: "Send a task prompt only after the worker reaches ready_for_prompt; can replay a recovered prompt.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" },
                    "prompt": { "type": "string" },
                    "task_receipt": {
                        "type": "object",
                        "properties": {
                            "repo": { "type": "string" },
                            "task_kind": { "type": "string" },
                            "source_surface": { "type": "string" },
                            "expected_artifacts": {
                                "type": "array",
                                "items": { "type": "string" }
                            },
                            "objective_preview": { "type": "string" }
                        },
                        "required": ["repo", "task_kind", "source_surface", "objective_preview"],
                        "additionalProperties": false
                    }
                },
                "required": ["worker_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "WorkerRestart",
            description: "Restart worker boot state after a failed or stale startup.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" }
                },
                "required": ["worker_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "WorkerTerminate",
            description: "Terminate a worker and mark the lane finished from the control plane.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" }
                },
                "required": ["worker_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "WorkerObserveCompletion",
            description: "Report session completion to the worker, classifying finish_reason into Finished or Failed (provider-degraded). Use after the opencode session completes to advance the worker to its terminal state.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "worker_id": { "type": "string" },
                    "finish_reason": { "type": "string" },
                    "tokens_output": { "type": "integer", "minimum": 0 }
                },
                "required": ["worker_id", "finish_reason", "tokens_output"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TeamCreate",
            description: "Create a team of sub-agents for parallel task execution.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "prompt": { "type": "string" },
                                "description": { "type": "string" }
                            },
                            "required": ["prompt"]
                        }
                    }
                },
                "required": ["name", "tasks"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TeamDelete",
            description: "Delete a team and stop all its running tasks.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "team_id": { "type": "string" }
                },
                "required": ["team_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronCreate",
            description: "Schedule a prompt to run in this session: recurring (default) or once, at times given by a 5-field cron expression in local time (`*/10 * * * *`, `0 9 * * 1-5`, `@hourly`). It runs as a new turn when the session is idle at that time; tasks last as long as the session.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schedule": { "type": "string", "description": "Cron expression (also accepted as `cron`)." },
                    "cron": { "type": "string" },
                    "prompt": { "type": "string" },
                    "recurring": { "type": "boolean" },
                    "description": { "type": "string" }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "CronDelete",
            description: "Cancel a task scheduled with CronCreate, by its id.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cron_id": { "type": "string" }
                },
                "required": ["cron_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronList",
            description: "List all scheduled recurring tasks.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "LSP",
            description: "Code intelligence from a language server (rust-analyzer, pyright/pylsp, typescript-language-server, gopls, clangd, … whichever is installed; `lspServers` in settings adds others): jump to definitions and implementations, find references, hover for types and docs, list a file's or the workspace's symbols, walk call hierarchies, and get a file's errors and warnings. line and character are 1-based, as an editor shows them. The first request for a language starts its server, which may need a while to index.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "enum": ["goToDefinition", "findReferences", "hover", "documentSymbol", "workspaceSymbol", "goToImplementation", "prepareCallHierarchy", "incomingCalls", "outgoingCalls", "diagnostics"] },
                    "filePath": { "type": "string" },
                    "line": { "type": "integer", "minimum": 1 },
                    "character": { "type": "integer", "minimum": 1 },
                    "query": { "type": "string", "description": "For workspaceSymbol." }
                },
                "required": ["operation", "filePath"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ListMcpResources",
            description: "List available resources from connected MCP servers.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ReadMcpResource",
            description: "Read a specific resource from an MCP server by URI.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "McpAuth",
            description: "Authenticate with an MCP server that requires OAuth or credentials.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "required": ["server"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "RemoteTrigger",
            description: "Trigger a remote action or webhook endpoint.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                    "headers": { "type": "object" },
                    "body": { "type": "string" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "MCP",
            description: "Execute a tool provided by a connected MCP server.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "tool": { "type": "string" },
                    "arguments": { "type": "object" }
                },
                "required": ["server", "tool"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TestingPermission",
            description: "Test-only tool for verifying permission enforcement behavior.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string" }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "GitStatus",
            description: "Show the working tree status (branch, staged, unstaged, untracked). Equivalent to 'git status --short --branch'. Use this instead of running git status via bash to get structured, parseable output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "short": { "type": "boolean" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "GitDiff",
            description: "Show changes between commits, the index, and the working tree. Supports staged changes ('git diff --cached'), specific paths, commit ranges, and comparing two commits. Use this instead of running git diff via bash to get structured output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "staged": { "type": "boolean" },
                    "commit": { "type": "string" },
                    "commit2": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "GitLog",
            description: "Show commit history. Supports limiting count, filtering by author/date/path, and oneline format. Defaults to the last 20 commits. Use this instead of running git log via bash to get structured output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "count": { "type": "integer", "minimum": 1 },
                    "oneline": { "type": "boolean" },
                    "author": { "type": "string" },
                    "since": { "type": "string" },
                    "until": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "GitShow",
            description: "Show a commit, tag, or tree object. Use format to control output: patch (default) shows the full diff, stat shows a diffstat summary, and metadata shows commit info without the diff. Supports showing a specific file at a commit (commit:path) for patch/stat output. Use this instead of running git show via bash to get structured output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "commit": { "type": "string" },
                    "path": { "type": "string" },
                    "stat": { "type": "boolean" },
                    "format": { "type": "string", "enum": ["patch", "stat", "metadata"] },
                },
                "required": ["commit"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "GitBlame",
            description: "Show what revision and author last modified each line of a file. Supports line range filtering (start_line, end_line). Use this instead of running git blame via bash to get structured output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
    .into_iter()
    .chain(local_tools::specs())
    .chain(pty_tool::specs())
    .chain(code_search::specs())
    .chain(dev_tools::specs())
    .chain(hawkscan::specs())
    .chain(code_exec_specs())
    .chain(external_agent::specs())
    .chain(apply_patch::specs())
    .chain(media::specs())
    .chain(signals::specs())
    .chain(session_search::specs())
    .collect()
}

/// Check permission before executing a tool. Returns Err with denial reason if blocked.
pub fn enforce_permission_check(
    enforcer: &PermissionEnforcer,
    tool_name: &str,
    input: &Value,
) -> Result<(), String> {
    let input_str = serde_json::to_string(input).unwrap_or_default();
    let result = enforcer.check(tool_name, &input_str);

    match result {
        EnforcementResult::Allowed => Ok(()),
        EnforcementResult::Denied { reason, .. } => Err(reason),
    }
}

/// `input` with its values coerced to the types the built-in tool `name`
/// declares (see [`arg_coercion`]); other tools' input is returned as it is.
#[must_use]
pub fn coerce_tool_input(name: &str, input: &Value) -> Value {
    static SCHEMAS: std::sync::OnceLock<BTreeMap<&'static str, Value>> = std::sync::OnceLock::new();
    let schemas = SCHEMAS.get_or_init(|| {
        mvp_tool_specs()
            .into_iter()
            .map(|spec| (spec.name, spec.input_schema))
            .collect()
    });
    schemas.get(name).map_or_else(
        || input.clone(),
        |schema| arg_coercion::coerce_to_schema(input, schema),
    )
}

#[cfg(unix)]
fn code_exec_specs() -> Vec<ToolSpec> {
    code_exec::specs()
}

#[cfg(not(unix))]
fn code_exec_specs() -> Vec<ToolSpec> {
    Vec::new()
}

#[cfg(unix)]
fn code_exec_execute(name: &str, input: &Value) -> Option<Result<String, String>> {
    code_exec::execute(name, input)
}

#[cfg(not(unix))]
fn code_exec_execute(_name: &str, _input: &Value) -> Option<Result<String, String>> {
    None
}

pub fn execute_tool(name: &str, input: &Value) -> Result<String, String> {
    execute_tool_with_enforcer(None, name, input)
}

#[allow(clippy::too_many_lines)]
fn execute_tool_with_enforcer(
    enforcer: Option<&PermissionEnforcer>,
    name: &str,
    input: &Value,
) -> Result<String, String> {
    let input = &expand_home_in_paths(&coerce_tool_input(name, input));
    match name {
        "bash" => {
            // Parse input to get the command for permission classification
            let bash_input: BashCommandInput = from_value(input)?;
            let classified_mode = classify_bash_permission(&bash_input.command);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, classified_mode)?;
            run_bash(bash_input)
        }
        "read_file" => {
            let file_input: ReadFileInput = from_value(input)?;
            let required_mode = classify_read_path_permission(&file_input.path, false);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            // Images are shown to the model instead of refused as binary.
            let path = Path::new(&file_input.path);
            if is_image_path(path)
                && path.extension().is_some_and(|ext| ext != "svg")
                && path.is_file()
            {
                return media::image_result(path, None);
            }
            // Memories in use stay ranked as active.
            runtime::touch_memory(&file_input.path);
            run_read_file(file_input)
        }
        "write_file" => {
            let file_input: WriteFileInput = from_value(input)?;
            let required_mode = classify_file_path_permission(&file_input.path, true);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            let old = std::fs::read_to_string(&file_input.path).unwrap_or_default();
            let note = leangate::before_write(name, &file_input.path, &old, &file_input.content)?;
            let path = file_input.path.clone();
            let output = run_write_file(file_input)?;
            leangate::record_edit(&path);
            runtime::touch_memory(&path);
            Ok(leangate::annotate(output, note))
        }
        "write_docx" => {
            let docx_input: WriteDocxInput = from_value(input)?;
            let required_mode = classify_file_path_permission(&docx_input.path, true);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            run_write_docx(&docx_input)
        }
        "write_pptx" => {
            let pptx_input: WritePptxInput = from_value(input)?;
            let required_mode =
                std::iter::once(classify_file_path_permission(&pptx_input.path, true))
                    .chain(
                        pptx_input
                            .slides
                            .iter()
                            .filter_map(|slide| slide.image.as_deref())
                            .map(|image| classify_read_path_permission(image, false)),
                    )
                    .max()
                    .unwrap_or(PermissionMode::WorkspaceWrite);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            run_write_pptx(&pptx_input)
        }
        "write_xlsx" => {
            let xlsx_input: WriteXlsxInput = from_value(input)?;
            let required_mode = classify_file_path_permission(&xlsx_input.path, true);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            run_write_xlsx(&xlsx_input)
        }
        "apply_patch" => run_apply_patch(enforcer, input),
        "edit_file" => {
            let file_input: EditFileInput = from_value(input)?;
            let required_mode = classify_file_path_permission(&file_input.path, false);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            let note = leangate::before_write(
                name,
                &file_input.path,
                &file_input.old_string,
                &file_input.new_string,
            )?;
            let path = file_input.path.clone();
            let output = run_edit_file(file_input)?;
            leangate::record_edit(&path);
            runtime::touch_memory(&path);
            Ok(leangate::annotate(output, note))
        }
        "glob_search" => {
            let glob_input: GlobSearchInputValue = from_value(input)?;
            let required_mode = classify_glob_permission(&glob_input);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            run_glob_search(glob_input)
        }
        "grep_search" => {
            let grep_input: GrepSearchInput = from_value(input)?;
            let required_mode = classify_grep_permission(&grep_input);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, required_mode)?;
            run_grep_search(grep_input)
        }
        "WebFetch" => {
            let web_input = from_value::<WebFetchInput>(input)?;
            maybe_enforce_permission_check_with_mode(
                enforcer,
                name,
                input,
                PermissionMode::DangerFullAccess,
            )?;
            run_web_fetch(web_input)
        }
        "X11" => {
            let x11_input = from_value::<x11::X11Input>(input)?;
            maybe_enforce_permission_check_with_mode(
                enforcer,
                name,
                input,
                PermissionMode::DangerFullAccess,
            )?;
            x11::run(x11_input).and_then(x11::render)
        }
        "Browser" => {
            let browser_input = from_value::<browser::BrowserInput>(input)?;
            maybe_enforce_permission_check_with_mode(
                enforcer,
                name,
                input,
                PermissionMode::DangerFullAccess,
            )?;
            browser::run(browser_input).and_then(browser::render)
        }
        "COM" => {
            let com_input = from_value::<com::ComInput>(input)?;
            maybe_enforce_permission_check_with_mode(
                enforcer,
                name,
                input,
                PermissionMode::DangerFullAccess,
            )?;
            com::run(com_input).and_then(to_pretty_json)
        }
        "SocialMedia" => {
            let social_input = from_value::<social::SocialMediaInput>(input)?;
            maybe_enforce_permission_check_with_mode(
                enforcer,
                name,
                input,
                PermissionMode::DangerFullAccess,
            )?;
            social::run(social_input).and_then(to_pretty_json)
        }
        "Download" => {
            let download_input = from_value::<download::DownloadInput>(input)?;
            maybe_enforce_permission_check_with_mode(
                enforcer,
                name,
                input,
                PermissionMode::DangerFullAccess,
            )?;
            download::run(download_input).and_then(to_pretty_json)
        }
        "WebSearch" => {
            let web_input = from_value::<WebSearchInput>(input)?;
            maybe_enforce_permission_check_with_mode(
                enforcer,
                name,
                input,
                PermissionMode::DangerFullAccess,
            )?;
            run_web_search(web_input)
        }
        "TodoWrite" => from_value::<TodoWriteInput>(input).and_then(run_todo_write),
        "Skill" => from_value::<SkillInput>(input).and_then(run_skill),
        "Agent" => from_value::<AgentInput>(input).and_then(run_agent),
        "ToolSearch" => from_value::<ToolSearchInput>(input).and_then(run_tool_search),
        "NotebookEdit" => from_value::<NotebookEditInput>(input).and_then(run_notebook_edit),
        "Sleep" => from_value::<SleepInput>(input).and_then(run_sleep),
        "SendUserMessage" | "Brief" => from_value::<BriefInput>(input).and_then(run_brief),
        "Config" => from_value::<ConfigInput>(input).and_then(run_config),
        "EnterPlanMode" => from_value::<EnterPlanModeInput>(input).and_then(run_enter_plan_mode),
        "ExitPlanMode" => from_value::<ExitPlanModeInput>(input).and_then(run_exit_plan_mode),
        "StructuredOutput" => {
            from_value::<StructuredOutputInput>(input).and_then(run_structured_output)
        }
        "REPL" => from_value::<ReplInput>(input).and_then(run_repl),
        "PowerShell" => {
            // Parse input to get the command for permission classification
            let ps_input: PowerShellInput = from_value(input)?;
            let classified_mode = classify_powershell_permission(&ps_input.command);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, classified_mode)?;
            run_powershell(ps_input)
        }
        "AskUserQuestion" => {
            from_value::<AskUserQuestionInput>(input).and_then(run_ask_user_question)
        }
        "TaskCreate" => from_value::<TaskCreateInput>(input).and_then(run_task_create),
        "RunTaskPacket" => from_value::<TaskPacket>(input).and_then(run_task_packet),
        "TaskGet" => from_value::<TaskIdInput>(input).and_then(run_task_get),
        "TaskList" => run_task_list(input.clone()),
        "TaskStop" => from_value::<TaskIdInput>(input).and_then(run_task_stop),
        "TaskUpdate" => from_value::<TaskUpdateInput>(input).and_then(run_task_update),
        "TaskOutput" => from_value::<TaskIdInput>(input).and_then(run_task_output),
        "BashOutput" => from_value::<BashOutputInput>(input).and_then(run_bash_output),
        "KillShell" => from_value::<KillShellInput>(input).and_then(run_kill_shell),
        "WorkerCreate" => from_value::<WorkerCreateInput>(input).and_then(run_worker_create),
        "WorkerGet" => from_value::<WorkerIdInput>(input).and_then(run_worker_get),
        "WorkerObserve" => from_value::<WorkerObserveInput>(input).and_then(run_worker_observe),
        "WorkerResolveTrust" => {
            from_value::<WorkerIdInput>(input).and_then(run_worker_resolve_trust)
        }
        "WorkerAwaitReady" => from_value::<WorkerIdInput>(input).and_then(run_worker_await_ready),
        "WorkerSendPrompt" => {
            from_value::<WorkerSendPromptInput>(input).and_then(run_worker_send_prompt)
        }
        "WorkerRestart" => from_value::<WorkerIdInput>(input).and_then(run_worker_restart),
        "WorkerTerminate" => from_value::<WorkerIdInput>(input).and_then(run_worker_terminate),
        "WorkerObserveCompletion" => from_value::<WorkerObserveCompletionInput>(input)
            .and_then(run_worker_observe_completion),
        "TeamCreate" => from_value::<TeamCreateInput>(input).and_then(run_team_create),
        "TeamDelete" => from_value::<TeamDeleteInput>(input).and_then(run_team_delete),
        "CronCreate" => from_value::<CronCreateInput>(input).and_then(run_cron_create),
        "CronDelete" => from_value::<CronDeleteInput>(input).and_then(run_cron_delete),
        "CronList" => run_cron_list(input.clone()),
        "LSP" => from_value::<lsp_tool::LspInput>(input)
            .and_then(lsp_tool::run)
            .and_then(to_pretty_json),
        "ListMcpResources" => {
            from_value::<McpResourceInput>(input).and_then(run_list_mcp_resources)
        }
        "ReadMcpResource" => from_value::<McpResourceInput>(input).and_then(run_read_mcp_resource),
        "McpAuth" => from_value::<McpAuthInput>(input).and_then(run_mcp_auth),
        "RemoteTrigger" => from_value::<RemoteTriggerInput>(input).and_then(run_remote_trigger),
        "MCP" => from_value::<McpToolInput>(input).and_then(run_mcp_tool),
        "TestingPermission" => {
            from_value::<TestingPermissionInput>(input).and_then(run_testing_permission)
        }
        "GitStatus" => from_value::<GitStatusInput>(input).and_then(run_git_status),
        "GitDiff" => from_value::<GitDiffInput>(input).and_then(run_git_diff),
        "GitLog" => from_value::<GitLogInput>(input).and_then(run_git_log),
        "GitShow" => from_value::<GitShowInput>(input).and_then(run_git_show),
        "GitBlame" => from_value::<GitBlameInput>(input).and_then(run_git_blame),
        _ => local_tools::execute(name, input)
            .or_else(|| pty_tool::execute(name, input))
            .or_else(|| code_search::execute(name, input))
            .or_else(|| dev_tools::execute(name, input))
            .or_else(|| hawkscan::execute(name, input))
            .or_else(|| code_exec_execute(name, input))
            .or_else(|| external_agent::execute(name, input))
            .or_else(|| media::execute(name, input))
            .or_else(|| signals::execute(name, input))
            .or_else(|| session_search::execute(name, input))
            .unwrap_or_else(|| Err(format!("unsupported tool: {name}"))),
    }
}

/// Enforce permission check with a dynamically classified permission mode.
/// Used for tools like bash and `PowerShell` where the required permission
/// depends on the actual command being executed.
fn maybe_enforce_permission_check_with_mode(
    enforcer: Option<&PermissionEnforcer>,
    tool_name: &str,
    input: &Value,
    required_mode: PermissionMode,
) -> Result<(), String> {
    if let Some(enforcer) = enforcer {
        let input_str = serde_json::to_string(input).unwrap_or_default();
        let result = enforcer.check_with_required_mode(tool_name, &input_str, required_mode);

        match result {
            EnforcementResult::Allowed => Ok(()),
            EnforcementResult::Denied { reason, .. } => Err(reason),
        }
    } else {
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_ask_user_question(input: AskUserQuestionInput) -> Result<String, String> {
    use std::io::{self, BufRead, Write};

    // Without a terminal nobody can answer; don't block on stdin.
    if !io::IsTerminal::is_terminal(&io::stdin()) {
        return to_pretty_json(json!({
            "question": input.question,
            "answer": null,
            "status": "unavailable",
            "note": "No interactive user is attached. Choose the option you would recommend, say so, and continue."
        }));
    }
    // The Esc watcher must not swallow the user's answer.
    let _keys_paused = runtime::turn_keys::pause();
    let options = input.options.as_ref().map(|options| {
        options
            .iter()
            .map(|option| match option {
                Value::String(label) => label.clone(),
                other => other
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect::<Vec<_>>()
    });

    // Display the question to the user via stdout
    let stdout = io::stdout();
    let stdin = io::stdin();
    let mut out = stdout.lock();

    writeln!(out, "\n[Question] {}", input.question).map_err(|e| e.to_string())?;

    if let Some(ref options) = options {
        for (i, option) in options.iter().enumerate() {
            writeln!(out, "  {}. {}", i + 1, option).map_err(|e| e.to_string())?;
        }
        write!(out, "Enter choice (1-{}): ", options.len()).map_err(|e| e.to_string())?;
    } else {
        write!(out, "Your answer: ").map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;

    // Read user response from stdin
    let mut response = String::new();
    stdin
        .lock()
        .read_line(&mut response)
        .map_err(|e| e.to_string())?;
    let response = response.trim().to_string();

    // If options were provided, resolve the numeric choice
    let answer = if let Some(ref options) = options {
        if let Ok(idx) = response.parse::<usize>() {
            if idx >= 1 && idx <= options.len() {
                options[idx - 1].clone()
            } else {
                response.clone()
            }
        } else {
            response.clone()
        }
    } else {
        response.clone()
    };

    to_pretty_json(json!({
        "question": input.question,
        "answer": answer,
        "status": "answered"
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_create(input: TaskCreateInput) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry.create(&input.prompt, input.description.as_deref());
    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "task_packet": task.task_packet,
        "created_at": task.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_packet(input: TaskPacket) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry
        .create_from_packet(input)
        .map_err(|error| error.to_string())?;

    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "task_packet": task.task_packet,
        "created_at": task.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_get(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.get(&input.task_id) {
        Some(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "prompt": task.prompt,
            "description": task.description,
            "task_packet": task.task_packet,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "messages": task.messages,
            "team_id": task.team_id
        })),
        None => Err(format!("task not found: {}", input.task_id)),
    }
}

fn run_task_list(_input: Value) -> Result<String, String> {
    let registry = global_task_registry();
    let tasks: Vec<_> = registry
        .list(None)
        .into_iter()
        .map(|t| {
            json!({
                "task_id": t.task_id,
                "status": t.status,
                "prompt": t.prompt,
                "description": t.description,
                "task_packet": t.task_packet,
                "created_at": t.created_at,
                "updated_at": t.updated_at,
                "team_id": t.team_id
            })
        })
        .collect();
    // Background shells, monitors and sub-agents are tasks too: TaskStop,
    // TaskOutput and SendMessage take their ids.
    let background: Vec<_> = runtime::global_background_jobs()
        .list()
        .into_iter()
        .map(|job| {
            json!({
                "task_id": job.id,
                "status": job.state.to_string(),
                "command": job.command,
                "elapsed_secs": job.elapsed_secs,
            })
        })
        .collect();
    let agents = local_tools::agents::rows();
    to_pretty_json(json!({
        "count": tasks.len() + background.len(),
        "tasks": tasks,
        "background": background,
        "agents": agents,
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_stop(input: TaskIdInput) -> Result<String, String> {
    // Background shells and monitors are tasks too (Claude Code's TaskStop
    // stops whatever background work has the id).
    if let Ok(read) = runtime::global_background_jobs().kill(&input.task_id) {
        return to_pretty_json(json!({
            "task_id": read.id,
            "status": read.state.to_string(),
            "message": "Background task stopped",
            "output": read.output,
        }));
    }
    let registry = global_task_registry();
    match registry.stop(&input.task_id) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message": "Task stopped"
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_update(input: TaskUpdateInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.update(&input.task_id, &input.message) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message_count": task.messages.len(),
            "last_message": input.message
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_output(input: TaskIdInput) -> Result<String, String> {
    if let Ok(read) = runtime::global_background_jobs().read_new(&input.task_id, Duration::ZERO) {
        return to_pretty_json(json!({
            "task_id": read.id,
            "status": read.state.to_string(),
            "output": read.output,
            "has_output": !read.output.is_empty()
        }));
    }
    let registry = global_task_registry();
    match registry.output(&input.task_id) {
        Ok(output) => to_pretty_json(json!({
            "task_id": input.task_id,
            "output": output,
            "has_output": !output.is_empty()
        })),
        Err(e) => Err(e),
    }
}

const MAX_BASH_OUTPUT_CHARS: usize = 30_000;

fn render_job_read(read: &runtime::JobRead, max_chars: usize) -> String {
    let output = read.output.trim_end();
    let output = if output.chars().count() > max_chars {
        let head: String = output.chars().take(max_chars / 2).collect();
        let tail: String = {
            let chars: Vec<char> = output.chars().collect();
            chars[chars.len() - max_chars / 2..].iter().collect()
        };
        format!("{head}\n... [output truncated] ...\n{tail}")
    } else if output.is_empty() {
        "(no new output)".to_string()
    } else {
        output.to_string()
    };
    let dropped = if read.dropped_bytes > 0 {
        format!(" ({} earlier bytes discarded)", read.dropped_bytes)
    } else {
        String::new()
    };
    format!("[{}: {}{dropped}]\n{output}", read.id, read.state)
}

#[allow(clippy::needless_pass_by_value)]
fn run_bash_output(input: BashOutputInput) -> Result<String, String> {
    let wait = std::time::Duration::from_secs(input.wait_seconds.unwrap_or(0).min(120));
    let read = runtime::global_background_jobs().read_new(&input.bash_id, wait)?;
    Ok(render_job_read(&read, MAX_BASH_OUTPUT_CHARS))
}

#[allow(clippy::needless_pass_by_value)]
fn run_kill_shell(input: KillShellInput) -> Result<String, String> {
    let read = runtime::global_background_jobs().kill(&input.shell_id)?;
    Ok(render_job_read(&read, 4_000))
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_create(input: WorkerCreateInput) -> Result<String, String> {
    // Merge config-level trusted_roots with per-call overrides.
    // Config provides the default allowlist; per-call roots add on top.
    let merged_roots: Vec<String> = ConfigLoader::default_for(&input.cwd)
        .load()
        .ok()
        .map_or_else(
            || input.trusted_roots.clone(),
            |config| config.trusted_roots_with_overrides(&input.trusted_roots),
        );
    let worker = global_worker_registry().create(
        &input.cwd,
        &merged_roots,
        input.auto_recover_prompt_misdelivery,
    );
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_get(input: WorkerIdInput) -> Result<String, String> {
    global_worker_registry().get(&input.worker_id).map_or_else(
        || Err(format!("worker not found: {}", input.worker_id)),
        to_pretty_json,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_observe(input: WorkerObserveInput) -> Result<String, String> {
    let worker = global_worker_registry().observe(&input.worker_id, &input.screen_text)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_resolve_trust(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().resolve_trust(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_await_ready(input: WorkerIdInput) -> Result<String, String> {
    let snapshot: WorkerReadySnapshot = global_worker_registry().await_ready(&input.worker_id)?;
    to_pretty_json(snapshot)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_send_prompt(input: WorkerSendPromptInput) -> Result<String, String> {
    let worker = global_worker_registry().send_prompt(
        &input.worker_id,
        input.prompt.as_deref(),
        input.task_receipt,
    )?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_restart(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().restart(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_terminate(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().terminate(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_worker_observe_completion(input: WorkerObserveCompletionInput) -> Result<String, String> {
    let worker = global_worker_registry().observe_completion(
        &input.worker_id,
        &input.finish_reason,
        input.tokens_output,
    )?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
fn run_team_create(input: TeamCreateInput) -> Result<String, String> {
    let task_ids: Vec<String> = input
        .tasks
        .iter()
        .filter_map(|t| t.get("task_id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    let team = global_team_registry().create(&input.name, task_ids);
    // Register team assignment on each task
    for task_id in &team.task_ids {
        let _ = global_task_registry().assign_team(task_id, &team.team_id);
    }
    to_pretty_json(json!({
        "team_id": team.team_id,
        "name": team.name,
        "task_count": team.task_ids.len(),
        "task_ids": team.task_ids,
        "status": team.status,
        "created_at": team.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_team_delete(input: TeamDeleteInput) -> Result<String, String> {
    match global_team_registry().delete(&input.team_id) {
        Ok(team) => to_pretty_json(json!({
            "team_id": team.team_id,
            "name": team.name,
            "status": team.status,
            "message": "Team deleted"
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_create(input: CronCreateInput) -> Result<String, String> {
    let task = runtime::scheduler::global_scheduler().add_cron(
        &input.schedule,
        &input.prompt,
        input.recurring,
    )?;
    to_pretty_json(json!({
        "cron_id": task.id,
        "schedule": task.expression,
        "prompt": task.prompt,
        "description": input.description,
        "recurring": task.recurring,
        "next_run": task.next_run.to_rfc3339(),
        "message": "Scheduled. It runs when the session is idle at that time (local time), for as long as this session runs.",
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_delete(input: CronDeleteInput) -> Result<String, String> {
    let task = runtime::scheduler::global_scheduler().delete(&input.cron_id)?;
    to_pretty_json(json!({
        "cron_id": task.id,
        "schedule": task.expression,
        "status": "deleted",
        "message": "Scheduled task removed"
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_list(_input: Value) -> Result<String, String> {
    let entries: Vec<_> = runtime::scheduler::global_scheduler()
        .list()
        .into_iter()
        .map(|task| {
            json!({
                "cron_id": task.id,
                "schedule": task.expression,
                "prompt": task.prompt,
                "recurring": task.recurring,
                "run_count": task.runs,
                "next_run": task.next_run.to_rfc3339(),
            })
        })
        .collect();
    to_pretty_json(json!({
        "crons": entries,
        "count": entries.len()
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_list_mcp_resources(input: McpResourceInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let server = input.server.as_deref().unwrap_or("default");
    match registry.list_resources(server) {
        Ok(resources) => {
            let items: Vec<_> = resources
                .iter()
                .map(|r| {
                    json!({
                        "uri": r.uri,
                        "name": r.name,
                        "description": r.description,
                        "mime_type": r.mime_type,
                    })
                })
                .collect();
            to_pretty_json(json!({
                "server": server,
                "resources": items,
                "count": items.len()
            }))
        }
        Err(e) => to_pretty_json(json!({
            "server": server,
            "resources": [],
            "error": e
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_mcp_resource(input: McpResourceInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let uri = input.uri.as_deref().unwrap_or("");
    let server = input.server.as_deref().unwrap_or("default");
    match registry.read_resource(server, uri) {
        Ok(resource) => to_pretty_json(json!({
            "server": server,
            "uri": resource.uri,
            "name": resource.name,
            "description": resource.description,
            "mime_type": resource.mime_type
        })),
        Err(e) => to_pretty_json(json!({
            "server": server,
            "uri": uri,
            "error": e
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_auth(input: McpAuthInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    match registry.get_server(&input.server) {
        Some(state) => to_pretty_json(json!({
            "server": input.server,
            "status": state.status,
            "server_info": state.server_info,
            "tool_count": state.tools.len(),
            "resource_count": state.resources.len()
        })),
        None => to_pretty_json(json!({
            "server": input.server,
            "status": "disconnected",
            "message": "Server not registered. Use MCP tool to connect first."
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_remote_trigger(input: RemoteTriggerInput) -> Result<String, String> {
    let method = input.method.unwrap_or_else(|| "GET".to_string());
    let client = Client::new();

    let mut request = match method.to_uppercase().as_str() {
        "GET" => client.get(&input.url),
        "POST" => client.post(&input.url),
        "PUT" => client.put(&input.url),
        "DELETE" => client.delete(&input.url),
        "PATCH" => client.patch(&input.url),
        "HEAD" => client.head(&input.url),
        other => return Err(format!("unsupported HTTP method: {other}")),
    };

    // Apply custom headers
    if let Some(ref headers) = input.headers {
        if let Some(obj) = headers.as_object() {
            for (key, value) in obj {
                if let Some(val) = value.as_str() {
                    request = request.header(key.as_str(), val);
                }
            }
        }
    }

    // Apply body
    if let Some(ref body) = input.body {
        request = request.body(body.clone());
    }

    // Execute with a 30-second timeout
    let request = request.timeout(Duration::from_secs(30));

    match request.send() {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = response.text().unwrap_or_default();
            let truncated_body = if body.len() > 8192 {
                format!(
                    "{}\n\n[response truncated — {} bytes total]",
                    &body[..8192],
                    body.len()
                )
            } else {
                body
            };
            to_pretty_json(json!({
                "url": input.url,
                "method": method,
                "status_code": status,
                "body": truncated_body,
                "success": (200..300).contains(&status)
            }))
        }
        Err(e) => to_pretty_json(json!({
            "url": input.url,
            "method": method,
            "error": e.to_string(),
            "success": false
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_tool(input: McpToolInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let args = input.arguments.unwrap_or(serde_json::json!({}));
    match registry.call_tool(&input.server, &input.tool, &args) {
        Ok(result) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "result": result,
            "status": "success"
        })),
        Err(e) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "error": e,
            "status": "error"
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_testing_permission(input: TestingPermissionInput) -> Result<String, String> {
    to_pretty_json(json!({
        "action": input.action,
        "permitted": true,
        "message": "Testing permission tool stub"
    }))
}

#[allow(clippy::needless_pass_by_value)]
/// Execute `git status --short --branch` and return structured JSON output.
/// Falls back to full `git status` if `short` is explicitly set to false.
fn run_git_status(input: GitStatusInput) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["status"];
    if input.short.unwrap_or(true) {
        args.push("--short");
        args.push("--branch");
    }
    match git_stdout(&args) {
        Some(output) => to_pretty_json(json!({
            "output": output
        })),
        None => Err(
            "git status failed. Ensure the current directory is inside a git repository."
                .to_string(),
        ),
    }
}

#[allow(clippy::needless_pass_by_value)]
/// Execute `git diff` with optional --cached, commit, and path filters.
/// Returns the diff output wrapped in a JSON object.
fn run_git_diff(input: GitDiffInput) -> Result<String, String> {
    let mut args: Vec<String> = vec!["diff".to_string()];
    if input.staged.unwrap_or(false) {
        args.push("--cached".to_string());
    }
    if let Some(ref commit) = input.commit {
        if let Some(ref commit2) = input.commit2 {
            args.push(format!("{commit}...{commit2}"));
        } else {
            args.push(commit.clone());
        }
    }
    if let Some(ref path) = input.path {
        args.push("--".to_string());
        args.push(path.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match git_stdout(&arg_refs) {
        Some(output) => to_pretty_json(json!({
            "output": output
        })),
        None => Err(
            "git diff failed. Ensure the current directory is inside a git repository.".to_string(),
        ),
    }
}

#[allow(clippy::needless_pass_by_value)]
/// Execute `git log` with count, author, date, and path filters.
/// Defaults to the last 20 commits.
fn run_git_log(input: GitLogInput) -> Result<String, String> {
    let mut args: Vec<String> = vec!["log".to_string()];
    let count = input.count.unwrap_or(20);
    args.push(format!("-n{count}"));
    if input.oneline.unwrap_or(false) {
        args.push("--oneline".to_string());
    }
    if let Some(ref author) = input.author {
        args.push(format!("--author={author}"));
    }
    if let Some(ref since) = input.since {
        args.push(format!("--since={since}"));
    }
    if let Some(ref until) = input.until {
        args.push(format!("--until={until}"));
    }
    if let Some(ref path) = input.path {
        args.push("--".to_string());
        args.push(path.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match git_stdout(&arg_refs) {
        Some(output) => to_pretty_json(json!({
            "output": output
        })),
        None => Err(
            "git log failed. Ensure the current directory is inside a git repository.".to_string(),
        ),
    }
}

/// Execute `git show` for a given commit, optionally with --stat or a file path.
/// Uses the `commit:path` syntax when a path is specified.
fn run_git_show(input: GitShowInput) -> Result<String, String> {
    let mut args: Vec<String> = vec!["show".to_string()];

    match input.format.as_deref() {
        Some("metadata") if input.path.is_some() => {
            return Err(
                "GitShow format \"metadata\" cannot be combined with path; metadata describes a commit, not a blob. Use format \"patch\" or \"stat\" with path, or omit path."
                    .to_string(),
            );
        }
        Some("metadata") => {
            args.push("--format=medium".to_string());
            args.push("--no-patch".to_string());
        }
        Some("stat") => {
            args.push("--stat".to_string());
        }
        Some("patch") | None => {
            if input.format.is_none() && input.stat.unwrap_or(false) {
                args.push("--stat".to_string());
            }
        }
        Some(other) => {
            return Err(format!(
                "unknown GitShow format: \"{other}\". Supported values: \"patch\" (default), \"stat\", \"metadata\"."
            ));
        }
    }

    if let Some(ref path) = input.path {
        args.push(format!("{}:{}", input.commit, path));
    } else {
        args.push(input.commit.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match git_stdout(&arg_refs) {
        Some(output) => to_pretty_json(json!({
            "output": output
        })),
        None => Err(format!(
            "git show {} failed. Ensure the commit exists.",
            input.commit
        )),
    }
}

#[allow(clippy::needless_pass_by_value)]
/// Execute `git blame` on a file, optionally restricted to a line range.
fn run_git_blame(input: GitBlameInput) -> Result<String, String> {
    let mut args: Vec<String> = vec!["blame".to_string()];
    if let (Some(start), Some(end)) = (input.start_line, input.end_line) {
        args.push(format!("-L{start},{end}"));
    }
    args.push(input.path.clone());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match git_stdout(&arg_refs) {
        Some(output) => to_pretty_json(json!({
            "output": output
        })),
        None => Err(format!("git blame {} failed. Ensure the file exists and the directory is inside a git repository.", input.path)),
    }
}

fn from_value<T: for<'de> Deserialize<'de>>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|error| error.to_string())
}

/// Classify bash command permission based on command type and path.
/// ROADMAP #50: Read-only commands targeting CWD paths get `WorkspaceWrite`,
/// all others remain `DangerFullAccess`.
fn classify_bash_permission(command: &str) -> PermissionMode {
    // Read-only commands that are safe when targeting workspace paths
    const READ_ONLY_COMMANDS: &[&str] = &[
        "cat", "head", "tail", "less", "more", "ls", "ll", "dir", "find", "test", "[", "[[",
        "grep", "rg", "awk", "sed", "file", "stat", "readlink", "wc", "sort", "uniq", "cut", "tr",
        "pwd", "echo", "printf",
    ];

    // Get the base command (first word before any args or pipes)
    let base_cmd = command.split_whitespace().next().unwrap_or("");
    let base_cmd = base_cmd.split('|').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split(';').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split('>').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split('<').next().unwrap_or("").trim();

    // Check if it's a read-only command
    let cmd_name = base_cmd.split('/').next_back().unwrap_or(base_cmd);
    let is_read_only = READ_ONLY_COMMANDS.contains(&cmd_name);

    if !is_read_only {
        return PermissionMode::DangerFullAccess;
    }

    // Check if any path argument is outside workspace
    // Simple heuristic: check for absolute paths not starting with CWD
    if has_dangerous_paths(command) {
        return PermissionMode::DangerFullAccess;
    }

    PermissionMode::WorkspaceWrite
}

/// Check if command has dangerous paths (outside workspace).
fn has_dangerous_paths(command: &str) -> bool {
    // Look for absolute paths
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let cwd = std::env::current_dir()
        .ok()
        .map(|cwd| cwd.canonicalize().unwrap_or(cwd));

    for token in tokens {
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
            )
        });
        // Skip flags/options
        if token.starts_with('-') {
            continue;
        }

        if token.contains('$') {
            return true;
        }

        if looks_like_windows_absolute_path(token) {
            return true;
        }

        // Check for absolute paths
        if token.starts_with('/') || token.starts_with("~/") {
            // Check if it's within CWD
            let path =
                PathBuf::from(token.replace('~', &std::env::var("HOME").unwrap_or_default()));
            if let Some(cwd) = cwd.as_ref() {
                let resolved = path.canonicalize().unwrap_or(path);
                if !resolved.starts_with(cwd) {
                    return true; // Path outside workspace
                }
            }
        }

        // Check for parent directory traversal that escapes workspace
        if token.contains("../..") || token.starts_with("../") && !token.starts_with("./") {
            return true;
        }

        if let Some(cwd) = cwd.as_ref() {
            if token.starts_with('.') || token.contains('/') || Path::new(token).exists() {
                let candidate = if Path::new(token).is_absolute() {
                    PathBuf::from(token)
                } else {
                    cwd.join(token)
                };
                if let Ok(canonical) = candidate.canonicalize() {
                    if !canonical.starts_with(cwd) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn looks_like_windows_absolute_path(token: &str) -> bool {
    let bytes = token.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\'))
        || token.starts_with(r"\\")
}

fn run_bash(input: BashCommandInput) -> Result<String, String> {
    if let Some(output) = workspace_test_branch_preflight(&input.command) {
        return serde_json::to_string_pretty(&output).map_err(|error| error.to_string());
    }
    serde_json::to_string_pretty(&execute_bash(input).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())
}

fn workspace_test_branch_preflight(command: &str) -> Option<BashCommandOutput> {
    if !is_workspace_test_command(command) {
        return None;
    }

    let branch = git_stdout(&["branch", "--show-current"])?;
    let main_ref = resolve_main_ref(&branch)?;
    let freshness = check_freshness(&branch, &main_ref);
    match freshness {
        BranchFreshness::Fresh => None,
        BranchFreshness::Stale {
            commits_behind,
            missing_fixes,
        } => Some(branch_divergence_output(
            command,
            &branch,
            &main_ref,
            commits_behind,
            None,
            &missing_fixes,
        )),
        BranchFreshness::Diverged {
            ahead,
            behind,
            missing_fixes,
        } => Some(branch_divergence_output(
            command,
            &branch,
            &main_ref,
            behind,
            Some(ahead),
            &missing_fixes,
        )),
    }
}

fn is_workspace_test_command(command: &str) -> bool {
    let normalized = normalize_shell_command(command);
    [
        "cargo test --workspace",
        "cargo test --all",
        "cargo nextest run --workspace",
        "cargo nextest run --all",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn normalize_shell_command(command: &str) -> String {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn resolve_main_ref(branch: &str) -> Option<String> {
    let has_local_main = git_ref_exists("main");
    let has_remote_main = git_ref_exists("origin/main");

    if branch == "main" && has_remote_main {
        Some("origin/main".to_string())
    } else if has_local_main {
        Some("main".to_string())
    } else if has_remote_main {
        Some("origin/main".to_string())
    } else {
        None
    }
}

fn git_ref_exists(reference: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn git_stdout(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

fn branch_divergence_output(
    command: &str,
    branch: &str,
    main_ref: &str,
    commits_behind: usize,
    commits_ahead: Option<usize>,
    missing_fixes: &[String],
) -> BashCommandOutput {
    let relation = commits_ahead.map_or_else(
        || format!("is {commits_behind} commit(s) behind"),
        |ahead| format!("has diverged ({ahead} ahead, {commits_behind} behind)"),
    );
    let missing_summary = if missing_fixes.is_empty() {
        "(none surfaced)".to_string()
    } else {
        missing_fixes.join("; ")
    };
    let stderr = format!(
        "branch divergence detected before workspace tests: `{branch}` {relation} `{main_ref}`. Missing commits: {missing_summary}. Merge or rebase `{main_ref}` before re-running `{command}`."
    );

    BashCommandOutput {
        stdout: String::new(),
        stderr: stderr.clone(),
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: None,
        return_code_interpretation: Some("preflight_blocked:branch_divergence".to_string()),
        no_output_expected: Some(false),
        structured_content: Some(vec![serde_json::to_value(
            LaneEvent::new(
                LaneEventName::BranchStaleAgainstMain,
                LaneEventStatus::Blocked,
                iso8601_now(),
            )
            .with_failure_class(LaneFailureClass::BranchDivergence)
            .with_detail(stderr.clone())
            .with_data(json!({
                "branch": branch,
                "mainRef": main_ref,
                "commitsBehind": commits_behind,
                "commitsAhead": commits_ahead,
                "missingCommits": missing_fixes,
                "blockedCommand": command,
                "recommendedAction": format!("merge or rebase {main_ref} before workspace tests")
            })),
        )
        .expect("lane event should serialize")]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_file(input: ReadFileInput) -> Result<String, String> {
    let workspace = tool_workspace(&input.path, false)?;
    if let Some(output) = read_document_file(&input, &workspace)? {
        note_file_seen(&input.path);
        return to_pretty_json(output);
    }
    let output = read_file_in_workspace(&input.path, input.offset, input.limit, &workspace)
        .map_err(io_to_string)?;
    note_file_seen(&input.path);
    to_pretty_json(output)
}

/// Largest document `read_file` will open.
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

/// Canonical form of a document path, rejecting paths outside `workspace`.
fn document_path_in_workspace(
    path: &str,
    workspace: &Path,
    allow_missing: bool,
) -> Result<PathBuf, String> {
    let absolute = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path)
    };
    let resolved = match absolute.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) if !allow_missing => return Err(format!("{}: {error}", absolute.display())),
        Err(_) => {
            let name = absolute
                .file_name()
                .ok_or_else(|| format!("invalid path: {path}"))?;
            absolute
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .map_or_else(|| absolute.clone(), |parent| parent.join(name))
        }
    };
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if !resolved.starts_with(&root) {
        return Err(format!(
            "path {} escapes workspace boundary {}",
            resolved.display(),
            root.display()
        ));
    }
    Ok(resolved)
}

/// Renders Office documents, PDFs and notebooks for `read_file`; `None` for
/// plain files (and notebooks that are not nbformat 4).
fn read_document_file(
    input: &ReadFileInput,
    workspace: &Path,
) -> Result<Option<runtime::ReadFileOutput>, String> {
    if documents::DocumentKind::from_path(Path::new(&input.path)).is_none() {
        return Ok(None);
    }
    let path = document_path_in_workspace(&input.path, workspace, false)?;
    let size = std::fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .len();
    if size > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "document is too large ({size} bytes, max {MAX_DOCUMENT_BYTES} bytes)"
        ));
    }
    let options = documents::ReadOptions {
        sheet: input.sheet.clone(),
        range: input.range.clone(),
        max_rows: None,
        formulas: input.formulas.unwrap_or(false),
        pages: input.pages.clone(),
    };
    let Some(text) = documents::read_document(&path, &options)? else {
        return Ok(None);
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = input.offset.unwrap_or(0).min(lines.len());
    let end = input.limit.map_or(lines.len(), |limit| {
        start.saturating_add(limit).min(lines.len())
    });
    Ok(Some(runtime::ReadFileOutput {
        kind: "document".to_string(),
        file: runtime::TextFilePayload {
            file_path: path.to_string_lossy().into_owned(),
            content: lines[start..end].join("\n"),
            num_lines: end - start,
            start_line: start + 1,
            total_lines: lines.len(),
        },
    }))
}

fn document_write_result(path: &Path, existed: bool, summary: &str) -> Result<String, String> {
    to_pretty_json(json!({
        "type": if existed { "update" } else { "create" },
        "filePath": path.to_string_lossy(),
        "summary": summary,
    }))
}

fn run_write_docx(input: &WriteDocxInput) -> Result<String, String> {
    if !input.path.to_ascii_lowercase().ends_with(".docx") {
        return Err("Path must end with .docx".to_string());
    }
    let workspace = tool_workspace(&input.path, true)?;
    let path = document_path_in_workspace(&input.path, &workspace, true)?;
    let existed = path.exists();
    let letter = input
        .page_size
        .as_deref()
        .is_some_and(|size| size.eq_ignore_ascii_case("letter"));
    documents::write_docx(&path, &input.markdown, input.title.as_deref(), letter)?;
    let kb = std::fs::metadata(&path).map_or(0, |m| m.len() / 1024);
    document_write_result(
        &path,
        existed,
        &format!(
            "{} {} ({kb} KB).",
            if existed { "Overwrote" } else { "Created" },
            path.display()
        ),
    )
}

fn run_write_pptx(input: &WritePptxInput) -> Result<String, String> {
    if !input.path.to_ascii_lowercase().ends_with(".pptx") {
        return Err("Path must end with .pptx".to_string());
    }
    let workspace = tool_workspace(&input.path, true)?;
    let path = document_path_in_workspace(&input.path, &workspace, true)?;
    let slides = input
        .slides
        .iter()
        .map(|slide| {
            let image = slide
                .image
                .as_deref()
                .map(|image| {
                    let root = tool_workspace(image, false)?;
                    document_path_in_workspace(image, &root, false)
                })
                .transpose()?;
            Ok(documents::SlideSpec {
                title: slide.title.clone(),
                bullets: slide.bullets.clone(),
                table_headers: slide.table_headers.clone(),
                table_rows: slide.table_rows.clone(),
                image,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let existed = path.exists();
    let count = documents::write_pptx(
        &path,
        input.title.as_deref(),
        input.subtitle.as_deref(),
        &slides,
    )?;
    document_write_result(
        &path,
        existed,
        &format!(
            "{} {}: {count} slide(s).",
            if existed { "Overwrote" } else { "Created" },
            path.display()
        ),
    )
}

fn run_write_xlsx(input: &WriteXlsxInput) -> Result<String, String> {
    let workspace = tool_workspace(&input.path, true)?;
    let path = document_path_in_workspace(&input.path, &workspace, true)?;
    let edit = documents::XlsxEdit {
        sheet: input.sheet.clone(),
        cells: input
            .cells
            .iter()
            .map(|(address, value)| (address.clone(), value.clone()))
            .collect(),
        rows: input.rows.clone(),
        start_cell: input.start_cell.clone(),
        bold_first_row: input.bold_first_row.unwrap_or(false),
        delete_sheet: input.delete_sheet.clone(),
    };
    let summary = documents::write_xlsx(&path, &edit)?;
    document_write_result(
        &path,
        !summary.created,
        &format!(
            "{} {}: wrote {} cell(s) on sheet '{}'.",
            if summary.created {
                "Created"
            } else {
                "Updated"
            },
            path.display(),
            summary.cells_written,
            summary.sheet
        ),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_write_file(input: WriteFileInput) -> Result<String, String> {
    let workspace = tool_workspace(&input.path, true)?;
    require_file_seen(&input.path)?;
    let output =
        write_file_in_workspace(&input.path, &input.content, &workspace).map_err(io_to_string)?;
    note_file_seen(&input.path);
    to_pretty_json(output)
}

#[allow(clippy::needless_pass_by_value)]
fn run_edit_file(input: EditFileInput) -> Result<String, String> {
    let workspace = tool_workspace(&input.path, false)?;
    require_file_seen(&input.path)?;
    let output = edit_file_in_workspace(
        &input.path,
        &input.old_string,
        &input.new_string,
        input.replace_all.unwrap_or(false),
        &workspace,
    )
    .map_err(io_to_string)?;
    note_file_seen(&input.path);
    to_pretty_json(output)
}

/// A file's size and modification time when the model last saw it.
type FileStamp = (u64, Option<std::time::SystemTime>);

/// Files the model has read (or written) in this process, keyed by
/// canonical path, as Claude Code does: `write_file` and `edit_file` refuse
/// an existing file it has not read, or one that changed since, so it does
/// not rewrite code it has not seen or clobber the user's changes.
fn seen_files() -> &'static Mutex<HashMap<PathBuf, FileStamp>> {
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, FileStamp>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The canonical path and stamp of an existing file; `None` when `path` is
/// not a file (a new one may be created without reading anything).
fn file_stamp(path: &str) -> Option<(PathBuf, FileStamp)> {
    let path = runtime::expand_home(path);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let path = path.canonicalize().ok()?;
    let meta = std::fs::metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_file)?;
    Some((path, (meta.len(), meta.modified().ok())))
}

fn note_file_seen(path: &str) {
    if let Some((path, stamp)) = file_stamp(path) {
        seen_files()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path, stamp);
    }
}

/// `apply_patch`: every file is checked (permission, read before change,
/// the hunks match) before any is written, so a patch applies whole or not
/// at all.
fn run_apply_patch(enforcer: Option<&PermissionEnforcer>, input: &Value) -> Result<String, String> {
    use apply_patch::PatchOp;
    let patch = input
        .get("patch")
        .or_else(|| input.get("input"))
        .and_then(Value::as_str)
        .ok_or("apply_patch needs `patch`: the text from *** Begin Patch to *** End Patch")?;
    let ops = apply_patch::parse_patch(patch)?;
    let paths = ops
        .iter()
        .flat_map(|op| match op {
            PatchOp::Add { path, .. } | PatchOp::Delete { path } => vec![path.as_str()],
            PatchOp::Update { path, move_to, .. } => std::iter::once(path.as_str())
                .chain(move_to.as_deref())
                .collect(),
        })
        .collect::<Vec<_>>();
    let required_mode = paths
        .iter()
        .map(|path| classify_file_path_permission(path, true))
        .max()
        .unwrap_or(PermissionMode::WorkspaceWrite);
    maybe_enforce_permission_check_with_mode(enforcer, "apply_patch", input, required_mode)?;

    let mut writes: Vec<(String, Option<String>)> = Vec::new();
    let mut summary = Vec::new();
    for op in &ops {
        match op {
            PatchOp::Add { path, content } => {
                if runtime::expand_home(path).exists() {
                    require_file_seen(path)?;
                }
                writes.push((path.clone(), Some(content.clone())));
                summary.push(format!("A {path} (+{})", content.lines().count()));
            }
            PatchOp::Delete { path } => {
                require_file_seen(path)?;
                writes.push((path.clone(), None));
                summary.push(format!("D {path}"));
            }
            PatchOp::Update {
                path,
                move_to,
                hunks,
            } => {
                require_file_seen(path)?;
                let old = std::fs::read_to_string(runtime::expand_home(path))
                    .map_err(|error| format!("{path}: {error}"))?;
                let new = apply_patch::apply_hunks(&old, hunks)
                    .map_err(|error| format!("{path}: {error}"))?;
                let (added, removed) = line_delta(&old, &new);
                let target = move_to.clone().unwrap_or_else(|| path.clone());
                writes.push((target.clone(), Some(new)));
                if move_to.is_some() {
                    writes.push((path.clone(), None));
                    summary.push(format!("R {path} -> {target} (+{added} -{removed})"));
                } else {
                    summary.push(format!("M {path} (+{added} -{removed})"));
                }
            }
        }
    }
    for (path, content) in writes {
        match content {
            Some(content) => {
                let workspace = tool_workspace(&path, true)?;
                write_file_in_workspace(&path, &content, &workspace).map_err(io_to_string)?;
                note_file_seen(&path);
                leangate::record_edit(&path);
                runtime::touch_memory(&path);
            }
            None => std::fs::remove_file(runtime::expand_home(&path))
                .map_err(|error| format!("{path}: {error}"))?,
        }
    }
    Ok(format!("Applied the patch:\n{}", summary.join("\n")))
}

/// Lines added and removed between two versions of a file.
fn line_delta(old: &str, new: &str) -> (usize, usize) {
    let mut old_lines = old.lines().collect::<Vec<_>>();
    let mut added = 0;
    for line in new.lines() {
        if let Some(at) = old_lines.iter().position(|old| *old == line) {
            old_lines.swap_remove(at);
        } else {
            added += 1;
        }
    }
    (added, old_lines.len())
}

fn require_file_seen(path: &str) -> Result<(), String> {
    let Some((canonical, stamp)) = file_stamp(path) else {
        return Ok(());
    };
    match seen_files()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&canonical)
    {
        Some(seen) if *seen == stamp => Ok(()),
        Some(_) => Err(format!(
            "{} changed since you last read it (the user, a formatter or a command changed it). Read it again with read_file before changing it.",
            canonical.display()
        )),
        None => Err(format!(
            "{} exists but you have not read it with read_file yet. Read it first (the part you change is enough), then change it with edit_file. Output of cat or other bash commands does not count.",
            canonical.display()
        )),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_glob_search(input: GlobSearchInputValue) -> Result<String, String> {
    // Without a search path, an absolute pattern (`/etc/*.conf`) sets the scope.
    let scope = input.path.as_deref().unwrap_or_else(|| {
        if Path::new(&input.pattern).is_absolute() {
            &input.pattern
        } else {
            "."
        }
    });
    let workspace = tool_workspace(scope, true)?;
    to_pretty_json(
        glob_search_in_workspace(&input.pattern, input.path.as_deref(), &workspace)
            .map_err(io_to_string)?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_grep_search(input: GrepSearchInput) -> Result<String, String> {
    let workspace = tool_workspace(input.path.as_deref().unwrap_or("."), false)?;
    to_pretty_json(grep_search_in_workspace(&input, &workspace).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_web_fetch(input: WebFetchInput) -> Result<String, String> {
    to_pretty_json(execute_web_fetch(&input)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_web_search(input: WebSearchInput) -> Result<String, String> {
    to_pretty_json(execute_web_search(&input)?)
}

fn run_todo_write(input: TodoWriteInput) -> Result<String, String> {
    to_pretty_json(execute_todo_write(input)?)
}

fn run_skill(input: SkillInput) -> Result<String, String> {
    to_pretty_json(execute_skill(input)?)
}

fn run_agent(input: AgentInput) -> Result<String, String> {
    if input.run_in_background {
        return to_pretty_json(execute_agent(input)?);
    }
    to_pretty_json(execute_agent_foreground(input)?)
}

/// Runs a sub-agent on the calling thread and returns its result, the way a
/// foreground agent call works in Claude Code.
fn execute_agent_foreground(input: AgentInput) -> Result<serde_json::Value, String> {
    let mut outcome: Option<Result<String, String>> = None;
    let manifest = execute_agent_with_spawn(input, |job| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_agent_job(&job)))
            .unwrap_or_else(|_| Err(String::from("sub-agent panicked")));
        if let Err(error) = &result {
            let _ =
                persist_agent_terminal_state(&job.manifest, "failed", None, Some(error.clone()));
        }
        outcome = Some(result);
        Ok(())
    })?;
    Ok(match outcome {
        Some(Ok(result)) => json!({
            "agentId": manifest.agent_id,
            "name": manifest.name,
            "status": "completed",
            "result": result,
        }),
        Some(Err(error)) => json!({
            "agentId": manifest.agent_id,
            "name": manifest.name,
            "status": "failed",
            "error": error,
        }),
        None => json!({
            "agentId": manifest.agent_id,
            "name": manifest.name,
            "status": "failed",
            "error": "sub-agent did not run",
        }),
    })
}

/// The sub-agent model when the call names none: the session's model (set by
/// the CLI), so local setups do not fall back to a hosted model.
static DEFAULT_AGENT_MODEL_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Makes sub-agents default to `model` (the session's current model).
pub fn set_default_agent_model(model: &str) {
    *DEFAULT_AGENT_MODEL_OVERRIDE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(model.to_string());
}

/// Keeps a notice's result short enough for the context window.
fn truncate_for_notice(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit).collect();
    cut.push_str("\n[... truncated; full result in the agent's output file]");
    cut
}

fn run_tool_search(input: ToolSearchInput) -> Result<String, String> {
    to_pretty_json(execute_tool_search(input))
}

fn run_notebook_edit(input: NotebookEditInput) -> Result<String, String> {
    to_pretty_json(execute_notebook_edit(input)?)
}

fn run_sleep(input: SleepInput) -> Result<String, String> {
    to_pretty_json(execute_sleep(input)?)
}

fn run_brief(input: BriefInput) -> Result<String, String> {
    to_pretty_json(execute_brief(input)?)
}

fn run_config(input: ConfigInput) -> Result<String, String> {
    to_pretty_json(execute_config(input)?)
}

fn run_enter_plan_mode(input: EnterPlanModeInput) -> Result<String, String> {
    to_pretty_json(execute_enter_plan_mode(input)?)
}

fn run_exit_plan_mode(input: ExitPlanModeInput) -> Result<String, String> {
    to_pretty_json(execute_exit_plan_mode(input)?)
}

fn run_structured_output(input: StructuredOutputInput) -> Result<String, String> {
    to_pretty_json(execute_structured_output(input)?)
}

fn run_repl(input: ReplInput) -> Result<String, String> {
    to_pretty_json(execute_repl(input)?)
}

fn classify_file_path_permission(path: &str, allow_missing: bool) -> PermissionMode {
    if path_within_current_workspace(path, allow_missing) {
        PermissionMode::WorkspaceWrite
    } else {
        PermissionMode::DangerFullAccess
    }
}

fn classify_read_path_permission(path: &str, allow_missing: bool) -> PermissionMode {
    // Saved long tool output (see runtime::tool_spill) is read like the
    // workspace: the model was told to read it there.
    if path_within_current_workspace(path, allow_missing) || runtime::is_spill_path(path) {
        PermissionMode::ReadOnly
    } else {
        PermissionMode::DangerFullAccess
    }
}

fn classify_glob_permission(input: &GlobSearchInputValue) -> PermissionMode {
    let base_allowed = input
        .path
        .as_deref()
        .is_none_or(|path| path_within_current_workspace(path, false));
    let pattern_allowed = path_within_current_workspace(&input.pattern, true);
    if base_allowed && pattern_allowed {
        PermissionMode::ReadOnly
    } else {
        PermissionMode::DangerFullAccess
    }
}

fn classify_grep_permission(input: &GrepSearchInput) -> PermissionMode {
    if input.path.as_deref().is_none_or(|path| {
        path_within_current_workspace(path, false) || runtime::is_spill_path(path)
    }) {
        PermissionMode::ReadOnly
    } else {
        PermissionMode::DangerFullAccess
    }
}

fn path_within_current_workspace(path: &str, allow_missing: bool) -> bool {
    workspace_root_for(path, allow_missing).is_some()
}

/// Workspace root containing `path`: the current directory, or a directory
/// added with `/add-dir`. `None` if the path is outside all of them.
fn workspace_root_for(path: &str, allow_missing: bool) -> Option<PathBuf> {
    let trimmed = path.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
        )
    });
    if looks_like_windows_absolute_path(trimmed) {
        return None;
    }

    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let candidate = PathBuf::from(trimmed);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    };

    let resolved = if allow_missing {
        absolute
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .map(|parent| parent.join(absolute.file_name().unwrap_or_default()))
            .unwrap_or(absolute)
    } else {
        match absolute.canonicalize() {
            Ok(path) => path,
            Err(_) => absolute,
        }
    };

    if resolved.starts_with(&cwd) {
        return Some(cwd);
    }
    additional_workspace_roots()
        .into_iter()
        .find(|root| resolved.starts_with(root))
}

/// Root to enforce the workspace boundary against for a file tool call: the
/// workspace containing `path`, or `/` for paths outside every workspace.
/// Whether such a call may run at all is decided by the permission policy
/// before the tool executes (writes outside the workspace need approval).
fn tool_workspace(path: &str, allow_missing: bool) -> Result<PathBuf, String> {
    Ok(workspace_root_for(path, allow_missing).unwrap_or_else(|| PathBuf::from("/")))
}

/// Expands a leading `~` in the path-like fields of a tool input, so
/// `~/notes.txt` means the home directory rather than a folder named `~`.
fn expand_home_in_paths(input: &Value) -> Value {
    let mut input = input.clone();
    if let Value::Object(fields) = &mut input {
        for key in ["path", "file_path", "filePath", "notebook_path", "pattern"] {
            if let Some(Value::String(value)) = fields.get_mut(key) {
                if value == "~" || value.starts_with("~/") {
                    *value = runtime::expand_home(value).display().to_string();
                }
            }
        }
    }
    input
}

fn additional_roots_store() -> &'static Mutex<Vec<PathBuf>> {
    static ROOTS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Directories added with `/add-dir`. File tools treat paths inside them like
/// paths inside the current workspace when classifying permissions.
#[must_use]
pub fn additional_workspace_roots() -> Vec<PathBuf> {
    additional_roots_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Adds a directory to the workspace roots. Returns the canonical path.
pub fn add_workspace_root(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot add {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("not a directory: {}", canonical.display()));
    }
    let mut roots = additional_roots_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !roots.contains(&canonical) {
        roots.push(canonical.clone());
    }
    Ok(canonical)
}

/// Classify `PowerShell` command permission based on command type and path.
/// ROADMAP #50: Read-only commands targeting CWD paths get `WorkspaceWrite`,
/// all others remain `DangerFullAccess`.
fn classify_powershell_permission(command: &str) -> PermissionMode {
    // Read-only commands that are safe when targeting workspace paths
    const READ_ONLY_COMMANDS: &[&str] = &[
        "Get-Content",
        "Get-ChildItem",
        "Test-Path",
        "Get-Item",
        "Get-ItemProperty",
        "Get-FileHash",
        "Select-String",
    ];

    // Check if command starts with a read-only cmdlet
    let cmd_lower = command.trim().to_lowercase();
    let is_read_only_cmd = READ_ONLY_COMMANDS
        .iter()
        .any(|cmd| cmd_lower.starts_with(&cmd.to_lowercase()));

    if !is_read_only_cmd {
        return PermissionMode::DangerFullAccess;
    }

    // Check if the path is within workspace (CWD or subdirectory)
    // Extract path from command - look for -Path or positional parameter
    let path = extract_powershell_path(command);
    match path {
        Some(p) if is_within_workspace(&p) => PermissionMode::WorkspaceWrite,
        _ => PermissionMode::DangerFullAccess,
    }
}

/// Extract the path argument from a `PowerShell` command.
fn extract_powershell_path(command: &str) -> Option<String> {
    // Look for -Path parameter
    if let Some(idx) = command.to_lowercase().find("-path") {
        let after_path = &command[idx + 5..];
        let path = after_path.split_whitespace().next()?;
        return Some(path.trim_matches('"').trim_matches('\'').to_string());
    }

    // Look for positional path parameter (after command name)
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.len() >= 2 {
        // Skip the cmdlet name and take the first argument
        let first_arg = parts[1];
        // Check if it looks like a path (contains \, /, or .)
        if first_arg.contains(['\\', '/', '.']) {
            return Some(first_arg.trim_matches('"').trim_matches('\'').to_string());
        }
    }

    None
}

/// Check if a path is within the current workspace.
fn is_within_workspace(path: &str) -> bool {
    let trimmed = path.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
        )
    });
    if looks_like_windows_absolute_path(trimmed) {
        return false;
    }

    let path = PathBuf::from(trimmed);

    // Reject any parent-directory traversal. Callers never need `..` to refer
    // to files inside the workspace, and `..` defeats both checks below: the
    // relative branch only inspects the leading component, and the absolute
    // branch's `canonicalize()` silently falls back to the literal `..` path
    // when the target does not exist yet (e.g. a file about to be created).
    // Returning false here is the safe direction: it classifies the command as
    // requiring full-access permission rather than workspace-write.
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }

    // If path is absolute, check if it starts with CWD
    if path.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            let cwd = cwd.canonicalize().unwrap_or(cwd);
            let resolved = path.canonicalize().unwrap_or(path);
            return resolved.starts_with(&cwd);
        }
    }

    // Relative paths are assumed to be within workspace
    !path.starts_with("/") && !path.starts_with("\\") && !path.starts_with("..")
}

fn run_powershell(input: PowerShellInput) -> Result<String, String> {
    to_pretty_json(execute_powershell(input).map_err(|error| error.to_string())?)
}

#[cfg(test)]
mod workspace_traversal_guard_tests {
    use super::is_within_workspace;

    #[test]
    fn rejects_parent_traversal_components() {
        // Leading and embedded `..` must both be rejected (was previously a hole
        // because only the leading component was inspected).
        assert!(!is_within_workspace("../secrets"));
        assert!(!is_within_workspace("src/../../etc/passwd"));
        assert!(!is_within_workspace("a/b/../../../etc/crontab"));
    }

    #[test]
    fn allows_plain_relative_paths() {
        assert!(is_within_workspace("src/main.rs"));
        assert!(is_within_workspace("Cargo.toml"));
    }
}

fn to_pretty_json<T: serde::Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn io_to_string(error: std::io::Error) -> String {
    error.to_string()
}

// Models trained on other agents (Claude Code, opencode) send their
// argument names (`file_path`, `filePath`, `oldString`, …); accept those too
// instead of failing the call with "missing field".
#[derive(Debug, Deserialize)]
struct ReadFileInput {
    #[serde(alias = "file_path", alias = "filePath", alias = "filename")]
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    sheet: Option<String>,
    range: Option<String>,
    formulas: Option<bool>,
    pages: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WriteDocxInput {
    path: String,
    markdown: String,
    title: Option<String>,
    page_size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WritePptxInput {
    path: String,
    title: Option<String>,
    subtitle: Option<String>,
    slides: Vec<WritePptxSlide>,
}

#[derive(Debug, Deserialize)]
struct WritePptxSlide {
    title: Option<String>,
    #[serde(default)]
    bullets: Vec<String>,
    #[serde(default)]
    table_headers: Vec<String>,
    #[serde(default)]
    table_rows: Vec<Vec<String>>,
    image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WriteXlsxInput {
    path: String,
    sheet: Option<String>,
    #[serde(default)]
    cells: serde_json::Map<String, Value>,
    #[serde(default)]
    rows: Vec<Vec<Value>>,
    start_cell: Option<String>,
    bold_first_row: Option<bool>,
    delete_sheet: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    #[serde(alias = "file_path", alias = "filePath", alias = "filename")]
    path: String,
    #[serde(alias = "contents", alias = "text")]
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditFileInput {
    #[serde(alias = "file_path", alias = "filePath", alias = "filename")]
    path: String,
    #[serde(alias = "oldString", alias = "old_str", alias = "old")]
    old_string: String,
    #[serde(alias = "newString", alias = "new_str", alias = "new")]
    new_string: String,
    #[serde(alias = "replaceAll")]
    replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GlobSearchInputValue {
    pattern: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebFetchInput {
    url: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct WebSearchInput {
    query: String,
    allowed_domains: Option<Vec<String>>,
    blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TodoWriteInput {
    todos: Vec<TodoItem>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
struct TodoItem {
    content: String,
    #[serde(rename = "activeForm")]
    active_form: String,
    status: TodoStatus,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize)]
struct SkillInput {
    skill: String,
    args: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentInput {
    description: String,
    prompt: String,
    subagent_type: Option<String>,
    name: Option<String>,
    model: Option<String>,
    /// Return right away and report the result as a notice when the agent
    /// finishes; otherwise wait for it and return its result.
    #[serde(default)]
    run_in_background: bool,
}

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct NotebookEditInput {
    notebook_path: String,
    cell_id: Option<String>,
    new_source: Option<String>,
    cell_type: Option<NotebookCellType>,
    edit_mode: Option<NotebookEditMode>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookCellType {
    Code,
    Markdown,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookEditMode {
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Deserialize)]
struct SleepInput {
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
struct BriefInput {
    message: String,
    attachments: Option<Vec<String>>,
    status: BriefStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BriefStatus {
    Normal,
    Proactive,
}

#[derive(Debug, Deserialize)]
struct ConfigInput {
    setting: String,
    value: Option<ConfigValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EnterPlanModeInput {}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExitPlanModeInput {}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfigValue {
    String(String),
    Bool(bool),
    Number(f64),
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct StructuredOutputInput(BTreeMap<String, Value>);

#[derive(Debug, Deserialize)]
struct ReplInput {
    code: String,
    language: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PowerShellInput {
    command: String,
    timeout: Option<u64>,
    description: Option<String>,
    run_in_background: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AskUserQuestionInput {
    question: String,
    /// Plain labels or `{label, description, recommended}` objects.
    #[serde(default)]
    options: Option<Vec<Value>>,
    #[serde(default, alias = "multiSelect")]
    #[allow(dead_code)]
    multi_select: bool,
    #[serde(default)]
    #[allow(dead_code)]
    header: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskCreateInput {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BashOutputInput {
    bash_id: String,
    wait_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct KillShellInput {
    shell_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskIdInput {
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskUpdateInput {
    task_id: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct WorkerCreateInput {
    cwd: String,
    #[serde(default)]
    trusted_roots: Vec<String>,
    #[serde(default = "default_auto_recover_prompt_misdelivery")]
    auto_recover_prompt_misdelivery: bool,
}

#[derive(Debug, Deserialize)]
struct WorkerIdInput {
    worker_id: String,
}

#[derive(Debug, Deserialize)]
struct WorkerObserveCompletionInput {
    worker_id: String,
    finish_reason: String,
    tokens_output: u64,
}

#[derive(Debug, Deserialize)]
struct WorkerObserveInput {
    worker_id: String,
    screen_text: String,
}

#[derive(Debug, Deserialize)]
struct WorkerSendPromptInput {
    worker_id: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    task_receipt: Option<WorkerTaskReceipt>,
}

const fn default_auto_recover_prompt_misdelivery() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct TeamCreateInput {
    name: String,
    tasks: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct TeamDeleteInput {
    team_id: String,
}

#[derive(Debug, Deserialize)]
struct CronCreateInput {
    /// Claude Code calls it `cron`; claw's older name was `schedule`.
    #[serde(alias = "cron")]
    schedule: String,
    prompt: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_true")]
    recurring: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct CronDeleteInput {
    #[serde(alias = "id")]
    cron_id: String,
}

#[derive(Debug, Deserialize)]
struct McpResourceInput {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpAuthInput {
    server: String,
}

#[derive(Debug, Deserialize)]
struct RemoteTriggerInput {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<Value>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpToolInput {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct TestingPermissionInput {
    action: String,
}

/// Input for the GitStatus tool: shows working tree status.
/// Defaults to --short --branch mode for concise, parseable output.
#[derive(Debug, Deserialize)]
struct GitStatusInput {
    #[serde(default)]
    /// If true, use --short --branch format. Defaults to true.
    short: Option<bool>,
}

/// Input for the GitDiff tool: shows changes between commits, index, and working tree.
/// All fields are optional - calling with no options is equivalent to `git diff`.
#[derive(Debug, Deserialize)]
struct GitDiffInput {
    #[serde(default)]
    /// File path to diff. Prepends `--` before the path.
    path: Option<String>,
    #[serde(default)]
    /// If true, show staged changes (`git diff --cached`).
    staged: Option<bool>,
    #[serde(default)]
    /// A commit hash, tag, or branch to diff against.
    commit: Option<String>,
    #[serde(default)]
    /// A second commit for range diffs (commit...commit2).
    commit2: Option<String>,
}

/// Input for the GitLog tool: shows commit history.
/// Defaults to the last 20 commits in full format.
#[derive(Debug, Deserialize)]
struct GitLogInput {
    #[serde(default)]
    /// File or directory path to filter commits by.
    path: Option<String>,
    #[serde(default)]
    /// Maximum number of commits to return. Defaults to 20.
    count: Option<usize>,
    #[serde(default)]
    /// If true, use --oneline format (hash + subject only).
    oneline: Option<bool>,
    #[serde(default)]
    /// Filter commits by author pattern.
    author: Option<String>,
    #[serde(default)]
    /// Filter commits since date (e.g. "2024-01-01" or "2.weeks").
    since: Option<String>,
    #[serde(default)]
    /// Filter commits until date.
    until: Option<String>,
}

/// Input for the GitShow tool: shows a commit, tag, or tree object.
#[derive(Debug, Deserialize)]
struct GitShowInput {
    /// Commit hash, tag, or branch ref to show. Required.
    commit: String,
    #[serde(default)]
    /// If set, show only this file at the given commit (commit:path syntax).
    path: Option<String>,
    #[serde(default)]
    /// If true, show diffstat summary instead of full diff.
    stat: Option<bool>,
    #[serde(default)]
    /// Output format: "patch" (default) shows the full diff, "stat" shows a diffstat summary, and "metadata" shows commit info without the diff. When set, takes priority over `stat`.
    format: Option<String>,
}

/// Input for the GitBlame tool: shows per-line author/revision info for a file.
#[derive(Debug, Deserialize)]
struct GitBlameInput {
    /// File path to blame. Required.
    path: String,
    #[serde(rename = "start_line")]
    #[serde(default)]
    /// Start of line range (1-based). Only used if end_line is also set.
    start_line: Option<usize>,
    #[serde(rename = "end_line")]
    #[serde(default)]
    /// End of line range (1-based). Only used if start_line is also set.
    end_line: Option<usize>,
}

#[derive(Debug, Serialize)]
struct WebFetchOutput {
    bytes: usize,
    code: u16,
    #[serde(rename = "codeText")]
    code_text: String,
    result: String,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
    url: String,
}

#[derive(Debug, Serialize)]
struct WebSearchOutput {
    query: String,
    results: Vec<WebSearchResultItem>,
    #[serde(rename = "durationSeconds")]
    duration_seconds: f64,
}

#[derive(Debug, Serialize)]
struct TodoWriteOutput {
    #[serde(rename = "oldTodos")]
    old_todos: Vec<TodoItem>,
    #[serde(rename = "newTodos")]
    new_todos: Vec<TodoItem>,
    #[serde(rename = "verificationNudgeNeeded")]
    verification_nudge_needed: Option<bool>,
    /// What changed, as typed events (also appended to the events log).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    events: Vec<todo_events::TodoEvent>,
}

#[derive(Debug, Serialize)]
struct SkillOutput {
    skill: String,
    path: String,
    args: Option<String>,
    description: Option<String>,
    prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentOutput {
    #[serde(rename = "agentId")]
    agent_id: String,
    name: String,
    description: String,
    #[serde(rename = "subagentType")]
    subagent_type: Option<String>,
    model: Option<String>,
    status: String,
    #[serde(rename = "outputFile")]
    output_file: String,
    #[serde(rename = "manifestFile")]
    manifest_file: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(rename = "laneEvents", default, skip_serializing_if = "Vec::is_empty")]
    lane_events: Vec<LaneEvent>,
    #[serde(rename = "currentBlocker", skip_serializing_if = "Option::is_none")]
    current_blocker: Option<LaneEventBlocker>,
    #[serde(rename = "derivedState")]
    derived_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentJob {
    manifest: AgentOutput,
    prompt: String,
    system_prompt: Vec<String>,
    allowed_tools: BTreeSet<String>,
    /// For a fork: the parent's finished conversation it starts from.
    seed: Option<Session>,
}

/// The main conversation as of the start of the current turn, with its
/// system prompt: what a `fork` sub-agent starts from.
static FORK_SEED: Mutex<Option<(Session, Vec<String>)>> = Mutex::new(None);

/// Records the conversation a `fork` sub-agent would start from: the
/// finished turns (the CLI calls it as each turn starts, so a fork never
/// sees a half-done turn) and the system prompt they were made with.
pub fn set_fork_seed(session: Session, system_prompt: Vec<String>) {
    *FORK_SEED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((session, system_prompt));
}

/// Said before a fork's task, so it knows it is a copy working on a side
/// task rather than the main agent continuing.
const FORK_NOTE: &str = "<system-reminder>You are now a fork: a sub-agent that starts with the \
     conversation above. The main agent is still working; do only the task below, without asking \
     the user anything, then end with a concise report of what you did and found (your last \
     message is all the main agent receives).</system-reminder>";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolSearchOutput {
    pub matches: Vec<String>,
    query: String,
    normalized_query: String,
    #[serde(rename = "total_deferred_tools")]
    total_deferred_tools: usize,
    #[serde(rename = "pending_mcp_servers")]
    pending_mcp_servers: Option<Vec<String>>,
    #[serde(rename = "mcp_degraded", skip_serializing_if = "Option::is_none")]
    mcp_degraded: Option<McpDegradedReport>,
}

#[derive(Debug, Serialize)]
struct NotebookEditOutput {
    new_source: String,
    cell_id: Option<String>,
    cell_type: Option<NotebookCellType>,
    language: String,
    edit_mode: String,
    error: Option<String>,
    notebook_path: String,
    original_file: String,
    updated_file: String,
}

#[derive(Debug, Serialize)]
struct SleepOutput {
    duration_ms: u64,
    message: String,
}

#[derive(Debug, Serialize)]
struct BriefOutput {
    message: String,
    attachments: Option<Vec<ResolvedAttachment>>,
    #[serde(rename = "sentAt")]
    sent_at: String,
}

#[derive(Debug, Serialize)]
struct ResolvedAttachment {
    path: String,
    size: u64,
    #[serde(rename = "isImage")]
    is_image: bool,
}

#[derive(Debug, Serialize)]
struct ConfigOutput {
    success: bool,
    operation: Option<String>,
    setting: Option<String>,
    value: Option<Value>,
    #[serde(rename = "previousValue")]
    previous_value: Option<Value>,
    #[serde(rename = "newValue")]
    new_value: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanModeState {
    #[serde(rename = "hadLocalOverride")]
    had_local_override: bool,
    #[serde(rename = "previousLocalMode")]
    previous_local_mode: Option<Value>,
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct PlanModeOutput {
    success: bool,
    operation: String,
    changed: bool,
    active: bool,
    managed: bool,
    message: String,
    #[serde(rename = "settingsPath")]
    settings_path: String,
    #[serde(rename = "statePath")]
    state_path: String,
    #[serde(rename = "previousLocalMode")]
    previous_local_mode: Option<Value>,
    #[serde(rename = "currentLocalMode")]
    current_local_mode: Option<Value>,
}

#[derive(Debug, Clone)]
struct SearchableToolSpec {
    name: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct StructuredOutputResult {
    data: String,
    structured_output: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ReplOutput {
    language: String,
    stdout: String,
    stderr: String,
    #[serde(rename = "exitCode")]
    exit_code: i32,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WebSearchResultItem {
    SearchResult {
        tool_use_id: String,
        content: Vec<SearchHit>,
    },
    Commentary(String),
}

#[derive(Debug, Serialize)]
struct SearchHit {
    title: String,
    url: String,
    /// The result's text excerpt, when the search page has one.
    snippet: Option<String>,
}

fn execute_web_fetch(input: &WebFetchInput) -> Result<WebFetchOutput, String> {
    let started = Instant::now();
    // Sites with a clean API (Wikipedia, arXiv, GitHub, Semantic Scholar)
    // are read through it; if that fails, the page is fetched as usual.
    if let Some(Ok(adapted)) = source_adapters::fetch(&input.url) {
        let result = adapted.render();
        return Ok(WebFetchOutput {
            bytes: result.len(),
            code: 200,
            code_text: "OK".to_string(),
            result,
            duration_ms: started.elapsed().as_millis(),
            url: adapted.url,
        });
    }
    let client = build_http_client()?;
    let request_url = normalize_fetch_url(&input.url)?;
    let response = client
        .get(request_url.clone())
        .send()
        .map_err(|error| error.to_string())?;

    let status = response.status();
    let final_url = response.url().to_string();
    let code = status.as_u16();
    let code_text = status.canonical_reason().unwrap_or("Unknown").to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if content_type.starts_with("image/") {
        // Binary: decoding it as text only feeds the model noise.
        let bytes = response.bytes().map_err(|error| error.to_string())?.len();
        return Ok(WebFetchOutput {
            bytes,
            code,
            code_text,
            result: format!(
                "Fetched {final_url}\nThis is an image ({content_type}, {bytes} bytes). To show it to the user, put ![description]({final_url}) in the reply."
            ),
            duration_ms: started.elapsed().as_millis(),
            url: final_url,
        });
    }
    let body = response.text().map_err(|error| error.to_string())?;
    let bytes = body.len();
    let normalized = normalize_fetched_content(&body, &content_type);
    let mut result =
        summarize_web_fetch(&final_url, &input.prompt, &normalized, &body, &content_type);
    if content_type.contains("html") {
        let images = extract_image_urls(&body, &final_url);
        if !images.is_empty() {
            result.push_str("\nImages on the page (show one with ![description](url)):\n");
            for image in images {
                result.push_str(&format!("- {image}\n"));
            }
        }
    }

    Ok(WebFetchOutput {
        bytes,
        code,
        code_text,
        result,
        duration_ms: started.elapsed().as_millis(),
        url: final_url,
    })
}

fn execute_web_search(input: &WebSearchInput) -> Result<WebSearchOutput, String> {
    let started = Instant::now();
    let (engine, mut hits) = search_hits(&input.query)?;

    if let Some(allowed) = input.allowed_domains.as_ref() {
        hits.retain(|hit| host_matches_list(&hit.url, allowed));
    }
    if let Some(blocked) = input.blocked_domains.as_ref() {
        hits.retain(|hit| !host_matches_list(&hit.url, blocked));
    }

    dedupe_hits(&mut hits);
    hits.truncate(8);

    let summary = if hits.is_empty() {
        format!("No web search results matched the query {:?}.", input.query)
    } else {
        let rendered_hits = hits
            .iter()
            .map(|hit| match &hit.snippet {
                Some(snippet) => format!("- [{}]({})\n  {snippet}", hit.title, hit.url),
                None => format!("- [{}]({})", hit.title, hit.url),
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Search results for {:?} (via {engine}). Include a Sources section in the final answer.\n{}",
            input.query, rendered_hits
        )
    };

    Ok(WebSearchOutput {
        query: input.query.clone(),
        results: vec![
            WebSearchResultItem::Commentary(summary),
            WebSearchResultItem::SearchResult {
                tool_use_id: String::from("web_search_1"),
                content: hits,
            },
        ],
        duration_seconds: started.elapsed().as_secs_f64(),
    })
}

/// Hits for `query` and where they came from. A configured backend
/// (`CLAWD_WEB_SEARCH_BASE_URL`) is used alone. Otherwise: the local
/// SearXNG when it answers, then DuckDuckGo (skipped for a while after it
/// answers with its bot check), then Brave Search and Bing.
fn search_hits(query: &str) -> Result<(String, Vec<SearchHit>), String> {
    if std::env::var_os("CLAWD_WEB_SEARCH_BASE_URL").is_some() {
        return duckduckgo_hits(query).map(|hits| ("DuckDuckGo".to_string(), hits));
    }
    if let Some(found) = search_fallback::searxng(query) {
        return Ok(found);
    }
    if !search_fallback::duckduckgo_blocked() {
        let hits = duckduckgo_hits(query).unwrap_or_default();
        if !hits.is_empty() {
            return Ok(("DuckDuckGo".to_string(), hits));
        }
        // A bot check (or no answer): leave DuckDuckGo alone for a while
        // instead of asking again on every search.
        search_fallback::block_duckduckgo();
    }
    Ok(search_fallback::search(query)
        .map(|(engine, hits)| (engine.to_string(), hits))
        .unwrap_or_else(|| ("DuckDuckGo".to_string(), Vec::new())))
}

/// DuckDuckGo's HTML results (or the configured backend's), without the
/// links back to DuckDuckGo itself that its bot-check page consists of.
fn duckduckgo_hits(query: &str) -> Result<Vec<SearchHit>, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(search_fallback::USER_AGENT)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(build_search_url(query)?)
        .send()
        .map_err(|error| error.to_string())?;
    let final_url = response.url().clone();
    let html = response.text().map_err(|error| error.to_string())?;
    let mut hits = extract_search_hits(&html);
    if hits.is_empty() && final_url.host_str().is_some() {
        hits = extract_search_hits_from_generic_links(&html);
    }
    if std::env::var_os("CLAWD_WEB_SEARCH_BASE_URL").is_none() {
        hits.retain(|hit| !hit.url.contains("duckduckgo.com"));
    }
    Ok(hits)
}

/// Whether `host` is, or resolves to, an address on this machine or a
/// private network: loopback, RFC 1918, CGNAT, link-local (cloud metadata at
/// 169.254.169.254), unique-local IPv6, or the unspecified address. A name
/// that does not resolve counts as public.
fn is_private_host(host: &str) -> bool {
    use std::net::{IpAddr, ToSocketAddrs};
    fn private(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                let [a, b, ..] = v4.octets();
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    || (a == 100 && (64..128).contains(&b))
            }
            IpAddr::V6(v6) => {
                let first = v6.segments()[0];
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80
                    || v6
                        .to_ipv4_mapped()
                        .is_some_and(|v4| private(IpAddr::V4(v4)))
            }
        }
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return private(ip);
    }
    (host, 0)
        .to_socket_addrs()
        .is_ok_and(|mut addrs| addrs.any(|addr| private(addr.ip())))
}

/// Follows up to 10 redirects, but never from a public address to a private
/// one: WebFetch runs in claw itself, outside the shell sandbox, so a page
/// could otherwise send it to `localhost` services or the cloud metadata
/// endpoint. Asking for a private address directly still works.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let started_public = attempt
            .previous()
            .first()
            .and_then(|url| url.host_str())
            .is_some_and(|host| !is_private_host(host));
        let lands_private = attempt.url().host_str().is_none_or(is_private_host);
        if started_public && lands_private {
            let target = attempt.url().to_string();
            attempt.error(format!(
                "refused a redirect from a public page to a private address ({target}); fetch that address directly if you mean to"
            ))
        } else {
            attempt.follow()
        }
    })
}

fn build_http_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(redirect_policy())
        .user_agent("clawd-rust-tools/0.1")
        .build()
        .map_err(|error| error.to_string())
}

fn normalize_fetch_url(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    if parsed.scheme() == "http" {
        let host = parsed.host_str().unwrap_or_default();
        if host != "localhost" && host != "127.0.0.1" && host != "::1" {
            let mut upgraded = parsed;
            upgraded
                .set_scheme("https")
                .map_err(|()| String::from("failed to upgrade URL to https"))?;
            return Ok(upgraded.to_string());
        }
    }
    Ok(parsed.to_string())
}

fn build_search_url(query: &str) -> Result<reqwest::Url, String> {
    if let Ok(base) = std::env::var("CLAWD_WEB_SEARCH_BASE_URL") {
        let mut url = reqwest::Url::parse(&base).map_err(|error| error.to_string())?;
        url.query_pairs_mut().append_pair("q", query);
        return Ok(url);
    }

    let mut url = reqwest::Url::parse("https://html.duckduckgo.com/html/")
        .map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("q", query);
    Ok(url)
}

fn normalize_fetched_content(body: &str, content_type: &str) -> String {
    if content_type.contains("html") {
        html_to_readable_text(body)
    } else {
        body.trim().to_string()
    }
}

/// How much page text one fetch hands the model. It used to be a 900-char
/// preview, which on most real pages was only `<head>` scripts and JSON-LD,
/// so models re-fetched the same URL over and over looking for the article.
const WEB_FETCH_MAX_CONTENT_CHARS: usize = 10_000;

fn summarize_web_fetch(
    url: &str,
    prompt: &str,
    content: &str,
    raw_body: &str,
    content_type: &str,
) -> String {
    let mut out = format!("Fetched {url}\n");
    if content_type.contains("html") {
        if let Some(title) = extract_title(content, raw_body, content_type) {
            out.push_str(&format!("Title: {title}\n"));
        }
    }
    out.push_str(&format!("Prompt: {prompt}\nContent:\n"));
    let total = content.chars().count();
    if total <= WEB_FETCH_MAX_CONTENT_CHARS {
        out.push_str(content);
    } else {
        let shown = content
            .chars()
            .take(WEB_FETCH_MAX_CONTENT_CHARS)
            .collect::<String>();
        out.push_str(shown.trim_end());
        out.push_str(&format!(
            "\n[truncated: showing {WEB_FETCH_MAX_CONTENT_CHARS} of {total} characters of page text]"
        ));
    }
    out
}

/// Image URLs on an HTML page: `og:image` first, then `<img src>`, made
/// absolute, without data URIs, tracking pixels or duplicates.
fn extract_image_urls(html: &str, page_url: &str) -> Vec<String> {
    const MAX_IMAGES: usize = 12;
    let base = reqwest::Url::parse(page_url).ok();
    let mut found: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let raw = decode_html_entities(raw.trim());
        if raw.is_empty() || raw.starts_with("data:") || found.len() >= MAX_IMAGES {
            return;
        }
        let absolute = match &base {
            Some(base) => match base.join(&raw) {
                Ok(url) => url.to_string(),
                Err(_) => return,
            },
            None => raw,
        };
        let lower = absolute.to_lowercase();
        if lower.contains("pixel") || lower.contains("1x1") || lower.contains("spacer") {
            return;
        }
        if !found.contains(&absolute) {
            found.push(absolute);
        }
    };
    let lower = html.to_lowercase();
    for (start, _) in lower.match_indices("<meta") {
        let tag = &html[start..start + lower[start..].find('>').unwrap_or(0)];
        let tag_lower = tag.to_lowercase();
        if tag_lower.contains("og:image") || tag_lower.contains("twitter:image") {
            if let Some(content) = html_attribute(tag, "content") {
                push(&content);
            }
        }
    }
    for (start, _) in lower.match_indices("<img") {
        let tag = &html[start..start + lower[start..].find('>').unwrap_or(0)];
        if let Some(src) = html_attribute(tag, "src").or_else(|| html_attribute(tag, "data-src")) {
            push(&src);
        }
    }
    found
}

/// The value of `name="..."` (or single-quoted) inside one HTML tag.
fn html_attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let mut search = 0;
    while let Some(found) = lower[search..].find(name) {
        let at = search + found;
        search = at + name.len();
        let before_ok = at == 0
            || lower[..at]
                .chars()
                .last()
                .is_some_and(|c| c.is_whitespace());
        let rest = lower[search..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let value_start = tag.len() - rest.len() + 1;
        let value = tag[value_start..].trim_start();
        let quote = value.chars().next()?;
        return if quote == '"' || quote == '\'' {
            value[1..].find(quote).map(|end| value[1..=end].to_string())
        } else {
            Some(
                value
                    .split(|c: char| c.is_whitespace() || c == '>')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            )
        };
    }
    None
}

/// Elements whose content is never part of the readable page.
const HTML_SKIPPED_ELEMENTS: &[&str] = &[
    "head", "script", "style", "noscript", "svg", "template", "iframe", "nav", "footer", "form",
    "button", "select",
];
/// Elements that start a new line in the readable text.
const HTML_BLOCK_ELEMENTS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "table",
    "pre",
    "section",
    "article",
    "header",
    "main",
    "blockquote",
    "hr",
    "dd",
    "dt",
    "dl",
    "figure",
    "figcaption",
    "aside",
    "details",
    "summary",
];

/// A page's readable text: scripts, styles and navigation dropped, the
/// `<article>` or `<main>` region preferred (whichever holds more text: some
/// sites use `<article>` only for small cards beside the real content), and
/// line breaks kept for paragraphs, headings and list items.
fn html_to_readable_text(html: &str) -> String {
    let lowered = html.to_ascii_lowercase();
    let best = ["article", "main"]
        .iter()
        .filter_map(|tag| {
            let start = lowered.find(&format!("<{tag}"))?;
            let end = lowered.rfind(&format!("</{tag}>"))?;
            (start < end).then(|| readable_text_of(&html[start..end]))
        })
        .max_by_key(|text| text.chars().count())
        .filter(|text| text.chars().count() >= 200);
    best.unwrap_or_else(|| readable_text_of(html))
}

/// The `>` that ends the tag starting at `start`, skipping any `>` inside a
/// quoted attribute (Tailwind classes like `class="[&>a]:hidden"`).
fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in html.as_bytes()[start + 1..].iter().enumerate() {
        match (quote, byte) {
            (None, b'"' | b'\'') => quote = Some(*byte),
            (Some(open), _) if open == *byte => quote = None,
            (None, b'>') => return Some(start + 1 + offset),
            _ => {}
        }
    }
    None
}

fn readable_text_of(html: &str) -> String {
    // ASCII lowercasing keeps byte offsets identical to `html`.
    let lowered = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut text = String::with_capacity(html.len() / 2);
    let mut index = 0;
    while index < html.len() {
        if bytes[index] != b'<' {
            let next = html[index..]
                .find('<')
                .map_or(html.len(), |off| index + off);
            text.push_str(&html[index..next]);
            index = next;
            continue;
        }
        if lowered[index..].starts_with("<!--") {
            index = lowered[index..]
                .find("-->")
                .map_or(html.len(), |off| index + off + 3);
            continue;
        }
        let Some(close) = tag_end(html, index) else {
            break;
        };
        let inner = &lowered[index + 1..close];
        let closing = inner.starts_with('/');
        let name = inner
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default();
        index = close + 1;
        if !closing && HTML_SKIPPED_ELEMENTS.contains(&name) && !inner.ends_with('/') {
            let end_tag = format!("</{name}");
            index = lowered[index..]
                .find(&end_tag)
                .and_then(|off| {
                    let at = index + off;
                    lowered[at..].find('>').map(|end| at + end + 1)
                })
                .unwrap_or(html.len());
            continue;
        }
        if name == "li" {
            // Only the opening tag breaks the line, so items stay adjacent.
            if !closing {
                text.push_str("\n- ");
            }
        } else if HTML_BLOCK_ELEMENTS.contains(&name) {
            text.push('\n');
        } else if matches!(name, "td" | "th") {
            text.push(' ');
        }
    }
    let decoded = decode_html_entities(&text);
    let mut lines: Vec<String> = Vec::new();
    let mut blank = false;
    for line in decoded.lines() {
        let line = collapse_whitespace(line);
        if line.is_empty() || line == "-" {
            if !blank && !lines.is_empty() {
                lines.push(String::new());
            }
            blank = true;
        } else {
            lines.push(line);
            blank = false;
        }
    }
    lines.join("\n").trim().to_string()
}

fn extract_title(content: &str, raw_body: &str, content_type: &str) -> Option<String> {
    if content_type.contains("html") {
        let lowered = raw_body.to_lowercase();
        if let Some(start) = lowered.find("<title>") {
            let after = start + "<title>".len();
            if let Some(end_rel) = lowered[after..].find("</title>") {
                let title =
                    collapse_whitespace(&decode_html_entities(&raw_body[after..after + end_rel]));
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut previous_was_space = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            '&' => {
                text.push('&');
                previous_was_space = false;
            }
            ch if ch.is_whitespace() => {
                if !previous_was_space {
                    text.push(' ');
                    previous_was_space = true;
                }
            }
            _ => {
                text.push(ch);
                previous_was_space = false;
            }
        }
    }

    collapse_whitespace(&decode_html_entities(&text))
}

fn decode_html_entities(input: &str) -> String {
    const NAMED: &[(&str, &str)] = &[
        ("lt", "<"),
        ("gt", ">"),
        ("quot", "\""),
        ("apos", "'"),
        ("nbsp", " "),
        ("rsquo", "\u{2019}"),
        ("lsquo", "\u{2018}"),
        ("rdquo", "\u{201d}"),
        ("ldquo", "\u{201c}"),
        ("mdash", "\u{2014}"),
        ("ndash", "\u{2013}"),
        ("hellip", "\u{2026}"),
        ("copy", "\u{a9}"),
        ("amp", "&"),
    ];
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after.find(';').filter(|semi| *semi <= 10).and_then(|semi| {
            let entity = &after[..semi];
            let ch = if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
            } else if let Some(dec) = entity.strip_prefix('#') {
                dec.parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
            } else {
                NAMED
                    .iter()
                    .find(|(name, _)| *name == entity)
                    .map(|(_, value)| (*value).to_string())
            };
            ch.map(|ch| (ch, semi))
        });
        match decoded {
            Some((ch, semi)) => {
                out.push_str(&ch);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_search_hits(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut remaining = html;

    while let Some(anchor_start) = remaining.find("result__a") {
        let after_class = &remaining[anchor_start..];
        let Some(href_idx) = after_class.find("href=") else {
            remaining = &after_class[1..];
            continue;
        };
        let href_slice = &after_class[href_idx + 5..];
        let Some((url, rest)) = extract_quoted_value(href_slice) else {
            remaining = &after_class[1..];
            continue;
        };
        let Some(close_tag_idx) = rest.find('>') else {
            remaining = &after_class[1..];
            continue;
        };
        let after_tag = &rest[close_tag_idx + 1..];
        let Some(end_anchor_idx) = after_tag.find("</a>") else {
            remaining = &after_tag[1..];
            continue;
        };
        let title = html_to_text(&after_tag[..end_anchor_idx]);
        let after_anchor = &after_tag[end_anchor_idx + 4..];
        if let Some(decoded_url) = decode_duckduckgo_redirect(&url) {
            hits.push(SearchHit {
                title: title.trim().to_string(),
                url: decoded_url,
                snippet: extract_result_snippet(after_anchor),
            });
        }
        remaining = after_anchor;
    }

    hits
}

/// The `result__snippet` excerpt that follows a result link, up to the next
/// result. Without it the model only sees titles and guesses the answer.
fn extract_result_snippet(html: &str) -> Option<String> {
    let scope = &html[..html.find("result__a").unwrap_or(html.len())];
    let start = scope.find("result__snippet")?;
    let after = &scope[start..];
    let open_end = after.find('>')? + 1;
    let body = &after[open_end..];
    let close = body.find("</a>").or_else(|| body.find("</div>"))?;
    let snippet = html_to_text(&body[..close]);
    let snippet = snippet.trim();
    (!snippet.is_empty()).then(|| snippet.to_string())
}

fn extract_search_hits_from_generic_links(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut remaining = html;

    while let Some(anchor_start) = remaining.find("<a") {
        let after_anchor = &remaining[anchor_start..];
        let Some(href_idx) = after_anchor.find("href=") else {
            remaining = &after_anchor[2..];
            continue;
        };
        let href_slice = &after_anchor[href_idx + 5..];
        let Some((url, rest)) = extract_quoted_value(href_slice) else {
            remaining = &after_anchor[2..];
            continue;
        };
        let Some(close_tag_idx) = rest.find('>') else {
            remaining = &after_anchor[2..];
            continue;
        };
        let after_tag = &rest[close_tag_idx + 1..];
        let Some(end_anchor_idx) = after_tag.find("</a>") else {
            remaining = &after_anchor[2..];
            continue;
        };
        let title = html_to_text(&after_tag[..end_anchor_idx]);
        if title.trim().is_empty() {
            remaining = &after_tag[end_anchor_idx + 4..];
            continue;
        }
        let decoded_url = decode_duckduckgo_redirect(&url).unwrap_or(url);
        if decoded_url.starts_with("http://") || decoded_url.starts_with("https://") {
            hits.push(SearchHit {
                title: title.trim().to_string(),
                url: decoded_url,
                snippet: None,
            });
        }
        remaining = &after_tag[end_anchor_idx + 4..];
    }

    hits
}

fn extract_quoted_value(input: &str) -> Option<(String, &str)> {
    let quote = input.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &input[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some((rest[..end].to_string(), &rest[end + quote.len_utf8()..]))
}

fn decode_duckduckgo_redirect(url: &str) -> Option<String> {
    let decoded = html_entity_decode_url(url);
    let parsed = if decoded.starts_with("http://") || decoded.starts_with("https://") {
        reqwest::Url::parse(&decoded).ok()
    } else if decoded.starts_with("//") {
        reqwest::Url::parse(&format!("https:{decoded}")).ok()
    } else if decoded.starts_with('/') {
        reqwest::Url::parse(&format!("https://duckduckgo.com{decoded}")).ok()
    } else {
        return None;
    }?;

    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if (host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
        && (parsed.path() == "/l/" || parsed.path() == "/l")
    {
        for (key, value) in parsed.query_pairs() {
            if key == "uddg" {
                return Some(html_entity_decode_url(value.as_ref()));
            }
        }
    }

    if decoded.starts_with("http://") || decoded.starts_with("https://") {
        Some(decoded)
    } else {
        Some(parsed.to_string())
    }
}

fn html_entity_decode_url(url: &str) -> String {
    decode_html_entities(url)
}

fn host_matches_list(url: &str, domains: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    domains.iter().any(|domain| {
        let normalized = normalize_domain_filter(domain);
        !normalized.is_empty() && (host == normalized || host.ends_with(&format!(".{normalized}")))
    })
}

fn normalize_domain_filter(domain: &str) -> String {
    let trimmed = domain.trim();
    let candidate = reqwest::Url::parse(trimmed)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| trimmed.to_string());
    candidate
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn dedupe_hits(hits: &mut Vec<SearchHit>) {
    let mut seen = BTreeSet::new();
    hits.retain(|hit| seen.insert(hit.url.clone()));
}

fn execute_todo_write(input: TodoWriteInput) -> Result<TodoWriteOutput, String> {
    validate_todos(&input.todos)?;
    let store_path = todo_store_path()?;
    let old_todos = if store_path.exists() {
        serde_json::from_str::<Vec<TodoItem>>(
            &std::fs::read_to_string(&store_path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
    } else {
        Vec::new()
    };

    let all_done = input
        .todos
        .iter()
        .all(|todo| matches!(todo.status, TodoStatus::Completed));
    let persisted = if all_done {
        Vec::new()
    } else {
        input.todos.clone()
    };

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&persisted).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    let verification_nudge_needed = (all_done
        && input.todos.len() >= 3
        && !input
            .todos
            .iter()
            .any(|todo| todo.content.to_lowercase().contains("verif")))
    .then_some(true);

    let events = todo_events::diff(&old_todos, &input.todos, todo_events::now_ms());
    todo_events::append(&store_path, &events);

    Ok(TodoWriteOutput {
        old_todos,
        new_todos: input.todos,
        verification_nudge_needed,
        events,
    })
}

fn execute_skill(input: SkillInput) -> Result<SkillOutput, String> {
    let skill_path = resolve_skill_path(&input.skill)?;
    let prompt = std::fs::read_to_string(&skill_path).map_err(|error| error.to_string())?;
    // Learned skills in use are kept, and shown to the next review.
    runtime::learning::record_skill_use(&input.skill);
    let description = parse_skill_description(&prompt);

    Ok(SkillOutput {
        skill: input.skill,
        path: skill_path.display().to_string(),
        args: input.args,
        description,
        prompt,
    })
}

fn validate_todos(todos: &[TodoItem]) -> Result<(), String> {
    if todos.is_empty() {
        return Err(String::from("todos must not be empty"));
    }
    // Allow multiple in_progress items for parallel workflows
    if todos.iter().any(|todo| todo.content.trim().is_empty()) {
        return Err(String::from("todo content must not be empty"));
    }
    if todos.iter().any(|todo| todo.active_form.trim().is_empty()) {
        return Err(String::from("todo activeForm must not be empty"));
    }
    Ok(())
}

/// The model's current todo list (what `/todos` shows), one item per line.
#[must_use]
pub fn render_todo_list() -> String {
    let todos = todo_store_path()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Vec<TodoItem>>(&text).ok())
        .unwrap_or_default();
    if todos.is_empty() {
        return "No todos. The model keeps a list here while it works through a multi-step task."
            .to_string();
    }
    let done = todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::Completed)
        .count();
    let mut lines = vec![format!("Todos ({done}/{} done)", todos.len())];
    for todo in &todos {
        let (mark, text) = match todo.status {
            TodoStatus::Completed => ("✔", &todo.content),
            TodoStatus::InProgress => ("◼", &todo.active_form),
            TodoStatus::Pending => ("◻", &todo.content),
        };
        lines.push(format!("  {mark} {text}"));
    }
    lines.join("\n")
}

fn todo_store_path() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("CLAWD_TODO_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(cwd.join(".clawd-todos.json"))
}

fn resolve_skill_path(skill: &str) -> Result<std::path::PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    match commands::resolve_skill_path(&cwd, skill) {
        Ok(path) => Ok(path),
        Err(_) => resolve_skill_path_from_compat_roots(skill),
    }
}

fn resolve_skill_path_from_compat_roots(skill: &str) -> Result<std::path::PathBuf, String> {
    let requested = skill.trim().trim_start_matches('/').trim_start_matches('$');
    if requested.is_empty() {
        return Err(String::from("skill must not be empty"));
    }

    for root in skill_lookup_roots() {
        if let Some(path) = resolve_skill_path_in_root(&root, requested) {
            return Ok(path);
        }
    }

    Err(format!("unknown skill: {requested}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillLookupOrigin {
    SkillsDir,
    LegacyCommandsDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillLookupRoot {
    path: std::path::PathBuf,
    origin: SkillLookupOrigin,
}

fn skill_lookup_roots() -> Vec<SkillLookupRoot> {
    let mut roots = Vec::new();

    if let Ok(cwd) = std::env::current_dir() {
        push_project_skill_lookup_roots(&mut roots, &cwd);
    }

    if let Ok(claw_config_home) = std::env::var("CLAW_CONFIG_HOME") {
        push_prefixed_skill_lookup_roots(&mut roots, std::path::Path::new(&claw_config_home));
    }
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        push_prefixed_skill_lookup_roots(&mut roots, std::path::Path::new(&codex_home));
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        push_home_skill_lookup_roots(&mut roots, std::path::Path::new(&home));
    }
    if let Ok(claude_config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let claude_config_dir = std::path::PathBuf::from(claude_config_dir);
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("skills"),
            SkillLookupOrigin::SkillsDir,
        );
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("skills").join("omc-learned"),
            SkillLookupOrigin::SkillsDir,
        );
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("commands"),
            SkillLookupOrigin::LegacyCommandsDir,
        );
    }
    roots
}

fn push_project_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, cwd: &std::path::Path) {
    for ancestor in cwd.ancestors() {
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".omc"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".agents"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".claw"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".codex"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".claude"));
    }
}

fn push_home_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, home: &std::path::Path) {
    push_prefixed_skill_lookup_roots(roots, &home.join(".omc"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".claw"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".codex"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".claude"));
    push_skill_lookup_root(
        roots,
        home.join(".agents").join("skills"),
        SkillLookupOrigin::SkillsDir,
    );
    push_skill_lookup_root(
        roots,
        home.join(".config").join("opencode").join("skills"),
        SkillLookupOrigin::SkillsDir,
    );
    push_skill_lookup_root(
        roots,
        home.join(".claude").join("skills").join("omc-learned"),
        SkillLookupOrigin::SkillsDir,
    );
}

fn push_prefixed_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, prefix: &std::path::Path) {
    push_skill_lookup_root(roots, prefix.join("skills"), SkillLookupOrigin::SkillsDir);
    push_skill_lookup_root(
        roots,
        prefix.join("commands"),
        SkillLookupOrigin::LegacyCommandsDir,
    );
}

fn push_skill_lookup_root(
    roots: &mut Vec<SkillLookupRoot>,
    path: std::path::PathBuf,
    origin: SkillLookupOrigin,
) {
    if path.is_dir() && !roots.iter().any(|existing| existing.path == path) {
        roots.push(SkillLookupRoot { path, origin });
    }
}

fn resolve_skill_path_in_root(
    root: &SkillLookupRoot,
    requested: &str,
) -> Option<std::path::PathBuf> {
    match root.origin {
        SkillLookupOrigin::SkillsDir => resolve_skill_path_in_skills_dir(&root.path, requested),
        SkillLookupOrigin::LegacyCommandsDir => {
            resolve_skill_path_in_legacy_commands_dir(&root.path, requested)
        }
    }
}

fn resolve_skill_path_in_skills_dir(
    root: &std::path::Path,
    requested: &str,
) -> Option<std::path::PathBuf> {
    let direct = root.join(requested).join("SKILL.md");
    if direct.is_file() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let skill_path = entry.path().join("SKILL.md");
        if !skill_path.is_file() {
            continue;
        }
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(requested)
            || skill_frontmatter_name_matches(&skill_path, requested)
        {
            return Some(skill_path);
        }
    }

    None
}

fn resolve_skill_path_in_legacy_commands_dir(
    root: &std::path::Path,
    requested: &str,
) -> Option<std::path::PathBuf> {
    let direct_dir = root.join(requested).join("SKILL.md");
    if direct_dir.is_file() {
        return Some(direct_dir);
    }

    let direct_markdown = root.join(format!("{requested}.md"));
    if direct_markdown.is_file() {
        return Some(direct_markdown);
    }

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let candidate_path = if path.is_dir() {
            let skill_path = path.join("SKILL.md");
            if !skill_path.is_file() {
                continue;
            }
            skill_path
        } else if path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
        {
            path
        } else {
            continue;
        };

        let matches_entry_name = candidate_path
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case(requested))
            || entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".md")
                .eq_ignore_ascii_case(requested);
        if matches_entry_name || skill_frontmatter_name_matches(&candidate_path, requested) {
            return Some(candidate_path);
        }
    }

    None
}

fn skill_frontmatter_name_matches(path: &std::path::Path, requested: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| parse_skill_name(&contents))
        .is_some_and(|name| name.eq_ignore_ascii_case(requested))
}

fn parse_skill_name(contents: &str) -> Option<String> {
    parse_skill_frontmatter_value(contents, "name")
}

fn parse_skill_frontmatter_value(contents: &str, key: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }

    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix(&format!("{key}:")) {
            let value = value
                .trim()
                .trim_matches(|ch| matches!(ch, '"' | '\''))
                .trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    None
}

const DEFAULT_AGENT_MODEL: &str = "claude-opus-4-6";
const DEFAULT_AGENT_SYSTEM_DATE: &str = "2026-03-31";
const DEFAULT_AGENT_MAX_ITERATIONS: usize = 32;

fn execute_agent(input: AgentInput) -> Result<AgentOutput, String> {
    execute_agent_with_spawn(input, spawn_agent_job)
}

fn execute_agent_with_spawn<F>(input: AgentInput, spawn_fn: F) -> Result<AgentOutput, String>
where
    F: FnOnce(AgentJob) -> Result<(), String>,
{
    if input.description.trim().is_empty() {
        return Err(String::from("description must not be empty"));
    }
    if input.prompt.trim().is_empty() {
        return Err(String::from("prompt must not be empty"));
    }

    let agent_id = make_agent_id();
    let output_dir = agent_store_dir()?;
    std::fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    let output_file = output_dir.join(format!("{agent_id}.md"));
    let manifest_file = output_dir.join(format!("{agent_id}.json"));
    let normalized_subagent_type = normalize_subagent_type(input.subagent_type.as_deref());
    // A name that is not a built-in kind is a custom agent definition
    // (`.claw/agents/`, `~/.claude/agents/`, …).
    let custom = if BUILTIN_SUBAGENT_TYPES.contains(&normalized_subagent_type.as_str()) {
        None
    } else {
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        Some(
            commands::find_custom_agent(&cwd, &normalized_subagent_type).ok_or_else(|| {
                let custom = commands::custom_agents(&cwd)
                    .into_iter()
                    .map(|agent| agent.name)
                    .collect::<Vec<_>>();
                format!(
                    "unknown subagent_type `{normalized_subagent_type}`: use Explore, Plan, Verification, fork, general-purpose{}",
                    if custom.is_empty() {
                        String::new()
                    } else {
                        format!(", or a custom agent: {}", custom.join(", "))
                    }
                )
            })?,
        )
    };
    // A custom agent's own model is used only when claw can route it
    // (provider/model); Claude Code's `sonnet`/`opus`/`inherit` mean this
    // session's model here.
    let custom_model = custom
        .as_ref()
        .and_then(|agent| agent.model.clone())
        .filter(|model| model.contains('/'));
    let model = resolve_agent_model(input.model.as_deref().or(custom_model.as_deref()));
    let agent_name = input
        .name
        .as_deref()
        .map(slugify_agent_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| slugify_agent_name(&input.description));
    let created_at = iso8601_now();
    let fork_seed = (normalized_subagent_type == "fork")
        .then(|| {
            FORK_SEED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .flatten();
    let (system_prompt, seed) = match (fork_seed, &custom) {
        // The parent's own system prompt, so the conversation reads as it
        // was written (and a local server can reuse its cached prefix).
        (Some((session, system_prompt)), _) => (system_prompt, Some(session)),
        (None, Some(agent)) => (
            build_agent_system_prompt_with_role(&agent.name, &model, &agent.prompt)?,
            None,
        ),
        (None, None) => (
            build_agent_system_prompt(&normalized_subagent_type, &model)?,
            None,
        ),
    };
    let allowed_tools = custom
        .as_ref()
        .and_then(|agent| agent.tools.as_deref())
        .map(custom_agent_tools)
        .filter(|tools| !tools.is_empty())
        .unwrap_or_else(|| allowed_tools_for_subagent(&normalized_subagent_type));

    let output_contents = format!(
        "# Agent Task

- id: {}
- name: {}
- description: {}
- subagent_type: {}
- created_at: {}

## Prompt

{}
",
        agent_id, agent_name, input.description, normalized_subagent_type, created_at, input.prompt
    );
    std::fs::write(&output_file, output_contents).map_err(|error| error.to_string())?;

    let manifest = AgentOutput {
        agent_id,
        name: agent_name,
        description: input.description,
        subagent_type: Some(normalized_subagent_type),
        model: Some(model),
        status: String::from("running"),
        output_file: output_file.display().to_string(),
        manifest_file: manifest_file.display().to_string(),
        created_at: created_at.clone(),
        started_at: Some(created_at),
        completed_at: None,
        lane_events: vec![LaneEvent::started(iso8601_now())],
        current_blocker: None,
        derived_state: String::from("working"),
        error: None,
    };
    write_agent_manifest(&manifest)?;

    let manifest_for_spawn = manifest.clone();
    let prompt = if seed.is_some() {
        format!("{FORK_NOTE}\n\n{}", input.prompt)
    } else {
        input.prompt
    };
    let job = AgentJob {
        manifest: manifest_for_spawn,
        prompt,
        system_prompt,
        allowed_tools,
        seed,
    };
    if let Err(error) = spawn_fn(job) {
        let error = format!("failed to spawn sub-agent: {error}");
        persist_agent_terminal_state(&manifest, "failed", None, Some(error.clone()))?;
        return Err(error);
    }

    Ok(manifest)
}

fn spawn_agent_job(job: AgentJob) -> Result<(), String> {
    let thread_name = format!("clawd-agent-{}", job.manifest.agent_id);
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_agent_job(&job)));
            let manifest = &job.manifest;
            let notice = match result {
                Ok(Ok(text)) => format!(
                    "Background agent {} ({}) completed. Result:\n{}",
                    manifest.agent_id,
                    manifest.description,
                    truncate_for_notice(&text, 4000)
                ),
                Ok(Err(error)) => {
                    let notice = format!(
                        "Background agent {} ({}) failed: {error}",
                        manifest.agent_id, manifest.description
                    );
                    let _ = persist_agent_terminal_state(manifest, "failed", None, Some(error));
                    notice
                }
                Err(_) => {
                    let _ = persist_agent_terminal_state(
                        manifest,
                        "failed",
                        None,
                        Some(String::from("sub-agent thread panicked")),
                    );
                    format!(
                        "Background agent {} ({}) failed: the sub-agent panicked",
                        manifest.agent_id, manifest.description
                    )
                }
            };
            runtime::global_background_jobs().push_notice(notice);
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Runs the sub-agent's conversation and returns its final answer.
fn run_agent_job(job: &AgentJob) -> Result<String, String> {
    local_tools::agents::started(job);
    let session = job.seed.clone().unwrap_or_default();
    let outcome = run_agent_conversation(job, session, job.prompt.clone());
    let final_text = local_tools::agents::finished(job, outcome)?;
    persist_agent_terminal_state(&job.manifest, "completed", Some(final_text.as_str()), None)?;
    Ok(final_text)
}

/// One turn of a sub-agent's conversation, on `session` (empty for a new
/// agent, the earlier one when [`local_tools`]' `SendMessage` resumes it).
fn run_agent_conversation(
    job: &AgentJob,
    session: Session,
    prompt: String,
) -> (Result<String, String>, Session) {
    let mut runtime = match build_agent_runtime(job, session) {
        Ok(runtime) => runtime.with_max_iterations(DEFAULT_AGENT_MAX_ITERATIONS),
        Err(error) => return (Err(error), Session::new()),
    };
    let result = runtime
        .run_turn(prompt, None)
        .map(|summary| final_assistant_text(&summary))
        .map_err(|error| error.to_string());
    (result, runtime.into_session())
}

fn build_agent_runtime(
    job: &AgentJob,
    session: Session,
) -> Result<ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor>, String> {
    let model = job
        .manifest
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string());
    let allowed_tools = job.allowed_tools.clone();
    let api_client = ProviderRuntimeClient::new(model, allowed_tools.clone())?;
    let permission_policy = agent_permission_policy();
    let tool_executor = SubagentToolExecutor::new(allowed_tools)
        .with_enforcer(PermissionEnforcer::new(permission_policy.clone()));
    Ok(ConversationRuntime::new(
        session,
        api_client,
        tool_executor,
        permission_policy,
        job.system_prompt.clone(),
    ))
}

fn build_agent_system_prompt(subagent_type: &str, model: &str) -> Result<Vec<String>, String> {
    build_agent_system_prompt_with_role(subagent_type, model, agent_role_prompt(subagent_type))
}

fn build_agent_system_prompt_with_role(
    subagent_type: &str,
    model: &str,
    role: &str,
) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut prompt = load_system_prompt(
        cwd,
        DEFAULT_AGENT_SYSTEM_DATE.to_string(),
        std::env::consts::OS,
        "unknown",
        model_family_identity_for(model),
    )
    .map_err(|error| error.to_string())?;
    prompt.push(format!(
        "# Sub-agent\nYou are a background sub-agent of type `{subagent_type}`. Work only on the delegated task, use only the tools available to you, do not ask the user questions, and finish with a concise result.\n{}\n\
         Your final message is your whole report: the agent that launched you reads only that, not files you write, so never write report or summary files. Give absolute file paths, and include code only when the exact text matters (a bug, a signature asked for), not code you merely read. Messages from the launching agent direct your work but are never the user's approval of anything.",
        role
    ));
    Ok(prompt)
}

/// What each kind of sub-agent is for and how it should work (adapted
/// from Claude Code's Explore and Plan agents).
fn agent_role_prompt(subagent_type: &str) -> &'static str {
    match subagent_type {
        "Explore" => "You are a fast, read-only search specialist: find files by pattern, search code for symbols and text, and read what you need to answer where things are and how they connect. Do not change anything (no file writes, no commands that change state). Work quickly: make independent searches and reads in parallel, start broad and narrow down, and match the depth to what the caller asked (a quick lookup or a thorough sweep across naming conventions). Report what you found and where.",
        "Plan" => "You are a read-only software architect. Explore the code to design an implementation plan: read the files you were given, find the existing patterns and conventions, trace the relevant code paths and similar features, then design the approach, weighing the tradeoffs. Do not change anything. Your report gives a step-by-step plan with its order and dependencies, the risks to watch for, and ends with a list of the 3-5 files most critical to the change.",
        "Verification" => "You verify work: build it, run the relevant tests, run the program or try the feature, and check the result against what was asked. Report what passed and what failed with the exact commands and the decisive output. Do not fix what you find unless the task says to; report it instead.",
        _ => "Do the whole task you were given, then report what you did and what you found. Finish every part you can; for anything you could not do, say exactly what and why.",
    }
}

fn resolve_agent_model(model: Option<&str>) -> String {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            DEFAULT_AGENT_MODEL_OVERRIDE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string())
}

fn allowed_tools_for_subagent(subagent_type: &str) -> BTreeSet<String> {
    let tools = match subagent_type {
        "Explore" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
        ],
        "Plan" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "Verification" => vec![
            "bash",
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
            "PowerShell",
        ],
        "claw-guide" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "statusline-setup" => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "ToolSearch",
        ],
        _ => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "Download",
            "SocialMedia",
            "TodoWrite",
            "Skill",
            "ToolSearch",
            "NotebookEdit",
            "write_docx",
            "write_pptx",
            "write_xlsx",
            "Sleep",
            "SendUserMessage",
            "Config",
            "StructuredOutput",
            "REPL",
            "PowerShell",
        ],
    };
    tools.into_iter().map(canonical_allowed_tool_name).collect()
}

/// The sub-agent kinds claw defines itself; any other `subagent_type` is a
/// custom agent definition.
const BUILTIN_SUBAGENT_TYPES: [&str; 7] = [
    "general-purpose",
    "Explore",
    "Plan",
    "Verification",
    "fork",
    "claw-guide",
    "statusline-setup",
];

/// The claw tools for a custom agent's `tools:` list, which may use Claude
/// Code's names (`Read`, `Grep`, `Bash`, …) or claw's; unknown names are
/// left out.
fn custom_agent_tools(tools: &[String]) -> BTreeSet<String> {
    let known = mvp_tool_specs()
        .into_iter()
        .map(|spec| canonical_allowed_tool_name(spec.name))
        .collect::<BTreeSet<_>>();
    tools
        .iter()
        .flat_map(|tool| {
            let mapped: &[&str] = match tool.trim() {
                "Read" => &["read_file"],
                "Write" => &["write_file"],
                "Edit" | "MultiEdit" => &["edit_file"],
                "Glob" | "LS" => &["glob_search"],
                "Grep" => &["grep_search"],
                "Bash" => &["bash", "bash_output", "kill_shell"],
                "Task" => &["Agent"],
                other => return vec![canonical_allowed_tool_name(other)],
            };
            mapped
                .iter()
                .map(|name| canonical_allowed_tool_name(name))
                .collect()
        })
        .filter(|tool| known.contains(tool))
        .collect()
}

fn agent_permission_policy() -> PermissionPolicy {
    mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(PermissionMode::DangerFullAccess),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    )
}

fn write_agent_manifest(manifest: &AgentOutput) -> Result<(), String> {
    let mut normalized = manifest.clone();
    normalized.lane_events = dedupe_superseded_commit_events(&normalized.lane_events);
    std::fs::write(
        &normalized.manifest_file,
        serde_json::to_string_pretty(&normalized).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn persist_agent_terminal_state(
    manifest: &AgentOutput,
    status: &str,
    result: Option<&str>,
    error: Option<String>,
) -> Result<(), String> {
    let blocker = error.as_deref().map(classify_lane_blocker);
    append_agent_output(
        &manifest.output_file,
        &format_agent_terminal_output(status, result, blocker.as_ref(), error.as_deref()),
    )?;
    let mut next_manifest = manifest.clone();
    next_manifest.status = status.to_string();
    next_manifest.completed_at = Some(iso8601_now());
    next_manifest.current_blocker.clone_from(&blocker);
    next_manifest.derived_state =
        derive_agent_state(status, result, error.as_deref(), blocker.as_ref()).to_string();
    next_manifest.error = error;
    if let Some(blocker) = blocker {
        next_manifest
            .lane_events
            .push(LaneEvent::blocked(iso8601_now(), &blocker));
        next_manifest
            .lane_events
            .push(LaneEvent::failed(iso8601_now(), &blocker));
    } else {
        next_manifest.current_blocker = None;
        let mut finished_summary = build_lane_finished_summary(&next_manifest, result);
        finished_summary.data.disabled_cron_ids = disable_matching_crons(&next_manifest, result);
        next_manifest.lane_events.push(
            LaneEvent::finished(iso8601_now(), finished_summary.detail).with_data(
                serde_json::to_value(&finished_summary.data)
                    .expect("lane summary metadata should serialize"),
            ),
        );
        if let Some(provenance) = maybe_commit_provenance(result) {
            next_manifest.lane_events.push(LaneEvent::commit_created(
                iso8601_now(),
                Some(format!("commit {}", provenance.commit)),
                provenance,
            ));
        }
    }
    write_agent_manifest(&next_manifest)
}

const MIN_LANE_SUMMARY_WORDS: usize = 7;
const REVIEW_VERDICTS: &[(&str, &str)] = &[
    ("APPROVE", "approve"),
    ("REJECT", "reject"),
    ("BLOCKED", "blocked"),
];
const CONTROL_ONLY_SUMMARY_WORDS: &[&str] = &[
    "ack",
    "commit",
    "continue",
    "everyting",
    "everything",
    "keep",
    "next",
    "push",
    "ralph",
    "resume",
    "retry",
    "run",
    "stop",
    "sweep",
    "sweeping",
    "team",
];
const CONTEXTUAL_SUMMARY_WORDS: &[&str] = &[
    "added",
    "audited",
    "blocked",
    "completed",
    "documented",
    "failed",
    "finished",
    "fixed",
    "implemented",
    "investigated",
    "merged",
    "pushed",
    "refactored",
    "removed",
    "reviewed",
    "tested",
    "updated",
    "verified",
];

#[derive(Debug, Clone, Serialize)]
struct LaneFinishedSummaryData {
    #[serde(rename = "qualityFloorApplied")]
    quality_floor_applied: bool,
    reasons: Vec<String>,
    #[serde(rename = "rawSummary", skip_serializing_if = "Option::is_none")]
    raw_summary: Option<String>,
    #[serde(rename = "wordCount")]
    word_count: usize,
    #[serde(rename = "reviewVerdict", skip_serializing_if = "Option::is_none")]
    review_verdict: Option<String>,
    #[serde(rename = "reviewTarget", skip_serializing_if = "Option::is_none")]
    review_target: Option<String>,
    #[serde(rename = "reviewRationale", skip_serializing_if = "Option::is_none")]
    review_rationale: Option<String>,
    #[serde(rename = "selectionOutcome", skip_serializing_if = "Option::is_none")]
    selection_outcome: Option<SelectionOutcome>,
    #[serde(rename = "recoveryOutcome", skip_serializing_if = "Option::is_none")]
    recovery_outcome: Option<RecoveryOutcome>,
    #[serde(rename = "artifactProvenance", skip_serializing_if = "Option::is_none")]
    artifact_provenance: Option<ArtifactProvenance>,
    #[serde(rename = "disabledCronIds", skip_serializing_if = "Vec::is_empty")]
    disabled_cron_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct LaneFinishedSummary {
    detail: Option<String>,
    data: LaneFinishedSummaryData,
}

#[derive(Debug)]
struct LaneSummaryAssessment {
    apply_quality_floor: bool,
    reasons: Vec<String>,
    word_count: usize,
    review_outcome: Option<ReviewLaneOutcome>,
    recovery_outcome: Option<RecoveryOutcome>,
}

#[derive(Debug, Clone)]
struct ReviewLaneOutcome {
    verdict: String,
    rationale: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SelectionOutcome {
    #[serde(rename = "chosenItems", skip_serializing_if = "Vec::is_empty")]
    chosen_items: Vec<String>,
    #[serde(rename = "skippedItems", skip_serializing_if = "Vec::is_empty")]
    skipped_items: Vec<String>,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rationale: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RecoveryOutcome {
    cause: String,
    #[serde(rename = "targetLane", skip_serializing_if = "Option::is_none")]
    target_lane: Option<String>,
    #[serde(rename = "preservedState", skip_serializing_if = "Option::is_none")]
    preserved_state: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ArtifactProvenance {
    #[serde(rename = "sourceLanes", skip_serializing_if = "Vec::is_empty")]
    source_lanes: Vec<String>,
    #[serde(rename = "roadmapIds", skip_serializing_if = "Vec::is_empty")]
    roadmap_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<String>,
    #[serde(rename = "diffStat", skip_serializing_if = "Option::is_none")]
    diff_stat: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    verification: Vec<String>,
    #[serde(rename = "commitSha", skip_serializing_if = "Option::is_none")]
    commit_sha: Option<String>,
}

fn build_lane_finished_summary(
    manifest: &AgentOutput,
    result: Option<&str>,
) -> LaneFinishedSummary {
    let raw_summary = result.map(str::trim).filter(|value| !value.is_empty());
    let assessment = assess_lane_summary_quality(raw_summary.unwrap_or_default());
    let detail = match raw_summary {
        Some(summary) if !assessment.apply_quality_floor => Some(compress_summary_text(summary)),
        Some(summary) => Some(compose_lane_summary_fallback(
            manifest,
            Some(summary),
            assessment.recovery_outcome.as_ref(),
        )),
        None => Some(compose_lane_summary_fallback(manifest, None, None)),
    };
    let review_outcome = assessment.review_outcome.clone();
    let recovery_outcome = assessment.recovery_outcome.clone();
    let review_target = review_outcome
        .as_ref()
        .map(|_| manifest.description.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let artifact_provenance = extract_artifact_provenance(manifest, raw_summary);

    LaneFinishedSummary {
        detail,
        data: LaneFinishedSummaryData {
            quality_floor_applied: raw_summary.is_none() || assessment.apply_quality_floor,
            reasons: assessment.reasons,
            raw_summary: raw_summary.map(str::to_string),
            word_count: assessment.word_count,
            review_verdict: review_outcome
                .as_ref()
                .map(|outcome| outcome.verdict.clone()),
            review_target,
            review_rationale: review_outcome.and_then(|outcome| outcome.rationale),
            selection_outcome: extract_selection_outcome(raw_summary.unwrap_or_default()),
            recovery_outcome,
            artifact_provenance,
            disabled_cron_ids: Vec::new(),
        },
    }
}

fn assess_lane_summary_quality(summary: &str) -> LaneSummaryAssessment {
    let words = summary
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '#'))
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();

    let word_count = words.len();
    let mut reasons = Vec::new();
    if summary.trim().is_empty() {
        reasons.push(String::from("empty"));
    }

    let review_outcome = extract_review_outcome(summary);
    let recovery_outcome = extract_recovery_outcome(summary);
    if recovery_outcome.is_some() {
        reasons.push(String::from("recovery_control_prose"));
    }

    let control_only = !words.is_empty()
        && words
            .iter()
            .all(|word| CONTROL_ONLY_SUMMARY_WORDS.contains(&word.as_str()));
    if control_only && review_outcome.is_none() {
        reasons.push(String::from("control_only"));
    }

    let has_context_signal = summary.contains('`')
        || summary.contains('/')
        || summary.contains(':')
        || summary.contains('#')
        || review_outcome.is_some()
        || words
            .iter()
            .any(|word| CONTEXTUAL_SUMMARY_WORDS.contains(&word.as_str()));
    if word_count < MIN_LANE_SUMMARY_WORDS && !has_context_signal {
        reasons.push(String::from("too_short_without_context"));
    }

    LaneSummaryAssessment {
        apply_quality_floor: !reasons.is_empty(),
        reasons,
        word_count,
        review_outcome,
        recovery_outcome,
    }
}

fn compose_lane_summary_fallback(
    manifest: &AgentOutput,
    raw_summary: Option<&str>,
    recovery_outcome: Option<&RecoveryOutcome>,
) -> String {
    let target = manifest.description.trim();
    let base = format!(
        "Completed lane `{}` for target: {}. Status: completed.",
        manifest.name,
        if target.is_empty() {
            "unspecified task"
        } else {
            target
        }
    );
    if let Some(outcome) = recovery_outcome {
        let mut detail = format!(
            "{base} Recovery handoff observed via tmux reinjection (cause: `{}`).",
            outcome.cause
        );
        if let Some(target_lane) = &outcome.target_lane {
            let _ = std::fmt::Write::write_fmt(
                &mut detail,
                format_args!(" Target lane: `{target_lane}`."),
            );
        }
        if let Some(preserved_state) = &outcome.preserved_state {
            let _ = std::fmt::Write::write_fmt(
                &mut detail,
                format_args!(" Preserved state: {preserved_state}."),
            );
        }
        return detail;
    }
    match raw_summary {
        Some(summary) => format!(
            "{base} Original stop summary was too vague to keep as the lane result: \"{}\".",
            summary.trim()
        ),
        None => format!("{base} No usable stop summary was produced by the lane."),
    }
}

fn extract_review_outcome(summary: &str) -> Option<ReviewLaneOutcome> {
    let mut lines = summary
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next()?;
    let verdict = REVIEW_VERDICTS.iter().find_map(|(prefix, verdict)| {
        first
            .eq_ignore_ascii_case(prefix)
            .then(|| (*verdict).to_string())
    })?;
    let rationale = lines.collect::<Vec<_>>().join(" ").trim().to_string();
    Some(ReviewLaneOutcome {
        verdict,
        rationale: (!rationale.is_empty()).then_some(compress_summary_text(&rationale)),
    })
}

fn extract_selection_outcome(summary: &str) -> Option<SelectionOutcome> {
    let mut chosen_items = Vec::new();
    let mut skipped_items = Vec::new();
    let mut action = None;
    let mut rationale = None;

    for line in summary
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let lowered = line.to_ascii_lowercase();
        let roadmap_items = extract_roadmap_items(line);

        if lowered.starts_with("chosen:")
            || lowered.starts_with("picked:")
            || lowered.starts_with("selected:")
            || (lowered.contains("picked") && !roadmap_items.is_empty())
            || (lowered.contains("selected") && !roadmap_items.is_empty())
        {
            chosen_items.extend(roadmap_items);
        } else if lowered.starts_with("skipped:")
            || lowered.starts_with("skip:")
            || (lowered.contains("skipped") && !roadmap_items.is_empty())
        {
            skipped_items.extend(roadmap_items);
        }

        if let Some(rest) = lowered.strip_prefix("action:") {
            if rest.contains("execute") || rest.contains("implement") || rest.contains("fix") {
                action = Some(String::from("execute"));
            } else if rest.contains("review") || rest.contains("audit") {
                action = Some(String::from("review"));
            } else if rest.contains("no-op") || rest.contains("noop") {
                action = Some(String::from("no-op"));
            }
        }

        if let Some(rest) = line.strip_prefix("Rationale:") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                rationale = Some(compress_summary_text(trimmed));
            }
        }
    }

    chosen_items.sort();
    chosen_items.dedup();
    skipped_items.sort();
    skipped_items.dedup();

    if chosen_items.is_empty() && skipped_items.is_empty() && action.is_none() {
        return None;
    }

    let default_action = if chosen_items.is_empty() {
        String::from("no-op")
    } else {
        String::from("execute")
    };

    Some(SelectionOutcome {
        chosen_items,
        skipped_items,
        action: action.unwrap_or(default_action),
        rationale,
    })
}

fn extract_recovery_outcome(summary: &str) -> Option<RecoveryOutcome> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lowered = trimmed.to_ascii_lowercase();
    let has_tmux_inject_marker = lowered.contains("omx_tmux_inject");
    let has_recovery_phrase = lowered.contains("continue from current mode state")
        || (lowered.starts_with("team ") && lowered.contains(" next:"));
    if !has_tmux_inject_marker && !has_recovery_phrase {
        return None;
    }

    let cause = if lowered.contains("current mode state") {
        "resume_after_stop"
    } else if lowered.contains("tool failure") {
        "retry_after_tool_failure"
    } else if lowered.contains("worker panes stalled")
        || lowered.contains("no progress")
        || lowered.contains("leader stale")
        || lowered.contains("all workers idle")
        || lowered.contains("all 1 worker idle")
        || lowered.contains("pane(s) active")
    {
        "tmux_reinject_after_idle"
    } else {
        "manual_recovery"
    };

    let target_lane = trimmed.lines().map(str::trim).find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("team ") {
            return None;
        }
        line[5..]
            .split_once(':')
            .map(|(name, _)| name.trim())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    });

    let preserved_state = lowered
        .contains("current mode state")
        .then(|| String::from("current mode state"));

    Some(RecoveryOutcome {
        cause: cause.to_string(),
        target_lane,
        preserved_state,
    })
}

fn extract_roadmap_items(line: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '#' {
            let mut digits = String::new();
            while let Some(next) = chars.peek() {
                if next.is_ascii_digit() {
                    digits.push(*next);
                    chars.next();
                } else {
                    break;
                }
            }
            if !digits.is_empty() {
                items.push(format!("ROADMAP #{digits}"));
            }
        }
    }
    items
}

fn extract_artifact_provenance(
    manifest: &AgentOutput,
    raw_summary: Option<&str>,
) -> Option<ArtifactProvenance> {
    let summary = raw_summary?;
    let mut roadmap_ids = extract_roadmap_items(summary);
    roadmap_ids.extend(extract_roadmap_items(&manifest.description));
    roadmap_ids.sort();
    roadmap_ids.dedup();

    let mut files = extract_file_paths(summary);
    files.sort();
    files.dedup();

    let mut verification = Vec::new();
    let lowered = summary.to_ascii_lowercase();
    for (needle, label) in [
        ("tested", "tested"),
        ("committed", "committed"),
        ("pushed", "pushed"),
        ("merged", "merged"),
    ] {
        if lowered.contains(needle) {
            verification.push(label.to_string());
        }
    }

    let commit_sha = extract_commit_sha(summary);
    let diff_stat = extract_diff_stat(summary);
    let source_lanes = vec![manifest.name.clone()];

    if roadmap_ids.is_empty()
        && files.is_empty()
        && verification.is_empty()
        && commit_sha.is_none()
        && diff_stat.is_none()
    {
        return None;
    }

    Some(ArtifactProvenance {
        source_lanes,
        roadmap_ids,
        files,
        diff_stat,
        verification,
        commit_sha,
    })
}

fn extract_file_paths(summary: &str) -> Vec<String> {
    summary
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '(' | ')' | '[' | ']'))
        .map(|token| {
            token
                .trim_matches('`')
                .trim_matches('"')
                .trim_matches('\'')
                .trim_end_matches('.')
        })
        .filter(|token| {
            token.contains('.')
                && !token.starts_with("http")
                && !token
                    .chars()
                    .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == '+' || ch == '-')
        })
        .map(str::to_string)
        .collect()
}

fn extract_diff_stat(summary: &str) -> Option<String> {
    summary
        .split('\n')
        .map(str::trim)
        .find_map(|line| {
            line.find("Diff stat:")
                .map(|index| normalize_diff_stat(&line[(index + "Diff stat:".len())..]))
                .or_else(|| {
                    line.find("Diff:")
                        .map(|index| normalize_diff_stat(&line[(index + "Diff:".len())..]))
                })
        })
        .filter(|value| !value.is_empty())
}

fn normalize_diff_stat(value: &str) -> String {
    let trimmed = value.trim();
    for marker in [" Tested", " Committed", " committed", " pushed", " merged"] {
        if let Some((prefix, _)) = trimmed.split_once(marker) {
            return prefix.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn disable_matching_crons(manifest: &AgentOutput, result: Option<&str>) -> Vec<String> {
    let tokens = cron_match_tokens(manifest, result);
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut disabled = Vec::new();
    for entry in global_cron_registry().list(true) {
        let haystack = format!(
            "{} {}",
            entry.prompt,
            entry.description.as_deref().unwrap_or_default()
        )
        .to_ascii_lowercase();
        if tokens.iter().any(|token| haystack.contains(token))
            && global_cron_registry().disable(&entry.cron_id).is_ok()
        {
            disabled.push(entry.cron_id);
        }
    }
    disabled.sort();
    disabled
}

fn cron_match_tokens(manifest: &AgentOutput, result: Option<&str>) -> Vec<String> {
    let mut tokens = extract_roadmap_items(manifest.description.as_str())
        .into_iter()
        .chain(extract_roadmap_items(result.unwrap_or_default()))
        .map(|item| item.to_ascii_lowercase())
        .collect::<Vec<_>>();

    if tokens.is_empty() && !manifest.name.trim().is_empty() {
        tokens.push(manifest.name.trim().to_ascii_lowercase());
    }

    tokens.sort();
    tokens.dedup();
    tokens
}

fn derive_agent_state(
    status: &str,
    result: Option<&str>,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
) -> &'static str {
    let normalized_status = status.trim().to_ascii_lowercase();
    let normalized_error = error.unwrap_or_default().to_ascii_lowercase();

    if normalized_status == "running" {
        return "working";
    }
    if normalized_status == "completed" {
        return if result.is_some_and(|value| !value.trim().is_empty()) {
            "finished_cleanable"
        } else {
            "finished_pending_report"
        };
    }
    if normalized_error.contains("background") {
        return "blocked_background_job";
    }
    if normalized_error.contains("merge conflict") || normalized_error.contains("cherry-pick") {
        return "blocked_merge_conflict";
    }
    if normalized_error.contains("mcp") {
        return "degraded_mcp";
    }
    if normalized_error.contains("transport")
        || normalized_error.contains("broken pipe")
        || normalized_error.contains("connection")
        || normalized_error.contains("interrupted")
    {
        return "interrupted_transport";
    }
    if blocker.is_some() {
        return "truly_idle";
    }
    "truly_idle"
}

fn maybe_commit_provenance(result: Option<&str>) -> Option<LaneCommitProvenance> {
    let commit = extract_commit_sha(result?)?;
    let branch = current_git_branch().unwrap_or_else(|| "unknown".to_string());
    let worktree = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string());
    Some(LaneCommitProvenance {
        commit: commit.clone(),
        branch,
        worktree,
        canonical_commit: Some(commit.clone()),
        superseded_by: None,
        lineage: vec![commit],
    })
}

fn extract_commit_sha(result: &str) -> Option<String> {
    result
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|token| token.len() >= 7 && token.len() <= 40)
        .map(str::to_string)
}

fn current_git_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn append_agent_output(path: &str, suffix: &str) -> Result<(), String> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(suffix.as_bytes())
        .map_err(|error| error.to_string())
}

fn format_agent_terminal_output(
    status: &str,
    result: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
    error: Option<&str>,
) -> String {
    let mut sections = vec![format!("\n## Result\n\n- status: {status}\n")];
    if let Some(blocker) = blocker {
        sections.push(format!(
            "\n### Blocker\n\n- failure_class: {}\n- detail: {}\n",
            serde_json::to_string(&blocker.failure_class)
                .unwrap_or_else(|_| "\"infra\"".to_string())
                .trim_matches('"'),
            blocker.detail.trim()
        ));
    }
    if let Some(result) = result.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Final response\n\n{}\n", result.trim()));
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Error\n\n{}\n", error.trim()));
    }
    sections.join("")
}

fn classify_lane_blocker(error: &str) -> LaneEventBlocker {
    let detail = error.trim().to_string();
    LaneEventBlocker {
        failure_class: classify_lane_failure(error),
        detail,
        subphase: None,
    }
}

fn classify_lane_failure(error: &str) -> LaneFailureClass {
    let normalized = error.to_ascii_lowercase();

    if normalized.contains("prompt") && normalized.contains("deliver") {
        LaneFailureClass::PromptDelivery
    } else if normalized.contains("trust") {
        LaneFailureClass::TrustGate
    } else if normalized.contains("branch")
        && (normalized.contains("stale") || normalized.contains("diverg"))
    {
        LaneFailureClass::BranchDivergence
    } else if normalized.contains("gateway") || normalized.contains("routing") {
        LaneFailureClass::GatewayRouting
    } else if normalized.contains("compile")
        || normalized.contains("build failed")
        || normalized.contains("cargo check")
    {
        LaneFailureClass::Compile
    } else if normalized.contains("test") {
        LaneFailureClass::Test
    } else if normalized.contains("tool failed")
        || normalized.contains("runtime tool")
        || normalized.contains("tool runtime")
    {
        LaneFailureClass::ToolRuntime
    } else if normalized.contains("workspace") && normalized.contains("mismatch") {
        LaneFailureClass::WorkspaceMismatch
    } else if normalized.contains("plugin") {
        LaneFailureClass::PluginStartup
    } else if normalized.contains("mcp") && normalized.contains("handshake") {
        LaneFailureClass::McpHandshake
    } else if normalized.contains("mcp") {
        LaneFailureClass::McpStartup
    } else {
        LaneFailureClass::Infra
    }
}

struct ProviderEntry {
    model: String,
    client: ProviderClient,
}

struct ProviderRuntimeClient {
    runtime: tokio::runtime::Runtime,
    chain: Vec<ProviderEntry>,
    allowed_tools: BTreeSet<String>,
}

impl ProviderRuntimeClient {
    #[allow(clippy::needless_pass_by_value)]
    fn new(model: String, allowed_tools: BTreeSet<String>) -> Result<Self, String> {
        let fallback_config = load_provider_fallback_config();
        Self::new_with_fallback_config(model, allowed_tools, &fallback_config)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn new_with_fallback_config(
        model: String,
        allowed_tools: BTreeSet<String>,
        fallback_config: &ProviderFallbackConfig,
    ) -> Result<Self, String> {
        let primary_model = fallback_config.primary().map_or(model, str::to_string);
        let primary = build_provider_entry(&primary_model)?;
        let mut chain = vec![primary];
        for fallback_model in fallback_config.fallbacks() {
            match build_provider_entry(fallback_model) {
                Ok(entry) => chain.push(entry),
                Err(error) => {
                    eprintln!(
                        "warning: skipping unavailable fallback provider {fallback_model}: {error}"
                    );
                }
            }
        }
        Ok(Self {
            runtime: tokio::runtime::Runtime::new().map_err(|error| error.to_string())?,
            chain,
            allowed_tools,
        })
    }
}

fn build_provider_entry(model: &str) -> Result<ProviderEntry, String> {
    let resolved = resolve_model_alias(model).clone();
    let client = ProviderClient::from_model(&resolved).map_err(|error| error.to_string())?;
    Ok(ProviderEntry {
        model: resolved,
        client,
    })
}

fn load_provider_fallback_config() -> ProviderFallbackConfig {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| ConfigLoader::default_for(cwd).load().ok())
        .map_or_else(ProviderFallbackConfig::default, |config| {
            config.provider_fallbacks().clone()
        })
}

impl ApiClient for ProviderRuntimeClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let tools = tool_specs_for_allowed_tools(Some(&self.allowed_tools))
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            })
            .collect::<Vec<_>>();
        let messages = convert_messages(&request.messages);
        let system =
            (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n"));
        let tool_choice = (!self.allowed_tools.is_empty()).then_some(ToolChoice::Auto);

        let runtime = &self.runtime;
        let chain = &self.chain;
        let mut last_error: Option<ApiError> = None;
        for (index, entry) in chain.iter().enumerate() {
            let message_request = MessageRequest {
                model: entry.model.clone(),
                max_tokens: max_tokens_for_model(&entry.model),
                messages: messages.clone(),
                system: system.clone(),
                tools: (!tools.is_empty()).then(|| tools.clone()),
                tool_choice: tool_choice.clone(),
                stream: true,
                ..Default::default()
            };

            let attempt = runtime.block_on(stream_with_provider(&entry.client, &message_request));
            match attempt {
                Ok(events) => return Ok(events),
                Err(error) if error.is_retryable() && index + 1 < chain.len() => {
                    eprintln!(
                        "provider {} failed with retryable error, falling back: {error}",
                        entry.model
                    );
                    last_error = Some(error);
                }
                Err(error) => return Err(RuntimeError::new(error.to_string())),
            }
        }

        Err(RuntimeError::new(last_error.map_or_else(
            || String::from("provider chain exhausted with no attempts"),
            |error| error.to_string(),
        )))
    }
}

#[allow(clippy::too_many_lines, clippy::result_large_err)]
async fn stream_with_provider(
    client: &ProviderClient,
    message_request: &MessageRequest,
) -> Result<Vec<AssistantEvent>, ApiError> {
    let mut stream = client.stream_message(message_request).await?;
    let mut events = Vec::new();
    let mut pending_tools: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut pending_thinking: BTreeMap<u32, (String, Option<String>)> = BTreeMap::new();
    let mut saw_stop = false;

    while let Some(event) = stream.next_event().await? {
        match event {
            ApiStreamEvent::MessageStart(start) => {
                for block in start.message.content {
                    push_output_block(
                        block,
                        0,
                        &mut events,
                        &mut pending_tools,
                        &mut pending_thinking,
                        true,
                    );
                }
            }
            ApiStreamEvent::ContentBlockStart(start) => {
                push_output_block(
                    start.content_block,
                    start.index,
                    &mut events,
                    &mut pending_tools,
                    &mut pending_thinking,
                    true,
                );
            }
            ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                ContentBlockDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        events.push(AssistantEvent::TextDelta(text));
                    }
                }
                ContentBlockDelta::InputJsonDelta { partial_json } => {
                    if let Some((_, _, input)) = pending_tools.get_mut(&delta.index) {
                        input.push_str(&partial_json);
                    }
                }
                ContentBlockDelta::ThinkingDelta { thinking } => {
                    if let Some((pending, _)) = pending_thinking.get_mut(&delta.index) {
                        pending.push_str(&thinking);
                    }
                }
                ContentBlockDelta::SignatureDelta { signature } => {
                    if let Some((_, pending_signature)) = pending_thinking.get_mut(&delta.index) {
                        pending_signature
                            .get_or_insert_with(String::new)
                            .push_str(&signature);
                    }
                }
            },
            ApiStreamEvent::ContentBlockStop(stop) => {
                if let Some((thinking, signature)) = pending_thinking.remove(&stop.index) {
                    events.push(AssistantEvent::Thinking {
                        thinking,
                        signature,
                    });
                }
                if let Some((id, name, input)) = pending_tools.remove(&stop.index) {
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
                    events.push(AssistantEvent::OutputLimitReached);
                }
                events.push(AssistantEvent::Usage(delta.usage.token_usage()));
            }
            ApiStreamEvent::MessageStop(_) => {
                saw_stop = true;
                events.push(AssistantEvent::MessageStop);
            }
        }
    }

    push_prompt_cache_record(client, &mut events);

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

    let response = client
        .send_message(&MessageRequest {
            stream: false,
            ..message_request.clone()
        })
        .await?;
    let mut events = response_to_events(response);
    push_prompt_cache_record(client, &mut events);
    Ok(events)
}

struct SubagentToolExecutor {
    allowed_tools: BTreeSet<String>,
    enforcer: Option<PermissionEnforcer>,
}

impl SubagentToolExecutor {
    fn new(allowed_tools: BTreeSet<String>) -> Self {
        Self {
            allowed_tools,
            enforcer: None,
        }
    }

    fn with_enforcer(mut self, enforcer: PermissionEnforcer) -> Self {
        self.enforcer = Some(enforcer);
        self
    }
}

impl ToolExecutor for SubagentToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if !self
            .allowed_tools
            .contains(&canonical_allowed_tool_name(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled for this sub-agent"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        execute_tool_with_enforcer(self.enforcer.as_ref(), tool_name, &value)
            .map_err(ToolError::new)
    }
}

fn tool_specs_for_allowed_tools(allowed_tools: Option<&BTreeSet<String>>) -> Vec<ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| {
            allowed_tools
                .is_none_or(|allowed| allowed.contains(&canonical_allowed_tool_name(spec.name)))
        })
        .collect()
}

fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => InputContentBlock::Text { text: text.clone() },
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => InputContentBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: signature.clone(),
                    },
                    ContentBlock::ToolUse { id, name, input } => InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::from_str(input)
                            .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } => InputContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: vec![ToolResultContentBlock::Text {
                            text: output.clone(),
                        }],
                        is_error: *is_error,
                    },
                    ContentBlock::Image {
                        media_type, data, ..
                    } => InputContentBlock::Image {
                        source: ImageSource::base64(media_type.clone(), data.clone()),
                    },
                })
                .filter(
                    |block| !matches!(block, InputContentBlock::Text { text } if text.is_empty()),
                )
                .collect::<Vec<_>>();
            (!content.is_empty()).then(|| InputMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect()
}

fn push_output_block(
    block: OutputContentBlock,
    block_index: u32,
    events: &mut Vec<AssistantEvent>,
    pending_tools: &mut BTreeMap<u32, (String, String, String)>,
    pending_thinking: &mut BTreeMap<u32, (String, Option<String>)>,
    streaming_tool_input: bool,
) {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            pending_tools.insert(block_index, (id, name, initial_input));
        }
        OutputContentBlock::Thinking {
            thinking,
            signature,
        } => {
            if streaming_tool_input {
                pending_thinking.insert(block_index, (thinking, signature));
            } else {
                events.push(AssistantEvent::Thinking {
                    thinking,
                    signature,
                });
            }
        }
        OutputContentBlock::RedactedThinking { .. } => {}
    }
}

fn response_to_events(response: MessageResponse) -> Vec<AssistantEvent> {
    let mut events = Vec::new();
    let mut pending_tools = BTreeMap::new();
    let mut pending_thinking = BTreeMap::new();

    for (index, block) in response.content.into_iter().enumerate() {
        let index = u32::try_from(index).expect("response block index overflow");
        push_output_block(
            block,
            index,
            &mut events,
            &mut pending_tools,
            &mut pending_thinking,
            false,
        );
        if let Some((id, name, input)) = pending_tools.remove(&index) {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    if response
        .stop_reason
        .as_deref()
        .is_some_and(runtime::is_output_limit_stop_reason)
    {
        events.push(AssistantEvent::OutputLimitReached);
    }
    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    events
}

fn push_prompt_cache_record(client: &ProviderClient, events: &mut Vec<AssistantEvent>) {
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_tool_search(input: ToolSearchInput) -> ToolSearchOutput {
    GlobalToolRegistry::builtin().search(&input.query, input.max_results.unwrap_or(5), None, None)
}

fn deferred_tool_specs() -> Vec<ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| {
            !matches!(
                spec.name,
                "bash" | "read_file" | "write_file" | "edit_file" | "glob_search" | "grep_search"
            )
        })
        .collect()
}

fn search_tool_specs(query: &str, max_results: usize, specs: &[SearchableToolSpec]) -> Vec<String> {
    let lowered = query.to_lowercase();
    if let Some(selection) = lowered.strip_prefix("select:") {
        return selection
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(|wanted| {
                let wanted = canonical_tool_token(wanted);
                specs
                    .iter()
                    .find(|spec| canonical_tool_token(&spec.name) == wanted)
                    .map(|spec| spec.name.clone())
            })
            .take(max_results)
            .collect();
    }

    let mut required = Vec::new();
    let mut optional = Vec::new();
    for term in lowered.split_whitespace() {
        if let Some(rest) = term.strip_prefix('+') {
            if !rest.is_empty() {
                required.push(rest);
            }
        } else {
            optional.push(term);
        }
    }
    let terms = if required.is_empty() {
        optional.clone()
    } else {
        required.iter().chain(optional.iter()).copied().collect()
    };

    let mut scored = specs
        .iter()
        .filter_map(|spec| {
            let name = spec.name.to_lowercase();
            let canonical_name = canonical_tool_token(&spec.name);
            let normalized_description = normalize_tool_search_query(&spec.description);
            let haystack = format!(
                "{name} {} {canonical_name}",
                spec.description.to_lowercase()
            );
            let normalized_haystack = format!("{canonical_name} {normalized_description}");
            if required.iter().any(|term| !haystack.contains(term)) {
                return None;
            }

            let mut score = 0_i32;
            for term in &terms {
                let canonical_term = canonical_tool_token(term);
                if haystack.contains(term) {
                    score += 2;
                }
                if name == *term {
                    score += 8;
                }
                if name.contains(term) {
                    score += 4;
                }
                if canonical_name == canonical_term {
                    score += 12;
                }
                if normalized_haystack.contains(&canonical_term) {
                    score += 3;
                }
            }

            if score == 0 && !lowered.is_empty() {
                return None;
            }
            Some((score, spec.name.clone()))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored
        .into_iter()
        .map(|(_, name)| name)
        .take(max_results)
        .collect()
}

fn normalize_tool_search_query(query: &str) -> String {
    query
        .trim()
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .filter(|term| !term.is_empty())
        .map(canonical_tool_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn canonical_tool_token(value: &str) -> String {
    let mut canonical = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if let Some(stripped) = canonical.strip_suffix("tool") {
        canonical = stripped.to_string();
    }
    canonical
}

fn agent_store_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("CLAWD_AGENT_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    if let Some(workspace_root) = cwd.ancestors().nth(2) {
        return Ok(workspace_root.join(".clawd-agents"));
    }
    Ok(cwd.join(".clawd-agents"))
}

fn make_agent_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("agent-{nanos}")
}

fn slugify_agent_name(description: &str) -> String {
    let mut out = description
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(32).collect()
}

fn normalize_subagent_type(subagent_type: Option<&str>) -> String {
    let trimmed = subagent_type.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return String::from("general-purpose");
    }

    match canonical_tool_token(trimmed).as_str() {
        "general" | "generalpurpose" | "generalpurposeagent" => String::from("general-purpose"),
        "explore" | "explorer" | "exploreagent" => String::from("Explore"),
        "plan" | "planagent" => String::from("Plan"),
        "verification" | "verificationagent" | "verify" | "verifier" => {
            String::from("Verification")
        }
        "fork" | "forked" | "forkagent" => String::from("fork"),
        "clawguide" | "clawguideagent" | "guide" => String::from("claw-guide"),
        "statusline" | "statuslinesetup" => String::from("statusline-setup"),
        _ => trimmed.to_string(),
    }
}

fn iso8601_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[allow(clippy::too_many_lines)]
fn execute_notebook_edit(input: NotebookEditInput) -> Result<NotebookEditOutput, String> {
    let path = std::path::PathBuf::from(&input.notebook_path);
    if path.extension().and_then(|ext| ext.to_str()) != Some("ipynb") {
        return Err(String::from(
            "File must be a Jupyter notebook (.ipynb file).",
        ));
    }

    let original_file = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut notebook: serde_json::Value =
        serde_json::from_str(&original_file).map_err(|error| error.to_string())?;
    let language = notebook
        .get("metadata")
        .and_then(|metadata| metadata.get("kernelspec"))
        .and_then(|kernelspec| kernelspec.get("language"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("python")
        .to_string();
    let cells = notebook
        .get_mut("cells")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| String::from("Notebook cells array not found"))?;

    let edit_mode = input.edit_mode.unwrap_or(NotebookEditMode::Replace);
    let target_index = match input.cell_id.as_deref() {
        Some(cell_id) => Some(resolve_cell_index(cells, Some(cell_id), edit_mode)?),
        None if matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        ) =>
        {
            Some(resolve_cell_index(cells, None, edit_mode)?)
        }
        None => None,
    };
    let resolved_cell_type = match edit_mode {
        NotebookEditMode::Delete => None,
        NotebookEditMode::Insert => Some(input.cell_type.unwrap_or(NotebookCellType::Code)),
        NotebookEditMode::Replace => Some(input.cell_type.unwrap_or_else(|| {
            target_index
                .and_then(|index| cells.get(index))
                .and_then(cell_kind)
                .unwrap_or(NotebookCellType::Code)
        })),
    };
    let new_source = require_notebook_source(input.new_source, edit_mode)?;

    let cell_id = match edit_mode {
        NotebookEditMode::Insert => {
            let resolved_cell_type = resolved_cell_type
                .ok_or_else(|| String::from("insert mode requires a cell type"))?;
            let new_id = make_cell_id(cells.len());
            let new_cell = build_notebook_cell(&new_id, resolved_cell_type, &new_source);
            let insert_at = target_index.map_or(cells.len(), |index| index + 1);
            cells.insert(insert_at, new_cell);
            cells
                .get(insert_at)
                .and_then(|cell| cell.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Delete => {
            let idx = target_index
                .ok_or_else(|| String::from("delete mode requires a target cell index"))?;
            let removed = cells.remove(idx);
            removed
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Replace => {
            let resolved_cell_type = resolved_cell_type
                .ok_or_else(|| String::from("replace mode requires a cell type"))?;
            let idx = target_index
                .ok_or_else(|| String::from("replace mode requires a target cell index"))?;
            let cell = cells
                .get_mut(idx)
                .ok_or_else(|| String::from("Cell index out of range"))?;
            cell["source"] = serde_json::Value::Array(source_lines(&new_source));
            cell["cell_type"] = serde_json::Value::String(match resolved_cell_type {
                NotebookCellType::Code => String::from("code"),
                NotebookCellType::Markdown => String::from("markdown"),
            });
            match resolved_cell_type {
                NotebookCellType::Code => {
                    if !cell.get("outputs").is_some_and(serde_json::Value::is_array) {
                        cell["outputs"] = json!([]);
                    }
                    if cell.get("execution_count").is_none() {
                        cell["execution_count"] = serde_json::Value::Null;
                    }
                }
                NotebookCellType::Markdown => {
                    if let Some(object) = cell.as_object_mut() {
                        object.remove("outputs");
                        object.remove("execution_count");
                    }
                }
            }
            cell.get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
    };

    let updated_file =
        serde_json::to_string_pretty(&notebook).map_err(|error| error.to_string())?;
    std::fs::write(&path, &updated_file).map_err(|error| error.to_string())?;

    Ok(NotebookEditOutput {
        new_source,
        cell_id,
        cell_type: resolved_cell_type,
        language,
        edit_mode: format_notebook_edit_mode(edit_mode),
        error: None,
        notebook_path: path.display().to_string(),
        original_file,
        updated_file,
    })
}

fn require_notebook_source(
    source: Option<String>,
    edit_mode: NotebookEditMode,
) -> Result<String, String> {
    match edit_mode {
        NotebookEditMode::Delete => Ok(source.unwrap_or_default()),
        NotebookEditMode::Insert | NotebookEditMode::Replace => source
            .ok_or_else(|| String::from("new_source is required for insert and replace edits")),
    }
}

fn build_notebook_cell(cell_id: &str, cell_type: NotebookCellType, source: &str) -> Value {
    let mut cell = json!({
        "cell_type": match cell_type {
            NotebookCellType::Code => "code",
            NotebookCellType::Markdown => "markdown",
        },
        "id": cell_id,
        "metadata": {},
        "source": source_lines(source),
    });
    if let Some(object) = cell.as_object_mut() {
        match cell_type {
            NotebookCellType::Code => {
                object.insert(String::from("outputs"), json!([]));
                object.insert(String::from("execution_count"), Value::Null);
            }
            NotebookCellType::Markdown => {}
        }
    }
    cell
}

fn cell_kind(cell: &serde_json::Value) -> Option<NotebookCellType> {
    cell.get("cell_type")
        .and_then(serde_json::Value::as_str)
        .map(|kind| {
            if kind == "markdown" {
                NotebookCellType::Markdown
            } else {
                NotebookCellType::Code
            }
        })
}

const MAX_SLEEP_DURATION_MS: u64 = 300_000;

#[allow(clippy::needless_pass_by_value)]
fn execute_sleep(input: SleepInput) -> Result<SleepOutput, String> {
    if input.duration_ms > MAX_SLEEP_DURATION_MS {
        return Err(format!(
            "duration_ms {} exceeds maximum allowed sleep of {MAX_SLEEP_DURATION_MS}ms",
            input.duration_ms,
        ));
    }
    std::thread::sleep(Duration::from_millis(input.duration_ms));
    Ok(SleepOutput {
        duration_ms: input.duration_ms,
        message: format!("Slept for {}ms", input.duration_ms),
    })
}

fn execute_brief(input: BriefInput) -> Result<BriefOutput, String> {
    if input.message.trim().is_empty() {
        return Err(String::from("message must not be empty"));
    }

    let attachments = input
        .attachments
        .as_ref()
        .map(|paths| {
            paths
                .iter()
                .map(|path| resolve_attachment(path))
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?;

    let message = match input.status {
        BriefStatus::Normal | BriefStatus::Proactive => input.message,
    };

    Ok(BriefOutput {
        message,
        attachments,
        sent_at: iso8601_timestamp(),
    })
}

fn resolve_attachment(path: &str) -> Result<ResolvedAttachment, String> {
    let resolved = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| error.to_string())?;
    Ok(ResolvedAttachment {
        path: resolved.display().to_string(),
        size: metadata.len(),
        is_image: is_image_path(&resolved),
    })
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg")
    )
}

fn execute_config(input: ConfigInput) -> Result<ConfigOutput, String> {
    let setting = input.setting.trim();
    if setting.is_empty() {
        return Err(String::from("setting must not be empty"));
    }
    let Some(spec) = supported_config_setting(setting) else {
        return Ok(ConfigOutput {
            success: false,
            operation: None,
            setting: None,
            value: None,
            previous_value: None,
            new_value: None,
            error: Some(format!("Unknown setting: \"{setting}\"")),
        });
    };

    let path = config_file_for_scope(spec.scope)?;
    let mut document = read_json_object(&path)?;

    if let Some(value) = input.value {
        let normalized = normalize_config_value(spec, value)?;
        let previous_value = get_nested_value(&document, spec.path).cloned();
        set_nested_value(&mut document, spec.path, normalized.clone());
        write_json_object(&path, &document)?;
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("set")),
            setting: Some(setting.to_string()),
            value: Some(normalized.clone()),
            previous_value,
            new_value: Some(normalized),
            error: None,
        })
    } else {
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("get")),
            setting: Some(setting.to_string()),
            value: get_nested_value(&document, spec.path).cloned(),
            previous_value: None,
            new_value: None,
            error: None,
        })
    }
}

const PERMISSION_DEFAULT_MODE_PATH: &[&str] = &["permissions", "defaultMode"];

fn execute_enter_plan_mode(_input: EnterPlanModeInput) -> Result<PlanModeOutput, String> {
    let settings_path = config_file_for_scope(ConfigScope::Settings)?;
    let state_path = plan_mode_state_file()?;
    let mut document = read_json_object(&settings_path)?;
    let current_local_mode = get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned();
    let current_is_plan =
        matches!(current_local_mode.as_ref(), Some(Value::String(value)) if value == "plan");

    if let Some(state) = read_plan_mode_state(&state_path)? {
        if current_is_plan {
            return Ok(PlanModeOutput {
                success: true,
                operation: String::from("enter"),
                changed: false,
                active: true,
                managed: true,
                message: String::from("Plan mode override is already active for this worktree."),
                settings_path: settings_path.display().to_string(),
                state_path: state_path.display().to_string(),
                previous_local_mode: state.previous_local_mode,
                current_local_mode,
            });
        }
        clear_plan_mode_state(&state_path)?;
    }

    if current_is_plan {
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("enter"),
            changed: false,
            active: true,
            managed: false,
            message: String::from(
                "Worktree-local plan mode is already enabled outside EnterPlanMode; leaving it unchanged.",
            ),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: None,
            current_local_mode,
        });
    }

    let state = PlanModeState {
        had_local_override: current_local_mode.is_some(),
        previous_local_mode: current_local_mode.clone(),
    };
    write_plan_mode_state(&state_path, &state)?;
    set_nested_value(
        &mut document,
        PERMISSION_DEFAULT_MODE_PATH,
        Value::String(String::from("plan")),
    );
    write_json_object(&settings_path, &document)?;

    Ok(PlanModeOutput {
        success: true,
        operation: String::from("enter"),
        changed: true,
        active: true,
        managed: true,
        message: String::from("Enabled worktree-local plan mode override."),
        settings_path: settings_path.display().to_string(),
        state_path: state_path.display().to_string(),
        previous_local_mode: state.previous_local_mode,
        current_local_mode: get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned(),
    })
}

fn execute_exit_plan_mode(_input: ExitPlanModeInput) -> Result<PlanModeOutput, String> {
    let settings_path = config_file_for_scope(ConfigScope::Settings)?;
    let state_path = plan_mode_state_file()?;
    let mut document = read_json_object(&settings_path)?;
    let current_local_mode = get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned();
    let current_is_plan =
        matches!(current_local_mode.as_ref(), Some(Value::String(value)) if value == "plan");

    let Some(state) = read_plan_mode_state(&state_path)? else {
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("exit"),
            changed: false,
            active: current_is_plan,
            managed: false,
            message: String::from("No EnterPlanMode override is active for this worktree."),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: None,
            current_local_mode,
        });
    };

    if !current_is_plan {
        clear_plan_mode_state(&state_path)?;
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("exit"),
            changed: false,
            active: false,
            managed: false,
            message: String::from(
                "Cleared stale EnterPlanMode state because plan mode was already changed outside the tool.",
            ),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: state.previous_local_mode,
            current_local_mode,
        });
    }

    if state.had_local_override {
        if let Some(previous_local_mode) = state.previous_local_mode.clone() {
            set_nested_value(
                &mut document,
                PERMISSION_DEFAULT_MODE_PATH,
                previous_local_mode,
            );
        } else {
            remove_nested_value(&mut document, PERMISSION_DEFAULT_MODE_PATH);
        }
    } else {
        remove_nested_value(&mut document, PERMISSION_DEFAULT_MODE_PATH);
    }
    write_json_object(&settings_path, &document)?;
    clear_plan_mode_state(&state_path)?;

    Ok(PlanModeOutput {
        success: true,
        operation: String::from("exit"),
        changed: true,
        active: false,
        managed: false,
        message: String::from("Restored the prior worktree-local plan mode setting."),
        settings_path: settings_path.display().to_string(),
        state_path: state_path.display().to_string(),
        previous_local_mode: state.previous_local_mode,
        current_local_mode: get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned(),
    })
}

fn execute_structured_output(
    input: StructuredOutputInput,
) -> Result<StructuredOutputResult, String> {
    if input.0.is_empty() {
        return Err(String::from("structured output payload must not be empty"));
    }
    Ok(StructuredOutputResult {
        data: String::from("Structured output provided successfully"),
        structured_output: input.0,
    })
}

fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

fn execute_repl(input: ReplInput) -> Result<ReplOutput, String> {
    if input.code.trim().is_empty() {
        return Err(String::from("code must not be empty"));
    }
    let runtime = resolve_repl_runtime(&input.language)?;
    let started = Instant::now();
    let mut process = Command::new(runtime.program);
    process
        .args(runtime.args)
        .arg(&input.code)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = if let Some(timeout_ms) = input.timeout_ms {
        let mut child = process.spawn().map_err(|error| error.to_string())?;
        // Drain both pipes while waiting; otherwise a child that writes more
        // than the pipe buffer blocks until the timeout fires.
        let stdout_reader = spawn_pipe_reader(child.stdout.take());
        let stderr_reader = spawn_pipe_reader(child.stderr.take());
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                break status;
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                child.kill().map_err(|error| error.to_string())?;
                let _ = child.wait();
                return Err(format!(
                    "REPL execution exceeded timeout of {timeout_ms} ms"
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        std::process::Output {
            status,
            stdout: stdout_reader.join().unwrap_or_default(),
            stderr: stderr_reader.join().unwrap_or_default(),
        }
    } else {
        process
            .spawn()
            .map_err(|error| error.to_string())?
            .wait_with_output()
            .map_err(|error| error.to_string())?
    };

    Ok(ReplOutput {
        language: input.language,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(1),
        duration_ms: started.elapsed().as_millis(),
    })
}

struct ReplRuntime {
    program: &'static str,
    args: &'static [&'static str],
}

fn resolve_repl_runtime(language: &str) -> Result<ReplRuntime, String> {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "py" => Ok(ReplRuntime {
            program: detect_first_command(&["python3", "python"])
                .ok_or_else(|| String::from("python runtime not found"))?,
            args: &["-c"],
        }),
        "javascript" | "js" | "node" => Ok(ReplRuntime {
            program: detect_first_command(&["node"])
                .ok_or_else(|| String::from("node runtime not found"))?,
            args: &["-e"],
        }),
        "sh" | "shell" | "bash" => Ok(ReplRuntime {
            program: detect_first_command(&["bash", "sh"])
                .ok_or_else(|| String::from("shell runtime not found"))?,
            args: &["-lc"],
        }),
        other => Err(format!("unsupported REPL language: {other}")),
    }
}

fn detect_first_command(commands: &[&'static str]) -> Option<&'static str> {
    commands
        .iter()
        .copied()
        .find(|command| command_exists(command))
}

#[derive(Clone, Copy)]
enum ConfigScope {
    Global,
    Settings,
}

#[derive(Clone, Copy)]
struct ConfigSettingSpec {
    scope: ConfigScope,
    kind: ConfigKind,
    path: &'static [&'static str],
    options: Option<&'static [&'static str]>,
}

#[derive(Clone, Copy)]
enum ConfigKind {
    Boolean,
    String,
}

fn supported_config_setting(setting: &str) -> Option<ConfigSettingSpec> {
    Some(match setting {
        "theme" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["theme"],
            options: None,
        },
        "editorMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["editorMode"],
            options: Some(&["default", "vim", "emacs"]),
        },
        "verbose" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["verbose"],
            options: None,
        },
        "preferredNotifChannel" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["preferredNotifChannel"],
            options: None,
        },
        "autoCompactEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["autoCompactEnabled"],
            options: None,
        },
        "autoMemoryEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoMemoryEnabled"],
            options: None,
        },
        "autoDreamEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoDreamEnabled"],
            options: None,
        },
        "fileCheckpointingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["fileCheckpointingEnabled"],
            options: None,
        },
        "showTurnDuration" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["showTurnDuration"],
            options: None,
        },
        "terminalProgressBarEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["terminalProgressBarEnabled"],
            options: None,
        },
        "todoFeatureEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["todoFeatureEnabled"],
            options: None,
        },
        "model" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["model"],
            options: None,
        },
        "alwaysThinkingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["alwaysThinkingEnabled"],
            options: None,
        },
        "permissions.defaultMode" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["permissions", "defaultMode"],
            options: Some(&["default", "plan", "acceptEdits", "dontAsk", "auto"]),
        },
        "language" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["language"],
            options: None,
        },
        "teammateMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["teammateMode"],
            options: Some(&["tmux", "in-process", "auto"]),
        },
        _ => return None,
    })
}

fn normalize_config_value(spec: ConfigSettingSpec, value: ConfigValue) -> Result<Value, String> {
    let normalized = match (spec.kind, value) {
        (ConfigKind::Boolean, ConfigValue::Bool(value)) => Value::Bool(value),
        (ConfigKind::Boolean, ConfigValue::String(value)) => {
            match value.trim().to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(String::from("setting requires true or false")),
            }
        }
        (ConfigKind::Boolean, ConfigValue::Number(_)) => {
            return Err(String::from("setting requires true or false"))
        }
        (ConfigKind::String, ConfigValue::String(value)) => Value::String(value),
        (ConfigKind::String, ConfigValue::Bool(value)) => Value::String(value.to_string()),
        (ConfigKind::String, ConfigValue::Number(value)) => json!(value),
    };

    if let Some(options) = spec.options {
        let Some(as_str) = normalized.as_str() else {
            return Err(String::from("setting requires a string value"));
        };
        if !options.iter().any(|option| option == &as_str) {
            return Err(format!(
                "Invalid value \"{as_str}\". Options: {}",
                options.join(", ")
            ));
        }
    }

    Ok(normalized)
}

fn config_file_for_scope(scope: ConfigScope) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(match scope {
        ConfigScope::Global => config_home_dir()?.join("settings.json"),
        ConfigScope::Settings => cwd.join(".claw").join("settings.local.json"),
    })
}

fn config_home_dir() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("CLAW_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            String::from(
                "HOME is not set (on Windows, set USERPROFILE or HOME, \
                 or use CLAW_CONFIG_HOME to point directly at the config directory)",
            )
        })?;
    Ok(PathBuf::from(home).join(".claw"))
}

fn read_json_object(path: &Path) -> Result<serde_json::Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(serde_json::Map::new());
            }
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| error.to_string())?
                .as_object()
                .cloned()
                .ok_or_else(|| String::from("config file must contain a JSON object"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_json_object(path: &Path, value: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn get_nested_value<'a>(
    value: &'a serde_json::Map<String, Value>,
    path: &[&str],
) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut current = value.get(*first)?;
    for key in rest {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

fn set_nested_value(root: &mut serde_json::Map<String, Value>, path: &[&str], new_value: Value) {
    let (first, rest) = path.split_first().expect("config path must not be empty");
    if rest.is_empty() {
        root.insert((*first).to_string(), new_value);
        return;
    }

    let entry = root
        .entry((*first).to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(serde_json::Map::new());
    }
    let map = entry.as_object_mut().expect("object inserted");
    set_nested_value(map, rest, new_value);
}

fn remove_nested_value(root: &mut serde_json::Map<String, Value>, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return root.remove(*first).is_some();
    }

    let mut should_remove_parent = false;
    let removed = root.get_mut(*first).is_some_and(|entry| {
        entry.as_object_mut().is_some_and(|map| {
            let removed = remove_nested_value(map, rest);
            should_remove_parent = removed && map.is_empty();
            removed
        })
    });

    if should_remove_parent {
        root.remove(*first);
    }

    removed
}

fn plan_mode_state_file() -> Result<PathBuf, String> {
    Ok(config_file_for_scope(ConfigScope::Settings)?
        .parent()
        .ok_or_else(|| String::from("settings.local.json has no parent directory"))?
        .join("tool-state")
        .join("plan-mode.json"))
}

fn read_plan_mode_state(path: &Path) -> Result<Option<PlanModeState>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(None);
            }
            serde_json::from_str(&contents)
                .map(Some)
                .map_err(|error| error.to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_plan_mode_state(path: &Path, state: &PlanModeState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(state).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn clear_plan_mode_state(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn iso8601_timestamp() -> String {
    if let Ok(output) = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
    {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    iso8601_now()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_powershell(input: PowerShellInput) -> std::io::Result<runtime::BashCommandOutput> {
    let _ = &input.description;
    if let Some(output) = workspace_test_branch_preflight(&input.command) {
        return Ok(output);
    }
    let shell = detect_powershell_shell()?;
    execute_shell_command(
        shell,
        &input.command,
        input.timeout,
        input.run_in_background,
    )
}

fn detect_powershell_shell() -> std::io::Result<&'static str> {
    if command_exists("pwsh") {
        Ok("pwsh")
    } else if command_exists("powershell") {
        Ok("powershell")
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "PowerShell executable not found (expected `pwsh` or `powershell` in PATH)",
        ))
    }
}

fn command_exists(command: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {command} >/dev/null 2>&1"))
        .status()
        .is_ok_and(|status| status.success())
}

#[allow(clippy::too_many_lines)]
fn execute_shell_command(
    shell: &str,
    command: &str,
    timeout: Option<u64>,
    run_in_background: Option<bool>,
) -> std::io::Result<runtime::BashCommandOutput> {
    if run_in_background.unwrap_or(false) {
        let child = std::process::Command::new(shell)
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        return Ok(runtime::BashCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(child.id().to_string()),
            backgrounded_by_user: Some(true),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: None,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: None,
        });
    }

    let mut process = std::process::Command::new(shell);
    process
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(command);
    process
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(timeout_ms) = timeout {
        let mut child = process.spawn()?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let output = child.wait_with_output()?;
                return Ok(runtime::BashCommandOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    raw_output_path: None,
                    interrupted: false,
                    is_image: None,
                    background_task_id: None,
                    backgrounded_by_user: None,
                    assistant_auto_backgrounded: None,
                    dangerously_disable_sandbox: None,
                    return_code_interpretation: status
                        .code()
                        .filter(|code| *code != 0)
                        .map(|code| format!("exit_code:{code}")),
                    no_output_expected: Some(output.stdout.is_empty() && output.stderr.is_empty()),
                    structured_content: None,
                    persisted_output_path: None,
                    persisted_output_size: None,
                    sandbox_status: None,
                });
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                let _ = child.kill();
                let output = child.wait_with_output()?;
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let stderr = if stderr.trim().is_empty() {
                    format!("Command exceeded timeout of {timeout_ms} ms")
                } else {
                    format!(
                        "{}
Command exceeded timeout of {timeout_ms} ms",
                        stderr.trim_end()
                    )
                };
                let is_test = is_test_command(command);
                let return_code_interpretation = if is_test { "test.hung" } else { "timeout" };
                return Ok(runtime::BashCommandOutput {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr,
                    raw_output_path: None,
                    interrupted: true,
                    is_image: None,
                    background_task_id: None,
                    backgrounded_by_user: None,
                    assistant_auto_backgrounded: None,
                    dangerously_disable_sandbox: None,
                    return_code_interpretation: Some(String::from(return_code_interpretation)),
                    no_output_expected: Some(false),
                    structured_content: Some(vec![test_timeout_provenance(
                        command, timeout_ms, is_test,
                    )]),
                    persisted_output_path: None,
                    persisted_output_size: None,
                    sandbox_status: None,
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let output = process.output()?;
    Ok(runtime::BashCommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: None,
        return_code_interpretation: output
            .status
            .code()
            .filter(|code| *code != 0)
            .map(|code| format!("exit_code:{code}")),
        no_output_expected: Some(output.stdout.is_empty() && output.stderr.is_empty()),
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: None,
    })
}

fn is_test_command(command: &str) -> bool {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    normalized.contains("cargo test")
        || normalized.contains("cargo nextest")
        || normalized.contains("npm test")
        || normalized.contains("pnpm test")
        || normalized.contains("yarn test")
        || normalized.contains("pytest")
}

fn test_timeout_provenance(
    command: &str,
    timeout_ms: u64,
    classified_as_test_hang: bool,
) -> serde_json::Value {
    json!({
        "event": if classified_as_test_hang { "test.hung" } else { "command.timeout" },
        "failureClass": if classified_as_test_hang { "test_hang" } else { "timeout" },
        "data": {
            "command": command,
            "timeoutMs": timeout_ms,
            "provenance": "shell.timeout",
            "classification": if classified_as_test_hang { "test.hung" } else { "timeout" }
        }
    })
}

fn resolve_cell_index(
    cells: &[serde_json::Value],
    cell_id: Option<&str>,
    edit_mode: NotebookEditMode,
) -> Result<usize, String> {
    if cells.is_empty()
        && matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        )
    {
        return Err(String::from("Notebook has no cells to edit"));
    }
    if let Some(cell_id) = cell_id {
        cells
            .iter()
            .position(|cell| cell.get("id").and_then(serde_json::Value::as_str) == Some(cell_id))
            .ok_or_else(|| format!("Cell id not found: {cell_id}"))
    } else {
        Ok(cells.len().saturating_sub(1))
    }
}

fn source_lines(source: &str) -> Vec<serde_json::Value> {
    if source.is_empty() {
        return vec![serde_json::Value::String(String::new())];
    }
    source
        .split_inclusive('\n')
        .map(|line| serde_json::Value::String(line.to_string()))
        .collect()
}

fn format_notebook_edit_mode(mode: NotebookEditMode) -> String {
    match mode {
        NotebookEditMode::Replace => String::from("replace"),
        NotebookEditMode::Insert => String::from("insert"),
        NotebookEditMode::Delete => String::from("delete"),
    }
}

fn make_cell_id(index: usize) -> String {
    format!("cell-{}", index + 1)
}

fn parse_skill_description(contents: &str) -> Option<String> {
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("description:") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

mod apply_patch;
mod arg_coercion;
mod browser;
#[cfg(unix)]
mod code_exec;
mod code_search;
mod com;
mod dev_tools;
mod download;
mod external_agent;
mod hawkscan;
pub mod lane_completion;
pub mod leangate;
pub mod local_tools;
mod lsp_tool;
mod media;
pub mod pdf_extract;
mod pty_tool;
mod search_fallback;
mod session_search;
mod signals;
mod social;
mod todo_events;

/// Fingerprints a memory anchor's code, for `runtime::set_anchor_hasher`.
#[must_use]
pub fn memory_anchor_hash(
    root: &std::path::Path,
    path: &str,
    symbol: Option<&str>,
) -> Option<String> {
    code_search::anchor_hash(root, path, symbol)
}

/// One lock for every test in this crate that changes the environment
/// (`CLAW_CONFIG_HOME`, `HOME`, …), so they cannot interleave.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn test_env_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
mod source_adapters;
mod x11;

#[cfg(test)]
mod image_url_tests {
    #[test]
    fn lists_page_images_as_absolute_urls() {
        let html = r#"<html><head><meta property="og:image" content="https://cdn.example.com/cover.jpg">
            </head><body><img class="x" src="/pics/a.png" alt="a"><img data-src='b.webp'>
            <img src="data:image/gif;base64,AAAA"><img src="/pics/a.png"></body></html>"#;
        let images = super::extract_image_urls(html, "https://example.com/post/1");
        assert_eq!(
            images,
            vec![
                "https://cdn.example.com/cover.jpg",
                "https://example.com/pics/a.png",
                "https://example.com/post/b.webp",
            ]
        );
    }
}

#[cfg(test)]
mod tests;
