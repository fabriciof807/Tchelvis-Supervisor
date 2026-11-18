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
         1. When your task is complete, report back before ending your turn:\n\
            tchelvis orch send --type worker_done --outcome succeeded --dispatch-id {dispatch_id} --body \"<concise summary of changes and verification>\"\n\
         2. If you fail or cannot complete the task, report failure:\n\
            tchelvis orch send --type worker_done --outcome failed --dispatch-id {dispatch_id} --body \"<failure reason>\"\n\
         3. If you need clarification from the coordinator, ask and then end your turn immediately (never poll or sleep):\n\
            tchelvis orch ask --dispatch-id {dispatch_id} --question \"<question>\"\n\
         4. If you receive a `[tchelvis] You have N orchestration message(s)...` pointer, run:\n\
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

1. **Create an orchestration run** (binds your current `$HC_TERMINAL_ID` as the coordinator):
   ```bash
   tchelvis orch run-create --title "Feature or refactor title"
   ```
2. **Define DAG tasks** (optional if dispatching one-off tasks directly):
   ```bash
   tchelvis orch task-create --run <run_id> --id task_a --title "Step A" --prompt "Implement X..."
   tchelvis orch task-create --run <run_id> --id task_b --title "Step B" --prompt "Verify X..." --deps task_a
   ```
3. **Dispatch ready tasks to child workers** (`claude`, `codex`, `gemini`, `opencode`, `pi`, or custom command; optionally in an isolated worktree via `--worktree new-child`):
   ```bash
   tchelvis orch dispatch --run <run_id> --task task_a --agent claude --worktree current
   ```
4. **End your turn immediately (Never Poll!)**:
   - Do **not** run `sleep`, `hc wait`, or `hc read` loops waiting for workers.
   - `tchelvis daemon` monitors child terminals and automatically injects a single-line `[tchelvis] You have N orchestration message(s) in run <run_id>...` pointer via `hc send --if idle` when a worker finishes (`worker_done`), asks a question (`ask`), blocks on a prompt, or stops silently (`1500ms` post-turn safety net).
5. **Check your inbox when woken**:
   ```bash
   tchelvis orch check --run <run_id>
   ```
6. **Reply to worker questions**:
   ```bash
   tchelvis orch reply --question-id <q_id> --body "Your answer here"
   ```
7. **Inspect run status and release finished workers (opt-in)**:
   ```bash
   tchelvis orch status --run <run_id>
   tchelvis orch release --dispatch-id <dispatch_id>
   ```

## Worker Rules

When spawned or attached as an orchestrated worker, follow the `[TCHELVIS ORCHESTRATION WORKER CONTRACT]` in your prompt:
- Report completion: `tchelvis orch send --type worker_done --outcome succeeded --dispatch-id <dispatch_id> --body "<summary>"`
- Report failure: `tchelvis orch send --type worker_done --outcome failed --dispatch-id <dispatch_id> --body "<reason>"`
- Ask a blocking question (then end your turn immediately): `tchelvis orch ask --dispatch-id <dispatch_id> --question "<question>"`
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
        assert!(preamble.contains("tchelvis orch ask --dispatch-id disp_01"));
        assert!(preamble.contains("tchelvis orch check --run run_01"));
        assert!(preamble.contains("[TASK PROMPT]\nImplement JWT validation and run cargo test."));
    }

    fn assert_skill_documents_subcommands(skill: &str) {
        assert!(skill.contains("description:"));
        for subcmd in [
            "run-create",
            "task-create",
            "dispatch",
            "check",
            "reply",
            "status",
            "release",
            "send",
            "ask",
        ] {
            assert!(
                skill.contains(&format!("tchelvis orch {subcmd}")),
                "skill markdown missing `tchelvis orch {subcmd}`"
            );
        }
    }

    #[test]
    fn test_orchestration_skill_markdown_documents_subcommands() {
        let skill = orchestration_skill_markdown();
        for subcmd in [
            "run-create",
            "task-create",
            "dispatch",
            "check",
            "reply",
            "status",
            "release",
            "worker_done",
            "ask",
        ] {
            assert!(
                skill.contains(subcmd),
                "skill markdown missing subcommand {subcmd}"
            );
        }
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
}
