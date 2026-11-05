# Infinity engine: multi-threaded asynchronous agents for claw

Status: phases 1–3 landed (opt-in, `CLAW_ENGINE=infinity`). Started 2026-11-05.

This document records the decision to move claw's agent loop onto the
[Infinity runtime](https://github.com/hydro-project/infinity)
(`infinity-agent-core`, Apache-2.0, on crates.io) and the phased plan to get
there. For the current (default) loop see [`ARCHITECTURE.md`](../ARCHITECTURE.md).

## Why

claw's loop (`runtime::ConversationRuntime::run_turn`) is synchronous:

- `ApiClient::stream` blocks the calling thread until the whole reply has
  streamed, wrapping async `api` code in `block_on`.
- A tool call holds the turn open until the tool returns. A 10-minute build
  keeps the turn blocked for 10 minutes.
- Parallelism is limited to batches of read-only tools on scoped threads.
- Sub-agents (`Agent` tool) each get a whole OS thread and a private
  `ConversationRuntime`. They don't share a scheduler, history store or
  reporting channel with the parent.

Infinity is built around one invariant: **an execution slice never blocks on
anything external**.

- Everything that wakes an agent is an `InputMessage` (user text, tool
  result, child report, timer) carrying a `group_id` (the thread id).
- A slice loads the thread, runs at most one completion and persists the
  turn. Then it dispatches one tool call and yields. The tool result comes
  back later as a new message.
- Order is FIFO within a thread and threads run concurrently with each
  other: one `mpsc` channel plus one tokio driver task per thread. An idle
  thread is only data in the stores.
- Threads are a built-in: `spawn_thread` (inherits history up to the spawn
  point, or `fresh_context`), `report_to_parent` and `close_thread`.
  Reports reach the parent as synthetic tool results.
- Turn buffering means a half-streamed reply never reaches history. Input is
  deduplicated by id, and history is compacted automatically at about 3/4
  of the context window.

## Decision

claw **depends on the Infinity crates** rather than re-implementing them:

| Crate | Use |
|---|---|
| `infinity-agent-core` | Agent system, thread drivers, stores, built-in thread/sleep tools |
| `infinity-provider-protocol` | `ModelProvider`, `CompletionRequest`, `StreamChunk`, message types |
| `rap-protocol` | ids (`ThreadId`, `ToolCallId`) used in the `Tool` trait |

All the glue goes in a new crate, `rust/crates/claw-engine`. `runtime` stays
free of the dependency until the old loop is retired (phase 4).
Consequence: `rap-protocol` brings in `reqwest 0.13` (native-tls) next to
claw's `reqwest 0.12` (rustls). That costs some build time but nothing at
run time, since the engine makes no RAP HTTP calls.

## Mapping

```text
           claw today                         claw on infinity
  ─────────────────────────────    ─────────────────────────────────────────
  ConversationRuntime::run_turn    LaunchingSystem + one root ThreadHandle
  ApiClient (blocking)             ClawModelProvider: impl ModelProvider
                                     (api::ProviderClient, async, streamed)
  ToolExecutor::execute            ClawTool: impl Tool<ChannelSender>
                                     fast tools  → execute_synchronous
                                                    (inline, loop back)
                                     slow tools  → execute: spawn_blocking,
                                                    result → InputMessage
                                                    (thread yields meanwhile)
  Agent tool (OS thread)           spawn_thread / report_to_parent /
                                     close_thread (infinity built-ins)
  Session (JSONL)                  ConversationStore + StateStore
                                     (in-memory in phase 1; JSON-file
                                      store mirroring Session in phase 3)
  auto-compaction                  infinity compaction threads
  Ctrl+C abort                     send_user_text interrupts; shutdown()
```

**Fast and slow tools.** A *fast* tool runs inline in the slice. Examples
are `read_file`, `glob_search`, `grep_search`, `TodoWrite` and the edit
tools: they finish in milliseconds and yielding for them would only add a
round trip. A *slow* tool is dispatched and the thread yields. Examples are
`bash`, `WebFetch`, `WebSearch`, `Agent` and `REPL`. While it runs, the
thread's driver is free: user text, child reports and other threads keep
moving. The inline list is `claw_engine::tools::SYNC_TOOLS`; everything
else is dispatched.

**Permissions and hooks.** `ToolHost::run` follows the classic loop's
order:

1. The PreToolUse hook runs. It can deny the call, rewrite its input, or
   override the permission decision.
2. The `runtime::PermissionPolicy` check runs. A call the policy can't
   decide alone goes to the prompter: `CliPermissionPrompter` in the REPL,
   behind a mutex so parallel calls ask one at a time.
3. The tool runs.
4. The PostToolUse (or failure) hook runs.

A denied call returns an `Error: …` result, which the model sees and can
recover from. Without a prompter (`-p` mode), a call that needs approval is
denied with a message saying so.

**Several tool calls in one reply.** Infinity runs only the first tool call
of a completion and ignores the rest. Models often ask for several at once,
and a model whose second call is silently dropped tends to ask for it again
and again: claw's mock harness looped forever on exactly this. So the
engine's provider wrapper (`ClawPrompt`) holds back the reply's calls and
sends them as one call to the internal `parallel_tools` tool
(`claw_engine::batch`). That tool is dispatched like any slow tool. Each run
of consecutive read-only calls runs on parallel OS threads; every other call
runs alone, in order, as before. The result carries every call's output.
When history goes back to the provider, the batch is unpacked again, so the
model only ever sees its own `tool_use` blocks and one `tool_result` for
each. A single call goes through untouched.

## Phases

1. **Engine crate and headless mode** (done)
   - `claw-engine`: `ClawModelProvider` (Anthropic and OpenAI-compatible
     through `api::ProviderClient`, with `ApiError` mapped to `ErrorClass`),
     `ClawTool` adapters over `tools::GlobalToolRegistry` with policy
     checks, and the `Engine` facade (`launch`, `run_prompt`, wait until
     every thread is idle).
   - `claw -p "…"` / `claw prompt …` runs on the engine when
     `CLAW_ENGINE=infinity`. The root's reply streams to stdout. One line
     per finished tool call, child-thread output (`[#1] …`) and a summary
     go to stderr. `--output-format json` prints a summary object.
   - `parallel_tools` batching, so a reply's several tool calls all run.
   - Tests use a scripted `ModelProvider`: text, inline tools, dispatched
     tools, batches, children reporting back, and permission denial. Six
     mock-anthropic scenarios also pass end to end through `claw -p`:
     streaming text, read, grep, write allowed or denied, bash, and a
     two-call reply.
2. **Interactive REPL on the engine** (done)
   - `LiveCli::run_turn_inner` hands the turn to `live_engine.rs` when
     `CLAW_ENGINE=infinity`, so checkpoints, job notices and LeanGate
     still wrap it.
   - claw's `Session` stays the record. `Engine::run_turn` seeds the root
     thread's store with the session's messages (`claw_engine::history`),
     runs the turn, and returns the messages it added. Those are pushed
     onto the session, and `/undo`, `--resume`, compaction and learning
     need no change. A batch comes back as the model's own `tool_use`
     blocks and results. Adjacent same-role messages are merged for the
     provider.
   - `TurnControl`: Esc and Ctrl+C go through `TurnKeyWatcher`'s abort
     signal, which shuts the system down. Partial turns are flushed and
     kept, and the runtime is shut down in the background so a stopped
     long tool doesn't hold the REPL. Prompts typed meanwhile
     (`take_queued_for_running_turn`) are sent to the root as user text,
     which interrupts a streaming reply.
   - `ToolHost` runs PreToolUse hooks (deny, rewrite input, permission
     override), asks the user through `CliPermissionPrompter` (one
     question at a time), runs the tool through a CLI executor
     (`AskUserQuestion` picker, activity label), then runs PostToolUse
     hooks, all in the classic order.
   - Rendering reuses the CLI's markdown stream, folded tool-call and
     result blocks, activity row and diff-panel refresh. Child threads
     are labelled `[#n]`.
   - Tests: a seeded session, abort mid-tool (returns in about 0.3 s),
     mid-turn prompts, prompter allow and deny, a PreToolUse deny, and
     history round trips. Also two REPL turns in a PTY against the mock
     server, with the saved JSONL checked.
   - Not yet: the footer's live-thread count, image attachments,
     `/effort` passed to the provider, and keeping child threads alive
     across turns.
3. **Parity with the classic loop** (done)
   - **Persistence.** Phase 2 already keeps claw's `Session` as the record,
     so no Infinity store backed by the session directory is needed.
     `claw -p` in text mode now goes through `LiveCli` too, so `-p` runs
     are saved like classic ones. JSON output keeps the engine's summary
     path (`engine_prompt.rs`).
   - **MCP.** The engine REPL uses the session's own registry
     (`ConversationRuntime::tool_executor`), which includes the MCP
     servers' tools. The CLI executor runs those through the session's
     shared `RuntimeMcpState`. `run_mcp_tool` was factored out of
     `CliToolExecutor` for this. `ToolHost::with_runtime_tools` /
     `EngineConfig::runtime_tools` turns them on. MCP calls are slow
     tools, so they're dispatched.
   - **Images.** Results naming a picture (`claw-image: /path`) go through
     `runtime::tool_images::attach_tool_images`, with the same size budget
     and "left out" note. They're sent as Infinity image parts, per inner
     call for a batch. `ClawModelProvider::with_image_input(sends_images)`
     tells Infinity whether the model reads them; it sends placeholders
     otherwise. `@image` / `/image` attachments reach the model just
     before the prompt (`TurnControl::attachments`).
   - **Request options.** `/effort` is sent as `reasoning_effort`, and
     `reply_budget(model)` caps replies (`with_reasoning_effort`,
     `with_max_output_tokens`, `EngineConfig::for_provider`).
   - **Status line.** `turn_keys::set_live_threads` shows `N threads`
     while child threads run.
   - **Sub-agents.** The `Agent` tool stays: it runs claw's typed
     sub-agents (Explore, Plan, custom agents, each with its own prompt and
     tool limits). On the engine it's a slow tool, so the parent yields
     while it works. `spawn_thread` is the other kind, a fork that
     inherits the conversation. Making `Agent` an alias would have dropped
     the typed agents, so both are offered.
   - Tests: a tool image reaches a vision model, and an attachment goes
     before the prompt without being saved as part of the turn. Also
     checked by hand: an MCP tool called through `claw -p` (stdio fixture
     server plus a scripted SSE model), and the run saved as a session.
   - **Measured on a real local model** (cybertiel-35b-a3b on llama-gpu,
     logging proxy plus llama-server's `prompt eval time`). Two fixes came
     out of it:
     - *Tool list.* The engine offered all 40 tools; the classic loop sends
       21 (`sent_tool_definitions`: a local model's core set, rarely used
       tools behind `ToolSearch`). `EngineConfig::tool_list` /
       `ClawPrompt::tool_list` now offers the same list, plus the thread
       tools (`THREAD_TOOLS`), on every request. Every tool stays
       registered, so a tool `ToolSearch` loads still runs, and `ToolSearch`
       goes through the CLI's search so it records what it loaded. The
       first request dropped from 19,714 to 14,920 prompt tokens (classic:
       13,950).
     - *Stable root thread.* The thread-id line sits near the top of the
       preamble, and a new root id every turn made every REPL turn miss
       the server's prompt cache (about 15k tokens, around 2.5 min on this
       machine). `TurnControl::thread_id` (the claw session id in the
       REPL) seeds the store with `ensure_root_thread` and attaches through
       `thread_handle`. Turn 2's first request now processes 38 tokens, and
       the turn took 24 s.

4. **Flip the default and retire the old loop.** `CLAW_ENGINE=legacy`
   stays for one release. Then `ConversationRuntime`'s loop is deleted and
   `runtime` keeps sessions, config, prompt assembly and permissions.

## Current limitations

- `-p` with `--output-format json` has no permission prompts and isn't
  saved as a session (text output has both).
- Slow tools run on tokio's blocking pool and can't be cancelled once
  started. A stopped turn returns at once and leaves them to finish.
- Each turn builds a fresh engine, so child threads don't outlive the turn
  that spawned them.
- claw's tool-name repair (calling `Read` for `read_file`, …) and its
  repeated-call and pagination nudges live in `CliToolExecutor` and aren't
  applied on the engine. An unknown name gets Infinity's "tool does not
  exist" result, and the model retries.
- `Agent` sub-agents still run the classic loop inside; they move with
  phase 4.
