# Pending prompt region — design (short)

Date: 2026-11-17. Approved by the user in chat ("junte as duas coisas no mesmo push").

## Why

The supervisor finds the pending request by taking the **last 8 non-empty lines** of the screen
(`guardrails::pending_prompt_area`). Two problems, both from host test 12 screens:

1. **Claude's real permission dialog is taller than 8 lines.** In
   `src/supervisor/fixtures/claude_permission_bash_touch.txt` the command line is the 9th line
   from the bottom. A destructive command shown there (`rm -rf …`) is not detected, so the alert is
   `normal` instead of `critical`. Nothing is approved (the LLM never approves permissions, the
   fixed rule reads `status.msg`), but the urgency is wrong.
2. **The LLM receives the whole 25-line capture** as one block, with answered prompts from earlier
   steps (the `sequencia` scenario) mixed with the current one.

## Goals

- The prompt region is found by screen structure, not a fixed line count.
- Destructive detection for Action Cards also checks hc's `status.msg` (`approve Bash: <cmd>`).
- The LLM gets the current request and the earlier output as separate fields, with a rule that
  only the current request may produce an alert.

## Non-goals

- No change to the fixed read-only rule's allowlist or to what the LLM may send.
- No change to `capture_lines` (25).

## Approaches

- **A. Structural region (chosen).** Claude's dialog starts after a solid `─` rule line; use the
  lines after the last such rule. Other agents: the last 8 lines, cut after the previous prompt
  line when an earlier answered prompt is still inside them.
- **B. Bigger fixed window (e.g. 15 lines).** Rejected: pulls more stale prompts into the region
  for the simulated agents and still breaks for a long enough command.
- **C. Trust only `status.msg`.** Rejected: only Claude Code sends it; codex/opencode/pi do not.

## Design

- `pending_prompt_area(output)`:
  - Non-empty lines. Window: the last 8 lines; if two or more prompt lines (`[y/N]`-style or
    prompt signature) are inside, it starts after the second-to-last one.
  - If a line matching `^\s*─{20,}\s*$` exists with at most 30 lines after it, and the line after
    it starts earlier than the window, the region starts there. The rule only widens the region:
    a footer rule below the request never hides it (found in self-review).
- `detect_destructive_with_msg(output, payload, msg)`; `enforce_blocked_policy` takes the hc
  `msg` and uses it. The watcher passes `session.msg`.
- LLM payload: `pending_prompt` (the region) and `earlier_output` (the lines before it) replace
  `recent_hc_output`. System prompt: decide only on `pending_prompt`; `earlier_output` is context
  and may contain prompts already answered.

## Left to the builder

Helper names inside `guardrails.rs`; exact prompt wording.
