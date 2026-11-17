# Pending Prompt Region Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Find the pending request by screen structure, check hc `msg` for destructive commands, and give the LLM the current request apart from earlier output.

**Architecture:** All screen parsing stays in `src/supervisor/guardrails.rs`; the watcher passes `session.msg`; `ollama.rs` builds the split payload.

**Tech Stack:** Rust, `regex`, `serde_json`.

**Spec:** `docs/superpowers/specs/2026-11-17-pending-prompt-region-design.md`

## Global Constraints

- Dialog rule: `^\s*─{20,}\s*$` (U+2500 only; `╌` lines are not rules), region ≤ 30 lines after it.
- Fallback window: 8 non-empty lines.
- Test fixtures use anonymized paths (repo is public).

## Review Focus

- Claude idle screen (input box rules, not blocked): region is the footer; nothing alerts. Covered by existing watcher tests (not blocked → NoOp).
- Simulated `[y/N]` prompts with the command inline: fast path still approves (existing tests).
- `sequencia` screen: the region contains only the step-3 prompt.
- Destructive command only in `msg`: critical.

---

### Task 1: Structural region + msg-aware destructive detection

**Files:** Modify `src/supervisor/guardrails.rs`, `src/supervisor/watcher.rs:361`

**Interfaces:**
- Produces: `pub fn pending_prompt_split(output: &str) -> (String, String)` → `(earlier, pending)`; `pending_prompt_area` returns `.1`.
- Produces: `pub fn enforce_blocked_policy(is_blocked: bool, output: &str, msg: Option<&str>, eval) -> CognitiveEvaluation`.

- [ ] Tests (fail first): `destructive_detected_in_tall_claude_dialog` (CLAUDE_TOUCH with `touch` → `rm -rf`, `detect_destructive(..).is_some()`); `sequencia_region_holds_only_current_prompt`; `blocked_policy_uses_hc_msg_for_destructive` (output without command, msg `approve Bash: rm -rf build` → Critical, payload `n`).
- [ ] Implement; run `cargo test --quiet`; commit.

### Task 2: Split LLM payload

**Files:** Modify `src/supervisor/ollama.rs` (`SYSTEM_PROMPT`, `build_user_prompt_payload`, its test)

- [ ] Test: payload has `pending_prompt` = `"Continue? [y/N]"` and `earlier_output` = `"running 12 tests..."`, no `recent_hc_output`.
- [ ] Implement with `pending_prompt_split`; update prompt text; `cargo test --quiet`; commit.
