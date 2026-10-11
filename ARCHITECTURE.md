# Claw Code Architecture

How the `claw` binary is put together: which crate owns what, how one request
flows from the prompt to the model and through tools, and where state lives.
For day-to-day use, see [`MANUAL.md`](./MANUAL.md). For build and command
references, see [`USAGE.md`](./USAGE.md) and [`rust/README.md`](./rust/README.md).

Line numbers below are approximate landmarks; the big files move often.

## Repository layout

```text
claw-code/
├── rust/                 # canonical Cargo workspace (12 crates) → `claw` binary
├── src/  tests/          # Python porting/parity workspace + its unittest suite (not the runtime)
├── docs/                 # topic guides and g0XX gate verification maps
├── scripts/              # fmt.sh, install-local.sh, dogfood-build.sh, roadmap helpers
├── ARCHITECTURE.md       # this file
├── MANUAL.md             # user manual for the interactive CLI
└── USAGE.md              # task-oriented command reference
```

## Crates

```text
                     rusty-claude-cli  (bin: claw)
          ┌──────────┬──────┴──────┬───────────┬─────────┐
          ▼          ▼             ▼           ▼         ▼
        tools ──► commands      plugins       api     runtime
          │  │        │                        │        ▲  ▲
          │  └────────┼────► documents         └────────┘  │
          └───────────┴────────────────────────────────────┘
                 runtime ──► plugins, telemetry
```

| Crate | Kind | Owns |
|---|---|---|
| `rusty-claude-cli` | bin `claw` | Argument parsing, REPL, line editor, streaming renderer, permission prompts, slash-command handlers, doctor/status/JSON surfaces |
| `runtime` | lib | `ConversationRuntime` (the agent loop), sessions, config, permissions policy, prompt assembly, hooks, MCP, compaction, snapshots, background jobs, turn-time keyboard watcher |
| `api` | lib | Provider clients (Anthropic Messages, OpenAI-compatible: OpenAI, xAI, DashScope, Ollama, local servers), SSE parsing, HTTP client/timeouts/proxies, `ApiError` |
| `tools` | lib | The tool registry: 60 tool specs with JSON schemas and their implementations (`execute_tool`) |
| `commands` | lib | Slash-command specs (57), parsing, help rendering |
| `documents` | lib | Read Office/PDF/notebook files as text; write `.docx`/`.pptx`, edit `.xlsx` |
| `plugins` | lib | Plugin manifests (`.claude-plugin/plugin.json`), install/enable/disable, plugin tools and hooks |
| `telemetry` | lib | Request identity and analytics sinks |
| `mock-anthropic-service` | lib+bin | Scripted `/v1/messages` mock used by CLI integration tests and the parity harness |
| `compat-harness` | lib | Extracts upstream manifests for parity comparison |
| `claw-analog` | lib+bin | A lean alternate agent loop over `api` + `runtime` |
| `claw-rag-service` | bin | Standalone axum RAG service (SQLite, optional Qdrant) |

Dependency rules that matter:

- `tools` depends on `commands`, never the reverse.
- `runtime` has no HTTP client; the provider network code lives in `api`.
- `unsafe_code` is `forbid` across the workspace.

## The CLI binary

`rust/crates/rusty-claude-cli/src/`:

| File | Role |
|---|---|
| `main.rs` (~20k lines) | `main()` (L335) is only the error envelope; `run()` (L1000) dispatches the `CliAction` enum (L1168) built by the hand-rolled `parse_args` (L1484). `run_repl` (L7035) and `LiveCli` (L7147) drive the interactive session. `AnthropicRuntimeClient` (L12730) adapts `api` to the runtime's `ApiClient` trait; `CliToolExecutor` (L14024) adapts `tools`; `CliPermissionPrompter` (L12614) asks the user. Tests start at L14547. |
| `input.rs` | Raw-mode line editor (crossterm): markdown highlighting, multi-line input, paste placeholders, `/` command menu, tab completion, persistent history, Ctrl+C handling |
| `render.rs` | Streaming markdown renderer (word-by-word text, syntax-highlighted code via syntect, buffered tables), tool-call lines, diffs, the "thinking" animation, themes |
| `repl_commands.rs` | REPL-only commands: checkpoints (`/undo`, `/rewind`), `/plan`, `/effort`, `/usage`, `/context`, `/tasks`, `/files`, `/copy`, `/paste`, `/image`, `/theme`, `/retry`, `/add-dir`, `/rename`, prompt-backed review commands |
| `init.rs`, `setup_wizard.rs` | `claw init` scaffolding and the first-run provider setup |

## Lifecycle of one REPL request

```text
LineEditor::read_line ──► LiveCli::run_turn
                             │ begin_checkpoint      (shadow-git "before" snapshot)
                             │ prepare_turn_runtime  (fresh runtime + abort signal + Ctrl+C monitor)
                             │ TurnKeyWatcher::start (Esc / type-ahead while the reply runs)
                             ▼
                   ConversationRuntime::run_turn  (runtime/src/conversation.rs)
                     loop:
                       ApiClient::stream ──► api::ProviderClient ──► provider (SSE)
                       render streamed text
                       for tool calls in the reply:
                         pre-tool hooks → PermissionPolicy (→ prompt user if needed)
                         group consecutive read-only calls → run in parallel
                         ToolExecutor::execute(_batch) ──► tools::execute_tool
                         post-tool hooks → ToolResult back into the session
                       queued user prompts are injected between steps
                     until the model stops calling tools
                             ▼
                   finish_checkpoint (after snapshot) → persist session
```

Key points:

- **A fresh runtime per turn.** `prepare_turn_runtime` rebuilds the
  `ConversationRuntime` from the current session, re-applying reasoning
  effort and the abort signal, so per-turn settings never leak.
- **Parallel tool calls.** `ConversationRuntime` splits a reply's tool calls
  into groups: each run of consecutive parallel-safe calls (read-only built-ins
  such as `read_file`, `grep_search`, `WebFetch`) is one batch run on scoped
  threads; any other call runs alone. Order is preserved, so a write never
  overtakes a read. Output is printed in the original order.
- **Full-screen view.** The REPL draws the whole window (`turn_keys::screen`,
  entered from `run_repl`, `CLAW_FULLSCREEN=0` to skip). For the session,
  stdout and stderr go into a pipe; a relay thread feeds the bytes into a
  small terminal emulator (`turn_keys::vt`): the conversation is kept as
  unwrapped lines of styled cells (wrapped when drawn, so resizes reflow) and
  the bottom pane as a grid that wraps like a terminal. Code that draws the
  input box or a prompt writes its usual cursor-relative frames through
  `screen::write_pane`, which marks them for the pane; `commit_pane` moves a
  submitted prompt into the conversation. A draw thread paints changed rows
  of the alternate screen; mouse wheel / PgUp / PgDn / Ctrl+Home/End scroll
  the conversation (anchored while scrolled up). Mode and query sequences
  (bracketed paste, kitty keyboard, OSC 52) are passed to the terminal. On
  exit the conversation is printed to the normal screen; a panic hook puts
  the terminal back.
- **Activity row.** While a turn runs, a block shows an animated activity
  row, the input box and the status line: the pane in full-screen view, or
  (`turn_keys::live`, inline view) a block drawn right under the output by a
  turn-long relay that erases it, writes the output and draws it again.
  Prompts that read the keyboard mid-turn hold `turn_keys::pause()`, which
  hands the pane (or the space under the output) to them. Code that asks
  whether stdout is a terminal uses `turn_keys::stdout_is_terminal()`. `runtime::turn_keys` keeps the activity state (`begin_model_request`,
  `note_text`, `note_reasoning`, `begin_tool`/`end_tool`, called from the
  stream loop and the tool executor) and redraws it every 0.1 s from the
  key-watcher thread. Labels: Thinking, Writing, Working (silent mid-reply;
  a note after 20 s, since Ollama sends tool calls only when complete),
  Preparing/Running <tool>.
- **Margins.** Replies, tool output and the permission box go through
  `render::MarginWriter`, which word-wraps inside a two-column margin on both
  sides, measures visible width (ANSI- and Unicode-aware), and repeats the
  indent, bullet indent or `|`/`!` gutter on wrapped lines. Off a terminal it
  passes bytes through unchanged.
- **Type-ahead.** While a turn runs, `runtime::turn_keys` puts the terminal in
  no-echo mode and reads keys: a line finished with Enter is queued and
  delivered to the model before its next step; unfinished text is pre-filled
  at the next prompt. A status line at the bottom shows what is being typed.
- **Tool output cap.** `CLAW_TOOL_RESULT_MAX_CHARS` truncates what a single
  tool call sends back to the model (unset: no cap), to protect small context
  windows.

## Interrupts: Esc and Ctrl+C

One `HookAbortSignal` per turn is shared by three parties:

| Source | Sets the signal when |
|---|---|
| `TurnKeyWatcher` (`runtime/src/turn_keys.rs`) | Esc is pressed during the turn |
| `HookAbortMonitor` (`main.rs`) | SIGINT (Ctrl+C) arrives during the turn |

| Consumer | Effect |
|---|---|
| `AnthropicRuntimeClient::stream` | `select!`s the request against the signal; dropping the request closes the connection and the turn ends with "reply stopped by user" |
| `HookRunner` | Running hooks are cancelled |

Outside a turn, `ignore_ctrl_c_in_repl` (installed once by `run_repl`) keeps
SIGINT from killing the process. At the prompt the editor is in raw mode, so
Ctrl+C arrives as a key: it clears the input, and on an empty input it arms a
2-second "Press Ctrl-C again to exit". Queued prompts are moved back into the
input box when a reply is stopped.

## Providers and the network layer

`api::ProviderClient` picks a backend from the model name and environment
(`ANTHROPIC_*`, `OPENAI_*`, `XAI_API_KEY`, `OLLAMA_HOST`, DashScope prefixes).
Both backends stream SSE into a common `StreamEvent` type.

`api/src/http_client.rs` builds every `reqwest` client:

- `CLAW_API_CONNECT_TIMEOUT` (default 30 s) bounds connecting.
- `CLAW_API_REQUEST_TIMEOUT` (default 300 s; 1800 s for local servers, i.e.
  Ollama and localhost/private-network base URLs) is an **idle read
  timeout**: the longest wait for the next piece of data. A stream that keeps
  producing chunks is never cut off, however long it runs. Local servers get
  the longer default because Ollama sends a tool call in one piece once it is
  complete, so a slow GPU writing a large file is silent for minutes.
- Proxies come from `HTTP(S)_PROXY`/`NO_PROXY` or the `proxy_url` setting.

`ApiError::Http` prints reqwest's whole source chain, because reqwest reports
any body-read failure (including timeouts and dropped connections) as
"error decoding response body".

## Context-window recovery

When a request fails with a context-overflow signature (`context_window`,
`exceed_context_size`, llama.cpp's "Context size has been exceeded", a
non-SSE body with no parseable content, or a non-timeout decode error),
`LiveCli` retries with progressively harder compaction: it keeps 4, then 2, 1, 0
recent messages, running the Trident pipeline plus summary compaction
(`runtime/src/trident.rs`, `compact.rs`) before each retry. If the error
reveals the server's real context window, the auto-compaction threshold is
reset to 70% of it. Timeouts are excluded: compaction cannot fix them.

Compaction never splits an assistant tool-use message from its tool result.

Automatic compaction (`ConversationRuntime::maybe_auto_compact`) runs after
each model step when the last request's input (uncached plus cache read and
write tokens) reached the threshold: 100,000 tokens by default,
`CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS` to change it. If it summarizes away the
prompt of the running turn, that prompt is appended verbatim to the summary
message so the model can finish the task.

## Permissions

`runtime/src/permissions.rs` decides whether each tool call may run:

1. Every tool declares a required mode (`read-only`, `workspace-write`,
   `danger-full-access`); the session runs in one of these modes.
2. `required_mode_for_input` raises the requirement from the input:
   - file-writing tools aimed outside the current directory need full access;
   - bash commands are classified by content: read-only commands need
     `read-only`, privileged (`sudo`, `doas`, …) or destructive (`rm`, `dd`,
     `mkfs`, `kill`, `git reset --hard`, `git push --force`, recursive
     `chmod`, …) ones need full access, everything
     else `workspace-write`. Every command in a chain, pipeline or `$(…)`
     subshell is checked.
3. `allow` / `ask` / `deny` rules from config override the mode. `deny` and
   `ask` bash rules match any command inside a chain; `allow` rules only
   match the whole command, so they can never widen access by accident.
4. If the call needs more than the current mode, `CliPermissionPrompter` asks:
   `[y]es / [a]lways for <key> this session / [N]o`. "Always" remembers the
   tool plus, for bash, the first two words of the risky command (`sudo
   emerge`), so it doesn't also approve `sudo rm`.

`permissions.bashApproval: "risky"` limits bash prompts to privileged or
destructive commands.

## Prompt assembly and memory

`runtime/src/prompt.rs` builds the system prompt from:

- the built-in instructions and environment section;
- user-level instruction files: `~/.claw/CLAUDE.md`, `~/.claw/AGENTS.md`, and
  Claude Code's `~/.claude/CLAUDE.md` and `~/.claude/rules/`
  (`config::claude_config_home`, `$CLAUDE_CONFIG_DIR`);
- `@path` imports in any of these files, followed up to 5 levels;
- project instruction files up to the git root (`CLAUDE.md`, `CLAW.md`,
  `AGENTS.md`, `.claw/…`, `.claude/CLAUDE.md`), rule directories
  (`.claw/rules/`, `.claw/rules.local/`, `.claude/rules/`) and imported rules from other tools
  (Cursor, Copilot, Windsurf, …; `rulesImport` setting);
- git status and the diff. A diff over 8,000 characters is replaced by
  `git diff --stat`, so it can't fill a local model's whole context;
- auto memory (`runtime/src/memory.rs`): instructions for keeping memories
  plus the project's `MEMORY.md` index (cut at 200 lines). The model writes
  memory files itself with `write_file`/`edit_file`; `permissions.rs` treats
  the memory folder as inside the workspace, so changing a memory never asks.
  Off with `autoMemoryEnabled: false` or `CLAW_AUTO_MEMORY=0`.

Plan mode (`/plan`) appends a "read-only investigation, reply with a plan"
section and switches the permission mode to read-only until `/plan off`.

## State on disk

| What | Where |
|---|---|
| User config home | `$CLAW_CONFIG_HOME`, else `~/.claw/` |
| User settings (theme, provider, …) | `~/.claw/settings.json` (mode 0600) |
| Prompt history | `~/.claw/history.jsonl` |
| Auto memory | `~/.claude/projects/<project path, non-alphanumerics → ->/memory/`, shared with Claude Code; `~/.claw/projects/…` when there is no `~/.claude` (`MEMORY.md` index + one file per memory) |
| Sessions | `.claw/sessions/` in the workspace (JSONL, resumable with `--resume`) |
| Checkpoints | Shadow git repo `~/.claw/checkpoints/<hash>.git` whose work tree is the project; the project's own `.git` is never touched. Disabled for `$HOME`, `/`, huge folders, missing git, or `CLAW_SNAPSHOTS=0` |
| Background jobs | In memory (`runtime/src/background_jobs.rs`); output buffered up to 1 MB per job; killed when the REPL exits |
| Project config | `.claw.json`, `.claw/settings.json`, `.claw/settings.local.json` (see [`USAGE.md`](./USAGE.md#config-file-resolution-order)) |

## Checkpoints and undo

`LiveCli::run_turn` wraps every REPL request in `begin_checkpoint` /
`finish_checkpoint`. Each request that changes any file gets a before/after
commit in the shadow repo, so `/undo` and `/rewind [n]` can revert edits made
by any tool, including shell side effects, even after a restart. `/diff` uses
the session's first snapshot as its baseline.

## Background jobs

`bash` with `run_in_background: true` registers the process in the global
job registry instead of waiting for it. The model reads output with
`BashOutput` and stops jobs with `KillShell`; the user does the same with
`/tasks`. When a job exits on its own, a notice is prepended to the next
prompt so the model learns about it.

## Documents

The file tools route by extension through the `documents` crate. Reading turns
spreadsheets into tables with cell addresses, Word/PowerPoint into
markdown-like text, PDFs into pages and notebooks into cells with outputs.
`write_docx` builds Word files from markdown, `write_pptx` builds decks from
slide specs, and `write_xlsx` edits workbooks in place.

## Testing

- Unit tests live inline (`#[cfg(test)] mod tests`) at the end of each file.
- `rusty-claude-cli/tests/` runs the real `claw` binary against
  `mock-anthropic-service`. `output_format_contract.rs` pins the JSON
  contract of every subcommand.
- `rust/mock_parity_scenarios.json` drives the parity harness
  ([`rust/MOCK_PARITY_HARNESS.md`](./rust/MOCK_PARITY_HARNESS.md)).
- Env-mutating tests take the crate's env lock.
