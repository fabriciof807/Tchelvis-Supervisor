use sha2::{Digest, Sha256};

pub const HERDR_MOD_MAIN_JS: &str = include_str!("../assets/herdr-app/mods/tchelvis/main.js");
pub const HERDR_MOD_JSON: &str = include_str!("../assets/herdr-app/mods/tchelvis/mod.json");
pub const HERDR_LAYOUT_JSON: &str = include_str!("../assets/herdr-app/layouts/tchelvis.json");
pub const TO_TICKETS_SKILL_NAME: &str = "to-tickets";
pub const TO_TICKETS_SKILL: &str = include_str!("../assets/skills/to-tickets/SKILL.md");
pub const HERDR_MOD_ICONS: &[(&str, &str)] = &[
    (
        "LICENSE-lucide.txt",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/LICENSE-lucide.txt"),
    ),
    (
        "brain.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/brain.svg"),
    ),
    (
        "clipboard-list.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/clipboard-list.svg"),
    ),
    (
        "fish.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/fish.svg"),
    ),
    (
        "folder.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/folder.svg"),
    ),
    (
        "house.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/house.svg"),
    ),
    (
        "key-round.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/key-round.svg"),
    ),
    (
        "message-square.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/message-square.svg"),
    ),
    (
        "monitor.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/monitor.svg"),
    ),
    (
        "rocket.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/rocket.svg"),
    ),
    (
        "search.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/search.svg"),
    ),
    (
        "settings.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/settings.svg"),
    ),
    (
        "sparkles.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/sparkles.svg"),
    ),
    (
        "stethoscope.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/stethoscope.svg"),
    ),
    (
        "timer.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/timer.svg"),
    ),
    (
        "triangle-alert.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/triangle-alert.svg"),
    ),
    (
        "zap.svg",
        include_str!("../assets/herdr-app/mods/tchelvis/icons/zap.svg"),
    ),
];

pub const FENCE_START: &str = "# >>> tchelvis managed >>>";
pub const FENCE_END: &str = "# <<< tchelvis managed <<<";
pub const CLAUDE_HOOK_MARKER: &str = "# tchelvis-managed";

/// POSIX / bash / zsh opt-in helper script (`~/.config/tchelvis/hooks/shell.sh`).
/// Never emits OSC 7501 or `cockpit.*` labels automatically on plain shell prompts.
pub const SHELL_SH: &str = r#"# tchelvis shell helper (opt-in only; preserves plain-shell filter)
tchelvis-tag() {
    PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"
    if [ -z "${HC_TERMINAL_ID:-}" ]; then
        printf "tchelvis-tag: not running inside a Herdr (hc) terminal\n" >&2
        return 1
    fi
    if command -v tchelvis >/dev/null 2>&1; then
        tchelvis label "$HC_TERMINAL_ID" "$@"
    elif command -v hc >/dev/null 2>&1; then
        hc label "$HC_TERMINAL_ID" "$@"
    fi
}

if [ -n "${ZSH_VERSION:-}" ]; then
    _tchelvis_extract_agent() {
        local cmd="$1"
        local word=""
        local base=""
        for word in ${(z)cmd}; do
            case "$word" in
                *=*) continue ;;
                sudo|env|npx|bunx|pnpx|command|nocorrect|noglob) continue ;;
                -*) continue ;;
                *)
                    base="${word##*/}"
                    case "$base" in
                        claude|claude-code) echo "claude"; return 0 ;;
                        pi|codex|opencode|aider|goose|amp) echo "$base"; return 0 ;;
                        *) return 1 ;;
                    esac
                    ;;
            esac
        done
        return 1
    }

    _tchelvis_preexec() {
        [ -n "${HC_TERMINAL_ID:-}" ] || return 0
        local agent_name
        agent_name="$(_tchelvis_extract_agent "$1")" || return 0
        _TCHELVIS_ACTIVE_AGENT="$agent_name"
        (
            PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"
            if command -v hc >/dev/null 2>&1; then
                hc label "$HC_TERMINAL_ID" "cockpit.agent=${agent_name}" "cockpit.state=working" >/dev/null 2>&1
            fi
        ) &!
        if [ -w /dev/tty ]; then
            printf '\033]7501;state=working:app=%s\033\\' "$agent_name" > /dev/tty 2>/dev/null || true
        fi
    }

    _tchelvis_precmd() {
        [ -n "${HC_TERMINAL_ID:-}" ] || return 0
        [ -n "${_TCHELVIS_ACTIVE_AGENT:-}" ] || return 0
        local prev_agent="$_TCHELVIS_ACTIVE_AGENT"
        unset _TCHELVIS_ACTIVE_AGENT
        (
            PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"
            if command -v hc >/dev/null 2>&1; then
                hc label "$HC_TERMINAL_ID" "cockpit.agent=" "cockpit.state=" "cockpit.kind=" "cockpit.msg=" >/dev/null 2>&1
            fi
        ) &!
        if [ -w /dev/tty ]; then
            printf '\033]7501;state=idle:app=%s\033\\' "$prev_agent" > /dev/tty 2>/dev/null || true
        fi
    }

    autoload -Uz add-zsh-hook
    add-zsh-hook preexec _tchelvis_preexec
    add-zsh-hook precmd _tchelvis_precmd
fi
"#;

/// Fish shell opt-in helper (`~/.config/tchelvis/hooks/shell.fish`).
pub const SHELL_FISH: &str = r#"# tchelvis fish helper (opt-in only; preserves plain-shell filter)
function tchelvis-tag
    if not set -q HC_TERMINAL_ID
        echo "tchelvis-tag: not running inside a Herdr (hc) terminal" >&2
        return 1
    end
    if command -sq tchelvis
        tchelvis label "$HC_TERMINAL_ID" $argv
    else if command -sq hc
        hc label "$HC_TERMINAL_ID" $argv
    end
end
"#;

/// Nushell opt-in helper (`~/.config/tchelvis/hooks/shell.nu`).
pub const SHELL_NU: &str = r#"# tchelvis nushell helper (opt-in only; preserves plain-shell filter)
def --wrapped tchelvis-tag [...rest: string] {
    if ($env.HC_TERMINAL_ID? | is-empty) {
        print -e "tchelvis-tag: not running inside a Herdr (hc) terminal"
        return
    }
    if (which tchelvis | is-not-empty) {
        ^tchelvis label $env.HC_TERMINAL_ID ...$rest
    } else if (which hc | is-not-empty) {
        ^hc label $env.HC_TERMINAL_ID ...$rest
    }
}
"#;

/// Claude Code hook script (`~/.config/tchelvis/hooks/claude-hook.sh`, <20ms p99).
/// Emits Program Status Protocol OSC 7501 (`\x1b]7501;state=<state>:app=claude[:kind=<kind>][:msg=<base64>]\x1b\\`)
/// and stamps `cockpit.agent=claude` asynchronously in the background.
pub const CLAUDE_HOOK_SH: &str = r#"#!/bin/sh
# tchelvis Claude Code hook (<20ms p99, non-blocking)
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"
input="$(cat 2>/dev/null || true)"

emit_osc7501() {
    payload="$1"
    if [ -w /dev/tty ]; then
        printf '\033]7501;%s\033\\' "$payload" > /dev/tty 2>/dev/null || true
    elif [ -n "${PPID:-}" ] && [ -w "/proc/$PPID/fd/1" ]; then
        printf '\033]7501;%s\033\\' "$payload" > "/proc/$PPID/fd/1" 2>/dev/null || true
    elif [ -n "${PPID:-}" ] && [ -w "/proc/$PPID/fd/2" ]; then
        printf '\033]7501;%s\033\\' "$payload" > "/proc/$PPID/fd/2" 2>/dev/null || true
    fi
}

stamp_hc_state() {
    st="$1"
    kind="$2"
    msg="$3"
    if [ -n "${HC_TERMINAL_ID:-}" ] && command -v hc >/dev/null 2>&1; then
        (hc label "$HC_TERMINAL_ID" "cockpit.agent=claude" "cockpit.state=$st" "cockpit.kind=$kind" "cockpit.msg=$msg" >/dev/null 2>&1 &)
    fi
}

case "$input" in
    *'"PermissionRequest"'*|*'"permission_prompt"'*)
        msg_b64="$(printf 'Permission required' | base64 | tr -d '\n')"
        emit_osc7501 "state=blocked:app=claude:kind=permission:msg=${msg_b64}"
        stamp_hc_state "blocked" "permission" "Permission required"
        ;;
    *'"Notification"'*)
        case "$input" in
            *auth*|*login*|*token*)
                msg_b64="$(printf 'Authentication required' | base64 | tr -d '\n')"
                emit_osc7501 "state=blocked:app=claude:kind=auth:msg=${msg_b64}"
                stamp_hc_state "blocked" "auth" "Authentication required"
                ;;
            *permission*|*approve*)
                msg_b64="$(printf 'Permission required' | base64 | tr -d '\n')"
                emit_osc7501 "state=blocked:app=claude:kind=permission:msg=${msg_b64}"
                stamp_hc_state "blocked" "permission" "Permission required"
                ;;
            *)
                msg_b64="$(printf 'Waiting for user response' | base64 | tr -d '\n')"
                emit_osc7501 "state=blocked:app=claude:kind=question:msg=${msg_b64}"
                stamp_hc_state "blocked" "question" "Waiting for user response"
                ;;
        esac
        ;;
    *'"UserPromptSubmit"'*|*'"PreToolUse"'*|*'"PostToolUse"'*)
        emit_osc7501 "state=working:app=claude"
        stamp_hc_state "working" "" ""
        ;;
    *'"Stop"'*)
        emit_osc7501 "state=done:app=claude"
        stamp_hc_state "done" "" ""
        ;;
    *)
        if [ -n "${HC_TERMINAL_ID:-}" ] && command -v hc >/dev/null 2>&1; then
            (hc label "$HC_TERMINAL_ID" "cockpit.agent=claude" >/dev/null 2>&1 &)
        fi
        ;;
esac
exit 0
"#;

/// OpenCode ESM plugin (`~/.config/opencode/plugins/tchelvis.js`).
pub const OPENCODE_TCHELVIS_JS: &str = r#"// tchelvis-osc7501 OpenCode plugin (<20ms p99)
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

function hcEnv() {
  const home = process.env.HOME || "";
  const extra = `${home}/.local/bin:${home}/.cargo/bin:/usr/local/bin`;
  return {
    ...process.env,
    PATH: process.env.PATH ? `${extra}:${process.env.PATH}` : extra,
  };
}

function emitOsc7501(state, kind = null, message = "") {
  try {
    const parts = [`state=${state}`, "app=opencode"];
    if (state === "blocked" && kind) parts.push(`kind=${kind}`);
    if (message) {
      const b64 = Buffer.from(String(message).slice(0, 512), "utf8").toString("base64");
      parts.push(`msg=${b64}`);
    }
    writeFileSync("/dev/tty", `\x1b]7501;${parts.join(":")}\x1b\\`);
  } catch (_e) {}
}

function stampHcLabels(extraLabels = []) {
  const termId = process.env.HC_TERMINAL_ID;
  if (!termId) return;
  try {
    const child = spawn("hc", ["label", termId, "cockpit.agent=opencode", ...extraLabels], {
      detached: true,
      stdio: "ignore",
      env: hcEnv(),
    });
    child.unref();
  } catch (_e) {}
}

function reportState(state, kind = null, message = "") {
  emitOsc7501(state, kind, message);
  const labels = [`cockpit.state=${state}`];
  if (kind) labels.push(`cockpit.kind=${kind}`);
  if (message) labels.push(`cockpit.msg=${String(message).slice(0, 200)}`);
  stampHcLabels(labels);
}

export const TchelvisPlugin = async () => {
  stampHcLabels();
  return {
    event: async ({ event }) => {
      const t = event?.type ?? "";
      if (t.includes("permission")) {
        reportState("blocked", "permission", "Waiting for permission");
      } else if (t.includes("question") || t.includes("prompt")) {
        reportState("blocked", "question", "Waiting for answer");
      } else if (t.includes("busy") || t.includes("start")) {
        reportState("working");
      } else if (t.includes("idle") || t.includes("complete")) {
        reportState("done");
      }
    },
  };
};
export default TchelvisPlugin;
"#;

/// Pi coding-agent extension (`~/.pi/agent/extensions/tchelvis.js`).
/// Note: Pi 1.5.2+ natively emits OSC 7501 via `@earendil-works/pi-tui/dist/program-status.js`.
/// This extension stamps `cockpit.agent=pi` and `cockpit.state` labels via `hc` and emits OSC 7501 fallback.
pub const PI_TCHELVIS_JS: &str = r#"// tchelvis-osc7501 Pi extension
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

function hcEnv() {
  const home = process.env.HOME || "";
  const extra = `${home}/.local/bin:${home}/.cargo/bin:/usr/local/bin`;
  return {
    ...process.env,
    PATH: process.env.PATH ? `${extra}:${process.env.PATH}` : extra,
  };
}

function setHcState(state, kind = "", msg = "") {
  const termId = process.env.HC_TERMINAL_ID;
  if (termId) {
    try {
      const args = ["label", termId, "cockpit.agent=pi"];
      if (state) args.push(`cockpit.state=${state}`);
      if (kind) args.push(`cockpit.kind=${kind}`);
      if (msg) args.push(`cockpit.msg=${String(msg).slice(0, 200)}`);
      const child = spawn("hc", args, {
        detached: true,
        stdio: "ignore",
        env: hcEnv(),
      });
      child.unref();
    } catch (_e) {}
  }
  if (state) {
    try {
      const parts = [`state=${state}`, "app=pi"];
      if (state === "blocked" && kind) parts.push(`kind=${kind}`);
      if (msg) {
        const b64 = Buffer.from(String(msg).slice(0, 512), "utf8").toString("base64");
        parts.push(`msg=${b64}`);
      }
      writeFileSync("/dev/tty", `\x1b]7501;${parts.join(":")}\x1b\\`);
    } catch (_e) {}
  }
}

export default function tchelvisPiExtension(pi) {
  setHcState("");
  if (typeof pi?.on === "function") {
    pi.on("session_start", () => setHcState(""));
    pi.on("agent_start", () => setHcState("working"));
    pi.on("turn_start", () => setHcState("working"));
    pi.on("tool_call", () => setHcState("working"));
    pi.on("agent_end", () => setHcState("done"));
    pi.on("turn_end", () => setHcState("done"));
  }
}
"#;

pub const SYSTEMD_UNIT_TEMPLATE: &str = r#"[Unit]
Description=Tchelvis Herdr Coding Agent Context Daemon
After=default.target

[Service]
Type=simple
ExecStart=%h/.local/bin/tchelvis daemon
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#;

pub fn bytes_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn asset_sha256(content: &str) -> String {
    bytes_sha256(content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_layout_json_is_valid_and_matches_schema() {
        let val: serde_json::Value = serde_json::from_str(HERDR_LAYOUT_JSON).unwrap();
        assert_eq!(val.get("id").and_then(|v| v.as_str()), Some("root"));
        assert!(matches!(
            val.get("type").and_then(|v| v.as_str()),
            Some("row" | "column")
        ));
        let children = val.get("children").and_then(|c| c.as_array()).unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(
            children[0].get("id").and_then(|v| v.as_str()),
            Some("tchelvis-pill")
        );

        let mod_json: serde_json::Value = serde_json::from_str(HERDR_MOD_JSON).unwrap();
        assert_eq!(mod_json.get("place").and_then(|v| v.as_str()), Some("none"));
    }

    #[test]
    fn test_embedded_mod_js_exports_contract_symbols() {
        assert!(HERDR_MOD_MAIN_JS.contains("export default class CockpitMod extends View"));
        assert!(HERDR_MOD_MAIN_JS.contains("export const id = \"tchelvis\""));
        assert!(HERDR_MOD_MAIN_JS.contains("export const schemaVersion = 1"));
        assert!(HERDR_MOD_MAIN_JS.contains("export function partitionAgents"));
        assert!(HERDR_MOD_MAIN_JS.contains("export function buildViewModel"));
        assert!(HERDR_MOD_MAIN_JS.contains("export function mount"));
    }

    #[test]
    fn embedded_icons_match_the_icons_folder() {
        use std::collections::BTreeSet;
        let icons_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/herdr-app/mods/tchelvis/icons");
        let on_disk: BTreeSet<String> = std::fs::read_dir(&icons_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let embedded: BTreeSet<String> = HERDR_MOD_ICONS
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();
        assert_eq!(HERDR_MOD_ICONS.len(), 17);
        assert_eq!(embedded, on_disk);
    }

    #[test]
    fn to_tickets_skill_states_its_contract() {
        assert_eq!(TO_TICKETS_SKILL_NAME, "to-tickets");
        assert!(
            TO_TICKETS_SKILL.starts_with("---\nname: to-tickets\ndescription: "),
            "missing required frontmatter prefix"
        );
        for needle in [
            "status: ready",
            "blocked_by:",
            "files:",
            "workflow:",
            "run_id:",
            ".loops/tickets/<feature>/",
            ".tickets/<feature>/",
            "integration",
            "open decision",
            "demo",
            "lockfile",
            "adds the ignore rules",
            "union of every other ticket's `files`",
            "expand",
            "contract",
            "approve",
        ] {
            assert!(
                TO_TICKETS_SKILL.contains(needle),
                "TO_TICKETS_SKILL missing required substring: {needle}"
            );
        }
        let lower = TO_TICKETS_SKILL.to_ascii_lowercase();
        for forbidden in ["claude", "gemini", "gpt", "haiku", "opus", "sonnet"] {
            assert!(
                !lower.contains(forbidden),
                "TO_TICKETS_SKILL must not name model/provider '{forbidden}'"
            );
        }
        assert!(
            TO_TICKETS_SKILL.lines().count() <= 120,
            "TO_TICKETS_SKILL has {} lines, expected <= 120",
            TO_TICKETS_SKILL.lines().count()
        );
    }
}
