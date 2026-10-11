# AGENTS.md — rusty-claude-cli (the `claw` binary)

## OVERVIEW

Crate `rusty-claude-cli` produces the `claw` binary (~25 subcommands). Wires together api, runtime, tools, commands, plugins; terminal via crossterm/syntect (custom line editor, no rustyline). Hand-rolled arg parser (no clap).

## main.rs MAP

src/main.rs is ~20,200 lines. Landmark table (approximate; re-grep before relying on exact lines):

| Lines | Section |
|---|---|
| 73–330 | Provenance/model types (`ModelSource`, `ModelProvenance`, `PermissionModeSource`), build constants |
| 330–682 | Error taxonomy (`classify_error_kind`, JSON/text error output) |
| 774–994 | Global flags, `--cwd`/`-C` stripping, stdin plumbing |
| 335 | `main()`: error envelope only |
| 1000–1165 | `run()`: single flat match dispatching `CliAction` variants |
| 1168–1290 | `CliAction` enum, struct variants (each carries `output_format`) |
| 1484+ | `parse_args`: manual token loop |
| 1312–1477 | Output-format machinery: `OnceLock` statics, duplicate-flag tracking |
| 1478–2272 | `parse_args`: ~800-line manual token loop |
| 2542–3389 | Per-subcommand sub-parsers inside `parse_args` |
| 2902–3389 | Model/permission/allowed-tools resolution |
| 3390–4696 | Doctor subsystem (~13 `check_*_health` fns) |
| 4697–6062 | Manifests, bootstrap-plan, system-prompt, version, `resume_session` |
| 5459–7047 | `StatusContext`, `BinaryProvenance`, broad-cwd policy, stale-base preflight |
| 7035–9500 | `run_repl` (L7035) interactive loop; `LiveCli` (L7147); `ignore_ctrl_c_in_repl` + `HookAbortMonitor` (~L7600); `prepare_turn_runtime`, `run_turn_inner` with Esc/Ctrl+C stop and progressive auto-compact retry; `handle_repl_command` (~L8180) |
| 9424–14264 | Snapshot printers: session-list, status, sandbox, models, help topics, acp, `run_init` (L11027), `run_export` (L11715), `print_help` (L14055) |
| 12614 | `CliPermissionPrompter`: `[y]es / [a]lways … / [N]o`, session approvals keyed by `bash_approval_key` |
| 12730 | `AnthropicRuntimeClient`: adapts `api::ProviderClient` to the runtime `ApiClient`; aborts the stream on the turn signal |
| 14024 | `CliToolExecutor`: tool output rendering, parallel read-only batches, `CLAW_TOOL_RESULT_MAX_CHARS` cap |
| 14547–end | In-file `mod tests` + smaller test modules (one embeds a Python MCP fixture) |

## CONVENTIONS

- Every subcommand supports `--output-format text|json`, env-overridable.
- `raw_args_request_json_output` pre-scans argv before parsing to suppress stderr config warnings in JSON mode.
- Dual renderers for each output path: human text + structured `*_json`.
- Unknown flags/typos get levenshtein-distance suggestions.
- Comments carry issue numbers (#824, #146, etc.) when tracking known problems.
- `main()` (L330) is only the error envelope. All real work happens in `run()`.
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
