# Goldfish Review Resolution Report (Stage 9 — Implementation Readiness & Stage 8 Consolidation)

**Status:** resolved
**Target File:** [cockpit.md](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md) (`1..428`, focusing on `DetailedRange = 253..428` and remaining `FeatureRange` altitude cleanup)
**Source Reports:** [implementation_readiness_report.md](file:///home/ubuntu/.t3/projects/cockpit/implementation_readiness_report.md), [intermediate_zoom_out_report.md](file:///home/ubuntu/.t3/projects/cockpit/intermediate_zoom_out_report.md)

---

## Findings & Proposed Fixes

### 1. [Blocker] Missing Incremental Phasing & Build/Test Checkpoints in `Detailed Implementation Plan`
* **Source:** `egm-implementation-readiness` (#1), `egm-codebase-details` (#3)
* **Affected Scope:** [cockpit.md:253-424](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L253-L424)
* **Evaluation:** Valid. Listing all 24 deliverables in a single flat sequence with one monolithic verification block at the end prevents incremental compilation and testing (`src/lib.rs` exports modules before they exist).
* **Proposed Fix:** Partition `Detailed Implementation Plan` into **5 incremental build/test phases**, where `src/lib.rs` adds module exports progressively and each phase concludes with a concrete `cargo check` / `cargo test` verification gate:
  - **Phase 1: Core Domain, Configuration, Logging & Local Cache (`Cargo.toml`, `src/lib.rs`, `src/model.rs`, `src/config.rs`, `src/logging.rs`, `src/store.rs`)** — Checkpoint: `cargo test --lib model config logging store`
  - **Phase 2: Herdr Protocol Ingestion & Git/Ticket Correlation (`src/herdr.rs`, `src/correlate.rs`)** — Checkpoint: `cargo test --lib herdr correlate`
  - **Phase 3: Read-Only Context Providers & Rate-Limit Engine (`src/providers/mod.rs`, `github.rs`, `gitlab.rs`, `jira.rs`, `linear.rs`, `slack.rs`, `runbooks.rs`)** — Checkpoint: `cargo test --lib providers`
  - **Phase 4: Single-Instance Daemon, Unix Socket IPC & CLI (`src/daemon.rs`, `src/main.rs`)** — Checkpoint: `cargo test --lib daemon` and CLI smoke checks
  - **Phase 5: Embedded Assets, Setup/Rollback Engine, Doctor & Integration Suite (`src/assets.rs`, `assets/herdr-app/mods/cockpit/main.js`, `assets/herdr-app/layouts/cockpit.json`, `src/setup.rs`, `src/doctor.rs`, `tests/integration_tests.rs`, `README.md`)** — Checkpoint: full `cargo test` and isolated sandbox verification.

### 2. [Blocker] Underspecified `herdr-app` Mod, `gpui-kit` API, Layout JSON Schema, and `settings.json` Key Contract
* **Source:** `egm-implementation-readiness` (#2), `egm-codebase-details` (#1), Stage 8 `egm-zoom-out` (#1)
* **Affected Scope:** [cockpit.md:49-51](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L49-L51), [cockpit.md:272](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L272), [cockpit.md:368-381](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L368-L381), [cockpit.md:390](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L390)
* **Evaluation:** Valid. Because `herdr-app` is not installed on this headless host, an implementer needs the exact JS module export signature, `gpui-kit` call contract, `layouts/cockpit.json` JSON structure, and `settings.json` key path so zero design decisions are left open, while gating host config mutation safely when `herdr-app` is absent.
* **Proposed Fix:**
  1. Specify the exact `mods/cockpit/main.js` ESM contract (`export const id = "cockpit"; export function mount(ctx, ui) { ... }` using `ctx.doc.subscribe("cockpit.snapshot", cb)`, `ctx.exec(["cockpit", ...])`, `ctx.terminals.focus(address)`, `ctx.openUrl(url)`, and `ui.vsplit` / `ui.hsplit` / `ui.list` / `ui.panel` / `ui.badge` primitives).
  2. Specify the exact `assets/herdr-app/layouts/cockpit.json` JSON schema (`{"name": "cockpit", "version": 1, "root": {"direction": "horizontal", "ratios": [0.22, 0.50, 0.28], "panes": [{"type": "mod", "mod": "cockpit", "view": "attention_queue"}, {"type": "terminal", "binding": "selected"}, {"type": "mod", "mod": "cockpit", "view": "context_inspector"}]}}`).
  3. Specify that `--set-default-layout` sets `"default_layout": "cockpit"` in `~/.config/herdr-app/settings.json` only when `~/.config/herdr-app/` exists or `--only herdr-mod` is explicitly passed, recording the previous value in `setup-manifest.json`.

### 3. [Blocker] Underspecified `OSC 7501` Wire Format and Agent Hook Registration Contracts
* **Source:** `egm-implementation-readiness` (#3), `egm-codebase-details` (#4), Stage 8 `egm-zoom-out` (#4)
* **Affected Scope:** [cockpit.md:363-366](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L363-L366), [cockpit.md:383-392](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L383-L392), [cockpit.md:397](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L397)
* **Evaluation:** Valid. Verified directly against `@earendil-works/pi-tui/dist/program-status.js` on this machine: the Program Status Protocol (`OSC 7501`) wire format is `\x1b]7501;state=<state>[:app=<app>][:kind=<kind>][:msg=<base64_utf8>]\x1b\\` (where `state` is `idle|working|blocked|done|error`, `app` matches `^[A-Za-z0-9_.+-]{1,32}$`, `kind` is included when `state=blocked`, and `msg` is base64-encoded UTF-8 up to 2048 decoded bytes). Furthermore, local Pi (`1.5.2`) already emits `OSC 7501` natively.
* **Proposed Fix:**
  1. Document the exact `OSC 7501` colon-delimited key-value grammar (`\x1b]7501;state=<state>:app=<app>:kind=<kind>:msg=<base64>\x1b\\`) with base64 message encoding in `src/assets.rs` and `src/setup.rs`.
  2. Document the exact Claude Code hook registration in `~/.claude/settings.json` under `"hooks"` (`UserPromptSubmit` -> `state=working`, `Notification` / `PermissionRequest` -> `state=blocked:kind=permission|question`, `Stop` -> `state=done`), reading JSON on stdin (`hook_event_name`, `notification_type`, `message`) and tagging managed command strings with `# cockpit-managed`.
  3. Document OpenCode plugin installation at `~/.config/opencode/plugins/cockpit.js` and Pi extension installation at `~/.pi/agent/extensions/cockpit.js` (where the Pi extension only stamps `hc label $HC_TERMINAL_ID cockpit.agent=pi` on session start and defers `OSC 7501` emission to Pi's native `program-status.js`).

### 4. [Blocker] `src/herdr.rs` Protocol Mismatches with `hc 0.1.20`
* **Source:** `egm-codebase-details` (#2), `egm-implementation-readiness` (#5)
* **Affected Scope:** [cockpit.md:303-311](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L303-L311) (`src/herdr.rs`)
* **Evaluation:** Valid. Verified against `hc schema` and live `hc events --json`:
  - `hc events --json` outputs a raw `SubscribeResult` JSON line first (`{"channel":null,"seq":0,"terminals":[...]}` with full `Terminal` structs), followed by JSON-RPC 2.0 notifications (`{"jsonrpc":"2.0","method":"event","params":{"data":...,"seq":N,"terminal":"t1","type":"..."}}`) including `"type":"foreground"` (`{"argv","cwd","name","pid"}`), `"heartbeat"`, and `"missed"`.
  - `hc ls --all --json` returns compact summaries omitting `reported`, `status`, `labels`, `foreground`, and `parent`, whereas `terminal.list` over `hc connect` returns full `Terminal` records.
* **Proposed Fix:** Update `src/herdr.rs` to specify:
  1. `subscribe_events`: parses line 1 as `SubscribeResult { channel, seq, terminals: Vec<Terminal> }` and subsequent lines as JSON-RPC 2.0 `event` / `heartbeat` / `missed` notifications, handling `"foreground"` events to immediately update `foreground.cwd` and `foreground.argv` in memory, and handling `"missed"` by resetting from the fresh snapshot.
  2. A persistent `hc connect` JSON-RPC 2.0 child session (`rpc_call`) used for `terminal.list` (90s reconciliation and initial queries), `terminal.get`, `doc.get`, and `doc.set`, avoiding per-event subprocess spawns and avoiding `hc ls --all --json` field omissions.

### 5. [Major] Unlisted Hook Source Templates in `Target Files to Create/Modify`
* **Source:** `egm-implementation-readiness` (#4), `egm-codebase-details` (#4.1)
* **Affected Scope:** [cockpit.md:363-366](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L363-L366) (`src/assets.rs`)
* **Evaluation:** Valid.
* **Proposed Fix:** State explicitly in Item 18 (`src/assets.rs`) that the six hook templates (`SHELL_SH`, `SHELL_FISH`, `SHELL_NU`, `CLAUDE_HOOK_SH`, `OPENCODE_COCKPIT_JS`, `PI_COCKPIT_JS`) are defined as `pub const` string literals directly inside `src/assets.rs` while `mods/cockpit/main.js` and `layouts/cockpit.json` are embedded via `include_str!` from `assets/herdr-app/`.

### 6. [Major] Undefined `cockpit.sock` IPC Framing, `watch --json` / `inspect --json` Schemas, and Lease Liveness Mechanics
* **Source:** `egm-implementation-readiness` (#6), `egm-codebase-details` (#5.2, #5.3), Stage 8 `egm-zoom-out` (#2)
* **Affected Scope:** [cockpit.md:266-275](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L266-L275), [cockpit.md:358-361](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L358-L361)
* **Evaluation:** Valid.
* **Proposed Fix:**
  1. Define the `cockpit.sock` line-delimited JSON request envelope: `IpcRequest { op: IpcOp, terminal: Option<String> }` where `IpcOp` is `Status`, `Watch`, `Inspect`, or `Refresh`.
  2. Define responses: `Status` and `Refresh` return one `CockpitSnapshot` JSON line; `Inspect` returns one `InspectResponse { agent: AgentRow, provider_health: Vec<ProviderHealth> }` JSON line; `Watch` streams a full `CockpitSnapshot` JSON line on connect and on each state change (dropping the speculative custom delta format).
  3. Replace periodic `lease_expires_at` heartbeats in `FleetSummaryProjection` with `daemon_pid: Option<u32>` and `published_at: i64` (written as `daemon_pid: None` on graceful shutdown; clients verify live daemon status via `cockpit.sock` or `kill(pid, 0)` without periodic idle `doc.set` writes), and unify single-shot collection so `cockpit status` delegates to the same `collect_once()` path used by `cockpit daemon --once`.

### 7. [Major] Missing Crate Dependencies and Features in `Cargo.toml`
* **Source:** `egm-implementation-readiness` (#7), `egm-codebase-details` (#5.1), `egm-codebase-details` (#4.4)
* **Affected Scope:** [cockpit.md:257-260](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L257-L260) (`Cargo.toml`), [cockpit.md:356](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L356) (`src/providers/runbooks.rs`)
* **Evaluation:** Valid.
* **Proposed Fix:** Update Item 1 (`Cargo.toml`) to list exact crate versions and feature flags: `clap` (`4`, `derive`), `serde` (`1`, `derive`), `serde_json` (`1`, `preserve_order`), `tokio` (`1`, `rt-multi-thread`, `macros`, `net`, `process`, `io-util`, `sync`, `time`, `signal`, `fs`), `reqwest` (`0.12`, `json`, `rustls-tls`, `default-features = false`), `rusqlite` (`0.32`, `bundled`), `toml` (`0.8`), `regex` (`1`), `sha2` (`0.10`), `base64` (`0.22`), `chrono` (`0.4`, `serde`), `tracing` (`0.1`), `tracing-subscriber` (`0.3`, `json`, `env-filter`), `thiserror` (`2`), `anyhow` (`1`), `libc` (`0.2`), and `tempfile` (`3`, dev-dependency). Specify deterministic line-based `---` YAML frontmatter parsing (`title:`, `tickets:`, `branches:`, `services:`) and `.gitignore` entry matching in `src/providers/runbooks.rs`.

### 8. [Major] Git Worktree `commondir` Resolution, `.git/packed-refs`, and `AgentSummaryRow` / `AgentKind` Rules
* **Source:** `egm-codebase-details` (#6), `egm-implementation-readiness` (#9), Stage 8 `egm-zoom-out` (#6)
* **Affected Scope:** [cockpit.md:287-288](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L287-L288), [cockpit.md:316](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L316), [cockpit.md:361](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L361)
* **Evaluation:** Valid. In linked `git worktree` checkouts, `<worktree>/.git` points to `<gitdir>` where `HEAD` lives, while `config` and `packed-refs` live in `<commondir>` (resolved via `<gitdir>/commondir`).
* **Proposed Fix:**
  1. Specify in `src/correlate.rs` and `src/daemon.rs` that git resolution resolves `<worktree>/.git` -> `<gitdir>` (reading `<gitdir>/HEAD`) and resolves `<gitdir>/commondir` -> `<commondir>` (reading `<commondir>/config`, `<commondir>/refs/...`, and `<commondir>/packed-refs`), with remote selection precedence `origin` > `upstream` > first configured remote and SSH/HTTPS URL normalization to `host/owner/repo`.
  2. Define `AgentSummaryRow` exact fields (`address`, `name`, `machine`, `agent_kind`, `lifecycle`, `status`, `blocked_on`, `blocked_reason`, `AttentionPriority`, `correlation_confidence`, `ticket_id`, `pr_number`, `ci_state`, `updated_at`), the `<256 KiB` truncation rule (evict oldest finished rows first, then truncate `blocked_reason` to 120 chars), and the `AgentKind` matching table (`claude` -> `ClaudeCode`, `opencode` -> `OpenCode`, `pi` -> `Pi`, `codex` -> `Codex`).

### 9. [Major] Provider Endpoint Contracts, Rate-Limit / TTL Constants, `store.rs` Scope, and Setup Manifest Schema
* **Source:** `egm-implementation-readiness` (#8, #10), `egm-codebase-details` (#7), Stage 8 `egm-zoom-out` (#3)
* **Affected Scope:** [cockpit.md:318-351](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L318-L351), [cockpit.md:388-391](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L388-L391), [cockpit.md:399-408](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L399-L408)
* **Evaluation:** Valid.
* **Proposed Fix:**
  1. Keep `hc` as the sole authority for terminal lifecycle and remove the duplicate `agent_history` table from `src/store.rs`, retaining only `http_cache` and `provider_health` in SQLite WAL for offline/restart provider state.
  2. Specify exact REST endpoints and CLI fallback commands for `github.rs` (`GET /repos/{owner}/{repo}/pulls?head={owner}:{branch}`, `GET /repos/{owner}/{repo}/commits/{sha}/check-runs`, `GET /repos/{owner}/{repo}/pulls/{num}/reviews`, `GET /repos/{owner}/{repo}/deployments?sha={sha}`, `GET /repos/{owner}/{repo}/releases/latest`, or `gh api` fallback) and `gitlab.rs` (`/api/v4/projects/{id}/merge_requests?source_branch={branch}`, pipelines, approvals, deployments, releases, or `glab api` fallback), plus concrete defaults (`positive_ttl = 60s`, `negative_ttl = 900s`, token bucket `capacity = 10`, `refill = 1 req / 2s`, fallback `429` cooldown `60s`).
  3. Enumerate valid `--only`/`--skip` target names (`shell`, `claude`, `opencode`, `pi`, `herdr-mod`, `systemd`), gate `systemd` user unit creation behind `--systemd`, define `SetupManifest` fields (`version`, `created_at`, `entries: Vec<ManifestEntry>` with `path`, `pre_sha256`, `existed_before`, `action_kind`, `previous_json_value`), and add UDS IPC, CAS retry, `doctor`, and SQLite cache tests to `tests/integration_tests.rs`.

### 10. [Major] Remaining Altitude Drops in `FeatureRange` (`1..252`) and Uncalibrated Claims in `DetailedRange` (`253..428`)
* **Source:** Stage 8 `egm-zoom-out` (#5), Stage 9 `egm-style` (#1–#6)
* **Affected Scope:** [cockpit.md:1-252](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L1-L252), [cockpit.md:316](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L316), [cockpit.md:331](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L331), [cockpit.md:376](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L376), [cockpit.md:390-392](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L390-L392), [cockpit.md:397](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L397), [cockpit.md:408](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L408)
* **Evaluation:** Valid.
* **Proposed Fix:**
  1. Replace low-level implementation weeds in `FeatureRange` (octal `0700`, `flock`/`ECONNREFUSED`/`SIGKILL`, exact backoff steps `30s -> 60s -> 180s -> 600s`, the 11-prefix denylist literal, env var lists, and `# >>> cockpit managed >>>` literals) with high-level architectural statements, keeping all exact constants in `Detailed Implementation Plan`.
  2. Calibrate all 6 flagged sentences in `DetailedRange` (`src/correlate.rs`, `src/providers/github.rs`, `mods/cockpit/main.js`, `src/setup.rs`, `src/doctor.rs`, `tests/integration_tests.rs`) to state concrete mechanisms rather than marketing phrasing or uncalibrated guarantees.

### 11. [Nit] Isolated Verification Sandbox, Unauthenticated `gh` Mapping, `cockpit label` Semantics, and Minor Style Cleanups
* **Source:** `egm-implementation-readiness` (#11), `egm-codebase-details` (#8), `egm-style` (#7–#11)
* **Affected Scope:** [cockpit.md:260](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L260), [cockpit.md:270](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L270), [cockpit.md:326](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L326), [cockpit.md:331](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L331), [cockpit.md:341](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L341), [cockpit.md:351](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L351), [cockpit.md:361](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L361), [cockpit.md:413-423](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L413-L423)
* **Evaluation:** Valid.
* **Proposed Fix:**
  1. Specify running destructive setup/uninstall and `hc` integration checks inside an isolated temporary `HOME`/`XDG_*` and `HC_SESSION` sandbox in `Verification Plan`.
  2. State in `github.rs` that an unauthenticated `gh` binary with no `GITHUB_TOKEN` maps to `ProviderHealth::Inactive`, and clarify in `src/main.rs` that `cockpit label <terminal> [k=v...]` forwards `k=` label removal and prints existing `cockpit.*` labels when `[k=v...]` is omitted.
  3. Apply all minor wording cleanups from `egm-style` Findings #7–#11.
