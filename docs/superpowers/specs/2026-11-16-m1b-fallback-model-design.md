# M1b — Fallback model (phase 1: API)

**Date:** 2026-11-16 · **Roadmap:** M1b · **Depends on:** M1 ([spec](2026-11-14-m1-model-selection-design.md)), M1c

## Intent

The supervisor runs on a local model in the managed `llama.cpp` container. When that model cannot be
loaded or stops answering, the supervisor goes blind: blocked agents get no evaluation. M1b lets the
user pick a second model that takes over automatically in that case, so supervision keeps running.

Phase 1 uses a remote OpenAI-compatible API as the fallback (cheap to build, the client already
exists). If it works well, phase 2 makes the fallback a second local model in a container.

## Decisions taken with the user

1. **Triggers** (phase 1): the primary **load fails**, or the primary **health check fails**.
   Repeated evaluation/JSON errors are *not* a trigger yet (deferred).
2. **Return to the primary only when the user asks.** Never automatic.
3. **Visibility:** the `LLM` pill shows the fallback as `<model> (Fallback)`.
4. **Fallback is an API model** in phase 1.
5. **Return is available both in the mod and in the CLI.**
6. **The fallback is chosen in the Inspector's "Supervisor model" card**, the same card that chooses
   the primary, with the two roles made explicit (approach A below).

## Goals

- Configure one fallback API model (endpoint + model name; key from env/config).
- Switch to it automatically on a primary load failure or health-check failure.
- Show clearly, in the pill and in the card, which model is evaluating and why.
- Return to the primary on request (mod button or CLI); if the primary still fails, stay on fallback
  and say so.

## Non-goals

- Repeated evaluation/JSON errors as a trigger.
- Automatic return to the primary.
- A local (container) fallback model — phase 2. The reserved `[llm_container] fallback_model_id`
  stays unread in phase 1.
- Fallback for the non-container backends (`TCHELVIS_LLM_CONTAINER=off`: Ollama, LM Studio, cloud
  as primary). *(Builder-facing call: the M1 picker is container-only, and these backends have no
  load/health lifecycle of ours to watch.)*
- Typing or storing the API key from the mod.
- A chain of several fallbacks.

## Anti-goals (failure even if it "works")

- **Terminal output goes to a remote API without the user having configured it.** The fallback is
  off until the user sets an endpoint + model and a key exists. No default endpoint.
- The fallback activates because the user *chose* to stop the model (Unload, zero-idle unload,
  Paused mode). Only a failure of a model that should be running counts.
- The user cannot tell which model is answering, or which entry in the card is primary vs fallback.
- The API key appears in the snapshot, the mod, logs, `status --json`, or `supervisor.toml` written
  by us.
- Flapping: switching back and forth on its own.

## Approaches considered

| Approach | Trade-off | Outcome |
| :-- | :-- | :-- |
| **A. Two sections in the card**: `PRINCIPAL (local)` list as today; `FALLBACK (API)` section with the configured endpoint · model, a `Trocar` action listing the endpoint's `/v1/models` (with the existing text filter), state, and `Voltar ao principal`. | Most mod work; matches the user's request; extends naturally to phase 2 (local rows gain "Usar como fallback"). | **Chosen** |
| B. Single list, two buttons per row (Principal / Fallback). | Simplest, but only for local models — no place for an API fallback in phase 1. | Rejected |
| C. Card only displays the fallback; choice via CLI. | Cheapest; contradicts "choose in the card". | Rejected |

The CLI exists in every approach because the mod drives the CLI.

## Design

### Configuration (`supervisor.toml`)

New table *(names are the builder's to finalise; shape is the decision)*:

```toml
[fallback]
host  = "https://openrouter.ai/api"   # OpenAI-compatible base URL; no default
model = "openai/gpt-4.1-mini"
```

- Fallback is **configured** iff `host` and `model` are non-empty **and** a key resolves.
- **Key storage (user decision):** a key file `~/.config/tchelvis/fallback_api_key`, mode `0600`,
  written by `tchelvis supervisor fallback key` reading **stdin** (never an argv value, so it stays
  out of shell history). Reason: ticks started by the Herdr Desktop app do not reliably inherit the
  user's shell environment (same reason the GPU toggle has its own file), so an env-only key would
  make the mod and the CLI disagree.
- Key resolution order: `TCHELVIS_FALLBACK_API_KEY` env → key file → `TCHELVIS_LLM_API_KEY` →
  the **provider key that matches the fallback host** (user request: also read DeepSeek and
  OpenCode Go keys besides the existing three):

  | Host contains | Env var |
  | :-- | :-- |
  | `openai.com` | `OPENAI_API_KEY` |
  | `openrouter.ai` | `OPENROUTER_API_KEY` |
  | `deepseek.com` | `DEEPSEEK_API_KEY` |
  | `opencode.ai` (OpenCode Go, `https://opencode.ai/inference/go/openai/v1`, verified 2026-11-17) | `OPENCODE_API_KEY` |

  *(My call: a provider key is only sent to its own provider's host — never an OpenAI key to
  DeepSeek, etc. Sending terminal output still requires the user to set `host` + `model`
  explicitly, so an exported key alone never enables the fallback.)* The same host-matched table
  replaces today's blind chain in `resolve_api_key_from_env` for the primary
  `openai-compatible` backend, so the two paths agree. Keys are never written to the TOML,
  snapshot, mod, or logs.
- OpenCode Go serves some models only in Anthropic format (e.g. MiniMax); phase 1 speaks only
  `/v1/chat/completions`, so such a model fails like any other evaluation error. *(Known limitation;
  the builder may filter them out of `fallback models` if the listing marks them.)*
- Writes go through `config_write.rs` (`toml_edit`, atomic rename), like model selection.

### Runtime state (shared between daemon and CLI)

The `watch` daemon holds config and state in memory and never reloads them; the app also runs ticks
via `tchelvis supervisor tick`, possibly at the same time as `watch`. To let a CLI/mod action reach a
running daemon, the fallback state lives in its own small file, e.g.
`~/.local/state/tchelvis/supervisor_fallback.json`, **read at the start of every tick** and updated
read-modify-write under an exclusive advisory lock, then written atomically *(my call: both writers
can run concurrently)*:

```json
{ "primary_intended": true, "health_failures": 1,
  "active": true, "since": 1763260000, "reason": "health", "detail": "/health sem resposta há 24 s" }
```

- `primary_intended` — "the primary should be running". Set by a successful or attempted primary
  start (Load, model switch, GPU switch); cleared by Unload LLM and by zero-idle unload. *(My call,
  from the builder check: today `tick` sets `model_loaded = false` whenever the container is not
  Running, so a crashed container looks identical to an Unload; `model_loaded` cannot be the
  trigger signal.)*
- `health_failures` — consecutive failed probes, persisted so the 2-tick grace works across
  separate `tick` processes.

`reason` ∈ `load_failed` | `health`. The `[fallback]` config is also re-read each tick (cheap) so a
newly chosen fallback applies without restarting `watch`. *(My call.)*

### Triggers

- **Load failed:** any primary start that fails — `load_model()`, and the restarts in `set_gpu`,
  model selection (`recreate_container_if_running`) and the other `wait_healthy` call sites in
  `src/supervisor/mod.rs` *(my call, from the builder check: otherwise a failed model/GPU switch
  leaves the supervisor blind)*. If the fallback is configured, activate it (`load_failed`); the
  command still reports the original error, plus "fallback ativo". If not configured, behaviour is
  unchanged.
- **Health failed:** in `tick`, only while `primary_intended == true`, mode not Paused, and
  sessions > 0: failure = container not Running, or `GET {host}/health` not 200. Activate after
  **2 consecutive failed ticks** *(my call: one tick of grace avoids switching on a single slow
  probe; ~24 s at the default 12 s poll)*. Intentional stops clear `primary_intended` and
  therefore never trigger.
- While `active`, the primary is not probed and not restarted.

### What ends the fallback (user decision)

| Event | Effect |
| :-- | :-- |
| `Voltar ao principal` / `fallback off` | Try the primary; leave fallback only if it loads. |
| **Load LLM** while fallback active | Same as `Voltar ao principal`. |
| **Unload LLM** while fallback active | Fallback deactivated too; nothing evaluates (user asked to stop). |
| **Zero-idle** (no sessions for `idle_unload_secs`) | Fallback deactivated, same as unloading the primary; `primary_intended` cleared. |
| **Paused** | Fallback stays active but nothing is evaluated while paused. |
| Fallback evaluation errors | Do not deactivate it (recorded in `last_eval_error`). |

### Evaluation while active

`tick` uses the fallback client for every evaluation: the existing OpenAI-compatible path
(`/v1/chat/completions`, `Authorization: Bearer`, JSON schema) with the fallback's host, model and key.
Same guardrails, same `max_evals_per_tick`, same audit log; the audit entry records which model
answered. There is nothing to unload on the API side; zero-idle ends the fallback instead (table below).

A fallback evaluation error is recorded in `last_eval_error` as today; it does not deactivate the
fallback.

### Return to the primary (user-initiated)

`tchelvis supervisor fallback off` (and the mod button `Voltar ao principal`): run `load_model()` for
the primary. On success clear the state file (`active: false`). On failure keep the fallback active,
update `detail`, and return an error message the mod shows in the card. Running `load` while the
fallback is active behaves the same way.

### CLI

```
tchelvis supervisor fallback status [--json]   # configured?, active?, reason, model, host
tchelvis supervisor fallback set --host <url> --model <name>
tchelvis supervisor fallback key               # reads the key from stdin → key file (0600)
tchelvis supervisor fallback models [--json] [--filter <text>]   # GET {host}/v1/models with the key
tchelvis supervisor fallback off               # try primary; see above
tchelvis supervisor fallback clear             # remove [fallback] config (and deactivate)
```

`models` errors clearly when no key or the endpoint refuses (401/404). No forced activation command
*(my call: activation is only by failure, keeping it honest; tests cover activation directly)*.

### Snapshot (`DesktopModSnapshot` / `status --json`)

Add: `fallback_configured`, `fallback_active`, `fallback_model`, `fallback_host`, `fallback_reason`,
`fallback_detail`, `fallback_since`. Never the key. While active, `llm_model` (or the field the pill
reads) reflects the fallback model and a flag marks it as fallback.

### Mod

**Pill** (`main.js` ~L2603): while active → `● LLM: <fallback model> (Fallback)`. Colour: warning
tone, so it reads as degraded, not normal. *(My call.)*

**"Supervisor model" card** — two labelled sections:

```
🧠 Supervisor model
 ── PRINCIPAL (local) ───────────────────────
  ● lfm3:1.7b             [em uso]
    qwen3:4b              [Usar]
 ── FALLBACK (API) ──────────────────────────
  openrouter.ai · openai/gpt-4.1-mini   [Trocar]
  Em espera            (or)   ATIVO — health falhou 14:02  [Voltar ao principal]
```

- When the fallback is active, the primary row reads `falhou` (not `em uso`).
- The pill's `▶ Load LLM` while the fallback is active acts as `Voltar ao principal`; `⏻ Unload LLM`
  ends both (see table above).
- Not configured: the section says how (`tchelvis supervisor fallback key` / no endpoint yet) and
  offers an endpoint field + `Trocar` once a key exists. *(Endpoint entry UI is the builder's call,
  within GPUI text-input limits; a CLI hint is acceptable if a text field is impractical.)*
- `Trocar` lists `fallback models` with the existing filter; choosing one runs `fallback set`.
- `Voltar ao principal` shows progress (load can take up to 90 s) and the error on failure.

## Tests (TDD, lowest level that proves it)

- Trigger logic as a pure function: (expected-running, probe results history, mode) → activate? —
  covers the 2-tick grace, intentional unload, Paused, zero sessions.
- `load_model` with failing start/health (injected) → fallback active with `load_failed`; unconfigured
  → error unchanged.
- State file round-trip; daemon picks up `off` written by another process (two engines, one file).
- `fallback off` success clears state; failure keeps it and reports.
- Evaluation routes to the fallback client when active (fake backends).
- Key never present in snapshot JSON / status output.
- `models` parsing of an OpenAI `/v1/models` response; 401 handling.
- Mod: `node --check`; manual host validation of pill and card.

## Left to the builder

Exact TOML key/field names, file name of the state file, how the fallback client is composed with the
primary (`CognitiveBackend` wrapper vs. two clients in the engine), endpoint input UI, and copy text,
within the decisions above.

## Phase 2 (not in this spec)

Local fallback model in a container (`[llm_container] fallback_model_id`), picker rows gain
"Usar como fallback", and possibly repeated eval/JSON errors as a trigger.
