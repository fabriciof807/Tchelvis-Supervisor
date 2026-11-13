# Goldfish Review Report — Stage 7: Critic Review

**Target:** `/home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md` (`1-225`)
**Goldfish Context:** `["/home/ubuntu/.t3/projects/cockpit/concepts_brief.md"]`

## Reviewer Status

| Reviewer | Status |
| --- | --- |
| `egm-design` | completed |
| `egm-structural` | completed |
| `egm-style` | completed |

## Prioritized Findings (Blockers -> Major -> Nits)

### 1. `hc ls --all --json` Schema Mismatch & Missing `foreground.cwd` Event - Severity: Blocker
* **Reviewer Lens:** `egm-design`
* **Affected Scope:** `Goals` ([cockpit.md:28](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L28)), `High-Level Architecture` ([cockpit.md:78](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L78)), `Event-Gated Refresh Cadence` ([cockpit.md:166](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L166))
* **Discrepancy Description:**
  1. In `herdr-core 0.1.20`, `hc ls --all --json` returns compact summary rows (`address`, `agent`, `cwd`, `state`, `turn`, `where`, `exit_code`, `reason`) and omits `reported`, `labels`, `foreground`, `status`, `argv`, and `parent` (which are returned by `hc get <t> --json` or `terminal.list` over `hc connect` / `events.subscribe` initial snapshot).
  2. `hc events --json` (`events.subscribe`) emits `(created, lost, restarted, keep, finished, removed, status, turn, title, labels, size, doc)` and does not emit a separate `foreground.cwd` change event.
* **Rationale & Recommendation:** Use `hc events --json` (whose initial `snapshot` event includes full `Terminal` objects with `reported`, `labels`, `status`, `parent`, `argv`) and `hc get <t> --json` / `hc connect` (`terminal.list`) for full terminal state reconciliation rather than relying on `hc ls --all --json` alone for `reported`/`labels`/`foreground.cwd`. Detect `cwd`/branch switches via `labels` events, `turn`/`status` transitions, and periodic `hc get` / `.git/HEAD` inspection on active terminals.

### 2. Shell Hook Self-Defeat of Plain-Shell Filtering & OSC 7501 Collision - Severity: Blocker
* **Reviewer Lens:** `egm-design`
* **Affected Scope:** `The Problem` ([cockpit.md:20](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L20)), `Goals` ([cockpit.md:28](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L28)), `Source Provenance & Hook Layer` ([cockpit.md:71-74](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L71-L74), [cockpit.md:126](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L126))
* **Discrepancy Description:**
  1. Line 28 filters out plain shells by checking `reported == false` without `cockpit.*` labels, yet lines 72–73 install hooks in interactive shells (`bash`, `zsh`, `fish`, `nushell`) that emit OSC 7501 and stamp `cockpit.cwd`, `cockpit.repo`, `cockpit.branch`, and `cockpit.head`. Stamping `cockpit.*` labels from every interactive shell prompt causes every plain shell to pass the inclusion filter.
  2. Emitting root OSC 7501 from shell prompts races with native agent OSC 7501 reports, and a `<20 ms` offline hook cannot resolve `cockpit.pr` without network calls.
* **Rationale & Recommendation:** Do not emit OSC 7501 or `cockpit.*` labels automatically on every plain interactive shell prompt. Reserve automatic OSC 7501 / `cockpit.agent` / `cockpit.cwd` hook emission for coding agent hooks (Claude Code, OpenCode, Pi), and provide an explicit shell helper function (`cockpit-track` / `cockpit label`) for users who want to tag a shell terminal manually (`cockpit.issue`, `cockpit.pr`, `cockpit.slack`).

### 3. Dual-Transport Ambiguity, `<256 KiB` `doc.set` Cap, and Hysteresis Placement - Severity: Blocker
* **Reviewer Lens:** `egm-design`
* **Affected Scope:** `Overview` ([cockpit.md:12](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L12)), `CUJ 2` ([cockpit.md:57](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L57)), `High-Level Architecture` ([cockpit.md:82-91](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L82-L91), [cockpit.md:134-137](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L134-L137)), `Non-Functional Requirements` ([cockpit.md:180](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L180))
* **Discrepancy Description:**
  1. `main.js` is wired to both `hc` `doc` store (`cockpit.snapshot` `<256 KiB`) and `cockpit.sock` without defining primary vs. fallback ownership or how `doc.set` CAS `version` conflicts and `<256 KiB` size bounds are enforced when 72 hours of finished agents accumulate.
  2. Line 90 places the 1.5s hysteresis filter in the "stateless" `main.js` mod while line 280 places it in `src/herdr.rs`, and a 1.5s delay before entering `blocked` contradicts CUJ 2's `<250 ms` snapshot update target.
* **Rationale & Recommendation:** Place asymmetric clear-hysteresis strictly inside `src/herdr.rs` (entering `blocked` immediately within `<250 ms` p99, while holding `blocked` for 1.5s against transient clear flickers). Define `hc` `doc.set` (`cockpit.snapshot`) as a bounded compact fleet projection (capping finished rows and reading `doc.get` `version` before `doc.set`) and `cockpit.sock` / `cockpit status --json` as the full snapshot + detail transport.

### 4. Missing `## Risks & Mitigations` Section in `FeatureRange` - Severity: Major
* **Reviewer Lens:** `egm-design`
* **Affected Scope:** `FeatureRange` ([cockpit.md:1-225](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L1-L225))
* **Discrepancy Description:** The Feature Specification Standard requires an explicit `Risks & Mitigations` section calling out failure modes, security boundaries, and scalability limits (such as stale socket recovery on daemon crash, `$XDG_RUNTIME_DIR` fallback permissions, ANSI/OSC escape injection in external PR/issue titles, and headless `herdr-app` absence).
* **Rationale & Recommendation:** Add a `## Risks & Mitigations` section in `FeatureRange` covering daemon crash/stale-socket recovery, ANSI/OSC control-character sanitization on untrusted strings, `$XDG_RUNTIME_DIR` fallback (`~/.local/state/cockpit/run` with `0700` mode), and headless operation when `herdr-app` is absent.

### 5. Provider Rate-Limit Header Differences & Jira/Slack Auth Details - Severity: Major
* **Reviewer Lens:** `egm-design`
* **Affected Scope:** `Goals` ([cockpit.md:31](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L31)), `Event-Gated Refresh Cadence` ([cockpit.md:168](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L168)), `Security & Privacy` ([cockpit.md:176](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L176))
* **Discrepancy Description:**
  1. Slack Web API and some Jira/GitLab endpoints do not return remaining-quota headers on `200 OK` responses, relying instead on HTTP `429 Retry-After` and local token buckets.
  2. Jira Cloud REST v3 requires `jira_email` + API token (Basic Auth) or Bearer PAT (Server/DC) alongside `jira_base_url`.
* **Rationale & Recommendation:** Specify header-based 25% quota reserve floors for providers that return quota headers (GitHub, Linear, GitLab) combined with local token-bucket rate limits and `429 Retry-After` circuit breakers for all providers (including Slack and Jira), and support optional `COCKPIT_JIRA_EMAIL` / `jira_email` alongside `COCKPIT_JIRA_TOKEN` and `jira_base_url`.

### 6. Correlation Edge Cases (Unpushed Commits, Regex False Positives, Remote URL Normalization) - Severity: Major
* **Reviewer Lens:** `egm-design`
* **Affected Scope:** `Core Data Structures & Domain Model` ([cockpit.md:146-151](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L146-L151)), `Correlation Precedence` ([cockpit.md:160-164](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L160-L164))
* **Discrepancy Description:**
  1. Requiring `git_head_sha` to match the remote PR SHA for Tier 1 `Exact` demotes an open PR whenever the agent makes an unpushed local commit on the PR branch.
  2. The ticket regex `\b[A-Z][A-Z0-9_]{1,9}-\d+\b` matches common technical tokens (`UTF-8`, `SHA-256`, `CVE-2026`, `HTTP-2`).
  3. Git remote URLs (`git@github.com:org/repo.git` vs `https://github.com/org/repo`) must be normalized to canonical `(host, owner, repo)`.
* **Rationale & Recommendation:** Normalize git remotes to `(host, owner, repo)`, match Tier 1 `Exact` PRs by `(host, owner, repo, git_branch)` (or explicit `cockpit.pr` label), and exclude standard technical prefixes (`UTF`, `SHA`, `ISO`, `RFC`, `CVE`, `HTTP`, `TLS`, `SSH`, `OSC`, `PR`, `MR`) from inferred ticket regex matches.

### 7. Heading Hierarchy & Section Ordering in `FeatureRange` - Severity: Major
* **Reviewer Lens:** `egm-structural`
* **Affected Scope:** `Non-Functional Requirements`, `Alternatives Considered`, `Additional Resources`, `Future Improvements` ([cockpit.md:172-224](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L172-L224))
* **Discrepancy Description:**
  1. `### Additional Resources` (line 214) and `### Future Improvements` (line 219) use `H3` instead of `H2`, causing them to nest under `## Alternatives Considered`.
  2. `## Non-Functional Requirements` (line 172) appears before `## Alternatives Considered` (line 186), whereas the EGM template orders `Alternatives Considered` before `Non-Functional Requirements`.
* **Rationale & Recommendation:** Promote `## Additional Resources` and `## Future Improvements` to `H2` (or keep them aligned after `## Non-Functional Requirements`) and place `## Alternatives Considered` before `## Non-Functional Requirements`.

### 8. Claim Calibration, Redundant Enumerations, and Promotional Modifiers - Severity: Major
* **Reviewer Lens:** `egm-style`
* **Affected Scope:** `Overview`, `The Problem`, `Goals`, `Open Questions`, `CUJs`, `Technical Plan`, `Alternatives Considered` ([cockpit.md:11-210](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L11-L210))
* **Discrepancy Description:**
  1. `CALIBRATE`: Lines 15 and 31 pair "Everything stays on the user's machine" / "Strict Local-Only Privacy" with outbound API calls to GitHub, GitLab, Jira, Linear, and Slack. State accurately that state storage, correlation, and logs stay on-machine with zero third-party telemetry, while outbound requests are read-only calls to configured providers.
  2. `CALIBRATE` / `RELOCATE`: Move the `<250 ms` local event-to-snapshot propagation target into `Non-Functional Requirements` as a `p99` target and replace absolute claims ("never", "ever", "guarantees") with bounded engineering targets.
  3. `DROP`: Remove meta-process chatter in `Open Questions` ("all initial scope... resolved during brainstorming") and strip redundant "Rejected by author:" prefixes inside `Reason for Rejection` bullets and promotional adverbs ("natively", "surgically", "cleanly", "reversibly").

### 9. Mermaid Diagram Data Return Path & List Formatting - Severity: Nit
* **Reviewer Lens:** `egm-structural`, `egm-design`
* **Affected Scope:** `The Problem` ([cockpit.md:20-22](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L20-L22)), `High-Level Architecture` ([cockpit.md:93-139](file:///home/ubuntu/.t3/projects/cockpit/docs/designs/cockpit.md#L93-L139))
* **Discrepancy Description:** In the Mermaid diagram, `Coord` does not show an arrow feeding enriched context into snapshot publication (`SOCK`/`DOC`), and bullet spacing in `The Problem` uses `* ` instead of consistent list spacing.
* **Rationale & Recommendation:** Update the Mermaid diagram edges so `Coord` feeds into snapshot publication and normalize Markdown list spacing and Unicode arrows (`→`).
