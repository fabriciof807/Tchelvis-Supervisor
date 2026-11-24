---
name: to-tickets
description: Break an approved spec into small, self-contained implementation tickets with explicit dependencies and verifiable acceptance criteria.
---

# Spec to Tickets

Turn a spec (file path or conversation) into self-contained tickets. Write ticket titles and bodies in the language of the spec.

## 1. When not to use

If the change fits one session or one loop, say tickets are unnecessary and stop.

## 2. Read the spec and code

Read the full spec and the code it touches. Check existing file paths, symbols, and contracts on disk.

## 3. Open decisions

If the spec leaves an open decision (behavior, naming, interface, scope, or error handling), ask the user and wait. Never invent answers.

## 4. Slice rules

Divide the work into vertical slices. Each ticket must:
- Deliver something you can demo or verify on its own.
- Fit one clean session without requiring design decisions.
- List only the `files` it creates or modifies.

If building or testing leaves generated files the project does not ignore (caches, build output), the first ticket adds the ignore rules, and every ticket that runs code depends on it.

Put preparatory code changes first. For broad changes, expand, migrate in batches, and contract: build the new path first, move callers in small slices, then remove the old path.

## 5. Dependency rules

Use `blocked_by` only for real dependencies:
- If two tickets share any file (including a lockfile, changelog, or module index), serialize them with `blocked_by`.
- When unsure whether two slices conflict, add the dependency instead of running in parallel.

## 6. Final integration ticket

Always add a final `integration` ticket blocked by every earlier ticket. It runs the full suite and fixes what breaks when the pieces are together. Its `files` are the union of every other ticket's `files`.

## 7. Review and approve

Before writing any file, show:
1. The ticket list: title, `blocked_by`, what it demos, and the spec excerpt it covers.
2. What in the spec has no ticket (or confirm full coverage).

Ask the user to approve before writing files.

## 8. Write the files

Choose the target directory:
- `.loops/tickets/<feature>/NN-<slug>.md` if `.loops/` exists in the project.
- `.tickets/<feature>/NN-<slug>.md` otherwise.

If that directory already contains tickets, ask first and never overwrite.

Write one file per ticket using the template below. After writing, print one table with columns: `id`, `title`, `blocked by`, `files`.

## 9. Ticket template

Keep the frontmatter keys exact. Every acceptance criterion must fail before the work starts and name the command or observation that proves it.

```markdown
---
id: "01"
title: Short imperative title
status: ready          # ready | running | merged | failed
blocked_by: []
files: [src/a.rs, tests/a.rs]
workflow:              # optional workflow team
run_id:                # filled by the coordinator
---
## Goal
What starts working and how to demo it.

## From the spec
Copied spec excerpt and its source (the runner may not have the spec file).

## Start here
Files and contracts to inspect. If a file moved, the contract wins.

## Out of scope
What this ticket must not change.

## Acceptance criteria
- [ ] Criterion that fails before the work — proven by `<command or observation>`.

## Verification
`<command>`
```
