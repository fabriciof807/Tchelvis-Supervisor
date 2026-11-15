# Read-only fast path (deterministic approve before the LLM) — design

**Date:** 2026-11-15 · **Status:** approved by the user · **Size:** small change

**Update (test 7):** the command may also be inline in the prompt line (`Allow bash tool: <cmd> ? [y/N]`); a `Read` tool on one non-sensitive file counts as `cat`. Write/Edit/other tools are never fast-pathed.

## Intent
Test 5: Semi-Auto approved only 1 of 4 trivial read-only requests (`ls`, `cat`, `git status`) —
the 1.7B model alerts too often and takes 40–245 s. Approve the trivial ones with a fixed rule,
before and without the LLM, so Semi-Auto does its job and is predictable.

## Rule (all must hold, else the normal LLM flow runs)
- Mode is Semi-Auto; the session is blocked; its hc kind is not `question`; the loop breaker
  (`consecutive_auto_replies < max_auto_replies`) allows a reply.
- The pending prompt area holds a yes/no prompt (`[y/N]`, `(y/n)`, `Continue?`, `Proceed?`) →
  payload `y`, or a permission menu with a `1. Yes` option → payload `1`.
- No destructive command in the prompt area (`detect_destructive`).
- The command is the nearest non-empty line above the prompt line (header-only lines such as
  `Bash command` skipped; prefixes `$ `, `> `, `❯ `, `Bash(`, `Run:` and a trailing `)` stripped).
- It is a single command: no `|`, `;`, `&&`, `||`, `>`, `<`, `$(`, backtick.
- It starts with `ls`, `cat`, `head`, `tail`, `wc`, `pwd`, `rg`, `grep`, `git status`,
  `git diff`, `git log` or `git show`.
- It does not mention a sensitive path (case-insensitive): `.ssh`, `.gnupg`, `.env`, `id_rsa`,
  `id_ed25519`, `secret`, `token`, `credential`, `password`, `key`, `/etc/shadow`.

## Approaches
- **Fixed rule before the LLM (chosen):** fast, predictable, works with the LLM down.
- Prompt-only tuning: still model-dependent and slow. Rejected.

## Known risk
The command is read from the screen; a screen that imitates a prompt could be misread. The
single-command, nearest-line and sensitive-path rules reduce this; destructive detection still
runs first.
