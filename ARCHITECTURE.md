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
- **Images from tools.** Tool results are text; a tool names a picture with
  an output line `claw-image: /path.png` (`runtime::tool_images`), and the
  conversation loop attaches it to the tool result message as an image
  block, keeping only the newest two. The OpenAI-compatible request moves
  such image messages after the whole run of tool results, so parallel tool
  calls stay paired. The `X11` tool (`tools/src/x11.rs`, over xdotool,
  wmctrl, ffmpeg and xclip) uses this for screenshots. A model that takes no
  images (the models.dev catalog's `modalities.input`, or a provider that
  refused one: OpenRouter's "No endpoints found that support image input",
  llama-server without `mmproj`) gets each image as text instead: what the
  `visionModel` setting's model says it shows (cached per image), else a
  note that it was left out (`sends_images`, `images_as_text`). A refusal is
  retried at once without the image.
- **COM automation.** `tools/src/com.rs` keeps one PowerShell process per
  claw session (started with `-EncodedCommand`, `-STA`), so COM objects
  survive between calls. Each call sends a JSON line with base64 PowerShell
  generated from the action; the host runs it in the global scope and
  answers with a marked JSON line (2-D arrays as rows, COM objects
  summarised). Timeouts kill and restart the process.
- **Inline images.** `rusty-claude-cli/src/term_image.rs` draws `![alt](url)`
  in kitty/Ghostty with Unicode placeholders: the PNG (decoded and scaled
  with `image`, SVG rasterised with `resvg`) is sent once as a quiet virtual
  placement (`ESC _ G a=T,U=1`), then each row is U+10EEEE coloured with the
  image id plus a row diacritic. Being text, the placeholders survive the
  full-screen emulator, which keeps one combining mark per cell and passes
  `ESC _ G` through. The alternate screen holds its own images, so the
  graphics commands are replayed before the conversation is printed on exit.
- **Diffs and the diff panel.** Write and edit tools report unified-diff
  hunks with 3 context lines (`runtime::line_diff`, Myers' algorithm after
  trimming the common start and end; past 4,000 differing lines the middle is
  reported replaced). `diff_view` (CLI) draws hunks Claude Code style:
  numbered rows, `+`/`-` rows on full-width green/red, syntect colours, long
  lines wrapped with the marker. The screen can show a side panel
  (`screen::set_side_panel`, a `PanelRender` closure called with the panel's
  width, outside the screen lock): at ≥110 columns it takes two fifths of the
  window with its own scroll (the wheel goes to whatever is under the
  pointer) and a ✕; narrower, a dismissible notice box sits above the pane.
  `diff_panel` fills it with `git diff HEAD` plus untracked files, worked out
  on a background thread (pre-drawn there for the current width) after edits,
  turns and `/diffpanel`. Output is wrapped to `screen::conversation_cols`;
  a line drawn wider before the panel opened is cut, not wrapped, where its
  overflow is only padding or a rule.
- **Scheduling and monitors.** `runtime::scheduler` holds the session's
  cron tasks (CronCreate; 5-field expressions in local time, via `chrono`)
  and the single pending wake-up of a self-paced `/loop` (ScheduleWakeup).
  The line editor polls it while idle, next to background-job notices; due
  prompts come back as `ReadOutcome::Wake` and run as turns, so a scheduled
  prompt never interrupts a running one. A Monitor is a background job
  (`BackgroundJobs::start_monitor`) whose new lines a watcher thread turns
  into notices every 250 ms, with a timeout and a lines-per-minute cap.
- **Local tools.** `tools::local_tools` implements Claude Code tools without a
  hosted service: worktrees (`git worktree`, cwd switch), sub-agent messaging
  (finished agents keep their `Session`; SendMessage resumes them on a thread,
  replies arrive as notices), notifications, findings, feedback drafts, user
  files, EndConversation (a flag the REPL checks before each prompt) and
  WaitForMcpServers (status the CLI records after MCP discovery). The CLI
  supplies background-session rows and MCP status through setter hooks.
- **LSP.** `tools::lsp_tool` starts a language server per (command, project
  root) on first use, speaks JSON-RPC over stdio, opens and re-syncs files
  before each request, answers the server's `workspace/configuration`
  requests, and keeps `publishDiagnostics` per file. Requests time out after
  60 s; positions are 1-based at the tool boundary.
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
  windows. `read_file` results go to the model as plain text with a
  `[path: lines a-b of n]` header and are cut at a line boundary, ending with
  the `offset` to continue from. When the cap is set, a `read_file` page is
  sized to the model's context window instead (about a quarter of it, 4k-100k
  characters; `read_file_budget`), so large-context models read whole files;
  up to 64k windows a page is half that, as the prompt, tools and reply
  already take half the window. Lines are numbered (`<n>\t<text>`, as
  Claude Code, opencode and hermes number them). A read of a whole file
  (no `offset`/`limit`) that does not fit in one page returns the file's
  outline instead: the numbered lines that start its functions, types and
  sections (`read_file_outline_for_model`); the same call again returns the
  first page. Paging through whole files filled a 32k window, compaction
  dropped them, and the model read them again (nine compactions in one run).
  The terminal shows the range the model got, not the range the tool read.
  A `read_file` result identical to one still in the session (unpruned) is
  replaced by a short "unchanged, see above" note
  (`ConversationRuntime::note_unchanged_read`, after hermes' read dedup).
- **Deferred tools.** For every model, tools outside `ALWAYS_LOADED_TOOLS`
  are sent only as names in `ToolSearch`'s description
  (`defer_tool_definitions`); a search loads its matches into
  `LOADED_TOOLS`, returns their full definitions, and they are sent from the
  next request on (hosted models used to get all 87 in full; Nemotron then
  told the user it had no tool for the web). `NEVER_SENT_TOOLS` (the
  test-only `TestingPermission`) never reach a model. The system prompt
  names the loaded settings files but no
  longer includes the merged settings.
- **Model-sized limits.** Auto-compaction triggers at the model's window
  minus its reply budget and a margin for every model whose window is known
  (`window_auto_compaction_threshold`), not only local ones. Hosted models
  write the compaction summary themselves (`ApiClient::summarize`,
  `runtime::SUMMARY_SYSTEM_PROMPT`); local ones keep the summary built from the
  messages. Non-`read_file` tool results are capped per model
  (`tool_output_budget`) and saved whole to `~/.claw/tool-output/` when cut.
  Session JSONL keeps blocks up to 256k characters. OpenRouter requests for
  Anthropic and Gemini models carry `cache_control` markers
  (`openai_compat::add_cache_markers`).
- **Side questions during a turn.** `/btw` typed while a turn runs goes to
  `turn_keys::set_side_question_handler`: the CLI answers it on a thread from
  the last request sent to the model (`SIDE_CONTEXT`) and the answer is printed
  between steps (`turn_keys::print_side_answers`). claw desktop's agent queues
  prompts that arrive during a turn with `turn_keys::queue_for_running_turn`.
- **Reasoning across a turn's steps.** The OpenAI-compatible request sends
  each assistant message's thinking back as `reasoning_content` for the running
  turn (after the last user request) to local models and to models models.dev
  marks `interleaved` (as `reasoning` to OpenRouter, which passes it on), as
  hermes and opencode do. Without it, thinking models
  lost their plan between tool calls and re-derived it at every step. For
  `local/` models each step's reasoning is replayed from its end, at most
  4,000 characters (`replayed_reasoning`, `CLAW_REASONING_REPLAY_MAX_CHARS`):
  one 36k-character step otherwise took ~9k tokens of a 32k window on every
  later step. A reply
  cut off while still thinking is continued with the tail of that thinking and
  an instruction to decide and act, not a bare "continue"; the cut-off reply
  itself is not kept, since replaying a whole reply budget of thinking filled
  small windows and forced a compaction that dropped the files just read.
  For `local/` models that next request also switches thinking off
  (`chat_template_kwargs.enable_thinking = false`), since Qwen asked to think
  briefly still thought up to the limit again. The 3-continuation cap counts
  cut-offs in a row; a tool call in between resets it. The request after a
  tool call cut off at the limit also goes without thinking
  (`ApiRequest::follows_thinking_cut_off`). A reply that stops
  with only thinking is handled like one cut off mid-thought (hermes treats
  such replies as empty and retries); it used to end the turn silently.
- **Social media.** `tools/src/social/` holds `igd` (Instagram), `fbd`
  (Facebook) and `pind` (Pinterest), ported from the herald service with
  their SSRF hardening (per-platform host allowlists, DNS-pinned public
  addresses, size caps); `social::run` is the `SocialMedia` tool around them.
- **Capabilities prompt.** `capabilities_prompt` (terminal and desktop agent)
  tells the model what it can do beyond code: showing pictures, which tool
  fetches what, `/undo`.
- **Desktop slash commands.** The desktop agent runs the REPL's
  non-interactive slash commands (`DESKTOP_SLASH_COMMANDS`) through
  `handle_repl_command`, capturing what they print (stdout/stderr redirected
  to a pipe with `nix`) and sending it as a `notice`; its turns are
  checkpointed for `/undo`.
- **Image check.** After a turn (REPL and desktop agent), the reply's
  Markdown images are fetched as the app fetches them (`image_check`,
  `image_problem`); ones that fail or are not images go back to the model in
  one follow-up turn, as LeanGate does for type errors.
- **Prompt progress.** Requests to `local/` models ask llama.cpp for
  `return_progress`; its `prompt_progress` chunks reach
  `api::set_prompt_progress_hook`, which the CLI shows on the activity row
  and spinner and the desktop agent sends as `progress` events.
- **Shell.** The bash tool, its sandbox and `Monitor` run commands with
  `bash -c` when bash is installed (`runtime::shell_program`), else `sh -c`;
  where `sh` is dash, models' bash syntax (`<(...)`, `[[ ]]`) failed.
- **Tolerant tool calls.** Calls by other agents' tool names (`Read`, `read`,
  `Bash`) resolve to claw's tools (`CliToolExecutor::resolve_tool_name`), and
  the file tools accept their argument names (`file_path`, `oldString`, …).
  `edit_file` matches exactly, then with typographic quotes/dashes/spaces as
  plain ones, then line by line ignoring trailing whitespace and `\r`, then
  ignoring indentation when that finds a single place (`find_edit_matches`,
  after opencode), re-indenting `new_string` to the file (`reindent`); line
  numbers copied from `read_file` output are stripped.
  It refuses an `old_string` that occurs more than once unless `replace_all`
  is set. A miss shows the closest lines of the file, numbered
  (`closest_lines`, as aider does), so the model fixes `old_string` without
  re-reading the file. Tool errors reach the model as plain text.
- **Repeat guard.** When a tool call repeats the previous call exactly and
  gets the same result, the CLI adds a note to that result telling the model
  that repeating it will not return anything new (`CliToolExecutor::note_repeated_call`).

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

## Local context budgets

For `local/` models the window comes from the server itself:
`api/src/providers/local_window.rs` asks llama.cpp's `/props?model=<id>` (next
to the `CLAW_LOCAL_BASE_URL` API root; the model id lets a router-mode server
answer for that model) for `n_ctx`, caches it per model for 30 s, and falls
back to `CLAW_CONTEXT_WINDOW` for servers without `/props`. From it,
`max_tokens_for_model` asks for a quarter of the window as the reply budget
(instead of 64K, which let a reply run until the window was full), and
`build_runtime_with_plugin_state` sets the auto-compaction threshold to what
is left of the window after the reply budget and a 1/16 margin (about 69%)
unless `/autocompact` or `CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS`
set one. `maybe_auto_compact` counts the last reply's output tokens as well
as its input. The post-tool stall timeout is 120 s for `local/` models
(10 s otherwise), since a local server sends nothing until it has read the
whole prompt.

A reply that stops at the token limit (`max_tokens` / `length`) is marked
with `AssistantEvent::OutputLimitReached`. The turn loop then does not run a
last tool call whose input is unfinished JSON (it gets an error result telling
the model to send less per call), and a text-only reply is continued with a
reminder, up to 3 times per turn, instead of ending the turn silently.

## Context-window recovery

When a request fails with a context-overflow signature (`context_window`,
`exceed_context_size`, llama.cpp's "Context size has been exceeded", a
non-SSE body with no parseable content, or a non-timeout decode error),
`LiveCli` retries with progressively harder compaction: it keeps 4, then 2, 1, 0
recent messages, running the Trident pipeline plus summary compaction
(`runtime/src/trident.rs`, `compact.rs`) before each retry. If the error
reveals the server's real context window, the auto-compaction threshold is
reset from it the same way, and for `local/` models the window is remembered
(`api::note_local_context_window`) so the fresh runtime of the next turn uses
it too. Timeouts are excluded: compaction cannot fix them.
Auto-compaction (`maybe_auto_compact`) sizes what it keeps: it estimates the
prompt overhead (system prompt, tools) from the last request and keeps 4, 2
or 1 recent messages, cutting large kept tool results last, until the request
lands halfway between that overhead and the threshold. A fixed 4 messages
stayed over the threshold on a 32k window and compacted on every step.
Merged summaries carry earlier user requests and key files once each and drop
per-chunk bookkeeping, so they stay bounded.
Each compaction summary carries a "Work done so far" list (one line per tool
call: tool, path or command, outcome) across later compactions, so a model on
a small window still knows which edits and builds it already did.
Each round also cuts tool results longer than about a quarter of the window
(`truncate_large_tool_results`, limit from `overflow_tool_result_limit`),
since one whole large file in a kept message can overflow the window alone.
The window is the one the error reports, else the model's known window
(`known_context_window`: `api::local_context_window` for `local/` models, else
`api::model_token_limit`, else `api::hosted_context_window`: the models.dev
catalog in `api::providers::catalog`, cached in `~/.claw/cache/`, then the
provider's `/models` list). The same catalog supplies the OpenAI-compatible
providers that `api::all_hosted_providers` lists after the built-in and custom
ones (`catalog_hosted`, loaded only when a model prefix is not otherwise known), else the size of the request that overflowed, an
upper bound that shrinks each round; so large-context models are cut only for
enormous results. `CLAW_OVERFLOW_TOOL_RESULT_CHARS` or the
`overflowToolResultChars` setting overrides it (`off` never cuts). A round
that changes nothing is skipped. The retry continues the failed turn
(`ConversationRuntime::continue_turn`) with the work it already did, rather
than sending the prompt again; the prompt is re-sent only when compaction
removed it.

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
| Project config | `.claw.json`, `.claw/settings.json`, `.claw/settings.local.json` (see [`USAGE.md`](./USAGE.md#config-file-resolution-order)); only presentation keys apply until trusted |
| Project config trust | `~/.claw/trusted-project-configs.json` (mode 0600): canonical path → SHA-256 of the parsed file, written only by `claw trust` and user edits through claw (`runtime/src/project_trust.rs`) |
| Known workspaces | `~/.claw/workspaces.txt`: every folder claw started a session in, for the desktop app |
| Desktop app state | `~/.claw/desktop.json` (projects, chat titles, which sessions the app started, preferences, scheduled tasks); project instructions in `<folder>/.claw/instructions.md`; skills, MCP servers and turned-off tools go to claw's own `~/.claw/skills` and `~/.claw/settings.json` |

## Desktop app

`crates/claw-desktop` (Iced) is a front end only. Each open chat starts
`claw desktop-agent [--resume <session.jsonl>]` (`rusty-claude-cli/src/desktop_agent.rs`)
in the chat's folder, through the `~/.local/bin/claw` launcher, and talks JSON
lines over its stdin/stdout. The agent reuses `LiveCli`'s per-turn runtime
(`prepare_turn_runtime`) with `emit_output = false`; hooks in the stream loop
(`emit_text`, `emit_thinking`, `emit_tool_call`) and in
`CliToolExecutor::report_result` (`emit_tool_result`) report the turn as
events, `DesktopPermissionPrompter` sends approvals to the app and waits for
the answer, and `cancel` trips the turn's `HookAbortSignal` like Esc. The app
reads session files directly for the sidebar and history
(`claw-desktop/src/sessions.rs`), so terminal sessions appear there too.

`--incognito` clears the session's persistence path and removes the file
`LiveCli::new` wrote, and the turn skips `persist_session`, so nothing reaches
disk. The agent adds a system-prompt section telling the model it can show
images with Markdown and that commands have no terminal.

In the app, `markdown_view.rs` is a custom `markdown::Viewer`: code blocks are
highlighted with `iced::highlighter` (Solarized dark, InspiredGitHub in light
mode; results cached by content hash), wrap instead of scrolling, and images
load asynchronously (reqwest or the file system) into a URL-keyed cache.
`settings.rs` edits claw's `settings.json` (`mcpServers`,
`permissions.deniedTools`, keeping a `.bak`) and installs skills. Scheduled
tasks are checked every 30 s with `runtime::scheduler::CronSchedule` and run
as new chats, so they only run while the app is open.

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

## Stateful command actions

`commands::SlashCommand::LocalAction` routes the additional REPL operations to
`rusty-claude-cli/src/command_actions.rs`. Legacy report variants remain available
for `--resume`; mutation dispatch is limited to the live REPL. The settings layer
validates assignment batches before writing, preserves unrelated JSON keys, and
atomically replaces `.claw/settings.local.json`. Theme, model, and permission-mode
edits update live state; rebuilding the runtime reloads permission rules, memory,
plugins, and MCP discovery. MCP enablement uses the boolean map `mcpServerStates`:
project-local `true` can override inherited `false`, unlike additive array settings.
Disabled servers are filtered before transport construction and tool discovery.

Artifacts stay under `.claw/artifacts`; attachment resolves canonical paths to
reject traversal and symlink escape, enforces a 1 MiB UTF-8 limit, and persists the
attached text in the session. Generation/reference commands send explicit prompts
through the normal tool and permission pipeline. Account commands launch fixed
HTTPS destinations as process arguments and never send conversation state.
External editors temporarily release the full-screen renderer and restore it on
return. The line editor's optional Vim mode implements insert/normal state and a
small documented motion set without changing Ctrl-key handling.

## Shell sandbox execution

`runtime::sandbox` builds a Bubblewrap mount/user/PID sandbox and probes the
requested setup using the same launcher as execution. Filesystem status is active
only after that probe succeeds. The default is a read-only host root with a
writable workspace and private temporary storage; configured allow-list roots
are canonicalized and must exist. Network isolation is probed with `--unshare-net`
when requested. Missing backends, invalid mounts and denied namespaces fail
closed. There is no unshare/plain-shell fallback or fake-root Portage environment.

As in Claude Code, the sandbox confines writes, not reads: the environment and
readable host files are inherited. Mounts are applied in order: read-only root,
writable roots, then the paths that execute later outside the sandbox
(`.git/hooks`, `.git/config`, `.envrc`, claw's project config) read-only inside
each writable root. The file-writing tools run in claw itself, so
`PermissionPolicy` requires full access for writes to those same paths.
`sandbox.excludedCommands` (`SandboxConfig::excludes`) runs a command on the
host when every simple command in it starts with an entry; a sandboxed command
that fails on a refused write gets a note telling the model to request a host
run (`dangerouslyDisableSandbox`).

Project config files are gated in `ConfigLoader` (`project_trust`): until the
user approves a file's exact content, only presentation keys are merged, so a
cloned repository cannot turn the sandbox off or add hooks and MCP servers.

`PermissionPolicy` treats Bash `dangerouslyDisableSandbox: true` as a separate
host permission request, after deny rules and before allow rules/mode shortcuts.
The CLI distinguishes this prompt and binds remembered host approvals to the full
input and cwd. `ConversationRuntime` keeps Bash calls serial and installs a
private, thread-local, single-use execution scope only around an authorized call.
`execute_bash` consumes that scope after matching the typed input and cwd; direct
tool dispatch cannot manufacture approval through JSON. Scope teardown clears an
unused grant. Ordinary commands cannot weaken the configured sandbox through
per-call override fields. Background jobs use the same checked launcher.

The parent Claw process is the host executor; it is not itself launched inside the
per-command sandbox. An outer sandbox on Claw still applies to host requests.
This boundary covers Bash writes and requested network namespaces, not
confidentiality of host files; localhost stays reachable without network
isolation. Other tools retain their own permission
checks.
