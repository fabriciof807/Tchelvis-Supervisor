mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tchelvis::loops::state::load_state;
use tchelvis::orch::store::OrchStore;
use tempfile::TempDir;

struct LoopHarness {
    tmp: TempDir,
    home: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
    runtime_dir: PathBuf,
    bin_dir: PathBuf,
    trace_dir: PathBuf,
    repo: PathBuf,
}

impl LoopHarness {
    fn new() -> Self {
        let tmp = TempDir::new().expect("create tempdir");
        let root = tmp.path();
        let home = root.join("home");
        let config_dir = home.join(".config/tchelvis");
        let data_dir = home.join(".local/share/tchelvis");
        let state_dir = home.join(".local/state/tchelvis");
        let runtime_dir = state_dir.join("run");
        let bin_dir = root.join("bin");
        let trace_dir = root.join("trace");
        let repo = root.join("repo");

        for dir in [
            &home,
            &config_dir,
            &data_dir,
            &state_dir,
            &runtime_dir,
            &bin_dir,
            &trace_dir,
            &repo,
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let h = Self {
            tmp,
            home,
            config_dir,
            data_dir,
            state_dir,
            runtime_dir,
            bin_dir,
            trace_dir,
            repo,
        };
        h.init_git_repo();
        h.install_fake_clis();
        h
    }

    fn tchelvis_bin() -> &'static str {
        env!("CARGO_BIN_EXE_tchelvis")
    }

    fn path_with_fake_bins(&self) -> String {
        let base = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
        format!("{}:{base}", self.bin_dir.display())
    }

    fn path_without_bwrap(&self) -> String {
        let nobwrap = self.tmp.path().join("nobwrap-bin");
        fs::create_dir_all(&nobwrap).unwrap();
        for dir in std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        ) {
            let git_cand = dir.join("git");
            if git_cand.is_file() && !nobwrap.join("git").exists() {
                let _ = std::os::unix::fs::symlink(&git_cand, nobwrap.join("git"));
                break;
            }
        }
        format!("{}:{}", self.bin_dir.display(), nobwrap.display())
    }

    fn git(&self, args: &[&str]) -> Output {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn init_git_repo(&self) {
        self.git(&["init", "-b", "main"]);
        self.git(&["config", "user.name", "Test User"]);
        self.git(&["config", "user.email", "test@example.com"]);
        fs::create_dir_all(self.repo.join("src")).unwrap();
        fs::write(self.repo.join("README.md"), "# Test Repo\n").unwrap();
        fs::write(
            self.repo.join("src/lib.rs"),
            "pub fn value() -> i32 {\n    1\n}\n",
        )
        .unwrap();
        fs::write(self.repo.join(".gitignore"), "target/\n").unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", "initial commit"]);
    }

    fn cmd_in(&self, dir: &Path) -> Command {
        let mut cmd = Command::new(Self::tchelvis_bin());
        cmd.current_dir(dir)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("PATH", self.path_with_fake_bins())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("TCHELVIS_TEST_CONTAINER", "1")
            .env("TCHELVIS_TEST_NO_DAEMON", "1")
            .env("LOOP_TEST_TRACE_DIR", &self.trace_dir)
            .env_remove("HC_TERMINAL_ID")
            .env_remove("OPENCODE_API_KEY")
            .env_remove("SHELL");
        cmd
    }

    fn cmd(&self) -> Command {
        self.cmd_in(&self.repo)
    }

    fn write_config(
        &self,
        max_concurrent: u32,
        auto_approve: bool,
        sandbox_kind: &str,
        extra_toml: &str,
    ) {
        let body = format!(
            "max_concurrent = {max_concurrent}\n\
             auto_approve_in_sandbox = {auto_approve}\n\
             env_pass = [\"LOOP_TEST_TRACE_DIR\", \"LOOP_TEST_CLAUDE_FAIL\", \"LOOP_TEST_CLAUDE_FAIL_PROBE\", \"LOOP_TEST_PROBE_READY_FILE\", \"LOOP_TEST_PROBE_SLEEP_SECS\", \"LOOP_TEST_AGY_FAIL_PROBE\", \"LOOP_TEST_AGY_FAIL\", \"LOOP_TEST_AGY_MALFORMED_VERDICT\", \"LOOP_TEST_AGY_MISSING_OUT\", \"LOOP_TEST_AGY_MODELS_SLEEP_SECS\", \"LOOP_TEST_CLAUDE_AUTH_SLEEP_SECS\", \"LOOP_TEST_CLAUDE_NO_PROFILE\", \"LOOP_TEST_CHECK_EVIDENCE_PATH\", \"LOOP_TEST_CHECK_EVIDENCE_OUT\", \"LOOP_TEST_OPENCODE_FAIL_PROBE\", \"LOOP_TEST_GATE_VERDICT\", \"LOOP_TEST_EDIT_WORKSPACE\", \"LOOP_TEST_READY_FILE\", \"LOOP_TEST_SLEEP_SECS\", \"LOOP_TEST_LEAK_SECRET\", \"LOOP_TEST_ESCAPE_ANSI\", \"LOOP_TEST_SPAWN_SETSID_PIDFILE\"]\n\
             \n\
             [sandbox]\n\
             kind = \"{sandbox_kind}\"\n\
             \n\
             {extra_toml}\n"
        );
        fs::create_dir_all(&self.config_dir).unwrap();
        let path = self.config_dir.join("loops.toml");
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn write_default_inherited_config(&self, max_concurrent: u32, auto_approve: bool) {
        let extra = "[defaults.worker]\nprovider = \"claude\"\nmodel = \"sonnet\"\n\n\
                     [defaults.reviewer]\nprovider = \"agy\"\nmodel = \"gemini-test\"\n\n\
                     [defaults.critic]\nprovider = \"claude\"\nmodel = \"haiku\"\n\n\
                     [defaults.designer]\nprovider = \"claude\"\nmodel = \"sonnet\"\n\n\
                     [defaults.planner]\nprovider = \"claude\"\nmodel = \"sonnet\"\n\n\
                     [defaults.tester]\nprovider = \"opencode\"\nmodel = \"opencode/free-test\"\n";
        self.write_config(max_concurrent, auto_approve, "inherited", extra);
    }

    fn write_accounts(&self) {
        let claude_cred = self.home.join(".claude/.credentials.json");
        fs::create_dir_all(claude_cred.parent().unwrap()).unwrap();
        fs::write(&claude_cred, "{\"token\":\"dummy-claude-token\"}\n").unwrap();

        let agy_cred = self
            .home
            .join(".gemini/antigravity-cli/antigravity-oauth-token");
        fs::create_dir_all(agy_cred.parent().unwrap()).unwrap();
        fs::write(&agy_cred, "dummy-agy-token\n").unwrap();
    }

    fn install_fake_clis(&self) {
        // Fake `claude`: inspects argv & stdin to separate auth/status, preflight probes, and real stages.
        let claude_script = r#"#!/usr/bin/env python3
import json, os, pathlib, signal, subprocess, sys, time

argv = sys.argv[1:]
if argv[:2] == ["auth", "status"]:
    if os.environ.get("LOOP_TEST_CLAUDE_AUTH_SLEEP_SECS"):
        time.sleep(float(os.environ["LOOP_TEST_CLAUDE_AUTH_SLEEP_SECS"]))
    cred = pathlib.Path(os.environ.get("HOME", "")) / ".claude" / ".credentials.json"
    logged_in = cred.is_file() and cred.stat().st_size > 0
    # The real CLI does not know the profile inside the sandbox.
    profile = os.environ.get("LOOP_TEST_CLAUDE_NO_PROFILE") != "1"
    if logged_in:
        print(json.dumps({
            "loggedIn": True,
            "authMethod": "claude.ai",
            "apiProvider": "firstParty",
            "email": "fake-user@example.com" if profile else None,
            "orgId": "org-secret-999" if profile else None,
            "orgName": "SecretFakeOrg" if profile else None,
            "loginExpiresAt": "2026-12-09T01:02:03.456Z",
        }))
    else:
        print(json.dumps({"loggedIn": False, "authMethod": "none"}))
    sys.exit(0)
if argv and argv[0] == "auth":
    sys.exit(0)
prompt = sys.stdin.read()
trace_dir = os.environ.get("LOOP_TEST_TRACE_DIR")
if trace_dir and pathlib.Path(trace_dir).is_dir():
    rec = {
        "cli": "claude",
        "argv": argv,
        "cwd": os.getcwd(),
        "prompt": prompt,
        "has_opencode_key": "OPENCODE_API_KEY" in os.environ,
        "opencode_key": os.environ.get("OPENCODE_API_KEY"),
    }
    p = pathlib.Path(trace_dir) / f"claude-{os.getpid()}-{time.time_ns()}.json"
    try:
        p.write_text(json.dumps(rec))
    except OSError:
        pass

if prompt.strip() == "Reply with exactly: OK":
    if os.environ.get("LOOP_TEST_PROBE_READY_FILE"):
        pathlib.Path(os.environ["LOOP_TEST_PROBE_READY_FILE"]).write_text(str(os.getpid()))
    if os.environ.get("LOOP_TEST_PROBE_SLEEP_SECS"):
        time.sleep(float(os.environ["LOOP_TEST_PROBE_SLEEP_SECS"]))
    if os.environ.get("LOOP_TEST_CLAUDE_FAIL_PROBE") == "1":
        msg = "claude probe boom"
        leak = os.environ.get("LOOP_TEST_LEAK_SECRET")
        if leak:
            msg = f"probe failed with key {leak}"
        print(json.dumps({"type": "result", "is_error": True, "result": msg}))
        sys.exit(0)
    print(json.dumps({
        "type": "result",
        "is_error": False,
        "num_turns": 1,
        "total_cost_usd": 0.0005,
        "usage": {"input_tokens": 10, "output_tokens": 2, "cache_read_input_tokens": 0},
        "result": "OK"
    }))
    sys.exit(0)

if os.environ.get("LOOP_TEST_CHECK_EVIDENCE_PATH") and os.environ.get("LOOP_TEST_CHECK_EVIDENCE_OUT"):
    ev_p = pathlib.Path(os.environ["LOOP_TEST_CHECK_EVIDENCE_PATH"])
    out_file = pathlib.Path.cwd() / os.environ["LOOP_TEST_CHECK_EVIDENCE_OUT"]
    out_file.write_text("READABLE" if ev_p.exists() else "BLOCKED")

if os.environ.get("LOOP_TEST_READY_FILE"):
    pathlib.Path(os.environ["LOOP_TEST_READY_FILE"]).write_text(str(os.getpid()))

if os.environ.get("LOOP_TEST_SPAWN_SETSID_PIDFILE"):
    proc = subprocess.Popen(
        ["/bin/sh", "-c", "sleep 60"],
        start_new_session=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    pathlib.Path(os.environ["LOOP_TEST_SPAWN_SETSID_PIDFILE"]).write_text(str(proc.pid))

if os.environ.get("LOOP_TEST_SLEEP_SECS"):
    time.sleep(float(os.environ["LOOP_TEST_SLEEP_SECS"]))

if os.environ.get("LOOP_TEST_CLAUDE_FAIL") == "1":
    print(json.dumps({"type": "result", "is_error": True, "result": "claude stage failed"}))
    sys.exit(1)

cwd = pathlib.Path.cwd()
if os.environ.get("LOOP_TEST_EDIT_WORKSPACE", "1") == "1" and "Papel / Papel" in prompt or "papel: worker" in prompt.lower() or "edits" in prompt.lower() or ".tchelvis-loop/out/" not in prompt:
    pass

# Detect writes path from prompt: `.tchelvis-loop/out/<name>`
writes_name = None
for line in prompt.splitlines():
    if ".tchelvis-loop/out/" in line:
        idx = line.find(".tchelvis-loop/out/") + len(".tchelvis-loop/out/")
        rest = line[idx:].strip()
        token = rest.split()[0].rstrip(".,;:)`'\"")
        if token and "/" not in token:
            writes_name = token

if writes_name is None:
    if os.environ.get("LOOP_TEST_EDIT_WORKSPACE", "1") == "1":
        (cwd / "agent_work.txt").write_text("implemented by fake worker\n")
else:
    verdict = os.environ.get("LOOP_TEST_GATE_VERDICT", "PASS")
    summary = "Everything verified cleanly"
    if os.environ.get("LOOP_TEST_ESCAPE_ANSI") == "1":
        summary = "Clean \x1b[31mANSI\x1b[0m and control\x07 text"
    leak = os.environ.get("LOOP_TEST_LEAK_SECRET")
    if leak:
        summary = f"Summary with secret {leak}"
    out_path = cwd / ".tchelvis-loop" / "out" / writes_name
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(f"Notes\n\n{summary}\nVERDICT: {verdict}\n")

if os.environ.get("LOOP_TEST_ESCAPE_ANSI") == "1":
    sys.stderr.write("agent stderr \x1b[31mred\x1b[0m\n")

print(json.dumps({
    "type": "result",
    "is_error": False,
    "num_turns": 2,
    "total_cost_usd": 0.01,
    "usage": {"input_tokens": 100, "output_tokens": 40, "cache_read_input_tokens": 20},
    "result": "done"
}))
"#;
        common::write_executable(self.bin_dir.join("claude"), claude_script);

        // Fake `agy`: handles `agy models` and `-p <prompt>`
        let agy_script = r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time

argv = sys.argv[1:]
if argv[:1] == ["models"]:
    if os.environ.get("LOOP_TEST_AGY_MODELS_SLEEP_SECS"):
        time.sleep(float(os.environ["LOOP_TEST_AGY_MODELS_SLEEP_SECS"]))
    tok = pathlib.Path(os.environ.get("HOME", "")) / ".gemini" / "antigravity-cli" / "antigravity-oauth-token"
    if tok.is_file() and tok.stat().st_size > 0:
        for row in [
            ("gemini-test", "Gemini Test"),
            ("gemini-4-hydrogen-high", "Gemini 4 Hydrogen (High)"),
            ("gemini-4-neon-high", "Gemini 4 Neon (High)"),
            ("gemini-40-high", "Gemini 40 (High)"),
            ("claude-opus-5-5-low", "Claude Opus 5.5 (Low)"),
        ]:
            print(f"{row[0]}\t{row[1]}")
    sys.exit(0)

prompt = ""
if "-p" in argv:
    idx = argv.index("-p")
    if idx + 1 < len(argv):
        prompt = argv[idx + 1]

trace_dir = os.environ.get("LOOP_TEST_TRACE_DIR")
if trace_dir and pathlib.Path(trace_dir).is_dir():
    rec = {
        "cli": "agy",
        "argv": argv,
        "cwd": os.getcwd(),
        "prompt": prompt,
        "has_opencode_key": "OPENCODE_API_KEY" in os.environ,
    }
    p = pathlib.Path(trace_dir) / f"agy-{os.getpid()}-{time.time_ns()}.json"
    try:
        p.write_text(json.dumps(rec))
    except OSError:
        pass

if prompt.strip() == "Reply with exactly: OK":
    if os.environ.get("LOOP_TEST_AGY_FAIL_PROBE") == "1":
        print(json.dumps({"event": "result", "result": {"status": "ERROR", "error": "agy probe denied"}}))
        sys.exit(1)
    print(json.dumps({"event": "step_update", "step_update": {"step_type": "agent_response", "state": "DONE"}}))
    print(json.dumps({
        "event": "result",
        "result": {
            "status": "SUCCESS",
            "usage": {"input_tokens": 5, "output_tokens": 1, "cache_read_tokens": 0}
        }
    }))
    sys.exit(0)

if os.environ.get("LOOP_TEST_AGY_FAIL") == "1":
    print(json.dumps({"event": "result", "result": {"status": "ERROR", "error": "agy stage failed"}}))
    sys.exit(1)

writes_name = None
for line in prompt.splitlines():
    if ".tchelvis-loop/out/" in line:
        idx = line.find(".tchelvis-loop/out/") + len(".tchelvis-loop/out/")
        rest = line[idx:].strip()
        token = rest.split()[0].rstrip(".,;:)`'\"")
        if token and "/" not in token:
            writes_name = token

if writes_name and os.environ.get("LOOP_TEST_AGY_MISSING_OUT") != "1":
    out_path = pathlib.Path.cwd() / ".tchelvis-loop" / "out" / writes_name
    out_path.parent.mkdir(parents=True, exist_ok=True)
    if os.environ.get("LOOP_TEST_AGY_MALFORMED_VERDICT") == "1":
        out_path.write_text("Reviewed without a verdict line\n")
    else:
        verdict = os.environ.get("LOOP_TEST_GATE_VERDICT", "PASS")
        out_path.write_text(f"Reviewed OK\nVERDICT: {verdict}\n")

print(json.dumps({"event": "step_update", "step_update": {"step_type": "agent_response", "state": "DONE"}}))
print(json.dumps({
    "event": "result",
    "result": {
        "status": "SUCCESS",
        "usage": {"input_tokens": 50, "output_tokens": 20, "cache_read_tokens": 5}
    }
}))
"#;
        common::write_executable(self.bin_dir.join("agy"), agy_script);

        // Fake `opencode`: handles `opencode models --standalone` and stdin prompts
        let opencode_script = r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time

argv = sys.argv[1:]
if argv[:1] == ["models"]:
    models = [
        "opencode/beam-free",
        "opencode/free-test",
        "opencode/glm-4.7-free",
        "opencode/proj-def-r",
        "openrouter/x:free",
    ]
    if os.environ.get("OPENCODE_API_KEY"):
        models.extend([
            "opencode-go/glm-paid",
            "opencode-go/glm-5.3-flash",
            "opencode-go/hy4-flash-free",
        ])
    for m in models:
        print(m)
    sys.exit(0)

prompt = sys.stdin.read()
trace_dir = os.environ.get("LOOP_TEST_TRACE_DIR")
if trace_dir and pathlib.Path(trace_dir).is_dir():
    rec = {
        "cli": "opencode",
        "argv": argv,
        "cwd": os.getcwd(),
        "prompt": prompt,
        "has_opencode_key": "OPENCODE_API_KEY" in os.environ,
        "opencode_key": os.environ.get("OPENCODE_API_KEY"),
    }
    p = pathlib.Path(trace_dir) / f"opencode-{os.getpid()}-{time.time_ns()}.json"
    try:
        p.write_text(json.dumps(rec))
    except OSError:
        pass

if prompt.strip() == "Reply with exactly: OK":
    if os.environ.get("LOOP_TEST_OPENCODE_FAIL_PROBE") == "1":
        leak = os.environ.get("OPENCODE_API_KEY", "")
        print(json.dumps({"type": "error", "error": {"message": f"opencode auth error {leak}"}}))
        sys.exit(1)
    print(json.dumps({"type": "step_finish", "part": {"cost": 0.0, "tokens": {"input": 8, "output": 2, "cache": {"read": 0}}}}))
    sys.exit(0)

writes_name = None
for line in prompt.splitlines():
    if ".tchelvis-loop/out/" in line:
        idx = line.find(".tchelvis-loop/out/") + len(".tchelvis-loop/out/")
        rest = line[idx:].strip()
        token = rest.split()[0].rstrip(".,;:)`'\"")
        if token and "/" not in token:
            writes_name = token

leak = os.environ.get("LOOP_TEST_LEAK_SECRET", "")
if writes_name:
    out_path = pathlib.Path.cwd() / ".tchelvis-loop" / "out" / writes_name
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(f"Tester report {leak}\nVERDICT: PASS\n")
else:
    (pathlib.Path.cwd() / "opencode_work.txt").write_text(f"opencode {leak}\n")

print(json.dumps({"type": "step_finish", "part": {"cost": 0.002, "tokens": {"input": 30, "output": 10, "cache": {"read": 4}}}}))
"#;
        common::write_executable(self.bin_dir.join("opencode"), opencode_script);

        // Fake `hc`: fast POSIX sh implementing `connect` JSON-RPC and `run --bg --no-notify --outlive-parent ...`
        let hc_script = r#"#!/bin/sh
set -eu
trace_dir="${LOOP_TEST_TRACE_DIR:-}"

if [ "${1:-}" = "connect" ]; then
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    id="${line#*\"id\":}"
    id="${id%%[!0-9]*}"
    term="${line#*\"terminal\":\"}"
    term="${term%%\"*}"
    [ -n "$term" ] || term="t1"
    printf '{"jsonrpc":"2.0","id":%s,"result":{"address":"%s","created_ms":1000,"state":"idle"}}\n' "$id" "$term"
  done
  exit 0
fi

if [ "${1:-}" = "run" ]; then
  if [ -n "$trace_dir" ]; then
    j='['
    f=1
    for a in "$@"; do
      case "$a" in
        *\"*|*\\*) esc=$(printf '%s' "$a" | sed 's/\\/\\\\/g; s/"/\\"/g') ;;
        *) esc="$a" ;;
      esac
      if [ "$f" = 1 ]; then
        j="$j\"$esc\""
        f=0
      else
        j="$j,\"$esc\""
      fi
    done
    printf '%s]\n' "$j" > "$trace_dir/hc_run_argv.json"
  fi
  if [ "${LOOP_TEST_HC_FAIL:-}" = "1" ]; then
    printf 'fake hc run failure\n' >&2
    exit 1
  fi
  shift
  cwd=""
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --cwd)
        cwd="$2"
        shift 2
        ;;
      --json)
        shift
        break
        ;;
      *)
        shift
        ;;
    esac
  done
  if [ "${LOOP_TEST_HC_SKIP_SPAWN:-}" != "1" ] && [ "$#" -gt 0 ]; then
    (
      if [ -n "$cwd" ]; then
        cd "$cwd" || exit 1
      fi
      # Like the real Herdr: the terminal does not inherit the caller's PATH.
      setsid env PATH=/usr/bin:/bin "$@" </dev/null >/dev/null 2>&1 &
    )
  fi
  printf '{"address":"t-loop-1","created_ms":2000}\n'
  exit 0
fi

exit 0
"#;
        common::write_executable(self.bin_dir.join("hc"), hc_script);
    }

    fn read_traces(&self, cli: &str) -> Vec<serde_json::Value> {
        let mut items = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.trace_dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(&format!("{cli}-")) && name.ends_with(".json") {
                    let raw = fs::read_to_string(e.path()).unwrap();
                    items.push(serde_json::from_str(&raw).unwrap());
                }
            }
        }
        items
    }

    fn clear_traces(&self) {
        if let Ok(entries) = fs::read_dir(&self.trace_dir) {
            for e in entries.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

#[test]
fn init_twice_keeps_edited_files_and_does_not_touch_repo_gitignore() {
    let h = LoopHarness::new();
    let repo_gitignore_before = fs::read_to_string(h.repo.join(".gitignore")).unwrap();

    let out1 = h.cmd().args(["loop", "init"]).output().unwrap();
    assert_eq!(out1.status.code(), Some(0), "{:?}", out1);

    let cfg_path = h.config_dir.join("loops.toml");
    let mode = fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(fs::read_to_string(&cfg_path)
        .unwrap()
        .contains("auto_approve_in_sandbox = false"));
    assert_eq!(
        fs::read_to_string(h.repo.join(".loops/runs/.gitignore")).unwrap(),
        "*\n"
    );
    assert_eq!(
        fs::read_to_string(h.repo.join(".gitignore")).unwrap(),
        repo_gitignore_before
    );

    // Customize generated files
    let custom_cfg =
        "# user customized\nauto_approve_in_sandbox = true\n[sandbox]\nkind = \"inherited\"\n";
    let role_path = h.repo.join(".loops/roles/worker.md");
    let wf_path = h.repo.join(".loops/workflows/fast-worker-reviewer.toml");
    fs::write(&cfg_path, custom_cfg).unwrap();
    fs::write(&role_path, "# customized worker\n").unwrap();
    fs::write(&wf_path, "# customized workflow\n").unwrap();

    let out2 = h.cmd().args(["loop", "init"]).output().unwrap();
    assert_eq!(out2.status.code(), Some(0));
    assert_eq!(fs::read_to_string(&cfg_path).unwrap(), custom_cfg);
    assert_eq!(
        fs::read_to_string(&role_path).unwrap(),
        "# customized worker\n"
    );
    assert_eq!(
        fs::read_to_string(&wf_path).unwrap(),
        "# customized workflow\n"
    );
    assert_eq!(
        fs::read_to_string(h.repo.join(".gitignore")).unwrap(),
        repo_gitignore_before
    );
}

#[test]
fn loop_new_generates_valid_workflows_keeps_existing_roles_and_rejects_invalid() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    // Pre-create a custom worker role to verify `loop new` keeps existing roles untouched
    fs::create_dir_all(h.repo.join(".loops/roles")).unwrap();
    fs::write(
        h.repo.join(".loops/roles/worker.md"),
        "CUSTOM WORKER DO NOT OVERWRITE\n",
    )
    .unwrap();

    // Generate workflow with repeated role + unknown role `auditor`
    let out = h
        .cmd()
        .args([
            "loop",
            "new",
            "team-flow",
            "--stages",
            "planner,worker,worker,auditor,reviewer",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("name = \"team-flow\""));
    assert!(stdout.contains("id = \"worker-2\""));
    assert!(stdout.contains("on_retry = \"worker\""));

    // Existing role preserved, missing custom role created
    assert_eq!(
        fs::read_to_string(h.repo.join(".loops/roles/worker.md")).unwrap(),
        "CUSTOM WORKER DO NOT OVERWRITE\n"
    );
    assert!(h.repo.join(".loops/roles/auditor.md").is_file());

    // Confirm generated workflow validates with `loop check` (needs defaults for auditor)
    let chk = h
        .cmd()
        .args([
            "loop",
            "check",
            "team-flow",
            "--use",
            "auditor=claude:haiku",
        ])
        .output()
        .unwrap();
    assert_eq!(
        chk.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&chk.stderr)
    );

    // Invalid cases return exit 2 and write nothing
    let invalid_cases = [
        vec!["loop", "new", "no-edit", "--stages", "planner,reviewer"],
        vec!["loop", "new", "edit-last", "--stages", "planner,worker"],
        vec!["loop", "new", "bad-role", "--stages", "worker,BadRole!"],
        vec!["loop", "new", "team-flow", "--stages", "worker,reviewer"], // already exists
    ];
    for args in invalid_cases {
        let res = h.cmd().args(&args).output().unwrap();
        assert_eq!(res.status.code(), Some(2), "expected exit 2 for {:?}", args);
    }
    assert!(!h.repo.join(".loops/workflows/no-edit.toml").exists());
    assert!(!h.repo.join(".loops/workflows/edit-last.toml").exists());
    assert!(!h.repo.join(".loops/workflows/bad-role.toml").exists());
}

#[test]
fn run_pass_error_escalate_and_cancel_outcomes_exit_codes_5line_stdout_and_branch() {
    let h = LoopHarness::new();
    // Presets must work even with ZERO `.loops/` directory in the repository!
    assert!(!h.repo.join(".loops").exists());
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    let head_before = String::from_utf8_lossy(&h.git(&["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();

    // 1. PASS using preset `fast-worker-reviewer` + ANSI escape in stderr/summary (to verify sanitization)
    let pass_out = h
        .cmd()
        .env("LOOP_TEST_ESCAPE_ANSI", "1")
        .args(["loop", "run", "fast-worker-reviewer", "add feature"])
        .output()
        .unwrap();
    assert_eq!(
        pass_out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&pass_out.stderr)
    );
    let pass_stdout = String::from_utf8_lossy(&pass_out.stdout);
    let lines: Vec<&str> = pass_stdout.lines().collect();
    assert!(lines.len() <= 5 && !lines.is_empty(), "lines={:?}", lines);
    assert!(lines[0].starts_with("STATUS: PASS | LOOP: fast-worker-reviewer"));
    assert!(!pass_stdout.contains('\x1b'), "ANSI leaked into stdout");

    let run_id = lines[1]
        .strip_prefix("RUN_ID: ")
        .unwrap()
        .trim()
        .to_string();
    let branch = lines[2]
        .strip_prefix("BRANCH: ")
        .unwrap()
        .trim()
        .to_string();
    assert!(branch.starts_with("loop/fast-worker-reviewer-"));

    // Checkout and HEAD untouched; branch created with agent work
    let head_after = String::from_utf8_lossy(&h.git(&["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    assert_eq!(head_before, head_after);
    assert!(!h.repo.join("agent_work.txt").exists());
    let branch_files =
        String::from_utf8_lossy(&h.git(&["ls-tree", "-r", "--name-only", &branch]).stdout)
            .into_owned();
    assert!(branch_files.contains("agent_work.txt"));

    // Verify `result`, `status`, `logs`, `report`
    let res_out = h.cmd().args(["loop", "result", &run_id]).output().unwrap();
    assert_eq!(res_out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&res_out.stdout).lines().count(), 5);

    let logs_out = h
        .cmd()
        .args(["loop", "logs", &run_id, "--stage", "worker"])
        .output()
        .unwrap();
    assert_eq!(logs_out.status.code(), Some(0));
    let logs_txt = String::from_utf8_lossy(&logs_out.stdout);
    assert!(!logs_txt.contains('\x1b'), "ANSI leaked into logs");

    let rep_out = h.cmd().args(["loop", "report", &run_id]).output().unwrap();
    assert_eq!(rep_out.status.code(), Some(0));
    let rep_txt = String::from_utf8_lossy(&rep_out.stdout);
    assert!(!rep_txt.contains('\x1b'), "ANSI leaked into report");
    assert!(rep_txt.contains("Pré-teste"));

    // 2. ERROR when stage fails -> exit 1, <= 5 stdout lines
    let err_out = h
        .cmd()
        .env("LOOP_TEST_CLAUDE_FAIL", "1")
        .args(["loop", "run", "fast-worker-reviewer", "fail please"])
        .output()
        .unwrap();
    assert_eq!(err_out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&err_out.stdout).lines().count() <= 5);

    // 3. ESCALATE when max_cycles=1 and gate says RETRY -> exit 3, <= 5 stdout lines
    let esc_out = h
        .cmd()
        .env("LOOP_TEST_GATE_VERDICT", "RETRY")
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "--max-cycles",
            "1",
            "keep retrying",
        ])
        .output()
        .unwrap();
    assert_eq!(esc_out.status.code(), Some(3));
    let esc_stdout = String::from_utf8_lossy(&esc_out.stdout);
    assert!(esc_stdout.lines().count() <= 5);
    assert!(esc_stdout.contains("STATUS: ESCALATE"));

    // 4. CANCELLED via SIGINT during active stage -> exit 4 + setsid child killed
    let ready_file = h.tmp.path().join("stage_ready");
    let pid_file = h.tmp.path().join("setsid_child_pid");
    let child = h
        .cmd()
        .env("LOOP_TEST_READY_FILE", &ready_file)
        .env("LOOP_TEST_SPAWN_SETSID_PIDFILE", &pid_file)
        .env("LOOP_TEST_SLEEP_SECS", "30")
        .args(["loop", "run", "fast-worker-reviewer", "slow task"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    // The file exists before its content is written: wait for a whole number.
    let grandchild_pid: i32 = loop {
        let pid = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|raw| raw.trim().parse().ok());
        match pid {
            Some(pid) if ready_file.exists() => break pid,
            _ => assert!(
                Instant::now() < deadline,
                "timed out waiting for stage ready"
            ),
        }
        thread::sleep(Duration::from_millis(15));
    };

    // Send SIGINT to the `tchelvis loop run` process
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    let cancel_out = child.wait_with_output().unwrap();
    assert_eq!(
        cancel_out.status.code(),
        Some(4),
        "stderr={}",
        String::from_utf8_lossy(&cancel_out.stderr)
    );
    let cancel_stdout = String::from_utf8_lossy(&cancel_out.stdout);
    assert!(cancel_stdout.lines().count() <= 5);
    assert!(cancel_stdout.contains("STATUS: CANCELLED"));

    // Reap check: setsid grandchild must be dead
    let kill_deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(grandchild_pid, 0) } == 0 {
        assert!(
            Instant::now() < kill_deadline,
            "setsid grandchild {grandchild_pid} still alive after cancel"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn usage_errors_missing_bwrap_and_path_like_run_id_refused() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, false); // auto_approve_in_sandbox = false
    h.write_accounts();

    // auto_approve_in_sandbox = false -> exit 2
    let no_approve = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "hello"])
        .output()
        .unwrap();
    assert_eq!(no_approve.status.code(), Some(2));

    h.write_default_inherited_config(2, true);

    // --detach without Herdr -> exit 2
    let no_hc = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "hello", "--detach"])
        .output()
        .unwrap();
    assert_eq!(no_hc.status.code(), Some(2));

    // prompt + --input-file -> exit 2
    let inp = h.tmp.path().join("prompt.txt");
    fs::write(&inp, "task from file\n").unwrap();
    let conflict = h
        .cmd()
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "inline",
            "--input-file",
        ])
        .arg(&inp)
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(2));

    // neither prompt nor --input-file -> exit 2
    let missing_prompt = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer"])
        .output()
        .unwrap();
    assert_eq!(missing_prompt.status.code(), Some(2));

    // --use unknown stage -> exit 2
    let bad_stage = h
        .cmd()
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "hello",
            "--use",
            "ghost=claude",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_stage.status.code(), Some(2));

    // --use unknown CLI or bad syntax -> exit 2
    let bad_cli = h
        .cmd()
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "hello",
            "--use",
            "worker=unknowncli",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_cli.status.code(), Some(2));

    let bad_syn = h
        .cmd()
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "hello",
            "--use",
            "missing_equals",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_syn.status.code(), Some(2));

    // Path-like run_id `../x` refused everywhere (exit 2)
    for args in [
        vec!["loop", "status", "../x"],
        vec!["loop", "result", "../x"],
        vec!["loop", "logs", "../x"],
        vec!["loop", "report", "../x"],
        vec!["loop", "stop", "../x"],
        vec!["loop", "clean", "../x", "--yes"],
        vec!["loop", "engine", "../x"],
    ] {
        let out = h.cmd().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "args={:?}", args);
    }

    // Missing `bwrap` in default bwrap mode -> check & run say bubblewrap is missing and how to install
    h.write_config(
        2,
        true,
        "bwrap",
        "[defaults.worker]\nprovider = \"claude\"\n[defaults.reviewer]\nprovider = \"claude\"\n",
    );
    let bwrap_chk = h
        .cmd()
        .env("PATH", h.path_without_bwrap())
        .args(["loop", "check", "fast-worker-reviewer"])
        .output()
        .unwrap();
    assert_eq!(bwrap_chk.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bwrap_chk.stderr).contains("bubblewrap"));

    let bwrap_run = h
        .cmd()
        .env("PATH", h.path_without_bwrap())
        .args(["loop", "run", "fast-worker-reviewer", "hello"])
        .output()
        .unwrap();
    assert_eq!(
        bwrap_run.status.code(),
        Some(1),
        "stderr={}",
        String::from_utf8_lossy(&bwrap_run.stderr)
    );
    assert!(String::from_utf8_lossy(&bwrap_run.stderr).contains("bubblewrap"));
}

#[test]
fn max_concurrent_enforced_and_clean_behavior() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(1, true);
    h.write_accounts();

    // Run 1 finishes cleanly so we have a branch + run_dir + workspace to clean
    let pass = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "task one"])
        .output()
        .unwrap();
    assert_eq!(pass.status.code(), Some(0));
    let pass_stdout = String::from_utf8_lossy(&pass.stdout);
    let lines: Vec<&str> = pass_stdout.lines().collect();
    let finished_id = lines[1]
        .strip_prefix("RUN_ID: ")
        .unwrap()
        .trim()
        .to_string();
    let branch = lines[2]
        .strip_prefix("BRANCH: ")
        .unwrap()
        .trim()
        .to_string();

    // Start an active slow run; while active, a second run is refused (max_concurrent = 1)
    let ready_file = h.tmp.path().join("active_ready");
    let mut active_child = h
        .cmd()
        .env("LOOP_TEST_READY_FILE", &ready_file)
        .env("LOOP_TEST_SLEEP_SECS", "20")
        .args(["loop", "run", "fast-worker-reviewer", "active task"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready_file.exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(15));
    }

    let second = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "overflow task"])
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).contains("concorrência"));

    // Clean refuses active run
    let store = OrchStore::open(&h.data_dir.join("orch.db")).unwrap();
    let active_rec = store
        .list_loops()
        .unwrap()
        .into_iter()
        .find(|r| r.run_id != finished_id)
        .unwrap();
    let clean_active = h
        .cmd()
        .args(["loop", "clean", &active_rec.run_id, "--yes"])
        .output()
        .unwrap();
    assert_eq!(clean_active.status.code(), Some(1));

    // Stop active run cleanly
    let stop_out = h
        .cmd()
        .args(["loop", "stop", &active_rec.run_id])
        .output()
        .unwrap();
    assert_eq!(stop_out.status.code(), Some(0));
    let _ = active_child.wait();

    // Clean finished run without `--yes` (non-TTY) prints paths and keeps files
    let dry_clean = h
        .cmd()
        .args(["loop", "clean", &finished_id])
        .output()
        .unwrap();
    assert_eq!(dry_clean.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&dry_clean.stdout).contains(&finished_id));
    assert!(h.repo.join(".loops/runs").join(&finished_id).exists());

    // Clean with `--yes` removes disk artifacts + workspace while keeping the git branch
    let do_clean = h
        .cmd()
        .args(["loop", "clean", &finished_id, "--yes"])
        .output()
        .unwrap();
    assert_eq!(do_clean.status.code(), Some(0));
    assert!(!h.repo.join(".loops/runs").join(&finished_id).exists());
    assert!(!h.state_dir.join("loops").join(&finished_id).exists());
    let branches =
        String::from_utf8_lossy(&h.git(&["branch", "--list", &branch]).stdout).into_owned();
    assert!(branches.contains(&branch));
}

#[test]
fn detach_prints_only_run_id_spawns_engine_via_herdr_and_writes_single_wake_message() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    let det = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "detached task",
            "--detach",
        ])
        .output()
        .unwrap();
    assert_eq!(
        det.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&det.stderr)
    );
    let stdout = String::from_utf8_lossy(&det.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected only RUN_ID line, got {:?}", lines);
    let run_id = lines[0].trim().to_string();

    // Verify `hc run` args used by HerdrClient::build_run_worker_args
    let hc_argv: Vec<String> =
        serde_json::from_str(&fs::read_to_string(h.trace_dir.join("hc_run_argv.json")).unwrap())
            .unwrap();
    assert!(hc_argv
        .windows(3)
        .any(|w| w == ["--bg", "--no-notify", "--outlive-parent"]));
    assert!(hc_argv.contains(&format!("cockpit.loop_run={run_id}")));
    assert_eq!(hc_argv.last().map(String::as_str), Some(run_id.as_str()));

    // Wait for detached engine to finish cleanly
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let st = h
            .cmd()
            .args(["loop", "status", &run_id, "--json"])
            .output()
            .unwrap();
        if st.status.code() == Some(0) {
            if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&st.stdout) {
                if val.get("status").and_then(|v| v.as_str()) == Some("PASS") {
                    break;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "detached loop did not finish in time"
        );
        thread::sleep(Duration::from_millis(25));
    }

    // Verify single wake message recorded in orch.db and readable via `tchelvis orch check`
    // (recorded right after the result reaches the disk).
    let store = OrchStore::open(&h.data_dir.join("orch.db")).unwrap();
    let rec = loop {
        let rec = store.get_loop(&run_id).unwrap().unwrap();
        if rec.status != "RUNNING" {
            break rec;
        }
        assert!(Instant::now() < deadline, "the end was never recorded");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(rec.status, "PASS");
    let orch_run_id = rec.orch_run_id.expect("orch_run_id created");
    let check_out = h
        .cmd()
        .args(["orch", "check", "--run", &orch_run_id])
        .output()
        .unwrap();
    assert_eq!(check_out.status.code(), Some(0));
    let check_txt = String::from_utf8_lossy(&check_out.stdout);
    assert!(check_txt.contains(&format!(
        "Loop {run_id}: PASS. Rode: tchelvis loop result {run_id}"
    )));

    // Failed `hc run` marks the DB row ERROR so concurrency slot isn't leaked
    h.write_default_inherited_config(1, true);
    let fail_det = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .env("LOOP_TEST_HC_FAIL", "1")
        .args(["loop", "run", "fast-worker-reviewer", "hc fail", "--detach"])
        .output()
        .unwrap();
    assert_eq!(fail_det.status.code(), Some(1));
    let loops = store.list_loops().unwrap();
    assert!(loops.iter().all(|r| r.status != "RUNNING"));
}

#[test]
fn models_precedence_confirmation_set_comments_and_check_behavior() {
    let h = LoopHarness::new();
    h.write_accounts();
    // Level 5 global defaults
    h.write_config(
        2,
        true,
        "inherited",
        "[defaults.worker]\nprovider = \"claude\"\nmodel = \"global-w\"\n\
         [defaults.reviewer]\nprovider = \"agy\"\nmodel = \"global-r\"\n\
         [defaults.critic]\nprovider = \"claude\"\nmodel = \"global-c\"\n",
    );

    // Custom workflow with 3 stages: stage `implement` sets Level 3 workflow choice; `review` & `audit` omit provider.
    fs::create_dir_all(h.repo.join(".loops/workflows")).unwrap();
    fs::write(
        h.repo.join(".loops/workflows/mix-test.toml"),
        "name = \"mix-test\"\n\
         description = \"precedence\"\n\
         [[stages]]\n\
         id = \"implement\"\n\
         role = \"worker\"\n\
         provider = \"claude\"\n\
         model = \"wf-w\"\n\
         reads = [\"input.md\"]\n\
         edits = true\n\
         next = \"review\"\n\
         [[stages]]\n\
         id = \"review\"\n\
         role = \"reviewer\"\n\
         reads = [\"input.md\", \"current.diff\"]\n\
         writes = \"review.md\"\n\
         next = \"audit\"\n\
         [[stages]]\n\
         id = \"audit\"\n\
         role = \"critic\"\n\
         reads = [\"input.md\", \"review.md\"]\n\
         writes = \"critic.md\"\n\
         verdict_gate = true\n\
         on_pass = \"done\"\n\
         on_retry = \"implement\"\n",
    )
    .unwrap();

    // `models set --project` writes `.loops/models.toml` preserving comments
    fs::write(
        h.repo.join(".loops/models.toml"),
        "# keep this comment\n\n[stages.mix-test.implement]\nprovider = \"agy\"\nmodel = \"proj-stage-w\"\n\n[stages.unknown-wf.implement]\nprovider = \"claude\"\n",
    )
    .unwrap();
    let set_proj = h
        .cmd()
        .args([
            "loop",
            "models",
            "set",
            "reviewer=opencode:opencode/proj-def-r",
            "--project",
        ])
        .output()
        .unwrap();
    assert_eq!(set_proj.status.code(), Some(0));
    let models_toml = fs::read_to_string(h.repo.join(".loops/models.toml")).unwrap();
    assert!(models_toml.contains("# keep this comment"));

    // Symlink `.loops/models.toml` is refused by `models set`
    // (Test on a second temp file? Let's test `check` reporting unknown wf/stage first!)
    let chk = h
        .cmd()
        .args(["loop", "check", "mix-test"])
        .output()
        .unwrap();
    assert_eq!(chk.status.code(), Some(0));
    let chk_stderr = String::from_utf8_lossy(&chk.stderr);
    assert!(chk_stderr.contains("unknown-wf"));

    // Inspect resolved levels in `loop models mix-test`:
    // implement: Level 2 (agy:proj-stage-w), review: Level 4 (opencode:opencode/proj-def-r), audit: Level 5 (claude:global-c)
    let mod_out = h
        .cmd()
        .args(["loop", "models", "mix-test"])
        .output()
        .unwrap();
    assert_eq!(mod_out.status.code(), Some(0));
    let mod_txt = String::from_utf8_lossy(&mod_out.stdout);
    assert!(mod_txt.contains("implement (worker): agy:proj-stage-w"));
    assert!(mod_txt.contains("review (reviewer): opencode:opencode/proj-def-r"));
    assert!(mod_txt.contains("audit (critic): claude:global-c"));
    assert!(mod_txt.contains("models confirm mix-test"));

    // Non-TTY / `--detach` refuses repo choices without confirmation
    let unconf = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .args(["loop", "run", "mix-test", "task", "--detach"])
        .output()
        .unwrap();
    assert_eq!(unconf.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unconf.stderr).contains("models confirm mix-test"));

    // `--use` with `/` and `:` overrides cleanly without losing model string; if NO repo choice remains, skips confirmation!
    let all_use = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .args([
            "loop",
            "run",
            "mix-test",
            "task",
            "--use",
            "implement=opencode:openrouter/x:free",
            "--use",
            "review=claude:haiku",
            "--detach",
        ])
        .output()
        .unwrap();
    assert_eq!(
        all_use.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&all_use.stderr)
    );
    let det_id = String::from_utf8_lossy(&all_use.stdout).trim().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(st) = load_state(&h.repo.join(".loops/runs").join(&det_id)) {
            if st.status.is_final() {
                break;
            }
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }

    // Confirm repo choices non-interactively
    let conf = h
        .cmd()
        .args(["loop", "models", "confirm", "mix-test"])
        .output()
        .unwrap();
    assert_eq!(conf.status.code(), Some(0));

    // After confirmation, foreground non-TTY `run` succeeds!
    let run_ok = h
        .cmd()
        .args(["loop", "run", "mix-test", "confirmed task"])
        .output()
        .unwrap();
    assert_eq!(
        run_ok.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&run_ok.stderr)
    );

    // Mutate `.loops/models.toml` -> confirmation invalidated, must re-ask!
    let updated_models = models_toml.replace("proj-stage-w", "proj-stage-w-v2");
    fs::write(h.repo.join(".loops/models.toml"), updated_models).unwrap();
    let reask = h
        .cmd()
        .args(["loop", "run", "mix-test", "changed repo model"])
        .output()
        .unwrap();
    assert_eq!(reask.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&reask.stderr).contains("models confirm mix-test"));

    // `models set --global` preserves comments in loops.toml
    let loops_toml_path = h.config_dir.join("loops.toml");
    let before_global = format!(
        "# global comment\n{}",
        fs::read_to_string(&loops_toml_path).unwrap()
    );
    fs::write(&loops_toml_path, before_global).unwrap();
    let set_global = h
        .cmd()
        .args(["loop", "models", "set", "auditor=claude:haiku", "--global"])
        .output()
        .unwrap();
    assert_eq!(set_global.status.code(), Some(0));
    let after_global = fs::read_to_string(&loops_toml_path).unwrap();
    assert!(after_global.contains("# global comment"));
    assert!(after_global.contains("defaults.auditor"));

    // Symlink `.loops/models.toml` is refused by `models set --project`
    let real_target = h.tmp.path().join("outside_models.toml");
    fs::write(&real_target, "").unwrap();
    fs::remove_file(h.repo.join(".loops/models.toml")).unwrap();
    std::os::unix::fs::symlink(&real_target, h.repo.join(".loops/models.toml")).unwrap();
    let sym_set = h
        .cmd()
        .args(["loop", "models", "set", "worker=claude:sonnet", "--project"])
        .output()
        .unwrap();
    assert_ne!(sym_set.status.code(), Some(0));
    assert!(fs::read_to_string(&real_target).unwrap().is_empty());
}

#[test]
fn preflight_deduplicates_lists_all_failures_leaves_zero_side_effects_and_respects_check_and_dirty_repo(
) {
    let h = LoopHarness::new();
    h.write_accounts();
    h.write_config(
        2,
        true,
        "inherited",
        "[preflight]\nmode = \"always\"\n\n\
         [defaults.worker]\nprovider = \"claude\"\nmodel = \"haiku\"\n\
         [defaults.reviewer]\nprovider = \"agy\"\nmodel = \"agy-bad\"\n\
         [defaults.critic]\nprovider = \"claude\"\nmodel = \"haiku\"\n\
         [defaults.tester]\nprovider = \"opencode\"\nmodel = \"opencode/free-bad\"\n",
    );

    // Check spends zero tokens (0 probe traces)
    h.clear_traces();
    let chk_free = h
        .cmd()
        .args(["loop", "check", "worker-reviewer-critic"])
        .output()
        .unwrap();
    assert_eq!(chk_free.status.code(), Some(0));
    assert!(h.read_traces("claude").is_empty());
    assert!(h.read_traces("agy").is_empty());

    // `check --live` probes each distinct pair once (`worker` and `critic` share `claude:haiku` -> 1 claude probe, 1 agy probe)
    let chk_live = h
        .cmd()
        .args(["loop", "check", "worker-reviewer-critic", "--live"])
        .output()
        .unwrap();
    assert_eq!(chk_live.status.code(), Some(0));
    let claude_probes = h.read_traces("claude");
    let agy_probes = h.read_traces("agy");
    assert_eq!(claude_probes.len(), 1);
    assert_eq!(agy_probes.len(), 1);
    assert_eq!(claude_probes[0]["prompt"], "Reply with exactly: OK");
    assert!(claude_probes[0]["argv"]
        .as_array()
        .unwrap()
        .windows(2)
        .any(|w| w == ["--tools", ""]));
    assert!(!claude_probes[0]["prompt"]
        .as_str()
        .unwrap()
        .contains("Test Repo"));

    // Dirty repository is refused WITHOUT running preflight probes!
    h.clear_traces();
    fs::write(h.repo.join("README.md"), "# dirty uncommitted change\n").unwrap();
    let dirty = h
        .cmd()
        .args(["loop", "run", "worker-reviewer-critic", "task"])
        .output()
        .unwrap();
    assert_eq!(dirty.status.code(), Some(1));
    assert!(h.read_traces("claude").is_empty());
    assert!(h.read_traces("agy").is_empty());
    // Revert dirty change
    h.git(&["checkout", "--", "README.md"]);

    // Failing probes across ALL distinct targets (including retry-only or later stages) list EVERY failure and leave ZERO side effects
    h.clear_traces();
    let branches_before = String::from_utf8_lossy(&h.git(&["branch"]).stdout).into_owned();
    let fail_det = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .env("LOOP_TEST_CLAUDE_FAIL_PROBE", "1")
        .env("LOOP_TEST_AGY_FAIL_PROBE", "1")
        .args([
            "loop",
            "run",
            "worker-reviewer-critic",
            "should fail preflight",
            "--detach",
        ])
        .output()
        .unwrap();
    assert_eq!(fail_det.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&fail_det.stdout).trim().is_empty());
    let err_txt = String::from_utf8_lossy(&fail_det.stderr);
    assert!(err_txt.contains("claude:haiku"));
    assert!(err_txt.contains("implement"));
    assert!(err_txt.contains("audit"));
    assert!(err_txt.contains("agy:agy-bad"));
    assert!(err_txt.contains("review"));
    assert!(!h.repo.join(".loops/runs").exists());
    assert!(!h.trace_dir.join("hc_run_argv.json").exists());
    assert_eq!(
        String::from_utf8_lossy(&h.git(&["branch"]).stdout),
        branches_before
    );
    if h.data_dir.join("orch.db").exists() {
        let store = OrchStore::open(&h.data_dir.join("orch.db")).unwrap();
        assert!(store.list_loops().unwrap().is_empty());
    }
}

#[test]
fn accounts_and_opencode_api_key_isolation_shell_fallback_and_redaction() {
    let h = LoopHarness::new();
    // 1. Missing claude credentials refused before run
    h.write_default_inherited_config(2, true);
    let no_cred = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "task"])
        .output()
        .unwrap();
    assert_eq!(no_cred.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&no_cred.stderr).contains("claude"));

    h.write_accounts();

    // 2. An opencode-go model without OPENCODE_API_KEY is refused
    let no_key = h
        .cmd()
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "task",
            "--use",
            "implement=opencode:opencode-go/glm-paid",
        ])
        .output()
        .unwrap();
    assert_eq!(no_key.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&no_key.stderr).contains("OPENCODE_API_KEY"));

    // 3. Free `opencode/...` model runs WITHOUT OPENCODE_API_KEY
    let free_ok = h
        .cmd()
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "task",
            "--use",
            "implement=opencode:opencode/glm-4.7-free",
        ])
        .output()
        .unwrap();
    assert_eq!(
        free_ok.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&free_ok.stderr)
    );

    // 4. OPENCODE_API_KEY from noisy shell reaches ONLY opencode, never in argv, and is redacted everywhere if leaked
    h.clear_traces();
    let secret = "sk-opencode-super-secret-xyz987";
    let noisy_shell = h.tmp.path().join("noisy-user-shell");
    common::write_executable(
        &noisy_shell,
        format!(
            "#!/usr/bin/env sh\n\
             echo 'MOTD startup noise'\n\
             export OPENCODE_API_KEY='{secret}'\n\
             shift\n\
             sh -c \"$1\"\n"
        ),
    );

    // Test probe failure redaction too!
    let probe_fail = h
        .cmd()
        .env("SHELL", &noisy_shell)
        .env("LOOP_TEST_OPENCODE_FAIL_PROBE", "1")
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "task",
            "--use",
            "implement=opencode:opencode-go/glm-paid",
        ])
        .output()
        .unwrap();
    assert_eq!(probe_fail.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&probe_fail.stderr).contains(secret));

    // Successful run with OPENCODE_API_KEY resolved via noisy shell + simulated leak in stage output
    h.clear_traces();
    let paid_ok = h
        .cmd()
        .env("SHELL", &noisy_shell)
        .env("LOOP_TEST_LEAK_SECRET", secret)
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "task",
            "--use",
            "implement=opencode:opencode-go/glm-paid",
        ])
        .output()
        .unwrap();
    assert_eq!(paid_ok.status.code(), Some(0));
    let out_txt = String::from_utf8_lossy(&paid_ok.stdout);
    let run_id = out_txt
        .lines()
        .nth(1)
        .unwrap()
        .strip_prefix("RUN_ID: ")
        .unwrap()
        .trim()
        .to_string();

    // Verify opencode got the key in env (never in argv), and agy did NOT get it
    let oc_traces = h.read_traces("opencode");
    assert!(!oc_traces.is_empty());
    for tr in &oc_traces {
        assert_eq!(tr["opencode_key"].as_str(), Some(secret));
        let argv_joined = serde_json::to_string(&tr["argv"]).unwrap();
        assert!(!argv_joined.contains(secret));
    }
    let agy_traces = h.read_traces("agy");
    assert!(!agy_traces.is_empty());
    for tr in &agy_traces {
        assert_eq!(tr["has_opencode_key"].as_bool(), Some(false));
    }

    // Verify secret redacted from state.json, report.md, and logs
    let run_dir = h.repo.join(".loops/runs").join(&run_id);
    let state_raw = fs::read_to_string(run_dir.join("state.json")).unwrap();
    let report_raw = fs::read_to_string(run_dir.join("report.md")).unwrap();
    assert!(!state_raw.contains(secret));
    assert!(!report_raw.contains(secret));
    let logs_out = h.cmd().args(["loop", "logs", &run_id]).output().unwrap();
    assert!(!String::from_utf8_lossy(&logs_out.stdout).contains(secret));
}

#[test]
fn ctrl_c_during_preflight_exits_4_and_leaves_no_artifacts() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    let probe_ready = h.tmp.path().join("probe_ready");
    let child = h
        .cmd()
        .env("LOOP_TEST_PROBE_READY_FILE", &probe_ready)
        .env("LOOP_TEST_PROBE_SLEEP_SECS", "20")
        .args(["loop", "run", "fast-worker-reviewer", "cancel during probe"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !probe_ready.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for probe ready"
        );
        thread::sleep(Duration::from_millis(15));
    }

    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(4),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!h.repo.join(".loops/runs").exists());
    assert!(
        !h.state_dir.join("loops").exists()
            || fs::read_dir(h.state_dir.join("loops"))
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .count()
                == 0
    );
}

#[test]
fn workspace_models_toml_is_ignored_and_engine_refuses_duplicate_lock() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    // Commit a malicious `.loops/models.toml`? No, simulate an agent writing `.loops/models.toml` inside
    // a finished workspace or verifying repo models.toml vs workspace models.toml:
    // Start a slow detached run with `hc` NOT spawning engine so we can test `engine.lock` cleanly!
    let det = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .env("LOOP_TEST_HC_SKIP_SPAWN", "1")
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "lock check",
            "--detach",
        ])
        .output()
        .unwrap();
    assert_eq!(det.status.code(), Some(0));
    let run_id = String::from_utf8_lossy(&det.stdout).trim().to_string();

    // Put a rogue `.loops/models.toml` inside the agent workspace clone: `engine` must ignore it and use frozen.json.
    let ws_dir = h.state_dir.join("loops").join(&run_id).join("workspace");
    fs::create_dir_all(ws_dir.join(".loops")).unwrap();
    fs::write(
        ws_dir.join(".loops/models.toml"),
        "[defaults.worker]\nprovider = \"opencode\"\nmodel = \"rogue-model\"\n",
    )
    .unwrap();

    // Spawn engine #1 sleeping in stage
    let ready = h.tmp.path().join("engine_ready");
    let mut eng1 = h
        .cmd()
        .env("LOOP_TEST_READY_FILE", &ready)
        .env("LOOP_TEST_SLEEP_SECS", "15")
        .args(["loop", "engine", &run_id])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(15));
    }

    // Second concurrent `loop engine <run_id>` must be refused by `engine.lock`
    let eng2 = h.cmd().args(["loop", "engine", &run_id]).output().unwrap();
    assert_eq!(eng2.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&eng2.stderr).contains("execução")
            || String::from_utf8_lossy(&eng2.stderr).contains("trav")
    );

    unsafe {
        libc::kill(eng1.id() as libc::pid_t, libc::SIGINT);
    }
    let _ = eng1.wait();
    let st = load_state(&h.repo.join(".loops/runs").join(&run_id)).unwrap();
    assert!(st
        .executions
        .iter()
        .all(|e| e.model.as_deref() != Some("rogue-model")));
}

#[test]
fn list_runs_auth_login_and_input_file_work_end_to_end() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);

    // `loop auth` before accounts -> reports missing
    let auth_missing = h.cmd().args(["loop", "auth"]).output().unwrap();
    assert_eq!(auth_missing.status.code(), Some(0));
    let auth_txt = String::from_utf8_lossy(&auth_missing.stdout);
    assert!(auth_txt.contains("claude:   ausente"));

    h.write_accounts();
    let auth_ok = h.cmd().args(["loop", "auth"]).output().unwrap();
    assert_eq!(auth_ok.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&auth_ok.stdout).contains("claude:   autenticado"));

    // `loop auth login claude` with fake hc
    let login_out = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .args(["loop", "auth", "login", "claude"])
        .output()
        .unwrap();
    assert_eq!(login_out.status.code(), Some(0));

    // `loop list` shows presets
    let list_wf = h.cmd().args(["loop", "list"]).output().unwrap();
    assert_eq!(list_wf.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&list_wf.stdout).contains("fast-worker-reviewer"));

    // `--input-file` happy path
    let task_file = h.tmp.path().join("task.md");
    fs::write(&task_file, "Implement feature from input file\n").unwrap();
    let run_out = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "--input-file"])
        .arg(&task_file)
        .output()
        .unwrap();
    assert_eq!(
        run_out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_id = String::from_utf8_lossy(&run_out.stdout)
        .lines()
        .nth(1)
        .unwrap()
        .strip_prefix("RUN_ID: ")
        .unwrap()
        .trim()
        .to_string();

    // `loop list --runs` shows recorded run
    let list_runs = h.cmd().args(["loop", "list", "--runs"]).output().unwrap();
    assert_eq!(list_runs.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&list_runs.stdout).contains(&run_id));
}

fn register(h: &LoopHarness, id: &str, dir: &Path, status: &str, pid: Option<u32>) {
    fs::create_dir_all(&h.data_dir).unwrap();
    OrchStore::open(&h.data_dir.join("orch.db"))
        .unwrap()
        .register_loop(&tchelvis::orch::store::LoopRunRecord {
            run_id: id.to_string(),
            dir: dir.display().to_string(),
            workflow: "fast-worker-reviewer".to_string(),
            status: status.to_string(),
            caller_terminal: None,
            caller_created_ms: None,
            engine_pid: pid,
            engine_start_ticks: pid.map(|_| 1),
            orch_run_id: None,
            notify_state: "none".to_string(),
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        })
        .unwrap();
}

#[test]
fn clean_only_removes_registered_runs() {
    let h = LoopHarness::new();
    let victim = h.tmp.path().join("victim");
    fs::create_dir_all(victim.join("Documents")).unwrap();
    fs::write(victim.join("Documents/tese.txt"), "x").unwrap();
    fs::create_dir_all(h.repo.join(".loops")).unwrap();
    std::os::unix::fs::symlink(&victim, h.repo.join(".loops/runs")).unwrap();
    for args in [
        vec!["loop", "clean", "--yes"],
        vec!["loop", "clean", "Documents", "--yes"],
    ] {
        h.cmd().args(&args).output().unwrap();
        assert!(victim.join("Documents/tese.txt").exists(), "{args:?}");
    }
    // A directory the user keeps there, and a run "finished" by a cloned repository.
    fs::remove_file(h.repo.join(".loops/runs")).unwrap();
    for name in ["minhas-notas", "planted"] {
        fs::create_dir_all(h.repo.join(".loops/runs").join(name)).unwrap();
    }
    h.cmd().args(["loop", "clean", "--yes"]).output().unwrap();
    assert!(h.repo.join(".loops/runs/minhas-notas").exists());
    assert!(h.repo.join(".loops/runs/planted").exists());
    // A registered run whose directory became a link.
    let link = h.repo.join(".loops/runs/r1");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    register(&h, "r1", &link, "PASS", None);
    h.cmd()
        .args(["loop", "clean", "r1", "--yes"])
        .output()
        .unwrap();
    h.cmd().args(["loop", "clean", "--yes"]).output().unwrap();
    assert!(victim.join("Documents/tese.txt").exists());
}

#[test]
fn engine_runs_only_a_registered_unfinished_run() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();
    let out = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let id = OrchStore::open(&h.data_dir.join("orch.db"))
        .unwrap()
        .list_loops()
        .unwrap()[0]
        .run_id
        .clone();
    let again = h.cmd().args(["loop", "engine", &id]).output().unwrap();
    assert_eq!(
        again.status.code(),
        Some(1),
        "a finished run must not restart"
    );
    // The same files, planted by a repository under another id.
    let planted = h.repo.join(".loops/runs/planted");
    fs::create_dir_all(&planted).unwrap();
    fs::copy(
        h.repo.join(".loops/runs").join(&id).join("frozen.json"),
        planted.join("frozen.json"),
    )
    .unwrap();
    h.clear_traces();
    let out = h
        .cmd()
        .args(["loop", "engine", "planted"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(!planted.join("state.json").exists());
    assert!(h.read_traces("claude").is_empty());
}

#[test]
fn confirmation_covers_the_repository_choices_whatever_use_overrides() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();
    fs::create_dir_all(h.repo.join(".loops")).unwrap();
    fs::write(
        h.repo.join(".loops/models.toml"),
        "[defaults.worker]\nprovider = \"claude\"\nmodel = \"repo-a\"\n\n[defaults.reviewer]\nprovider = \"agy\"\nmodel = \"repo-b\"\n",
    )
    .unwrap();
    let run = |extra: &[&str]| {
        let mut args = vec!["loop", "run", "fast-worker-reviewer", "x"];
        args.extend_from_slice(extra);
        let out = h.cmd().args(&args).stdin(Stdio::null()).output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let stages: Vec<String> = {
        let out = h
            .cmd()
            .args(["loop", "models", "fast-worker-reviewer"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| Some(l.trim().split_once(" (")?.0.to_string()))
            .collect()
    };
    let (first, second) = (
        format!("{}=claude:mine", stages[0]),
        format!("{}=agy:mine", stages[1]),
    );
    // Nothing left to the repository: nothing to confirm.
    assert_eq!(run(&["--use", &first, "--use", &second]).0, Some(0));
    assert_eq!(run(&[]).0, Some(1));
    assert_eq!(run(&["--use", &first]).0, Some(1));
    let ok = h
        .cmd()
        .args(["loop", "models", "confirm", "fast-worker-reviewer"])
        .output()
        .unwrap();
    assert_eq!(ok.status.code(), Some(0));
    assert_eq!(run(&["--use", &first]), (Some(0), String::new()));
    assert_eq!(run(&["--use", &second]).0, Some(0));
    assert_eq!(run(&[]).0, Some(0));
}

#[test]
fn foreground_run_inside_herdr_wakes_nobody() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();
    let out = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .args(["loop", "run", "fast-worker-reviewer", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let rec = &OrchStore::open(&h.data_dir.join("orch.db"))
        .unwrap()
        .list_loops()
        .unwrap()[0];
    assert_eq!(rec.notify_state, "none");
    assert_eq!(rec.orch_run_id, None);
}

#[test]
fn terminal_output_never_carries_escape_sequences() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();
    h.cmd().args(["loop", "init"]).output().unwrap();
    let wf = h.repo.join(".loops/workflows/fast-worker-reviewer.toml");
    let raw = fs::read_to_string(&wf).unwrap();
    let start = raw.find("description").unwrap();
    let end = start + raw[start..].find('\n').unwrap();
    fs::write(
        &wf,
        format!(
            "{}description = \"a\\u001b]0;titulo\\u0007b\"{}",
            &raw[..start],
            &raw[end..]
        ),
    )
    .unwrap();
    h.git(&["add", "-A"]);
    h.git(&["commit", "-m", "loops"]);
    let run = h
        .cmd()
        .env("LOOP_TEST_ESCAPE_ANSI", "1")
        .args(["loop", "run", "fast-worker-reviewer", "x"])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(0), "{run:?}");
    let runs = h.repo.join(".loops/runs");
    let log_dir = fs::read_dir(&runs)
        .unwrap()
        .flatten()
        .map(|e| e.path().join("logs"))
        .find(|p| p.is_dir())
        .unwrap();
    let log = fs::read_dir(&log_dir).unwrap().flatten().next().unwrap();
    let mut text = fs::read(log.path()).unwrap();
    text.extend_from_slice(b"\x1b[2J\x1b]0;pwn\x07\x08\xff fim\n");
    fs::write(log.path(), text).unwrap();
    for args in [
        &["loop", "list"][..],
        &["loop", "logs"],
        &["loop", "report"],
        &["loop", "status"],
        &["loop", "result"],
    ] {
        let out = h.cmd().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        let bad = |b: &u8| *b < 0x20 && *b != b'\n' && *b != b'\t';
        assert!(!out.stdout.iter().any(bad), "{args:?}: {:?}", out.stdout);
        assert!(!out.stderr.iter().any(bad), "{args:?}");
    }
}

#[test]
fn auth_login_opens_the_official_command_and_nothing_else() {
    let h = LoopHarness::new();
    for (cli, expected) in [
        ("claude", &["claude", "auth", "login"][..]),
        ("agy", &["agy"]),
        ("opencode", &["opencode", "auth", "login"]),
    ] {
        let out = h
            .cmd()
            .env("HC_TERMINAL_ID", "t1")
            .env("LOOP_TEST_HC_SKIP_SPAWN", "1")
            .args(["loop", "auth", "login", cli])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let argv: Vec<String> = serde_json::from_str(
            &fs::read_to_string(h.trace_dir.join("hc_run_argv.json")).unwrap(),
        )
        .unwrap();
        let start = argv.iter().position(|a| a == "--json").unwrap() + 1;
        assert_eq!(&argv[start..], expected);
    }
}

#[test]
fn result_of_a_run_that_never_started_still_has_five_lines() {
    let h = LoopHarness::new();
    let dir = h.repo.join(".loops/runs/r1");
    fs::create_dir_all(&dir).unwrap();
    register(&h, "r1", &dir, "ERROR", None);
    let out = h.cmd().args(["loop", "result", "r1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(text.lines().count(), 5, "{text}");
    assert!(text.starts_with("STATUS: ERROR | LOOP: fast-worker-reviewer"));
}

#[test]
fn stop_never_signals_a_process_that_is_not_the_registered_engine() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();
    let out = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let store = OrchStore::open(&h.data_dir.join("orch.db")).unwrap();
    let done = PathBuf::from(&store.list_loops().unwrap()[0].dir);
    let mut state = load_state(&done).unwrap();
    let mut other = Command::new("sleep").arg("30").spawn().unwrap();
    let dir = h.repo.join(".loops/runs/r1");
    fs::create_dir_all(&dir).unwrap();
    state.run_id = "r1".to_string();
    state.status = tchelvis::loops::state::RunStatus::Running;
    state.engine_pid = Some(other.id());
    state.engine_start_ticks = None;
    tchelvis::loops::state::save_state(&dir, &state).unwrap();
    register(&h, "r1", &dir, "RUNNING", None);
    let stop = h.cmd().args(["loop", "stop", "r1"]).output().unwrap();
    thread::sleep(Duration::from_millis(200));
    let alive = other.try_wait().unwrap().is_none();
    let _ = other.kill();
    let _ = other.wait();
    assert!(alive, "stop signalled another process");
    assert_eq!(stop.status.code(), Some(1));
}

#[test]
fn a_dead_engine_neither_blocks_new_loops_nor_clean() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(1, true);
    h.write_accounts();
    let dir = h.repo.join(".loops/runs/r1");
    fs::create_dir_all(&dir).unwrap();
    register(&h, "r1", &dir, "RUNNING", Some(4_000_000));
    let out = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "x"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let clean = h
        .cmd()
        .args(["loop", "clean", "r1", "--yes"])
        .output()
        .unwrap();
    assert_eq!(clean.status.code(), Some(0), "{clean:?}");
    assert!(!dir.exists());
}

#[test]
fn detached_engine_is_the_same_binary_by_absolute_path() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();
    let out = h
        .cmd()
        .env("HC_TERMINAL_ID", "t1")
        .env("LOOP_TEST_HC_SKIP_SPAWN", "1")
        .args(["loop", "run", "fast-worker-reviewer", "x", "--detach"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let argv: Vec<String> =
        serde_json::from_str(&fs::read_to_string(h.trace_dir.join("hc_run_argv.json")).unwrap())
            .unwrap();
    let start = argv.iter().position(|a| a == "--json").unwrap() + 1;
    assert!(Path::new(&argv[start]).is_absolute(), "{argv:?}");
    assert_eq!(&argv[start + 1..start + 3], ["loop", "engine"]);
}

fn count_probe_traces(h: &LoopHarness, cli: &str) -> usize {
    h.read_traces(cli)
        .into_iter()
        .filter(|tr| tr["prompt"].as_str() == Some("Reply with exactly: OK"))
        .count()
}

fn extract_run_id(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("RUN_ID: "))
        .unwrap_or_else(|| panic!("missing RUN_ID line in stdout:\n{text}"))
        .trim()
        .to_string()
}

#[test]
fn preflight_auto_caches_evidence_discards_only_failed_stage_and_keeps_on_verdict_or_missing_out() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    let ev_path = h.state_dir.join("loops/model_evidence.json");
    assert!(!ev_path.exists());

    // 1. First run in default `auto` mode probes once per distinct model and records salted evidence (`0600`).
    h.clear_traces();
    let run1 = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "first auto run"])
        .output()
        .unwrap();
    assert_eq!(
        run1.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&run1.stderr)
    );
    assert_eq!(count_probe_traces(&h, "claude"), 1);
    assert_eq!(count_probe_traces(&h, "agy"), 1);

    let mode = fs::metadata(&ev_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let ev_raw = fs::read_to_string(&ev_path).unwrap();
    assert!(!ev_raw.contains("fake-user@example.com"));
    assert!(!ev_raw.contains("org-secret-999"));
    assert!(!ev_raw.contains("SecretFakeOrg"));

    let run1_id = extract_run_id(&run1.stdout);
    let report1 =
        fs::read_to_string(h.repo.join(".loops/runs").join(&run1_id).join("report.md")).unwrap();
    assert!(report1.contains("| claude | sonnet | probe |"));
    assert!(report1.contains("| agy | gemini-test | probe |"));

    // 2. Second run in `auto` mode reuses cached evidence -> ZERO probes.
    h.clear_traces();
    let run2 = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "second auto run"])
        .output()
        .unwrap();
    assert_eq!(
        run2.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&run2.stderr)
    );
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);
    let run2_id = extract_run_id(&run2.stdout);
    let report2 =
        fs::read_to_string(h.repo.join(".loops/runs").join(&run2_id).join("report.md")).unwrap();
    assert!(report2.contains("| claude | sonnet | evidência |"));
    assert!(report2.contains("| agy | gemini-test | evidência |"));

    // 3. Stage failure on `agy` discards ONLY `agy:gemini-test` evidence, keeping `claude:sonnet`.
    h.clear_traces();
    let fail_agy = h
        .cmd()
        .env("LOOP_TEST_AGY_FAIL", "1")
        .args(["loop", "run", "fast-worker-reviewer", "agy stage fails"])
        .output()
        .unwrap();
    assert_eq!(
        fail_agy.status.code(),
        Some(1),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&fail_agy.stdout),
        String::from_utf8_lossy(&fail_agy.stderr)
    );
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);

    // Next run re-probes ONLY `agy`, still reuses `claude` evidence!
    h.clear_traces();
    let recover = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "after agy fail"])
        .output()
        .unwrap();
    assert_eq!(
        recover.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&recover.stderr)
    );
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 1);
    let recover_id = extract_run_id(&recover.stdout);
    let report_recover = fs::read_to_string(
        h.repo
            .join(".loops/runs")
            .join(&recover_id)
            .join("report.md"),
    )
    .unwrap();
    assert!(report_recover.contains("| claude | sonnet | evidência |"));
    assert!(report_recover.contains("| agy | gemini-test | probe |"));

    // 4. Malformed verdict or missing artifact does NOT discard model evidence.
    for env_flag in [
        "LOOP_TEST_AGY_MALFORMED_VERDICT",
        "LOOP_TEST_AGY_MISSING_OUT",
    ] {
        h.clear_traces();
        let bad_out = h
            .cmd()
            .env(env_flag, "1")
            .args(["loop", "run", "fast-worker-reviewer", "bad reviewer output"])
            .output()
            .unwrap();
        assert_eq!(
            bad_out.status.code(),
            Some(1),
            "{env_flag} stdout={}\nstderr={}",
            String::from_utf8_lossy(&bad_out.stdout),
            String::from_utf8_lossy(&bad_out.stderr)
        );

        h.clear_traces();
        let next_ok = h
            .cmd()
            .args([
                "loop",
                "run",
                "fast-worker-reviewer",
                "after non-stage error",
            ])
            .output()
            .unwrap();
        assert_eq!(
            next_ok.status.code(),
            Some(0),
            "{env_flag} stderr={}",
            String::from_utf8_lossy(&next_ok.stderr)
        );
        assert_eq!(count_probe_traces(&h, "claude"), 0);
        assert_eq!(
            count_probe_traces(&h, "agy"),
            0,
            "{env_flag} must NOT discard agy evidence"
        );
    }
}

#[test]
fn preflight_free_and_always_modes_work_end_to_end() {
    let h = LoopHarness::new();
    h.write_accounts();

    // `mode = "free"` with listed models (`claude:sonnet`, `agy:gemini-4-hydrogen`, `opencode:opencode/beam-free`)
    // runs ZERO probes and records `gratuito` in report.md.
    h.write_config(
        2,
        true,
        "inherited",
        "[preflight]\nmode = \"free\"\n\n\
         [defaults.worker]\nprovider = \"opencode\"\nmodel = \"opencode/beam-free\"\n\
         [defaults.reviewer]\nprovider = \"agy\"\nmodel = \"gemini-4-hydrogen\"\n\
         [defaults.critic]\nprovider = \"claude\"\nmodel = \"sonnet\"\n",
    );

    h.clear_traces();
    let free_ok = h
        .cmd()
        .args(["loop", "run", "worker-reviewer-critic", "free mode run"])
        .output()
        .unwrap();
    assert_eq!(
        free_ok.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&free_ok.stderr)
    );
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);
    assert_eq!(count_probe_traces(&h, "opencode"), 0);
    let free_id = extract_run_id(&free_ok.stdout);
    let free_report =
        fs::read_to_string(h.repo.join(".loops/runs").join(&free_id).join("report.md")).unwrap();
    assert!(free_report.contains("| opencode | opencode/beam-free | gratuito |"));
    assert!(free_report.contains("| agy | gemini-4-hydrogen | gratuito |"));
    assert!(free_report.contains("| claude | sonnet | gratuito |"));

    // `mode = "free"` refuses `agy` partial stem (`gemini-4-hydro`) and unlisted `opencode/free-bad` with 0 probes.
    h.clear_traces();
    let partial_stem = h
        .cmd()
        .args([
            "loop",
            "run",
            "worker-reviewer-critic",
            "bad models",
            "--use",
            "review=agy:gemini-4-hydro",
            "--use",
            "implement=opencode:opencode/free-bad",
        ])
        .output()
        .unwrap();
    assert_eq!(partial_stem.status.code(), Some(1));
    let err_txt = String::from_utf8_lossy(&partial_stem.stderr);
    assert!(err_txt.contains("agy:gemini-4-hydro"), "{err_txt}");
    assert!(err_txt.contains("opencode:opencode/free-bad"), "{err_txt}");
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);
    assert_eq!(count_probe_traces(&h, "opencode"), 0);

    // `mode = "free"` refuses logged-out `claude` with 0 probes.
    let claude_cred = h.home.join(".claude/.credentials.json");
    fs::remove_file(&claude_cred).unwrap();
    let logged_out = h
        .cmd()
        .args(["loop", "run", "worker-reviewer-critic", "logged out claude"])
        .output()
        .unwrap();
    assert_eq!(logged_out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&logged_out.stderr).contains("desconectada"));
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);
    assert_eq!(count_probe_traces(&h, "opencode"), 0);
    h.write_accounts();

    // `mode = "always"` ignores prior valid evidence and re-probes on every run.
    h.write_config(
        2,
        true,
        "inherited",
        "[preflight]\nmode = \"auto\"\n\n\
         [defaults.worker]\nprovider = \"claude\"\nmodel = \"sonnet\"\n\
         [defaults.reviewer]\nprovider = \"agy\"\nmodel = \"gemini-test\"\n",
    );
    let seed = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "seed evidence"])
        .output()
        .unwrap();
    assert_eq!(seed.status.code(), Some(0));

    h.write_config(
        2,
        true,
        "inherited",
        "[preflight]\nmode = \"always\"\n\n\
         [defaults.worker]\nprovider = \"claude\"\nmodel = \"sonnet\"\n\
         [defaults.reviewer]\nprovider = \"agy\"\nmodel = \"gemini-test\"\n",
    );
    h.clear_traces();
    let always_run = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "always probes"])
        .output()
        .unwrap();
    assert_eq!(always_run.status.code(), Some(0));
    assert_eq!(count_probe_traces(&h, "claude"), 1);
    assert_eq!(count_probe_traces(&h, "agy"), 1);
}

#[test]
fn loop_check_live_and_auth_never_leak_secrets() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    // `loop auth` uses free checks and never leaks email/org/key.
    let secret_oc = "sk-opencode-auth-secret-4242";
    let auth_out = h
        .cmd()
        .env("OPENCODE_API_KEY", secret_oc)
        .args(["loop", "auth"])
        .output()
        .unwrap();
    assert_eq!(auth_out.status.code(), Some(0));
    let auth_stdout = String::from_utf8_lossy(&auth_out.stdout);
    let auth_stderr = String::from_utf8_lossy(&auth_out.stderr);
    assert!(auth_stdout.contains("claude:   autenticado (claude auth status)"));
    assert!(auth_stdout.contains("modelo(s) listado(s) por 'agy models'"));
    assert!(auth_stdout.contains("opencode: chave presente"));
    assert!(auth_stdout.contains("modelo(s) listado(s) por 'opencode models'"));
    for secret in [
        "fake-user@example.com",
        "org-secret-999",
        "SecretFakeOrg",
        secret_oc,
    ] {
        assert!(!auth_stdout.contains(secret), "stdout leaked {secret}");
        assert!(!auth_stderr.contains(secret), "stderr leaked {secret}");
    }

    // `loop check` prints planned preflight actions without running probes.
    h.clear_traces();
    let chk1 = h
        .cmd()
        .args(["loop", "check", "fast-worker-reviewer"])
        .output()
        .unwrap();
    assert_eq!(chk1.status.code(), Some(0));
    let chk1_txt = String::from_utf8_lossy(&chk1.stdout);
    assert!(chk1_txt.contains("Pré-teste (modo: auto, validade: 12h):"));
    assert!(chk1_txt.contains("chamada real (sem evidência válida)"));
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);

    // `loop check --live` executes probes and records evidence for subsequent runs.
    let chk_live = h
        .cmd()
        .args(["loop", "check", "fast-worker-reviewer", "--live"])
        .output()
        .unwrap();
    assert_eq!(chk_live.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&chk_live.stdout).contains("Pré-teste ao vivo concluído"));
    assert_eq!(count_probe_traces(&h, "claude"), 1);
    assert_eq!(count_probe_traces(&h, "agy"), 1);

    // Subsequent `loop check` and `loop run` in `auto` mode see valid evidence and run 0 probes.
    h.clear_traces();
    let chk2 = h
        .cmd()
        .args(["loop", "check", "fast-worker-reviewer"])
        .output()
        .unwrap();
    assert_eq!(chk2.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&chk2.stdout).contains("evidência recente válida"));
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);

    let run_after_live = h
        .cmd()
        .args(["loop", "run", "fast-worker-reviewer", "reuses live check"])
        .output()
        .unwrap();
    assert_eq!(run_after_live.status.code(), Some(0));
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 0);
}

#[test]
fn bwrap_sandbox_blocks_stage_from_reading_model_evidence() {
    let bwrap_works = Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !bwrap_works {
        eprintln!("SKIPPED: bwrap unavailable or unprivileged userns disabled");
        return;
    }

    let h = LoopHarness::new();
    h.write_accounts();
    // Place fake CLIs inside `$HOME/fakebin` so BwrapSandbox::adapter_binds mounts them even with `--tmpfs /tmp`.
    let home_bin = h.home.join("fakebin");
    fs::create_dir_all(&home_bin).unwrap();
    for cli in ["claude", "agy", "opencode"] {
        fs::copy(h.bin_dir.join(cli), home_bin.join(cli)).unwrap();
        fs::set_permissions(home_bin.join(cli), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path_with_home_bin = format!("{}:{}", home_bin.display(), h.path_with_fake_bins());

    h.write_config(
        2,
        true,
        "bwrap",
        "[defaults.worker]\nprovider = \"claude\"\nmodel = \"sonnet\"\n\
         [defaults.reviewer]\nprovider = \"agy\"\nmodel = \"gemini-test\"\n",
    );

    let ev_path = h.state_dir.join("loops/model_evidence.json");
    let run = h
        .cmd()
        .env("PATH", &path_with_home_bin)
        .env("LOOP_TEST_CHECK_EVIDENCE_PATH", &ev_path)
        .env("LOOP_TEST_CHECK_EVIDENCE_OUT", "ev_check.txt")
        .args(["loop", "run", "fast-worker-reviewer", "bwrap isolation"])
        .output()
        .unwrap();
    assert_eq!(
        run.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(ev_path.is_file());

    let run_id = extract_run_id(&run.stdout);
    let ws_check = h
        .state_dir
        .join("loops")
        .join(&run_id)
        .join("workspace/ev_check.txt");
    assert_eq!(fs::read_to_string(&ws_check).unwrap(), "BLOCKED");
}

#[test]
fn a_failed_probe_discards_the_evidence_of_its_own_pair() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();

    let seed = h
        .cmd()
        .args(["loop", "check", "fast-worker-reviewer", "--live"])
        .output()
        .unwrap();
    assert_eq!(seed.status.code(), Some(0));

    let broken = h
        .cmd()
        .env("LOOP_TEST_AGY_FAIL_PROBE", "1")
        .args(["loop", "check", "fast-worker-reviewer", "--live"])
        .output()
        .unwrap();
    assert_eq!(broken.status.code(), Some(1));

    h.clear_traces();
    let run = h
        .cmd()
        .args([
            "loop",
            "run",
            "fast-worker-reviewer",
            "after a failed probe",
        ])
        .output()
        .unwrap();
    assert_eq!(
        run.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(count_probe_traces(&h, "claude"), 0);
    assert_eq!(count_probe_traces(&h, "agy"), 1);
}

#[test]
fn claude_evidence_is_kept_without_the_profile_hidden_by_the_sandbox() {
    let h = LoopHarness::new();
    h.write_default_inherited_config(2, true);
    h.write_accounts();
    let run = |args: &[&str]| {
        h.cmd()
            .env("LOOP_TEST_CLAUDE_NO_PROFILE", "1")
            .args(args)
            .output()
            .unwrap()
    };
    let live = run(&["loop", "check", "fast-worker-reviewer", "--live"]);
    assert_eq!(live.status.code(), Some(0));
    h.clear_traces();
    let out = run(&["loop", "run", "fast-worker-reviewer", "x"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(count_probe_traces(&h, "claude"), 0);
}
