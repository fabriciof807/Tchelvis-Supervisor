# M1b — Fallback Model (Phase 1: API) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the managed local model fails to load or stops answering `/health`, supervision continues on a user-configured OpenAI-compatible API model until the user asks to return to the primary.

**Architecture:** A new `src/supervisor/fallback.rs` owns config, key resolution, the shared state file (flock + atomic write) and the pure trigger logic. `WatcherEngine` gets a `FallbackProvider` trait object (real impl in container mode, no-op otherwise, fake in tests) that it consults every tick to pick the evaluator. Primary-start call sites in `src/supervisor/mod.rs` route their result through one helper that activates the fallback on failure and clears it on success. The mod reads new snapshot fields.

**Tech Stack:** Rust (ureq, serde, toml_edit, libc flock), clap 4, Herdr Desktop mod JS, `node:test`.

**Spec:** `docs/superpowers/specs/2026-11-16-m1b-fallback-model-design.md`

## Global Constraints

- Fallback is **configured** iff `[fallback] host` and `model` are non-empty **and** a key resolves. No default host. An exported provider key alone never enables it.
- Fallback applies only in container mode (`config.container_mode_active()`); other backends get the no-op provider.
- Triggers: primary **start failure** (any `wait_healthy` start path) or **2 consecutive failed health ticks** (container not Running, or `GET {primary}/health` ≠ 200), counted only while `primary_intended && !Paused && sessions > 0 && !active`.
- Never automatic return. Return = `fallback off` / mod `Voltar ao principal` / Load LLM while active → run the normal primary load; success clears `active`, failure keeps it and updates `detail`.
- Unload LLM and zero-idle (no sessions for `idle_unload_secs`) → `active = false`, `primary_intended = false`. Paused → keep `active`, evaluate nothing. Fallback eval errors never deactivate.
- Key order (fallback): `TCHELVIS_FALLBACK_API_KEY` → `~/.config/tchelvis/fallback_api_key` → `TCHELVIS_LLM_API_KEY` → provider env matched by host. Primary `openai-compatible`: `TCHELVIS_LLM_API_KEY` → TOML `api_key` → provider env matched by `effective_primary_host()`.
- Host→env table (substring match on host, case-insensitive): `openai.com`→`OPENAI_API_KEY`, `openrouter.ai`→`OPENROUTER_API_KEY`, `deepseek.com`→`DEEPSEEK_API_KEY`, `opencode.ai`→`OPENCODE_API_KEY`. A provider key is never sent to another host.
- OpenCode Go base URL (verified 2026-11-17 at opencode.ai/docs/go): `https://opencode.ai/inference/go/openai/v1` (Chat Completions models only; Anthropic-/Responses-format models fail like any eval error). Use it in docs/examples, not as a default.
- Key file `~/.config/tchelvis/fallback_api_key`, mode `0600`, written only from **stdin**. The key never appears in TOML we write, snapshot, `status --json`, mod, logs or error messages.
- State file `~/.local/state/tchelvis/supervisor_fallback.json`, read every tick, updated read-modify-write under exclusive `flock` on `supervisor_fallback.json.lock`, written via temp + rename.
- Copy (pt-BR): pill `● LLM: <model> (Fallback)` in warning tone (`t.working`); card sections `PRINCIPAL (local)` / `FALLBACK (API)`; states `Em espera` / `ATIVO — <motivo> <HH:MM>`; button `Voltar ao principal`; primary row `falhou` while active; reasons `load falhou` / `health falhou`.
- Validation (all must pass before handoff): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked --quiet`, `node --check assets/herdr-app/mods/tchelvis/main.js`, `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs`.
- Tests are deterministic: no real network, no real containers; inject env getters, clocks (`now: u64`) and the `FallbackProvider`.

## Review Focus

1. **Container crashes while the user is away (no sessions)** — no probe counting, no activation; when sessions resume, 2 ticks later it activates. Task 2 test `no_counting_without_sessions`.
2. **`watch` daemon running while the user clicks `Voltar ao principal`** — the daemon must see `active=false` on its next tick (state re-read, not cached). Task 4 test `engine_reads_state_written_by_other_writer`.
3. **Fallback configured but the key was removed later** — `backend()` returns `None`; while `active`, evaluations are skipped (LLM unavailable path), not sent without a key; Task 4 test.
4. **One slow probe between healthy ones** (1 failure, then OK) — counter resets, never activates; Task 2 test `single_failure_then_success_resets`.
5. **Error message from a failed provider call containing the `Authorization` header or key** — error strings stored in `last_eval_error`/`detail` must not carry the key; Task 1 `redact_key` applied wherever a fallback error is stored; Task 4 test asserts the key is absent from snapshot JSON after a failing eval whose error text contains it.

---

### Task 1: `fallback.rs` — config, key resolution, key file, state store

**Files:**
- Create: `src/supervisor/fallback.rs` (register `pub mod fallback;` in `src/supervisor/mod.rs`)
- Modify: `src/supervisor/config.rs` — add `#[serde(default)] pub fallback: FallbackConfig` to `SupervisorConfig` (L109-150) and its `Default`; replace `resolve_api_key_from_env` (L226) with host-matched resolution.
- Modify: `src/supervisor/config_write.rs` — `write_fallback`, `clear_fallback`.

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)] pub struct FallbackConfig { pub host: String, pub model: String }` with `fn is_set(&self) -> bool` (both trimmed non-empty).
  - `pub fn provider_env_for_host(host: &str) -> Option<&'static str>`
  - `pub fn resolve_fallback_key(host: &str, key_file: Option<String>, get: impl Fn(&str) -> Option<String>) -> Option<String>` (blank values skipped)
  - `pub fn resolve_primary_key(host: &str, toml_key: Option<String>, get: impl Fn(&str) -> Option<String>) -> Option<String>`
  - `pub fn fallback_key_path() -> PathBuf`, `pub fn read_key_file(path: &Path) -> Option<String>`, `pub fn write_key_file(path: &Path, key: &str) -> Result<()>` (create parent, temp file created with mode 0600 via `OpenOptionsExt::mode`, rename; rejects blank key)
  - `pub fn redact_key(text: &str, key: &str) -> String` (replaces every occurrence with `***`; no-op on empty key)
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")] pub enum FallbackReason { LoadFailed, Health }`
  - `#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)] #[serde(default)] pub struct FallbackState { pub primary_intended: bool, pub health_failures: u32, pub active: bool, pub since: Option<u64>, pub reason: Option<FallbackReason>, pub detail: Option<String> }`
  - `pub fn fallback_state_path() -> PathBuf`, `pub fn load_state(path: &Path) -> FallbackState` (missing/corrupt → default, corrupt logs a warning), `pub fn update_state(path: &Path, f: impl FnOnce(&mut FallbackState)) -> Result<FallbackState>` (flock `<path>.lock` LOCK_EX, load, mutate, temp+rename, unlock)
  - `config_write::write_fallback(path: &Path, host: Option<&str>, model: Option<&str>) -> Result<()>` (sets only given keys in `[fallback]`), `config_write::clear_fallback(path: &Path) -> Result<()>` (removes the table)

- [ ] **Step 1: Write the failing tests** (in `fallback.rs` `mod tests`, and one in `config_write.rs` tests):

```rust
fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + '_ {
    move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
}

#[test]
fn provider_env_matches_host() {
    assert_eq!(provider_env_for_host("https://openrouter.ai/api"), Some("OPENROUTER_API_KEY"));
    assert_eq!(provider_env_for_host("https://api.deepseek.com"), Some("DEEPSEEK_API_KEY"));
    assert_eq!(provider_env_for_host("https://opencode.ai/inference/go/openai/v1"), Some("OPENCODE_API_KEY"));
    assert_eq!(provider_env_for_host("https://API.OPENAI.COM"), Some("OPENAI_API_KEY"));
    assert_eq!(provider_env_for_host("http://127.0.0.1:8080"), None);
}

#[test]
fn fallback_key_order() {
    let all = [("TCHELVIS_FALLBACK_API_KEY", "f"), ("TCHELVIS_LLM_API_KEY", "l"), ("OPENROUTER_API_KEY", "p")];
    let h = "https://openrouter.ai/api";
    assert_eq!(resolve_fallback_key(h, Some("file".into()), env(&all)).as_deref(), Some("f"));
    assert_eq!(resolve_fallback_key(h, Some("file".into()), env(&all[1..])).as_deref(), Some("file"));
    assert_eq!(resolve_fallback_key(h, None, env(&all[1..])).as_deref(), Some("l"));
    assert_eq!(resolve_fallback_key(h, None, env(&all[2..])).as_deref(), Some("p"));
}

#[test]
fn provider_key_never_crosses_hosts() {
    let e = [("OPENAI_API_KEY", "sk-openai")];
    assert_eq!(resolve_fallback_key("https://api.deepseek.com", None, env(&e)), None);
    assert_eq!(resolve_primary_key("https://api.deepseek.com", None, env(&e)), None);
    assert_eq!(resolve_primary_key("https://api.openai.com", None, env(&e)).as_deref(), Some("sk-openai"));
}

#[test]
fn primary_key_order_env_then_toml_then_provider() {
    let e = [("TCHELVIS_LLM_API_KEY", "l"), ("OPENAI_API_KEY", "p")];
    assert_eq!(resolve_primary_key("https://api.openai.com", Some("t".into()), env(&e)).as_deref(), Some("l"));
    assert_eq!(resolve_primary_key("https://api.openai.com", Some("t".into()), env(&e[1..])).as_deref(), Some("t"));
}

#[test]
fn key_file_is_0600_and_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("sub/fallback_api_key");
    write_key_file(&p, "  sk-x \n").unwrap();
    assert_eq!(read_key_file(&p).as_deref(), Some("sk-x"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(write_key_file(&p, "  ").is_err());
}

#[test]
fn redact_key_hides_every_occurrence() {
    assert_eq!(redact_key("Bearer sk-1 / sk-1", "sk-1"), "Bearer *** / ***");
    assert_eq!(redact_key("x", ""), "x");
}

#[test]
fn state_roundtrip_and_update_under_lock() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("supervisor_fallback.json");
    assert_eq!(load_state(&p), FallbackState::default());
    let s = update_state(&p, |s| { s.active = true; s.reason = Some(FallbackReason::Health); }).unwrap();
    assert!(s.active);
    assert_eq!(load_state(&p), s);
    std::fs::write(&p, "not json").unwrap();
    assert_eq!(load_state(&p), FallbackState::default());
}

#[test]
fn fallback_config_is_set_needs_both() {
    assert!(!FallbackConfig { host: "h".into(), model: " ".into() }.is_set());
    assert!(FallbackConfig { host: "h".into(), model: "m".into() }.is_set());
}

// config_write.rs
#[test]
fn write_and_clear_fallback_table() {
    // write host+model into a TOML with other tables; assert [fallback] host/model present and
    // other content preserved; write_fallback(path, None, Some("m2")) changes only model;
    // clear_fallback removes the table; SupervisorConfig parsed from the result has fallback == default.
}
```

- [ ] **Step 2: Run** `cargo test --lib supervisor::fallback supervisor::config_write` — Expected: FAIL (module missing).
- [ ] **Step 3: Implement** the interfaces above. In `config.rs`: delete `resolve_api_key_from_env`; at the end of `SupervisorConfig::load()` (after env overrides) and in `Default`, set `self.api_key = resolve_primary_key(&self.effective_primary_host(), self.api_key.take(), |k| std::env::var(k).ok())` — but in `Default` pass `None` as toml key. Keep `apply_env_overrides` from touching `api_key`.
- [ ] **Step 4: Run** `cargo test --locked --quiet` — Expected: PASS (whole suite: the primary key change must not break existing config tests).
- [ ] **Step 5: Commit** `feat(supervisor): fallback config, host-matched API keys, key file and shared state file`.

### Task 2: Pure trigger logic

**Files:**
- Modify: `src/supervisor/fallback.rs`

**Interfaces:**
- Consumes: `FallbackState`, `FallbackReason` (Task 1).
- Produces:
  - `pub struct ProbeCtx { pub healthy: bool, pub paused: bool, pub sessions: usize, pub configured: bool, pub now: u64, pub detail: String }`
  - `pub fn apply_health_probe(state: &mut FallbackState, ctx: &ProbeCtx) -> bool` — returns `true` iff this call activated the fallback. No-op (and returns false) when `state.active || !state.primary_intended || ctx.paused || ctx.sessions == 0`. Healthy → `health_failures = 0`. Unhealthy → `health_failures += 1` (saturating); if `>= 2 && ctx.configured` → `activate(state, Health, detail, now)`.
  - `pub fn activate(state: &mut FallbackState, reason: FallbackReason, detail: &str, now: u64)` — sets `active`, `reason`, `detail`, `since` (keeps existing `since` if already active), `health_failures = 0`.
  - `pub fn primary_started(state: &mut FallbackState)` — `primary_intended = true; active = false; reason = None; detail = None; since = None; health_failures = 0`.
  - `pub fn primary_stopped(state: &mut FallbackState)` — `primary_intended = false; active = false; reason/detail/since = None; health_failures = 0`.

- [ ] **Step 1: Write the failing tests:**

```rust
fn ctx(healthy: bool) -> ProbeCtx {
    ProbeCtx { healthy, paused: false, sessions: 1, configured: true, now: 100, detail: "/health sem resposta".into() }
}
fn intended() -> FallbackState { FallbackState { primary_intended: true, ..Default::default() } }

#[test] fn two_consecutive_failures_activate() {
    let mut s = intended();
    assert!(!apply_health_probe(&mut s, &ctx(false)));
    assert!(apply_health_probe(&mut s, &ctx(false)));
    assert!(s.active); assert_eq!(s.reason, Some(FallbackReason::Health)); assert_eq!(s.since, Some(100));
}
#[test] fn single_failure_then_success_resets() {
    let mut s = intended();
    apply_health_probe(&mut s, &ctx(false)); apply_health_probe(&mut s, &ctx(true));
    assert!(!apply_health_probe(&mut s, &ctx(false))); assert!(!s.active);
}
#[test] fn intentional_stop_never_triggers() {
    let mut s = FallbackState::default();
    apply_health_probe(&mut s, &ctx(false)); apply_health_probe(&mut s, &ctx(false));
    assert!(!s.active); assert_eq!(s.health_failures, 0);
}
#[test] fn paused_does_not_count() {
    let mut s = intended(); let mut c = ctx(false); c.paused = true;
    apply_health_probe(&mut s, &c); apply_health_probe(&mut s, &c);
    assert!(!s.active); assert_eq!(s.health_failures, 0);
}
#[test] fn no_counting_without_sessions() {
    let mut s = intended(); let mut c = ctx(false); c.sessions = 0;
    apply_health_probe(&mut s, &c); apply_health_probe(&mut s, &c);
    assert_eq!(s.health_failures, 0);
}
#[test] fn unconfigured_counts_but_never_activates() {
    let mut s = intended(); let mut c = ctx(false); c.configured = false;
    apply_health_probe(&mut s, &c); apply_health_probe(&mut s, &c); apply_health_probe(&mut s, &c);
    assert!(!s.active);
}
#[test] fn started_and_stopped_reset_state() {
    let mut s = intended(); activate(&mut s, FallbackReason::LoadFailed, "x", 5);
    primary_started(&mut s); assert!(s.primary_intended && !s.active && s.reason.is_none());
    activate(&mut s, FallbackReason::Health, "y", 6);
    primary_stopped(&mut s); assert!(!s.primary_intended && !s.active);
}
```

- [ ] **Step 2: Run** `cargo test --lib supervisor::fallback` — Expected: FAIL.
- [ ] **Step 3: Implement** the four functions.
- [ ] **Step 4: Run** the same — Expected: PASS.
- [ ] **Step 5: Commit** `feat(supervisor): fallback trigger logic with 2-tick health grace`.

### Task 3: `FallbackProvider` and the live implementation

**Files:**
- Modify: `src/supervisor/fallback.rs`
- Modify: `src/supervisor/ollama.rs` — add `fn model_label(&self) -> String { String::new() }` default method to `CognitiveBackend` (L41); `OllamaClient` returns `self.config.ollama_model.clone()`.

**Interfaces:**
- Consumes: Tasks 1–2; `container::probe_llama_server_health` (`container.rs:844`), `LlmContainer::state()`, `OllamaClient`.
- Produces:

```rust
pub trait FallbackProvider: Send + Sync {
    fn load_state(&self) -> FallbackState;
    fn update_state(&self, f: &mut dyn FnMut(&mut FallbackState)) -> anyhow::Result<FallbackState>;
    /// (healthy, detail) for the primary: container Running AND GET {primary}/health == 200.
    fn probe_primary(&self) -> (bool, String);
    /// Fresh each call: re-reads `[fallback]` from supervisor.toml and resolves the key.
    /// None when not configured. Errors returned by the backend are redacted of the key.
    fn backend(&self) -> Option<Box<dyn CognitiveBackend>>;
}
pub struct NoFallback;            // load_state → default; update_state → Ok(default); probe → (true, ""); backend → None
pub struct LiveFallback { pub state_path: PathBuf, pub primary: SupervisorConfig }
pub fn fallback_client_config(primary: &SupervisorConfig, fb: &FallbackConfig, key: String) -> SupervisorConfig
pub fn fallback_snapshot_info(cfg: &FallbackConfig, key_present: bool, state: &FallbackState) -> FallbackInfo
```

`fallback_client_config`: clone of `primary` with `llm_container.mode = "off"`, `llm_backend = OpenAiCompatible`, `ollama_host = fb.host` (normalized like other hosts), `ollama_fallback_host = None`, `ollama_model = fb.model`, `api_key = Some(key)`. `LiveFallback::backend()` wraps the `OllamaClient` in a small `RedactingBackend { inner, key }` whose `evaluate` maps errors through `redact_key`.

`FallbackInfo` (serde, used by Task 5): `{ configured: bool, key_present: bool, active: bool, host: String, model: String, reason: Option<FallbackReason>, detail: Option<String>, since: Option<u64> }` — `configured = cfg.is_set() && key_present`. No key field.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test] fn fallback_client_config_talks_openai_to_the_fallback_host() {
    let mut primary = SupervisorConfig::default(); primary.llm_container.mode = "on".into();
    let fb = FallbackConfig { host: "https://openrouter.ai/api".into(), model: "openai/gpt-4.1-mini".into() };
    let c = fallback_client_config(&primary, &fb, "sk".into());
    assert!(!c.container_mode_active());
    assert_eq!(c.effective_backend(), LlmBackendKind::OpenAiCompatible);
    assert_eq!(c.effective_primary_host(), "https://openrouter.ai/api");
    assert_eq!(c.ollama_model, "openai/gpt-4.1-mini");
    assert_eq!(c.ollama_fallback_host, None);
}
#[test] fn redacting_backend_strips_key_from_errors() {
    // inner fake backend returning Err(anyhow!("401 for Bearer sk-secret")) → message has "***", not "sk-secret"
}
#[test] fn snapshot_info_has_no_key_and_requires_key_for_configured() {
    let cfg = FallbackConfig { host: "h".into(), model: "m".into() };
    let info = fallback_snapshot_info(&cfg, false, &FallbackState::default());
    assert!(!info.configured);
    let json = serde_json::to_string(&fallback_snapshot_info(&cfg, true, &FallbackState::default())).unwrap();
    assert!(!json.contains("key\":\"")); // only key_present bool
}
```

- [ ] **Step 2: Run** `cargo test --lib supervisor::fallback` — Expected: FAIL.
- [ ] **Step 3: Implement** the trait, `NoFallback`, `LiveFallback` (`probe_primary`: container state via `LlmContainer::new(rt, primary.llm_container.clone(), SystemContainerCli).state()`; not Running → `(false, "container parado")`; else `probe_llama_server_health(&primary.effective_primary_host())` → detail `"/health sem resposta"`), `RedactingBackend`, `fallback_client_config`, `FallbackInfo`, `fallback_snapshot_info`, and `model_label`.
- [ ] **Step 4: Run** — Expected: PASS.
- [ ] **Step 5: Commit** `feat(supervisor): FallbackProvider with live and no-op implementations`.

### Task 4: Engine integration (tick routing, zero-idle, audit model)

**Files:**
- Modify: `src/supervisor/watcher.rs` — `WatcherEngine` (L45-68), `tick` (L71-307), `apply_decision` audit entry (L359)
- Modify: `src/supervisor/state.rs` — `DurableAutoReplyEntry` (L106): `#[serde(default, skip_serializing_if = "Option::is_none")] pub model: Option<String>`
- Modify: `src/supervisor/mod.rs:35-46` — `build_engine_from_config` sets `LiveFallback` when `config.container_mode_active()`
- Test: `src/supervisor/watcher.rs` `mod tests`

**Interfaces:**
- Consumes: `FallbackProvider`, `apply_health_probe`, `primary_stopped`, `ProbeCtx` (Tasks 2–3), `CognitiveBackend::model_label`.
- Produces: `pub fallback: Box<dyn FallbackProvider>` field (default `Box::new(NoFallback)` in `new`), `pub fn with_fallback(self, p: Box<dyn FallbackProvider>) -> Self`; `pub fn fallback_state(&self) -> FallbackState` (cached copy of the last state read in `tick`, used by Task 5).

Tick changes (order matters):
1. Sessions empty branch: after computing `zero_since`, if the fallback state is `active` and `now - zero_since >= idle_unload_secs` → `update_state(primary_stopped)`. Also call `primary_stopped` wherever this branch unloads the primary (`model_unloaded = true`).
2. Paused branch: unchanged (nothing evaluated; state not modified).
3. Before the existing `llm_available` check: `let mut fb = self.fallback.load_state();` If `!fb.active && fb.primary_intended`: `let (healthy, detail) = self.fallback.probe_primary();` `let configured = self.fallback.backend().is_some();` then `fb = self.fallback.update_state(&mut |s| { apply_health_probe(s, &ProbeCtx { healthy, paused: false, sessions: sessions.len(), configured, now, detail: detail.clone() }); })?` (log and continue with the read state on lock/write error). When `fb.active`, do **not** probe.
4. `let fb_backend = if fb.active { self.fallback.backend() } else { None };` `llm_available = if fb.active { fb_backend.is_some() } else { self.cognitive.is_available() }`.
5. At the evaluate call (L259): `match &fb_backend { Some(b) => b.evaluate(..), None => self.cognitive.evaluate(..) }`; record `eval_model = fb_backend.as_ref().map(|b| b.model_label()).unwrap_or_else(|| self.cognitive.model_label())`, store it in a field read by `apply_decision` and written into the audit entry's `model` for `AutoReplySource::Llm`.
6. Only set `self.model_loaded = true` after a successful eval when **not** on the fallback.

- [ ] **Step 1: Write the failing tests** using a `FakeFallback { state: Mutex<FallbackState>, healthy: Mutex<bool>, backend: Mutex<Option<FakeCognitive handle>> }` and the existing `FakeCognitive`/fake hc helpers in `watcher.rs` tests:

```rust
#[test] fn health_failures_switch_evaluation_to_fallback_after_two_ticks() {
    // primary_intended=true, healthy=false, fallback backend configured, one blocked session
    // tick 1: primary not evaluated? (primary is_available false → no eval); fallback not active
    // tick 2: fallback active (reason Health) and fallback backend eval_calls == 1, primary eval_calls == 0
}
#[test] fn active_fallback_skips_probe_and_primary() {
    // state.active=true; probe counter in FakeFallback stays 0 after a tick; fallback evaluated
}
#[test] fn engine_reads_state_written_by_other_writer() {
    // use LiveFallback-like provider over a tempdir state file (or FakeFallback over a real file
    // via fallback::load_state/update_state): tick with active=true evaluates fallback;
    // then another writer calls update_state(primary_started); next tick evaluates the primary.
}
#[test] fn active_without_key_skips_evaluation() {
    // active=true, backend() → None: no eval on either backend, no panic, state unchanged
}
#[test] fn zero_idle_ends_fallback() {
    // active=true, no sessions; tick at t0 and at t0+idle_unload_secs → state.active false, primary_intended false
}
#[test] fn paused_keeps_fallback_active_and_evaluates_nothing() { /* mode Paused */ }
#[test] fn fallback_eval_error_keeps_fallback_and_is_redacted() {
    // fallback backend errors with text containing "sk-secret" via RedactingBackend; state stays active;
    // build_desktop_mod_snapshot JSON does not contain "sk-secret"
}
#[test] fn audit_entry_records_answering_model() {
    // fallback backend model_label "openai/gpt-4.1-mini" auto-replies → auto_replies.jsonl entry has that model
}
```

- [ ] **Step 2: Run** `cargo test --lib supervisor::watcher` — Expected: new tests FAIL; existing ones still compile.
- [ ] **Step 3: Implement** the tick changes above.
- [ ] **Step 4: Run** `cargo test --locked --quiet` — Expected: PASS (all existing watcher tests unchanged).
- [ ] **Step 5: Commit** `feat(supervisor): route evaluations to the fallback while active; zero-idle ends it`.

### Task 5: Start/stop call sites, snapshot fields, CLI

**Files:**
- Modify: `src/supervisor/mod.rs` — `load_model` (L157), `unload_model` (L210), `set_gpu` (L225), `recreate_container_if_running` (L475), new `fallback_*` pub fns
- Modify: `src/supervisor/tui.rs` — `DesktopModSnapshot` (L22) + `build_desktop_mod_snapshot` / `enrich_snapshot_with_llm_health`
- Modify: `src/main.rs` — `SupervisorCommands::Fallback { #[command(subcommand)] action: FallbackCommand }` (near `Model {`, L288) and its arm
- Modify: `src/supervisor/fallback.rs` — models listing helpers

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `pub fn finish_primary_start(p: &dyn FallbackProvider, res: anyhow::Result<()>, now: u64) -> anyhow::Result<()>` (in `fallback.rs`): `Ok` → `update_state(primary_started)`, `Ok(())`. `Err(e)` → if `p.backend().is_some()`: `update_state(|s| { s.primary_intended = true; activate(s, LoadFailed, &short_err, now) })` and return `Err(e.context(format!("fallback ativo: {model}")))`; else return `Err(e)` unchanged (still set `primary_intended = true`).
  - Every primary start path in `mod.rs` routes through it: `load_model` container branch, `set_gpu` warm restart, `recreate_container_if_running`. `unload_model` → `update_state(primary_stopped)`. Use `LiveFallback` only when container mode is active.
  - `pub fn fallback_off() -> Result<DesktopModSnapshot>` = `load_model()` (success clears; failure keeps active with new detail and returns the error). `load_model` while active therefore behaves the same.
  - `pub fn fallback_status() -> FallbackInfo`, `pub fn fallback_set(host: Option<&str>, model: Option<&str>) -> Result<FallbackInfo>`, `pub fn fallback_key_from_reader(r: impl Read) -> Result<()>`, `pub fn fallback_models(filter: Option<&str>) -> Result<Vec<String>>`, `pub fn fallback_clear() -> Result<FallbackInfo>` (removes `[fallback]` and `update_state(|s| { s.active = false; s.reason = None; s.detail = None; s.since = None })`).
  - In `fallback.rs`: `pub fn models_url(host: &str) -> String` (`{host}/models` if host ends with `/v1`, else `{host}/v1/models`), `pub fn parse_models_list(v: &serde_json::Value) -> Vec<String>` (`data[].id`, sorted, deduped), `pub fn filter_models(ids: &[String], filter: Option<&str>) -> Vec<String>` (case-insensitive substring), `pub fn models_http_error(status: u16) -> String` (401/403 → `chave recusada pelo endpoint (HTTP 401)`, 404 → `endpoint sem /models (HTTP 404) — confira o host`, other → `HTTP <n>`).
  - Snapshot fields (all `#[serde(default)]`): `fallback_configured: bool`, `fallback_key_present: bool`, `fallback_active: bool`, `fallback_model: String`, `fallback_host: String`, `fallback_reason: Option<String>` (`"load_failed"|"health"`), `fallback_detail: Option<String>`, `fallback_since: Option<u64>`. While active: `llm_model = fallback_model`.
  - CLI (`tchelvis supervisor fallback …`): `status [--json]`, `set [--host <url>] [--model <name>]` (at least one required), `key` (stdin; prints `Chave salva em <path> (0600)`; refuses a TTY-less empty read), `models [--json] [--filter <text>]`, `off`, `clear`. Human `status` prints configured/active/reason/detail/model/host and the key **source name** only.

- [ ] **Step 1: Write the failing tests** (`fallback.rs` + `mod.rs` tests with a `FakeFallback`):

```rust
#[test] fn failed_start_activates_when_configured() {
    let p = FakeFallback::configured("openai/gpt-4.1-mini");
    let err = finish_primary_start(&p, Err(anyhow!("Container llama.cpp não respondeu")), 7).unwrap_err();
    assert!(format!("{err:#}").contains("fallback ativo"));
    let s = p.load_state(); assert!(s.active && s.primary_intended); assert_eq!(s.reason, Some(FallbackReason::LoadFailed));
}
#[test] fn failed_start_unconfigured_returns_original_error() {
    let p = FakeFallback::unconfigured();
    let err = finish_primary_start(&p, Err(anyhow!("boom")), 7).unwrap_err();
    assert_eq!(format!("{err:#}"), "boom"); assert!(!p.load_state().active);
}
#[test] fn successful_start_clears_active() {
    let p = FakeFallback::configured("m"); p.set_active();
    finish_primary_start(&p, Ok(()), 8).unwrap(); assert!(!p.load_state().active);
}
#[test] fn failed_return_keeps_fallback_and_updates_detail() {
    let p = FakeFallback::configured("m"); p.set_active();
    let _ = finish_primary_start(&p, Err(anyhow!("ainda quebrado")), 9);
    let s = p.load_state(); assert!(s.active); assert!(s.detail.unwrap().contains("ainda quebrado"));
}
#[test] fn models_url_and_parsing() {
    assert_eq!(models_url("https://openrouter.ai/api"), "https://openrouter.ai/api/v1/models");
    assert_eq!(models_url("https://opencode.ai/inference/go/openai/v1"), "https://opencode.ai/inference/go/openai/v1/models");
    let v = serde_json::json!({"data":[{"id":"b"},{"id":"a"},{"id":"a"}]});
    assert_eq!(parse_models_list(&v), vec!["a", "b"]);
    assert_eq!(filter_models(&["GPT-4.1-mini".into(), "glm".into()], Some("gpt")), vec!["GPT-4.1-mini"]);
    assert!(models_http_error(401).contains("401"));
}
#[test] fn snapshot_json_never_contains_key() {
    // build a DesktopModSnapshot with fallback configured, key "sk-very-secret", active;
    // serde_json::to_string(&snap) does not contain "sk-very-secret"; llm_model == fallback model;
    // fallback_reason == Some("health")
}
#[test] fn key_from_reader_trims_and_rejects_empty() { /* into a tempdir path via an injectable path variant */ }
```

(Give `fallback_key_from_reader` a path-taking inner fn `write_key_from_reader(path, r)` so the test does not touch `$HOME`.)

- [ ] **Step 2: Run** `cargo test --lib supervisor` — Expected: FAIL.
- [ ] **Step 3: Implement** the interfaces; wire CLI args in `main.rs` following the `Model` subcommand pattern (`spawn_blocking`, `--json` prints `serde_json::to_string_pretty`). `models` uses ureq with `Authorization: Bearer <key>`, timeout 15 s, errors via `models_http_error`, never echoing the key.
- [ ] **Step 4: Run** `cargo test --locked --quiet` and `cargo clippy --all-targets -- -D warnings` — Expected: PASS. Smoke: `HOME=$(mktemp -d) cargo run -q -- supervisor fallback status --json` shows `configured:false`; `printf 'sk-x' | HOME=… cargo run -q -- supervisor fallback key` creates a 0600 file; `… fallback set --host https://openrouter.ai/api --model m` then `status --json` shows `configured:true` and no `sk-x`.
- [ ] **Step 5: Commit** `feat(supervisor): fallback CLI, snapshot fields, primary start paths activate the fallback`.

### Task 6: Mod — pill and "Supervisor model" card

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` — pill (~L2593-2625), Load/Unload button (~L2800), `renderModelPicker` (~L4641), new action methods near `loadSupervisorLlm` (~L1537)
- Test: create `tests/mod/fallback.test.mjs`

**Interfaces:**
- Consumes: snapshot fields from Task 5; CLI `supervisor fallback {models --json --filter, set, off}`.
- Produces (exported pure helpers):
  - `fallbackPill(supSnap) -> { text: string, tone: "working" } | null` — `null` unless `supSnap?.fallback_active`; text `● LLM: ${pillModelName(supSnap.fallback_model)} (Fallback)`.
  - `fallbackSection(supSnap) -> { state: "no_key" | "needs_endpoint" | "standby" | "active", label: string }` — `no_key` when `!fallback_key_present`; `needs_endpoint` when key present but host/model empty; `active` label `ATIVO — ${reason === "load_failed" ? "load falhou" : "health falhou"} ${HH:MM of fallback_since}`; `standby` label `Em espera`.
  - `primaryRowBadge(row, supSnap) -> string | null` — `"falhou"` when `supSnap?.fallback_active && row.in_use`, else the current badge logic (`em uso` etc.) unchanged.

- [ ] **Step 1: Write the failing tests** in `tests/mod/fallback.test.mjs`:

```js
import { test } from "node:test";
import assert from "node:assert/strict";
import { fallbackPill, fallbackSection, primaryRowBadge } from "../../assets/herdr-app/mods/tchelvis/main.js";

test("pill only when fallback is active", () => {
  assert.equal(fallbackPill({ fallback_active: false }), null);
  assert.deepEqual(fallbackPill({ fallback_active: true, fallback_model: "openai/gpt-4.1-mini" }),
    { text: "● LLM: openai/gpt-4.1-mini (Fallback)", tone: "working" });
});
test("section states", () => {
  assert.equal(fallbackSection({ fallback_key_present: false }).state, "no_key");
  assert.equal(fallbackSection({ fallback_key_present: true, fallback_host: "", fallback_model: "" }).state, "needs_endpoint");
  assert.deepEqual(fallbackSection({ fallback_key_present: true, fallback_configured: true, fallback_host: "h", fallback_model: "m" }),
    { state: "standby", label: "Em espera" });
  const a = fallbackSection({ fallback_key_present: true, fallback_configured: true, fallback_host: "h", fallback_model: "m",
    fallback_active: true, fallback_reason: "health", fallback_since: 0 });
  assert.equal(a.state, "active"); assert.match(a.label, /^ATIVO — health falhou \d\d:\d\d$/);
});
test("primary in-use row reads falhou while fallback is active", () => {
  assert.equal(primaryRowBadge({ in_use: true }, { fallback_active: true }), "falhou");
  assert.notEqual(primaryRowBadge({ in_use: true }, { fallback_active: false }), "falhou");
});
```

- [ ] **Step 2: Run** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/fallback.test.mjs` — Expected: FAIL.
- [ ] **Step 3: Implement helpers and wire UI.**
  - Pill: check `fallbackPill(supSnap)` before the existing `llmManaged` branches; use its text and `t.working`.
  - Load/Unload button: when `supSnap?.fallback_active`, show `▶ Load LLM`, and clicking runs `this.returnToPrimary(cx)`; Unload keeps `supervisor unload`.
  - `renderModelPicker`: header row `PRINCIPAL (local)` above the existing list (rows use `primaryRowBadge`); new `FALLBACK (API)` section below:
    - `no_key`: hint text `Sem chave: rode "tchelvis supervisor fallback key" e cole a chave (stdin).`
    - `needs_endpoint`: endpoint text field (`editingField = "fallbackHost"`, same key handling as the task field; Enter runs `supervisor fallback set --host <text>`), hint with the OpenCode Go/OpenRouter URL examples.
    - configured: line `${hostName} · ${model}` + `Trocar`; state label; when active, `Voltar ao principal` (shows `◌ Voltando…` while running; error from stderr shown in the card in `t.blocked`).
    - `Trocar` toggles a list from `supervisor fallback models --json [--filter <this.modelFilterText>]` reusing the existing filter input; clicking a model runs `supervisor fallback set --model <id>` and closes the list. Errors (401/404 text) render in the list area.
  - `returnToPrimary(cx)`: `exec("tchelvis", ["supervisor", "fallback", "off"], { timeout: 120000 })`, sets `this.fallbackReturning`, refreshes supervisor status after.
- [ ] **Step 4: Run** `node --check assets/herdr-app/mods/tchelvis/main.js` and `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` — Expected: PASS.
- [ ] **Step 5: Commit** `feat(mod): fallback pill and two-section Supervisor model card`.

### Task 7: Docs

- [ ] Update the spec's OpenCode Go row to `https://opencode.ai/inference/go/openai/v1`; README supervisor section (how to set up the fallback: `key`, `set`, `status`); CHANGELOG `## [Unreleased]` (Added: fallback model; Changed: primary `openai-compatible` key is now host-matched — an `OPENAI_API_KEY` is no longer sent to non-OpenAI hosts); `ROADMAP.md` M1b → **Implemented**; host validation pending. Commit `docs: M1b fallback model`.

Manual host check (coordinator, after merge): stop the container by hand (`podman stop`) with an agent session open → pill flips to `(Fallback)` within ~2 ticks; `Voltar ao principal` reloads; Unload LLM ends both; `status --json` has no key.
