# Switching Model Providers

How to point `claw` at Claude (Anthropic), OpenAI, Z.ai, OpenRouter,
DeepSeek, Gemini, Groq, Mistral, xAI, DashScope or any other OpenAI-compatible service, and how to switch back to your local
Ollama models. For everything else about using `claw`, see
[`MANUAL.md`](./MANUAL.md).

## 1. How claw chooses a provider

For every request, `claw` picks a provider in this order:

1. **`OLLAMA_HOST` is set → Ollama, for every model.** Even `claude-…` or
   `gpt-…` names go to Ollama (which then answers `model not found`).
2. Otherwise, the **model name** decides:

   | Model name starts with | Provider | Key variable |
   |---|---|---|
   | `claude`, or alias `opus` / `sonnet` / `haiku` | Anthropic | `ANTHROPIC_API_KEY` |
   | `gpt-` or `openai/` | OpenAI (or the gateway in `OPENAI_BASE_URL`) | `OPENAI_API_KEY` |
   | `local/` | Any OpenAI-compatible server in `OPENAI_BASE_URL` (prefix removed before sending) | `OPENAI_API_KEY` |
   | `qwen/`, `kimi/` | Alibaba DashScope | `DASHSCOPE_API_KEY` |
   | `xai/` | xAI | `XAI_API_KEY` |

3. Any other name: whichever key is set, checking Anthropic, then OpenAI,
   then xAI.

Because the model name decides, you can keep **all** your keys exported at
once. They don't conflict.

> **Your setup:** the `claw` command in `~/.local/bin` is a wrapper script that
> points `local/` models at the `llama-gpu` server (`CLAW_LOCAL_BASE_URL`) and
> unsets `OLLAMA_HOST`, so every other model name goes to its cloud provider.
> Inside the REPL, `/model` switches between local and API models (see
> section 6); exporting the keys (section 2) is all a cloud model needs.

## 2. Store your API keys

`claw` reads keys from environment variables first, then from the provider
saved by `claw setup`.

**Option A: `claw setup`** (one provider). The wizard asks for the provider,
key, base URL and model, and saves them to `~/.claw/settings.json` (owner-only
permissions). On every start, `claw` uses the saved key for any variable you
haven't set yourself. Only one provider can be saved this way, and the saved
model becomes the default for plain `claw` as well. Under the Ollama wrapper
that's not what you want, so prefer option B there.

**Option B: environment variables** (any number of providers). Keep the
keys in one private file:

```bash
cat > ~/.config/claw-keys.env <<'EOF'
export ANTHROPIC_API_KEY="sk-ant-..."
export OPENAI_API_KEY="sk-..."
# export XAI_API_KEY="xai-..."
# export DASHSCOPE_API_KEY="sk-..."
EOF
chmod 600 ~/.config/claw-keys.env
echo '[ -f ~/.config/claw-keys.env ] && . ~/.config/claw-keys.env' >> ~/.bashrc
. ~/.bashrc
```

Only add the keys you have. Never commit this file or paste keys into
`.claw.json`.

Which variable to use:

| You have | Put it in |
|---|---|
| Anthropic API key (`sk-ant-…`) from console.anthropic.com | `ANTHROPIC_API_KEY` |
| Bearer token from a proxy or OAuth flow | `ANTHROPIC_AUTH_TOKEN` (not `ANTHROPIC_API_KEY`) |
| OpenAI key (`sk-…`) | `OPENAI_API_KEY` |
| OpenRouter key (`sk-or-v1-…`) | `OPENROUTER_API_KEY` (models `openrouter/<vendor>/<model>`) |
| xAI key | `XAI_API_KEY` |
| DashScope key | `DASHSCOPE_API_KEY` |

A Claude.ai subscription login does **not** work; you need an API key.

## 3. Provider recipes

Each recipe starts from a shell where `OLLAMA_HOST` is **not** set. The
`env -u OLLAMA_HOST` prefix guarantees that for one command.

### Claude (Anthropic)

```bash
env -u OLLAMA_HOST ~/.local/lib/claw/claw --model claude-sonnet-5-5
```

| Model | Use |
|---|---|
| `claude-opus-5-5` | Most capable |
| `claude-sonnet-5-5` | Best balance for coding (recommended) |
| `claude-haiku-5-5` | Fastest, cheapest |

The short aliases `opus`, `sonnet` and `haiku` still point at older 4.x
models; use the full names above for the current ones.

### OpenAI

```bash
env -u OLLAMA_HOST -u OPENAI_BASE_URL ~/.local/lib/claw/claw --model gpt-4.1
```

Use any OpenAI model name that starts with `gpt-` (e.g. `gpt-4.1-mini`,
`gpt-5.4`), or `openai/<name>` for others.

### Z.ai, OpenRouter, DeepSeek, Gemini, Groq, Mistral, NVIDIA

These are built in: each has its own key variable and a model prefix, so they
work side by side with Anthropic and OpenAI. `/login` sets any of them up,
lists the provider's live models (from its `/models` endpoint), and saves the
choice in `~/.claw/settings.json`.

| Provider | Key | Base URL variable (default) | Model |
|---|---|---|---|
| Z.ai (GLM) | `ZAI_API_KEY` | `ZAI_BASE_URL` (`https://api.z.ai/api/paas/v4`) | `zai/glm-4.6` |
| OpenRouter | `OPENROUTER_API_KEY` | `OPENROUTER_BASE_URL` (`https://openrouter.ai/api/v1`) | `openrouter/anthropic/claude-sonnet-4` |
| DeepSeek | `DEEPSEEK_API_KEY` | `DEEPSEEK_BASE_URL` (`https://api.deepseek.com/v1`) | `deepseek/deepseek-v4-pro` |
| Google Gemini | `GEMINI_API_KEY` | `GEMINI_BASE_URL` (`https://generativelanguage.googleapis.com/v1beta/openai`) | `gemini/gemini-2.5-pro` |
| Groq | `GROQ_API_KEY` | `GROQ_BASE_URL` (`https://api.groq.com/openai/v1`) | `groq/llama-3.3-70b-versatile` |
| Mistral | `MISTRAL_API_KEY` | `MISTRAL_BASE_URL` (`https://api.mistral.ai/v1`) | `mistral/mistral-large-latest` |
| NVIDIA NIM | `NVIDIA_API_KEY` | `NVIDIA_BASE_URL` (`https://integrate.api.nvidia.com/v1`; self-hosted NIM: `http://<host>:8000/v1`) | `nvidia/qwen/qwen3-coder-480b-a35b-instruct` |

The prefix is removed before the request is sent (OpenRouter receives
`anthropic/claude-sonnet-4`). Bare names (`glm-4.6`, `deepseek-v4-pro`,
`gemini-2.5-pro`, `mistral-large-latest`) also route there, but only while
that provider's key is set. Model ids change often: `/login` shows the
current ones. For Z.ai's GLM Coding Plan set
`ZAI_BASE_URL=https://api.z.ai/api/coding/paas/v4`; in China,
`https://open.bigmodel.cn/api/paas/v4`.

```bash
env -u OLLAMA_HOST ZAI_API_KEY=... ~/.local/lib/claw/claw --model zai/glm-4.6
```

A model without a known output limit on these providers asks for 32K tokens
(8K on Groq and Mistral); a reply that reaches it is continued automatically.

### xAI (Grok)

Use the `xai/` prefix (bare `grok…` names are rejected with a hint):

```bash
env -u OLLAMA_HOST ~/.local/lib/claw/claw --model xai/grok-3
```

(Or use Grok through OpenRouter: `--model openrouter/x-ai/grok-3`.)

### Alibaba DashScope (Qwen, Kimi)

Use the `qwen/` or `kimi/` prefix (bare `qwen-plus` is rejected):

```bash
env -u OLLAMA_HOST ~/.local/lib/claw/claw --model qwen/qwen-plus
env -u OLLAMA_HOST ~/.local/lib/claw/claw --model kimi/kimi-k2.5
```

### Any other OpenAI-compatible service (Together, Fireworks, a remote vLLM, LM Studio…)

Give it its own prefix in `~/.claw/settings.json` (or the project's
`.claw/settings.json`), so it works side by side with `local/` (your
llama-gpu server) and every built-in provider:

```json
"customProviders": {
  "together": { "baseUrl": "https://api.together.xyz/v1", "apiKeyEnv": "TOGETHER_API_KEY" },
  "gpubox":   { "baseUrl": "http://gpu-box:8000/v1", "apiKey": "token-abc", "label": "vLLM on gpu-box" }
}
```

```bash
claw --model together/meta-llama/Llama-3.3-70B-Instruct-Turbo
claw --model gpubox/Qwen/Qwen3-32B
```

- The prefix is removed before sending (`together/meta-llama/…` is sent as
  `meta-llama/…`).
- The key comes from `apiKeyEnv` (default `<NAME>_API_KEY`, here
  `TOGETHER_API_KEY` / `GPUBOX_API_KEY`), else `apiKey` written in the
  entry. Prefer the variable: settings files get copied around.
- `<NAME>_BASE_URL` overrides `baseUrl`; `maxOutputTokens` sets the reply
  budget (default 8192); `label` is the name `/login` shows.
- Names of built-in providers and `local` are reserved; such entries are
  ignored.
- `/model gpubox/Qwen/Qwen3-32B` switches to it inside a session; `local/`
  models keep going to your local server.

`local/` with `CLAW_LOCAL_BASE_URL` (and `CLAW_LOCAL_API_KEY`) still works for
a single extra endpoint, but then `local/` no longer reaches llama-gpu.

## 4. Switch with one word

Add these to `~/.bashrc`, then run `. ~/.bashrc`:

```bash
# Cloud providers: the real binary, without OLLAMA_HOST.
CLAW_BIN="$HOME/.local/lib/claw/claw"
alias claw-claude='env -u OLLAMA_HOST "$CLAW_BIN" --allow-broad-cwd --model claude-sonnet-5-5'
alias claw-opus='env -u OLLAMA_HOST "$CLAW_BIN" --allow-broad-cwd --model claude-opus-5-5'
alias claw-openai='env -u OLLAMA_HOST -u OPENAI_BASE_URL "$CLAW_BIN" --allow-broad-cwd --model gpt-4.1'
```

Then:

| Command | Runs |
|---|---|
| `claw` | Ollama, your default local model (unchanged) |
| `claw-claude` | Claude Sonnet |
| `claw-opus` | Claude Opus |
| `claw-openai` | OpenAI GPT-4.1 |

The aliases take the usual flags and prompts: `claw-claude "explain main.rs"`,
`claw-claude --resume latest`.

## 5. Switch back to Ollama

Just run `claw`. The wrapper sets `OLLAMA_HOST`, so every request goes to
Ollama again, using the `model` in `~/.claw/settings.json` (currently
`huihui_ai/qwen3.5-abliterated:4b-Claude`). Nothing you did with a cloud
provider changes this.

If you exported `OLLAMA_HOST` yourself or use the real binary directly:

```bash
export OLLAMA_HOST=http://127.0.0.1:11434
~/.local/lib/claw/claw --model huihui_ai/qwen3.5-abliterated:4b-Claude
```

To use a different local model for one run: `claw --model hf.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive:Q4_K_M`.

## 6. Change the default model

The model is chosen in this order: `--model` flag, then the `CLAW_MODEL`
environment variable, then `"model"` in settings.

```bash
claw --model qwen3-admin                 # this run only
CLAW_MODEL=claude-sonnet-5-5 claw-claude # same, via environment
```

To change the permanent default, edit `"model"` in `~/.claw/settings.json`:

```json
{ "model": "huihui_ai/qwen3.5-abliterated:4b-Claude" }
```

Keep a **local** model there, since plain `claw` always goes to Ollama, and
pick cloud models with the aliases above.

Short names of your own can go in settings too:

```json
{ "aliases": { "fast": "claude-haiku-5-5", "smart": "claude-opus-5-5" } }
```

Inside the REPL, `/model <name>` switches models, and providers, for the rest
of the session; the history carries over. Under the `claw` wrapper:

```text
/model coder          # local/qwen36-35b-a3b on llama-gpu (alias; the default)
/model uncensored     # local/qwen35-4b-uncensored on llama-gpu (alias)
/model sonnet         # Anthropic, needs ANTHROPIC_API_KEY
/model gpt-4.1        # OpenAI, needs OPENAI_API_KEY
```

The local models are the sections of `~/.config/llama-gpu/models.ini`;
`llama-gpu` runs llama.cpp in router mode, which loads the model a request
names and unloads the previous one (one at a time on the 4 GB GPU). To add a
local model, add a section to `scripts/wrappers/llama-models.ini`, run
`scripts/install-wrappers.sh` and `systemctl --user restart llama-gpu`.

## 6b. Start and stop a model server with claw

claw can start a provider's server when it starts and stop it when the last
claw using it exits (terminal sessions, desktop chats, sub-agents all count).
Configure it per provider in `~/.claw/settings.json`, keyed by the model's
prefix (`local` for `local/...`, `ollama`, ...; `default` matches any
model). Cloud APIs need no entry.

```json
"modelServers": {
  "local": {
    "start": "systemctl --user start llama-gpu",
    "stop": "systemctl --user stop llama-gpu",
    "health": "http://127.0.0.1:8080/health",
    "stopOnExit": true
  }
}
```

- `start` runs only when `health` does not answer (without `health`, on
  every first use). claw waits up to 20 s for `health` to come up.
- The entry only applies while the provider points at that server: when
  `local/` models are sent elsewhere (`CLAW_LOCAL_BASE_URL` set to NVIDIA,
  Together, a remote vLLM, ...), its host and port differ from `health` and
  claw leaves `llama-gpu` alone. Cloud providers (Anthropic, OpenAI, Z.ai,
  DeepSeek, DashScope, ...) never match an entry unless you add one.
- `stopOnExit: false` starts the server but leaves it running.
- A launcher can override the entry for the provider in use:
  `CLAW_SERVER_START`, `CLAW_SERVER_STOP`, `CLAW_SERVER_HEALTH`,
  `CLAW_SERVER_STOP_ON_EXIT=0`; `CLAW_MANAGE_SERVER=0` turns it off.
- Each claw holds a lease in `~/.claw/run/model-server/<provider>/`. A claw
  that exits normally stops the server if it was the last; one that is
  killed or loses its terminal is noticed by a small watcher
  (`claw __model-server-watch`) within about 15 s.
- With this, the `llama-gpu` service does not need to start at login
  (`systemctl --user disable llama-gpu`). Its models also unload after 15
  idle minutes (`sleep-idle-seconds = 900` in `llama-models.ini`) and load
  again on the next request.

## 7. Check what you're connected to

```bash
claw-claude status          # shows the model
claw-claude doctor          # checks credentials and setup
claw-claude prompt "reply with the word ready"
```

The REPL banner prints the connected model on start.

## 8. Differences to expect

| | Local (Ollama, your GTX 1650) | Cloud (Claude, OpenAI, …) |
|---|---|---|
| Speed | ~10 tokens/s; ~30 s to read claw's prompt | Much faster |
| Context | 16k tokens | 128k–1M tokens |
| Cost | Free | Paid per token; `/usage` and `/cost` show estimates |
| Tools | Wrapper sends a core set (shell, files, search, web, todo) | All tools |
| Timeout | 30 min idle wait | 5 min idle wait |
| Privacy | Stays on your machine | Prompts and files you share go to the provider |

The cost shown for local models is not meaningful (it uses Claude prices).

## 9. Troubleshooting

| Message | Cause | Fix |
|---|---|---|
| `missing Anthropic credentials` / `missing OpenAI credentials` | Key not in this shell's environment | `. ~/.config/claw-keys.env`, check `echo ${ANTHROPIC_API_KEY:0:7}` |
| `model 'claude-…' not found` (404) | `OLLAMA_HOST` is set, so it went to Ollama | Use `claw-claude` or `env -u OLLAMA_HOST …` |
| `401 Invalid bearer token` | `sk-ant-…` key placed in `ANTHROPIC_AUTH_TOKEN` | Move it to `ANTHROPIC_API_KEY` |
| `401` / `invalid api key` from OpenAI or a gateway | Wrong key, or `OPENAI_BASE_URL` points at a different service | Check both; unset `OPENAI_BASE_URL` for OpenAI itself |
| Request went to Anthropic instead of your gateway | Model name has no routing prefix and `ANTHROPIC_API_KEY` is set | Use `local/<model>` or `openai/<model>` |
| `invalid model syntax` | Bare local name without `OLLAMA_HOST`, or a bare `grok…` / `qwen-…` / `kimi` name | Use the `claw` wrapper for local models; `xai/…`, `qwen/…`, `kimi/…` for cloud ones |
