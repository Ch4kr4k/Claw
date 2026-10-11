# Claw Code Manual

How to use `claw` day to day: the interactive session, keys, permissions,
slash commands, undo, settings and troubleshooting. For scripting, JSON output
and the full flag reference, see [`USAGE.md`](./USAGE.md). For how it works
inside, see [`ARCHITECTURE.md`](./ARCHITECTURE.md).

## 1. Install

```bash
git clone https://github.com/ultraworkers/claw-code
cd claw-code
scripts/install-local.sh          # release build, installs claw to ~/.local/bin
scripts/install-local.sh --all    # also installs claw-analog
claw doctor                       # first health check
```

`PREFIX_BIN=/some/dir scripts/install-local.sh` installs somewhere else.
Without the script: `cd rust && cargo build --release`, then use
`rust/target/release/claw`. Do **not** use `cargo install claw-code`, which
installs an unrelated deprecated stub.

## 2. Connect a model

Pick one:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."                       # Anthropic
export OPENAI_API_KEY="sk-..."                              # OpenAI (or OPENAI_BASE_URL for a gateway)
export XAI_API_KEY="..."                                    # xAI
export OLLAMA_HOST="http://127.0.0.1:11434"                 # local Ollama
```

Or run `claw setup` (or `/setup` inside the REPL) for the interactive provider
wizard: Anthropic, xAI, OpenAI, DashScope or any OpenAI-compatible endpoint.
It saves to `~/.claw/settings.json` with owner-only permissions; environment
variables you set yourself take precedence over the saved key. Choose a model with `--model` or `/model`:

```bash
claw --model sonnet
claw --model qwen3:latest        # use the exact tag your local server exposes
```

To use Claude, OpenAI, OpenRouter or other cloud providers alongside Ollama,
and to switch back, see [`PROVIDERS.md`](./PROVIDERS.md).

Local servers (llama.cpp, vLLM, LM Studio) are covered in
[`docs/local-openai-compatible-providers.md`](./docs/local-openai-compatible-providers.md).

## 3. Start a session

```bash
claw                               # interactive REPL in the current directory
claw "explain src/main.rs"         # one-shot prompt, then exit
claw --resume latest               # continue the last session here
claw -C ../other-project           # run against another directory
```

The REPL shows a banner and the connected model, then a `>` prompt under a
dim rule. Replies stream in as rendered markdown; each tool call shows as one
line (`[bash] cargo test`, `[read] src/lib.rs`) followed by its output, and
edits show as a coloured diff. Replies and tool output keep a two-column
margin on both sides and wrap inside it; wrapped lines keep their indent,
list-bullet indent and the `|` gutter of tool output.

### Full-screen view

Like Claude Code, the interactive REPL draws the whole terminal window: the
conversation fills the top and the input box and status line stay at the
bottom, even while you scroll back. The bottom area grows for a long prompt,
the `/` menu, an approval or a question, and shrinks back afterwards.

| Key | Action |
|---|---|
| Mouse wheel | Scroll the conversation (3 rows a step) |
| PgUp / PgDn | Scroll a page |
| Ctrl+Home | Jump to the start of the conversation |
| Ctrl+End | Jump back to the end (a *Jump to bottom* hint shows while you are scrolled up) |
| Click and drag | Select conversation text; it is copied to the clipboard when you let go (*Copied N characters* shows briefly). Dragging past the top or bottom edge scrolls. A click clears the selection |

While you are scrolled up, new output does not move the view. Selected text
is copied with `wl-copy`, `xclip`, `xsel` or `pbcopy`, or else through the
terminal (OSC 52). Selection covers the conversation, not the input box; for
the terminal's own selection hold **Shift** while dragging. Resizing the window rewraps the conversation. When you leave
claw, the conversation is printed into the terminal's history, so it is still
there to scroll through. Set `CLAW_FULLSCREEN=0` for the previous inline view
(output in the terminal's own history, input box under the last line).

### Your own keys

`~/.claw/keybindings.json` binds keys in the input box, ahead of the
built-in ones (`/keybindings` creates it and lists the actions):

```json
{ "bindings": {
    "ctrl+o": "/focus",
    "ctrl+g": "submit",
    "ctrl+t": "text:TODO: ",
    "ctrl+l": "none"
} }
```

A key (`ctrl+x`, `alt+x`, `shift+up`, `f5`, `pageup`, `enter`, `tab`, `esc`)
maps to an editor action (`submit`, `newline`, `clear-input`, `cancel`,
`history-prev`, `history-next`, `history-search`, `line-start`, `line-end`,
`word-left`, `word-right`, `delete-word`, `delete-to-start`, `delete-to-end`,
`clear-screen`, `cycle-mode`, `complete`), a slash command (what you typed
comes back at the next prompt), `text:` to insert, or `none` to switch a
built-in key off. Changes apply at the next prompt.

## 4. Typing at the prompt

| Key | Action |
|---|---|
| Enter | Send |
| Shift+Enter, Alt+Enter, Ctrl+J, or a trailing `\` | New line |
| Up / Down | Previous / next history entry (in multi-line input, moves between lines first) |
| Tab | Complete a `/command` or an `@path` |
| `/` | Opens the command menu; type to filter, Up/Down to pick, Tab to complete, Enter to run, Esc to hide |
| Esc | Hide the menu; with no menu open, clear the input |
| Ctrl+C | Clear the input. On an empty input: shows *Press Ctrl-C again to exit*; a second press within 2 s exits |
| Ctrl+D | Delete the character under the cursor. On an empty input: press twice to exit |
| Ctrl+L | Clear the screen |
| Ctrl+A / Ctrl+E, Home / End | Start / end of line |
| Ctrl+W, Ctrl/Alt+Backspace | Delete the previous word |
| Ctrl+U / Ctrl+K | Delete to start / end of line |
| Ctrl/Alt+← / → | Move by word |

The input is highlighted as you type (`code`, **bold**, headings, fences,
`/commands`, `@paths`). Pasted text keeps its newlines and never submits on
its own; a large paste collapses to `[Pasted text #1 +40 lines]` and is
expanded when sent. History is saved to `~/.claw/history.jsonl`.

Mention files with `@path` (Tab completes them) so the agent reads them. See
[`docs/navigation-file-context.md`](./docs/navigation-file-context.md).

## 5. While a reply is running

| Key | Action |
|---|---|
| Esc | Stop the reply |
| Ctrl+C | Stop the reply (same as Esc; the program keeps running) |
| Typing + Enter | Queue a message. It is passed to the agent before its next step, shown as `> text (sent to the agent)` |
| Typing without Enter | Kept and pre-filled at the next prompt |
| Shift+Tab | Pick the next permission mode. The status line shows it with *(after this reply)*; it takes effect when the reply ends |

At the bottom of the window, a block shows what is happening, what you are
typing and the status line:

```text
✻ Pondering… (1m 12s · ↓ 2.3k tokens · writing · esc to interrupt)
────────────────────────────────────────────────
❯  type to queue a message for the agent
────────────────────────────────────────────────
  ⏵⏵ auto mode on (shift+tab to cycle) · local/qwen3.6 · ctx 6.3k/16k (39%)
```

| Activity | Meaning |
|---|---|
| **Thinking…** | Waiting for the model, or it is reasoning |
| **Writing…** | Reply text is arriving |
| **Working…** | The model went quiet mid-reply. After 20 s it adds *silent 45s · the model may be writing a file*: local models (Ollama) send a tool call only once it is complete, so a long file arrives all at once |
| **Preparing write_file…** | The model is streaming a tool call (providers that stream them) |
| **Running bash · cargo test…** | A tool is running |

The time counts the whole request; tokens are an estimate of what the model
has sent. The input row shows what you are typing and how many messages are
queued. When you stop a reply, queued messages go back into the input box
instead of running. A `/btw` question typed during a reply is not passed to
the running task; it is answered on its own as soon as the reply ends.

With `CLAW_FULLSCREEN=0` (inline view), the block follows the reply down
the screen and stays at the bottom once the screen is full; the reply scrolls
into the terminal's history above it, and a line still being written is
shown at the top of the block until it ends.

### When the agent asks you something

When a decision is yours to make, the agent can ask with a picker instead of
guessing. Its recommendation comes first, marked *(Recommended)*, and the
cursor starts on it:

```text
  [Database] Which database should this project use?
  ❯ 1. PostgreSQL (Recommended)
       Robust indexing, ACID, advanced queries
    2. SQLite
       Single file, zero setup
    3. Type something else…
  ↑↓ move · Enter select · 1-9 jump · Esc cancel
```

| Key | Action |
|---|---|
| ↑ / ↓ (or j / k) | Move |
| Enter | Choose the highlighted option |
| 1–9 | Choose that option directly |
| Space | Tick or untick a checkbox (when several answers are allowed, shown as `[ ]`/`[x]`) |
| Last row, *Type something else…* | Write your own answer, then Enter (Esc goes back) |
| Esc | Don't answer; the agent picks a sensible default and says which |

After you answer, the picker is replaced by a one-line `✓ your choice`. In
scripts and other non-interactive runs nobody can answer, so the agent is told
to choose its recommendation and continue.

Ctrl+C never kills the REPL. It stops the running reply, clears the input,
or is ignored. To leave, press Ctrl+C twice at an empty prompt, press Ctrl+D
twice, or run `/exit`.

## 6. Permissions

Every tool needs a permission level; the session runs in one mode:

| Mode | Allows without asking |
|---|---|
| `read-only` | Reading files, searching, read-only shell commands |
| `workspace-write` (default) | The above plus editing files inside the current directory and ordinary shell commands |
| `danger-full-access` | Everything |

Switch with `--permission-mode MODE` or `/permissions MODE`. When a call needs
more than the current mode, you are asked:

```text
  Allow? [y]es / [a]lways for `sudo emerge` this session / [N]o:
```

- **y**: allow this one call.
- **a**: allow it for the rest of the session. For bash, this covers the
  first two words of the risky command, so approving `sudo emerge` does not
  approve `sudo rm`.
- **N** (or Enter): refuse. The model is told and continues.

You are always asked, even in `workspace-write`, for:

- bash commands that run as root (`sudo`, `su`, `doas`, `pkexec`, `run0`);
- destructive commands: `rm`, `dd`, `mkfs*`, `kill`, `shutdown`, `git reset
  --hard`, `git push --force`, `git clean -f`, recursive `chmod`/`chown`,
  `emerge --depclean`, and the like. Every part of `a && b`, `a | b` and
  `$(…)` is checked;
- writing files outside the current directory (use `/add-dir PATH` to allow
  another directory).

To be asked only about privileged or destructive bash commands, set:

```json
{ "permissions": { "bashApproval": "risky" } }
```

Rules in settings can pre-approve or block tools, e.g.
`"permissions": { "allow": ["bash(cargo test*)"], "deny": ["bash(curl*)"] }`.
`deny`/`ask` rules match a command anywhere in a chain; `allow` rules only
match the whole command.

## 7. Slash commands

Type `/` to browse; `/help` lists them all. The most useful:

**Session**

| Command | What it does |
|---|---|
| `/help` | List commands |
| `/status` | Model, permissions, workspace, loaded memory files |
| `/model [name]` | Show or switch the model |
| `/permissions [mode]` | Show or switch the permission mode |
| `/effort [low\|medium\|high\|default]` | Reasoning effort for models that support it |
| `/clear [--confirm]` | Start a fresh session |
| `/compact` | Summarize old history to free context |
| `/btw [question]` | Quick side question, answered from the conversation without tools; neither question nor answer is kept in the conversation or the saved session. Without a question, shows the last one |
| `/recap` | One-line summary of the session (not kept in the conversation) |
| `/goal [condition\|clear]` | Keep working across turns until the condition holds (up to 20 rounds; Esc pauses). No argument shows the goal |
| `/loop [interval] <prompt>` (`/proactive`) | Run a prompt now and again every interval (`30s`, `5m`, `2h`; default 10m) until you press a key |
| `/autocompact [auto\|<tokens>]` | Show or set the context size that triggers compaction (`150k`, `1m`); saved |
| `/branch [name]` | Fork the conversation here and switch to the fork; `/resume <id>` returns to the original |
| `/cd <path>` | Move the session to another directory; instructions and memory are reloaded |
| `/subtask <task>` | Hand a side task to a background subagent that reports back |
| `/fork [prompt]` | With a prompt, a background subagent works on it with this conversation's context; without one, saves a forked copy you can `/resume` |
| `/advisor [model\|off]` | Let the model consult a second model (through a sub-agent) at key moments |
| `/bug [report]` (`/share`), `/feedback [report]` | Save a report with your setup and this session to `~/.claw/feedback-bundles/` for you to send |
| `/batch <instruction>` | Plan a large change as independent units, then run them as background subagents in git worktrees |
| `/debug [problem]` | Troubleshoot claw itself from its settings, `claw doctor` and this session |
| `/import [codex\|gemini\|cursor] [--dry-run]` | Bring instructions, MCP servers and commands over from other agents |
| `/insights` | HTML report on how you use claw, from your sessions |
| `/team-onboarding` | Onboarding guide for a teammate from this project's sessions |
| `/dataviz [request]` | Chart and dashboard design guidance |
| `/run-skill-generator` | Write a project skill that teaches `/run` and `/verify` to start the app |
| `/context [show\|clear]` | Estimated tokens versus the model's context window |
| `/usage`, `/cost`, `/stats` | Token usage, estimated cost, cache hit rate |
| `/resume <session>`, `/session` | Load, list, fork or delete saved sessions |
| `/rename <name>` | Name the session |
| `/history [count]` | Summary of recent turns |
| `/export [file]` | Save the conversation |
| `/exit` | Quit |

Claude Code's names work too: `/reset` and `/new` (`/clear`), `/quit`
(`/exit`), `/settings` (`/config`), `/continue` (`/resume`), `/checkpoint`
(`/rewind`), `/allowed-tools` (`/permissions`), `/bashes` (`/tasks`),
`/checkup` (`/doctor`), `/code-review` (`/review`). `/plan <task>` turns plan
mode on and starts planning that task.

**Files and changes**

| Command | What it does |
|---|---|
| `/undo` | Revert the files changed by the last request |
| `/rewind [n]` | List checkpoints, or undo the last *n* requests |
| `/diff` | Git diff of current changes |
| `/files` | Files read, written or edited in this session |
| `/add-dir <path>` | Let file tools work in another directory without asking |
| `/commit`, `/pr`, `/issue` | Draft a commit, pull request or issue |

**Input helpers**

| Command | What it does |
|---|---|
| `/retry` | Resend the last failed (or last) prompt |
| `/paste` | Send the clipboard as a prompt |
| `/image [path]` | Attach an image file, or the clipboard image, to the next message |
| `/copy [last\|all]` | Copy the last reply or the whole transcript |

**Modes and reviews**

| Command | What it does |
|---|---|
| `/plan [on\|off]` | Plan mode: read-only tools, the model replies with a plan. `/plan off` restores your previous permissions |
| `/review [scope]`, `/security-review [scope]`, `/bughunter [scope]` | Ask the model for a review |
| `/simplify [focus]` | Have the model clean up the changed code (reuse, simpler logic, style) and apply it |
| `/pr-comments [pr]` | Summarize a GitHub PR's comments with `gh` (default: the current branch's PR) |
| `/todos` | Show the model's current todo list |
| `/run [what]`, `/verify [what]` | Have the model launch the app and drive it / confirm a change works by running it, not only tests |
| `/deep-research <question>` | Web research across sources with a cited report |
| `/update-config <request>` | Describe a settings change; the model edits the right settings file |
| `/fewer-permission-prompts` | Find often-approved read-only commands in past sessions and propose allow rules |
| `/output-style [style]` | List or switch output styles: `default`, `explanatory`, `learning`, `concise`, or your own in `.claude/output-styles/` / `.claw/output-styles/` (Claude Code's format); saved |
| `/tui [fullscreen\|default]` | Switch between the full-screen and inline views now; saved |
| `/scroll-speed [rows]` | Rows one mouse-wheel step scrolls (1-20); saved |
| `/color [color\|default]` | Input box color for this session: red, blue, green (default), yellow, purple, orange, pink, cyan; no argument picks one |
| `/statusline [description]` | Have the model write a status line script; claw runs the `statusLine.command` setting before each prompt with session JSON on stdin and shows its first line under the input box |
| `/terminal-setup` | Check whether Shift+Enter works in this terminal, with setup steps for ones that need it |
| `/reload-skills`, `/reload-plugins` | Re-scan skills; reload plugins, MCP servers and instructions |
| `/skill-doctor` | Each skill's size and how often it was used here |
| `/powerup` | A quick tour of claw features |
| `/keybindings` | Create or show `~/.claw/keybindings.json` (see below) |
| `/focus [on\|off]` | Focus view (full-screen only): your last prompt, one line per tool call with ✓/✗ and +/− lines for edits, and the final reply. The full conversation is kept; `/focus` again shows it. Saved |
| `/background [prompt]` (`/bg`) | Move this conversation to a background claw process that keeps working (on the prompt, or else on the current task) and start fresh here. It cannot ask for permission, so actions that would ask are denied. A notice shows when it finishes |
| `/list-agents` (`/peers`) | Background sessions with their status, plus background shells |
| `/stop [id]` | Stop a background session (or background shell); without an id, the only running one |
| `/summary`, `/release-notes` | Summaries from the conversation or git history |
| `/ultraplan [task]` | Deep multi-step planning |

**Environment**

| Command | What it does |
|---|---|
| `/tasks [list\|get <id>\|stop <id>]` | Background jobs started by the agent |
| `/theme [name]` | `dark`, `light`, `solarized-dark`, `solarized-light`, `tokyo-night`, `none`; saved for next time |
| `/config`, `/memory`, `/hooks`, `/mcp`, `/agents`, `/skills`, `/plugin` | Inspect configuration and extensions |
| `/doctor`, `/sandbox`, `/version`, `/providers`, `/setup` | Diagnostics and provider setup |
| `/init` | Create a starter `CLAUDE.md` for the repository |

## 8. Undo and checkpoints

Each request that changes files is recorded as a before/after snapshot in a
private git repository under `~/.claw/checkpoints/`. It never touches your
project's own `.git`. This covers edits made by any tool, including shell
commands. Checkpoints survive restarts.

```text
/rewind        # list checkpoints for this session
/undo          # revert the last request's file changes
/rewind 3      # revert the last three requests
```

Snapshots are skipped in your home directory, at `/`, in very large folders,
when git is not installed, or with `CLAW_SNAPSHOTS=0`.

## 9. Background jobs

The agent can start long-running commands (dev servers, watchers, long
builds) in the background and keep working. You manage them with:

```text
/tasks            # list jobs and their state
/tasks get 2      # show job 2's output
/tasks stop 2     # stop it
```

When a job finishes on its own, the agent is told on your next message. All
jobs are stopped when you exit.

## 10. Documents and images

The file tools read `.docx`, `.pptx`, `.xlsx`/`.xls`/`.ods`, `.pdf` and
`.ipynb` as text, so you can ask about them directly. The agent can also
create Word documents (from markdown) and PowerPoint decks, and edit Excel
workbooks in place.

Attach screenshots or photos with `/image path.png` or `/image` (clipboard).
PNG, JPEG, GIF and WebP are supported; the model must accept images.

## 11. Instructions and memory

`claw` adds these files to the system prompt:

- `~/.claw/CLAUDE.md` and `~/.claw/AGENTS.md`: your personal instructions for
  every project;
- Claude Code's personal files, `~/.claude/CLAUDE.md` and `~/.claude/rules/`
  (`$CLAUDE_CONFIG_DIR` if set), so what you wrote for Claude Code applies here;
- in the project, up to the git root: `CLAUDE.md`, `CLAW.md`, `AGENTS.md`,
  `.claw/CLAUDE.md`, `.claude/CLAUDE.md`, `.claw/instructions.md`;
- rule folders `.claw/rules/` (shared), `.claw/rules.local/` (personal) and
  Claude Code's `.claude/rules/`;
- rules from other tools (Cursor, Copilot, Windsurf, …), unless
  `"rulesImport": "none"`.

Like in Claude Code, a file can pull in others with `@path` (`See
@docs/style.md`, `@~/notes.md`), relative to the file, up to 5 levels deep;
`@` inside code is ignored. A repository set up for Claude Code (CLAUDE.md,
CLAUDE.local.md, `.claude/rules/`, imports) works in claw unchanged.

`/memory` shows what was loaded. `/init` writes a starter `CLAUDE.md`.

### Auto memory

claw also keeps notes for itself, so it remembers across sessions what it
learned: your preferences, corrections you gave it, project decisions, where
things are. Each project has a folder `projects/<project path>/memory/` with
one markdown file per memory and an index, `MEMORY.md`. It is Claude Code's
folder and format: when `~/.claude` exists the folder is
`~/.claude/projects/<project path>/memory/`, so claw and Claude Code share
what they learn; otherwise it is under `~/.claw`. The index is loaded into every session; the model
opens a memory file when it is relevant.

The model saves a memory when you correct it or state a preference, when it
learns something about the project that will matter later, or when you say
"remember …". Say "forget …" to have it delete one. It is told not to save
what the code or git history already shows, or secrets. Writing in the memory
folder never asks for permission.

`/memory` lists the saved memories and the folder; edit or delete the files to
change them. Turn it off with `"autoMemoryEnabled": false` in settings or
`CLAW_AUTO_MEMORY=0`.

## 12. Settings

Settings merge in this order, later files winning:

1. `~/.claw.json`
2. `~/.config/claw/settings.json`
3. `<repo>/.claw.json`
4. `<repo>/.claw/settings.json`
5. `<repo>/.claw/settings.local.json` (personal, not committed)

Some user-level settings (theme, provider) are saved by the REPL to
`~/.claw/settings.json`. Example project file:

```json
{
  "model": "sonnet",
  "permissions": {
    "defaultMode": "workspace-write",
    "bashApproval": "risky",
    "allow": ["bash(cargo test*)"],
    "deny": ["bash(curl*)"]
  },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "scripts/audit.sh" }] }
    ]
  }
}
```

Hooks, MCP servers and plugins are described in [`USAGE.md`](./USAGE.md).

### Environment variables

| Variable | Effect |
|---|---|
| `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL` | Anthropic credentials and endpoint |
| `OPENAI_API_KEY`, `OPENAI_BASE_URL` | OpenAI or any OpenAI-compatible endpoint |
| `XAI_API_KEY`, `OLLAMA_HOST` | xAI; local Ollama |
| `CLAW_CONFIG_HOME` | Use another directory instead of `~/.claw` |
| `CLAW_API_CONNECT_TIMEOUT` | Seconds to wait for a connection (default 30) |
| `CLAW_API_REQUEST_TIMEOUT` | Seconds to wait for the next piece of a reply before giving up (default 300; 1800 for local servers such as Ollama). Long replies that keep streaming are never cut off |
| `CLAW_TOOL_RESULT_MAX_CHARS` | Cap on what one tool call sends to the model (useful for small local models) |
| `CLAW_SNAPSHOTS=0` | Turn off checkpoints |
| `CLAW_THEME` | Theme, overriding the saved one |
| `CLAW_FULLSCREEN=0` | Inline view instead of the full-screen window (output stays in the terminal's history) |
| `CLAW_AUTO_MEMORY=0` | Turn off auto memory for this run |
| `CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS` | Context size (input tokens of the last request) at which the session is compacted automatically; default 100000 |
| `NO_COLOR` | No colours |
| `CLAW_OUTPUT_FORMAT=json` | Default to JSON output for non-interactive commands |
| `CLAW_LOG`, `RUST_LOG` | Logging |
| `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` | Proxies |

## 13. Troubleshooting

Start with `claw doctor` (or `/doctor`). Common problems:

**`http error: error decoding response body: … operation timed out`**
The server sent nothing for `CLAW_API_REQUEST_TIMEOUT` seconds. With local
models this usually happens while the model writes a large file: Ollama sends
a tool call only once it is complete, so a slow GPU generating a long
`write_file` is silent until the whole file is written (at ~8 tokens/s, a
10k-character file takes over 5 minutes). Local servers get a 30-minute wait
by default; raise it further with `export CLAW_API_REQUEST_TIMEOUT=3600`, ask
for the file in parts, or `/compact` to shrink the prompt. Then `/retry`.

**`http error: error decoding response body: … connection closed` (or reset)**
The server dropped the connection mid-reply: it crashed, ran out of memory, or
a proxy timed out. Check the server's logs, then `/retry`.

**`Auto-compacting session … No further compaction possible`**
The request still failed after the history was compacted, so the history is
not the problem: either the system prompt plus tools alone exceed the model's
context window (use a model or server with a larger context, e.g. a bigger
`--ctx-size` on llama.cpp), or the error isn't really about context. Read the
error printed after it.

**Local model replies are very slow**
Check whether it's the model or `claw` by timing the model directly:
`ollama run <model> --verbose "hello"` prints its tokens per second. On a
4 GB GPU a 4B model runs at about 10 tokens/s, and reading claw's ~4k-token
system prompt takes about 30 s before the first word; claw adds little on top.
To speed things up, ask for shorter answers, use a smaller model or context so
it fits in GPU memory, or limit the tools sent with `--allowedTools`. A
verbose fine-tune can write thousands of tokens for a one-line request.
Ollama models default to a small context; create a variant with
`PARAMETER num_ctx 16384` in a Modelfile (`ollama create name -f Modelfile`)
so claw's prompt fits.

**Context-window errors with local models**
Set `CLAW_TOOL_RESULT_MAX_CHARS` (e.g. `20000`) so one large file or command
output can't fill the window, and use `/context` to watch usage.

**A command keeps asking for approval**
Answer `a` to allow it for the session, add an `allow` rule, or set
`"bashApproval": "risky"`.

**The terminal looks wrong after a crash**
Run `reset` (or `stty sane`).
