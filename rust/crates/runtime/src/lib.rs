//! Core runtime primitives for the `claw` CLI and supporting crates.
//!
//! This crate owns session persistence, permission evaluation, prompt assembly,
//! MCP plumbing, tool-facing file operations, and the core conversation loop
//! that drives interactive and one-shot turns.

mod approval_tokens;
pub mod askpass;
pub mod background_jobs;
mod bash;
pub mod bash_validation;
mod bootstrap;
pub mod branch_lock;
mod compact;
mod config;
pub mod config_validate;
mod conversation;
mod file_ops;
pub mod g004_conformance;
mod git_context;
pub mod green_contract;
mod hooks;
mod json;
mod lane_events;
pub mod line_diff;
pub mod lsp_client;
mod mcp;
mod mcp_client;
pub mod mcp_lifecycle_hardened;
pub mod mcp_server;
mod mcp_stdio;
pub mod mcp_tool_bridge;
mod memory;
pub mod model_server;
mod oauth;
pub mod permission_enforcer;
mod permissions;
pub mod plugin_lifecycle;
pub mod project_trust;
mod policy_engine;
mod prompt;
pub mod recovery_recipes;
mod remote;
mod report_schema;
pub mod sandbox;
pub mod scheduler;
mod session;
pub mod session_control;
pub mod snapshots;
pub mod trident;
pub mod turn_keys;
pub use session_control::SessionStore;
mod sse;
pub mod stale_base;
pub mod stale_branch;
pub mod summary_compression;
pub mod task_packet;
pub mod task_registry;
pub mod team_cron_registry;
pub mod tool_images;
#[cfg(test)]
mod trust_resolver;
mod usage;
pub mod worker_boot;

/// Expands a leading `~` or `~/` to the home directory.
#[must_use]
pub fn expand_home(path: &str) -> std::path::PathBuf {
    let home = || std::env::var_os("HOME").map(std::path::PathBuf::from);
    if path == "~" {
        if let Some(home) = home() {
            return home;
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

pub use approval_tokens::{
    ApprovalDelegationHop, ApprovalScope, ApprovalTokenAudit, ApprovalTokenError,
    ApprovalTokenGrant, ApprovalTokenLedger, ApprovalTokenStatus,
};
pub use background_jobs::{global_background_jobs, BackgroundJobs, JobRead, JobState, JobSummary};
pub use bash::{execute_bash, shell_program, BashCommandInput, BashCommandOutput};
pub use bootstrap::{BootstrapPhase, BootstrapPlan};
pub use branch_lock::{detect_branch_lock_collisions, BranchLockCollision, BranchLockIntent};
pub use compact::{
    compact_session, compact_session_with, estimate_session_tokens, format_compact_summary,
    get_compact_continuation_message, prune_old_tool_results, should_compact, summary_transcript,
    truncate_large_tool_results, CompactionConfig, CompactionResult, SUMMARY_SYSTEM_PROMPT,
};
pub use config::{
    claude_config_home, clear_user_provider_settings, default_config_home, read_user_setting,
    read_user_setting_value, save_user_provider_settings, save_user_setting,
    suppress_config_warnings_for_json_mode, ApiTimeoutConfig, ConfigEntry, ConfigError,
    ConfigFileReport, ConfigFileStatus, ConfigInspection, ConfigLoader, ConfigSource,
    McpConfigCollection, McpInvalidServerConfig, McpManagedProxyServerConfig, McpOAuthConfig,
    McpRemoteServerConfig, McpSdkServerConfig, McpServerConfig, McpStdioServerConfig, McpTransport,
    McpWebSocketServerConfig, OAuthConfig, ProviderFallbackConfig, ResolvedPermissionMode,
    RulesImportConfig, RuntimeConfig, RuntimeFeatureConfig, RuntimeHookCommand, RuntimeHookConfig,
    RuntimeInvalidHookConfig, RuntimePermissionRuleConfig, RuntimePluginConfig,
    RuntimeProviderConfig, ScopedMcpServerConfig, CLAW_SETTINGS_SCHEMA_NAME,
};
pub use config_validate::{
    check_unsupported_format, format_diagnostics, validate_config_file, ConfigDiagnostic,
    DiagnosticKind, ValidationResult,
};
pub use conversation::{
    auto_compaction_threshold_from_env, configured_auto_compaction_threshold,
    is_output_limit_stop_reason, set_auto_compaction_threshold_override, ApiClient, ApiRequest,
    AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RuntimeError,
    StaticToolExecutor, ToolError, ToolExecutor, TurnSummary, THINKING_CUT_OFF_PROMPT,
};
pub use file_ops::{
    edit_file, edit_file_in_workspace, glob_search, glob_search_in_workspace, grep_search,
    grep_search_in_workspace, read_file, read_file_in_workspace, write_file,
    write_file_in_workspace, EditFileOutput, GlobSearchOutput, GrepSearchInput, GrepSearchOutput,
    ReadFileOutput, StructuredPatchHunk, TextFilePayload, WriteFileOutput,
};
pub use git_context::{GitCommitEntry, GitContext};
pub use hooks::{
    HookAbortSignal, HookEvent, HookProgressEvent, HookProgressReporter, HookRunResult, HookRunner,
};
pub use lane_events::{
    compute_event_fingerprint, dedupe_superseded_commit_events, dedupe_terminal_events,
    is_terminal_event, BlockedSubphase, EventProvenance, LaneCommitProvenance, LaneEvent,
    LaneEventBlocker, LaneEventBuilder, LaneEventMetadata, LaneEventName, LaneEventStatus,
    LaneFailureClass, LaneOwnership, SessionIdentity, ShipMergeMethod, ShipProvenance,
    WatcherAction,
};
pub use mcp::{
    mcp_server_signature, mcp_tool_name, mcp_tool_prefix, normalize_name_for_mcp,
    scoped_mcp_config_hash, unwrap_ccr_proxy_url,
};
pub use mcp_client::{
    McpClientAuth, McpClientBootstrap, McpClientTransport, McpManagedProxyTransport,
    McpRemoteTransport, McpSdkTransport, McpStdioTransport,
};
pub use mcp_lifecycle_hardened::{
    McpDegradedReport, McpErrorSurface, McpFailedServer, McpLifecyclePhase, McpLifecycleState,
    McpLifecycleValidator, McpPhaseResult,
};
pub use mcp_server::{McpServer, McpServerSpec, ToolCallHandler, MCP_SERVER_PROTOCOL_VERSION};
pub use mcp_stdio::{
    spawn_mcp_stdio_process, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    ManagedMcpTool, McpDiscoveryFailure, McpInitializeClientInfo, McpInitializeParams,
    McpInitializeResult, McpInitializeServerInfo, McpListResourcesParams, McpListResourcesResult,
    McpListToolsParams, McpListToolsResult, McpReadResourceParams, McpReadResourceResult,
    McpResource, McpResourceContents, McpServerManager, McpServerManagerError, McpStdioProcess,
    McpTool, McpToolCallContent, McpToolCallParams, McpToolCallResult, McpToolDiscoveryReport,
    UnsupportedMcpServer,
};
pub use memory::{
    is_memory_path, list_memories, memory_dir, memory_enabled, record_session,
    render_memory_section, render_memory_section_for, set_anchor_hasher, touch_memory,
    AnchorHasher, MemoryEntry, MEMORY_INDEX,
};
pub use oauth::{
    clear_oauth_credentials, code_challenge_s256, credentials_path, generate_pkce_pair,
    generate_state, load_oauth_credentials, loopback_redirect_uri, parse_oauth_callback_query,
    parse_oauth_callback_request_target, save_oauth_credentials, OAuthAuthorizationRequest,
    OAuthCallbackParams, OAuthRefreshRequest, OAuthTokenExchangeRequest, OAuthTokenSet,
    PkceChallengeMethod, PkceCodePair,
};
pub use permissions::{
    bash_approval_key, bash_command_destructive_risk, bash_command_risk, is_host_execution_request,
    PermissionContext, PermissionMode, PermissionOutcome, PermissionOverride, PermissionPolicy,
    PermissionPromptDecision, PermissionPrompter, PermissionRequest,
};
pub use plugin_lifecycle::{
    DegradedMode, DiscoveryResult, PluginHealthcheck, PluginLifecycle, PluginLifecycleEvent,
    PluginState, ResourceInfo, ServerHealth, ServerStatus, ToolInfo,
};
pub use policy_engine::{
    evaluate, evaluate_with_events, ApprovalToken, DiffScope, GreenLevel, LaneBlocker, LaneContext,
    PolicyAction, PolicyCondition, PolicyDecisionEvent, PolicyDecisionKind, PolicyEngine,
    PolicyEvaluation, PolicyRule, ReconcileReason, ReviewStatus,
};
pub use prompt::{
    load_system_prompt, load_system_prompt_with_context, prepend_bullets, ContextFile,
    ModelFamilyIdentity, ProjectContext, PromptBuildError, SystemPromptBuilder,
    FRONTIER_MODEL_NAME, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
pub use recovery_recipes::{
    attempt_recovery, recipe_for, EscalationPolicy, FailureScenario, RecoveryAttemptState,
    RecoveryAttemptType, RecoveryCommandResult, RecoveryContext, RecoveryEvent,
    RecoveryLedgerEntry, RecoveryRecipe, RecoveryResult, RecoveryStatusReport, RecoveryStep,
};
pub use remote::{
    inherited_upstream_proxy_env, no_proxy_list, read_token, upstream_proxy_ws_url,
    RemoteSessionContext, UpstreamProxyBootstrap, UpstreamProxyState, DEFAULT_REMOTE_BASE_URL,
    DEFAULT_SESSION_TOKEN_PATH, DEFAULT_SYSTEM_CA_BUNDLE, NO_PROXY_HOSTS, UPSTREAM_PROXY_ENV_KEYS,
};
pub use report_schema::{
    canonicalize_report, project_report, report_content_hash, report_schema_v1_registry,
    CanonicalReportV1, ClaimKind, ConsumerCapabilities, FieldDelta, FieldDeltaState,
    NegativeEvidence, NegativeFindingStatus, ProjectionProvenance, RedactionProvenance,
    ReportClaim, ReportConfidence, ReportIdentity, ReportProjectionV1, ReportSchemaField,
    ReportSchemaRegistry, SensitivityClass, DEFAULT_PROJECTION_POLICY_V1, REPORT_SCHEMA_V1,
};
pub use sandbox::{
    build_linux_sandbox_command, detect_container_environment, detect_container_environment_from,
    resolve_sandbox_status, resolve_sandbox_status_for_request, ContainerEnvironment,
    FilesystemIsolationMode, LinuxSandboxCommand, SandboxConfig, SandboxDetectionInputs,
    SandboxRequest, SandboxStatus,
};
pub use session::{
    ContentBlock, ConversationMessage, MessageRole, Session, SessionCompaction, SessionError,
    SessionFork, SessionHeartbeat, SessionLiveness, SessionPromptEntry,
};
pub use snapshots::{CheckpointRecord, CheckpointStack, GitSnapshots, RestoreAction};
pub use sse::{IncrementalSseParser, SseEvent};
pub use stale_base::{
    check_base_commit, format_stale_base_warning, read_claw_base_file, resolve_expected_base,
    BaseCommitSource, BaseCommitState,
};
pub use stale_branch::{
    apply_policy, check_freshness, BranchFreshness, StaleBranchAction, StaleBranchEvent,
    StaleBranchPolicy,
};
pub use task_packet::{
    validate_packet, TaskPacket, TaskPacketValidationError, TaskResource, ValidatedPacket,
};
pub use task_registry::{LaneBoard, LaneBoardEntry, LaneFreshness, LaneHeartbeat};
#[cfg(test)]
pub use trust_resolver::{TrustConfig, TrustDecision, TrustEvent, TrustPolicy, TrustResolver};
pub use usage::{
    format_usd, pricing_for_model, ModelPricing, TokenUsage, UsageCostEstimate, UsageTracker,
};
pub use worker_boot::{
    Worker, WorkerEvent, WorkerEventKind, WorkerEventPayload, WorkerFailure, WorkerFailureKind,
    WorkerPromptTarget, WorkerReadySnapshot, WorkerRegistry, WorkerStatus, WorkerTrustResolution,
};

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
