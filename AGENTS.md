# PROJECT KNOWLEDGE BASE

**Generated:** 2026-08-16 (updated 2026-10-09)
**Commit:** 6de30ae + working tree
**Branch:** feat/port-azh-subsystems

## OVERVIEW
Claw Code: public Rust implementation of the `claw` CLI agent harness (Claude-Code-style). Canonical code lives in `rust/`; the repo is an agent-managed exhibit (harnesses plan/execute/verify per README), not a hand-operated product. `src/` is a companion Python porting/parity workspace, not production code.

## STRUCTURE
```
claw-code/
├── rust/      # canonical Cargo workspace: 12 crates, `claw` binary
├── src/       # Python porting workspace + reference_data/ parity snapshots
├── tests/     # Python unittest validation of src/ + scripts/ (stdlib unittest)
├── docs/      # g0XX gate verification maps + topic docs
├── scripts/   # fmt.sh, install-local.sh, dogfood-build.sh, roadmap/board helpers
├── assets/    # README images only
├── ARCHITECTURE.md, MANUAL.md, USAGE.md   # design / user manual / command reference
└── install.sh, Containerfile, docker-compose.yml
```

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| CLI subcommands | rust/crates/rusty-claude-cli/src/main.rs | hand-rolled parser; CliAction enum ~L1168; dispatch in run() L1000-1165 |
| REPL loop / interrupts | rust/crates/rusty-claude-cli/src/main.rs | run_repl ~L7035, LiveCli ~L7147, ignore_ctrl_c_in_repl + HookAbortMonitor ~L7600 |
| Line editor (prompt input) | rust/crates/rusty-claude-cli/src/input.rs | crossterm raw mode; menu, paste, history, Ctrl+C double-press exit |
| Streaming renderer | rust/crates/rusty-claude-cli/src/render.rs | markdown streaming, tool lines, diffs, themes |
| REPL-only slash commands | rust/crates/rusty-claude-cli/src/repl_commands.rs | undo/rewind, plan, effort, tasks, theme, image, copy… |
| Esc / type-ahead during a turn | rust/crates/runtime/src/turn_keys.rs | TurnKeyWatcher, queued prompts, status line |
| Checkpoints / background jobs | rust/crates/runtime/src/snapshots.rs, background_jobs.rs | shadow git repo; bash run_in_background registry |
| Sessions/permissions/MCP | rust/crates/runtime/src/ | 50 flat modules; bash risk classification in permissions.rs |
| Provider clients | rust/crates/api/src/providers/ | anthropic.rs + openai_compat.rs; timeouts in http_client.rs |
| Tool definitions | rust/crates/tools/src/lib.rs | 60-tool spec table L485-1460 |
| Office/PDF/notebook I/O | rust/crates/documents/src/ | read as text; write docx/pptx, edit xlsx |
| Slash commands | rust/crates/commands/src/lib.rs | 57-command spec table; REPL handlers in main.rs handle_repl_command + repl_commands.rs |
| Plugins/hooks | rust/crates/plugins/src/ | manifest = .claude-plugin/plugin.json |
| Lean agent harness | rust/crates/claw-analog/src/lib.rs | lib+bin; tool loop over api+runtime |
| RAG HTTP service | rust/crates/claw-rag-service/src/ | axum; SQLite + optional Qdrant |
| Test mock server | rust/crates/mock-anthropic-service/ | SCENARIO_PREFIX scripted responses |
| Python porting CLI | src/main.py | argparse: manifest, parity-audit, graphs |
| Parity reference DB | src/reference_data/subsystems/ | 29 JSON snapshots of TS archive |

## CODE MAP
| Symbol | Type | Location | Refs | Role |
|--------|------|----------|------|------|
| Session | struct | runtime/src/session.rs:124 | 229 | session persistence/lifecycle |
| ConfigLoader | struct | runtime/src/config.rs:412 | 83 | config schema/load |
| PluginManager | struct | plugins/src/lib.rs | 48 | plugin install/registry |
| PermissionEnforcer | struct | runtime/src/permission_enforcer.rs:27 | 35 | pre-dispatch permission gate |
| ConversationRuntime | struct | runtime/src/conversation.rs:174 | 32 | conversation loop driver |
| McpServerManager | struct | runtime/src/mcp_stdio.rs:488 | 30 | MCP JSON-RPC processes |
| HookRunner | struct | runtime/src/hooks.rs:155 | 25 | shell hook execution |
| CliAction | enum | rusty-claude-cli/src/main.rs:1168 | — | 25 subcommand variants |
| mvp_tool_specs | fn | tools/src/lib.rs:485 | — | static 60-tool table |
| SLASH_COMMAND_SPECS | const | commands/src/lib.rs | — | 57 slash commands |
| LiveCli | struct | rusty-claude-cli/src/main.rs:7147 | — | REPL session state, run_turn, auto-compact retry |
| LineEditor | struct | rusty-claude-cli/src/input.rs | — | prompt input editor |
| TurnKeyWatcher | struct | runtime/src/turn_keys.rs | — | Esc + type-ahead during a turn |

(Refs = rg count across rust/crates; rust-analyzer references timed out during mapping.)

## CONVENTIONS
- `unsafe_code = "forbid"` workspace-wide; every crate opts in via `[lints] workspace = true`; clippy all=warn, pedantic=allow
- Edition 2021, resolver 2, publish=false; no rust-toolchain pin (CI floats stable); no rustfmt.toml/clippy.toml — stock defaults
- Giant flat files by design (main.rs 20.2k, tools/lib.rs 11.6k, commands/lib.rs 6.5k): organization is positional — types → spec table → dispatch → handlers → tests at EOF
- Dual output paths everywhere: `render_x` + `render_x_json`; JSON errors to **stdout**, text errors to **stderr**
- Tests: inline `#[cfg(test)] mod tests` primary; integration tests spawn `CARGO_BIN_EXE_claw` subprocess against mock-anthropic-service; tempfile everywhere; env-mutating tests serialize via env_lock/test_env_lock
- Comments carry issue numbers (#824, #146); gate tests named by roadmap gate (g004_conformance.rs)
- Python side: stdlib only, `python -m unittest`; src/ mixes camelCase (QueryEngine.py) and snake_case filenames

## ANTI-PATTERNS (THIS PROJECT)
- NEVER `cargo install claw-code` — crates.io stub is deprecated and installs `claw-code-deprecated.exe`; build from source
- Forbidden doc strings (CI-enforced by .github/scripts/check_doc_source_of_truth.py): old org links `github.com/Yeachan-Heo/claw-code`, `github.com/code-yeongyu/claw-code`, `discord.gg/6ztZB9jvWq`, `assets/clawd-hero.jpeg`
- Deprecated config keys: `permissionMode` → `permissions.defaultMode`; `enabledPlugins` → `plugins.enabled`; env `RUSTY_CLAUDE_PERMISSION_MODE` is dead
- Direct push to main is policy-blocked (`main_push_forbidden` approval scope)
- Automation lanes must not merge/close remote PRs/issues (docs/anti-slop-triage.md)
- `claw init` must not scaffold `dontAsk` permission mode (regression-pinned in output_format_contract.rs)
- File-level `#![allow(dead_code)]` blocks (main.rs, session_control.rs) are tolerated legacy — do not extend the pattern

## UNIQUE STYLES
- Dogfood build: scripts/dogfood-build.sh injects GIT_SHA; `claw version` provenance must equal HEAD
- Mock parity: rust/mock_parity_scenarios.json drives CLI subprocess vs MockAnthropicService
- Dogfooding uses `CLAW_CONFIG_HOME=$(mktemp -d)` for config isolation
- Env contracts: GIT_SHA (build), CLAW_CONFIG_HOME (config dir), OLLAMA_HOST (provider override), `*_API_KEY`/`*_BASE_URL` per provider

## COMMANDS
```bash
scripts/fmt.sh --check                 # fmt check (apply: scripts/fmt.sh)
cd rust && cargo clippy --workspace --all-targets -- -D warnings
cd rust && cargo test --workspace
cd rust && cargo build -p rusty-claude-cli   # binary: rust/target/debug/claw
python -m unittest discover -s tests   # Python suite
python .github/scripts/check_doc_source_of_truth.py && scripts/roadmap-check-ids.sh   # docs/roadmap CI
```

## NOTES
- User-visible behavior is documented in MANUAL.md; design in ARCHITECTURE.md. Update them with behavior changes.
- REPL uses a custom crossterm line editor (input.rs); rustyline is no longer a dependency
- `claw` binary comes from crate `rusty-claude-cli` (package/bin name mismatch)
- rust-ci.yml triggers only on rust/**, docs/**, listed meta file changes (path filters)
- CI clippy job runs without `-D warnings` — weaker than the documented gate; known pre-existing failures recorded in docs/g002/g003 maps
- `claw acp` is a status stub, not a real ACP server
- rust/ has committed harness dotdirs (.clawd-agents/, .omc/, .sandbox-home/) — intentional
