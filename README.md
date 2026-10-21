# claw

A terminal coding and sysadmin agent in Rust, tuned to run well on **local models on modest hardware**.

This is a fork of [**ultraworkers/claw-code**](https://github.com/ultraworkers/claw-code), an open-source Rust reimplementation of a Claude Code–style agent harness. The fork was built **with Claude** (pair-programmed in Claude Code) and pushes the upstream harness toward an *almost complete* agent for a niche: one person, one Linux box, a local LLM, and real work. That work is code, system administration and security research.

> [!NOTE]
> This is a personal fork, not the upstream project. For the original harness, its roadmap and community, go to [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code).

---

## Why this fork

Upstream claw-code is a general harness that mostly targets hosted APIs. This fork is for a different setup:

- **A local model is the default.** It is tested daily against `llama-server` (llama.cpp) running a Qwen 35B-A3B MoE on a 4 GB GPU, with the experts kept in system RAM. Timeouts, context limits, reasoning fields and tool-call quirks are handled for slow local inference.
- **It does the work itself.** It runs commands, reads output, edits files and checks results instead of telling you what to type.
- **It is safe by default.** Commands run in a Linux namespace sandbox, risky shell commands need your approval, and every turn is checkpointed so you can undo it.
- **It feels like Claude Code.** That means the input box, live status, markdown rendering, interrupting with Esc and steering a running agent.

## What it adds on top of upstream

**Interface**
- Claude Code–style input box: a `❯` prompt between rules, plus a status line showing permission mode (`⏵⏵ auto mode on`, shift+tab to cycle), model, context used and running background shells.
- Live activity row while the agent works: `✻ Spelunking… (36s · ↓ 2.5k tokens · thinking · esc to interrupt)`. The input box stays usable, so you can type and queue a message for the running agent.
- Markdown rendering for replies: headings, lists, syntax-highlighted code blocks, and **boxed tables** (`┌┬┐`) that keep column alignment and wrap to fit the terminal.
- Markdown-aware line editor with multi-line paste, a `/` command menu, `@path` completion, persistent history and Ctrl+R search.
- **Esc** or **Ctrl+C** stops a reply without losing it. The prompt, the finished tool calls and their results stay in the conversation, marked as interrupted.

**Agent and runtime**
- Content-aware bash permissions: privileged or destructive commands need approval, and every segment of a `&&`/`;` chain is checked.
- A namespace sandbox (`unshare` user/mount/pid/ipc/uts) that keeps your real `$HOME`, so `~/...` paths work as expected.
- Consecutive read-only tool calls run in parallel.
- Background shell jobs, plus shadow-git checkpoints of the working tree with `/undo`.
- Reads Office, PDF and notebook files, writes `.docx`/`.pptx`, and edits `.xlsx`.
- Image input (base64 / data URLs for OpenAI-compatible servers).
- User-level instructions in `~/.claw/AGENTS.md`, loaded in every directory.

**Local-model fixes**
- Idle-read timeouts instead of a total deadline, so long generations on a slow GPU are not cut off. Local servers default to 30 minutes.
- Stream usage is requested so `/usage` and auto-compaction see real token counts.
- Tool-call-only messages are compatible with Ollama and llama.cpp, and reasoning fields from Qwen-style models are parsed.
- Timeouts no longer trigger pointless auto-compaction.

## Quick start (local model)

**1. Serve a model** with an OpenAI-compatible endpoint. This example uses llama.cpp:

```bash
llama-server -m Qwen3.6-35B-A3B-Q4_K_M.gguf --alias local/qwen3.6-35b-a3b \
  --host 127.0.0.1 --port 8080 --jinja -c 65536 \
  -ngl 99 -ncmoe 99 -fa on -ctk q8_0 -ctv q8_0   # MoE experts in RAM, rest on the GPU
```

**2. Build and install claw:**

```bash
git clone https://github.com/Ch4kr4k/claw-code
cd claw-code
scripts/install-local.sh            # release build, installs `claw` to ~/.local/bin
```

**3. Point it at the server and run it:**

```bash
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
export CLAW_CONTEXT_WINDOW=65536    # match llama-server's -c; shown in the status line
claw --model local/qwen3.6-35b-a3b
```

Or set the model once in `~/.claw/settings.json`:

```json
{ "model": "local/qwen3.6-35b-a3b", "sandbox": { "enabled": true } }
```

> [!TIP]
> Avoid a `qwen/` prefix in the model alias. claw routes that prefix to DashScope instead of your local server.

Hosted providers still work: Anthropic, OpenAI, OpenRouter, xAI and DashScope. See [`PROVIDERS.md`](./PROVIDERS.md).

## Everyday use

| Key / command | What it does |
|---|---|
| `Enter` / `Shift+Enter` | send / new line |
| `Esc`, `Ctrl+C` | stop the current reply (kept in history) |
| typing during a reply | queue a message for the running agent |
| `Shift+Tab` | cycle permission mode: plan → auto → bypass |
| `!cmd` | run a shell command yourself and add its output to the context |
| `# note` | save a one-line note to `AGENTS.md` |
| `/undo`, `/rewind` | roll back what the last turns changed |
| `/compact`, `/usage`, `/status` | manage context and see token use |

The full list is in [`MANUAL.md`](./MANUAL.md).

## Tips for small local models

- **Context is the bottleneck.** The system prompt and tool definitions take about 6k tokens, and reasoning models can use the rest thinking. Use a 32k context if your VRAM allows it, or turn thinking off (`--chat-template-kwargs '{"enable_thinking":false}'` on llama-server).
- **Send fewer tools.** Fewer tool descriptions mean faster prompt processing, and `--allowedTools` limits what is sent.

## Documentation

- [`MANUAL.md`](./MANUAL.md): keys, permissions, slash commands, undo, settings, environment variables, troubleshooting
- [`ARCHITECTURE.md`](./ARCHITECTURE.md): crate map, request lifecycle, interrupts, sandbox, state on disk
- [`PROVIDERS.md`](./PROVIDERS.md): hosted and local providers
- [`USAGE.md`](./USAGE.md): quick commands, sessions and config
- [`rust/README.md`](./rust/README.md): workspace layout

## Credits

- **Upstream:** [ultraworkers/claw-code](https://github.com/ultraworkers/claw-code) and its contributors. The harness, crates and most of the code come from there.
- **Built with:** [Claude](https://claude.ai), through Claude Code, which wrote and reviewed most of this fork's changes alongside the maintainer.
- Parts of the REPL and runtime were ported from an earlier personal harness (AzureAiHarness).

## License and disclaimer

MIT, same as upstream. See [`LICENSE`](./LICENSE).

This project is **not affiliated with, endorsed by, or maintained by Anthropic**, and it does not claim ownership of any Claude Code source material.
