# Goldfish Review Report — Stage 5: Initial Zoom Out Review

**Target:** `/home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md` (`1-398`)
**Goldfish Context:** `["/home/ubuntu/.t3/projects/cockpit/concepts_brief.md"]`

## Reviewer Status

| Reviewer | Status |
| --- | --- |
| `egm-zoom-out` | completed |
| `egm-inventive-catalyst` | completed |

## Prioritized Findings (Blockers -> Major -> Nits)

### 1. Unverified `herdr-app` Mod & `gpui-kit` Runtime Contract - Severity: Blocker
* **Reviewer Lens:** `egm-zoom-out`
* **Affected Scope:** `Overview` ([cockpit.md:11-13](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L11-L13)), `Goals` ([cockpit.md:30](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L30)), `Open Questions` ([cockpit.md:41-43](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L41-L43)), `High-Level Architecture` ([cockpit.md:86-92](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L86-L92)), `Target Files` #18–20 ([cockpit.md:333-355](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L333-L355))
* **Discrepancy Description:** The design commits to shipping `~/.config/herdr-app/mods/cockpit/main.js` (`gpui-kit`) and `~/.config/herdr-app/layouts/cockpit.json`, while stating under `Open Questions` that none are open. On this host, `herdr-app` is not installed (`which herdr-app` fails and `~/.config/herdr-app` is absent), so the exact `herdr.d.ts` / `gpui-kit.d.ts` runtime API and layout schema cannot be verified locally via `herdr-app check`.
* **Rationale & Recommendation:** Document the `herdr-app` runtime verification constraint explicitly under `Open Questions` / `Technical Plan` (or obtain/inline the exact `gpui-kit` mod interface contract), and ensure the `mods/cockpit/main.js` and `layouts/cockpit.json` artifacts degrade cleanly and remain verifiable via unit/schema tests when `herdr-app` is absent on a headless Linux host.

### 2. V1 Provider Sprawl & Premature Generalization Across 6 Integrations - Severity: Blocker
* **Reviewer Lens:** `egm-zoom-out`
* **Affected Scope:** `Goals` ([cockpit.md:28-29](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L28-L29)), `High-Level Architecture` ([cockpit.md:80](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L80), [cockpit.md:114-120](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L114-L120)), `Target Files` #10–16 ([cockpit.md:293-330](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L293-L330))
* **Discrepancy Description:** Building six full provider clients (`github`, `gitlab`, `jira`, `linear`, `slack`, `runbooks`) with custom REST/GraphQL pagination, ETag caches, token buckets, and circuit breakers in V1 introduces high complexity when the author's immediate environment only has `hc` and `gh` installed.
* **Rationale & Recommendation:** While the author explicitly requested initial support for GitHub/GitLab CLI (`gh`/`glab`), Slack, Jira, Linear, and local runbooks, keep the V1 adapters lightweight: delegate `gh` and `glab` queries to their respective CLI JSON outputs or thin HTTP calls, keep Jira/Linear/Slack adapters as minimal single-endpoint read-only fetchers (plus URL/label link extraction when unauthenticated), and avoid over-engineered per-provider rate-limit state machines.

### 3. Monolithic `CockpitSnapshot` vs. `<256 KiB` `hc` Document Store Cap & Daemon Liveness - Severity: Major
* **Reviewer Lens:** `egm-inventive-catalyst`
* **Affected Scope:** `High-Level Architecture` ([cockpit.md:81-89](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L81-L89), [cockpit.md:134-138](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L134-L138)), `src/model.rs` ([cockpit.md:254-261](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L254-L261)), `src/herdr.rs` ([cockpit.md:279](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L279)), `assets/herdr-app/mods/cockpit/main.js` ([cockpit.md:338-346](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L338-L346))
* **Discrepancy Description:** Bundling both the fleet summary and all deep inspector payloads (full CI check lists, reviews, runbook previews, provenance edges) into a single `CockpitSnapshot` pushed to `hc`'s `doc.set` (`cockpit.snapshot`, `<256 KiB`) risks breaching the 256 KiB cap as agents and CI checks grow. Furthermore, `hc`'s document store persists after the `cockpit` daemon stops, risking stale `blocked` alerts in `main.js`.
* **Rationale & Recommendation:** Apply **Segmentation**, **Extraction**, and **Beforehand Cushioning**: keep the `hc` `doc.set` payload as a bounded Fleet Summary Projection (`O(1)` KiB per agent) with a `heartbeat_at` / `lease_expires_at` timestamp so `main.js` detects stale daemon state immediately, while serving full per-agent inspector details via `cockpit inspect <t> --json` / `cockpit.sock`.

### 4. Redundant Shell Hook Label-Stamping & Unverified Agent Config Schemas - Severity: Major
* **Reviewer Lens:** `egm-zoom-out`, `egm-inventive-catalyst`
* **Affected Scope:** `Goals` ([cockpit.md:28](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L28)), `Source Provenance & Hook Layer` ([cockpit.md:71-75](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L71-L75)), `src/correlate.rs` ([cockpit.md:283-286](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L283-L286)), `src/setup.rs` ([cockpit.md:353-365](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L353-L365))
* **Discrepancy Description:**
  1. Having interactive shell hooks (`shell.sh`, `shell.fish`, `shell.nu`) emit `cockpit.cwd`, `cockpit.repo`, `cockpit.branch`, and `cockpit.head` via `hc label` on every prompt duplicates `herdr-core`'s native `foreground.cwd` tracking and promotes every plain shell into a tracked agent (violating the filter that excludes terminals with `reported == false` and no `cockpit.*` labels).
  2. `src/setup.rs` proposes merging `cockpit-osc7501` hook maps into `~/.config/opencode/opencode.json` and `~/.pi/agent/settings.json`, whereas actual files on disk show OpenCode uses `~/.config/opencode/plugins/` (or `plugin` array) and Pi uses `~/.pi/agent/settings.json` `packages`/`skills` or `~/.pi/agent/extensions/`.
* **Rationale & Recommendation:** Derive CWD, repo root, branch, and `HEAD` SHA directly inside `src/correlate.rs` from `herdr-core`'s `foreground.cwd` and `.git/HEAD` instead of stamping git labels from shell prompts. Keep shell integration strictly as a helper (`cockpit label` / `hc` wrapper) rather than tagging every plain shell, and align Claude Code (`~/.claude/settings.json` `hooks`), OpenCode (`~/.config/opencode/plugins/`), and Pi (`~/.pi/agent/extensions/`) with their real on-disk extension mechanisms.

### 5. Duplicate IPC Surfaces & Overweight Persistence Infrastructure - Severity: Major
* **Reviewer Lens:** `egm-zoom-out`
* **Affected Scope:** `High-Level Architecture` ([cockpit.md:76-85](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L76-L85)), `Target Files` #1, #5–6, #9, #17 ([cockpit.md:230-233](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L230-L233), [cockpit.md:263-274](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L263-L274), [cockpit.md:288-291](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L288-L291), [cockpit.md:328-332](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L328-L332))
* **Discrepancy Description:** Maintaining both a custom Unix domain socket delta protocol (`cockpit.sock`), `hc`'s `doc.set` store, and a multi-table SQLite WAL database (`rusqlite` `bundled`) adds significant dependency and code weight for a local read-only dashboard, and `Cargo.toml` omits a `keyring` crate despite line 176 mentioning `libsecret`.
* **Rationale & Recommendation:** Simplify persistence or align `Cargo.toml` with the stated storage/credential mechanisms (e.g., use env vars + `secret_command` + CLI auth without requiring native `libsecret` C bindings, and keep the local cache schema minimal).

### 6. Premature Implementation Details (Altitude Drops) in `FeatureRange` - Severity: Major
* **Reviewer Lens:** `egm-zoom-out`
* **Affected Scope:** `Overview`, `Goals`, `Technical Plan`, `Non-Functional Requirements` ([cockpit.md:11-185](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L11-L185))
* **Discrepancy Description:** `FeatureRange` (`1..225`) includes code-level Rust trait signatures (`fn id(&self) -> ProviderId`, `async fn fetch_by_keys`), exact regex literals (`\b[A-Z][A-Z0-9_]{1,9}-\d+\b`), token prefix lists (`ghp_`, `glpat-`, `ATATT3x`), file mode constants (`0600`), and shell fence markers (`# >>> cockpit >>>`) that belong in `Detailed Implementation Plan` (`226..398`).
* **Rationale & Recommendation:** Keep `FeatureRange` focused on architectural boundaries, component responsibilities, and data flows, moving concrete Rust trait signatures, regex literals, and file-permission constants into `Detailed Implementation Plan`.

### 7. Missing `.git/HEAD` Watcher Mechanism in Implementation Plan - Severity: Nit
* **Reviewer Lens:** `egm-inventive-catalyst`
* **Affected Scope:** `Correlation Cascade & Event-Gated Refresh Rules` ([cockpit.md:166](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L166)), `Cargo.toml` ([cockpit.md:233](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L233)), `src/daemon.rs` ([cockpit.md:331](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L331))
* **Discrepancy Description:** Line 166 lists `.git/HEAD` changes as an event-gated refresh trigger, but `src/daemon.rs` and `Cargo.toml` do not specify how `.git/HEAD` is monitored between `hc` events.
* **Rationale & Recommendation:** Either watch active repositories' `.git/HEAD` files (or check `.git/HEAD` mtime/contents on lightweight local ticks) in `src/daemon.rs` and `src/correlate.rs` so branch switches trigger refreshes deterministically.

### 8. Live Verification Plan Assumes Authenticated `gh` and Active Agent in `t7` - Severity: Nit
* **Reviewer Lens:** `egm-zoom-out`
* **Affected Scope:** `Verification Plan` ([cockpit.md:385-396](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L385-L396))
* **Discrepancy Description:** On the current machine, `t7` is an ordinary `zsh` shell (`reported: false`) and `gh` is not logged into a remote host, so live verification needs deterministic test fixtures or simulated `hc label` / OSC 7501 states in addition to live `hc` checks.
* **Rationale & Recommendation:** Update the Verification Plan to include end-to-end tests with simulated `hc` agent labels/statuses and mock provider responses alongside live `hc` CLI verification.
