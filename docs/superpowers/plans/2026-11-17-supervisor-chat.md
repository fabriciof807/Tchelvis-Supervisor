# Supervisor Chat Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A read-only chat (`tchelvis chat ask` + a "Chat" tab in the mod drawer) that answers questions about active agents and Daily Tasks using the supervisor's effective model.

**Architecture:** A pure module `src/supervisor/chat.rs` builds a compact text context (agents from `hc` + supervisor state, tasks from `daily_tasks.json`) and the message list; a new free-text method on `CognitiveBackend` sends it to the effective backend (fallback API when active, else the llama.cpp container, else legacy Ollama). The mod only renders the conversation and keeps history in memory, calling the CLI once per question.

**Tech Stack:** Rust (clap, serde_json, ureq — already deps), mod JS (GPUI `herdr` API, Node tests with `tests/mod/stub-hooks.mjs`).

**Spec:** `docs/superpowers/specs/2026-11-17-supervisor-chat-design.md`

## Global Constraints

- Read-only: the chat path never calls `hc send`/`send_keys`/`set_labels`, never writes `daily_tasks.json`, never calls `engine.tick` or `store.save()`.
- Never starts the model: container stopped and no active fallback → error code `llm_stopped`.
- No terminal screens in the context (`last_output_preview` is never used).
- Context budget `CONTEXT_BUDGET_CHARS = 6000`; history limit `HISTORY_MESSAGES = 12` (6 exchanges); `CHAT_MAX_TOKENS = 512`; per-task title cap `TASK_TITLE_MAX_CHARS = 300`; request timeout 120 s.
- No JSON schema / `response_format` / Ollama `format` on chat requests (free text).
- Nothing about the conversation is written to disk or logged (no question/answer in `tracing`).
- Error codes are exactly `llm_stopped`, `llm_error`, `bad_input`.
- User-facing strings in Portuguese, matching the existing mod copy.

## Review Focus

- No tasks and no agents → the context says so explicitly ("Nenhum agente ativo." / "Nenhuma tarefa.") so the model does not invent any. Test in Task 1.
- Huge context (40 agents, 200 tasks, multi-line titles) → stays under the budget with a "… e mais N" line; blocked agents and open tasks survive the cut. Test in Task 1.
- Fallback active but key missing (`backend()` is `None`) → `llm_error` with a clear message, not a call to the stopped primary. Test in Task 3.
- Fallback error text containing the API key → redacted (goes through `RedactingBackend`). Test in Task 2.
- Request JSON with unknown roles, empty question or malformed JSON → `bad_input`; history entries with roles other than `user`/`assistant` are dropped. Test in Task 3.

---

### Task 1: Context and message building (pure)

**Files:**
- Create: `src/supervisor/chat.rs`
- Modify: `src/supervisor/mod.rs` (add `pub mod chat;`)

**Interfaces:**
- Consumes: `HcSession` (`src/supervisor/hc.rs`), `AgentDurableState`, `PendingOperatorAction` (`src/supervisor/state.rs`), `DailyTask`, `TaskPriority` (`src/home.rs`).
- Produces:
  - `pub const CONTEXT_BUDGET_CHARS: usize = 6000;`, `pub const HISTORY_MESSAGES: usize = 12;`, `pub const CHAT_MAX_TOKENS: u32 = 512;`, `pub const TASK_TITLE_MAX_CHARS: usize = 300;`
  - `pub const CHAT_SYSTEM_PROMPT: &str` — Portuguese; must say: answer only from the CONTEXTO block; say "não sei / não está no contexto" when missing; the assistant cannot execute anything and must never claim it did; short answers.
  - `#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] pub struct ChatTurn { pub role: String, pub content: String }`
  - `#[derive(Deserialize, Debug)] pub struct ChatRequest { pub question: String, #[serde(default)] pub history: Vec<ChatTurn> }`
  - `pub fn build_chat_context(sessions: &[HcSession], agents: &HashMap<String, AgentDurableState>, tasks: &[DailyTask], now: u64, budget_chars: usize) -> String`
  - `pub fn build_chat_messages(context: &str, history: &[ChatTurn], question: &str) -> Vec<serde_json::Value>` — `[system(CHAT_SYSTEM_PROMPT + "\n\nCONTEXTO:\n" + context), …last HISTORY_MESSAGES of history with role user|assistant only…, user(question)]`, each `{"role","content"}`.

Context format (the builder may adjust wording, not content): a line with the current date/time (UTC, `YYYY-MM-DD HH:MM`), then `AGENTES (N):` with one entry per session — address, name, program, hc status, `kind`/`msg` when blocked, cwd basename, `rolling_summary` when non-empty, the pending card's `reason_for_operator` + urgency when present, last auto action when present; blocked sessions first, then the others in input order. Then `TAREFAS (abertas N, concluídas hoje M):` — open tasks first, then tasks with `completed_at` on the same UTC day as `now`; each with id, priority, tag (`project` or `Geral`), creation date, title (newlines → ` / `, cut to `TASK_TITLE_MAX_CHARS` with `…`). Done tasks from earlier days are omitted. When the budget would be exceeded, stop adding entries in each section and append `… e mais N agentes` / `… e mais N tarefas`; the agents section gets at most half the budget when tasks exist.

- [ ] **Step 1: Write the failing tests** in `chat.rs` `#[cfg(test)] mod tests`:
  - `context_lists_blocked_agents_first_with_pending_prompt` — sessions `[working "t1", blocked "t2" kind=permission msg="approve Bash: ls"]`; assert `ctx.find("t2") < ctx.find("t1")` and `ctx.contains("approve Bash: ls")`.
  - `context_includes_summary_and_card_reason_but_never_the_screen` — state with `rolling_summary="refatorando auth"`, `last_output_preview="SECRET_SCREEN"`, a pending card `reason_for_operator="rm detectado"`; assert contains `"refatorando auth"` and `"rm detectado"`, not `"SECRET_SCREEN"`.
  - `context_shows_open_tasks_then_done_today_and_drops_older_done` — open task, task done today, task done 3 days ago; assert open before done-today; the old one absent.
  - `task_without_project_is_tagged_geral_and_multiline_title_is_joined` — title `"linha 1\nlinha 2"`, `project: None`; assert contains `"Geral"` and `"linha 1 / linha 2"`.
  - `empty_inputs_say_so_explicitly` — assert contains `"Nenhum agente ativo."` and `"Nenhuma tarefa."`.
  - `context_respects_budget_and_reports_the_cut` — 40 sessions (5 blocked) and 200 open tasks with 280-char titles, budget `CONTEXT_BUDGET_CHARS`; assert `ctx.chars().count() <= CONTEXT_BUDGET_CHARS`, contains `"e mais"` for both sections, and all 5 blocked addresses present.
  - `messages_keep_last_history_and_drop_unknown_roles` — 20 alternating turns plus one `{"role":"system"}` turn; assert `messages.len() == 1 + HISTORY_MESSAGES + 1`, first role `system` containing `"CONTEXTO:"`, last is the question with role `user`, no injected `system` beyond index 0.

- [ ] **Step 2: Run** `cargo test --lib supervisor::chat` — Expected: FAIL (module/functions missing).
- [ ] **Step 3: Implement** the constants, types and the two functions in `src/supervisor/chat.rs`; add `pub mod chat;` to `src/supervisor/mod.rs`. Format dates with `chrono` (already a dependency).
- [ ] **Step 4: Run** `cargo test --lib supervisor::chat` — Expected: PASS.
- [ ] **Step 5: Commit** `feat(chat): build read-only context and messages for the supervisor chat`

---

### Task 2: Free-text completion on the backends

**Files:**
- Modify: `src/supervisor/ollama.rs` (trait `CognitiveBackend` ~line 41; `impl OllamaClient` ~line 463; `impl CognitiveBackend for OllamaClient` ~line 595)
- Modify: `src/supervisor/fallback.rs` (`impl CognitiveBackend for RedactingBackend` ~line 383)

**Interfaces:**
- Consumes: message list from Task 1 (`&[serde_json::Value]`).
- Produces:
  - Trait method with default: `fn complete_text(&self, messages: &[Value], max_tokens: u32) -> Result<String> { anyhow::bail!("free-text completion not supported by this backend") }`
  - `impl OllamaClient { pub fn build_text_chat_body(&self, messages: &[Value], max_tokens: u32) -> Value; pub fn build_openai_text_chat_body(&self, messages: &[Value], max_tokens: u32) -> Value }`
  - `OllamaClient::complete_text` routes like `evaluate`: Ollama → `/api/chat` (`stream:false`, `think:false`, `options.num_predict = max_tokens`, `options.temperature`, `options.num_gpu`, **no** `format`) and reads `message.content`; LlamaCpp/LmStudio/OpenAiCompatible → `openai_chat_completions_path` on `effective_primary_host()` with `max_tokens`, `temperature`, **no** `response_format`, reads via `extract_openai_chat_content`. Returns the trimmed content.
  - `RedactingBackend::complete_text` delegates and redacts with `redact_key`, same as `evaluate`.

- [ ] **Step 1: Write the failing tests:**
  - in `ollama.rs` tests: `text_chat_bodies_have_no_schema_and_carry_max_tokens` — for both builders assert `body.get("format").is_none()` / `body.get("response_format").is_none()`, `num_predict == 512` (Ollama) and `max_tokens == 512` (OpenAI), and `messages` equals the input.
  - in `fallback.rs` tests: `redacting_backend_redacts_key_in_complete_text_errors` — inner fake whose `complete_text` fails with `"401 for key sk-SECRET"`; key `"sk-SECRET"`; assert the error string does not contain `"sk-SECRET"`.
- [ ] **Step 2: Run** `cargo test --lib text_chat_bodies redacting_backend_redacts_key_in_complete_text` — Expected: FAIL.
- [ ] **Step 3: Implement** the trait default, the two builders, `OllamaClient::complete_text` and `RedactingBackend::complete_text`.
- [ ] **Step 4: Run** `cargo test --quiet` — Expected: all pass (existing evaluation tests unchanged).
- [ ] **Step 5: Commit** `feat(supervisor): free-text completion for chat on Ollama, OpenAI-compatible and fallback backends`

---

### Task 3: Routing and `tchelvis chat ask` CLI

**Files:**
- Modify: `src/supervisor/chat.rs`
- Modify: `src/main.rs` (`enum Commands` ~line 37; dispatch `match` ~line 660 area)

**Interfaces:**
- Consumes: Task 1 (`ChatRequest`, `build_chat_context`, `build_chat_messages`, constants), Task 2 (`complete_text`), `WatcherEngine` public fields `hc`, `store.agents`, `cognitive`, `fallback` and `fallback_state()` (`src/supervisor/watcher.rs`), `build_default_engine`/`build_engine_from_config` (`src/supervisor/mod.rs`), `DailyTaskStore::{default_path, load_or_init}` (`src/home.rs`), `build_desktop_mod_snapshot(engine).llm_model` for the primary label (`src/supervisor/tui.rs`).
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum ChatRoute { Primary, Fallback, FallbackWithoutKey, Stopped }`
  - `pub fn choose_route(fallback_active: bool, fallback_backend: bool, primary_available: bool) -> ChatRoute` — active+backend → `Fallback`; active+no backend → `FallbackWithoutKey`; else primary available → `Primary`; else `Stopped`.
  - `#[derive(Serialize, Debug, PartialEq)] pub struct ChatAnswer { pub answer: String, pub model: String, pub via_fallback: bool, pub fallback_host: Option<String>, pub context_chars: usize }`
  - `#[derive(Serialize, Debug, PartialEq)] pub struct ChatError { pub error: String, pub code: String }`
  - `pub fn parse_chat_request(raw: &str) -> Result<ChatRequest, ChatError>` — malformed JSON or blank `question` → `code "bad_input"`.
  - `pub fn run_chat<H: HcBackend, C: CognitiveBackend>(engine: &WatcherEngine<H, C>, primary_label: &str, tasks: &[DailyTask], req: &ChatRequest, now: u64) -> Result<ChatAnswer, ChatError>` — `Stopped` → `llm_stopped` ("LLM desligada: use Load LLM"); `FallbackWithoutKey` → `llm_error` ("Fallback ativo sem chave"); `hc.list_sessions()` failure → empty agent list (context says none), not an error; backend error → `llm_error` with the (already redacted) message truncated to 300 chars.
  - CLI: `Commands::Chat { #[command(subcommand)] action: ChatCommand }`, `enum ChatCommand { Ask { #[arg(long)] json: bool, #[arg(long)] request: Option<String>, question: Vec<String> } }`. With `--json`: request from `--request`, else from stdin; prints `ChatAnswer` or `ChatError` JSON; exit code 1 on `ChatError`. Without `--json`: question from the positional words (blank → `bad_input`), prints only the answer text; on error prints `error` to stderr and exits 1. The CLI builds the engine from `SupervisorConfig::load()` with `ollama_timeout_secs = 120`, loads tasks with `DailyTaskStore::load_or_init(&DailyTaskStore::default_path(&paths))`, and never ticks or saves.

- [ ] **Step 1: Write the failing tests** in `chat.rs` (reuse the watcher test style: a fake `HcBackend` and a fake `CognitiveBackend` recording `complete_text` calls and every `send_input`/`set_labels`; a fake `FallbackProvider`):
  - `route_table` — the four cases of `choose_route`.
  - `stopped_primary_returns_llm_stopped_without_calling_the_model` — primary `is_available=false`, no fallback; assert `Err(code=="llm_stopped")` and 0 `complete_text` calls.
  - `active_fallback_answers_via_fallback_backend` — fallback state active + backend fake answering `"ok"`; assert `via_fallback`, answer `"ok"`, primary fake 0 calls.
  - `active_fallback_without_key_is_llm_error` — active, `backend()` → `None`; assert `code=="llm_error"`, 0 calls on the primary.
  - `chat_never_writes_to_terminals_or_state` — normal primary answer; assert fake hc recorded no `send_input`/`send_keys`/`set_labels` and the state file on disk is unchanged (or absent) after `run_chat`.
  - `bad_requests_are_bad_input` — `parse_chat_request("not json")`, `{"question":"  "}` → `code=="bad_input"`.
- [ ] **Step 2: Run** `cargo test --lib supervisor::chat` — Expected: new tests FAIL.
- [ ] **Step 3: Implement** `ChatRoute`, `choose_route`, `ChatAnswer`, `ChatError`, `parse_chat_request`, `run_chat` in `chat.rs`; add `Commands::Chat` / `ChatCommand::Ask` and the dispatch in `src/main.rs`.
- [ ] **Step 4: Verify:**
  - `cargo test --quiet` — all pass.
  - `cargo build --release && ./target/release/tchelvis chat ask --json --request '{"question":"oi"}'; echo "exit=$?"` with no container running — Expected: `{"error":"LLM desligada…","code":"llm_stopped"}` and `exit=1`.
  - `./target/release/tchelvis chat ask --json --request 'x'` — Expected: `"code":"bad_input"`.
  - In the `ai-box`: `tchelvis supervisor load` (SmolLM2 via `hf_repo`, as in earlier validations), add two tasks, then `tchelvis chat ask "quais tarefas estão abertas?"` — Expected: a text answer; then `tchelvis supervisor unload`.
- [ ] **Step 5: Commit** `feat(chat): tchelvis chat ask routes to the effective supervisor model (read-only)`

---

### Task 4: "Chat" tab in the mod drawer

**Files:**
- Modify: `assets/herdr-app/mods/tchelvis/main.js` (drawer tab state ~line 1389 `this.inspectorTab`; tab switcher ~lines 3200–3225 and ~6743; `openDrawerView` ~line 2813; renderers near `renderHome` ~line 4475 / `renderDoctorBody` ~line 4559; task input key handling ~lines 2330–2375 as the model for Enter/Shift+Enter)
- Create: `tests/mod/chat.test.mjs`

**Interfaces:**
- Consumes: CLI from Task 3 (`tchelvis chat ask --json --request <json>`; stdout `ChatAnswer` | `ChatError`), `shared("supervisor.snapshot")` fields `llm_model`, `fallback_active`, `fallback_host`, `llm_container_state`, `llm_managed`; existing `pillModelName(name, maxLen)`.
- Produces (exported pure functions):
  - `export function chatHistoryForRequest(messages, maxMessages = 12)` → `[{role, content}]` from the last `maxMessages` entries with `role` `"user"`/`"assistant"` (error entries have `role: "error"` and are skipped).
  - `export function chatRequestArgv(messages, question)` → `["chat", "ask", "--json", "--request", JSON.stringify({question, history: chatHistoryForRequest(messages)})]`.
  - `export function parseChatResult(code, stdout)` → `{ok: true, answer, model, viaFallback, fallbackHost}` | `{ok: false, code, error}`; unparsable stdout → `{ok:false, code:"llm_error", error:"Resposta inválida do tchelvis"}`.
  - `export function chatHeaderLabel(supSnap)` → `pillModelName(llm_model)`, plus `" (Fallback · <host>)"` when `fallback_active`.

UI behavior (spec "Mod — aba Chat"): fourth drawer tab `chat` next to Home/Inspector/Doctor; header from `chatHeaderLabel` in warning tone when fallback; message list (user right/accent, assistant left, `error` entries in error tone); growing input (Enter sends, Shift+Enter newline, blank draft ignored); `Limpar` clears the history; while waiting: `◌ Pensando…` and input disabled; `llm_stopped` → message "LLM desligada" with the existing `▶ Load LLM` action; history kept in instance memory only. Exec with `timeout: 130000`.

- [ ] **Step 1: Write the failing tests** in `tests/mod/chat.test.mjs`:
  - `history keeps the last 12 user/assistant messages and skips errors` — 15 messages incl. 2 `error`; assert length 12 and no `error` role.
  - `request argv carries question and history as JSON` — assert argv prefix and `JSON.parse(argv[4]).question === "q"`.
  - `parseChatResult maps answer, llm_stopped and garbage` — `(0, '{"answer":"a","model":"m","via_fallback":false,"context_chars":10}')` → ok; `(1, '{"error":"LLM desligada","code":"llm_stopped"}')` → `{ok:false, code:"llm_stopped"}`; `(1, "boom")` → `code "llm_error"`.
  - `header shows fallback host only when fallback is active`.
- [ ] **Step 2: Run** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/chat.test.mjs` — Expected: FAIL (exports missing).
- [ ] **Step 3: Implement** the four exports and the tab (state, renderer, key handling, exec call) in `main.js`.
- [ ] **Step 4: Verify:**
  - `node --check assets/herdr-app/mods/tchelvis/main.js`
  - `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` — all pass.
  - `cargo test --quiet` — all pass.
- [ ] **Step 5: Commit** `feat(mod): Chat tab in the drawer for read-only questions about agents and tasks`

---

## Host validation (coordinator, after merge and nightly)

- Ask with the 1.7B and with the 350M: "quais agentes estão esperando por mim?", "quais tarefas estão abertas?", "o que eu deveria fazer primeiro?"; note answer time and quality.
- With the container stopped: the tab shows "LLM desligada" + `▶ Load LLM`; nothing is loaded by the chat.
- During a chat answer, trigger a dangerous sim: the supervisor still raises its alert (possibly delayed by one answer).
- Fallback active: header shows `(Fallback · host)`.
