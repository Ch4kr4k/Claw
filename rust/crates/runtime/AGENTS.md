# AGENTS.md — runtime crate

## OVERVIEW

Core crate of `claw`: session persistence, permissions, prompt assembly, MCP plumbing, tool-facing file ops, conversation loop. 50 flat modules in src/, ~330 pub symbols, ~170 re-exported flat from lib.rs.

## WHERE TO LOOK

| Group | Files | Entry points |
|---|---|---|
| session/conversation | session.rs, session_control.rs, conversation.rs, compact.rs, trident.rs, summary_compression.rs, usage.rs | `Session` (L124), `SessionStore`, `ConversationRuntime` (L174), `ApiClient`/`ToolExecutor` traits (`can_run_in_parallel`/`execute_batch` for read-only batches) |
| config | config.rs, config_validate.rs, bootstrap.rs | `ConfigLoader` (L412), `default_config_home`, `read_user_setting`/save helpers, type-export heaviest file; MCP server config enums live here |
| MCP (6-file split) | mcp.rs, mcp_client.rs, mcp_stdio.rs, mcp_server.rs, mcp_tool_bridge.rs, mcp_lifecycle_hardened.rs | `McpServerManager` (L488 in mcp_stdio.rs), JSON-RPC spawn |
| hooks/plugins | hooks.rs, plugin_lifecycle.rs | `HookRunner` (L155), abort signal, healthcheck, degraded mode |
| permissions/safety (`bash_command_risk`, `bash_approval_key`, `required_mode_for_input`, `bashApproval`) | permissions.rs, permission_enforcer.rs, policy_engine.rs, approval_tokens.rs, sandbox.rs, bash_validation.rs, trust_resolver.rs | `PermissionEnforcer` (L27), `GreenLevel`, lane decisions |
| tools/execution | bash.rs, file_ops.rs, lsp_client.rs | `execute_bash`, `*_in_workspace` file op variants |
| lane/worker | lane_events.rs, worker_boot.rs, task_packet.rs, task_registry.rs, team_cron_registry.rs, branch_lock.rs, stale_base.rs, stale_branch.rs | `LaneEvent` dedupe/provenance, `LaneBoard` |
| prompt | prompt.rs | `SystemPromptBuilder`, `ContextFile`, dynamic boundary marker; user-level `~/.claw/CLAUDE.md`/`AGENTS.md`; git diff over 8,000 chars → `--stat` |
| git/remote/auth | git_context.rs, remote.rs, oauth.rs | Upstream proxy, PKCE flow |
| checkpoints/jobs | snapshots.rs, background_jobs.rs | `GitSnapshots`/`CheckpointStack` (shadow git repo under config home), `global_background_jobs()` registry for `bash(run_in_background)` |
| turn input | turn_keys.rs | `TurnKeyWatcher` (Esc → abort signal, type-ahead, queued prompts, scroll keys), full-screen view (`turn_keys/screen.rs` session/relay/draw thread, `turn_keys/vt.rs` cell emulator), the inline block under the output (`turn_keys/live.rs`: stdout/stderr relayed through a pipe; animated activity row + input box + status line; activity API `begin_model_request`/`note_text`/`note_reasoning`/`begin_tool`/`end_tool`), `pause()` guard for prompts that read the keyboard mid-turn |
| misc | json.rs, sse.rs, g004_conformance.rs, green_contract.rs, recovery_recipes.rs, report_schema.rs, trident.rs | Report v1 + redaction |

Largest files by line count: config.rs (3894), mcp_stdio.rs (2969), lane_events.rs (2561), worker_boot.rs (2441), session.rs (1961), conversation.rs (1878).

## CONVENTIONS

- One file per module, flat layout. No subdirectories.
- Most modules are private `mod x` with selective `pub use`. 21 modules are `pub mod`, so consumers use both the re-export and the qualified path.
- Deps kept minimal: serde, tokio, glob, regex, sha2, walkdir + internal plugins/telemetry. No reqwest, no async-trait. Remote/SSE done by hand.
- Inline `#[cfg(test)]` tests per file. session.rs has two test modules.
- `pub(crate) test_env_lock()` mutex in lib.rs serializes env-mutating tests. Use it when touching env vars.
- trust_resolver.rs is `#[cfg(test)]`-gated yet pub-used: test-only API surface.

## INVARIANTS (do not break)

1. **Compaction pairs**: compact.rs must never split assistant(ToolUse)/ToolResult pairs.
2. **No side effects on construction**: SessionStore construction must not create `.claw` directories (session_control.rs:1090).
3. **Workspace containment**: file_ops.rs workspace ops must not escape the workspace root.
4. **Permission ordering**: a leading read-only permission token must not launder a trailing destructive one (permission_enforcer.rs:450).
5. **Bash chains**: risk checks and `deny`/`ask` rules inspect every segment of a chain, pipeline or `$(…)`; `allow` rules match only the whole command. Never let an `allow` rule match a segment.
6. **Tool order**: parallel batches only group consecutive parallel-safe calls; a write must never move past a read.
7. **Snapshots never touch the project's `.git`**: the shadow repo uses the project as work tree only.

## ANTI-PATTERNS

- Don't extend whole-file `#![allow(...)]` blocks. They exist as legacy tolerance in worker_boot.rs, mcp_tool_bridge.rs, lsp_client.rs, stale_branch.rs, stale_base.rs, recovery_recipes.rs, mcp_lifecycle_hardened.rs, session_control.rs. Adding new ones is not acceptable.
- Don't add reqwest or async-trait as deps. Remote calls go through the manual SSE/proxy layer in remote.rs and sse.rs.
- Don't create subdirectories under src/. The flat module layout is intentional.
- Don't bypass `*_in_workspace` variants for file ops when running inside a workspace context. The unchecked versions exist for bootstrap and out-of-workspace scenarios only.
- Don't add new `pub mod` exports without reason. Prefer private mod + selective `pub use` from lib.rs.
