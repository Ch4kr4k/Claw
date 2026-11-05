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

With the launcher setup (a `~/.local/bin/claw` script that runs
`~/.local/lib/claw/claw` against a local llama.cpp server), use
`scripts/install-claw.sh`: it builds and installs the binary to
`~/.local/lib/claw/` and installs the launchers kept in `scripts/wrappers/`
(`claw`, `llama-gpu` and its `llama-gpu.service` user unit) through
`scripts/install-wrappers.sh`, backing up any that differ.

`PREFIX_BIN=/some/dir scripts/install-local.sh` installs somewhere else. If
`~/.local/bin/claw` is a launcher script (one that `exec`s the real binary),
the script is left alone and the binary goes to `~/.local/lib/claw/claw`.
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

With the launcher setup, `llama-gpu` runs llama.cpp in router mode: every model
in `~/.config/llama-gpu/models.ini` (repo copy: `scripts/wrappers/llama-models.ini`)
is available, and `/model local/<section>` (e.g. `/model local/cybertiel-35b-a3b`,
or an alias of your own from `"aliases"` in `~/.claw/settings.json`) switches to
it mid-session. The server loads the model on the first request and unloads the
previous one, so the first reply after a switch waits for the load. `/model`
also switches to API models (`/model sonnet`, `/model gpt-4.1`) as long as that
provider's key is exported; the history carries over. `/model` on its own
opens a searchable list of every model you can switch to, and the model you
switch to becomes the default for the next start.

`llama-gpu` runs `llama-server` from the async-prefetch llama.cpp fork
(`~/ws/src/cpp/nn/llama-abominated`, about 6–8% faster prompt processing).
`llama-gpu-switch stock` goes back to the stock build (`~/ws/src/cpp/nn/llama`)
with the same models and settings, `llama-gpu-switch fork` returns to the fork,
and `llama-gpu-switch status` shows which one is running.

## 3. Start a session

```bash
claw                               # interactive REPL in the current directory
claw "explain src/main.rs"         # one-shot prompt, then exit
claw --resume latest               # continue the last session here
claw -C ../other-project           # run against another directory
```

`claw --resume [latest|<session-id>]` reopens that session in the REPL (with
slash commands after it, as in `claw --resume latest /status`, it runs them
and exits instead). On `/exit`, claw prints the `claw --resume <id>` line for
the session you just left.

The REPL shows a banner and the connected model, then a `>` prompt under a
dim rule. Replies stream in as rendered markdown; each tool call shows as one
line (`[bash] cargo test`, `[read] src/lib.rs`) followed by its output, and
edits show as a coloured diff. Replies and tool output keep a two-column
margin on both sides and wrap inside it; wrapped lines keep their indent,
list-bullet indent and the `|` gutter of tool output.

### Desktop app

`claw desktop` (or `claw-desktop`, or **Claw** in rofi and other app
launchers) opens a chat window that works like Claude's desktop app. Each chat
runs `claw desktop-agent` in its working folder, so settings, permissions,
tools, models and sessions are the same as in the terminal.

- **Look:** Solarized dark (or light, in Settings). Replies are Markdown with
  syntax-highlighted code blocks (language label and **Copy**), long lines
  wrap inside the block, and tool output is colored (diff lines, errors,
  exit codes). Images in replies are shown: web URLs and local paths. The
  model is told it can show pictures with `![...](url)` and WebFetch lists a
  page's image URLs, so "find a picture of X" works. **Save** under an image
  puts it in `~/Downloads` and then shows where (**Show** opens the folder).
- **Selecting text:** formatted replies can't be selected with the mouse
  directly (the toolkit has no text selection). **Copy** under a reply or
  under your own message copies all of it. **Double-click** a message, or
  **Select text** under it, to show it as plain text you can select with the
  mouse and copy with Ctrl+C; **Done** goes back.
- **Edit a message:** **Edit** under one of your messages puts it back in the
  message box (a ✎ line says so; **Cancel** drops the edit). Sending it
  replaces that message and every reply after it, in the app and in the
  saved session, and claw answers the new version.
- **Attach:** **Attach** adds `[Attached file: path]` lines to the message.
  Attached images (PNG, JPEG, GIF, WebP) are also sent to the model as
  pictures, so a vision model can see them; other files are read with the
  tools.
- **Chats and terminal sessions:** the sidebar has two tabs. **Chats** are the
  ones started in the app (blue edge), **Terminal sessions** the ones started
  with `claw` in a terminal (green edge). Click one to read and continue it.
  **Find terminal sessions** searches your home folder for older ones; folders
  where claw starts a session are remembered in `~/.claw/workspaces.txt`.
- **Delete:** the ✕ next to a chat deletes it after a confirmation. **Select**
  shows checkboxes to delete several at once (**All** selects the whole tab).
- **Incognito:** **Incognito** (Ctrl+Shift+N) starts a chat that saves
  nothing: no session file, and it is gone when the app closes.
- **Sidebar:** ☰ in the header (or Ctrl+B) hides and shows it.
- **Rename:** ✎ next to any chat in the sidebar (app chats and terminal
  sessions, open or not), or the title in the header (✎), new chats
  included. Enter or ✓ saves, Esc or ✕ cancels. A name given before the
  first message is kept instead of the first message becoming the title.
- **Delete** also removes the chat's undo checkpoints.
- **Working folder:** new chats start in `~/ws/tmp` (when it exists; change it
  in Settings > General). The folder button in a new chat's header picks
  another. A saved chat keeps the folder it started in.
- **Projects:** **+ New** under Projects creates a project: a name, a folder
  (created if missing) and optional instructions, saved to
  `<folder>/.claw/instructions.md` so claw follows them in that folder from the
  terminal too. Chats in the folder are listed under the project; **+** starts
  one, ⚙ edits the project. Removing a project keeps its folder and chats.
- **Model:** the picker in the header switches models for the rest of the
  chat. It lists the models your local llama.cpp server is serving right now
  (`local/<name>`), your aliases with the model each points to
  (`coder → local/cybertiel-35b-a3b`; aliases to local models the server no
  longer has are left out), and `sonnet`/`opus`/`haiku` only when an
  Anthropic key or `claw login` is set up.
- **Approvals:** commands that need approval show a card with **Allow once**,
  **Always allow** (for the rest of the chat) and **Deny**; questions from the
  model show their options as buttons.
- **Hints:** hovering a button shows what it does in a bubble; the keyboard
  shortcuts are listed as keycaps in Settings > General > Keyboard and under
  the message box.
- **Background work:** while a chat has background shells or monitors
  running, a "⚙ Running in the background" bar above the message box lists
  them, with **Stop** for each and **Stop all**. When one finishes or reports,
  claw continues on its own (a note says so). Closing the chat or quitting
  ends them.
- **Composer:** Enter sends, Shift+Enter adds a line; **Stop** ends the reply
  and any command it is running (with everything that command started),
  **Retry** under the last reply sends the last message again, **Attach**
  adds file paths to the message.
- **Quit:** Ctrl+Q ends every chat's agent and closes the app. Closing the
  window does the same.
- **Settings** (bottom of the sidebar, or Ctrl+,):
  - *General:* default folder, theme, text size, notifications when a reply
    finishes or claw needs approval while the window is in the background
    (`notify-send`), thinking shown expanded.
  - *Skills:* install from a folder, a `SKILL.md`/`.md` file, a `.zip`/`.skill`
    archive or a git URL into `~/.claw/skills`; open or remove installed ones.
  - *MCP servers:* add a local server (a command line plus `KEY=value`
    environment lines) or a remote one (an `https://` URL plus `Name: value`
    header lines), or remove one. Saved in `~/.claw/settings.json`
    (`mcpServers`), so the terminal uses them too; new chats connect to them.
  - *Tools:* every built-in tool claw has, each with a switch. Switched-off
    tools go to `permissions.deniedTools`, which claw refuses and no longer
    offers to the model.
  - *Scheduled tasks:* a name, a cron schedule (`0 9 * * *`, `*/30 * * * *`,
    `@daily`, ...), a folder and a prompt. The app runs it in a new chat at
    those times while it is open, and notifies you; **Run now** runs it at
    once.

Commands run without a terminal, so prompts such as `emerge --ask` cannot be
answered; claw tells the model to use non-interactive flags, and adds a hint
to the output when a command fails that way.

The app keeps its projects, chat titles, preferences and scheduled tasks in
`~/.claw/desktop.json`; agent errors go to `~/.claw/desktop-agent.log`.
`claw desktop-agent --incognito` is the mode incognito chats use.

### Full-screen view

Like Claude Code, the interactive REPL draws the whole terminal window: the
conversation fills the top and the input box and status line stay at the
bottom, even while you scroll back. The status line shows the permission
mode, the model, context used of the window, the session (last 8
characters; the full id is in `/status`) and running shells; what does not
fit the width is left off from the right. The bottom area grows for a long prompt,
the `/` menu, an approval or a question, and shrinks back afterwards.

| Key | Action |
|---|---|
| Mouse wheel | Scroll the conversation (3 rows a step) |
| PgUp / PgDn | Scroll a page |
| Ctrl+Home | Jump to the start of the conversation |
| Ctrl+End | Jump back to the end (a *Jump to bottom* hint shows while you are scrolled up; clicking it does the same) |
| Click and drag | Select conversation text; it is copied to the clipboard when you let go (*Copied N characters* shows briefly). Dragging past the top or bottom edge scrolls. A click clears the selection |
| Click on a cut-short tool call, result or thinking line | Expand it in place: the whole command or file content the agent wrote, the whole output, the whole reasoning. Click it again to collapse it |
| Ctrl+O | Expand every cut-short item; press again to collapse them all (also works while a reply is running) |
| Click **N shells** in the status line | Watch a background job's output live (see [Background jobs](#9-background-jobs)) |

Tool calls and results longer than a few lines, and the agent's thinking,
are shown cut short with *(ctrl+o to expand)*; the full text stays in memory
for this session's window. Without full-screen mode (`CLAW_FULLSCREEN=0`,
`/tui default`) they stay cut short.

While you are scrolled up, new output does not move the view. Selected text
is copied with `wl-copy`, `xclip`, `xsel` or `pbcopy`, or else through the
terminal (OSC 52). Selection covers the conversation, not the input box; for
the terminal's own selection hold **Shift** while dragging. Resizing the window rewraps the conversation. When you leave
claw, the conversation is printed into the terminal's history, so it is still
there to scroll through. Set `CLAW_FULLSCREEN=0` for the previous inline view
(output in the terminal's own history, input box under the last line).

### Diffs and the diff panel

When the agent edits a file, the edit shows under the tool line the way
Claude Code shows it: `Added 3 lines, removed 1 line`, then the changed lines
with their line numbers, removed lines on red and added lines on green across
the row, syntax-coloured, with a few unchanged lines around them. Long lines
wrap with their `+`/`-` marker; a long diff ends with `… +N more lines`.

In the full-screen view, a **diff panel** on the right shows what is
uncommitted in the working tree (against `HEAD`, new files included; claw's
own `.claw/` files left out): `N files changed +A -R`, one row per file with
its counts, then each file's diff. It updates after every edit and every turn.

| | |
|---|---|
| Mouse wheel over the panel | Scroll the panel (the conversation stays where it is) |
| Click its ✕ | Close it for this session (`/diffpanel` opens it again) |
| Window narrower than 110 columns | A box above the input box says so instead; its ✕ dismisses it |
| `/diffpanel [on\|off]` | Show or hide it; saved (`"diffPanel": "off"` in `~/.claw/settings.json`) |
| `CLAW_DIFF_PANEL=0` | Turn it off for one run |

The panel takes two fifths of the window. It appears only when something has
changed.

### Your own keys

`~/.claw/keybindings.json` binds keys in the input box, ahead of the
built-in ones (`/keybindings` creates it and lists the actions):

```json
{ "bindings": {
    "f2": "/focus",
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
| Esc | Stop the reply, and the shell command it is running (that command's process group gets SIGTERM, then SIGKILL after 3 s). Background jobs keep running; stop them with `KillShell`/`TaskStop` or `/tasks` |
| Ctrl+C | Stop the reply (same as Esc; the program keeps running) |
| Typing + Enter | Queue a message. It is passed to the agent before its next step (after the tools it is running, or right after its reply, which then goes on in the same turn), shown as `> text (sent to the agent)` |
| Typing + Ctrl+Enter | Send it now: queue the message (with any already queued) and cut off the reply being written, so the agent reads it at once and goes on in the same turn (`(reply cut off to send your message)`). A tool that is running finishes first. Needs a terminal with the kitty keyboard protocol (kitty, Ghostty, WezTerm, foot); elsewhere Ctrl+Enter is a plain Enter |
| `/` | Opens the same command menu as at the prompt, under the input row: ↑/↓ select, Tab completes, Enter runs the selected command, Esc closes the menu (the reply keeps running) |
| `/command` + Enter | Runs a slash command. Commands that only show something or change the view (`/help`, `/todos`, `/tasks`, `/diff`, `/config`, `/agents`, `/skills`, `/hooks`, `/focus`, `/diffpanel`, `/theme`, `/color`, `/stop`, `/keybindings`, `/version`, `/sandbox`) run at once; the others (`/model`, `/compact`, `/cost`, …) show `(runs when the reply ends)` and run then. Slash commands are never passed to the agent |
| Typing without Enter | Kept and pre-filled at the next prompt |
| Ctrl+O | Expand or collapse every cut-short tool call, result and thinking block (full-screen mode) |
| ↑ (Ctrl+P) | Edit queued messages: every message the agent has not received yet comes back into the input row (one per line, shown as `↵`), in front of what you are typing. Enter queues the edited text again, as one message. Messages already passed to the agent stay sent |
| ←/→, Home/End (Ctrl+A/E), Backspace, Delete (Ctrl+D), Ctrl+U/K | Move the cursor and edit what you are typing, as at the prompt. Down and Ctrl+←/→ do nothing here |
| Shift+Tab | Pick the next permission mode. At the prompt the status line changes in place (your typing stays) and the mode applies to what you send next; during a reply it shows *(after this reply)* and takes effect when the reply ends |

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
queued (`[2 queued · ↑ to edit]`). When you stop a reply, queued messages go back into the input box
instead of running. A `/btw` question typed during a reply is not passed to
the running task: it is answered at once, beside it, from the conversation as
last sent to the model (no tools), and the answer is printed as soon as it
arrives (the input row shows `answering /btw…` meanwhile). A `local/` model
served with `--parallel 1` (llama-gpu's default) answers it only once the
running step is done, so claw waits up to 15 minutes for it there (3 for
hosted models); set `auxiliaryModel` to answer side questions elsewhere at
once. claw desktop works the
same way: a message sent while the agent works is queued for its next step, and
`/btw <question>` is answered without starting a turn.

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

Switch with `--permission-mode MODE` or `/permissions MODE`. `--plan` (or
`--dry-run`) starts in `read-only` as a dry run: the model is told to
investigate with read-only tools and reply with the exact commands and file
edits it would make, the risks and how to verify, without attempting them.
`/plan` does the same inside a session. When a call needs
more than the current mode, you are asked:

```text
  Allow? [y]es / [a]lways for `sudo emerge` this session / [N]o:
```

- **y**: allow this one call.
- **a**: allow it for the rest of the session. For bash, this covers the
  first two words of the risky command, so approving `sudo emerge` does not
  approve `sudo rm`, and `rm -rf` stays separate from `rm`. Targets are not
  part of it: approving `kill 1234` covers every `kill`, `pkill llama-server`
  every `pkill` (likewise `killall`, `shutdown`, `reboot`, `poweroff`,
  `halt`), and a second word that is a number or a path is ignored. The
  desktop app's **Always allow** works the same way.
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

Two values make bash approvals apply in every mode, `danger-full-access` and
`--dangerously-skip-permissions` included:

- `"bashApproval": "sudo-and-destructive"`: commands that run as root
  (`sudo`, `su`, `doas`, `pkexec`, `run0`) and destructive commands ask.
- `"bashApproval": "destructive"`: only destructive commands ask. Running as
  root alone does not, but a destructive command behind `sudo`, `doas`,
  `pkexec` or `su -c` still does (`sudo rm`, `sudo mkfs`, `sudo emerge
  --depclean`).

Combined with `"defaultMode": "danger-full-access"` and
`"sandbox": { "enabled": false }`, everything else (editing files anywhere,
building, installing into your home) runs without asking. With the sandbox
off, a request to run a command outside it needs no separate approval.

Rules in settings can pre-approve or block tools, e.g.
`"permissions": { "allow": ["bash(cargo test:*)"], "deny": ["bash(curl:*)"] }`.
`bash(cmd)` matches exactly that command; a trailing `:*` (`bash(cmd:*)`)
matches any command starting with `cmd`. A bare `*` is not a wildcard:
`bash(cargo test*)` matches only the literal text `cargo test*`.
`deny`/`ask` rules match a command anywhere in a chain. An `allow` rule must
match the whole command, and with a chain every risky command in it
(root, destructive) must also be matched by an allow rule of its own:
`bash(shutdown:*)` approves `shutdown -h +5` (and `sync && shutdown now` in
full-access mode), but `shutdown now; rm -rf ~` still asks.

### SSH key passphrases

Commands the agent runs have no terminal, so `ssh` (and `git push`/`pull`
over ssh) cannot prompt on it. In the REPL, claw sets `SSH_ASKPASS` for those
commands to a helper that asks you instead: a `🔑 Enter passphrase for key …`
line appears, typing shows as `•`, Enter sends and Esc cancels. The answer
goes straight to `ssh`; the model never sees it. Host-key `yes/no` questions
use the same line, with the answer visible. If you set `SSH_ASKPASS`
yourself, or a running `ssh-agent` already holds the key, claw leaves ssh
alone. You do not need to give the agent your passphrase.

## 7. Slash commands

Type `/` to browse; `/help` lists them all. The most useful:

**Settings and local integrations**

These commands run inside the live REPL. Existing `--resume` report commands
remain read-only; the new editing operations require a live session.

| Command | What it does |
|---|---|
| `/config key=value ...` (`/settings`) | Save validated settings to `.claw/settings.local.json` and apply them to this session. `/config --help` lists keys; `/config edit` opens the file in `$VISUAL` or `$EDITOR` |
| `/permissions list` | Show effective permission rules |
| `/permissions add allow\|ask\|deny <rule>` | Add a project-local rule; for example `/permissions add allow Bash(git status)` |
| `/permissions remove allow\|ask\|deny <rule>` | Remove a project-local rule. Rules inherited from other files must be edited in those files; rules across scopes are additive |
| `/memory [list\|on\|off]` | Inspect memories or persist the project's automatic-memory preference. `CLAW_AUTO_MEMORY=0` still takes precedence |
| `/memory edit [project\|user\|auto]` | Edit the project's `CLAUDE.md`, the user's Claw `CLAUDE.md`, or the automatic-memory index, then reload instructions |
| `/sandbox [status\|on\|off\|toggle]` | Bare `/sandbox` (or `status`) only reports; `on`, `off` or `toggle` change the project's sandbox request. The status report distinguishes requested protection from protection supported and active on this machine |
| `/mcp enable\|disable\|reconnect [server\|all]` | Update persistent server enablement or reconnect. Refresh rebuilds all runtime connections and tool discovery; existing transport limitations remain, including unsupported remote OAuth transports |
| `/release-notes` | Display the Claw changelog (`rust/crates/rusty-claude-cli/CHANGELOG.md`) bundled into the binary; does not spend model tokens |
| `/terminal-setup` | Install Shift+Enter in VS Code/Cursor terminal keybindings, preserving unrelated entries and creating a backup. Existing files must be strict JSON; JSONC comments or conflicting terminal bindings produce an error without changing the file. Other terminal types still need manual setup |
| `/ide [status\|open [code\|cursor\|zed]]` | Show terminal/workspace information or launch an installed editor on this workspace; no IDE RPC connection is established |
| `/heapdump` | Save a private JSON report of process memory metrics and session counts under `~/.claw/diagnostics`. This Rust process has no JavaScript heap; the report contains neither conversation text nor credentials |
| `/vim [on\|off]` | Toggle and save basic Vim editing. Start in insert mode; Esc enters normal mode; `i/a/I/A` return to insert mode; `h/j/k/l`, `w/b`, `0/$`, and `x` move or delete. Enter submits. While Vim mode is on, Esc no longer clears the input box. Counts, operators, registers, and Vim undo are not implemented |

### Shell sandbox and host commands

On Linux, `sandbox.enabled: true` requires Bubblewrap (`bwrap`) with working
unprivileged user namespaces. Commands run with your user ID and no capabilities.
The host filesystem is mounted read-only; the current workspace is writable.
`filesystemMode: "allow-list"` additionally permits existing paths listed in
`allowedMounts` (relative paths resolve against the workspace). An empty allow-list
is rejected. `filesystemMode: "off"` explicitly permits host filesystem writes
subject to your normal user permissions; namespace isolation remains enabled.
A private `/tmp`, `/dev`, and PID namespace are always created. The private
`/dev` holds only basic devices (null, zero, random, tty, pts, shm), so hardware
such as `/dev/i2c-*`, `/dev/video*` or `/dev/ttyUSB*` is absent inside the
sandbox. When a sandboxed command names such a device, its result ends with a
`[sandbox]` note saying so, so the model does not conclude that the device is
missing on the machine and can ask to rerun the command on the host. Network access is
available unless `networkIsolation: true` is set. `namespaceRestrictions: false`
disables the extra IPC/UTS isolation, not the namespaces needed for enforcement.

As in Claude Code, the sandbox confines **writes**, not reads: commands
inherit claw's environment (API keys, `SSH_AUTH_SOCK`, `DISPLAY`) and can read
your home folder, so ssh, git over ssh, docker and GUI programs work as they
do in your shell. Two things are kept from running outside the sandbox later:
in the workspace, `.git/hooks`, `.git/config`, `.envrc`, `.claw.json` and
`.claw/settings*.json` are read-only, so a command cannot plant something that
runs on your next `git commit`, `git status`, `cd` or claw start. `git add`,
`commit`, `status`, `log` and `push` work; `git config` does not.
`write_file`/`edit_file` run in claw itself, so writing those paths (anything
under `.git`) asks first, in every mode below full access.

When a sandboxed command fails on a write the sandbox refuses (`Read-only file
system`, `Permission denied`, `Operation not permitted`), its result tells the
model so, and the model asks you to rerun it on the host (below). The same goes
for `sudo`, `su`, `doas` and `pkexec`, which cannot gain root in the sandbox:
there sudo reports root's files as owned by uid 65534 and a "no new
privileges" flag, which the model is told comes from the sandbox, not a broken
sudo.
`sandbox.excludedCommands` lists commands that always run on the host, for
example `["docker", "git config", "npm publish"]`: an entry matches a command
that starts with it as whole words, and only when every command in the line
matches (`docker ps` runs on the host, `docker ps; rm -rf ~` does not).
Ordinary Bash permissions still apply to them. Network access stays on unless
`networkIsolation: true`, so `localhost` services remain reachable. Don't
start claw in your home folder: the whole home folder then becomes the
writable workspace. The sandbox covers shell commands only, not other tools or
the Claw process.

`/sandbox` probes the requested setup, including network isolation and mount roots.
If it cannot be enforced, shell execution stops with an error. It never silently
runs the command on the host. Tool arguments may tighten protections but cannot
weaken the configured protections or change the configured mount list.

For necessary host administration, Bash can explicitly request
`dangerouslyDisableSandbox: true`. The approval prompt says **Host command
(outside sandbox)** and shows the entire command. Ordinary Bash approvals, allow
rules, and hook allow decisions do not approve this request. Session approval
covers only that exact host input and working directory. Without approval, the
host command cannot execute, including from unattended/sub-agent calls. This
exception leaves the global sandbox setting enabled. It does not escape a
sandbox imposed on the Claw process by another application, or grant sudo rights;
sudo still requires the host's usual authentication. There is no automatic host
retry when a sandboxed command fails.

Use `emerge -s lockscreen` for package searches; it does not need sudo. Shells run
noninteractively as `sh -c`, using the inherited environment; interactive Bash
aliases such as `em` are not loaded.

Supported `/config` keys are `model`, `theme`, `autoMemoryEnabled`,
`sandbox.enabled`, `sandbox.networkIsolation`, `permissions.defaultMode`, and
`permissions.allow/ask/deny`. Booleans use `true` or `false`; rule lists use JSON
arrays. Assignment values cannot contain whitespace; use `/config edit` for
complex values. Invalid assignments leave the existing file unchanged. Settings
edits preserve unrelated keys; files are replaced atomically with owner-only
permissions on Unix. An unrelated setting edit preserves an explicit session
model or permission mode. Changes can still be overridden by environment
variables. Editor commands require a terminal and temporarily leave the full-screen view.

**Local artifacts and reference prompts**

| Command | What it does |
|---|---|
| `/artifacts [list\|open <file>\|attach <file>]` | List files in `.claw/artifacts`, open one in its default application, or attach a UTF-8 file up to 1 MiB to the conversation. Paths and symlinks must remain inside the artifact directory |
| `/artifact-capabilities [request]` | Explain local artifact formats, opening, and attachment without claiming a hosted artifact API |
| `/artifact-diagramming [request]` | Guide the model in producing accessible SVG diagrams |
| `/design <brief>` | Ask the model to create a self-contained HTML/SVG design in `.claw/artifacts` |
| `/slides <brief>` | Ask the model to create a presentation with available document tools, or a navigable and printable HTML slide deck |
| `/claude-api [task]` | Ask the model to inspect the project's SDK and consult official Anthropic documentation before working on API code |
| `/plugin-authoring [task]` | Ask the model to inspect the actual Claw plugin schema and build and validate a plugin |

The reference and generation commands run a model turn and use its configured
tools and permissions. Designs and slides are local files, not uploads to Claude
Design or Claude Slides. Claw does not provide account sharing, cloud artifact
hosting, or the proprietary dynamic-workflow engine.

**Browser-assisted account commands**

`/upgrade`, `/usage-credits`, and `/privacy-settings` open the corresponding
Claude account page, or print its URL when no browser can launch. Sign-in and
account changes happen in that browser. `/install-github-app` requires a
`github.com` origin and opens the official App installation flow; it does not
create Actions workflows, write secrets, or install the app without browser
confirmation. `/rate-limit-options` lists recovery choices; `usage` and `upgrade`
open the corresponding pages. It does not automatically resume a rate-limited
request or change billing.

**Session**

| Command | What it does |
|---|---|
| `/help` | List commands |
| `/status` | Model, permissions, workspace, loaded memory files |
| `/model [name]` | Switch the model. Without a name it opens a list: the current model, your aliases, and the models of every provider whose key is set, plus the local server's when it is running. Each row shows the provider, context size and whether it's free. Type to filter (every word must match), ↑↓ and Enter to pick, Esc clears the filter, then cancels. Enter on text that matches nothing uses it as the model name. The list fits the terminal height, and the model lists load in the background when claw starts. A switch (from the list or `/model <name>`) is saved as `model` in `~/.claw/settings.json`, so claw starts with it next time |
| `/permissions [mode]` | Show or switch the permission mode |
| `/effort [low\|medium\|high\|default]` | Reasoning effort for models that support it |
| `/fast [on\|off]` | Quicker replies: low reasoning effort, and the `fastModel` setting's model if you set one; `off` restores both |
| `/clear [--confirm]` | Start a fresh session |
| `/compact` | Summarize old history to free context. Hosted models write the summary themselves (objective, requirements, decisions, work state, files, next steps); local models get the one built from the messages, which costs no request |
| `/fallback [model\|off]` | The model to switch to when the current one fails (rate limit, no credit, errors, silence, empty replies). Without an argument, a list like `/model`'s with `off` at the top; type to filter. Saved as `providerFallbacks.fallbacks`; a `local/` fallback starts its model server when needed. Works in claw desktop too |
| `/btw [question]` | Quick side question, answered from the conversation without tools; neither question nor answer is kept in the conversation or the saved session. Without a question, shows the last one |
| `/recap` | One-line summary of the session (not kept in the conversation) |
| `/goal [--fresh] [condition\|clear]` | Keep working across turns until the condition holds (up to 20 rounds; Esc pauses). The model can answer `GOAL BLOCKED: <reason>` for something only you can fix; blocked twice in a row, the goal pauses instead of using up its rounds. The goal is saved with the session, so it survives a restart or `/resume`. `--fresh` runs each round in a new session that starts with only the goal, the previous round's report and `.claw/goal-progress.md` (kept by the model): for long goals on a small local context window. Earlier rounds stay saved (`/resume`). No argument shows the goal |
| `/loop [interval] <prompt>` (`/proactive`) | With an interval (`30s`, `5m`, `2h`): run the prompt now and again every interval until you press a key. Without one: self-paced; after each run the agent picks when to run again (1 min to 1 h, with ScheduleWakeup) or ends the loop, and the next run starts while the prompt is idle. `/loop stop` cancels the pending run |
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
| `/context [show\|clear]` | Estimated tokens versus the model's context window, the auto-compaction threshold, and where they go: system prompt, tool definitions (as sent, after deferral), message text, tool calls, tool results, thinking, images |
| `/usage`, `/cost` | Token usage, estimated cost, cache hit rate |
| `/stats` | Token usage and cost, plus a speed table for the last turn: per model request, the context size and how much of it the server had cached, the wait for the first token, the generating time and output tokens/s, and the time the tools took; then session averages. A low cache share means the prompt is re-read every step; a long first-token wait means slow prompt reading |
| `/resume [session]`, `/session` | Load, list, fork or delete saved sessions. `/resume` alone lists this folder's sessions (newest first, by their first request, with age and size) to pick from; type to filter |
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
| `/undo [list\|<n>]` | Revert the files changed by the last request; `list` shows the checkpoints, `<n>` (or `--to <n>`) reverts the newest *n* |
| `/rewind [n]` | List checkpoints, or undo the last *n* requests |
| `/diff [--stat\|--name-only]` | Current changes. On a terminal: totals, one row per file, then highlighted hunks (new files included; plain `git diff` when piped or with `NO_COLOR`). `--stat`: lines added/removed per file; `--name-only`: just the changed and untracked files |
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
| `/todos` | Show the model's current todo list. Each change is also logged as a typed event (`task.created`, `task.started`, `task.completed`, `task.reopened`, `task.removed`, with the task and a timestamp) to `.clawd-todos.events.jsonl` beside `.clawd-todos.json`, for scripts and status tools; `claw init` gitignores both |
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
| `/diffpanel [on\|off]` | Diff panel (full-screen only): uncommitted changes right of the conversation; see [Diffs and the diff panel](#diffs-and-the-diff-panel). Saved |
| `/focus [on\|off]` | Focus view (full-screen only): your last prompt, one line per tool call with ✓/✗ and +/− lines for edits, and the final reply. The full conversation is kept; `/focus` again shows it. Saved |
| `/background [prompt]` (`/bg`) | Move this conversation to a background claw process that keeps working (on the prompt, or else on the current task) and start fresh here. It cannot ask for permission, so actions that would ask are denied. A notice shows when it finishes |
| `/list-agents` (`/peers`) | Background sessions with their status, plus background shells |
| `/stop [id]` | Stop a background session (or background shell); without an id, the only running one |
| `/summary` | Summary of the conversation |
| `/ultraplan <task>` | Plan before coding: parallel explorer sub-agents, then a plan in `.claw/plans/<task>.md` (approach, steps, risks, verification, rollback). Nothing else is changed; the agent asks before carrying it out |
| `/ultrareview [PR#\|branch\|path]` | Review with four parallel sub-agents (correctness, security, performance, tests); findings are checked and shown as a list (ReportFindings). Changes nothing |
| `/autofix-pr [PR#\|prompt]` | Fix a pull request's failing checks with `gh`: read the failed logs, fix, rerun locally, commit; asks before pushing |

**Environment**

| Command | What it does |
|---|---|
| `/learn [now\|undo\|pin <skill>\|unpin <skill>\|on\|off]` | What the learning loop saved (learned skills with their use, recent changes); `now` reviews the conversation at once, `undo` reverts the last review or curator pass, `pin` keeps a skill from being archived (see [Learning loop](#learning-loop)) |
| `/tasks [list\|view [id]\|get <id>\|stop <id>]` | Background jobs started by the agent; `view` watches one's output live |
| `/theme [name]` | `dark`, `light`, `solarized-dark`, `solarized-light`, `tokyo-night`, `none`; saved for next time |
| `/config`, `/memory`, `/hooks`, `/mcp`, `/agents`, `/skills`, `/plugin` | Inspect configuration and extensions |
| `/doctor`, `/sandbox`, `/version`, `/providers`, `/setup` | Diagnostics and provider setup |
| `/login`, `/logout` | Set up a provider, key and model (the setup wizard), or remove the saved ones from `~/.claw/settings.json`. Providers: Anthropic, OpenAI, Z.ai, OpenRouter, DeepSeek, Gemini, Groq, Mistral, NVIDIA, xAI, DashScope, your `customProviders`, and local or custom OpenAI-compatible servers; the model list is fetched from the provider. Pick with ↑↓ and Enter (1-9 jump; long lists scroll; "Type something else" for an unlisted model); Enter on an empty answer keeps the current value; Esc cancels at any step and saves nothing |
| `/auto-mode-setup [careful\|balanced\|autonomous]` | How much runs without asking. `careful`: every shell command asks. `balanced`: only risky ones ask, and common read-only and build/test commands are allowed. `autonomous`: full access, risky commands ask, destructive ones (force push, `rm -rf /`, `sudo`, …) are denied. Written to `.claw/settings.local.json` |
| `/schedule [list\|add <cron> <prompt>\|remove <id>\|run <id>]` | Routines on this machine: an entry in your crontab runs `claw -p "<prompt>"` in this directory on the schedule (`"0 9 * * 1-5"`, `@daily`), with workspace-write permissions, logging to `~/.claw/schedules/<id>.log`. `run` starts one now |
| `/init` | Create a starter `CLAUDE.md` for the repository |

## 8. Undo and checkpoints

Each request that changes files is recorded as a before/after snapshot in a
private git repository under `~/.claw/checkpoints/`. It never touches your
project's own `.git`. This covers edits made by any tool, including shell
commands. Checkpoints survive restarts.

```text
/rewind        # list checkpoints for this session (same as /undo list)
/undo          # revert the last request's file changes
/rewind 3      # revert the last three requests (same as /undo 3)
```

Undo restores the working tree's files only; your git index and commits
are left as they are.

Snapshots are skipped in your home directory, at `/`, in very large folders,
when git is not installed, or with `CLAW_SNAPSHOTS=0`.

## 9. Background jobs

The agent can start long-running commands (dev servers, watchers, long
builds) in the background and keep working. You manage them with:

```text
/tasks            # list jobs and their state
/tasks view bg2   # watch bg2's output live (no id: the running one)
/tasks get bg2    # print its output not read yet
/tasks stop bg2   # stop it
```

In full-screen mode, click **N shells** in the status line (shown while
jobs run), or the *Started background job bg2* line under the agent's Bash
call, to watch a job's output live in place of the conversation, like
Claude Code's background tasks. The bottom row shows whether it is still
running. ←/→ switch between jobs, PgUp/PgDn and the wheel scroll back, the
**stop** button ends it, and Esc or ✕ goes back to the conversation (Esc
closes the view first, even during a reply). Watching does not take the
output away from the agent's `BashOutput` reads.

When a job finishes on its own, the agent is told at once, even when the
prompt is idle. All jobs are stopped when you exit.

A plain shell command that leaves a program running with `&`
(`./server &`) no longer blocks the turn: claw returns once the shell
exits, waits a second for the rest of the output, and tells the agent the
program is still running and holds the output open.

The agent can also **monitor** a command (the Monitor tool): each batch of
new output lines (a log line, a test result) reaches it as soon as it
appears, and it can react while you are idle. Monitors stop after their
timeout (5 minutes by default, at most an hour), when the command exits, or
when they print more than 600 lines a minute. `/tasks stop <id>` stops one
early. Besides a command, a monitor can watch:

- a shell already running in the background (`task_id`, e.g. a build started
  with `run_in_background`): its new output arrives as notices, without
  restarting it or polling with BashOutput. When the watching stops (timeout
  or flood), the shell keeps running;
- a WebSocket (`websocket_url`, plain `ws://host:port/path`, with an optional
  first message such as a subscription): each message is an event. For
  `wss://`, the agent runs `websocat` as the command instead.

**Scheduled prompts.** The agent can schedule a prompt in this session with
a cron expression (the CronCreate tool, e.g. `*/10 * * * *`, local time),
once or recurring. A due prompt runs as a turn when the session is idle;
CronList and CronDelete manage them. They last as long as the session; for
schedules that run without claw open, use `/schedule`.

**Code intelligence.** The LSP tool talks to a real language server:
definitions, references, hover, symbols, call hierarchies and diagnostics.
It uses the first one installed of rust-analyzer, pyright (or basedpyright,
pylsp), typescript-language-server, gopls, clangd, jdtls,
lua-language-server, solargraph (or ruby-lsp) and zls. Add others by file
extension in settings:

```json
{ "lspServers": { "ex": { "command": "elixir-ls", "args": [] } } }
```

**Worktrees.** EnterWorktree creates `.claw/worktrees/<name>` on a new branch
`claw/<name>` and moves the session there; ExitWorktree returns, keeping or
removing it.


### Asynchronous engine (experimental)

`CLAW_ENGINE=infinity` runs a one-shot prompt (`claw -p "…"`,
`claw prompt …`) on the [Infinity](https://github.com/hydro-project/infinity)
runtime instead of the classic loop:

- **Nothing waits on a slow tool.** `bash`, web fetches and sub-agents run in
  the background, and the agent picks up their results as they arrive.
  Reads, searches and edits still answer at once.
- **Parallel calls.** Several tool calls in one reply all run; read-only
  ones run at the same time.
- **Threads.** The model can split work with `spawn_thread`. Each child
  runs concurrently, inherits the conversation so far (or starts empty with
  `fresh_context`), and reports back with `report_to_parent` or
  `close_thread`. Child output is shown on stderr as `[#1] …`.

```sh
CLAW_ENGINE=infinity claw --permission-mode workspace-write -p "review src/ in parallel, one thread per module"
```

The answer goes to stdout. Tool lines, child-thread output and a summary
(threads, completions, tokens) go to stderr. With `--output-format json`,
only a summary object is printed (`engine`, `message`, `threads`,
`completions`, `usage`).

Limitations for now:

- No permission prompts: any call your `--permission-mode` doesn't allow
  is denied, and the model is told why.
- The session isn't saved.
- Hooks, MCP tools and image results aren't used.
- The interactive REPL still uses the classic loop.

The design and the plan for the remaining work are in
[`docs/infinity-engine.md`](docs/infinity-engine.md).

## 10. Documents and images

The file tools read `.docx`, `.pptx`, `.xlsx`/`.xls`/`.ods`, `.pdf` and
`.ipynb` as text, so you can ask about them directly. The agent can also
create Word documents (from markdown) and PowerPoint decks, and edit Excel
workbooks in place.

The agent can look at images itself: `read_file` on a PNG, JPEG, GIF, WebP
or BMP (and the `vision_analyze` tool, which also takes a URL) attaches the
picture to the tool result, so "what does ~/Pictures/bug.png show?" works.

Attach screenshots or photos with `/image path.png` or `/image` (clipboard).
PNG, JPEG, GIF and WebP are supported; the model must accept images (the
local `cybertiel-35b-a3b` does, through its `mmproj` vision file).

In kitty and Ghostty, images in replies (`![alt](url)`: web URLs, local
paths, PNG/JPEG/GIF/WebP/SVG) are drawn in the terminal, in the full-screen
and the inline view, up to 80 columns by 24 rows. SVG is drawn on white. The
model is told it can show pictures this way. Elsewhere, and inside tmux, the
reply shows `[image:url]`. `CLAW_INLINE_IMAGES=0` turns this off, `=1` forces
it on.

Mermaid diagrams: a ```` ```mermaid ```` block in a reply (flowchart,
sequence, class, state, ER, gantt, pie and more) is drawn as a picture. In
the terminal the block's source is printed as usual and the diagram follows
once the block closes: as an image where inline images work (kitty, Ghostty),
as Unicode box-drawing text elsewhere. The desktop app shows the diagram in
place of the source (**Copy source** copies it), in the app's light or dark
theme; source that does not parse yet, as mid-stream, stays a code block.
Rendering is built in (no browser or `mmdc`). `CLAW_MERMAID=0` turns it off
in the terminal, `=text` always draws text.

The **Download** tool saves a file for you: ask "download that image" or
"save the PDF you found", and the agent fetches the URL (or copies a local
file) into `~/Downloads` (`$XDG_DOWNLOAD_DIR` when set), or wherever you say.
It never overwrites: a second `logo.png` becomes `logo (1).png`. Like
WebFetch, it asks for approval unless the permission mode allows it.

### Controlling the desktop (X11)

The **X11** tool lets the agent use your X11 desktop like a person at the
keyboard: list windows, launch programs and wait for their window, focus,
close, move or resize windows, type text, press keys (`ctrl+s`, `Return`,
`alt+Tab`), click, double-click, drag and scroll, read and set the
clipboard, and take screenshots. Screenshots go to `$TMPDIR/claw-x11/` and
are shown to the model, so a vision model (like the local
`cybertiel-35b-a3b`) can check what happened; only the newest two screenshots
stay in the conversation. It uses `xdotool`, `wmctrl`, `ffmpeg` and `xclip`,
which must be installed. Ask for things like "open firefox and search for
X", "close the calculator window", or "screenshot the terminal and tell me
the error".

The agent types and clicks into your real session, so don't use the
keyboard or mouse while it works. Like other risky tools it needs approval,
unless the permission mode is *bypass*. Remove `x11` from the launcher's
tool list (`~/.local/bin/claw`) or turn it off in the desktop app's Settings
to keep it out of the model's reach.

Besides screenshots, X11 has cheaper ways to see and act: `ocr` reads the
text of a window or region with its screen positions (needs `tesseract`:
`emerge app-text/tesseract`), and the `ui_*` actions use the accessibility
tree (AT-SPI) of GTK, Qt and Electron apps to find buttons and fields by
name and press or fill them without coordinates (kitty and some apps expose
none).

### Web pages (Browser)

The **Browser** tool drives a Chrome window through the DevTools protocol:
open pages, list numbered links, buttons and fields, click and type into
them, read the page text, run JavaScript and take page screenshots. It works
on page elements, so it is faster and more reliable than X11 clicking. claw
runs its own Chrome with the profile `~/.claw/browser` (Chrome refuses
remote control of your everyday profile); logins made there are kept.
`CLAW_BROWSER_PORT` (default 9222), `CLAW_BROWSER` and
`CLAW_BROWSER_PROFILE` change the port, program and profile.

### Automating Windows applications (COM)

On Windows (or in WSL, through the Windows `powershell.exe`), the **COM**
tool drives applications through COM automation: Word, Excel, PowerPoint,
Outlook, Access, Visio, and anything else with a ProgID, such as
`Shell.Application`, `WScript.Shell`, WMI (`WbemScripting.SWbemLocator`),
the Task Scheduler (`Schedule.Service`), Photoshop or AutoCAD. Ask for things
like "open report.xlsx, total column C and save it as a PDF" or "draft an
Outlook mail to …".

The agent starts or attaches to an application under a name (`excel`), then
reads and sets properties and calls methods on it (`ActiveSheet.Range("A1:C3")
.Value2`, `Documents.Open`). Ranges come back as rows, and rows sent back
fill a range. `inspect` lists an object's members so the model can find its
way around an unfamiliar API, `progids` lists what is registered, and `run`
executes PowerShell in the same session. One PowerShell session (Windows
PowerShell 5.1 preferred) keeps the objects for the whole claw session. A
call that gets no answer within 2 minutes, usually because the application
shows a dialog, restarts the session; the applications keep running and
`attach` reconnects. Paths must be Windows paths. Like X11, it needs approval
unless the permission mode is *bypass*. `CLAW_COM_POWERSHELL` picks another
PowerShell program.

### More agent tools

Besides reading, editing, shell and web tools, the agent has:

- **CodeSearch:** search the code by what you mean ("where are permissions
  checked"): a symbol index of functions, types and modules (cached in
  `~/.claw/index/`, updated as files change), ranked text search and file
  names.
- **Pty:** interactive programs in a terminal: Python or Node REPLs, `gdb`,
  `ssh`, installers that ask questions, TUIs. The agent starts one, types
  into it, presses keys (Ctrl+C, arrows) and reads what it shows.
- **Checkpoint:** the agent can save a snapshot before a risky batch of edits
  and restore it (restoring first saves the current state, so it can be
  undone too). Same private snapshot store as `/undo`.
- **Diagnostics:** runs the project's type checkers, linters and formatters
  (cargo check/clippy/fmt, pyright/mypy, ruff/black, tsc/eslint/prettier,
  go vet/gofmt, shellcheck, clang-tidy) and reads their findings. The `LSP`
  tool gives one file's language-server diagnostics.
- **RepoSignals:** reads the repository before changes: load-bearing files
  (imported by many), contested ones (many recent commits), failing ones
  (named by the last failing check), well-tested ones (an `lcov.info`
  report) and cleanup candidates; or one file in detail before editing it.
- **Trail:** agents working on the same repository (sub-agents, parallel
  chats, worktrees) leave fading notes on paths ("claim: I'm editing this",
  "danger: fragile", "info: …") and sense each other's before touching a
  file. Kept in `~/.claw/trails/`; notes halve in strength every 30-60
  minutes and disappear.
- **GoogleDork:** searches with operators (`site:`, `filetype:`, `intitle:`,
  `inurl:`, exact phrases, exclusions, dates) and gives a Google link for the
  same query.
- **HawkScan:** StackHawk's DAST scanner, as the HawkScan plugin gives it to
  Claude Code: ask for a security scan of a running app or API. `status`
  says what is missing (the `hawk` CLI, or else docker/podman with the
  `stackhawk/hawkscan` image; an API key in `HAWK_API_KEY` or from
  `hawk init --browser`; a `stackhawk.yml`). `guide` serves the HawkScan
  playbook and its references from the Claude Code plugin or
  `~/.claw/skills/hawkscan`. `validate`, `scan` (findings worst first) and
  `rescan` (only the checks that fired, to verify fixes) run in the sandbox
  like Bash; when it blocks the scanner, the result gives the command to rerun
  outside it, which you approve. Unlike Claude Code's plugin, claw does not
  scan on its own after every change.
- **GitHub / GitHubWrite:** the GitHub REST API without the `gh` CLI.
  `GitHub` only reads, so it works in read-only mode: search repositories,
  code and issues (GitHub search syntax, e.g. `repo:o/r is:open label:bug`);
  a repository's facts and README; open/closed issues and pull requests; one
  issue or PR with its comments (a PR also lists its changed files, and
  `diff` adds the diff); a file or folder at any branch, tag or commit;
  commits, releases, Actions runs, a user; and `api` for any other GET
  endpoint. `GitHubWrite` creates issues, comments and pull requests, opens
  or closes issues, and sends any other API request; it needs full access,
  so it asks each time unless allowed. The token comes from `GITHUB_TOKEN`,
  `GH_TOKEN` or `gh auth token`; reading works without one (60 requests an
  hour, public repositories, no code search), writing does not.
- **HuggingFace:** reads the Hugging Face Hub: search or list what is
  trending among models, datasets and Spaces (filter by tag, e.g. `gguf`,
  or author); a repo's facts (task, library, license, downloads, parameter
  count, GGUF architecture and context, gated) and its card; its files with
  sizes and download URLs; a text file such as `config.json`; today's papers
  or a paper search; and `whoami`. Weights are fetched by giving a file's
  URL to `Download`. Gated and private repos need `HF_TOKEN` (or
  `huggingface-cli login`) after accepting the terms on the repo page.
- **Custom agents:** an agent definition in `.claw/agents/`,
  `.claude/agents/` (in the project or above it), `~/.claw/agents/` or
  `~/.claude/agents/` (Claude Code's Markdown format, or Codex's TOML) is a
  sub-agent the model can run: its name is the Agent tool's `subagent_type`,
  its body is the sub-agent's instructions, and its `tools:` list (Claude Code
  names such as `Read`, `Grep`, `Bash`, or claw's) limits what it may use.
  `model:` is used only when it is a `provider/model` claw can route;
  Claude Code's `sonnet`/`opus`/`inherit` mean the session's model. They are
  listed in the system prompt, and `/agents` shows them. An unknown
  `subagent_type` is an error that names the available agents (before, it
  quietly ran a general-purpose agent).
- **Agent `fork`:** besides Explore, Plan, Verification and general-purpose
  sub-agents, the model can start a `fork`: a sub-agent that starts with the
  whole conversation so far (the finished turns) and the same system prompt,
  so its task can be one line. Like `/fork <prompt>`, but the model decides.
- **ExternalAgent:** hands a self-contained task to another agent CLI
  installed here, `claude` (Claude Code, `claude -p`) or `codex` (`codex
  exec`), and returns its final answer, so a local model can pass a hard
  subtask to a stronger one. It runs in the working directory with its own
  model, quota, sandbox and permissions (Claude Code with
  `--permission-mode acceptEdits`, Codex with its `workspace-write` sandbox),
  outside claw's sandbox, so the call asks for approval unless claw has
  full access. Esc and the timeout (30 minutes) stop it. Change the command
  lines with `"externalAgents": {"claude": {"args": [...]}, "codex":
  {"args": [...]}}` in `~/.claw/settings.json` (`{prompt}` and `{output}`
  are filled in).
- **apply_patch:** edits in Codex's patch format (`*** Begin Patch`, `***
  Update File:`, `@@`, `-`/`+` lines), several files in one call, all or
  nothing. The same rules as `edit_file` apply: files must have been read,
  and unchanged since. For models trained on that format (gpt-oss, Codex).
- **ExecuteCode:** the agent writes one Python script that calls claw's
  read-only tools as functions (`read_file`, `glob_search`, `grep_search`,
  `code_search`, `web_search`, `web_fetch`, the git tools,
  `session_search`), so ten searches and reads take one step instead of ten,
  and only what the script prints comes back. The script runs like a bash
  command (same sandbox and approval, Esc stops it, 300 s by default), with a
  limit of 100 tool calls. `agent(prompt, subagent_type=None)` in the script
  runs a sub-agent and returns its report; several, from threads, run in
  parallel (on a one-slot local server they take turns). On local models
  this saves the most time, so the launcher loads it with the core tools.
- **WebFetch** reads Wikipedia, arXiv, GitHub (repositories, files,
  folders, issues, pull requests) and Semantic Scholar/DOI links through
  their APIs instead of scraping the page; the result starts with a short
  header naming the source and the links worth following next. Set
  `GITHUB_TOKEN` and `SEMANTIC_SCHOLAR_API_KEY` to avoid their rate limits.
- **WebSearch** asks, in order: your local SearXNG (below), DuckDuckGo,
  then Brave Search and Bing. After DuckDuckGo answers with its bot check,
  claw leaves it alone for 30 minutes instead of asking on every search.
  The result says which engine answered.
- **TaskList** shows everything in the background: tasks, shells, monitors
  and sub-agents; **TaskStop** stops any of them.
- **image_generate** and **text_to_speech** need a backend: an
  OpenAI-compatible API (`CLAW_IMAGE_API_URL`, `CLAW_TTS_API_URL`, with
  `OPENAI_API_KEY`), or locally stable-diffusion.cpp's `sd` with
  `CLAW_SD_MODEL`, and piper (`CLAW_PIPER_MODEL`) or espeak-ng for speech.
  Images go to `~/Pictures/claw/`, audio to `~/Music/claw/`.

### How claw keeps a turn on track

These run on every turn without settings, and are mostly for the habits
of smaller local models:

- **Tool input repaired:** a tool call with small JSON mistakes (a code fence
  around it, trailing commas, `True`/`None`, single quotes, raw newlines in
  strings) is repaired instead of failing, and values of the wrong type
  (`"40"` for a number, `"true"` for a yes/no, one path where a list is
  expected) are converted to what the tool asks for. A call cut off at the
  output limit is never completed.
- **Tool calls written as text:** when the server's tool-call parser misses
  the model's format (a wrong chat template, llama.cpp without `--jinja`, an
  Ollama model parser that does not match), a call arrives as reply text:
  `<tool_call>{"name": …, "arguments": …}</tool_call>` (Hermes/Qwen) or
  `<function=…><parameter=…>…</parameter></function>` (Qwen3-Coder). claw runs
  it as a real call ("the model wrote 1 tool call(s) as text") instead of
  ending the turn. Not done when the reply also has a native call, for
  markup inside code fences, or with `CLAW_TEXT_TOOL_CALLS=0`.
- **Reasoning in the text:** `<think>…</think>` blocks a model writes into
  its reply are moved to the thinking block instead of being shown as the
  answer.
- **Loops stopped:** a reply stuck repeating one passage is not continued
  ("the reply got stuck repeating itself"), and its text is not sent back. A
  call that fails twice the same way gets a note to change approach and is
  not run after four failures; a tool that fails eight times in a row is not
  run again that turn. A call that keeps returning the same result is not
  run after three times; sending it again after that gets a sharper note
  naming the call, and three more steps of nothing but that blocked call end
  the turn ("stopped: the model kept repeating a blocked … call"). Compaction
  summaries collapse such a repeated call and its result into one entry
  marked as a loop, so a continued session does not start it again.
- **Check before finishing:** when a turn edited code and is about to end
  without having run anything since, the model is reminded once to build,
  test or run it, or to say it is not verified. Edits to docs and data
  files do not count. `"verifyOnStop": false` or `CLAW_VERIFY_ON_STOP=0`
  turns it off.
- **Instructions where the work is:** the first time the model reads, edits
  or `cd`s into a subdirectory, that directory's `AGENTS.md`, `CLAUDE.md`,
  `CLAW.md`, `.cursorrules` or `.clinerules` (and those of the directories
  between) come with the tool result, once a session. The system prompt
  only holds the ones from the working directory up.
- **Prompt injection noted:** results of WebFetch, WebSearch, Browser,
  SocialMedia, Download, GoogleDork and MCP tools that contain text aimed at
  the model ("ignore previous instructions", hidden HTML, commands that send
  secrets away, …) get a note telling the model it is data, not a request.
- **No whole-disk searches:** a recursive `grep`, `rg`, `find`, `fd` or `du`
  across the home directory or `/` is not run the first time: the model is
  told it takes minutes and reads unrelated private files (other projects,
  chat transcripts, keys), and to search the project instead. The same call
  made again runs. (In a test, a goal round spent 12 minutes grepping `~`
  and came back with lines of Claude Code's transcripts.)
- **Instruction edits seen:** when `CLAUDE.md`, `AGENTS.md` or another
  instruction file changes, is added or is removed during a session, the next
  message tells the model the new version (the system prompt is built once,
  so before this an edit only applied after a restart).
- **The time:** each message carries the current time and how long since
  the previous message, at most every 10 minutes, so "earlier" and
  "yesterday" make sense and the model can tell how long a background job
  has run.
- **Images within budget:** a tool result's text and the images it attaches
  share one budget (about 25,000 tokens; a quarter of
  `CLAW_TOOL_RESULT_MAX_CHARS` when set). Images over it are named, with
  their paths, instead of attached.
- **Long output kept:** bash output over 16 KiB keeps its start and its end
  (where errors and test summaries are), and the whole of it is saved under
  `~/.claw/tool-output/` for the model to read or grep.

#### LeanGate

A referee in the agent's loop. When the agent writes or edits code that
defines a function or type whose name already exists elsewhere in the
repository, the write is held back once and the agent is pointed at the
existing one (repeating the identical call writes it anyway); similar names
get a note. When a reply that edited files is about to end, claw runs the
project's type checker (`cargo check`, `tsc`, pyright/mypy, `go build`);
errors in the files it edited go back to the agent, which fixes them before
finishing (two rounds at most; a "LeanGate" line says so). It is skipped when
you stop the reply. Turn it off with `"leanGate": false` in
`~/.claw/settings.json` or `CLAW_LEANGATE=0`; `CLAW_LEANGATE_TIMEOUT_SECS`
(default 240) limits the check.

#### Thinking commands

- `/scope-check <idea>` (or `/scope`): an adversarial second opinion before
  building something: a delusion score and verdict, the case for not
  building it, and a v0 plan small enough for one session.
- `/diverge <problem>`: ideas that differ in kind, from techniques chosen for
  the problem, with the blind spots nobody would think of.

#### Local search engine (SearXNG)

`scripts/install-searxng.sh` runs [SearXNG](https://docs.searxng.org), a
self-hosted meta search engine, in rootless podman on `127.0.0.1:8888`,
started at login (Quadlet unit `~/.config/containers/systemd/searxng.container`,
settings in `~/.config/searxng/settings.yml`). It asks Google, DuckDuckGo,
Brave, Bing, Wikipedia and others at once, so one engine blocking you does
not empty the results; claw uses it whenever it answers.
`CLAW_SEARXNG_URL` points claw elsewhere, `CLAW_SEARXNG_URL=off` skips it.
Manage it with `systemctl --user status|restart|stop searxng`; update it with
`podman pull docker.io/searxng/searxng:latest && systemctl --user restart searxng`.

The `claw` launcher offers a `local/` model a core set of tools to save
context (`CodeSearch`, `Pty`, `Checkpoint`, `Diagnostics`, `GoogleDork`,
`vision_analyze`, `RepoSignals`, `Trail`, `HawkScan`, `GitHub`, `GitHubWrite`,
`HuggingFace` and `ExecuteCode` are in it); `CLAW_ALL_TOOLS=1 claw` offers all of them.
When claw starts with a hosted model (`--model`, else `model` in settings, aliases
resolved), it gets every tool. A `/model` switch later keeps the starting set.

Every model gets the everyday tools (shell, files, search, web, `Browser`,
`Download`, `X11`, `vision_analyze`, `Monitor`, `CodeSearch`, notebooks, todo
list, questions, skills, sub-agents) with their full description. The other
specialised tools are listed by name, with a one-line summary, in
`ToolSearch`; the model loads one with `ToolSearch` (`select:GitBlame`) when it
needs it, and it then stays loaded for the session, as in Claude Code. This
keeps each request about 15k tokens smaller and the model's choice of tools
short. `CLAW_DEFER_TOOLS=0` sends every description.

`SocialMedia` saves the images and videos of an Instagram, Facebook or
Pinterest post, reel or pin from its link (share and short links work) into
`~/Downloads/claw-social/<platform>`, and returns lines that show the images
in the reply. It also takes an Instagram profile (its link or `@username`)
and fetches its latest `count` posts (default 6): the post links come from
the profile page in claw's browser. Public posts need no login; a Netscape cookie file at
`~/.claw/cookies/<platform>.txt` (as a browser's "export cookies" extension
writes it) is used when present, which Instagram needs for private posts.

Every session's system prompt has a short "What you can do" section (the
same in the terminal and the desktop app): how to show pictures, which tool
fetches what, `/undo`. It is built into claw so it always matches its tools;
keep `AGENTS.md` for your own instructions.

In the desktop app, the terminal's non-interactive slash commands work too
(`/undo`, `/rewind`, `/compact`, `/cost`, `/status`, `/context`, `/memory`,
`/diff`, `/plan`, `/effort`, `/fast`, `/permissions`, `/export`, …): their
output appears in the chat. Each request is checkpointed as in the
terminal, so `/undo` reverts its file changes (not in incognito chats).

After a reply with Markdown images (`![alt](url or path)`), claw checks that
each one loads (in the terminal and the desktop app). If one definitely does
not (a missing page or file, or something that is not an image), the model is
told which and asked once to replace it. Rate limits and server errors do not
count: the picture may still show.

While a local server reads the prompt, the activity line (and the desktop
app) shows "Reading the prompt N%": on a cold llama-gpu the first reply waits
a minute or two for that.

`read_file` shows numbered lines. Asked for a whole file that is larger than
one page (16k characters on a 32k window), it returns the file's outline: the
numbered lines where its functions, types and sections start. The model then
reads the part it needs with `offset` and `limit`; asking for the whole file
again gets the first page. When an edit's `old_string` is not in the file,
the error shows the closest lines, numbered, so the model can fix the edit
without re-reading the file. Reading the same lines again while the earlier
result is still in the conversation returns a short "unchanged" note instead
of the text. Shell commands run in bash (in `sh` only where bash is not
installed).
`claude-tool-list.md` lists every tool and its status.

## 11. Instructions and memory

`claw` adds these files to the system prompt:

- `~/.claw/CLAUDE.md` and `~/.claw/AGENTS.md`: your personal instructions for
  every project;
- Claude Code's personal files, `~/.claude/CLAUDE.md` and `~/.claude/rules/`
  (`$CLAUDE_CONFIG_DIR` if set), so what you wrote for Claude Code applies here;
- in the project, up to the git root: `CLAUDE.md`, `CLAW.md`, `AGENTS.md`,
  `.claw/CLAUDE.md`, `.claw/AGENTS.md`, `.claude/CLAUDE.md`,
  `.claw/instructions.md` (added to your personal ones, which stay loaded);
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

With the [learning loop](#learning-loop) on (the default), a review after
each turn that taught something saves memories, so the model does not stop
its work to save passing facts; it saves one itself when you say "remember
…". Say "forget …" to have it delete one. With the loop off, the model saves
a memory when you correct it, state a preference, or it learns something
about the project that will matter later. It is told not to save
what the code or git history already shows, or secrets. Writing in the memory
folder never asks for permission. claw creates the folder when a session
starts (the index appears with the first memory).

`/memory` lists the saved memories and the folder; edit or delete the files to
change them. Turn it off with `"autoMemoryEnabled": false` in settings or
`CLAW_AUTO_MEMORY=0`.

Three additions keep memories honest (all optional in the file format, so
Claude Code still reads the same folder):

- **Anchors:** a memory about specific code lists it in its frontmatter,
  under `metadata:`, as `anchors: path#symbol, path` (paths from the project
  root; a bare path means the whole file). The first time claw loads it, it
  adds a fingerprint (`src/lib.rs#alpha@a1340cfa0a7d`). When that function or
  file changes or disappears, the memory is shown to the model as
  "⚠ stale: … changed since this was saved" and moved below the others, so
  it checks before relying on it. To refresh one, rewrite the anchors
  without the `@…` part (or delete the memory).
- **Activity:** `.claw-activity.json` in the memory folder counts sessions
  in the project and when each memory was last read or written. A memory
  unused for 40 sessions is listed last as dormant. Time away from the
  project does not count, only sessions in it.
- **Reflection:** `/reflect [focus]` has the model save what the session
  taught (decisions and why, facts learned the hard way, mistakes and their
  fixes, your corrections), updating existing memories instead of adding
  duplicates, at most five. It also runs on `/exit` after a session of five
  or more turns; Esc skips it, and `"memoryReflectOnExit": false` or
  `CLAW_MEMORY_REFLECT=0` turns that off.

### Learning loop

After a turn that taught something, claw reviews it in the background and
saves what will help next time:

- **Skills** are procedures: how to do a kind of task in this setup (the
  commands that work, the steps, the pitfalls hit, how to check the result).
  They go to `~/.claw/skills/<name>/SKILL.md`, marked `source: learned`, and
  are listed in every session's system prompt with the other skills (the
  project's `.claw/skills/` and `.claude/skills/`, and Claude Code's
  `~/.claude/skills/`), so the model loads one with the Skill tool when a
  task matches.
- **Memories** are facts, saved in the [auto memory](#auto-memory) folder.

Every review is logged with its outcome (saved, nothing worth saving,
cancelled by your next prompt, or failed) in `~/.claw/learning/reviews.log`;
`/learn` shows the latest ones.

Which turns are reviewed: one where you corrected claw or said how to work
("don't …", "always …", "from now on …"), where a failure was then overcome,
with ten or more tool calls, or where a learned skill was used and something
failed. At most one review every two turns, unless you corrected it. Other
turns cost nothing.

The review is the conversation plus one instruction. It goes to
`auxiliaryModel` when set, else to the main model with the turn's own
request, so a local server reuses the prompt it already processed. Sending a
new prompt cancels a review still running, so a one-slot local server is
never busy with it when you need it. The model answers with a list of
changes and claw applies them itself after checking each one: a skill you
wrote is never changed, text that looks like prompt injection, a backdoor or
a hardcoded secret is refused, and the instruction holds it to a high bar
(would it still help a month from now; is it already saved; did you say it,
or only not object). What was saved is shown as a dim `✎ learned: …` line.

Skills improve with use: each load is counted, and a learned skill used in a
turn that had failures is shown to the review with its text, to be fixed or
extended. A curator keeps the set small: a learned skill unused for 12
sessions is shown as stale, and one unused for 40 sessions is archived to
`~/.claw/skills/.archive/` (pinned skills never are). Look-alike skills are
offered to the review to merge. Only sessions count, not time away.

`/learn` shows the learned skills with their use and the latest changes;
`/learn undo` reverts the last review or curator pass (every change is
journaled with what it replaced, in `~/.claw/learning/`); `/learn now`
reviews the conversation at once; `/learn pin <skill>` keeps a skill from
being archived; `/learn off` turns the loop off (`"learningLoop": false`,
`CLAW_LEARNING=0`, or the older `"learning": "off"`).

`CodeSearch` answers structural questions about the code as well:
`kind: "callers"` lists where a function is called from (with the function
each call sits in) and `kind: "callees"` what a function calls.

## 12. Settings

Settings merge in this order, later files winning:

1. `~/.claw.json`
2. `~/.config/claw/settings.json`
3. `<repo>/.claw.json`
4. `<repo>/.claw/settings.json`
5. `<repo>/.claw/settings.local.json` (personal, not committed)

**Project files need your approval.** Files 3–5 come with whatever directory
claw starts in: a cloned repository or a downloaded archive can ship them.
Until you trust a file, claw uses only its presentation keys (`theme`,
`editorMode`, `viewMode`, `outputStyle`, `diffPanel`, `scrollSpeed`,
`autoCompactWindow`, `overflowToolResultChars`) and warns about the rest
(hooks, MCP servers, sandbox, permissions, model and provider settings, ...).
Run `claw trust` in the directory: it shows each such file and asks before
trusting it. Trust is pinned to the file's content (recorded in
`~/.claw/trusted-project-configs.json`), so any later change, by anyone,
needs a new approval. Edits you make through claw (`/permissions`, `/config`,
`/auto-mode-setup`, `/init`) keep a file trusted when it was trusted before.

**Every key.** `~/.claw/settings.json` can list all of them; `null` means
"not set" (the default applies, and a lower file's value is kept), so a full
file works as a template.

| Key | Value (default) | What it does |
|---|---|---|
| `model` | model or alias | Model claw starts with; `/model` saves it |
| `fastModel` | model (null) | Model for fast mode (`/fast`) |
| `subagentModel` | model (null = main model) | Model for sub-agents |
| `auxiliaryModel` | model (null = main model) | Side tasks: summaries, `/btw`, `/recap`, learning |
| `aliases` | `{name: model}` | Short names for `/model`, `/fallback`, `--model` |
| `providerFallbacks` | `{primary, fallbacks: [model]}` | Model to switch to on failures; `/fallback` sets it |
| `provider` | `{kind, apiKey, baseUrl, model}` (null) | Provider saved by `/login` |
| `customProviders` | `{id: {baseUrl, apiKeyEnv, apiKey, label, maxOutputTokens}}` | Extra OpenAI-compatible providers (`id/model`) |
| `modelServers` | `{provider: {start, stop, health, stopOnExit}}` | Start/stop a model server (llama-gpu for `local/`) |
| `permissions` | `{defaultMode, bashApproval, allow, ask, deny, deniedTools}` | Permission mode and rules (section 6) |
| `sandbox` | `{enabled, namespaceRestrictions (true), networkIsolation (false), filesystemMode ("workspace-only"\|"off"\|"allow-list"), allowedMounts, excludedCommands}` | Command sandbox |
| `trustedRoots` | `[path]` ([]) | Folders trusted without asking |
| `hooks` | `{PreToolUse, PostToolUse, PostToolUseFailure: [command]}` | Commands run around tool calls |
| `env` | `{NAME: value}` | Environment for claw and its commands |
| `mcpServers` | `{name: {command, args, env} \| {url}}` | MCP servers |
| `mcpServerStates` | `{name: bool}` | MCP servers turned off with `/mcp` |
| `plugins` | `{enabled, externalDirectories, installRoot, registryPath, bundledRoot, maxOutputTokens}` | Plugins |
| `lspServers` | `{extension: {command, args}}` | Language servers for the Diagnostics tool |
| `oauth` | `{clientId, authorizeUrl, tokenUrl, callbackPort, manualRedirectUrl, scopes}` (null) | Custom OAuth login |
| `rulesImport` | `"auto"` \| `"none"` \| `[framework]` | Import Cursor/Copilot/… rule files |
| `autoMemoryEnabled` | bool (true) | Memory files in the prompt |
| `memoryReflectOnExit` | bool (true) | Save what the session learned when it ends |
| `learningLoop` | bool (true) | The [learning loop](#learning-loop): background reviews that save skills and memories, and the skill curator (`"learning": "off"` also turns it off) |
| `verifyOnStop` | bool (true) | Remind the model once to check code it edited before a turn ends |
| `externalAgents` | `{claude: {args}, codex: {args}}` | Command lines for the ExternalAgent tool (`{prompt}`, `{output}` filled in) |
| `leanGate` | bool (true) | Lean checks after edits |
| `theme` | theme name | `/theme` saves it |
| `editorMode` | `"normal"` \| `"vim"` | Input editor keys (`/vim`) |
| `viewMode` | `"normal"` \| `"focus"` | Focus view (`/focus`) |
| `outputStyle` | style name (`"default"`) | `/output-style` saves it |
| `diffPanel` | `"on"` \| `"off"` | Diff panel (`/diffpanel`) |
| `scrollSpeed` | rows as a string (`"3"`) | Mouse-wheel rows per step |
| `autoCompactWindow` | tokens as a string (null = from the model) | Auto-compaction threshold (`/autocompact`) |
| `overflowToolResultChars` | characters as a string, or `"off"` (null = a quarter of the window) | How far tool results are cut after a context overflow |
| `statusLine` | `{command}` (null) | Command whose output is shown in the status line |

Some user-level settings (theme, provider) are saved by the REPL to
`~/.claw/settings.json`. `modelServers` there makes claw start a provider's
model server (for `local/...` models: `llama-gpu`) when it starts and stop it
when the last claw exits; see PROVIDERS.md. Example project file:

```json
{
  "model": "sonnet",
  "permissions": {
    "defaultMode": "workspace-write",
    "bashApproval": "risky",
    "allow": ["bash(cargo test:*)"],
    "deny": ["bash(curl:*)"]
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
| `CLAW_SERVER_START`, `CLAW_SERVER_STOP`, `CLAW_SERVER_HEALTH`, `CLAW_SERVER_STOP_ON_EXIT`, `CLAW_MANAGE_SERVER` | Override `modelServers` (start/stop a model server with claw; see PROVIDERS.md) |
| `CLAW_SEARXNG_URL` | SearXNG for WebSearch (default `http://127.0.0.1:8888`, `off` skips it) |
| `CLAW_CONFIG_HOME` | Use another directory instead of `~/.claw` |
| `CLAW_API_CONNECT_TIMEOUT` | Seconds to wait for a connection (default 30) |
| `CLAW_API_REQUEST_TIMEOUT` | Seconds to wait for the next piece of a reply before giving up (default 300; 1800 for local servers such as Ollama). Long replies that keep streaming are never cut off |
| `CLAW_REASONING_REPLAY_MAX_CHARS` | How much of each earlier step's thinking (its end) goes back to a `local/` model within a turn; default 4000, `0` for all of it. Hosted models always get all of it |
| `CLAW_THINKING_BUDGET` | Most tokens a `local/` model may think per reply (llama-server's `thinking_budget_tokens`); default half the reply budget, never more than the reply budget minus 1024; `-1` sends none |
| `WGPU_POWER_PREF` | Which GPU the desktop app draws on: `low` (its default, a laptop's integrated GPU, leaving the discrete GPU's memory to a local model) or `high`. `ICED_BACKEND=tiny-skia` draws on the CPU instead |
| `WINIT_X11_SCALE_FACTOR` | UI scale of the desktop app on X11. claw-desktop sets `1` when it is unset, so the window is the same size on a high-DPI laptop panel as on an external monitor; set e.g. `1.5` to scale it up (Settings → Text size changes only the text) |
| `CLAW_TEXT_TOOL_CALLS` | `0` stops running tool calls a model wrote as text (`<tool_call>…`, `<function=…>`) |
| `CLAW_DEFER_TOOLS` | `0` sends every tool's full description, instead of listing the specialised tools by name in `ToolSearch` |
| `CLAW_TOOL_RESULT_MAX_CHARS` | Cap on what one tool call sends to a local model (the launcher sets 12000). Hosted models get a quarter of their context window instead (4k–100k characters). A cut result is saved whole under `~/.claw/tool-output/` (kept a week) and the note tells the model where to read the rest |
| `CLAW_MODELS_CATALOG=0` | Don't look up hosted models' context and output limits in the models.dev catalog |
| `CLAW_MODELS_URL` | Where to fetch the catalog's `api.json` (default `https://models.dev`) |
| `CLAW_SNAPSHOTS=0` | Turn off checkpoints |
| `CLAW_THEME` | Theme, overriding the saved one |
| `CLAW_DIFF_PANEL=0` | No diff panel this run (`/diffpanel off` saves it) |
| `CLAW_FULLSCREEN=0` | Inline view instead of the full-screen window (output stays in the terminal's history) |
| `CLAW_ENGINE=infinity` | Run `claw -p` / `claw prompt` on the asynchronous multi-threaded engine (experimental; see *Asynchronous engine* in section 9) |
| `CLAW_MERMAID` | `0` prints ```` ```mermaid ```` blocks as code only, `text` always draws them as box-drawing text (default: an image where inline images work, text elsewhere) |
| `CLAW_INLINE_IMAGES` | `0` never draws reply images in the terminal, `1` always does (default: kitty and Ghostty, not inside tmux) |
| `CLAW_AUTO_MEMORY=0` | Turn off auto memory for this run |
| `CLAW_LEARNING=0` | Turn off the learning loop for this run |
| `CLAW_VERIFY_ON_STOP=0` | Do not remind the model to check edited code before a turn ends |
| `CLAW_OVERFLOW_TOOL_RESULT_CHARS` | When a request overflows the context window, longest a tool result may stay (`64k`; at least 1k), or `off` to never cut. Default: about a quarter of the model's window. Also `overflowToolResultChars` in `~/.claw/settings.json` |
| `CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS` | Context size (input tokens of the last request) at which the session is compacted automatically; default 100000 |
| `NO_COLOR` | No colours |
| `CLAW_OUTPUT_FORMAT=json` | Default to JSON output for non-interactive commands |
| `CLAW_LOG`, `RUST_LOG` | Logging |
| `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` | Proxies |

### Fallback, side tasks and learning

These settings in `~/.claw/settings.json` keep a session going and let claw
learn across sessions:

```json
"providerFallbacks": { "fallbacks": ["local/cybertiel-35b-a3b"] },
"auxiliaryModel": "nvidia/nvidia/nemotron-3-super-120b-a12b",
"learningLoop": true
```

- **`providerFallbacks.fallbacks`** (set it with `/fallback`): when the model in use is rate-limited
  (429), out of credit (402), failing (5xx), silent (no reply within 60s), or
  replies with nothing twice, claw switches to the first fallback that works
  for the next 10 minutes, then tries the configured model again. A `local/`
  fallback starts its model server (llama-gpu) when it is stopped, and with
  `modelServers.local.stopOnExit` it stops again when claw exits. A session that
  falls back to a local model uses the launcher's short tool list. With a
  fallback set, a failing provider is retried twice instead of eight times.
  A 402 that names the reply size the account can afford (OpenRouter's "can
  only afford N") is first retried with that smaller reply budget, which
  then holds for that model until claw exits.
- **`visionModel`**: a model that reads images, e.g. the local
  `"local/cybertiel-35b-a3b"` (llama-gpu loads its `mmproj`, as for the desktop's
  Attach) or `"openrouter/nvidia/nemotron-3.5-super-vl:free"`. When the model in use
  takes no images (screenshots from `Browser` or `X11`, `vision_analyze`),
  claw sends it this model's description of each image instead. Unset, such
  a model gets a note that the image was left out. Images go to this model's
  provider, so pick one you are happy to send screenshots to.
- **`auxiliaryModel`**: the model for side tasks (compaction summaries,
  `/btw`, `/recap`, learning), so they do not spend the main model's quota and
  keep working while it is limited. Unset, the main model does them (a local
  main model keeps the summary built from the messages, which costs nothing).
- **`learningLoop`**: reviews turns that taught something and saves skills
  and memories; see [Learning loop](#learning-loop). With `auxiliaryModel`
  set, reviews go there instead of to the main model.
- **`SessionSearch`** (tool): the model can search every past claw session
  for words, to recall earlier fixes and decisions.

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

**`Auto-compacting session (round N/4 …)`**
A request didn't fit the model's context window. `claw` compacts the history,
cuts very large tool results (`Cut N oversized tool result(s)`), and continues
the same request from where it stopped, without sending your prompt again.
Esc stops it at any point. With small windows (e.g. llama.cpp's default
`--ctx-size`), reading a whole large file is the usual cause: ask for line
ranges instead. Results are cut to about a quarter of the model's window, so
a large-context model keeps them whole unless one is enormous; set
`CLAW_OVERFLOW_TOOL_RESULT_CHARS` (or `"overflowToolResultChars"` in
`~/.claw/settings.json`) to a size such as `"64k"`, or `"off"` to never cut.

**`(reply cut off at the N-token limit; asking the model to continue)`**
The reply reached its token budget. `claw` asks the model to continue where it
stopped (up to 3 times in a row); a tool call that was cut off is not run, and
the model is told to send less per call. A `local/` model cut off while still
thinking answers the next request with thinking switched off. A reply that
stops with only thinking (no answer, no tool call) is treated the same way:
the model is asked to act on it instead of the turn ending with nothing. For `local/` models the budget is a
quarter of the server's context window, which `claw` reads from llama.cpp's
`/props` (`CLAW_CONTEXT_WINDOW` is only the fallback for servers that don't
report it), and the session is compacted once it fills the window less the
reply budget and a small margin (about 69%)
unless `/autocompact` sets another size. A server started with a bigger `-c`
gives both more room.

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
output can't fill the window, and use `/context` to watch usage. A long
file read is cut at a whole line and ends with the `offset` the model should
use to read the rest. File reads are paged by the model's context window
(about a quarter of it, 4k-100k characters) rather than by this cap, so a
bigger `-c` on llama-server means fewer, larger reads.

**A command keeps asking for approval**
Answer `a` to allow it for the session, add an `allow` rule, or set
`"bashApproval": "risky"`.

**The terminal looks wrong after a crash**
Run `reset` (or `stty sane`).
