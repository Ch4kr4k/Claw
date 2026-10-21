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
is available, and `/model local/<section>` (or an alias such as `/model coder`
or `/model uncensored` from `"aliases"` in `~/.claw/settings.json`) switches to
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
  (`coder → local/qwen36-35b-a3b`; aliases to local models the server no
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
bottom, even while you scroll back. The bottom area grows for a long prompt,
the `/` menu, an approval or a question, and shrinks back afterwards.

| Key | Action |
|---|---|
| Mouse wheel | Scroll the conversation (3 rows a step) |
| PgUp / PgDn | Scroll a page |
| Ctrl+Home | Jump to the start of the conversation |
| Ctrl+End | Jump back to the end (a *Jump to bottom* hint shows while you are scrolled up; clicking it does the same) |
| Click and drag | Select conversation text; it is copied to the clipboard when you let go (*Copied N characters* shows briefly). Dragging past the top or bottom edge scrolls. A click clears the selection |

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
| Esc | Stop the reply, and the shell command it is running (that command's process group gets SIGTERM, then SIGKILL after 3 s). Background jobs keep running; stop them with `KillShell`/`TaskStop` or `/tasks` |
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
the running task: it is answered at once, beside it, from the conversation as
last sent to the model (no tools), and the answer is printed between the task's
steps (the input row shows `answering /btw…` meanwhile). claw desktop works the
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

Switch with `--permission-mode MODE` or `/permissions MODE`. When a call needs
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
model so, and the model asks you to rerun it on the host (below).
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
| `/goal [condition\|clear]` | Keep working across turns until the condition holds (up to 20 rounds; Esc pauses). No argument shows the goal |
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
| `/context [show\|clear]` | Estimated tokens versus the model's context window |
| `/usage`, `/cost`, `/stats` | Token usage, estimated cost, cache hit rate |
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
| `/tasks [list\|get <id>\|stop <id>]` | Background jobs started by the agent |
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
early.

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
local `qwen36-35b-a3b` does, through its `mmproj` vision file).

In kitty and Ghostty, images in replies (`![alt](url)`: web URLs, local
paths, PNG/JPEG/GIF/WebP/SVG) are drawn in the terminal, in the full-screen
and the inline view, up to 80 columns by 24 rows. SVG is drawn on white. The
model is told it can show pictures this way. Elsewhere, and inside tmux, the
reply shows `[image:url]`. `CLAW_INLINE_IMAGES=0` turns this off, `=1` forces
it on.

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
`qwen36-35b-a3b`) can check what happened; only the newest two screenshots
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
`vision_analyze`, `RepoSignals` and `Trail` are in it); `CLAW_ALL_TOOLS=1 claw` offers all of them.
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
| `learning` | `"on"` \| `"off"` | Background memory/skill reviews and the skill curator |
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
| `CLAW_DEFER_TOOLS` | `0` sends every tool's full description, instead of listing the specialised tools by name in `ToolSearch` |
| `CLAW_TOOL_RESULT_MAX_CHARS` | Cap on what one tool call sends to a local model (the launcher sets 12000). Hosted models get a quarter of their context window instead (4k–100k characters). A cut result is saved whole under `~/.claw/tool-output/` (kept a week) and the note tells the model where to read the rest |
| `CLAW_MODELS_CATALOG=0` | Don't look up hosted models' context and output limits in the models.dev catalog |
| `CLAW_MODELS_URL` | Where to fetch the catalog's `api.json` (default `https://models.dev`) |
| `CLAW_SNAPSHOTS=0` | Turn off checkpoints |
| `CLAW_THEME` | Theme, overriding the saved one |
| `CLAW_DIFF_PANEL=0` | No diff panel this run (`/diffpanel off` saves it) |
| `CLAW_FULLSCREEN=0` | Inline view instead of the full-screen window (output stays in the terminal's history) |
| `CLAW_INLINE_IMAGES` | `0` never draws reply images in the terminal, `1` always does (default: kitty and Ghostty, not inside tmux) |
| `CLAW_AUTO_MEMORY=0` | Turn off auto memory for this run |
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
"providerFallbacks": { "fallbacks": ["local/qwen36-35b-a3b"] },
"auxiliaryModel": "nvidia/nvidia/nemotron-3-super-120b-a12b",
"learning": "on"
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
  `"local/qwen36-35b-a3b"` (llama-gpu loads its `mmproj`, as for the desktop's
  Attach) or `"openrouter/nvidia/nemotron-3.5-super-vl:free"`. When the model in use
  takes no images (screenshots from `Browser` or `X11`, `vision_analyze`),
  claw sends it this model's description of each image instead. Unset, such
  a model gets a note that the image was left out. Images go to this model's
  provider, so pick one you are happy to send screenshots to.
- **`auxiliaryModel`**: the model for side tasks (compaction summaries,
  `/btw`, `/recap`, learning), so they do not spend the main model's quota and
  keep working while it is limited. Unset, the main model does them (a local
  main model keeps the summary built from the messages, which costs nothing).
- **`learning`**: after a turn with four or more tool calls, a background
  request decides whether to save a memory or a skill
  (`~/.claw/skills/<name>/SKILL.md`, marked `learned-by: claw`); what it saved is
  shown at the next prompt. Once a week, while idle, the learned skills are
  merged or archived (`~/.claw/skills/.archive/`; never deleted, and skills you
  wrote are never touched). `"off"` turns it off. It needs `auxiliaryModel` or a
  hosted main model.
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
