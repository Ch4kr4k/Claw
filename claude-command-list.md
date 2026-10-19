# Claude command implementation matrix

Source: [Claude Code commands](https://code.claude.com/docs/en/commands), checked on 2026-10-13. Covers all 119 rows in the source's “All commands” table, including bundled skills, workflows, and removed commands. Aliases that have their own source rows are retained; other aliases are not counted separately.

Implementation audit: current canonical Rust working tree at `fe1b79e` plus existing local changes. `yes` means a working local implementation of the core command exists, including model-prompt implementations; it does **not** mean complete Claude Code parity. Differences and missing options appear in the purpose column. `no` means absent, explicitly rejected, inspection-only where an action is required, or a different behavior under the same name. Command signatures are upstream syntax, not a promise that every argument is supported locally.

`implementedby` is `opus` for the original 70 `yes` rows and the 8 added in Opus pass 2, and `astra` for the 22 that Astra added. It is a tracking label, not independently verified authorship. Unimplemented entries are blank. This is a source-code audit, not an end-to-end runtime certification.

## Astra pass (GPT/Codex, 2026-10-13)

Astra (Codex session `01a1328c…`) was asked to implement the `no` rows. Between 05:41 and 05:54 IST it added 22 commands, mostly in `rust/crates/rusty-claude-cli/src/command_actions.rs`, plus `SlashCommand::LocalAction` in `commands/src/lib.rs`, vim mode in `input.rs`, docs in `MANUAL.md`/`ARCHITECTURE.md`, and a new changelog (now `rust/crates/rusty-claude-cli/CHANGELOG.md`). It then **hit its usage limit before updating this file or finishing verification**. Those 22 rows are now `yes`/`astra`. All changes remain uncommitted.

Opus reviewed the rows marked `opus-review`. In that column, *Pass* means it works as described; *Weak* means it runs, but the core upstream behavior is still missing, so the original audit bar would have scored it `no`. Rows that Opus implemented earlier are not re-reviewed (`—`). The remaining 27 `no` rows depend on Anthropic-hosted services (cloud sessions, Desktop, Chrome, billing, voice, workflows) or were not attempted.

Opus follow-up fixes (2026-10-13):
1. `/release-notes` embedded `../../../../CHANGELOG.md`, a file outside the crate. The changelog moved into the crate, so `include_str!("../CHANGELOG.md")` also works for `cargo package`. Like `command_actions.rs`, it is untracked and must be committed with the change.
2. Fixed the stale `/mcp` summary assertion, plus the `/mcp` unknown-action test that the new hint affects.
3. Bare `/sandbox` shows status again; changing it needs `on`, `off` or `toggle`.
4. `/help` hints now list the new subcommands for `/mcp`, `/memory`, `/permissions` and `/sandbox`. The `/mcp` unknown-action error names them too.
5. `/permissions list` prints readable allow / ask / deny / denied-tools lists instead of a `{:?}` dump (new test).
Docs: the MANUAL rows for `/sandbox` and `/release-notes` are corrected. The stale entry calling `/release-notes` a git-history summary is removed. Vim mode's Esc behaviour is now documented.

Verification after the fixes:
- `cargo test -p commands`: 44 passed.
- `cargo test -p rusty-claude-cli --bin claw`: 274 passed. One run hung once; the rerun passed in 0.5 s.
- `cargo test -p runtime config_validate`: 24 passed.
- Clippy: no warnings in the changed files. `scripts/fmt.sh --check`: clean.
- Not re-run: the `rusty-claude-cli` integration tests.

Count: **100 yes, 19 no** (78 opus, 22 astra).

## Opus pass 2 (2026-10-13, later)

Eight `no` rows are now implemented, each marked `opus`:
- `/auto-mode-setup`, `/fast`, `/login`, `/logout` and `/schedule` live in `command_actions.rs`.
- `/ultrareview` and `/autofix-pr` are prompt-backed, also in `command_actions.rs`.
- `/ultraplan` now runs a planning turn instead of printing a static report.

`/login` and `/logout` used to be rejected in the parser as a removed auth flow. `claw login` from the shell now points to the REPL command. Supporting features: `/loop` without an interval is self-paced through the new ScheduleWakeup tool, and `/loop stop` ends it. The tool counterparts are in [`claude-tool-list.md`](claude-tool-list.md).

The 19 rows still `no` need Anthropic-hosted services or a claude.ai account (`/chrome`, `/claude-in-chrome`, `/desktop`, `/design-login`, `/design-sync`, `/install-slack-app`, `/mobile`, `/passes`, `/radio`, `/remote-control`, `/remote-env`, `/stickers`, `/teleport`, `/voice`, `/web-setup`, `/workflows`, `/workflow-authoring`), or a new provider (`/setup-bedrock`, `/setup-vertex`: claw has no Bedrock or Vertex provider). Link-only versions of `/mobile`, `/stickers`, `/passes` and `/install-slack-app` were skipped on purpose.

Verification for this pass: `cargo test -p commands` 44 passed (help and parser tests updated for the 7 new specs and `/login`); `cargo test -p rusty-claude-cli --bin claw` 285 passed, including new tests for cron-line parsing, crontab entries, shell quoting, the auto-mode presets, and the review and autofix prompts. Not exercised end to end: `/schedule add` (it edits the real crontab) and `/login` (interactive wizard).

|Command|purpose|Implemented-yes/no|implementedby|opus-review|
|---|---|---|---|---|
|`/add-dir <path>`|Allow file tools to access another directory during this session.|yes|opus|—|
|`/advisor [model\|off]`|Consult another model; Claw adds guidance to use a subagent, rather than a dedicated advisor tool.|yes|opus|—|
|`/agents`|Inspect or create agent configurations; Claw retains management commands.|yes|opus|—|
|`/artifact-capabilities`|Prompts the model with local-artifact guidance (`.claw/artifacts`, `/artifacts`); no hosted artifact runtime.|yes|astra|Pass. Prompt-only; explicitly disclaims hosting and sharing.|
|`/artifact-diagramming`|Prompts the model to produce accessible, self-contained SVG diagrams in `.claw/artifacts`.|yes|astra|Pass. Prompt-only.|
|`/artifacts`|`list`, `open <file>`, `attach <file>` for local files in `.claw/artifacts`; no hosted artifacts or sharing.|yes|astra|Pass. Path traversal and symlink escape rejected (tested); attach limited to 1 MiB UTF-8. `list` shows top-level files only.|
|`/auto-mode-setup`|`careful`, `balanced` or `autonomous` presets set `permissions.defaultMode`, `bashApproval` and allow/deny rules in `.claw/settings.local.json`, applied live. Bare shows the presets and current rules. Claude Code's auto mode uses a hosted classifier; claw uses rules.|yes|opus|—|
|`/autocompact [auto\|<tokens>]`|Inspect, change, or reset the automatic compaction threshold; saves the setting.|yes|opus|—|
|`/autofix-pr [prompt]`|Prompt-backed: uses `gh` to find the PR's failing checks, reads `--log-failed`, fixes locally, reruns the commands, and commits. It asks before pushing. Local instead of a cloud session.|yes|opus|—|
|`/background [prompt]`|Continue the conversation in a background Claw process; starts a fresh foreground session.|yes|opus|—|
|`/batch <instruction>`|Prompt the model to plan parallel changes and dispatch background agents in separate worktrees.|yes|opus|—|
|`/branch [name]`|Copy the conversation and switch into the copy, preserving the original session.|yes|opus|—|
|`/btw [question]`|Answer a side question without persisting it; show the previous side answer when no question is supplied.|yes|opus|—|
|`/bug [report]`|Create a bug report with session context; Claw writes a local bundle and does not submit it.|yes|opus|—|
|`/cd <path>`|Change the working directory and reload instructions while preserving the conversation.|yes|opus|—|
|`/chrome`|Configure Chrome integration.|no||Not attempted by Astra.|
|`/claude-api [migrate\|upgrade\|managed-agents-onboard\|prompt-audit\|cost-optimize\|build-eval\|hillclimb\|preserved-thinking-migration]`|Prompts the model to inspect the project SDK and use official Anthropic docs; upstream subcommands are passed through as free text, not parsed.|yes|astra|Pass (prompt). Named workflows such as `migrate` and `build-eval` have no dedicated handling.|
|`/claude-in-chrome [task]`|Perform browser tasks through Chrome.|no||Not attempted by Astra.|
|`/clear [name]`|Start an empty local conversation; Claw accepts `--confirm`, not the upstream session-name argument.|yes|opus|—|
|`/code-review [low\|medium\|high\|xhigh\|max\|ultra] [--fix] [--comment] [--max-findings n\|all\|default] [pr#\|branch\|path]`|Review repository changes through a model prompt; aliases `/review`. Structured effort, fix, comment, and cloud-review options are not implemented.|yes|opus|—|
|`/color [color\|default]`|Change the session input color; supports named colors, random selection, and reset.|yes|opus|—|
|`/compact [instructions]`|Compact conversation history; the parser does not accept optional focus instructions.|yes|opus|—|
|`/config [key=value ...]`|`key=value` edits for 9 whitelisted keys, saved to `.claw/settings.local.json` and applied live; `/config edit` opens `$VISUAL`/`$EDITOR`; `--help` lists keys. No interactive settings UI.|yes|astra|Pass, with issues. Validates every pair before writing, and writes atomically with mode 0600 (tested). Values cannot contain spaces. If `/config edit` leaves invalid JSON, the error appears only after the file is saved.|
|`/context [all]`|Report estimated context consumption; Claw prints text metrics, without the upstream grid or `all` option.|yes|opus|—|
|`/copy [N]`|Copy assistant output; Claw supports the latest response or full transcript, not numbered responses or a block picker.|yes|opus|—|
|`/cost`|Report cumulative session tokens; Claw has a separate handler, not an alias to the full `/usage` report.|yes|opus|—|
|`/dataviz [request]`|Send chart-selection, color, labeling, and accessibility guidance to the model; no bundled palette-validation script.|yes|opus|—|
|`/debug [description]`|Prompt the model to investigate Claw configuration, transcripts, and provider issues; does not enable runtime debug logging.|yes|opus|—|
|`/deep-research <question>`|Prompt web research and a cited report; implemented as a normal model turn, without the bundled dynamic workflow engine.|yes|opus|—|
|`/design [brief]`|Prompts the model to build a self-contained HTML/SVG design in `.claw/artifacts`; requires a brief. No hosted canvas.|yes|astra|Pass. Prompt-only; local equivalent.|
|`/design-login`|Authenticate design-system access.|no||Not attempted by Astra.|
|`/design-sync [hint]`|Upload a React design system.|no||Not attempted by Astra.|
|`/desktop`|Transfer sessions to Desktop.|no||Not attempted by Astra.|
|`/diff`|Display the workspace Git diff.|yes|opus|—|
|`/doctor [prompt-audit [path]]`|Run setup and environment health checks; no `prompt-audit` subcommand or guided repair skill.|yes|opus|—|
|`/effort [level\|auto\|status\|ultracode [on\|off]]`|Change reasoning effort; supports low, medium, high, and reset, without the newer upstream levels or ultracode controls.|yes|opus|—|
|`/exit`|Leave the interactive CLI; `/quit` is also recognized.|yes|opus|—|
|`/export [filename]`|Write the current conversation to a file; no upstream clipboard/save dialog.|yes|opus|—|
|`/fast [on\|off]`|Low reasoning effort, plus a switch to the `fastModel` setting's model when one is set; `off` restores both. A local stand-in for Claude Code's faster serving of the same model.|yes|opus|—|
|`/feedback [report]`|Write a local feedback bundle containing the report and session; no remote submission or drafts queue.|yes|opus|—|
|`/fewer-permission-prompts`|Prompt transcript analysis and proposed allow rules; writing changes requires confirmation in the generated prompt.|yes|opus|—|
|`/focus`|Toggle and persist a compact full-screen view of prompts, tool calls, and replies.|yes|opus|—|
|`/fork [prompt]`|Save a conversation copy; with a prompt, request a background subagent. Does not create the upstream standalone background session.|yes|opus|—|
|`/goal [condition\|clear]`|Run successive turns toward a condition; supports status and cancellation, with a bounded number of rounds.|yes|opus|—|
|`/heapdump`|Writes Rust process memory metrics (`/proc/self/status`), message count and estimated tokens to `~/.claw/diagnostics/*.json`. No JS heap exists.|yes|astra|Pass (equivalent). Process metrics are Linux-only; elsewhere the field is null. Contains no conversation text.|
|`/help`|Display available slash commands and usage.|yes|opus|—|
|`/hooks`|List configured tool lifecycle hooks and invalid entries; the report covers pre-tool, post-tool, and tool-failure hooks.|yes|opus|—|
|`/ide`|`status` prints terminal and workspace; `open [code\|cursor\|zed]` launches the editor on the workspace. No IDE connection (diffs, selection, diagnostics).|yes|astra|Weak. Only an editor launcher; the integration that is the core of `/ide` is missing. Under the original audit bar this would be `no`.|
|`/import [codex\|gemini\|cursor] [--dry-run] [--yes]`|Prompt migration of other agents' configuration to Claw; `--dry-run` is passed to the model, and there is no native importer or `--yes` handling.|yes|opus|—|
|`/init`|Create a starter `CLAUDE.md` and project configuration.|yes|opus|—|
|`/insights`|Prompt analysis of local usage and write an HTML report.|yes|opus|—|
|`/install-github-app`|Checks for a github.com `origin` remote and opens the Claude GitHub App install page; does not create workflows or secrets.|yes|astra|Weak. Upstream also sets up the Actions workflow and API secret; that is left to the user.|
|`/install-slack-app`|Install the Slack integration.|no||Not attempted by Astra.|
|`/keybindings`|Create or display Claw's keyboard configuration file; no editor-launch interface.|yes|opus|—|
|`/list-agents`|List background Claw sessions and jobs; does not provide the upstream directory of messageable peers.|yes|opus|—|
|`/login`|Runs the setup wizard: provider, API key and model, saved to `~/.claw/settings.json`. No claude.ai account sign-in.|yes|opus|—|
|`/logout`|Removes the saved provider, key and model from `~/.claw/settings.json`; says that keys in environment variables still apply.|yes|opus|—|
|`/loop [interval] [prompt]`|Repeat a supplied prompt at an interval until interrupted; no prompt-free maintenance mode or adaptive pacing.|yes|opus|—|
|`/mcp [reconnect (<server>\|all)\|enable\|disable [<server>\|all]]`|Adds `enable\|disable [server\|all]` (persisted in `mcpServerStates`) and `reconnect`, which rebuilds the runtime and rediscovers tools; list/show unchanged. No MCP OAuth.|yes|astra|Pass. Disabled servers are filtered at startup, and a more specific scope can re-enable a server (tested). Fixed by Opus: the `/help` hint, unknown-action error and stale tests are updated.|
|`/memory`|`list`, `on\|off` (persists `autoMemoryEnabled`), `edit [project\|user\|auto]` in `$EDITOR`, then reloads instructions.|yes|astra|Pass. `CLAW_AUTO_MEMORY=0` still overrides. `edit project` targets `./CLAUDE.md` in the cwd, not the git root. Fixed by Opus: the `/help` hint lists the subcommands.|
|`/mobile`|Show mobile download codes.|no||Not attempted by Astra.|
|`/model [model]`|Switch the current runtime's model; no interactive picker or saving a new default through this handler.|yes|opus|—|
|`/output-style [style]`|List and select built-in or custom response styles, saving the selection.|yes|opus|—|
|`/passes`|Share subscription trial passes.|no||Not attempted by Astra.|
|`/permissions`|Adds `list` and `add\|remove allow\|ask\|deny <rule>` on project-local settings, applied live; mode switching unchanged. No interactive rule editor.|yes|astra|Pass. Additive scopes are handled honestly: `remove` refuses inherited rules. Fixed by Opus: `list` printed a `{:?}` dump and now prints readable per-bucket lists (tested); the `/help` hint lists the rule subcommands.|
|`/plan [description]`|Enter read-only planning mode, optionally starting a task; restore the previous permission mode when leaving.|yes|opus|—|
|`/plugin [subcommand]`|Manage local plugins, including installation, enablement, removal, and updates.|yes|opus|—|
|`/plugin-authoring`|Prompts the model to read the local plugin loader and schema, then build and validate a plugin.|yes|astra|Pass. Prompt-only.|
|`/powerup`|Display a static tour of Claw features; no interactive lessons or animated demos.|yes|opus|—|
|`/pr-comments [PR]`|Prompt retrieval and summarization of PR comments through `gh`; retained locally although removed upstream.|yes|opus|—|
|`/privacy-settings`|Opens the claude.ai privacy settings page, or prints the URL.|yes|astra|Pass (browser redirect). Only relevant to claude.ai accounts; Claw itself uses API keys.|
|`/radio`|Open the music stream.|no||Not attempted by Astra.|
|`/rate-limit-options`|Prints recovery choices; `usage` and `upgrade` open the matching claude.ai pages.|yes|astra|Weak. Informational only; it neither resumes the request nor switches models or plans.|
|`/recap`|Generate a short session recap without retaining the recap exchange.|yes|opus|—|
|`/release-notes`|Prints the Claw changelog (`rust/crates/rusty-claude-cli/CHANGELOG.md`), embedded at build time; no model call.|yes|astra|Pass. Fixed by Opus: the changelog now lives in the crate, so the build no longer reads a file outside it. It must be committed with the change.|
|`/reload-plugins [--force]`|Rebuild the runtime to reload plugins, MCP servers, and instructions; no `--force` option.|yes|opus|—|
|`/reload-skills`|Re-scan and list available skills; no added/removed comparison report.|yes|opus|—|
|`/remote-control`|Expose sessions to remote clients.|no||Not attempted by Astra.|
|`/remote-env`|Select cloud execution environments.|no||Not attempted by Astra.|
|`/rename [name]`|Set a session title; without a name, Claw shows the existing title rather than generating one.|yes|opus|—|
|`/resume [session]`|Load a saved local session; an explicit session reference is required, without the upstream picker.|yes|opus|—|
|`/review [low\|medium\|high\|xhigh\|max\|ultra] [--fix] [--comment] [--max-findings n\|all\|default] [pr#\|branch\|path]`|Run the local review prompt; `/code-review` is an alias. Advanced upstream review flags are not parsed.|yes|opus|—|
|`/rewind`|List checkpoints or undo the last N requests, restoring files and conversation where possible; no selected-message summarization.|yes|opus|—|
|`/run`|Prompt the model to build, launch, and exercise the application with available tools.|yes|opus|—|
|`/run-skill-generator`|Prompt creation of a project skill describing verified build, launch, exercise, and cleanup steps.|yes|opus|—|
|`/sandbox`|Bare `/sandbox` or `status` reports; `on`, `off` or `toggle` persist `sandbox.enabled` in local settings and apply live. `--resume` shows status only.|yes|astra|Pass. Fixed by Opus: bare `/sandbox` toggled silently; it now only shows status.|
|`/schedule [description]`|`list`, `add <cron> <prompt>`, `remove <id>`, `run <id>`: tagged entries in the user's crontab run `claw -p` in the directory with workspace-write permissions, logging to `~/.claw/schedules/<id>.log`. Runs on this machine, not claude.ai. In-session scheduling is the CronCreate tool.|yes|opus|—|
|`/scroll-speed`|Inspect or persist the number of rows scrolled per wheel step.|yes|opus|—|
|`/security-review`|Prompt a security review; defaults to pending changes or the latest commit, not comparison with origin's default branch.|yes|opus|—|
|`/setup-bedrock`|Configure Bedrock access interactively.|no||Not attempted by Astra.|
|`/setup-vertex`|Configure Vertex access interactively.|no||Not attempted by Astra.|
|`/simplify [target]`|Prompt cleanup of changed code and verification; does not enforce the upstream parallel review structure.|yes|opus|—|
|`/skill-doctor`|Report estimated skill token sizes and usage counts from local transcripts.|yes|opus|—|
|`/skills`|List and inspect skills; Claw also supports installing, removing, and invoking them, without the upstream visibility picker.|yes|opus|—|
|`/slides [brief]`|Prompts the model to create a PPTX (if a writer is available) or a navigable, printable HTML deck in `.claw/artifacts`.|yes|astra|Pass. Prompt-only; requires a brief.|
|`/stats`|Display local session statistics; a separate handler, without the upstream usage dashboard tabs.|yes|opus|—|
|`/status`|Report current session, model, configuration, and environment status.|yes|opus|—|
|`/statusline`|Prompt creation of a status-line script and setting; the runtime executes it with session data before prompts.|yes|opus|—|
|`/stickers`|Order branded stickers.|no||Not attempted by Astra.|
|`/stop`|Stop a background Claw session or job by ID, preserving the saved session.|yes|opus|—|
|`/subtask <task>`|Prompt dispatch through the background Agent tool; context is supplied in the prompt rather than inherited automatically.|yes|opus|—|
|`/tasks`|List, inspect, or stop background jobs; includes the `/bashes` alias, without the full upstream agent-work dashboard.|yes|opus|—|
|`/team-onboarding`|Prompt generation of a project onboarding guide from local sessions and configuration.|yes|opus|—|
|`/teleport`|Resume a cloud session locally; Claw's same-named command instead searches the workspace for files or symbols.|no||Not attempted by Astra.|
|`/terminal-setup`|Installs a Shift+Enter (ESC CR) binding into VS Code/Cursor `keybindings.json`, keeping a backup; other terminals return an error with manual instructions.|yes|astra|Pass, partial. Idempotent; refuses JSONC and conflicting bindings without touching the file (tested). Only VS Code-family terminals are automated.|
|`/theme`|Select and persist a supported terminal color theme; upstream theme choices are not all present.|yes|opus|—|
|`/tui [default\|fullscreen]`|Switch between inline and full-screen rendering while preserving the conversation and saving the preference.|yes|opus|—|
|`/ultraplan <prompt>`|A read-only planning turn: parallel explorer sub-agents, claims checked, plan written to `.claw/plans/<slug>.md` (approach, steps, risks, verification, rollback), then the agent asks before carrying it out. Upstream removed the cloud version.|yes|opus|—|
|`/ultrareview [PR or branch]`|Prompt-backed multi-agent review: four background sub-agents (correctness, security, performance, tests) over the diff; findings are verified, then reported with the ReportFindings tool. Local sub-agents instead of cloud review agents.|yes|opus|—|
|`/update-config [request]`|Prompt a settings change in the appropriate Claw configuration file while preserving other keys.|yes|opus|—|
|`/upgrade`|Opens the claude.ai upgrade page, or prints the URL.|yes|astra|Pass (browser redirect).|
|`/usage`|Show session tokens, API calls, and estimated cost; no Anthropic subscription-limit integration.|yes|opus|—|
|`/usage-credits`|Opens the claude.ai usage settings page, or prints the URL.|yes|astra|Pass (browser redirect).|
|`/verify`|Prompt building and directly exercising changed behavior, with observed results.|yes|opus|—|
|`/vim`|Toggles and persists a basic vim mode (`editorMode`): Esc enters normal; `i a I A`, `h j k l`, `w b 0 $`, `x`; Enter submits. No counts, operators, registers or undo.|yes|astra|Pass, minimal. While on, Esc no longer clears the input box (now documented). Unicode and multiline motions are tested.|
|`/voice [hold\|tap\|off]`|Control speech dictation.|no||Not attempted by Astra.|
|`/web-setup`|Connect GitHub for cloud sessions.|no||Not attempted by Astra.|
|`/workflow-authoring`|Load dynamic-workflow authoring guidance.|no||Not attempted by Astra.|
|`/workflows`|Manage dynamic-workflow progress.|no||Not attempted by Astra.|

Audit evidence: [command registry and parser](rust/crates/commands/src/lib.rs), [REPL dispatch and core handlers](rust/crates/rusty-claude-cli/src/main.rs), [checkpoint, reporting, clipboard, and prompt handlers](rust/crates/rusty-claude-cli/src/repl_commands.rs), and [Claude-compatible command handlers](rust/crates/rusty-claude-cli/src/claude_commands.rs). Status is based on executable dispatch and handler behavior, not merely command names in help or category tables.
