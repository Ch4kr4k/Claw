# Local model benchmark: Qwen3.6-35B-A3B vs Qwen3-Coder-30B-A3B

**Date:** 2026-10-17
**Result:** Qwen3.6-35B-A3B UD-Q4_K_XL became the llama-gpu / claw default. The Qwen3-Coder-30B-A3B GGUF was deleted.
**Judged on:** answer quality plus total time to a finished answer, not tok/s alone.

## 1. Setup

### Hardware

| | |
|---|---|
| GPU | NVIDIA GTX 1650 Mobile, 4 GB, PCIe Gen3; driver 595.104.02, CUDA 13.2 |
| CPU | Intel i5-10300H, 4C/8T, AVX2 |
| RAM | 2x16 GB DDR4, running at 2933 MT/s |
| OS | Gentoo, kernel 7.2.9-gentoo-dist |

### Models

| Id in llama-gpu | File | Size |
|---|---|---|
| `qwen3coder30A3B` | `Qwen3-Coder-30B-A3B-Instruct-UD-Q4_K_XL.gguf` (qwen3moe, 48 layers) | 18 GB |
| `qwen36-35b-a3b` | `Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf` from `unsloth/Qwen3.6-35B-A3B-GGUF` (qwen35moe: hybrid Gated DeltaNet + attention, 40 layers, 256 experts) | 22.4 GB, sha256 `707a55a8…4f450` (matches the Hub) |

### llama.cpp builds

| Build | Tree | Revision |
|---|---|---|
| Stock | `~/ws/src/cpp/nn/llama` | `781dbc5`, clean |
| Fork (async weight prefetch) | `~/ws/src/cpp/nn/llama-abominated` | branch `async-overlap`, `1b57fd5` plus the working tree of that day; `GGML_SCHED_PREFETCH` at its default of 2 slots unless noted |

Both are full CUDA builds (`-DGGML_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES=75 -DGGML_NATIVE=ON -DGGML_CUDA_FA=ON -DGGML_CUDA_GRAPHS=ON -DBUILD_SHARED_LIBS=ON`).

### Configuration

- **`llama-bench`:** `-ngl 99 -ncmoe 99 -fa 1 -ctk q8_0 -ctv q8_0 -t 4 -b 2048 -p 2048 -n 64 -d 0,16384 -r 2`, weights mmapped. This puts every layer on the GPU and every expert in RAM.
  - ubatch: Qwen3-Coder `-ub 2048`; Qwen3.6 `-ub 1024`, because 2048 runs out of VRAM even with all experts in RAM.
- **Server tests:** run through `llama-gpu` (router mode, `-c 32768`, `--models-max 1`, `parallel 1`, `threads 4`, q8_0 KV cache, `flash-attn auto`), on the **stock** build.
  - Qwen3-Coder preset: `n-cpu-moe 48`, `ub 2048`, temp 0.7, top-p 0.8, top-k 20, min-p 0, repeat-penalty 1.05.
  - Qwen3.6 preset: `n-cpu-moe 40`, `ub 1024`, temp 0.6, top-p 0.95, top-k 20, min-p 0, presence 0, repeat-penalty 1.0. This is Qwen's thinking-mode setting for precise coding.
  - Qwen3.6 runs with thinking on (the template default) unless marked "no thinking".
- **`max_tokens`:** 12000 per task; `cache_prompt` off for the task suite.

## 2. Speed (`llama-bench`, tok/s)

| Test | Coder, stock | Coder, fork | Qwen3.6, stock (ub 1024) | Qwen3.6, fork (ub 768) |
|---|---|---|---|---|
| pp2048 | 120.10 ± 0.16 | **130.08 ± 0.15** | 92.14 ± 2.13 | 96.75 (1 run) |
| tg64 | **20.33 ± 0.39** | 18.89 ± 2.04 | 19.43 ± 0.14 | 21.12 (tg32, 1 run) |
| pp2048 @ d16384 | 34.81 ± 0.06 | 34.99 ± 0.01 | **67.11 ± 1.16** | 67.20 (1 run) |
| tg64 @ d16384 | 10.80 ± 0.00 | 10.75 ± 0.03 | **17.28 ± 0.08** | 17.88 (tg32, 1 run) |

- Qwen3-Coder's numbers match the run from 2026-10-16 (stock 120.29 / 20.26 / 34.67 / 10.44; fork 129.56 / 20.68 / 35.50 / 10.76).
- **The fork with Qwen3.6 at ub 1024:** pp2048 118.05 and tg32 22.71 at depth 0, then a **CUDA out-of-memory crash at 16k depth**. The prefetch slot buffer takes 420 MiB (2 slots), on top of a 1422 MiB compute buffer and 2039 MiB of weights; the failing allocation was 1002 MiB. ub 768 works. Fixing this was handed to a separate session.
- **Qwen3.6 ubatch probe, stock, pp2048 at depth 0:** ub 1536 out of VRAM, ub 1024 115.9 (single run), ub 512 82.3.

### GPU offload threshold (Qwen3.6, stock, `-ub 1024`, `-n 0 -r 2`)

`GGML_OP_OFFLOAD_MIN_BATCH` sets the smallest batch, in tokens, whose RAM-resident expert matmuls are moved to the GPU. That move first copies the weights over PCIe.

| Prompt tokens | Threshold 32 (default) | Threshold 100000 (experts always on CPU) |
|---|---|---|
| 32 | 27.07 | **57.71** |
| 64 | 28.61 | **62.28** |
| 128 | 29.83 | **69.86** |
| 256 | 45.26 | **75.93** |
| 384 | 60.46 | **80.22** |
| 512 | 70.19 | **84.54** |
| 768 | **89.17** | 83.13 |
| 1024 | **101.43** | 90.37 |

The two lines cross at about 690 tokens, so the wrapper uses **640**.

In the live server with the new setting, a 153-token follow-up turn prefilled in 2.77 s (55.2 tok/s). Before, a ~100-token turn took 7–8 s (~14–15 tok/s, Qwen3-Coder logs).

## 3. Quality: 19-task suite

There are five categories: coding (4), sysadmin (3), security research (3), daily use (3) and tool calling (6).

**How tasks were graded:**
- **Coding:** hidden unit tests. Three Python tasks (an expression parser without `eval`, a TTL+LRU cache, a `bisect_left` bug fix) and a Rust `top_k_frequent`, built with `rustc --test`.
- **Bash script:** run against a test tree with tricky file names.
- **Tool calling:** exact function-name and argument checks through the OpenAI tools API. The tasks cover a single call, parallel calls, choosing the right tool, a date-resolved calendar event, not calling a tool when none is needed, and using a tool result.
- **Everything else:** a keyword rubric first, then every answer read by hand.

| Run | Coding /4 | Sysadmin /3 | Security /3 | Daily /3 | Tools /6 | **Total /19** | Tokens generated |
|---|---|---|---|---|---|---|---|
| Qwen3-Coder | **3.00** | 2.69 | 2.71 | 2.86 | 5.55 | 16.81 | 4,594 |
| Qwen3.6, thinking | 2.00 | **3.00** | 2.86 | **3.00** | **6.00** | **16.86** | 63,424 |
| Qwen3.6, no thinking | 2.00 | 1.90 | **3.00** | **3.00** | **6.00** | 15.90 | 8,443 |

At ~20 tok/s decode, the tokens generated work out to about 4 min of generation for Qwen3-Coder, 54 min for Qwen3.6 with thinking and 7 min without.

### Notable misses

- **Qwen3-Coder:**
  - The Rust task doesn't compile (a `collect()` type error).
  - It put "next Wednesday" on 10-19, which is a Monday.
  - It booked "next Tuesday" for 10-26 instead of 10-20.
  - It didn't flag the hardcoded Flask `secret_key`.
  - It made a redundant repeat tool call after already getting the result.
  - Its inode answer missed deleted-but-open files.
- **Qwen3.6, thinking:**
  - The expression parser (11.9k tokens, 618 s) never converts tokens to numbers.
  - The LRU cache (10.4k tokens, 501 s) doesn't evict when full.
  - Everything else is correct.
- **Qwen3.6, no thinking:**
  - The LRU cache's TTL handling is wrong.
  - The Rust task doesn't compile (a reference/value mismatch).
  - The bash script prints a literal `\t`.
  - The systemd unit uses `$USER` in paths, which systemd doesn't expand.

### Hand-review adjustments to the auto scores

| Run | Task | Auto | Hand |
|---|---|---|---|
| Qwen3-Coder | sec_web_audit (didn't flag the hardcoded secret) | 0.86 | 0.71 |
| Qwen3-Coder | tool_result_use (repeated the tool call) | 1.0 | 0.7 |
| Qwen3.6, no thinking | sys_systemd (broken `$USER` paths) | 0.89 | 0.6 |

A grader bug was fixed before the final scores: string arguments were compared case-sensitively. Both models were regraded.

## 4. Agentic claw task

`claw --model local/<id> prompt …` was run on a small Python project with three planted bugs and one missing method. The task was to make all 7 unit tests pass without editing the test file (30 min limit).

| Model | Result | Wall time | Notes |
|---|---|---|---|
| Qwen3-Coder | 6/7 | 682 s | Fixed all 3 bugs; wrote `low_stock` with `<` instead of `<=`, then declared the test "logically incorrect" and stopped |
| **Qwen3.6, thinking** | **7/7** | **339 s** | Read the files, ran the tests, made 5 small edits, re-ran and fixed the last failure; minimal diff |

## 5. Multi-turn prompt cache (llama-server, `cache_prompt` on)

| Model | Turn 1 | Turn 2 |
|---|---|---|
| Qwen3-Coder | 5,300 tok at 91.2 tok/s | 23 new tokens processed, 2.6 s |
| Qwen3.6 | 5,859 tok at 85.1 tok/s | 31 new tokens processed, 2.4 s |

Both reuse the cache, so the hybrid model's context checkpoints work.

## 6. Decision

Qwen3.6-35B-A3B won on:
- the agentic task: correct, and in half the time;
- an equal suite score, winning sysadmin, security, daily use and tools;
- long-context speed: about 2× prefill and 1.6× decode at 16k depth.

Its cost is token-heavy thinking on one-shot questions, and it is weaker at one-shot Python. Qwen3-Coder is faster on short prompts (pp2048 120–130 vs 92) because it fits ubatch 2048.

### Config applied after the benchmark

- **`~/.config/llama-gpu/models.ini` and `scripts/wrappers/llama-models.ini`:** the `[qwen36-35b-a3b]` section has `load-on-startup = true`; the `[qwen3coder30A3B]` section is removed.
- **`~/.claw/settings.json`:** `model` and the `coder` alias are `local/qwen36-35b-a3b`.
- **`scripts/wrappers/llama-gpu`:**
  - exports `GGML_OP_OFFLOAD_MIN_BATCH=640`;
  - refuses to start while `llama-server --list-devices` shows no CUDA0. At boot on 2026-10-17, CUDA init failed and llama-server ran CPU-only at ~3 tok/s.
- **Build:** llama-gpu ran the stock build (`llama-gpu-switch stock`) until the fork's OOM at ub 1024 was fixed. It was switched back to the fork on 2026-10-18 (section 7).

### Caveats

- Speed numbers are 2 runs each (fork probes 1 run). Differences under ~3% are noise.
- The Qwen3-Coder task suite was answered while llama-server was CPU-only (the boot CUDA failure above). Its quality results stand, but its timings from that run were discarded. Its cache and agentic tests were re-run on the GPU.
- The suite is small (19 tasks, one sample each at the presets' sampling), so a single task can swing a category.

## 7. Update 2026-10-18: llama-gpu back on the fork

A separate session fixed the fork's out-of-memory crash. The prefetch ring is now allocated during reserve, after the compute buffers, and only if at least 320 MiB of VRAM stays free (`GGML_SCHED_PREFETCH_MARGIN`); otherwise prefetch turns itself off. At the preset's ub 1024 it stays off on this card. The fork also gained `--op-offload-min-batch`, `--moe-n-expert` and `-ser`; the details are in the fork's `ASYNC_PREFETCH.md`. llama-gpu was switched with `llama-gpu-switch fork`.

**Configuration:** the same `[qwen36-35b-a3b]` preset as above. `--op-offload-min-batch` is not set, so the fork falls back to the wrapper's `GGML_OP_OFFLOAD_MIN_BATCH=640`. `--moe-n-expert` and `-ser` are not used, because their quality cost has not been measured yet.

Live llama-server smoke test (Qwen3.6, thinking off, `cache_prompt` on):

| | Fork, 2026-10-18 | Stock, 2026-10-17 |
|---|---|---|
| 5.9k-token prompt, prefill | 99.9 tok/s | 85.1 tok/s |
| ~150-token follow-up turn | 2.14 s (69.6 tok/s) | 2.77 s (55.2 tok/s) |
| 20k-token prompt, prefill | 85.1 tok/s (no OOM) | — |
| Decode at 20k depth | 18.0 tok/s | — |

These are single requests through the live server, not `llama-bench`.
