# Claude tool implementation matrix

Source: the Claude Code tools reference (the table of 46 tools the user supplied on 2026-10-13: name, description, permission required). Every row of that table is listed here once.

Implementation audit: the canonical Rust working tree at `fe1b79e` plus the local changes. `yes` means claw has a working tool doing the core of what the Claude Code tool does, under the same name or the claw name given in the description column. It does **not** mean full parity; differences are stated in the description. `no` means absent, or present under the same name with a different purpose.

`implemented-by`: `claw` for tools that were already in the fork and work (authorship not verified), `opus` for tools Opus added or made work on 2026-10-13. When Opus fixed or extended an existing tool, the row says `claw, opus`. Unimplemented rows are blank.

## Opus pass (2026-10-13)

Before this pass, these tools were missing: `Monitor`, `EnterWorktree`, `ExitWorktree`, `ListAgents`, `SendMessage`, `PushNotification`, `ReportFindings`, `ScheduleWakeup`, `SendFeedback`, `SendUserFile`, `EndConversation` and `WaitForMcpServers`. Three more existed but did not work:
- `LSP` returned a placeholder ("dispatched") and never started a language server.
- `CronCreate` stored entries that nothing ever ran.
- `TaskOutput`/`TaskStop` only knew claw's task list, not background shells.

All of these now work. The new tools are in `rust/crates/tools/src/local_tools.rs`, the LSP client is in `tools/src/lsp_tool.rs`, and the scheduler is in `runtime/src/scheduler.rs`.

Verification:
- `cargo test -p tools`: 125 passed. This includes an LSP test against a real `clangd` (symbols, go-to-definition, diagnostics after an edit), skipped where clangd is not installed.
- `cargo test -p runtime`: scheduler tests (cron matching, steps, ranges, leap days, wake-ups) pass.
- `cargo test -p commands`: 44 passed.
- `cargo test -p rusty-claude-cli --bin claw`: 285 passed. One parallel run hung; it did not recur in ten reruns. The command matrix records the same intermittent hang before this pass.
- End to end through the REPL against a mock model server:
  - `Monitor`'s lines woke the idle session once per batch, then once on exit.
  - A one-shot `CronCreate` (`* * * * *`) fired once, at the next minute, while the prompt was idle.

Count (Claude Code's table): **41 yes, 5 no**: 17 `claw`, 12 `opus`, and 12 `claw, opus` (existing tools Opus fixed or extended).

Note for local setups: the `claw` launcher in `~/.local/bin` passes `--allowedTools` with a core list. New tools reach the model only if they are added to that list or `CLAW_ALL_TOOLS=1` is set.

|Tool|Description|Permission|Implemented-yes/no|implemented-by|
|---|---|---|---|---|
|Agent|Spawns a sub-agent with its own context: foreground (waits for the result) or `run_in_background` (result arrives as a notice). Its conversation is now kept, so `SendMessage` can resume it. No agent teams.|No|yes|claw, opus|
|Artifact|Publishes a page on claude.ai. Hosted; not possible locally. `/artifacts` manages local files in `.claw/artifacts` instead.|Yes|no||
|AskUserQuestion|Multiple-choice questions in the terminal picker; the answer goes back to the model.|No|yes|claw|
|Bash|`bash`: runs shell commands, with risk-classified approval and background jobs (`run_in_background`, `BashOutput`, `KillShell`).|Yes|yes|claw|
|CronCreate|Schedules a prompt with a 5-field cron expression in local time (or `@hourly`/`@daily`/…), recurring or one-shot; it runs as a turn when the session is idle. Accepts `cron` or `schedule`. Session-scoped: not restored on `--resume`.|No|yes|claw, opus|
|CronDelete|Cancels a scheduled task by id.|No|yes|claw, opus|
|CronList|Lists scheduled tasks with next run time and run count.|No|yes|claw, opus|
|Edit|`edit_file`: exact string replacement in a file. Now reports a real diff (Myers, 3 context lines), shown Claude Code style.|Yes|yes|claw, opus|
|EndConversation|Ends the session after the current reply; the REPL saves it and exits.|No|yes|opus|
|EnterPlanMode|Switches to plan mode (read-only) via the local settings override.|No|yes|claw|
|EnterWorktree|Creates `.claw/worktrees/<name>` on branch `claw/<name>` with `git worktree add` and moves the session there, or enters an existing worktree of the repository by path. Paths outside `.claw/worktrees/` do not get a separate approval prompt; the tool needs workspace-write.|Yes|yes|opus|
|ExitPlanMode|Presents the plan and leaves plan mode, restoring the previous mode.|Yes|yes|claw|
|ExitWorktree|Returns to the original directory. `keep` leaves the worktree; `remove` deletes it and its branch, refusing uncommitted changes unless `discard_changes`.|No|yes|opus|
|Glob|`glob_search`: file name patterns.|No|yes|claw|
|Grep|`grep_search`: regex search of file contents.|No|yes|claw|
|ListAgents|Lists this session's sub-agents (id, name, status, queued messages), plus background claw sessions and background shells for reference. No other local or cloud Claude sessions, no Remote Control.|No|yes|opus|
|ListMcpResourcesTool|`ListMcpResources`: resources of connected MCP servers, through the CLI's MCP manager.|No|yes|claw|
|LSP|A real language-server client: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, call hierarchy, diagnostics. Built in: rust-analyzer, pyright/basedpyright/pylsp, typescript-language-server, gopls, clangd, jdtls, lua-language-server, solargraph/ruby-lsp, zls (first found on PATH). Others via `lspServers` in settings. Servers stay up for the session.|No|yes|claw, opus|
|Monitor|Runs a command in the background; each batch of new output lines becomes a notice, delivered between steps and waking an idle prompt. Stops on timeout (default 5 min, max 1 h), on exit, or above 600 lines a minute. No WebSocket source.|Yes|yes|opus|
|NotebookEdit|Replaces, inserts or deletes Jupyter notebook cells.|Yes|yes|claw|
|PowerShell|Runs PowerShell commands when `pwsh`/`powershell` is installed.|Yes|yes|claw|
|PushNotification|Desktop notification via `notify-send` (Linux) or `osascript` (macOS), plus the terminal's OSC 9 notification. No phone push; that needs Anthropic's hosted Remote Control.|No|yes|opus|
|Read|`read_file`: text files with offset/limit; PDFs, Office files and notebooks are extracted; images (png, jpg, gif, webp, bmp) are attached for the model to see (since 2026-10-18).|No|yes|claw, opus|
|ReadMcpResourceTool|`ReadMcpResource`: reads an MCP resource by URI.|No|yes|claw|
|RemoteTrigger|Claude Code's tool manages claude.ai Routines (hosted). Claw's `RemoteTrigger` is a different thing, a generic HTTP request tool. `/schedule` provides local cron routines instead.|No|no||
|ReportFindings|Takes structured review findings (file, line, summary, failure scenario, optional category, verdict and outcome) and shows them as a numbered list. `/ultrareview` uses it.|No|yes|opus|
|ScheduleWakeup|Sets when the next iteration of a self-paced `/loop` runs (60–3600 s), or `stop: true` to end it; the wake-up runs as a turn while the prompt is idle. `/loop stop` cancels it too.|No|yes|opus|
|SendFeedback|Saves a feedback draft as JSON under `~/.claw/feedback/drafts`. Nothing is sent.|No|yes|opus|
|SendMessage|Messages a sub-agent of this session by id or name. A finished agent is resumed with its earlier conversation, and the reply arrives as a notice. A running one gets the message when its run ends. Other claw sessions and machines cannot be reached.|No|yes|opus|
|SendUserFile|Hands files to the user: lists them with sizes and, for `render` (or images, PDFs and HTML by default), opens them in the desktop viewer. No Remote Control or cloud delivery.|No|yes|opus|
|ShareOnboardingGuide|Uploads ONBOARDING.md for a share link. Hosted; not possible locally.|Yes|no||
|Skill|Runs a skill in the main conversation.|Yes|yes|claw|
|SubagentHandback|Specific to Claude Code's auto-mode classifier review of sub-agent reports. Claw has no such classifier; sub-agent results come back directly.|No|no||
|TaskCreate|Creates a task in the session's in-memory task list.|No|yes|claw|
|TaskGet|Gets a task's details.|No|yes|claw|
|TaskList|Lists tasks, background shells and monitors, and sub-agents, with their status (since 2026-10-18; before, only TaskCreate tasks).|No|yes|claw, opus|
|TaskOutput|New output of a background shell or monitor (by job id), else a task's recorded output. `BashOutput` does the same for shells.|No|yes|claw, opus|
|TaskStop|Stops a background shell or monitor by job id (with its final output), else marks a task stopped. Running sub-agent threads cannot be stopped.|No|yes|claw, opus|
|TaskUpdate|Updates a task's status or details.|No|yes|claw|
|TodoWrite|Manages the session checklist (shown in the transcript).|No|yes|claw|
|ToolSearch|Searches for and loads deferred tools, including MCP tools.|No|yes|claw|
|WaitForMcpServers|Reports each MCP server's status (connected or failed, with the error). Claw connects MCP servers when the session or `/mcp reconnect` builds the runtime, so there is no background connection to wait for; failed ones are retried with `/mcp reconnect`.|No|yes|opus|
|WebFetch|Fetches a URL and returns its text (HTML converted), with the HTTP status.|Yes|yes|claw|
|WebSearch|Web search with result snippets and links. Order: local SearXNG (`scripts/install-searxng.sh`, 127.0.0.1:8888; merges Google, DuckDuckGo, Brave, Bing, …), DuckDuckGo (skipped for 30 minutes after its bot check), Brave Search, Bing. Since 2026-10-18; before that every search from this machine came back empty because DuckDuckGo blocks it.|Yes|yes|claw, opus|
|Workflow|Runs Claude Code's dynamic workflow scripts (orchestrated background sub-agents). Not implemented. `/batch` and `/ultrareview` do comparable fan-out through the Agent tool.|Yes|no||
|Write|`write_file`: creates or overwrites a file. Now reports a real diff for overwrites and a numbered preview for new files.|Yes|yes|claw, opus|

## Claw tools without a Claude Code row

In claw's registry but not in the source table. All `yes` (implemented and working); tested on 2026-10-18 unless noted.

|Tool|What it does|Permission|implemented-by|
|---|---|---|---|
|`CodeSearch`|Codebase search like opencode/Hermes, plus a call graph (`kind: "callers"` / `"callees"`, after Neruva's code KG): a cached symbol index (functions, types, classes, modules, macros across Rust, Python, JS/TS, Go, C/C++, Java/Kotlin, Ruby, Lua, shell; stored under `~/.claw/index/`, refreshed per changed file), ranked ripgrep text search (scored by how many query terms meet in one stretch of a file) and path matches. 2,253-file llama.cpp tree: first index a few seconds, later symbol searches about 0.2 s.|read-only|opus|
|`Pty`|Interactive programs in a pseudo-terminal (REPLs, ssh, installers that ask, TUIs, debuggers): start, send text, press keys (ctrl-c, arrows, …), read new output, list, kill. Runs each session under util-linux `script`; escape codes are stripped unless `raw`.|full access|opus|
|`Checkpoint`|Snapshots and undo: save the work tree before risky edits, list, diff since a checkpoint, restore (which first saves the current state as `undo_id`). Uses claw's own snapshot repository, never the project's history.|workspace-write|opus|
|`Diagnostics`|Runs the project's type checkers, linters and formatters and returns their findings: cargo check/clippy/fmt, pyright or mypy, ruff or black, tsc/eslint/prettier, go vet/build/gofmt, shellcheck, clang-tidy (one C/C++ file, with compile_commands.json). Only applicable, installed ones; `fix` lets them rewrite files.|workspace-write|opus|
|`RepoSignals`|The repository as a pheromone field (after Physarum): load-bearing files (imported by many, read per language: Rust `crate::`/`mod`, C `#include`, JS/TS imports, Python imports), contested files (churn in the last N days), failing files (named by the last failing Diagnostics/LeanGate check or a given test command), protected files (lcov coverage ≥ 80%) and cleanup candidates; or one file's full reading before editing it. 2,255-file llama.cpp tree: about 2.6 s.|read-only|opus|
|`Trail`|Shared, fading signals between agents on one repository (after Stigmergy): `leave` a claim/danger/info/attraction trace on a path, `sense` traces on, above or below a path, `clear` your own. Exponential decay (half-life per trace, claims 30 min, others 60); one file per repository, shared by its worktrees, under `~/.claw/trails/`.|read-only|opus|
|`GoogleDork`|Search-operator queries (site, filetype, intitle, inurl, intext, exact phrases, exclusions, date range), run through the WebSearch backends, plus Google/Bing/DuckDuckGo links for the exact query.|read-only|opus|
|`vision_analyze`|Shows an image (file or URL) to the model with a question. Needs a model with vision input.|read-only|opus|
|`image_generate`|Text to image: an OpenAI-compatible images API (`CLAW_IMAGE_API_URL`) or a local stable-diffusion.cpp `sd` (`CLAW_SD_MODEL`). Implemented; no backend is set up on this machine yet, so it currently explains what to install.|workspace-write|opus|
|`text_to_speech`|Speech: an OpenAI-compatible API (`CLAW_TTS_API_URL`), piper (`CLAW_PIPER_MODEL`) or espeak-ng; saves audio and can play it. Implemented; no engine is installed on this machine yet (`emerge app-accessibility/espeak-ng` is the quickest).|workspace-write|opus|
|`X11`|Desktop control: windows, typing, clicks, screenshots (shown to the model).|full access|claw, opus|
|`Browser`|Drives a Chromium browser: open, click, type, read, screenshots.|full access|claw, opus|
|`COM`|Serial ports: list, read, write.|full access|claw, opus|
|`Download`|Saves files from the web into `~/Downloads`.|full access|claw, opus|
|`write_docx`, `write_pptx`, `write_xlsx`|Write Word, PowerPoint and Excel files.|workspace-write|claw|
|`GitStatus`, `GitDiff`, `GitLog`, `GitShow`, `GitBlame`|Read-only git. Commits, branches, push and the rest go through `bash` (git directly), with the commit/push checks in the system prompt.|read-only|claw|
|`BashOutput`, `KillShell`|Read and stop background shells.|—|claw|
|`PowerShell`|PowerShell commands; uses `pwsh` on Linux (installed here) or `powershell` on Windows.|full access|claw|
|`Sleep`, `SendUserMessage`, `Config`, `StructuredOutput`, `REPL`, `RunTaskPacket`, `McpAuth`, `MCP`|Utility and MCP tools.|varies|claw|
|`WorkerCreate`, `WorkerGet`, `WorkerObserve`, `WorkerObserveCompletion`, `WorkerResolveTrust`, `WorkerAwaitReady`, `WorkerSendPrompt`, `WorkerRestart`, `WorkerTerminate`, `TeamCreate`, `TeamDelete`|Worker sessions and teams of sub-agents.|varies|claw|
|`TestingPermission`|Internal test tool for permission checks (hidden in the desktop app).|—|claw|

## Built into existing tools (2026-10-19)

- **LeanGate** (in `write_file`/`edit_file` and the turn loop): a new function or type whose name already exists elsewhere in the repository is held back once with a pointer to the existing one (the same call again writes it); similar names get a `leangate` note in the result. When a turn that edited files ends, the project's type checker runs; errors in the edited files go back to the agent (up to 2 rounds). `"leanGate": false` in settings or `CLAW_LEANGATE=0` turns it off.
- **Parkour source adapters** (in `WebFetch`): Wikipedia (MediaWiki API, plain text), arXiv (export API; DOIs `10.48550/arXiv.*` too), GitHub (repository + README, raw files, folder listings, issues and pull requests with comments; `GITHUB_TOKEN` optional) and Semantic Scholar / doi.org (Graph API; `SEMANTIC_SCHOLAR_API_KEY` avoids the shared rate limit) are read through their APIs; results start with a YAML envelope (source, url, title, `next` links). Anything else, or a failing API, is fetched as before.
- **`/scope-check <idea>`** (alias `/scope`, after Scope Creeper): delusion score and verdict, a kill case, and a shippable v0 plan. **`/diverge <problem>`** (after Divergent Agents' simple mode): technique choice, diverse ideas, diversity audit, blind spots.
- **Memory** (after Mnemo and Neruva, inside claw's existing auto memory): code `anchors:` with fingerprints, so memories about code that changed are flagged stale and ranked last; activity-based dormancy (sessions in the project, not wall-clock time); `/reflect` and reflection on `/exit` to save decisions, facts, mistakes and preferences.
- Deferred: Harnyx-style harness optimization (see TODO.md). Not built: Chameleon (runtime self-modifying tools), DKGI (a pattern `REPL` already allows); Preciso and Logic Lab are better connected as MCP servers.

## Requested capabilities (2026-10-18)

|Capability|Tool(s)|Status|
|---|---|---|
|LSP, go-to-definition, symbols|`LSP`|yes (existed)|
|LSP diagnostics, type checkers, linters, formatters after edits|`LSP` (diagnostics op), `Diagnostics`|yes (`Diagnostics` new)|
|Google dorking|`GoogleDork`|yes (new)|
|Codebase search / indexing|`CodeSearch`|yes (new)|
|PTY / interactive shell|`Pty`|yes (new)|
|PowerShell|`PowerShell`|yes (existed; `pwsh` on Linux)|
|WebFetch|`WebFetch`|yes (existed)|
|Web search|`WebSearch`|fixed: was returning no results; now SearXNG first (includes Google), with fallbacks|
|Image read|`read_file` on images, `vision_analyze`|yes (new)|
|Task management for subagents|`TaskList`, `TaskStop`, `TaskOutput`, `TaskUpdate`|yes; `TaskList` now shows shells, monitors and agents|
|SendMessage / inter-agent messaging|`SendMessage`, `ListAgents`|yes (existed)|
|Git operations|`bash` plus `GitStatus`/`GitDiff`/`GitLog`/`GitShow`/`GitBlame`|yes (existed)|
|Worktree management|`EnterWorktree`, `ExitWorktree`|yes (existed)|
|Snapshots / undo|`Checkpoint`; `/undo`, `/rewind` in the REPL|yes (`Checkpoint` new)|
|List MCP resources|`ListMcpResources`, `ReadMcpResource`|yes (existed)|
|Monitor|`Monitor`|yes (existed)|
|Media: vision, image generation, text to speech|`vision_analyze`, `image_generate`, `text_to_speech`|yes (new); generation and speech need a backend installed|

The terminal launcher (`~/.local/bin/claw`, from `scripts/wrappers/claw`) offers the model a core list to save context on the local model; `code_search`, `pty`, `checkpoint`, `diagnostics`, `google_dork`, `vision_analyze`, `repo_signals` and `trail` are in it. Run `CLAW_ALL_TOOLS=1 claw` for every tool.

Audit evidence: [tool registry and dispatch](rust/crates/tools/src/lib.rs), [local tools](rust/crates/tools/src/local_tools.rs), [LSP client](rust/crates/tools/src/lsp_tool.rs), [scheduler](rust/crates/runtime/src/scheduler.rs), [background jobs and monitors](rust/crates/runtime/src/background_jobs.rs), and [CLI tool executor and idle wake](rust/crates/rusty-claude-cli/src/main.rs).
