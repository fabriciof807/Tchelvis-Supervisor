# Cockpit

* **Author:** notchelvis
* **Date:** 2026-11-13

## Overview

Cockpit is a local-first, read-only dashboard for tracking live and finished coding agents managed through Herdr (`herdr-core` / `hc`). It ships as two paired components:
1. A native Herdr Desktop (`herdr-app`) mod (`~/.config/herdr-app/mods/cockpit/main.js` and layout `~/.config/herdr-app/layouts/cockpit.json`) rendered with `gpui-kit`.
2. A local Rust CLI binary and background service (`cockpit`) that works both as the data engine for the Herdr mod and as a standalone CLI/headless inspection tool on Linux machines where `herdr-app` is not installed.

Cockpit shows every agent in one place, highlights agents waiting on human input (`blocked` status via OSC 7501 with `permission`, `question`, or `auth` kinds), and correlates each agent with its related ticket (Jira, Linear), pull/merge request, CI checks, deploy/release status, review state (GitHub `gh` / GitLab `glab`), Slack thread, and local Markdown runbooks. All state stays on the user's machine; external network requests are restricted to essential read-only calls to the user's configured tools, with zero third-party telemetry. A single setup command (`cockpit setup`) configures shell hooks, Claude Code, OpenCode, Pi, and the Herdr mod cleanly and reversibly.

## Analysis

### Reviewers Status

| Reviewer Lens | Status |
| :--- | :--- |
| `design_critic` | completed |
| `codebase_details` | completed |
| `zoom_out` | completed |
| `comprehension` | completed |
| `implementation_readiness` | completed |
| `structural` | completed |
| `inventive_catalyst` | completed |

### 1. Contrarian / YAGNI & Reuse of Existing `herdr-core` Capabilities (`zoom_out`)
* **Don't Duplicate `hc` State Tracking:** `herdr-core` (`hc 0.1.20`) already tracks `live` and `finished` terminals (`terminal.list`), streams live state transitions via `hc events --json` / `hc connect` (`events.subscribe`), records OSC 7501 status (`idle`, `working`, `done`, `blocked`, `error` with `kind: permission | question | auth` and `msg`), tracks live `foreground.cwd` and `argv`, supports key-value labels (`hc label <t> k=v`), and includes a built-in versioned 1 MiB JSON document store (`doc.get` / `doc.set` with `doc` events). Cockpit must treat `hc` as the sole source of truth for terminal and agent status rather than maintaining a competing terminal state tracker.
* **Filter Out Plain Non-Agent Shells:** `hc ls --all --json` includes finished plain shells (`agent: "zsh"`, `reported: false`). Querying GitHub, Jira, Linear, or Slack for every exited shell wastes API quota and clutters the UI. Cockpit should prioritize active/reported agent sessions (`reported == true` or explicit `cockpit.*` labels) and group or collapse finished sessions behind a retention window.
* **Leverage `hc` `doc.set` / `doc.get` Alongside Local IPC:** Because `herdr-core` already provides a versioned 1 MiB JSON document store (`doc.set`/`doc.get`) and broadcasts `doc` events to subscribers, the `cockpit` Rust daemon can publish its pre-joined ViewModel snapshot directly into `hc`'s document store (as well as serving it over a local Unix socket / `cockpit status --json`), letting the `herdr-app` mod consume updates without spawning per-tick subprocesses.

### 2. Cost, Quota & Subprocess Economics (`design_critic`)
* **Multi-Provider Rate-Limit Exhaustion:** Naive polling across $N$ live and $M$ finished agents across 5 integrations ($O((N+M) \times K)$) will quickly exhaust GitHub's 5,000 req/hr core quota (and 30 req/min Search API limit), Slack Tier 2/3 limits (20–50 req/min), and Jira/Linear burst budgets—starving the live coding agents that share the user's local OAuth/PAT tokens.
  * *Safeguard:* Deduplicate all external queries by canonical resource key (`(host, repo, branch_or_pr)`, `issue_key`, `slack_thread_url`) rather than polling per terminal. Freeze polling for `finished` agents and terminal states (merged/closed PRs). Enforce per-provider token buckets, `ETag`/`If-None-Match` conditional requests, `Retry-After` compliance, and a hard quota reserve floor (halting Cockpit background refreshes when remaining quota drops below 25%).
* **Subprocess Spawn Storms:** Spawning `gh`, `glab`, and `hc` CLI subprocesses inside tight polling loops incurs 80–250 ms CPU overhead and 30–60 MB transient RSS per call with no HTTP/2 connection reuse.
  * *Safeguard:* Consume `hc events --json` (or `hc connect`) as a persistent stream instead of polling `hc ls`. Use persistent in-process Rust HTTP clients (`reqwest`) for Jira, Linear, Slack, and batched GraphQL/REST queries where possible, using `gh auth token` / `glab` CLI only for credential discovery or bounded worker execution (max 2–4 concurrent subprocesses) with closed `stdin` and hard timeouts.

### 3. Contradiction Resolution & Inventive Patterns (`inventive_catalyst`)
* **Contradiction 1 — Fresh External Context vs. Rate-Limit & Local Privacy Budgets:** Resolved via **Segmentation + Dynamics + Intermediary**. Instead of fixed-interval background timers, external refreshes are event-gated by `hc` transitions (`turn` increment, `status.state` entering `blocked`/`done`/`error`) and local `.git/HEAD` or `.git/refs` changes, decaying exponentially when idle.
* **Contradiction 2 — Zero-Friction Discovery vs. Deterministic Multi-Agent Accuracy:** Resolved via **Inversion + Self-Service + Extraction**. Instead of fuzzy text searches or scraping terminal scrollback (`hc read`), lightweight hooks installed by `cockpit setup` stamp deterministic provenance onto the terminal via `hc label <t> cockpit.repo=... cockpit.branch=... cockpit.issue=... cockpit.pr=...`. Correlation is exposed with explicit confidence tiers (`Exact`, `Inferred`, `Ambiguous`, or `Local / No PR`) rather than silent guessing.
* **Contradiction 3 — Rich GPUI Mod UX vs. Standalone Headless Linux Operation:** Resolved via **Extraction + Preliminary Action**. All ingestion, correlation, and caching live in the Rust `cockpit` binary/daemon, which materializes a versioned ViewModel snapshot consumed identically by `~/.config/herdr-app/mods/cockpit/main.js` and `cockpit status`.

### 4. UX, Developer Ergonomics & Progressive Disclosure (`comprehension`)
* **Live Worktree & Branch Correlation Cascade:** Agents frequently switch branches mid-session, operate in parallel `git worktree` directories, or run in scratch folders without a remote PR. Cockpit must re-evaluate correlation from `foreground.cwd` and `hc` labels dynamically, isolate parallel worktrees by exact worktree path + branch, and display a clean `Local / No PR` badge when no remote PR exists.
* **Tiered Credential Degradation:** Developers rarely use all five external tools simultaneously (e.g., on this machine `gh` is installed while `glab`, Jira, Linear, and Slack are not yet configured). Unconfigured integrations must render as **Inactive** (omitted or muted) rather than noisy error banners. Expired auth tokens surface once as a collapsible status hint with a copyable remediation command.
* **Two-Tier Read-Only Layout:** To avoid visual overload in `gpui-kit`, the primary list stays compact—pinning `blocked` agents at the top (ordered by `auth` > `permission` > `question`), followed by `working`/`idle`, and collapsing `finished` agents—showing only 2–3 key badges per row. Selecting an agent opens an inspector drawer with PR checks, reviews, deploys, releases, ticket state, Slack thread links, and matched local runbooks (`.cockpit/runbooks/`, `docs/runbooks/`, `RUNBOOK.md`). "Read-only" means zero remote mutations and zero automated input injection into agents, while allowing local focus jumps (`hc attach` / pane focus in `herdr-app`) and opening external URLs.

### 5. Migration, Host Hygiene & Headless Compatibility (`implementation_readiness`)
* **Safe, Reversible `cockpit setup`:** Modifying `~/.claude/settings.json`, `~/.config/opencode/opencode.json`, `~/.pi/agent/settings.json`, `~/.config/herdr-core/config.toml`, `~/.config/herdr-app/`, and shell RC files risks corrupting user configs or failing on symlinked/read-only dotfiles. `cockpit setup` must isolate hook scripts inside `~/.config/cockpit/hooks/`, use format-preserving surgical merges and marker-delimited shell blocks (`# >>> cockpit >>>`), record a manifest (`~/.local/state/cockpit/setup-manifest.json`), and support `--dry-run`, `--only`, `--verify`, and `cockpit uninstall` / `--undo`.
* **Headless Host Resilience:** When `herdr-app` is not installed on a Linux host (such as the current environment where `hc 0.1.20` is installed without `herdr-app`), `cockpit setup` must not fail; it installs or stages the mod files cleanly and runs the CLI/service standalone.
* **Zero-Blocking Hooks:** Shell and agent hooks must execute in `<20 ms` (emitting OSC 7501 sequences or fire-and-forget `hc label` updates) so a stopped `cockpit` service never slows down interactive shell prompts or coding agents.

### 6. Observability & Decoupled Architecture (`codebase_details` & `structural`)
* **Uniform `ContextProvider` Trait Boundary:** Each integration (`gh`, `glab`, Jira, Linear, Slack, local runbooks) is encapsulated behind an isolated Rust adapter trait returning normalized domain structs (`PullRequestRef`, `CiStatus`, `DeployReleaseRef`, `IssueRef`, `ThreadRef`, `RunbookRef`, `ProviderHealth`) with independent ETag/TTL caches and circuit breakers. Future email and meeting adapters plug into this same trait without core state-machine changes.
* **Local-Only Diagnostics (`cockpit doctor` & `cockpit inspect`):** Because off-machine telemetry is forbidden, Cockpit provides structured local JSONL logs (`$XDG_STATE_HOME/cockpit/cockpit.log`) with automatic secret redaction, a `cockpit doctor` command (verifying `hc` connectivity, hook signatures, mod schema version match, and provider auth/rate-limit health), and `cockpit inspect <terminal>` to show exact correlation provenance.

## Goals vs. Non-Goals

* **In-Scope Goals:**
  * Track live and finished coding agents via `herdr-core` (`hc events --json` / `hc connect` and `hc ls --all --json`), prioritizing OSC 7501 `blocked` agents (`permission`, `question`, `auth`).
  * Ship a native Herdr Desktop mod (`~/.config/herdr-app/mods/cockpit/main.js`) and layout (`~/.config/herdr-app/layouts/cockpit.json`) built with `gpui-kit` consuming a versioned ViewModel snapshot.
  * Build the `cockpit` Rust CLI binary and local background service (`cockpit daemon`, `cockpit status`, `cockpit watch`, `cockpit inspect`, `cockpit doctor`, `cockpit setup`, `cockpit uninstall`).
  * Implement initial read-only `ContextProvider` adapters for GitHub (`gh`), GitLab (`glab`), Jira, Linear, Slack, and local Markdown runbooks (`RUNBOOK.md`, `docs/runbooks/`, `.cockpit/runbooks/`).
  * Enforce strict local-only data storage (SQLite WAL cache / `hc` doc store in XDG state directories, OS keyring/env for tokens, zero external telemetry) and event-gated, deduplicated API fetching with quota reserve floors.
  * Provide an idempotent, reversible Linux setup command (`cockpit setup` with `--dry-run`, `--only`, `--verify`, and `--undo`) supporting shell, Claude Code, OpenCode, Pi, and Herdr Desktop.
* **Out-of-Scope Non-Goals:**
  * Email and calendar/meetings integrations in V1 (explicitly deferred to Phase 2 via extensible `ContextProvider` slots).
  * Mutating remote state (merging PRs, transitioning Jira/Linear tickets, posting Slack messages, or triggering CI/deploys).
  * Injecting prompts or bypassing permission gates in blocked agents (`hc send --force`).
  * Any cloud relay, external webhook server, or third-party analytics/telemetry.

## Alternatives Weighed

* **Standalone GPUI Desktop App (Separate Binary Window):** Rejected by author: Since the workflow already runs inside Herdr Desktop, shipping Cockpit as a native Herdr mod (`mods/cockpit/main.js` + layout) backed by the `cockpit` Rust CLI/service keeps all terminals and the fleet dashboard inside a single window while retaining standalone CLI usability.
* **Pure JS Mod Without a Rust CLI/Service (`main.js` Polling CLIs Directly):** Rejected by author: Running `hc`, `gh`, `glab`, and HTTP queries directly inside `herdr-app`'s JS mod loop risks UI frame drops, duplicates state across window reloads, and provides no standalone CLI/headless service or `cockpit setup` / `cockpit doctor` tooling on Linux hosts where `herdr-app` is absent or closed.
* **Fuzzy Full-Text Search Across Providers for Unmatched Agents:** Rejected by author: Calling GitHub Search (`30 req/min`) or Slack `search.messages` (`20 req/min`) across $N$ terminals exhausts user API quotas and produces false-positive correlations; deterministic local extraction (`foreground.cwd` git remote/branch, commit/branch regexes, and `hc label` provenance) with negative caching is used instead.
* **Including Email and Meetings in V1:** Rejected by author: Deferred to a later phase to keep V1 focused on core coding-agent context (`gh`/`glab`, Jira, Linear, Slack, and local runbooks).

## Key Decisions & Staging for Next Steps

1. **Architecture Split:** Rust `cockpit` binary/daemon owns `hc` event ingestion, deterministic correlation, SQLite WAL caching, and rate-limited `ContextProvider` adapters; it publishes a versioned `CockpitSnapshot` (`schema_version: 1`) over a local Unix socket, CLI (`cockpit status --json`), and `hc`'s `doc.set` store (`cockpit.snapshot`) for `~/.config/herdr-app/mods/cockpit/main.js`.
2. **Correlation Cascade:** `Exact` (`hc label` `cockpit.*` keys or unique worktree branch-to-PR match) $\rightarrow$ `Inferred` (branch/commit regex `PROJ-123` / `ENG-123` + repo runbooks) $\rightarrow$ `Local / No PR` (clean fallback with zero error noise).
3. **Rate-Limit & Quota Protection:** Deduplicate queries by canonical resource key, gate refreshes on `hc` state transitions (`blocked`, `done`, `turn` change) or `.git/HEAD` updates, freeze polling for `finished` agents, and pause background refreshes if provider quota drops below 25%.
4. **Reversible Setup:** `cockpit setup` writes self-contained hook scripts under `~/.config/cockpit/hooks/`, uses surgical config merges with a rollback manifest (`~/.local/state/cockpit/setup-manifest.json`), and validates health via `cockpit doctor`.
