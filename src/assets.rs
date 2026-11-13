use sha2::{Digest, Sha256};

pub const HERDR_MOD_MAIN_JS: &str = include_str!("../assets/herdr-app/mods/cockpit/main.js");
pub const HERDR_MOD_JSON: &str = include_str!("../assets/herdr-app/mods/cockpit/mod.json");
pub const HERDR_LAYOUT_JSON: &str = include_str!("../assets/herdr-app/layouts/cockpit.json");

pub const FENCE_START: &str = "# >>> cockpit managed >>>";
pub const FENCE_END: &str = "# <<< cockpit managed <<<";
pub const CLAUDE_HOOK_MARKER: &str = "# cockpit-managed";

/// POSIX / bash / zsh opt-in helper script (`~/.config/cockpit/hooks/shell.sh`).
/// Never emits OSC 7501 or `cockpit.*` labels automatically on plain shell prompts.
pub const SHELL_SH: &str = r#"# cockpit shell helper (opt-in only; preserves plain-shell filter)
cockpit-tag() {
    if [ -z "${HC_TERMINAL_ID:-}" ]; then
        printf "cockpit-tag: not running inside a Herdr (hc) terminal\n" >&2
        return 1
    fi
    if command -v cockpit >/dev/null 2>&1; then
        cockpit label "$HC_TERMINAL_ID" "$@"
    elif command -v hc >/dev/null 2>&1; then
        hc label "$HC_TERMINAL_ID" "$@"
    fi
}
"#;

/// Fish shell opt-in helper (`~/.config/cockpit/hooks/shell.fish`).
pub const SHELL_FISH: &str = r#"# cockpit fish helper (opt-in only; preserves plain-shell filter)
function cockpit-tag
    if not set -q HC_TERMINAL_ID
        echo "cockpit-tag: not running inside a Herdr (hc) terminal" >&2
        return 1
    end
    if command -sq cockpit
        cockpit label "$HC_TERMINAL_ID" $argv
    else if command -sq hc
        hc label "$HC_TERMINAL_ID" $argv
    end
end
"#;

/// Nushell opt-in helper (`~/.config/cockpit/hooks/shell.nu`).
pub const SHELL_NU: &str = r#"# cockpit nushell helper (opt-in only; preserves plain-shell filter)
def --wrapped cockpit-tag [...rest: string] {
    if ($env.HC_TERMINAL_ID? | is-empty) {
        print -e "cockpit-tag: not running inside a Herdr (hc) terminal"
        return
    }
    if (which cockpit | is-not-empty) {
        ^cockpit label $env.HC_TERMINAL_ID ...$rest
    } else if (which hc | is-not-empty) {
        ^hc label $env.HC_TERMINAL_ID ...$rest
    }
}
"#;

/// Claude Code hook script (`~/.config/cockpit/hooks/claude-hook.sh`, <20ms p99).
/// Emits Program Status Protocol OSC 7501 (`\x1b]7501;state=<state>:app=claude[:kind=<kind>][:msg=<base64>]\x1b\\`)
/// and stamps `cockpit.agent=claude` asynchronously in the background.
pub const CLAUDE_HOOK_SH: &str = r#"#!/bin/sh
# cockpit Claude Code hook (<20ms p99, non-blocking)
input="$(cat 2>/dev/null || true)"

if [ -n "${HC_TERMINAL_ID:-}" ] && command -v hc >/dev/null 2>&1; then
    (hc label "$HC_TERMINAL_ID" cockpit.agent=claude >/dev/null 2>&1 &)
fi

emit_osc7501() {
    payload="$1"
    if [ -w /dev/tty ]; then
        printf '\033]7501;%s\033\\' "$payload" > /dev/tty 2>/dev/null || true
    fi
}

case "$input" in
    *'"PermissionRequest"'*|*'"permission_prompt"'*)
        msg_b64="$(printf 'Permission required' | base64 | tr -d '\n')"
        emit_osc7501 "state=blocked:app=claude:kind=permission:msg=${msg_b64}"
        ;;
    *'"Notification"'*)
        case "$input" in
            *auth*|*login*|*token*)
                msg_b64="$(printf 'Authentication required' | base64 | tr -d '\n')"
                emit_osc7501 "state=blocked:app=claude:kind=auth:msg=${msg_b64}"
                ;;
            *permission*|*approve*)
                msg_b64="$(printf 'Permission required' | base64 | tr -d '\n')"
                emit_osc7501 "state=blocked:app=claude:kind=permission:msg=${msg_b64}"
                ;;
            *)
                msg_b64="$(printf 'Waiting for user response' | base64 | tr -d '\n')"
                emit_osc7501 "state=blocked:app=claude:kind=question:msg=${msg_b64}"
                ;;
        esac
        ;;
    *'"UserPromptSubmit"'*|*'"PreToolUse"'*|*'"PostToolUse"'*)
        emit_osc7501 "state=working:app=claude"
        ;;
    *'"Stop"'*)
        emit_osc7501 "state=done:app=claude"
        ;;
esac
exit 0
"#;

/// OpenCode ESM plugin (`~/.config/opencode/plugins/cockpit.js`).
pub const OPENCODE_COCKPIT_JS: &str = r#"// cockpit-osc7501 OpenCode plugin (<20ms p99)
import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

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

function stampAgentLabel() {
  const termId = process.env.HC_TERMINAL_ID;
  if (!termId) return;
  try {
    const child = spawn("hc", ["label", termId, "cockpit.agent=opencode"], {
      detached: true,
      stdio: "ignore",
    });
    child.unref();
  } catch (_e) {}
}

export const CockpitPlugin = async () => {
  stampAgentLabel();
  return {
    event: async ({ event }) => {
      const t = event?.type ?? "";
      if (t.includes("permission")) {
        emitOsc7501("blocked", "permission", "Waiting for permission");
      } else if (t.includes("question") || t.includes("prompt")) {
        emitOsc7501("blocked", "question", "Waiting for answer");
      } else if (t.includes("busy") || t.includes("start")) {
        emitOsc7501("working");
      } else if (t.includes("idle") || t.includes("complete")) {
        emitOsc7501("done");
      }
    },
  };
};
export default CockpitPlugin;
"#;

/// Pi coding-agent extension (`~/.pi/agent/extensions/cockpit.js`).
/// Note: Pi 1.5.2+ natively emits OSC 7501 via `@earendil-works/pi-tui/dist/program-status.js`.
/// This extension only stamps `cockpit.agent=pi` on session start so Cockpit identifies Pi immediately.
pub const PI_COCKPIT_JS: &str = r#"// cockpit-osc7501 Pi extension (defers OSC 7501 to Pi's native program-status.js)
import { spawn } from "node:child_process";

export default function cockpitPiExtension(pi) {
  const termId = process.env.HC_TERMINAL_ID;
  if (termId) {
    try {
      const child = spawn("hc", ["label", termId, "cockpit.agent=pi"], {
        detached: true,
        stdio: "ignore",
      });
      child.unref();
    } catch (_e) {}
  }
  if (typeof pi?.on === "function") {
    pi.on("session_start", () => {});
  }
}
"#;

pub const SYSTEMD_UNIT_TEMPLATE: &str = r#"[Unit]
Description=Cockpit Herdr Coding Agent Context Daemon
After=default.target

[Service]
Type=simple
ExecStart=%h/.cargo/bin/cockpit daemon
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#;

pub fn asset_sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
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
            Some("cockpit-pill")
        );

        let mod_json: serde_json::Value = serde_json::from_str(HERDR_MOD_JSON).unwrap();
        assert_eq!(mod_json.get("place").and_then(|v| v.as_str()), Some("none"));
    }

    #[test]
    fn test_embedded_mod_js_exports_contract_symbols() {
        assert!(HERDR_MOD_MAIN_JS.contains("export default class CockpitMod extends View"));
        assert!(HERDR_MOD_MAIN_JS.contains("export const id = \"cockpit\""));
        assert!(HERDR_MOD_MAIN_JS.contains("export const schemaVersion = 1"));
        assert!(HERDR_MOD_MAIN_JS.contains("export function partitionAgents"));
        assert!(HERDR_MOD_MAIN_JS.contains("export function buildViewModel"));
        assert!(HERDR_MOD_MAIN_JS.contains("export function mount"));
    }
}
