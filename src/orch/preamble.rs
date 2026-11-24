pub fn build_dispatch_preamble(
    run_id: &str,
    task_id: &str,
    dispatch_id: &str,
    parent_terminal: &str,
    depth: i32,
    task_prompt: &str,
) -> String {
    format!(
        "[TCHELVIS ORCHESTRATION WORKER CONTRACT]\n\
         Run ID: {run_id} | Task ID: {task_id} | Dispatch ID: {dispatch_id} | Parent: {parent_terminal} | Depth: {depth}/2\n\
         \n\
         Mandatory Rules:\n\
         1. Report `worker_done` exactly once per dispatch, only after all implementation and verification are complete (never send a preliminary `worker_done` followed by another), and then stop immediately (take no further turns unless woken by a coordinator directive):\n\
            - When your task is complete, report back and stop immediately with no further turns:\n\
              tchelvis orch send --type worker_done --outcome succeeded --dispatch-id {dispatch_id} --body \"<concise summary of changes and verification>\"\n\
            - If you fail or cannot complete the task, report failure and stop immediately with no further turns:\n\
              tchelvis orch send --type worker_done --outcome failed --dispatch-id {dispatch_id} --body \"<failure reason>\"\n\
         2. If you need clarification from the coordinator, ask and then end your turn immediately (never poll or sleep):\n\
            tchelvis orch ask --dispatch-id {dispatch_id} --question \"<question>\"\n\
         3. If you receive a `[tchelvis] You have N orchestration message(s)...` pointer, run:\n\
            tchelvis orch check --run {run_id}\n\
         \n\
         [TASK PROMPT]\n\
         {task_prompt}"
    )
}

pub const ORCH_SKILL_NAME: &str = "orchestration-tchelvis";

pub fn orchestration_skill_file_markdown() -> String {
    format!(
        "---\n\
         name: {ORCH_SKILL_NAME}\n\
         description: Delegate work to parallel child coding agents over Herdr terminals using Tchelvis M6 (`tchelvis orch`). Use when orchestrating multi-agent tasks, spawning workers in shared or isolated git worktrees, and waking automatically on completion without polling.\n\
         ---\n\
         \n\
         {}",
        orchestration_skill_markdown()
    )
}

pub fn orchestration_opencode_command_markdown() -> String {
    format!(
        "---\n\
         description: \"Orchestrate parallel child agents with Tchelvis (`tchelvis orch`)\"\n\
         ---\n\
         \n\
         {}\n\
         \n\
         User request: $ARGUMENTS\n",
        orchestration_skill_markdown().trim_end()
    )
}

pub fn orchestration_skill_markdown() -> &'static str {
    r#"# Tchelvis Multi-Agent Orchestration (`tchelvis orch`)

Tchelvis provides event-driven parent-child agent orchestration over Herdr (`hc`) terminals backed by a local SQLite mailbox (`orch.db`) and `tchelvis daemon`.

## Coordinator Workflow

1. **Quick one-shot launch** (creates a run bound to `$HC_TERMINAL_ID` when `--run` is omitted, preflights the agent binary on `PATH`, creates the task, and spawns the worker):
   ```bash
   tchelvis orch launch --agent claude --title "Implement JWT" --prompt "Implement X..." --worktree new-child
   ```
   `--agent` takes a preset or a full command, split with shell quoting: `--agent "agy --model 'GPT-OSS 120B (Medium)'"`. `agy`/`antigravity` get `-i` added so the prompt is read; a wrapper script with another name does not.
2. **Or build a multi-task DAG explicitly**:
   ```bash
   tchelvis orch run-create --title "Feature or refactor title"
   tchelvis orch task-create --run <run_id> --id task_a --title "Step A" --prompt "Implement X..."
   tchelvis orch task-create --run <run_id> --id task_b --title "Step B" --prompt "Verify X..." --deps task_a
   tchelvis orch dispatch --run <run_id> --task task_a --agent claude --worktree current
   ```
3. **End your turn immediately (Never Poll!)**:
   - Do **not** run `sleep`, `hc wait`, or `hc read` loops waiting for workers.
   - `tchelvis daemon` monitors child terminals and automatically injects a single-line `[tchelvis] You have N orchestration message(s) in run <run_id>...` pointer when a worker finishes (`worker_done`), asks a question (`ask`), blocks on a prompt, or stops silently (`1500ms` post-turn safety net). Delivery uses `hc send --if idle` when `idle` and re-checks via `hc get` before plain `hc send` when `done` (never while `working`, `blocked`, `error`, or `unknown`).
4. **Check your inbox when woken (or if no pointer arrives)**:
   - When woken by a `[tchelvis]` pointer, or if no pointer arrives and the user asks for results, run:
   ```bash
   tchelvis orch check --run <run_id>
   ```
5. **Reply to worker questions or message a running worker**:
   ```bash
   tchelvis orch reply --question-id <q_id> --body "Your answer here"
   tchelvis orch send --to dispatch:<dispatch_id> --body "Follow-up directive"
   ```
6. **Inspect run status and release finished workers (opt-in)**:
   ```bash
   tchelvis orch status --run <run_id>
   tchelvis orch release --dispatch-id <dispatch_id> [--force]
   ```
   - `release` closes the worker terminal and removes a `--worktree new-child` checkout (refusing dirty or unmerged worktrees unless `--force` is passed). Even with `--force`, `release` **keeps** the `orch/<run_id>/<task_id>` branch so unmerged commits are preserved; delete it manually with `git branch -D orch/<run_id>/<task_id>` when no longer needed.

## Recovery Path (`send -> retry -> stop`)

**Silence is never proof of exit.** A worker terminal returning to an idle prompt without sending `worker_done` may simply be waiting for follow-up input, paused after a partial turn, or dead. Inspect `tchelvis orch status --run <run_id>` and follow this recovery order:

1. **`send` (live worker needs a nudge or new instruction)**:
   ```bash
   tchelvis orch send --to dispatch:<dispatch_id> --body "Follow-up instruction"
   ```
   Delivers immediately when the worker is live and `idle` (via `--if idle`) or `done` (after `hc get` re-check, via plain `hc send`), or queues for delivery when busy, recording a `directive` in `orch.db`. Rejected if the worker terminal is already `finished` (use `retry` for retryable dispatches such as `stopped`, `failed`, or `needs_review`; if the dispatch already `completed`, launch a new task with `tchelvis orch launch --run <run_id> ...`).
2. **`retry` (worker stopped, failed, errored, or stalled)**:
   ```bash
   tchelvis orch retry <dispatch_id>
   ```
   Closes the old terminal if needed, marks the prior attempt `superseded`, cancels open questions, and launches a fresh dispatch for the same task (reusing any existing `--worktree new-child` checkout) while respecting the 3-failure circuit breaker. `completed` and `superseded` dispatches cannot be retried.
3. **`stop` (cancel an active or runaway worker cleanly)**:
   ```bash
   tchelvis orch stop <dispatch_id>
   ```
   Closes the worker terminal, marks the dispatch `stopped`, cancels open questions, and returns the task to a retryable state without incrementing `failure_count` and without deleting dirty or unmerged worktrees.

## Worker Rules

When spawned or attached as an orchestrated worker, follow the `[TCHELVIS ORCHESTRATION WORKER CONTRACT]` in your prompt:
- Report completion and stop immediately (no further turns unless woken by a coordinator directive): `tchelvis orch send --type worker_done --outcome succeeded --dispatch-id <dispatch_id> --body "<summary>"`
- Report failure and stop immediately (no further turns): `tchelvis orch send --type worker_done --outcome failed --dispatch-id <dispatch_id> --body "<reason>"`
- Ask a blocking question (then end your turn immediately): `tchelvis orch ask --dispatch-id <dispatch_id> --question "<question>"`

## Declarative Multi-Agent Loops (`tchelvis loop`)

For repeatable fixed topologies (`designer` / `planner` -> `worker` -> `tester` / `reviewer` -> `critic`), run a headless loop in one Herdr terminal and wake once on completion:

```bash
tchelvis loop check <workflow>
tchelvis loop run <workflow> "Implement and verify X..." --detach
# End your turn; when `[tchelvis] You have N orchestration message(s)...` wakes you:
tchelvis orch check --run <orch_run_id>
tchelvis loop result <run_id>
```

Operational limits and guarantees to know before launching or advising a user:
- **Never merges or pushes:** the loop commits only to a new local `loop/<workflow>-<run-id>` branch and never touches the user's checkout or `.gitignore`.
- **`auto_approve_in_sandbox` must be turned on by the user:** `~/.config/tchelvis/loops.toml` ships with `auto_approve_in_sandbox = false`; `tchelvis loop run` refuses until the user explicitly sets it to `true`.
- **`bwrap` shares the network and localhost:** `bubblewrap` is the default sandbox (no silent fallback); it blocks outside filesystem writes/reads and clean-mounts `$HOME`, but network access, `localhost` services, and abstract sockets remain reachable.
- **Three-level preflight before creating a workspace or terminal:** prior to creating anything, `run` runs free sandboxed CLI checks (`claude auth status`, `agy models`, `opencode models --standalone`), reuses recent valid `model_evidence.json` entries (12 h default; invalidated on stage failure or binary/account/sandbox change), and sends lean `Reply with exactly: OK` probes only when needed (`[preflight]` supports `mode = "auto" | "always" | "free"` and `valid_hours = 1..=168`; `check --live` probes and renews all). Repo-defined model choices also require prior confirmation (`tchelvis loop models confirm <workflow>` when running with `--detach` or non-interactively).
- **An exported `OPENCODE_API_KEY` is visible to every program started from that shell:** `tchelvis` never saves keys to disk; inside the loop it forwards `OPENCODE_API_KEY` only to `opencode` stages and probes, though exporting it in an interactive shell exposes it to any child process spawned from that shell.

## Running Tickets with Loops

- **Context:** Read only ticket frontmatter and the 5 lines of `tchelvis loop result <RUN_ID>`. Never read diffs or implementation files, never edit code, and never run the project's build or tests (the final integration ticket runs them in the sandbox).
- **One upfront approval:** Show the ticket list with each ticket's team, the branch `integration/<feature>`, and `max_concurrent` (default 2 in `loops.toml`), and ask once to create a dedicated worktree and merge there:
  ```bash
  git -c core.hooksPath=/dev/null -c core.fsmonitor=false worktree add -b integration/<feature> <worktree_path>
  ```
  Never touch `main` and never push.
- **Worktree preflight:** Run all loop commands from inside the worktree using each ticket's absolute path. Before the first launch, run `tchelvis loop list` and `tchelvis loop check <workflow>` there; if a workflow/team is missing or model confirmation is pending, stop and ask. Never edit `loops.toml`.
- **Team:** Use the ticket's `workflow`, or pick: `fast-worker-reviewer` (simple), `worker-reviewer-critic` (delicate), `planner-worker-tester-critic` (test-centric), `designer-worker-reviewer` (needs design). Only offer teams whose check passes: before asking for approval, run `tchelvis loop check <workflow>` in the repository, and where it fails (for example, no model for a role) propose another team and say why. Models come from config; the reviewer must not be weaker than the worker.
- **Launch:** Pick `ready` tickets whose `blocked_by` are all `merged` and whose `files` do not overlap active tickets, up to `max_concurrent`. Write `status: running` **before** launching, run:
  ```bash
  tchelvis loop run <workflow> --input-file <abs_ticket_path> --detach
  ```
  Write `run_id` right after. Repeat for every eligible ticket, then end your turn. If the launch fails, set that ticket back to `ready`. Refused by the concurrency limit while one of your loops is active: wait for its wake-up. Refused with none of yours active, or for any other reason: stop and report it.
- **On wake-up (one per loop):** Run `tchelvis orch check --run loop_<RUN_ID>` and `tchelvis loop result <RUN_ID>`. On `PASS`, list the changed names in the worktree with `git diff --name-only --no-renames HEAD...<branch>` (`<branch>` is the `BRANCH:` line; the `ARQUIVOS:` line is shortened when long).
  - Every name inside the ticket's `files`: run `git -c core.hooksPath=/dev/null -c core.fsmonitor=false merge --no-ff <branch>` in the worktree, set `status: merged`, launch the tickets it unblocked, and end your turn.
  - `BRANCH: (nenhuma)`: the ticket needed no change; nothing to merge, set `status: merged`.
  - A name outside `files`, or any `.gitattributes` or `.gitmodules` (they can run programs during a merge): do not merge; stop and ask the user.
  - Other status or merge conflict (undo with `git merge --abort`): set `status: failed`, launch nothing new, wait for active loops, and report in the user's language in at most 6 lines (ticket, `RUN_ID`, report).
- **Finish:** Launch an independent review of `integration/<feature>` with `tchelvis orch launch` (outside your context). Tell the reviewer to only read and never run the project's code: it is not in the sandbox. Then report in the user's language; merging into `main` is the user's decision.
- **Lost session:** Rebuild state from ticket frontmatter, `tchelvis loop list --runs`, and `tchelvis loop list --active`. Check `tchelvis loop status <RUN_ID>` for `running` tickets (if `running` has no `run_id`, inspect `tchelvis loop list --active`; if unclear, ask) before any new launch.
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_dispatch_preamble_contains_contract_and_prompt() {
        let preamble = build_dispatch_preamble(
            "run_01",
            "task_a",
            "disp_01",
            "t1",
            1,
            "Implement JWT validation and run cargo test.",
        );
        assert!(preamble.contains("[TCHELVIS ORCHESTRATION WORKER CONTRACT]"));
        assert!(preamble.contains(
            "Run ID: run_01 | Task ID: task_a | Dispatch ID: disp_01 | Parent: t1 | Depth: 1/2"
        ));
        assert!(preamble.contains(
            "tchelvis orch send --type worker_done --outcome succeeded --dispatch-id disp_01"
        ));
        assert!(preamble.contains(
            "tchelvis orch send --type worker_done --outcome failed --dispatch-id disp_01"
        ));
        assert!(preamble.contains("stop immediately"));
        assert!(preamble.contains("no further turns"));
        assert!(preamble.contains("tchelvis orch ask --dispatch-id disp_01"));
        assert!(preamble.contains("tchelvis orch check --run run_01"));
        assert!(preamble.contains("[TASK PROMPT]\nImplement JWT validation and run cargo test."));
    }

    fn assert_skill_documents_subcommands(skill: &str) {
        assert!(skill.contains("description:"));
        for subcmd in [
            "launch",
            "run-create",
            "task-create",
            "dispatch",
            "check",
            "reply",
            "status",
            "release",
            "send",
            "retry",
            "stop",
            "ask",
        ] {
            assert!(
                skill.contains(&format!("tchelvis orch {subcmd}")),
                "skill markdown missing `tchelvis orch {subcmd}`"
            );
        }
        assert!(skill.contains("tchelvis orch send --to dispatch:"));
        assert!(skill.contains("send -> retry -> stop"));
        assert!(skill
            .to_ascii_lowercase()
            .contains("silence is never proof of exit"));
        assert!(skill
            .to_ascii_lowercase()
            .contains("if no pointer arrives and the user asks for results"));
    }

    #[test]
    fn test_orchestration_skill_markdown_documents_subcommands() {
        let skill = orchestration_skill_markdown();
        for subcmd in [
            "launch",
            "run-create",
            "task-create",
            "dispatch",
            "check",
            "reply",
            "status",
            "release",
            "retry",
            "stop",
            "--to dispatch:",
            "worker_done",
            "ask",
            "send -> retry -> stop",
        ] {
            assert!(
                skill.contains(subcmd),
                "skill markdown missing subcommand {subcmd}"
            );
        }
        assert!(skill
            .to_ascii_lowercase()
            .contains("silence is never proof of exit"));
        assert!(skill
            .to_ascii_lowercase()
            .contains("if no pointer arrives and the user asks for results"));
        assert!(skill.contains("no further turns"));
        assert!(skill.contains("git branch -D orch/<run_id>/<task_id>"));
    }

    #[test]
    fn test_orchestration_skill_file_markdown_has_frontmatter_and_subcommands() {
        let skill = orchestration_skill_file_markdown();
        assert!(skill.starts_with("---\n"));
        assert!(skill.contains("name: orchestration-tchelvis"));
        assert_skill_documents_subcommands(&skill);
    }

    #[test]
    fn test_orchestration_opencode_command_markdown_has_frontmatter_and_subcommands() {
        let command = orchestration_opencode_command_markdown();
        assert!(command.starts_with("---\n"));
        assert!(command.contains("description: \"Orchestrate parallel child agents"));
        assert!(command.ends_with("User request: $ARGUMENTS\n"));
        assert_skill_documents_subcommands(&command);
    }

    #[test]
    fn orchestration_skill_documents_running_tickets() {
        let skill = orchestration_skill_markdown();
        let idx = skill
            .find("## Running Tickets with Loops")
            .expect("missing '## Running Tickets with Loops' section");
        let section = &skill[idx..];
        assert!(
            section.lines().count() <= 70,
            "section has {} lines, expected <= 70",
            section.lines().count()
        );
        for needle in [
            "integration/<feature>",
            "git -c core.hooksPath=/dev/null -c core.fsmonitor=false worktree add",
            "--input-file",
            "--detach",
            "loop_<RUN_ID>",
            "git -c core.hooksPath=/dev/null -c core.fsmonitor=false merge --no-ff",
            "git merge --abort",
            "status: running",
            "status: merged",
            "status: failed",
            "end your turn",
            "tchelvis loop list --active",
            "max_concurrent",
            "ARQUIVOS",
            "BRANCH: (nenhuma)",
            "git diff --name-only --no-renames HEAD...<branch>",
            ".gitattributes",
            ".gitmodules",
            "any other reason",
            "never run the project's code",
            "Only offer teams whose check passes",
            "never push",
            "fast-worker-reviewer",
            "worker-reviewer-critic",
            "planner-worker-tester-critic",
            "designer-worker-reviewer",
            "tchelvis orch launch",
        ] {
            assert!(
                section.contains(needle),
                "Running Tickets with Loops section missing '{needle}'"
            );
        }
        let lower = section.to_ascii_lowercase();
        assert!(!lower.contains("sleep"), "section must not contain 'sleep'");
        assert!(!lower.contains("--yes"), "section must not contain '--yes'");
    }
}
