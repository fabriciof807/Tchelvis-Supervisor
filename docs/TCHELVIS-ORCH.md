# Tchelvis orchestration: a guide for newcomers

This guide explains `tchelvis orch` (coordinator and worker agents) and
`tchelvis loop` (fixed multi-agent pipelines). It assumes you know what a
coding agent is, but nothing about Tchelvis. The reference for every flag is
`tchelvis <command> --help`; this page tells you which command to use and why.

## What it is, and when to use each part

| You want to... | Use | Why |
| :--- | :--- | :--- |
| Hand parts of a task to other agents in their own terminals, and talk to them while they work | `tchelvis orch` | Interactive workers you can inspect, nudge, answer and retry. |
| Run a fixed sequence such as worker, reviewer, critic, in clean headless sessions, with no supervision | `tchelvis loop` | Repeatable pipeline in a sandbox; you get one wake-up at the end. |

They relate like this: a coordinator agent can launch workers with `orch` and
start loops with `loop`. A loop is not a task of an `orch` run, but its result
arrives in the same mailbox. In both cases the coordinator
**ends its turn** and is woken by `tchelvis daemon`. Nobody polls.

## Pieces

- **Coordinator**: the agent that plans the work. It runs in a Herdr terminal
  and is bound to a **run**.
- **Worker**: a child agent (`claude`, `codex`, `gemini`, `opencode`, `pi`,
  `agy`, or any command) that runs one **dispatch** of one **task**.
- **Run**: one orchestration session, identified by `run_id`. It holds the task
  graph, dispatches and messages.
- **Task**: a node in the run's DAG, with a prompt and optional `--deps`.
- **Dispatch**: one attempt to run a task in a worker terminal. `retry` creates
  a new dispatch; the old one is `superseded`.
- **Mailbox**: messages (`worker_done`, `escalation`, `status`, questions,
  directives) stored in `~/.local/share/tchelvis/orch.db` (SQLite).
- **Daemon**: `tchelvis daemon`. It reads the mailbox and wakes the right
  terminal with a one-line pointer. It makes no LLM decisions.
- **Herdr terminals**: each worker, and each loop, lives in a Herdr terminal
  (`hc` is the Herdr CLI). You can attach to any of them to watch.

## Flow

```text
coordinator: launch / dispatch ──► worker runs in its own terminal
coordinator: END TURN (no sleep, no hc read loops)
worker:      worker_done | ask | (blocked or silent)
daemon:      sees the event ──► wakes coordinator: "[tchelvis] You have N message(s)..."
coordinator: orch check --run <id> ──► reads results, replies, or dispatches more
```

Silence is never proof of exit. A worker that goes quiet is handled by the
recovery path described below, not by assuming it finished.

## First use, step by step

1. **Install and set up.** Build or install `tchelvis`, then run
   `tchelvis setup` (add `--set-default-layout` to make it the Herdr Desktop
   default). The daemon starts by itself when needed. `tchelvis doctor` checks `hc`, `orch.db`, the daemon and the
   skill.
2. **Install the skill.** `tchelvis setup` installs `/orchestration-tchelvis`
   for supported agents. Inside your coordinator agent, run
   `/orchestration-tchelvis`, or print the guide with `tchelvis orch skill`.
3. **Run a one-shot launch.** From the coordinator's Herdr terminal:

   ```bash
   tchelvis orch launch --agent claude --title "Implement JWT" \
     --prompt "Add JWT middleware and run cargo test." --worktree new-child
   ```

   This creates a run bound to your terminal, a task and a worker. Then the
   coordinator ends its turn.
4. **Wait for the wake-up.** The daemon sends a pointer when the worker
   finishes or asks a question. Then run `tchelvis orch check --run <run_id>`.

## `tchelvis orch` commands

Every mutating command accepts `--json`. Most also accept `--request-id <id>`
(an idempotency receipt), except `release`.

| Command | Purpose | Main flags | Example |
| :--- | :--- | :--- | :--- |
| `launch` | Run preflight, create run (optional), task and dispatch in one step | `--agent`, `--prompt`, `--title`, `--run`, `--worktree current\|new-child` | `tchelvis orch launch --agent codex --prompt "Fix the parser"` |
| `run-create` | Create a run bound to the coordinator terminal | `--title`, `--coordinator` | `tchelvis orch run-create --title "Auth refactor"` |
| `task-create` | Add or update a DAG task | `--run`, `--id`, `--title`, `--prompt`, `--deps <csv\|json>` | `tchelvis orch task-create --run r1 --id b --title "Test" --prompt "..." --deps a` |
| `dispatch` | Start a worker for a task, or attach an existing terminal | `--run`, `--task`, `--agent` or `--terminal`, `--worktree` | `tchelvis orch dispatch --run r1 --task a --agent claude --worktree new-child` |
| `check` | Read and acknowledge pending messages for your terminal | `--run`, `--terminal`, `--peek` (read without acknowledging) | `tchelvis orch check --run r1` |
| `status` | Show tasks, dispatches, messages, open questions and unread counts | `--run`, `--json` | `tchelvis orch status --run r1` |
| `reply` | Answer a worker question and wake the worker | `--question-id`, `--body` | `tchelvis orch reply --question-id q1 --body "Use 15m expiry."` |
| `send` (report) | Worker reports to its parent | `--type worker_done\|escalation\|status`, `--outcome succeeded\|failed`, `--dispatch-id`, `--body` | `tchelvis orch send --type worker_done --outcome succeeded --dispatch-id d1 --body "Tests pass"` |
| `send` (directive) | Coordinator sends a message to a running worker | `--to dispatch:<id>`, `--body` | `tchelvis orch send --to dispatch:d1 --body "Also cover refresh."` |
| `ask` | Worker asks a blocking question, then ends its turn | `--dispatch-id`, `--question` | `tchelvis orch ask --dispatch-id d1 --question "15m or 1h?"` |
| `retry` | Supersede an attempt and launch a new one for the same task | `<dispatch_id>` | `tchelvis orch retry d1` |
| `stop` | Close the worker terminal, mark the dispatch stopped, make the task retryable | `<dispatch_id>` | `tchelvis orch stop d1` |
| `release` | Opt-in cleanup of a finished worker terminal and its `new-child` worktree | `--dispatch-id`, `--force` | `tchelvis orch release --dispatch-id d1` |
| `skill` | Print the full Markdown guide for coordinators and workers | none | `tchelvis orch skill` |

### Agent presets

`--agent` takes a preset (`claude`, `claude-code`, `codex`, `gemini`,
`opencode`, `pi`, `agy` or `antigravity`) or a full command, split with POSIX
shell quoting. Put extra flags inside the same string:

```bash
tchelvis orch launch --agent "agy --model 'GPT-OSS 120B (Medium)'" --prompt "..."
```

`agy` and `antigravity` get `-i` added, because agy reads the prompt only
through `-i`. Any other command receives the prompt as its last positional
argument.

## Dependencies, worktrees and release

- **Dependencies.** A task with `--deps a,b` becomes `ready` when those tasks
  complete. Nothing is dispatched automatically: the coordinator dispatches it.
- **Worktrees.** `--worktree current` (the default) runs the worker in the
  current checkout. `--worktree new-child` gives it its own git worktree and
  branch `orch/<run_id>/<task_id>`. Use `new-child` when workers run in
  parallel; merging the branches is up to you.
- **Release.** `release` closes a finished worker's terminal and removes a
  `new-child` worktree. It refuses dirty or unmerged worktrees unless you pass
  `--force`. Even with `--force`, it keeps the `orch/<run_id>/<task_id>`
  branch. Delete that branch yourself when you no longer need it.

## Questions and messages

- A worker asks with `orch ask`, then ends its turn. The coordinator sees the
  question in `orch check` and answers with `orch reply`. The reply wakes the
  worker.
- A coordinator nudges a running worker with `orch send --to dispatch:<id>`.
- Workers never poll. They run `orch check` only when they receive the pointer
  `[tchelvis] You have N orchestration message(s)...`.

## Recovery: `send -> retry -> stop`

Use this order when a worker has not reported:

1. **`send`**: a live worker that needs a nudge or new instruction.
   `tchelvis orch send --to dispatch:<id> --body "..."`
2. **`retry`**: a worker that stopped, failed or stalled. It marks the old
   attempt `superseded` and launches a new one. There is a circuit breaker after
   three failures. Completed or superseded dispatches cannot be retried.
   `tchelvis orch retry <dispatch_id>`
3. **`stop`**: cancel a runaway worker cleanly. The task becomes retryable and
   dirty worktrees are kept. `tchelvis orch stop <dispatch_id>`

Check `tchelvis orch status --run <run_id>` before choosing one.

The daemon also helps with two safety nets. If a worker returns to its prompt
for 1500 ms without `worker_done` or `ask`, the coordinator gets an escalation
with the last 25 sanitized lines. If a worker stays blocked on a prompt for
3000 ms, the coordinator gets an escalation too.

## `tchelvis loop` for coordinators

A coordinator can start a fixed pipeline with one terminal and one wake-up:

```bash
tchelvis loop check worker-reviewer-critic
tchelvis loop run worker-reviewer-critic "Implement and verify X..." --detach
# End your turn. When woken by "[tchelvis] You have N orchestration message(s)...":
tchelvis orch check --run loop_<RUN_ID>
tchelvis loop result <RUN_ID>
```

| Command | Purpose |
| :--- | :--- |
| `loop check <workflow>` | Validate workflow, config, sandbox and model plan. Spends no tokens. `--live` also probes models. |
| `loop run <workflow> [prompt] --detach` | Start the run in a background Herdr terminal and print `RUN_ID`. `--input-file <path>` reads the task from a file instead of a prompt; `--from-head` ignores uncommitted changes; `--use stage=cli[:model]` overrides a model. Exit codes without `--detach`: `0` PASS, `1` ERROR, `2` usage, `3` ESCALATE, `4` CANCELLED. |
| `loop list --active` | Show runs still in progress. `--runs` shows all recorded runs. |
| `loop status [run_id] --json` | Current or final state (`PASS`, `ERROR`, `ESCALATE`, `CANCELLED`). |
| `loop result [run_id]` | The five-line summary. |
| `loop report [run_id]` | The full sanitized `report.md`. |
| `loop stop [run_id]` | Cancel an active run. Without an ID, the only active one in the current repository. |
| `loop clean [run_id] --yes` | Delete finished run directories and clones. Keeps `loop/*` branches. |

### Safety limits

- **The loop never merges or pushes.** Each run works in its own clone and
  commits only to a new local branch `loop/<workflow>-<run_id>`. Your checkout
  and `.gitignore` are not touched.
- **`auto_approve_in_sandbox` is for you to set.** `~/.config/tchelvis/loops.toml`
  ships with it `false`, and `loop run` refuses to start until you review the
  sandbox and set it to `true`. An agent must not change this setting for you.
- **`bwrap` shares the network.** `bwrap` is the default sandbox. It makes `/`
  read-only and leaves only the run workspace writable. But network access,
  `localhost` services and abstract sockets stay reachable.
- **Models from the repository need confirmation.** Use
  `tchelvis loop models confirm <workflow>` when running with `--detach`.

## From a spec to merged branches

When a feature is too large for a single session or loop, pair `/to-tickets`
with `/orchestration-tchelvis`:

1. **Split the spec with `/to-tickets`.** Run `/to-tickets` on your spec (file
   path or conversation). It asks about any open decisions, proposes vertical
   slices plus a final `integration` ticket, waits for your approval, and writes
   `.loops/tickets/<feature>/NN-<slug>.md` (or `.tickets/<feature>/` when
   `.loops/` does not exist).
2. **Coordinate with `/orchestration-tchelvis`.** Follow **Running Tickets with
   Loops**: the coordinator asks once to create `integration/<feature>` in its
   own git worktree, checks `tchelvis loop list` and `tchelvis loop check` from
   inside that worktree, launches independent ready tickets with
   `tchelvis loop run <workflow> --input-file <path> --detach` (up to
   `max_concurrent`), and ends its turn.
3. **Merge and review.** On each wake-up, it checks `tchelvis orch check` and
   `tchelvis loop result`, merges `PASS` branches whose changed files stay
   inside the ticket's `files` via `git -c core.hooksPath=/dev/null -c core.fsmonitor=false merge --no-ff <branch>`, stops
   on out-of-scope edits or failures, and finishes with an independent
   `tchelvis orch launch` review. Merging into `main` is always your call.

Known limits:

- `PASS` reflects the reviewer's verdict; running the verification command
  deterministically requires Phase B `checks`.
- `tchelvis loop` supports `claude`, `agy`, `opencode`, and `pi` stages only (`pi` must be the managed install at `~/.pi/agent`).
- `.loops/runs` inside the integration worktree is removed if you delete that
  worktree.

## Troubleshooting

- **No wake-up arrived.** Run `tchelvis orch check --run <run_id>`. Act on
  any message it shows; delivery is postponed while your terminal is busy or
  blocked. Run `tchelvis doctor`
  to confirm `tchelvis daemon` is running. Check `tchelvis orch status` for
  unread counts. A pointer can be sent again if it was not read. If the
  terminal no longer exists, `tchelvis loop list --runs` shows the run as not
  delivered.
- **A worker stopped without reporting.** Run `tchelvis orch status --run <run_id>`,
  then follow `send -> retry -> stop`. Attach to its terminal with `hc attach`
  to see what it is doing.
- **The loop terminal is not shown as a tab.** Loop terminals are background
  `hc` terminals named `loop-<RUN_ID>`. They have no pane. They appear as
  working in `tchelvis status` and in the Fleet queue, where clicking one
  attaches it. Use `tchelvis loop status <run_id>` to see the state.
- **`loop auth login agy` needs a browser.** If the browser does not open,
  attach to the terminal with `hc attach <addr>` and copy the URL.

## Related

- Reference: `tchelvis orch --help`, `tchelvis loop --help`.
- Design of the loop engine: `docs/superpowers/specs/2026-11-23-loop-engine-design.md`.
- Main README: [`README.md`](../README.md).
