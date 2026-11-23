# Configurable Supervisor `[guardrails]` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user add deterministic allow/deny rules for the supervisor in `supervisor.toml`, without ever weakening the built-in floor.

**Architecture:** A pure module `src/supervisor/rules.rs` parses and matches rules. `guardrails.rs` keeps the floor and gains one decision function, `decide_fixed`, used by the watcher and by `rules test`. The watcher reloads the rules at the start of each tick from an injected source.

**Tech Stack:** Rust 2021, `toml` 0.8, `shlex` 2.0, `regex`, `clap`; Herdr mod JS with Node tests. No new dependency.

**Spec:** `docs/superpowers/specs/2026-11-22-supervisor-guardrails-config-design.md` (Portuguese; it decides behaviour).

**Prerequisite:** lot M (`.work/briefs/lot-m-fast-path-hardening.md`) merged in `main`. This plan calls its shared check `is_safe_readonly_command`; if it has another name, use that one.

## Global Constraints

- No `[guardrails]` table → behaviour identical to today; no existing test is edited, weakened or deleted.
- Order: floor → `deny_*` → built-in/`allow_*` → LLM. Nothing below reverts the floor.
- Invalid `[guardrails]` → **no** automatic reply (fixed or LLM, any key); other settings keep working.
- Invalid `allow_*` entry → ignored with a warning. Invalid `deny_*` entry, unknown key, wrong type, unreadable file, broken TOML → table invalid.
- `AlertOnly`, `Paused` and `max_auto_replies` apply to user rules too (go through `validate_evaluation`).
- Rule ids, verbatim: `floor:<reason>`, `deny_command:<entry>`, `deny_path:<entry>`, `builtin_read_only`, `builtin_tool:Read`, `allow_command:<entry>`, `requires_llm`, `llm_auto_reply_off`, `guardrails_invalid`.
- Card text on a denial: `[Resposta automática bloqueada: regra deny_commands "cargo publish"]` (same with `deny_paths`).
- User-visible supervisor text in Portuguese; code, docs and CHANGELOG in English.
- Tests: no network, sleeps or real `$HOME`; pass paths and sources in.
- Validation per task: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --locked`.

## Review Focus

- `msg` with a newline (`approve Bash: ls\npython evil.py`): must stay refused (lot M fixed the fixed-rule path). Test in Task 5.
- File exists but is a broken symlink, a directory or unreadable: invalid, not "absent". Test in Task 4.
- A typo in an unrelated setting (`poll_interval_secs = "x"`) resets that config to defaults but must not drop the user's `deny_*`. Test in Task 4.
- Denied text only on screen, while `msg` is safe: denied. A menu option or old answered prompt outside the pending area: not considered. Test in Task 3.
- Non-ASCII or very long commands: no panic on char boundaries. Test in Task 2.

---

### Task 1: Expose the floor as one reusable check

**Files:** Modify and test: `src/supervisor/guardrails.rs`

**Interfaces — produces:**
- `pub(crate) fn floor_violation(cmd: &str) -> Option<&'static str>` — reasons: `empty`, `forbidden`, `metachar`, `truncated`, `unparsable`, `glob`, `sensitive`, `option`. It is everything in lot M's check except the read-only allowlist, plus `contains_forbidden_payload`.
- `is_safe_readonly_command(cmd)` becomes `floor_violation(cmd).is_none()` plus the allowlist.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn floor_accepts_any_plain_single_command() {
    for c in ["just test", "cargo test --lib", "git branch", "docker ps"] {
        assert_eq!(floor_violation(c), None, "{c}");
    }
}
#[test]
fn floor_names_the_reason() {
    for (c, r) in [
        ("", "empty"), ("rm -rf x", "forbidden"), ("just test & evil", "metachar"),
        ("just $X", "metachar"), ("just te…", "truncated"), ("just 'x", "unparsable"),
        ("cat .e*", "glob"), ("cat a/.e?v", "glob"), ("cat .ssh/x", "sensitive"),
        ("git diff --output=x", "option"),
    ] {
        assert_eq!(floor_violation(c), Some(r), "{c}");
    }
}
#[test]
fn fast_path_refuses_glob_on_dot_segment_but_not_plain_globs() {
    let menu = "Do you want to proceed?\n❯ 1. Yes\n  2. No";
    let f = |c: &str| readonly_fast_path_with_msg(menu, Some("permission"), Some(&format!("approve Bash: {c}")));
    assert!(f("cat .e*").is_none());
    assert!(f("cat *.rs").is_some());
    assert!(f("ls ../*.rs").is_some());
}
```

- [ ] **Step 2:** `cargo test --locked floor_ fast_path_` → new tests FAIL.
- [ ] **Step 3: Implement.** Check order is the order of the reasons above. `glob`: an unquoted (`shlex`) token has a `/`-segment that starts with `.` and contains `*`, `?` or `[` (this also refuses `rg ".*x"`, accepted), or lot M's rg/git leading-glob rule. Keep lot M's `is_approve_msg` and `is_single_visible_request` rules.
- [ ] **Step 4:** full validation → PASS.
- [ ] **Step 5: Commit** `refactor(supervisor): expose the command floor as one check`

---

### Task 2: `rules.rs` — parse, validate, match (pure)

**Files:** Create: `src/supervisor/rules.rs` (tests inside); Modify: `src/supervisor/mod.rs` (`pub mod rules;`)

**Interfaces — consumes** `floor_violation`. **Produces:**

```rust
pub struct RuleWarning { pub key: String, pub entry: String, pub reason: String }
pub struct GuardrailRules { // Default = today's behaviour
    pub invalid: Option<String>,   // Some(reason) → no automatic replies
    pub warnings: Vec<RuleWarning>,
    pub allow_commands: Vec<String>, pub deny_commands: Vec<String>, pub deny_paths: Vec<String>,
    pub allow_read_tool: bool, pub builtin_read_only: bool,
    pub builtin_build_test: bool, pub llm_auto_reply: bool,
}
pub fn parse_guardrails(raw_toml: &str) -> GuardrailRules;
impl GuardrailRules {
    pub fn invalid(reason: impl Into<String>) -> Self;
    pub fn match_allow(&self, cmd: &str) -> Option<&str>;        // entry; does not check floor/deny
    pub fn match_deny_command(&self, text: &str) -> Option<&str>;
    pub fn match_deny_path(&self, text: &str) -> Option<&str>;
}
```

`GuardrailRules` derives `Debug, Clone, PartialEq, Serialize`; `RuleWarning` too. The raw serde struct uses `deny_unknown_fields`.

- [ ] **Step 1: Failing tests**

```rust
fn rules(body: &str) -> GuardrailRules { parse_guardrails(&format!("[guardrails]\n{body}")) }

#[test]
fn absent_table_is_default() {
    assert_eq!(parse_guardrails("poll_interval_secs = 5"), GuardrailRules::default());
    assert_eq!(parse_guardrails(""), GuardrailRules::default());
    let d = GuardrailRules::default();
    assert!(d.invalid.is_none() && d.allow_read_tool && d.builtin_read_only && d.builtin_build_test && d.llm_auto_reply);
}
#[test]
fn allow_is_exact_unless_trailing_star() {
    let r = rules(r#"allow_commands = ["git branch", "cargo test *"]"#);
    assert_eq!(r.match_allow("git  branch"), Some("git branch"));
    for c in ["cargo test", "cargo test --lib x"] { assert_eq!(r.match_allow(c), Some("cargo test *")); }
    for c in ["git branch -D main", "/usr/bin/git branch", "FOO=1 cargo test", "env cargo test", "cargo testx", "git 'branch"] {
        assert_eq!(r.match_allow(c), None, "{c}");
    }
}
#[test]
fn bad_allow_entries_are_ignored_with_a_warning() {
    for e in ["", "*", "cargo * test", "cargo te*", "git 'x", "rm x", "a | b", "cat .ssh/x",
              "bash *", "python3.12 *", "/usr/bin/env *", "uv run python *", "npx *"] {
        let r = rules(&format!("allow_commands = [{e:?}, \"just test\"]"));
        assert!(r.invalid.is_none(), "{e}");
        assert_eq!(r.allow_commands, ["just test"], "{e}");
        assert_eq!(r.warnings.len(), 1, "{e}");
    }
    let r = rules("allow_commands = [\"git commit\"]\ndeny_commands = [\"git commit\"]");
    assert!(r.allow_commands.is_empty() && r.warnings.len() == 1);
    assert_eq!(rules(r#"allow_commands = ["python scripts/check.py"]"#).warnings, []);
}
#[test]
fn bad_deny_unknown_key_wrong_type_or_broken_toml_is_invalid() {
    for b in ["deny_commands = [\"\"]", "deny_commands = [\"git *\"]", "deny_commands = [\"a 'b\"]",
              "deny_paths = [\"\"]", "deny_paths = [\"a/*\"]", "deny_command = [\"x\"]",
              "allow_commands = \"x\"", "llm_auto_reply = 1", "allow_commands = ["] {
        assert!(rules(b).invalid.is_some(), "{b}");
    }
    assert!(parse_guardrails("guardrails = 3").invalid.is_some());
}
#[test]
fn deny_command_matches_anywhere() {
    let r = rules(r#"deny_commands = ["cargo publish"]"#);
    for t in ["cargo publish --dry-run", "cd x && cargo publish", "bash -c \"cargo publish\"",
              "CARGO  Publish", "Run: (cargo publish)", "cargo\npublish", "`cargo publish`"] {
        assert_eq!(r.match_deny_command(t), Some("cargo publish"), "{t}");
    }
    for t in ["cargo publish-helper", "cargo test publish", "mycargo publish"] {
        assert_eq!(r.match_deny_command(t), None, "{t}");
    }
}
#[test]
fn deny_path_matches_whole_segments() {
    let r = rules(r#"deny_paths = [".kube", "prod/secrets"]"#);
    for t in ["cat ~/.kube/config", "cat a/.KUBE", "x --f=prod/secrets/a", "cat ./prod/./secrets", "host:prod/secrets", "cat '.kube/c'"] {
        assert!(r.match_deny_path(t).is_some(), "{t}");
    }
    for t in ["cat reprod/secrets", "cat prod/x/secrets", "cat .kubeconfig"] { assert!(r.match_deny_path(t).is_none(), "{t}"); }
}
#[test]
fn allow_tools_is_a_closed_list() {
    assert!(!rules("allow_tools = []").allow_read_tool);
    let r = rules(r#"allow_tools = ["Read", "Grep", "Bash", "Write", "mcp__x"]"#);
    assert!(r.allow_read_tool && r.invalid.is_none());
    assert_eq!(r.warnings.len(), 4);
}
#[test]
fn non_ascii_and_long_input_do_not_panic() {
    let r = rules("allow_commands = [\"just ação\"]\ndeny_commands = [\"é ü\"]\ndeny_paths = [\"ção\"]");
    let long = "ação…".repeat(20_000);
    assert!(r.match_allow(&long).is_none());
    assert_eq!(r.match_allow("just ação"), Some("just ação"));
    let _ = (r.match_deny_command(&long), r.match_deny_path(&long));
}
```

- [ ] **Step 2:** `cargo test --locked rules::` → FAIL.
- [ ] **Step 3: Implement.**
  - Parse as `toml::Table`, take `guardrails`, `try_into` the raw struct; errors become `invalid`.
  - Deny tokens: lower-case, split on whitespace and `; | & ( ) { } " ' $` and backtick, drop `\`; match as a contiguous run.
  - Path segments: split tokens the same way, then on `/ = :`; drop empty and `.` segments; a run may not cross a token boundary.
  - Executor check: basename of the literal token right before `*` is one of `bash sh zsh fish env xargs eval exec sudo ssh nohup timeout time node bun deno perl ruby npx uvx` or starts with `python`.
  - Warning for `Glob`, `Grep`, `LS`: `sem formato real capturado`; any other tool: `ferramenta não permitida`.
- [ ] **Step 4:** full validation → PASS.
- [ ] **Step 5: Commit** `feat(supervisor): parse and match [guardrails] rules`

---

### Task 3: One decision function, and rules in the LLM path

**Files:** Modify and test: `src/supervisor/guardrails.rs`

**Interfaces — consumes** Tasks 1–2. **Produces:**

```rust
pub struct FixedDecision { pub approval: Option<FixedApproval>, pub rule_id: String }
pub struct FixedApproval { pub payload: String, pub command: String }
pub fn decide_fixed(output: &str, kind: Option<&str>, msg: Option<&str>, rules: &GuardrailRules) -> FixedDecision;
pub struct SanitizedReply { pub eval: CognitiveEvaluation, pub rule_id: Option<String> }
pub fn sanitize_auto_reply_with_rules(output: &str, kind: Option<&str>, msg: Option<&str>, rules: &GuardrailRules, eval: CognitiveEvaluation) -> SanitizedReply;
```

`readonly_fast_path`, `readonly_fast_path_with_msg` and `sanitize_auto_reply` become `#[cfg(test)]` wrappers with default rules, so existing tests compile unchanged.

- [ ] **Step 1: Failing tests** (helpers: `rules(body)`; `MENU` = the `1. Yes` menu; `on_menu(cmd, &rules)` = `decide_fixed(MENU, Some("permission"), Some("approve Bash: <cmd>"), &rules)`; `auto(payload)` exists)

```rust
#[test]
fn user_allow_is_approved_on_the_claude_menu() {
    let r = rules(r#"allow_commands = ["just test"]"#);
    let d = on_menu("just test", &r);
    assert_eq!((d.approval.unwrap().payload.as_str(), d.rule_id.as_str()), ("1", "allow_command:just test"));
    assert_eq!(on_menu("just test x", &r).rule_id, "requires_llm");
    assert_eq!(on_menu("just test & evil", &r).rule_id, "floor:metachar");
    assert!(decide_fixed(MENU, Some("question"), Some("approve Bash: just test"), &r).approval.is_none());
    assert!(decide_fixed("working", Some("permission"), Some("approve Bash: just test"), &r).approval.is_none());
}
#[test]
fn builtins_report_their_rule_and_can_be_turned_off() {
    let d = GuardrailRules::default();
    assert_eq!(on_menu("git status", &d).rule_id, "builtin_read_only");
    assert_eq!(decide_fixed(MENU, None, Some("approve Read: a.rs"), &d).rule_id, "builtin_tool:Read");
    assert!(on_menu("git status", &rules("builtin_read_only = false")).approval.is_none());
    assert!(decide_fixed(MENU, None, Some("approve Read: a.rs"), &rules("allow_tools = []")).approval.is_none());
}
#[test]
fn deny_beats_builtin_and_allow() {
    assert_eq!(on_menu("git log", &rules(r#"deny_commands = ["git log"]"#)).rule_id, "deny_command:git log");
    assert_eq!(on_menu("cat prod/x", &rules(r#"deny_paths = ["prod"]"#)).rule_id, "deny_path:prod");
    assert_eq!(decide_fixed(MENU, None, Some("approve Read: prod/x"), &rules(r#"deny_paths = ["prod"]"#)).rule_id, "deny_path:prod");
    let r = rules("allow_commands = [\"just *\"]\ndeny_commands = [\"just deploy\"]");
    assert!(on_menu("just test", &r).approval.is_some());
    assert_eq!(on_menu("just deploy", &r).rule_id, "deny_command:just deploy");
}
#[test]
fn deny_reads_the_pending_area_not_answered_prompts() {
    let r = rules(r#"deny_commands = ["npm publish"]"#);
    let d = decide_fixed("npm publish\nDo you want to proceed?\n❯ 1. Yes", None, Some("approve Bash: ls"), &r);
    assert_eq!(d.rule_id, "deny_command:npm publish");
    let old = "npm publish\nProceed? [y/N]\nn\nAllow bash tool: ls src ? [y/N]";
    assert!(decide_fixed(old, None, None, &r).approval.is_some());
}
#[test]
fn user_allow_on_screen_needs_a_header_or_inline_prompt() {
    let r = rules(r#"allow_commands = ["just test"]"#);
    assert!(decide_fixed("Bash command\n  just test\nAllow? [y/N]", None, None, &r).approval.is_some());
    assert!(decide_fixed("Allow bash tool: just test ? [y/N]", None, None, &r).approval.is_some());
    assert!(decide_fixed("just test\nAllow? [y/N]", None, None, &r).approval.is_none());
}
#[test]
fn llm_reply_is_denied_with_the_rule_in_the_reason() {
    let r = rules(r#"deny_commands = ["cargo publish"]"#);
    let s = "cargo publish\nProceed? [y/N]";
    let out = sanitize_auto_reply_with_rules(s, None, None, &r, auto("y"));
    assert_eq!(out.eval.decision, SupervisorDecision::AlertOperator);
    assert_eq!(out.eval.urgency, Urgency::Normal);
    assert!(out.eval.hc_command_payload.is_empty());
    assert!(out.eval.reason_for_operator.contains(r#"[Resposta automática bloqueada: regra deny_commands "cargo publish"]"#));
    assert_eq!(out.rule_id.as_deref(), Some("deny_command:cargo publish"));
    assert_eq!(sanitize_auto_reply_with_rules(s, None, None, &r, auto("n")).eval.decision, SupervisorDecision::AutoReply);
}
#[test]
fn llm_reply_to_a_user_allowed_command_and_switches() {
    let s = "just test\nProceed? [y/N]";
    let ok = |b: &str, s: &str| sanitize_auto_reply_with_rules(s, None, None, &rules(b), auto("y")).eval.decision == SupervisorDecision::AutoReply;
    assert!(!ok("", s));
    assert!(ok(r#"allow_commands = ["just test"]"#, s));
    assert!(ok("", "cargo test\nProceed? [y/N]"));
    assert!(!ok("builtin_build_test = false", "cargo test\nProceed? [y/N]"));
    assert!(!ok("llm_auto_reply = false", "cargo test\nProceed? [y/N]"));
}
#[test]
fn invalid_rules_stop_every_automatic_reply() {
    let r = GuardrailRules::invalid("x");
    let d = on_menu("ls", &r);
    assert!(d.approval.is_none());
    assert_eq!(d.rule_id, "guardrails_invalid");
    for p in ["y", "n"] {
        let out = sanitize_auto_reply_with_rules("Continue? [y/N]", None, None, &r, auto(p));
        assert_eq!(out.eval.decision, SupervisorDecision::AlertOperator);
        assert_eq!(out.rule_id.as_deref(), Some("guardrails_invalid"));
        assert!(out.eval.reason_for_operator.contains("[Resposta automática bloqueada: [guardrails] inválido no supervisor.toml]"));
    }
}
```

- [ ] **Step 2:** run → FAIL.
- [ ] **Step 3: Implement.**
  - `decide_fixed` order: `floor:question`, `floor:destructive`, no prompt → `requires_llm`, invalid, command floor, deny, built-in, allow, `requires_llm`. Deny text = pending area + `msg` + identified command, each checked separately.
  - Sanitize order: not `AutoReply` → unchanged; invalid; `llm_auto_reply_off`; question; deny (any reply except `n`/`no`); the existing steps. User-allowed commands pass where `is_allowlisted_command` is checked, with the floor. The existing permission-request refusal stays.
- [ ] **Step 4:** full validation → PASS, existing tests untouched (`git diff` shows no removed assertion).
- [ ] **Step 5: Commit** `feat(supervisor): apply user guardrails to fixed and LLM replies`

---

### Task 4: Load rules every tick; rule id in the event log

**Files:** Modify: `src/supervisor/rules.rs`, `watcher.rs`, `mod.rs` (`build_engine_from_config`), `events.rs`

**Interfaces — produces:**
- `pub fn load_guardrail_rules(path: &Path) -> GuardrailRules` — `symlink_metadata` says not found → default; any other failure → invalid.
- `pub fn load_default_guardrail_rules() -> GuardrailRules` — uses `supervisor_config_path_in(home)`, so the legacy path is honoured.
- `WatcherEngine::with_rules_source(self, Box<dyn Fn() -> GuardrailRules + Send>) -> Self`; `tick` calls it first. `WatcherEngine::new` uses default rules; `build_engine_from_config` injects the loader (covers `tick`, `watch` and the TUI).
- `SupervisorEvent.rule_id: Option<String>` (`serde(default)`, skipped when `None`); `auto_reply` and `auto_reply_blocked` take a trailing `rule_id: Option<&str>`; `apply_decision` takes `rule_id: Option<&str>`.

- [ ] **Step 1: Failing tests**
  - `rules.rs`: `missing_file_is_default`; `directory_or_broken_symlink_or_bad_utf8_is_invalid`; `unrelated_bad_setting_keeps_deny_rules` (file with `poll_interval_secs = "x"` and `deny_commands = ["npm publish"]` → `invalid` is `None` and the rule is present).
  - `config.rs`: `guardrails_table_does_not_affect_other_settings` — `toml::from_str::<SupervisorConfig>` of `ollama_model = "m"` plus `[guardrails]` with `deny_command = 3` → `Ok`, model `m`. `config_write.rs`: `model_selection_keeps_guardrails` — `write_model_selection` on a file with the table leaves its text unchanged.
  - `watcher.rs`: `user_allow_replies_without_the_llm` (session with `msg: Some("approve Bash: just test")`, menu screen, cognitive unavailable → sends `1`; event has `rule_id == "allow_command:just test"`); `user_allow_respects_alert_only_and_max_auto_replies`; `rules_are_reloaded_on_each_tick` (source switches from default to deny between ticks, new screen each tick); `invalid_rules_send_nothing_but_still_alert` (LLM proposes `y`, nothing sent, card raised, blocked event has `guardrails_invalid`).
  - `events.rs`: `old_lines_without_rule_id_still_parse`.
- [ ] **Step 2:** run → FAIL.
- [ ] **Step 3: Implement.** For `allow_command:*` the summary is `Comando aprovado pela regra <rule_id>: <cmd>`; built-ins keep today's text.
- [ ] **Step 4:** full validation → PASS.
- [ ] **Step 5: Commit** `feat(supervisor): load [guardrails] each tick and log the rule`

---

### Task 5: `supervisor rules`, `rules test` and Doctor

**Files:** Create: `src/supervisor/rules_cli.rs`; Modify: `src/main.rs`, `src/supervisor/mod.rs`, `src/doctor.rs`; Test: `rules_cli.rs`, `tests/integration_tests.rs`

**Interfaces — consumes** `decide_fixed`, `load_default_guardrail_rules`. **Produces:**
- CLI `tchelvis supervisor rules [--json]`; `rules test ["<command>"] [--tool <name>] [--screen <file>] [--msg <text>] [--kind <kind>] [--json]`. Command and `--screen` are mutually exclusive, one is required; `--tool` only with a command; `--msg`/`--kind` only with `--screen`.
- `pub struct RuleTestReport { pub approved: bool, pub rule_id: String, pub command: Option<String>, pub payload: Option<String>, pub warnings: Vec<RuleWarning> }`
- `pub fn test_command(rules: &GuardrailRules, cmd: &str, tool: Option<&str>) -> RuleTestReport` — `decide_fixed` on the `1. Yes` menu with `approve <tool|Bash>: <cmd>`.
- `pub fn test_screen(rules: &GuardrailRules, screen: &str, msg: Option<&str>, kind: Option<&str>) -> RuleTestReport`
- `pub fn doctor_guardrails_check(rules: &GuardrailRules) -> DoctorCheck` in `doctor.rs`: name `guardrails`; status `ok`, `warn` (warnings) or `fail` (invalid); a `fail` sets `has_fail`.

- [ ] **Step 1: Failing tests**
  - `report_matches_the_supervisor_decision`: for every command used in Task 3, `test_command` equals `decide_fixed` in `rule_id` and approval.
  - `command_with_newline_is_not_approved`: `test_command(&default, "ls\npython evil.py", None).approved == false`.
  - Doctor: one test per status.
  - Integration, running the binary with a temporary `HOME`: exit 0 for `ls`, 1 for `rm -rf x`, 2 for no arguments and for a missing `--screen` file; `rules --json` contains `"invalid":null` without a file, and a reason with `deny_command = ["x"]`.
- [ ] **Step 2:** run → FAIL.
- [ ] **Step 3: Implement.** Human output in Portuguese, first line `APROVADO (<rule_id>)` or `NÃO APROVADO (<rule_id>)`. `rules test` prints that it never sends anything.
- [ ] **Step 4:** full validation → PASS.
- [ ] **Step 5: Commit** `feat(cli): supervisor rules, rules test and Doctor check`

---

### Task 6: Rule in the mod's Log view; docs

**Files:** Modify: `assets/herdr-app/mods/tchelvis/main.js` (`logEventRow`, ~764), `tests/mod/log-view.test.mjs`, `README.md`, `CHANGELOG.md`, `ROADMAP.md`, `docs/superpowers/specs/2026-11-15-readonly-fast-path-design.md` (link to the new rules)

- [ ] **Step 1: Failing test**

```js
test("logEventRow shows the rule that decided", () => {
  const row = (ev) => logEventRow({ ts_ms: 1, kind: "auto_reply", summary: "s", ...ev });
  assert.equal(row({ detail: "d", rule_id: "allow_command:just test" }).detail, "d\nRegra: allow_command:just test");
  assert.equal(row({ rule_id: "builtin_read_only" }).detail, "Regra: builtin_read_only");
  assert.equal(row({ detail: "d" }).detail, "d");
  assert.equal(row({}).detail, null);
});
```

- [ ] **Step 2:** `node --import ./tests/mod/stub-hooks.mjs --test tests/mod/*.test.mjs` → FAIL.
- [ ] **Step 3: Implement**, then document: the schema block from the spec, the order of decision, `rules test` examples, the warning that an allowed command really runs, and that invalid rules stop automatic replies. ROADMAP: M10 implemented, host validation pending.
- [ ] **Step 4:** mod tests, `node --check` on `main.js` and full Rust validation → PASS.
- [ ] **Step 5: Commit** `feat(mod): show the deciding rule in the Log; docs for [guardrails]`

---

## Host validation (after the nightly)

With a real Claude: `allow_commands = ["just --version"]` is answered without the LLM; `deny_commands = ["git log"]` is not; both show the rule in Log. Capture the `msg` of a multi-line command and of `Grep`/`Glob` requests (exact text) for a later task. Break the TOML on purpose: Doctor fails, nothing is answered, the LLM model stays the same.
