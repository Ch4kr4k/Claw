# AGENTS.md — rusty-claude-cli (the `claw` binary)

## OVERVIEW

Crate `rusty-claude-cli` produces the `claw` binary (~25 subcommands). Wires together api, runtime, tools, commands, plugins; terminal via crossterm/syntect (custom line editor, no rustyline). Hand-rolled arg parser (no clap).

## MODULE MAP

`main.rs` (~15,800 lines) keeps startup, dispatch and the shared helpers; the
big types live in sibling modules. Find things by name (`grep -n "fn name"`),
not by line number.

| File | What is there |
|---|---|
| `main.rs` | Provenance/model types, error taxonomy (`classify_error_kind`), `main()` (error envelope only), `run()` (one flat match over `CliAction`), doctor checks (`check_*_health`), manifests, status/help/config renderers, `run_repl` and `LiveCli`'s struct, constructor and `handle_repl_command` dispatcher, request helpers (`sent_tool_definitions`, `defer_tool_definitions`, `convert_messages`) |
| `cli_args.rs` | `CliAction` and `parse_args` (the manual token loop) |
| `live_turn.rs` | `LiveCli` turns: `run_turn` (checkpoint, LeanGate rounds, image check) around `run_turn_inner` (an engine turn), one-shot prompt modes, prompt-backed commands (`/bughunter`, `/commit`, `/pr`, …) |
| `live_session.rs` | `LiveCli` state: persist/resume/clear sessions, `set_model`, `set_permissions`, `rebuild_runtime`, the status line (`refresh_footer_status`), `/session` and `/plugins` actions |
| `live_reports.rs` | `LiveCli`'s read-only reports: `/status`, `/history`, `/sandbox`, `/cost`, `/config`, `/memory`, `/agents`, `/mcp`, `/skills`, `/plugins`, `/diff`, `/version` |
| `provider_client.rs` | `AnthropicRuntimeClient`: the session's client for compaction summaries; `turn_client` builds the provider client turns use (prompt cache, retry policy) |
| `live_engine.rs` | Turns on the engine: `engine_config`, `engine_provider` (reply budget, thinking budget, fallbacks, error wording), the REPL turn view, quiet and side-prompt turns, `after_engine_turn` (usage, auto-compaction) |
| `tool_executor.rs` | `CliToolExecutor`: allowed-list checks, `ToolSearch` loading, MCP/plugin tools, parallel read-only batches, tool output display, `CLAW_TOOL_RESULT_MAX_CHARS` cap |
| `permission_prompt.rs` | `CliPermissionPrompter`: `[y]es / [a]lways … / [N]o`, session approvals keyed by `bash_approval_key` |
| `tests.rs` | `main.rs`'s unit tests (`mod tests;`); smaller test modules stay at the end of `main.rs` |

Moved modules `use crate::*;`, so helpers stay in `main.rs` and are reachable
from them; methods and fields used across modules are `pub(crate)`.

## CONVENTIONS

- Every subcommand supports `--output-format text|json`, env-overridable.
- `raw_args_request_json_output` pre-scans argv before parsing to suppress stderr config warnings in JSON mode.
- Dual renderers for each output path: human text + structured `*_json`.
- Unknown flags/typos get levenshtein-distance suggestions.
- Comments carry issue numbers (#824, #146, etc.) when tracking known problems.
- `main()` is only the error envelope. All real work happens in `run()`.
- Error output always includes `status`, `error_kind`, `action`, `hint`, `exit_code` fields.
- `classify_error_kind` maps message prefixes to snake_case kind tokens.
- JSON errors go to stdout, text errors to stderr.
- Sibling modules:
  - `input.rs`: `LineEditor` in crossterm raw mode: markdown highlighting, multi-line input and paste placeholders, `/` command menu, Tab completion of `/commands` and `@paths`, history in `~/.claw/history.jsonl`, Ctrl+C clear / double-press exit. Unit-tested through `handle_key`.
  - `render.rs`: streaming markdown (word-by-word text, highlighted code, buffered tables), tool lines and diffs, thinking animation (suppressed when the REPL activity row is shown), themes (dark, light, solarized-dark, solarized-light, tokyo-night, none), `MarginWriter` (margins + ANSI-aware word wrap; route new user-facing output through it or `print_with_margins`).
  - `repl_commands.rs`: REPL-only slash commands (undo/rewind checkpoints, plan, effort, usage, context, tasks, files, copy, paste, image, theme, retry, add-dir, rename, prompt-backed reviews).
  - `ask.rs`: interactive `AskUserQuestion` picker (recommended option first, numbers, checkboxes for `multi_select`, "Type something else…" row, Esc cancels; key handling in `Picker::handle_key` is unit-tested). `CliToolExecutor::execute` intercepts the tool when output is shown; the `tools` crate keeps a plain fallback that never blocks without a terminal.
  - `init.rs`, `setup_wizard.rs` (`claw setup`).
- Ctrl+C: during a turn it sets the same abort signal as Esc (stops the stream); outside a turn the REPL-wide handler ignores it; at the prompt it is a key event. Never reintroduce a path where SIGINT kills the REPL.
- User-visible REPL behavior is documented in `../../../MANUAL.md`; update it with key, command or prompt changes.
- `build.rs` injects `GIT_SHA`, `RUSTC_VERSION`, `GIT_DIRTY` via `cargo:rustc-env`.

## ANTI-PATTERNS

- File opens with crate-wide `#![allow(...)]` suppressing 13 lints including `dead_code` and `unused_imports`. Legacy. Do NOT extend this list.
- 9+ functions carry `#[allow(clippy::too_many_lines)]`. Tolerated, not license for more.
- Package/bin name mismatch: crate is `rusty-claude-cli`, binary is `claw`. Watch for this in paths and test macros.

## TESTS

All tests live in `tests/` (6 files), black-box style. They spawn `env!("CARGO_BIN_EXE_claw")` in unique temp dirs (AtomicU64 counter).

| File | What it covers |
|---|---|
| `output_format_contract.rs` | ~105 tests pinning `kind`/`status`/`action` JSON contract for EVERY subcommand. Must update when adding/changing any command output. 5,986 lines. |
| `resume_slash_commands.rs` | Resume and slash-command behavior |
| `cli_flags_and_config_defaults.rs` | Flag parsing, config file defaults |
| `compact_output.rs` | Compact output mode |
| `compact_repl_panic.rs` | Nested-runtime panic regression |
| `mock_parity_harness.rs` | Scenario-driven tests against mock-anthropic-service, driven by `rust/mock_parity_scenarios.json` |
