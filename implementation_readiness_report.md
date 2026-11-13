# Goldfish Review Report: Stage 9 (Implementation Readiness Test)

* **Target File:** [cockpit.md](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md) (`253..428`)
* **Reviewers:**
  * `egm-implementation-readiness`: [Stage 9 Review: Implementation Readiness Goldfish](t3-thread://v2/06c187af-ff92-4b05-9894-f773aa206f38/thread:delegated-task:command%3Amcp%3A6e434759-06e5-4b4c-ba5f-59ad5d8a571d%3Adelegate-task%3Agoldfish-review-stage9-implementation-readiness)
  * `egm-codebase-details`: [Stage 9 Review: Codebase Details Goldfish](t3-thread://v2/06c187af-ff92-4b05-9894-f773aa206f38/thread:delegated-task:command%3Amcp%3A6e434759-06e5-4b4c-ba5f-59ad5d8a571d%3Adelegate-task%3Agoldfish-review-stage9-codebase-details)
  * `egm-style`: [Stage 9 Review: Style Goldfish](t3-thread://v2/06c187af-ff92-4b05-9894-f773aa206f38/thread:delegated-task:command%3Amcp%3A6e434759-06e5-4b4c-ba5f-59ad5d8a571d%3Adelegate-task%3Agoldfish-review-stage9-style)
* **Standard:** `implementation-readiness-standard.md`

---

## Consolidated Findings

### 🔴 Blockers

#### 1. Missing Incremental Phasing & Build/Test Checkpoints
* **Source:** `egm-implementation-readiness` (#1), `egm-codebase-details` (#3)
* **Affected Scope:** [cockpit.md:253-424](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L253-L424) (`Detailed Implementation Plan`, `Verification Plan`)
* **Finding & Guidance:** The plan lists 24 files in a single flat sequence with one monolithic `Verification Plan` at the end and zero incremental phases or intermediate build/test checkpoints (`src/lib.rs` exports all 11 modules upfront). Partition the deliverables into incremental phases (e.g., Phase 1: Foundation (`Cargo.toml`, `model`, `config`, `logging`, `store`) + unit tests; Phase 2: Herdr Ingestion & Git Correlation (`herdr`, `correlate`) + fixture tests; Phase 3: Context Providers & Quota Engine (`providers/*`) + mock HTTP/rate-limit tests; Phase 4: Daemon & CLI (`daemon`, `main.rs`) + UDS tests; Phase 5: Embedded Assets, Setup, Doctor & Docs (`assets`, `herdr-app/*`, `setup`, `doctor`, `README.md`) + end-to-end tests), each ending with an explicit `cargo check` / `cargo test` checkpoint.

#### 2. Underspecified `herdr-app` Mod, `gpui-kit` API, Layout JSON Schema, and `settings.json` Key Contract
* **Source:** `egm-implementation-readiness` (#2), `egm-codebase-details` (#1), Stage 8 `egm-zoom-out` (#1)
* **Affected Scope:** [cockpit.md:272](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L272), [cockpit.md:368-381](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L368-L381), [cockpit.md:390](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L390), [cockpit.md:397](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L397)
* **Finding & Guidance:** `herdr-app` is not installed on this host, and the plan leaves the `mods/cockpit/main.js` export/hook API, `gpui-kit` component calls, `layouts/cockpit.json` JSON schema, and `herdr-app/settings.json` key path (`--set-default-layout`) unspecified. Define the concrete JS module export contract, `gpui-kit` bindings, `layouts/cockpit.json` JSON schema, and `settings.json` key path (with graceful no-op when `herdr-app` is absent).

#### 3. Underspecified `OSC 7501` Wire Format and Agent Hook Registration Contracts (`claude-hook.sh`, `opencode-cockpit.js`, `pi-cockpit.js`)
* **Source:** `egm-implementation-readiness` (#3), `egm-codebase-details` (#4), Stage 8 `egm-zoom-out` (#4)
* **Affected Scope:** [cockpit.md:363-366](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L363-L366), [cockpit.md:383-392](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L383-L392), [cockpit.md:397](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L397)
* **Finding & Guidance:**
  1. **OSC 7501 wire syntax:** Verify against `hc 0.1.20` and `@earendil-works/pi-tui/dist/program-status.js` on disk (`\x1b]7501;<state>;<detail>\x07` or exact `hc` parser grammar) and document the exact format.
  2. **Pi (`1.5.2`) native OSC 7501:** Local Pi already emits `OSC 7501` natively and auto-discovers `~/.pi/agent/extensions/*.js`. Specify exact extension placement (`~/.pi/agent/extensions/cockpit.js`) and avoid colliding with Pi's native `OSC 7501` emissions.
  3. **Claude Code (`~/.claude/settings.json`) & OpenCode (`~/.config/opencode/`):** Specify the exact Claude Code hook event keys (`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`, `Stop`, `PermissionRequest`), stdin JSON fields, OpenCode plugin drop-in path (`~/.config/opencode/plugins/cockpit.js`), and enable `serde_json` `preserve_order` for format-preserving JSON edits.

#### 4. `src/herdr.rs` Protocol Mismatches with `hc 0.1.20`
* **Source:** `egm-codebase-details` (#2), `egm-implementation-readiness` (#5)
* **Affected Scope:** [cockpit.md:303-311](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L303-L311) (`src/herdr.rs`)
* **Finding & Guidance:**
  1. `hc events --json` emits a raw `SubscribeResult` JSON line on line 1 (`{"channel":null,"seq":0,"terminals":[...]}` with full `Terminal` objects), not an `"event": "snapshot"` wrapper. Subsequent lines are JSON-RPC 2.0 notifications (`{"jsonrpc":"2.0","method":"event","params":{"data":...,"seq":N,"terminal":"t1","type":"..."}}`).
  2. `hc events --json` emits `"type": "foreground"` (`{"argv","cwd","name","pid"}`) when a terminal's foreground process or `cwd` changes, plus `lost`, `restarted`, `keep`, `title`, `size`, `heartbeat`, and `missed`. `HerdrClient::subscribe_events` must handle `"foreground"` events to update `foreground.cwd` and `foreground.argv` immediately, and handle `"missed"` by re-syncing via `terminal.list`.
  3. Use `terminal.list` over a persistent `hc connect --json` RPC child process (separate from the `events.subscribe` notification stream) for full `Terminal` reconciliation and `doc.get`/`doc.set` CAS writes instead of `hc ls --all --json` (which omits `reported`, `labels`, `foreground`, `status`, `parent`) or spawning a new subprocess per event.

---

### 🟡 Majors

#### 5. Unlisted Hook Source Files in `Target Files to Create/Modify`
* **Source:** `egm-implementation-readiness` (#4), `egm-codebase-details` (#4.1)
* **Affected Scope:** [cockpit.md:363-366](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L363-L366) (`src/assets.rs`), [cockpit.md:386-387](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L386-L387) (`src/setup.rs`)
* **Finding & Guidance:** Explicitly state whether `shell.sh`, `shell.fish`, `shell.nu`, `claude-hook.sh`, `opencode-cockpit.js`, and `pi-cockpit.js` are defined as `const` string templates directly inside `src/assets.rs` or as separate files in `Target Files to Create/Modify`.

#### 6. Undefined `cockpit.sock` IPC Framing, `watch --json` / `inspect --json` Schemas, and Lease Liveness Mechanics
* **Source:** `egm-implementation-readiness` (#6), `egm-codebase-details` (#5.2, #5.3), Stage 8 `egm-zoom-out` (#2)
* **Affected Scope:** [cockpit.md:266-275](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L266-L275) (`src/main.rs`), [cockpit.md:358-361](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L358-L361) (`src/daemon.rs`)
* **Finding & Guidance:**
  1. Define the exact NDJSON request/response protocol over `cockpit.sock` (`{"op": "status" | "watch" | "inspect" | "refresh", "terminal": Option<String>}`).
  2. Simplify `cockpit watch --json` to stream newline-delimited `FleetSummaryProjection` (or `CockpitSnapshot`) JSON lines on state changes rather than an undocumented custom delta format.
  3. Define `cockpit inspect <terminal> --json` output as a serialized `AgentRow` (or `{ "agent": AgentRow, "provider_health": Vec<ProviderHealth> }`).
  4. Replace periodic `lease_expires_at` heartbeats in `doc.set` with `daemon_pid: Option<u32>` + `published_at: i64` (cleared on graceful shutdown; clients verify liveness via `cockpit.sock` / `kill(pid, 0)` on demand) to eliminate idle `doc.set` write amplification.

#### 7. Missing Crate Dependencies and Features in `Cargo.toml`
* **Source:** `egm-implementation-readiness` (#7), `egm-codebase-details` (#5.1)
* **Affected Scope:** [cockpit.md:257-260](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L257-L260) (`Cargo.toml`), [cockpit.md:301](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L301) (`src/logging.rs`), [cockpit.md:356](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L356) (`src/providers/runbooks.rs`), [cockpit.md:361](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L361) (`src/daemon.rs`)
* **Finding & Guidance:** Pin crate versions and required features in `Cargo.toml`: `libc` (for `flock` and `kill(pid, 0)`), `serde_json` with `preserve_order`, `chrono` with `serde`, `tracing-subscriber` with `json` and `env-filter`, and specify deterministic line-based YAML frontmatter parsing and `.gitignore` filtering in `src/providers/runbooks.rs`.

#### 8. Git Worktree `commondir` Resolution, `.git/packed-refs`, and `AgentSummaryRow` / `AgentKind` Rules
* **Source:** `egm-codebase-details` (#6), `egm-implementation-readiness` (#9), Stage 8 `egm-zoom-out` (#6)
* **Affected Scope:** [cockpit.md:287-288](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L287-L288) (`src/model.rs`), [cockpit.md:316](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L316) (`src/correlate.rs`), [cockpit.md:361](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L361) (`src/daemon.rs`)
* **Finding & Guidance:**
  1. In linked git worktrees, `<worktree>/.git` points to `<gitdir>` (`<main>/.git/worktrees/<name>`), where `HEAD` resides, while `config` and `packed-refs` reside in the common dir resolved via `<gitdir>/commondir`. Specify resolving both `<gitdir>/HEAD` and `<commondir>/config` + `<commondir>/packed-refs`.
  2. Specify remote selection precedence (`origin`, then `upstream`, then first configured remote) and SSH/HTTPS URL normalization.
  3. Define exact `AgentSummaryRow` fields, the `<256 KiB` truncation order (drop finished rows oldest-first, then truncate long status messages), and the exact `AgentKind` matching table (`claude`, `opencode`, `pi`, `codex`).

#### 9. Provider Endpoint Contracts, Rate-Limit / TTL Constants, and Setup Manifest Schema
* **Source:** `egm-implementation-readiness` (#8, #10), `egm-codebase-details` (#7)
* **Affected Scope:** [cockpit.md:323-351](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L323-L351) (`src/providers/*`), [cockpit.md:388-391](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L388-L391) (`src/setup.rs`), [cockpit.md:399-408](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L399-L408) (`tests/integration_tests.rs`)
* **Finding & Guidance:**
  1. Specify concrete GitHub/GitLab REST endpoints and `gh`/`glab` CLI fallback commands, plus per-provider positive cache TTL (`60s`), negative cache TTL (`900s`), token-bucket capacity/refill (`10` burst, `1 req/2s` refill), and fallback `429` cooldown (`60s`).
  2. Enumerate `--only`/`--skip` target names (`shell`, `claude`, `opencode`, `pi`, `herdr-mod`, `systemd`), define the `setup-manifest.json` struct fields, and expand `tests/integration_tests.rs` to cover UDS IPC (`status`, `watch`, `inspect`), `doc.set` CAS conflict retry, `cockpit doctor`, and SQLite WAL pruning.

#### 10. Uncalibrated Claims & Subjective Phrasing in `DetailedRange` (`253..428`)
* **Source:** `egm-style` (#1–#6)
* **Affected Scope:** [cockpit.md:316](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L316), [cockpit.md:331](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L331), [cockpit.md:376](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L376), [cockpit.md:390-392](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L390-L392), [cockpit.md:397](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L397), [cockpit.md:408](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L408)
* **Finding & Guidance:** Calibrate absolute claims and subjective modifiers (`"Delivers ... with zero blocking I/O"`, `"Verifies all functional and non-functional invariants automatically"`, `"Guarantees fast, deterministic linking"`, `"Eliminates troubleshooting blind spots"`, `"Ensures safe onboarding and clean uninstallability"`, `"minimal quota consumption"`). Also trim implementation weeds from `FeatureRange` (`1..252`) per Stage 8 Finding #5 so code-level constants reside in `Detailed Implementation Plan`.

---

### 🟢 Nits

#### 11. Isolated `HC_SESSION` & Temporary `HOME` in Verification Plan, Unauthenticated `gh` Health Mapping, and Minor Style Edits
* **Source:** `egm-implementation-readiness` (#11), `egm-codebase-details` (#8), `egm-style` (#7–#11)
* **Affected Scope:** [cockpit.md:260](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L260), [cockpit.md:270](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L270), [cockpit.md:326](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L326), [cockpit.md:331](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L331), [cockpit.md:341](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L341), [cockpit.md:351](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L351), [cockpit.md:361](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L361), [cockpit.md:413-423](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L413-L423)
* **Finding & Guidance:**
  1. Run setup/uninstall and live `hc` verification steps inside an isolated temporary `HOME`/`XDG_*` and `HC_SESSION` sandbox so live host configs are untouched.
  2. Map unauthenticated `gh` (`gh auth status` non-zero with no `GITHUB_TOKEN`) to `ProviderHealth::Inactive` (matching `gitlab.rs`), and clarify `cockpit label <terminal> [k=v...]` (`k=` removes label; omitting args prints labels).
  3. Replace minor buzzwords (`"enforces quota economics"` -> `"enforces rate-limit and quota budgets"`, `"Eliminates split-brain polling"` -> `"Prevents concurrent daemon instances via flock"`, `"zero noise"` -> `"without emitting errors"`, `"compile cleanly"` -> `"compile with zero errors"`).
