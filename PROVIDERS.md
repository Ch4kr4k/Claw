# Switching Model Providers

How to point `claw` at Claude (Anthropic), OpenAI, OpenRouter, xAI, DashScope
or any other OpenAI-compatible service, and how to switch back to your local
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
> always sets `OLLAMA_HOST`, so it can only talk to Ollama. To use a cloud
> provider, run the real binary, `~/.local/lib/claw/claw`, without
> `OLLAMA_HOST`. Section 4 sets up short commands for that.

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
| OpenRouter key (`sk-or-v1-…`) | `OPENAI_API_KEY`, plus `OPENAI_BASE_URL=https://openrouter.ai/api/v1` |
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

### OpenRouter (many providers, one key)

```bash
env -u OLLAMA_HOST OPENAI_BASE_URL=https://openrouter.ai/api/v1 \
  OPENAI_API_KEY="$OPENROUTER_API_KEY" \
  ~/.local/lib/claw/claw --model openai/gpt-4.1-mini
```

For OpenRouter models from other vendors, prefix the slug with `local/` so
`claw` doesn't route it to that vendor's own API:
`--model local/anthropic/claude-sonnet-4` or
`--model local/meta-llama/llama-3.3-70b-instruct`.

### xAI (Grok)

Use the `xai/` prefix (bare `grok…` names are rejected with a hint):

```bash
env -u OLLAMA_HOST ~/.local/lib/claw/claw --model xai/grok-3
```

(Or use Grok through OpenRouter: `--model local/x-ai/grok-3`.)

### Alibaba DashScope (Qwen, Kimi)

Use the `qwen/` or `kimi/` prefix (bare `qwen-plus` is rejected):

```bash
env -u OLLAMA_HOST ~/.local/lib/claw/claw --model qwen/qwen-plus
env -u OLLAMA_HOST ~/.local/lib/claw/claw --model kimi/kimi-k2.5
```

### Any other OpenAI-compatible service (DeepSeek, Groq, Together, vLLM, LM Studio…)

Set the service's base URL and key, and put `local/` in front of its model
name:

```bash
env -u OLLAMA_HOST OPENAI_BASE_URL=https://api.deepseek.com/v1 \
  OPENAI_API_KEY="$DEEPSEEK_API_KEY" \
  ~/.local/lib/claw/claw --model local/deepseek-chat
```

`local/` forces the OpenAI-compatible client and is removed before the
request is sent, so the service receives `deepseek-chat`.

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

Inside the REPL, `/model <name>` switches models for the session, but not
providers: under the `claw` wrapper it still goes to Ollama. Start
`claw-claude` (or another alias) to change provider.

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
