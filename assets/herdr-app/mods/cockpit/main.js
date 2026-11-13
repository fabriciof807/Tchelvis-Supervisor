// Cockpit Herdr Desktop Mod (`gpui-kit` + `herdr` runtime)
// Version: 0.1.0 | Schema Version: 1

import { View, div, svg, image } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import {
  agents,
  terminals,
  panes,
  spaces,
  focused,
  props,
  theme,
  run,
  exec,
  hc,
  shared,
  setShared,
  setAgentOrder,
  window as win,
  layout,
  keys,
} from "herdr";

export const id = "cockpit";
export const version = "0.1.0";
export const schemaVersion = 1;

const BLOCKED_KIND_ORDER = {
  auth: 0,
  permission: 1,
  question: 2,
  unknown: 3,
};

const LAUNCH_AGENTS = [
  ["Claude", ["claude"]],
  ["Codex", ["codex", "--no-daemon"]],
  ["Pi", ["pi"]],
  ["OpenCode", ["opencode"]],
  ["Shell", null],
];

const MODE_OPTIONS = [
  { key: "semi-auto", match: "SemiAuto", label: "Semi-Auto (Whitelist)" },
  { key: "alert-only", match: "AlertOnly", label: "Alert Only (Read-Only)" },
  { key: "paused", match: "Paused", label: "Paused" },
];

/**
 * Partitions and sorts agent summary rows into pinned Blocked, Active (Working/Done/Idle/Error),
 * and collapsible Recent Finished sections.
 */
export function partitionAgents(agentList = []) {
  const blocked = [];
  const active = [];
  const finished = [];

  for (const agent of agentList) {
    const isFinished =
      agent?.lifecycle?.state === "finished" || agent?.state === "finished";
    if (isFinished) {
      finished.push(agent);
      continue;
    }
    const attState =
      agent?.attention_state?.state ?? agent?.status ?? "idle";
    if (attState === "blocked") {
      blocked.push(agent);
    } else {
      active.push(agent);
    }
  }

  blocked.sort((a, b) => {
    const rankA =
      BLOCKED_KIND_ORDER[a?.attention_state?.kind ?? a?.kind ?? "question"] ?? 3;
    const rankB =
      BLOCKED_KIND_ORDER[b?.attention_state?.kind ?? b?.kind ?? "question"] ?? 3;
    if (rankA !== rankB) return rankA - rankB;
    return (
      (a?.attention_state?.since_ms ?? a?.status_ms ?? 0) -
      (b?.attention_state?.since_ms ?? b?.status_ms ?? 0)
    );
  });

  active.sort((a, b) => (b?.updated_at ?? b?.seq ?? 0) - (a?.updated_at ?? a?.seq ?? 0));
  finished.sort((a, b) => (b?.updated_at ?? b?.seq ?? 0) - (a?.updated_at ?? a?.seq ?? 0));

  return { blocked, active, finished };
}

/**
 * Pure ViewModel builder for the Attention Fleet List and Context Inspector Drawer.
 */
export function buildViewModel(
  snapshot,
  selectedAddress = null,
  inspectDetail = null,
  filterTab = "all"
) {
  if (!snapshot || snapshot.schema_version !== schemaVersion) {
    return {
      versionMismatch: Boolean(
        snapshot && snapshot.schema_version !== schemaVersion
      ),
      bannerMessage:
        snapshot && snapshot.schema_version !== schemaVersion
          ? `Cockpit schema mismatch (found v${snapshot.schema_version}, expected v${schemaVersion}). Run 'cockpit setup'.`
          : null,
      sections: { blocked: [], active: [], finished: [] },
      selectedAgent: null,
      inspector: null,
      providers: snapshot?.providers ?? {},
    };
  }

  const partitioned = partitionAgents(snapshot.agents ?? []);
  const filteredSections = {
    blocked: filterTab === "finished" ? [] : partitioned.blocked,
    active:
      filterTab === "blocked" || filterTab === "finished"
        ? []
        : partitioned.active,
    finished: filterTab === "blocked" ? [] : partitioned.finished,
  };

  const allAgents = snapshot.agents ?? [];
  const selectedAgent =
    allAgents.find((a) => a.address === selectedAddress) ??
    partitioned.blocked[0] ??
    partitioned.active[0] ??
    allAgents[0] ??
    null;

  return {
    versionMismatch: false,
    bannerMessage: null,
    summary: snapshot.summary ?? {},
    sections: filteredSections,
    selectedAgent,
    inspector: inspectDetail?.agent ?? selectedAgent,
    providers: inspectDetail?.providers ?? snapshot.providers ?? {},
  };
}

/**
 * Programmatic mount helper for headless tests.
 */
export function mount(ctx, ui) {
  let state = {
    snapshot: null,
    selectedAddress: null,
    inspectDetail: null,
    filterTab: "all",
  };
  return {
    getState: () => state,
    selectAgent: (addr) => {
      state.selectedAddress = addr;
      if (typeof ui?.render === "function") {
        ui.render({
          viewModel: buildViewModel(
            state.snapshot,
            state.selectedAddress,
            state.inspectDetail,
            state.filterTab
          ),
        });
      }
    },
  };
}

function findNode(node, nodeId) {
  if (!node) return null;
  if (node.id === nodeId) return node;
  for (const child of node.children ?? []) {
    const found = findNode(child, nodeId);
    if (found) return found;
  }
  return null;
}

function openExternalUrl(url) {
  if (!url) return;
  const p = win()?.platform;
  if (p === "windows") exec("explorer.exe", [url]);
  else exec(p === "macos" ? "open" : "xdg-open", [url]);
}

function programIcon(prog, size, color) {
  if (!prog?.icon) return null;
  return prog.tint
    ? svg(prog.icon).size(size).flex_none().text_color(color)
    : image(prog.icon).w(size).h(size).flex_none();
}

function formatTimestamp(ts) {
  if (!ts) return "--:--:--";
  const d = new Date(ts * 1000);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  return `${hh}:${mm}:${ss}`;
}

function parseSnapshotOutput(stdout) {
  if (!stdout) return null;
  const parsed = JSON.parse(stdout);
  return parsed?.snapshot ?? parsed ?? null;
}

function isPlainShellRow(name, programName, labels) {
  const agentLabel = String(labels?.["cockpit.agent"] ?? "")
    .trim()
    .toLowerCase();
  if (agentLabel && agentLabel !== "shell") {
    return false;
  }
  const rawProg = String(programName ?? "").trim().toLowerCase();
  const baseProg = rawProg.split(/[\\/]/).pop().replace(/^-/, "");
  const plainShells = new Set([
    "zsh",
    "bash",
    "fish",
    "sh",
    "nu",
    "nushell",
    "dash",
    "tmux",
    "cockpit",
    "herdr-supervisor",
    "cockpit-supervisor",
  ]);
  if (baseProg && plainShells.has(baseProg)) {
    return true;
  }
  return /\((zsh|bash|fish|sh|nu|dash|tmux)\)$/i.test(String(name ?? "").trim());
}

function mergeFleetRows(liveAgents, allTerminals, snap, supSnap = null) {
  const snapMap = new Map(
    (snap?.agents ?? []).map((a) => [a.address, a])
  );
  const supMap = new Map(
    (supSnap?.agents ?? []).map((a) => [a.agent_id, a])
  );
  const termMap = new Map(allTerminals.map((t) => [t.address, t]));
  const seen = new Set();
  const rows = [];

  for (const a of liveAgents) {
    const term = termMap.get(a.address) ?? {};
    const enriched = snapMap.get(a.address) ?? {};
    const sup = supMap.get(a.address) ?? null;
    const rowName =
      enriched.name ||
      sup?.session_name ||
      a.tab_name ||
      a.title ||
      term.name ||
      a.address;
    const rowProgram = a.program ?? term.program;
    const rowLabels = term?.labels ?? {};
    if (
      !snapMap.has(a.address) &&
      isPlainShellRow(rowName, rowProgram, rowLabels)
    ) {
      continue;
    }
    seen.add(a.address);
    const pane = panes().find((p) => p.terminal === a.address);
    const baseStatus = a.status ?? enriched?.attention_state?.state ?? "idle";
    const status = sup?.action_card ? "blocked" : baseStatus;
    rows.push({
      address: a.address,
      pane: a.pane ?? pane?.pane ?? null,
      space_name: a.space_name ?? "",
      tab_name: a.tab_name ?? a.title ?? a.address,
      name: rowName,
      status,
      kind:
        enriched?.attention_state?.kind ??
        term?.report?.kind ??
        term?.status?.kind ??
        (sup?.action_card ? "permission" : null),
      message:
        enriched?.attention_state?.message ??
        term?.report?.msg ??
        term?.status?.msg ??
        sup?.action_card?.reason_for_operator ??
        null,
      program: rowProgram,
      branch:
        pane?.branch ??
        enriched?.workspace?.git_branch ??
        null,
      cwd:
        pane?.cwd ??
        enriched?.workspace?.cwd ??
        term?.cwd ??
        null,
      labels: term?.labels ?? {},
      correlation_tier: enriched?.correlation_tier ?? null,
      context: enriched?.context ?? null,
      blocked_children: enriched?.blocked_children ?? [],
      lifecycle: { state: "live" },
      focused: Boolean(a.focused),
      pinned: Boolean(a.pinned),
      seq: a.seq ?? 0,
      sup_lifecycle: sup?.lifecycle_state ?? null,
      action_card: sup?.action_card ?? null,
      audit_badge: sup?.audit_badge ?? null,
      auto_reply_log: sup?.auto_reply_log ?? [],
      rolling_summary: sup?.rolling_summary ?? null,
      last_output_preview: sup?.last_output_preview ?? "",
    });
  }

  for (const sa of snap?.agents ?? []) {
    if (seen.has(sa.address)) continue;
    seen.add(sa.address);
    const term = termMap.get(sa.address) ?? {};
    const sup = supMap.get(sa.address) ?? null;
    const baseStatus = sa?.attention_state?.state ?? "idle";
    const status = sup?.action_card ? "blocked" : baseStatus;
    rows.push({
      address: sa.address,
      pane: null,
      space_name: "",
      tab_name: sa.name || sup?.session_name || sa.address,
      name: sa.name || sup?.session_name || sa.address,
      status,
      kind:
        sa?.attention_state?.kind ??
        (sup?.action_card ? "permission" : null),
      message:
        sa?.attention_state?.message ??
        sup?.action_card?.reason_for_operator ??
        null,
      program: term.program ?? null,
      branch: sa?.workspace?.git_branch ?? null,
      cwd: sa?.workspace?.cwd ?? null,
      labels: term?.labels ?? {},
      correlation_tier: sa?.correlation_tier ?? null,
      context: sa?.context ?? null,
      blocked_children: sa?.blocked_children ?? [],
      lifecycle: sa?.lifecycle ?? { state: term.state ?? "live" },
      focused: false,
      pinned: false,
      seq: 0,
      sup_lifecycle: sup?.lifecycle_state ?? null,
      action_card: sup?.action_card ?? null,
      audit_badge: sup?.audit_badge ?? null,
      auto_reply_log: sup?.auto_reply_log ?? [],
      rolling_summary: sup?.rolling_summary ?? null,
      last_output_preview: sup?.last_output_preview ?? "",
    });
  }

  for (const sup of supSnap?.agents ?? []) {
    if (seen.has(sup.agent_id)) continue;
    const term = termMap.get(sup.agent_id) ?? {};
    const rowName = sup.session_name || term.name || sup.agent_id;
    const rowProgram = term.program ?? null;
    const rowLabels = term?.labels ?? {};
    if (
      !snapMap.has(sup.agent_id) &&
      isPlainShellRow(rowName, rowProgram, rowLabels)
    ) {
      continue;
    }
    seen.add(sup.agent_id);
    const pane = panes().find((p) => p.terminal === sup.agent_id);
    rows.push({
      address: sup.agent_id,
      pane: pane?.pane ?? null,
      space_name: "",
      tab_name: rowName,
      name: rowName,
      status: sup.action_card ? "blocked" : "working",
      kind: sup.action_card ? "permission" : null,
      message: sup.action_card?.reason_for_operator ?? null,
      program: rowProgram,
      branch: pane?.branch ?? null,
      cwd: term?.cwd ?? null,
      labels: term?.labels ?? {},
      correlation_tier: null,
      context: null,
      blocked_children: [],
      lifecycle: {
        state: sup.lifecycle_state === "COMPLETED" ? "finished" : "live",
      },
      focused: false,
      pinned: false,
      seq: 0,
      sup_lifecycle: sup.lifecycle_state ?? null,
      action_card: sup.action_card ?? null,
      audit_badge: sup.audit_badge ?? null,
      auto_reply_log: sup.auto_reply_log ?? [],
      rolling_summary: sup.rolling_summary ?? null,
      last_output_preview: sup.last_output_preview ?? "",
    });
  }

  return rows;
}

export default class CockpitMod extends View {
  init(_p, cx) {
    this.filterTab = "all";
    this.showFinished = false;
    this.showLauncher = false;
    this.busy = false;
    this.pollingSupervisor = false;
    this.note = "";
    this.promptText = "";
    this.labelKey = "issue";
    this.labelValue = "";
    this.commandDraft = "";
    this.commandDraftKey = "";
    this.editingField = null; // "prompt" | "label" | "command" | null
    this.inspectCache = {}; // address -> InspectResponse
    this.doctorReport = null;
    this.handle = cx.focus_handle();

    const role = props()?.role ?? "fleet";
    if (role === "fleet") {
      this.refreshCockpit(cx, false);
      this.fetchSupervisorStatus(cx);
      cx.timer.every(5000, (cx) => this.refreshCockpit(cx, false));
      cx.timer.every(12000, (cx) => this.tickSupervisor(cx));
    } else if (role === "inspector") {
      this.refreshInspector(cx);
      if (!shared("supervisor.snapshot")) {
        this.fetchSupervisorStatus(cx);
      }
      cx.timer.every(6000, (cx) => this.refreshInspector(cx));
    } else if (role === "pill" || role === "status") {
      if (!shared("supervisor.snapshot")) {
        this.fetchSupervisorStatus(cx);
      }
    }
  }

  selectedAddress() {
    const f = focused();
    const sharedSel = shared("cockpit.selected");
    if (f?.terminal) return f.terminal;
    if (typeof sharedSel === "string" && sharedSel) return sharedSel;
    const first = agents()[0]?.address;
    if (first) return first;
    const supSnap = shared("supervisor.snapshot");
    const firstAlert = (supSnap?.agents ?? []).find((a) => a.action_card)?.agent_id;
    return firstAlert ?? supSnap?.agents?.[0]?.agent_id ?? null;
  }

  fetchSupervisorStatus(cx) {
    if (this.pollingSupervisor) return;
    this.pollingSupervisor = true;
    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["supervisor", "status", "--json"], {
          timeout: 8000,
        });
        if (res.code === 0 && res.stdout) {
          const snap = parseSnapshotOutput(res.stdout);
          if (snap) {
            setShared("supervisor.snapshot", snap);
            setShared("supervisor.lastError", "");
          }
        }
      } catch (_e) {
        // Ignore if supervisor is unavailable
      } finally {
        this.pollingSupervisor = false;
        cx.notify();
      }
    });
  }

  tickSupervisor(cx) {
    if (this.pollingSupervisor) return;
    this.pollingSupervisor = true;
    setShared("supervisor.evaluating", true);
    cx.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["supervisor", "tick", "--json"], {
          timeout: 25000,
        });
        if (res.code === 0 && res.stdout) {
          const snap = parseSnapshotOutput(res.stdout);
          if (snap) {
            setShared("supervisor.snapshot", snap);
            setShared("supervisor.lastError", "");
          }
        } else if (res.stderr) {
          setShared("supervisor.lastError", res.stderr.trim());
        }
      } catch (e) {
        setShared("supervisor.lastError", String(e));
      } finally {
        this.pollingSupervisor = false;
        setShared("supervisor.evaluating", false);
        cx.notify();
      }
    });
  }

  runSupervisorAction(cx, args, successNote) {
    if (this.busy) return;
    this.busy = true;
    cx.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", args, { timeout: 15000 });
        if (res.code === 0 && res.stdout) {
          const snap = parseSnapshotOutput(res.stdout);
          if (snap) {
            setShared("supervisor.snapshot", snap);
            setShared("supervisor.lastError", "");
          }
          if (successNote) this.note = successNote;
        } else {
          this.note = res.stderr?.trim() || `Command failed (${res.code})`;
        }
      } catch (e) {
        this.note = `Error: ${String(e)}`;
      } finally {
        this.busy = false;
        cx.notify();
      }
    });
  }

  approveActionCard(cx, agentId) {
    if (!agentId) return;
    const payload = this.commandDraft;
    this.editingField = null;
    this.runSupervisorAction(
      cx,
      ["supervisor", "approve", agentId, payload],
      `Aprovado e enviado para ${agentId}`
    );
  }

  ignoreActionCard(cx, agentId) {
    if (!agentId) return;
    this.editingField = null;
    this.runSupervisorAction(
      cx,
      ["supervisor", "ignore", agentId],
      `Alerta ignorado para ${agentId}`
    );
  }

  refreshCockpit(cx, forceRefresh = false) {
    if (this.fetchingSnap) return;
    this.fetchingSnap = true;
    cx.spawn(async (cx) => {
      try {
        const args = ["status", "--json", "--no-spawn"];
        if (forceRefresh) args.push("--refresh");
        const res = await exec("cockpit", args, { timeout: 8000 });
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          setShared("cockpit.snapshot", parsed);
          this.note = "";
        }
      } catch (_e) {
        // Fallback smoothly to live Herdr `agents()` / `terminals()`
      } finally {
        this.fetchingSnap = false;
        cx.notify();
      }
    });
  }

  refreshInspector(cx) {
    const addr = this.selectedAddress();
    if (!addr || this.fetchingInspect) return;
    this.fetchingInspect = true;
    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["inspect", addr, "--json"], {
          timeout: 8000,
        });
        if (res.code === 0 && res.stdout) {
          this.inspectCache[addr] = JSON.parse(res.stdout);
        }
      } catch (_e) {
        // Ignore if terminal is plain shell or cockpit binary is absent
      } finally {
        this.fetchingInspect = false;
        cx.notify();
      }
    });
  }

  runDoctorCheck(cx) {
    if (this.busy) return;
    this.busy = true;
    this.note = "Running cockpit doctor…";
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["doctor", "--json"], {
          timeout: 10000,
        });
        if (res.code === 0 && res.stdout) {
          this.doctorReport = JSON.parse(res.stdout);
          this.note = `Doctor: ${this.doctorReport.overall_status.toUpperCase()}`;
        } else {
          this.note = "cockpit doctor failed";
        }
      } catch (e) {
        this.note = `Error: ${String(e)}`;
      } finally {
        this.busy = false;
        cx.notify();
      }
    });
  }

  runSetupGui(cx) {
    if (this.busy) return;
    this.busy = true;
    this.note = "Running cockpit setup…";
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["setup"], { timeout: 10000 });
        this.note =
          res.code === 0 ? "Hooks & mod verified" : "Setup returned warning";
      } catch (e) {
        this.note = `Setup error: ${String(e)}`;
      } finally {
        this.busy = false;
        this.refreshCockpit(cx, true);
        cx.notify();
      }
    });
  }

  applyLabelGui(cx, address, key, value) {
    if (!address || !key) return;
    const fullKey = key.startsWith("cockpit.") ? key : `cockpit.${key}`;
    this.busy = true;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        await hc(["label", address, `${fullKey}=${value}`]);
        this.labelValue = "";
        this.editingField = null;
        this.note = value
          ? `Set ${fullKey}=${value}`
          : `Removed ${fullKey}`;
      } catch (e) {
        this.note = `Label error: ${String(e)}`;
      } finally {
        this.busy = false;
        this.refreshCockpit(cx, false);
        this.refreshInspector(cx);
        cx.notify();
      }
    });
  }

  sendPromptGui(cx, address) {
    const text = this.promptText.trim();
    if (!address || !text) return;
    this.promptText = "";
    this.editingField = null;
    this.note = `Sent to ${address}`;
    cx.notify();
    hc(["send", address, text]);
  }

  badge(t, text, fg, bg) {
    return div()
      .flex_none()
      .px(6)
      .py(1)
      .rounded(4)
      .bg(bg)
      .text_size(10.5)
      .font_semibold()
      .text_color(fg)
      .child(text);
  }

  actionBtn(t, id, label, onClick, accent = false, danger = false) {
    const borderCol = danger ? t.blocked : accent ? t.accent : t.border;
    const bgCol = danger ? t.blocked + "22" : accent ? t.selected : t.raised;
    const fgCol = danger ? t.blocked : accent ? t.foreground : t.muted;
    return h_flex()
      .id(id)
      .h(25)
      .px(8)
      .gap(5)
      .items_center()
      .rounded(6)
      .border(1)
      .border_color(borderCol)
      .bg(bgCol)
      .text_size(11.5)
      .text_color(fgCol)
      .cursor_pointer()
      .hover((s) => s.bg(t.hover).text_color(t.foreground))
      .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
      .on_click(onClick)
      .child(label);
  }

  lifecycleColors(t, state) {
    switch (state) {
      case "WAITING_CONFIRMATION":
        return { fg: t.blocked, bg: t.blocked + "24" };
      case "STUCK_LOOP":
      case "ERROR":
        return { fg: t.blocked, bg: t.blocked + "30" };
      case "COMPLETED":
        return { fg: t.done, bg: t.done + "24" };
      case "WORKING":
      default:
        return { fg: t.working, bg: t.working + "22" };
    }
  }

  renderPill(cx) {
    const t = theme();
    const supSnap = shared("supervisor.snapshot");
    const live = agents();
    const evaluating =
      Boolean(shared("supervisor.evaluating")) ||
      supSnap?.runtime_status === "Evaluating";
    const watchingCount =
      supSnap?.runtime_status?.Watching ??
      (supSnap?.agents?.length ?? live.length);
    const alertCount = (supSnap?.agents ?? []).filter((a) => a.action_card).length;
    const blockedCount = live.filter((a) => a.status === "blocked").length;
    const activeMode = supSnap?.mode ?? "SemiAuto";
    const pillText = evaluating
      ? "Supervisor: Evaluating..."
      : supSnap?.pill_text ??
        (watchingCount > 0
          ? `Supervisor: Watching (${watchingCount})`
          : "Supervisor: Idle (0 MB)");
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );
    const w = win();
    const macNoFs = w?.platform === "macos" && !w?.fullscreen;

    const dotColor = evaluating
      ? t.accent
      : alertCount > 0 || blockedCount > 0
        ? t.blocked
        : watchingCount > 0
          ? t.working
          : t.dim;

    return h_flex()
      .size_full()
      .pl(macNoFs ? 76 : 10)
      .pr(10)
      .gap(8)
      .items_center()
      .bg(t.chrome)
      .border_b(1)
      .border_color(t.border)
      .child(
        // Status Pill
        h_flex()
          .id("ck-status-pill")
          .h(24)
          .px(9)
          .gap(6)
          .items_center()
          .rounded(12)
          .border(1)
          .border_color(alertCount > 0 || blockedCount > 0 ? t.blocked : t.border)
          .bg(t.surface)
          .when(evaluating, (el) =>
            el.child(
              svg("mods/workspace-chrome/spinner.svg")
                .size(11)
                .text_color(t.accent)
                .spin(900)
            )
          )
          .when(!evaluating, (el) =>
            el.child(div().size(7).rounded(4).bg(dotColor))
          )
          .child(
            div()
              .text_size(11.5)
              .font_semibold()
              .text_color(t.foreground)
              .child(pillText)
          )
          .when(Boolean(supSnap?.model_loaded), (el) =>
            el.child(this.badge(t, "LLM RAM", t.accent, t.raised))
          )
      )
      .child(
        // Mode Toggle Group
        h_flex()
          .gap(4)
          .items_center()
          .children(
            MODE_OPTIONS.map((m) => {
              const isCurrent = activeMode === m.match;
              return h_flex()
                .id(`ck-mode-${m.key}`)
                .h(23)
                .px(7)
                .items_center()
                .rounded(5)
                .border(1)
                .border_color(isCurrent ? t.accent : t.border)
                .bg(isCurrent ? t.selected : t.raised)
                .text_size(11)
                .text_color(isCurrent ? t.foreground : t.muted)
                .cursor_pointer()
                .hover((s) => s.bg(t.hover).text_color(t.foreground))
                .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
                .on_click((_e, cx) =>
                  this.runSupervisorAction(
                    cx,
                    ["supervisor", "mode", m.key],
                    `Modo: ${m.label}`
                  )
                )
                .child(m.label);
            })
          )
      )
      .child(div().flex_1())
      .when(alertCount > 0, (el) =>
        el.child(
          this.badge(
            t,
            `${alertCount} Alert${alertCount > 1 ? "s" : ""}`,
            t.blocked,
            t.blocked + "24"
          )
        )
      )
      .when(blockedCount > 0, (el) =>
        el.child(
          this.badge(
            t,
            `${blockedCount} Blocked`,
            t.blocked,
            t.blocked + "24"
          )
        )
      )
      .child(
        this.actionBtn(t, "ck-pill-poll", "⚡ Poll Now", (_e, cx) => {
          this.refreshCockpit(cx, true);
          this.tickSupervisor(cx);
        })
      )
      .child(
        this.actionBtn(t, "ck-pill-unload", "⏏ Unload LLM", (_e, cx) =>
          this.runSupervisorAction(
            cx,
            ["supervisor", "unload"],
            "Modelo Ollama descarregado"
          )
        )
      )
      .child(
        this.actionBtn(
          t,
          "ck-pill-inspector",
          inspectorHidden ? "Inspector ◁" : "Inspector ▷",
          () => run("layout.toggle", { id: "cockpit-inspector" }),
          !inspectorHidden
        )
      )
      .child(
        this.actionBtn(t, "ck-pill-layout", "Switch Layout", () =>
          run("layout.switch")
        )
      );
  }

  renderFleet(cx) {
    const t = theme();
    const snap = shared("cockpit.snapshot");
    const supSnap = shared("supervisor.snapshot");
    const live = agents();
    const allTerms = terminals();
    const merged = mergeFleetRows(live, allTerms, snap, supSnap);
    const parts = partitionAgents(merged);

    const orderedAddrs = [
      ...parts.blocked.map((a) => a.address),
      ...parts.active.map((a) => a.address),
    ];
    if (orderedAddrs.length) setAgentOrder(orderedAddrs);

    const selAddr = this.selectedAddress();
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );

    const filterChip = (key, label, count) => {
      const on = this.filterTab === key;
      return h_flex()
        .id(`ck-filter-${key}`)
        .h(23)
        .px(7)
        .gap(4)
        .items_center()
        .rounded(5)
        .bg(on ? t.selected : t.chrome)
        .text_size(11)
        .text_color(on ? t.foreground : t.muted)
        .cursor_pointer()
        .hover((s) => s.bg(t.hover).text_color(t.foreground))
        .on_click((_e, cx) => {
          this.filterTab = key;
          cx.notify();
        })
        .child(label)
        .when(count !== null, (el) =>
          el.child(div().text_size(10).text_color(t.dim).child(String(count)))
        );
    };

    const renderAgentCard = (a) => {
      const isSelected = a.address === selAddr;
      const isBlocked = a.status === "blocked";
      const isWorking = a.status === "working";
      const isDone = a.status === "done";
      const statusColor = isBlocked
        ? t.blocked
        : isWorking
          ? t.working
          : isDone
            ? t.done
            : t.dim;
      const pr = a.context?.pull_request;
      const issue = a.context?.issues?.[0]?.key ?? a.labels?.["cockpit.issue"];
      const tier = a.correlation_tier;
      const hasAlert = Boolean(a.action_card);
      const urgency = (a.action_card?.urgency ?? "normal").toUpperCase();
      const supLc = a.sup_lifecycle
        ? this.lifecycleColors(t, a.sup_lifecycle)
        : null;

      return v_flex()
        .id(`ck-row-${a.address}`)
        .mx(8)
        .px(9)
        .py(7)
        .gap(4)
        .rounded(8)
        .border(1)
        .border_color(
          isBlocked || hasAlert ? t.blocked : isSelected ? t.accent : t.border
        )
        .bg(isSelected ? t.selected : t.surface)
        .cursor_pointer()
        .hover((s) => s.bg(t.hover))
        .on_click((_e, cx) => {
          setShared("cockpit.selected", a.address);
          if (a.pane) run("pane.focus", { pane: a.pane });
          else run("agent.focus", { terminal: a.address });
          cx.notify();
        })
        .child(
          h_flex()
            .gap(6)
            .items_center()
            .when(Boolean(a.program?.icon), (el) =>
              el.child(programIcon(a.program, 14, t.foreground))
            )
            .child(
              div()
                .flex_1()
                .min_w(0)
                .truncate()
                .text_size(12.5)
                .font_semibold()
                .text_color(t.foreground)
                .child(
                  a.space_name ? `${a.space_name} · ${a.name}` : a.name
                )
            )
            .child(
              div().text_size(10.5).text_color(t.dim).child(a.address)
            )
            .when(isWorking, (el) =>
              el.child(
                svg("mods/workspace-chrome/spinner.svg")
                  .size(12)
                  .text_color(t.working)
                  .spin(900)
              )
            )
            .when(!isWorking, (el) =>
              el.child(div().size(7).rounded(4).bg(statusColor))
            )
        )
        .when(
          Boolean(a.sup_lifecycle || hasAlert || a.audit_badge),
          (el) =>
            el.child(
              h_flex()
                .gap(5)
                .flex_wrap()
                .items_center()
                .when(Boolean(a.sup_lifecycle && supLc), (el2) =>
                  el2.child(
                    this.badge(t, a.sup_lifecycle, supLc.fg, supLc.bg)
                  )
                )
                .when(hasAlert, (el2) =>
                  el2.child(
                    this.badge(
                      t,
                      `ALERT · ${urgency}`,
                      t.blocked,
                      t.blocked + "28"
                    )
                  )
                )
                .when(Boolean(a.audit_badge), (el2) =>
                  el2.child(
                    this.badge(t, a.audit_badge, t.accent, t.raised)
                  )
                )
            )
        )
        .when(isBlocked, (el) =>
          el.child(
            h_flex()
              .gap(5)
              .items_center()
              .child(
                this.badge(
                  t,
                  `NEEDS YOU · ${(a.kind ?? "question").toUpperCase()}`,
                  t.blocked,
                  t.blocked + "24"
                )
              )
              .when(Boolean(a.message), (el2) =>
                el2.child(
                  div()
                    .flex_1()
                    .min_w(0)
                    .truncate()
                    .text_size(11)
                    .text_color(t.foreground)
                    .child(a.message)
                )
              )
          )
        )
        .when(Boolean(a.rolling_summary), (el) =>
          el.child(
            div()
              .text_size(11)
              .text_color(t.muted)
              .child(a.rolling_summary)
          )
        )
        .when(a.blocked_children?.length > 0, (el) =>
          el.child(
            div()
              .text_size(10.5)
              .text_color(t.blocked)
              .child(
                `↳ Subagent blocked: ${a.blocked_children.join(", ")}`
              )
          )
        )
        .child(
          h_flex()
            .gap(5)
            .items_center()
            .text_size(11)
            .text_color(t.muted)
            .when(Boolean(a.branch), (el) =>
              el.child(
                div().flex_1().min_w(0).truncate().child(`⎇ ${a.branch}`)
              )
            )
            .when(!a.branch, (el) =>
              el.child(
                div().flex_1().text_color(t.dim).child("Local / No branch")
              )
            )
            .when(Boolean(issue), (el) =>
              el.child(this.badge(t, issue, t.accent, t.raised))
            )
            .when(Boolean(pr), (el) =>
              el.child(
                this.badge(
                  t,
                  `#${pr.number} ${pr.ci_rollup ?? ""}`.trim(),
                  pr.ci_rollup === "FAILURE" ? t.blocked : t.done,
                  t.raised
                )
              )
            )
            .when(!pr && tier === "local_only", (el) =>
              el.child(this.badge(t, "Local", t.dim, t.raised))
            )
        );
    };

    const showBlocked = this.filterTab !== "finished" ? parts.blocked : [];
    const showActive =
      this.filterTab === "all" || this.filterTab === "active"
        ? parts.active
        : [];
    const showFinished =
      this.filterTab === "finished" || this.showFinished ? parts.finished : [];

    return v_flex()
      .size_full()
      .bg(t.chrome)
      .text_color(t.foreground)
      .border_t(1)
      .border_color(t.border)
      .child(
        // Header + GUI actions bar
        h_flex()
          .h(34)
          .px(10)
          .gap(6)
          .items_center()
          .border_b(1)
          .border_color(t.border)
          .child(
            div()
              .text_size(12)
              .font_semibold()
              .text_color(t.foreground)
              .child("COCKPIT FLEET")
          )
          .child(div().flex_1())
          .child(
            this.actionBtn(
              t,
              "ck-btn-new",
              "+ Agent",
              (_e, cx) => {
                this.showLauncher = !this.showLauncher;
                cx.notify();
              },
              this.showLauncher
            )
          )
          .child(
            this.actionBtn(t, "ck-btn-refresh", "↻", (_e, cx) =>
              this.refreshCockpit(cx, true)
            )
          )
          .child(
            this.actionBtn(
              t,
              "ck-btn-inspector",
              inspectorHidden ? "Inspector ◁" : "Inspector ▷",
              () => run("layout.toggle", { id: "cockpit-inspector" }),
              !inspectorHidden
            )
          )
      )
      .when(this.showLauncher, (el) =>
        el.child(
          v_flex()
            .p(8)
            .gap(6)
            .bg(t.raised)
            .border_b(1)
            .border_color(t.border)
            .child(
              div()
                .text_size(11)
                .text_color(t.muted)
                .child("Launch in active space:")
            )
            .child(
              h_flex()
                .gap(5)
                .flex_wrap()
                .children(
                  LAUNCH_AGENTS.map(([label, argv], idx) =>
                    this.actionBtn(
                      t,
                      `ck-launch-${idx}`,
                      label,
                      (_e, cx) => {
                        const sp = spaces().find((s) => s.active);
                        run("tab.new", {
                          space: sp?.id,
                          name: label.toLowerCase(),
                          argv: argv ?? undefined,
                        });
                        this.showLauncher = false;
                        cx.notify();
                      }
                    )
                  )
                )
            )
        )
      )
      .child(
        // Filter strip
        h_flex()
          .h(29)
          .px(8)
          .gap(4)
          .items_center()
          .border_b(1)
          .border_color(t.border)
          .child(filterChip("all", "All", merged.length))
          .child(filterChip("blocked", "Needs You", parts.blocked.length))
          .child(filterChip("active", "Active", parts.active.length))
          .child(filterChip("finished", "Done", parts.finished.length))
      )
      .child(
        // Fleet list
        v_flex()
          .flex_1()
          .min_h(0)
          .overflow_y_scrollbar()
          .py(6)
          .gap(6)
          .when(merged.length === 0, (el) =>
            el.child(
              v_flex()
                .p(14)
                .gap(8)
                .items_center()
                .text_size(12)
                .text_color(t.muted)
                .child("No coding agents running yet.")
                .child(
                  this.actionBtn(
                    t,
                    "ck-empty-claude",
                    "Start Claude Code",
                    () => {
                      const sp = spaces().find((s) => s.active);
                      run("tab.new", {
                        space: sp?.id,
                        name: "claude",
                        argv: ["claude"],
                      });
                    },
                    true
                  )
                )
            )
          )
          .children(showBlocked.map(renderAgentCard))
          .children(showActive.map(renderAgentCard))
          .when(
            this.filterTab === "all" && parts.finished.length > 0,
            (el) =>
              el.child(
                h_flex()
                  .id("ck-toggle-finished")
                  .mx(10)
                  .mt(4)
                  .px(6)
                  .h(24)
                  .items_center()
                  .justify_between()
                  .rounded(5)
                  .text_size(11)
                  .text_color(t.dim)
                  .cursor_pointer()
                  .hover((s) => s.bg(t.hover).text_color(t.muted))
                  .on_click((_e, cx) => {
                    this.showFinished = !this.showFinished;
                    cx.notify();
                  })
                  .child(`Recent Finished (${parts.finished.length})`)
                  .child(this.showFinished ? "▾" : "▸")
              )
          )
          .children(showFinished.map(renderAgentCard))
      );
  }

  renderInspector(cx) {
    const t = theme();
    const snap = shared("cockpit.snapshot");
    const supSnap = shared("supervisor.snapshot");
    const live = agents();
    const allTerms = terminals();
    const merged = mergeFleetRows(live, allTerms, snap, supSnap);
    const addr = this.selectedAddress();
    const agent =
      merged.find((a) => a.address === addr) ?? merged[0] ?? null;
    const detailed =
      (addr && this.inspectCache[addr]?.agent) ||
      (snap?.agents ?? []).find((a) => a.address === agent?.address) ||
      agent;
    const providers =
      (addr && this.inspectCache[addr]?.providers) ||
      snap?.providers ||
      {};

    const supAgent =
      (supSnap?.agents ?? []).find((a) => a.agent_id === agent?.address) ??
      null;
    const actionCard = supAgent?.action_card ?? agent?.action_card ?? null;
    if (actionCard && agent) {
      const nextKey = `${agent.address}:${actionCard.triggered_at ?? ""}`;
      if (this.editingField !== "command" && this.commandDraftKey !== nextKey) {
        this.commandDraft = actionCard.hc_command_payload ?? "";
        this.commandDraftKey = nextKey;
      }
    } else if (this.editingField !== "command") {
      this.commandDraft = "";
      this.commandDraftKey = "";
    }

    const urgencyLabel =
      actionCard?.urgency === "critical"
        ? "Critical"
        : actionCard?.urgency === "low"
          ? "Low"
          : "Normal";
    const urgencyColor =
      actionCard?.urgency === "critical"
        ? t.blocked
        : actionCard?.urgency === "low"
          ? t.muted
          : t.accent;
    const auditLog = supAgent?.auto_reply_log ?? agent?.auto_reply_log ?? [];
    const auditBadge = supAgent?.audit_badge ?? agent?.audit_badge ?? null;
    const rollingSummary =
      supAgent?.rolling_summary ?? agent?.rolling_summary ?? null;
    const outputPreview =
      supAgent?.last_output_preview ?? agent?.last_output_preview ?? "";
    const cleanedPreviewLines = String(outputPreview ?? "")
      .split(/\r?\n/)
      .map((line) => {
        const trimmed = line.trim();
        if (trimmed.startsWith("│") || trimmed.endsWith("│")) {
          return trimmed.replace(/^│+|│+$/g, "").trim();
        }
        return line.trimEnd();
      })
      .filter(
        (line) =>
          line.trim().length > 0 &&
          !/^[\s│┃╎╏¦|─━╭╮╯╰┌┐┘└├┤┬┴┼═║╔╗╚╝]+$/.test(line)
      )
      .slice(-20);

    const sectionTitle = (title) =>
      div()
        .pt(8)
        .pb(3)
        .text_size(10.5)
        .font_semibold()
        .text_color(t.dim)
        .child(title);

    const cardBox = () =>
      v_flex()
        .p(10)
        .gap(6)
        .rounded(8)
        .border(1)
        .border_color(t.border)
        .bg(t.surface);

    const pr = detailed?.context?.pull_request;
    const issues = detailed?.context?.issues ?? [];
    const slackThreads = detailed?.context?.slack_threads ?? [];
    const runbooks = detailed?.context?.runbooks ?? [];
    const labels = agent?.labels ?? {};

    return v_flex()
      .id("ck-inspector-root")
      .size_full()
      .track_focus(this.handle)
      .bg(t.chrome)
      .text_color(t.foreground)
      .rounded(10)
      .border(1)
      .border_color(t.border)
      .overflow_hidden()
      .on_key_down((e, cx) => {
        if (!this.editingField) return;
        const m = e.modifiers;
        if (e.key === "escape") {
          this.editingField = null;
        } else if (e.key === "enter") {
          if (this.editingField === "command" && agent) {
            this.approveActionCard(cx, agent.address);
          } else if (this.editingField === "prompt" && agent) {
            this.sendPromptGui(cx, agent.address);
          } else if (this.editingField === "label" && agent) {
            this.applyLabelGui(
              cx,
              agent.address,
              this.labelKey,
              this.labelValue.trim()
            );
          }
        } else if (e.key === "backspace") {
          if (this.editingField === "command") {
            this.commandDraft = this.commandDraft.slice(0, -1);
          } else if (this.editingField === "prompt") {
            this.promptText = this.promptText.slice(0, -1);
          } else {
            this.labelValue = this.labelValue.slice(0, -1);
          }
        } else if (e.key_char && !m?.control && !m?.platform) {
          if (this.editingField === "command") {
            this.commandDraft += e.key_char;
          } else if (this.editingField === "prompt") {
            this.promptText += e.key_char;
          } else {
            this.labelValue += e.key_char;
          }
        } else {
          return;
        }
        cx.stop_propagation();
        cx.notify();
      })
      .child(
        // Top bar
        h_flex()
          .h(38)
          .px(12)
          .gap(6)
          .items_center()
          .border_b(1)
          .border_color(t.border)
          .child(
            div()
              .flex_1()
              .truncate()
              .text_size(12.5)
              .font_semibold()
              .child(
                agent
                  ? `COCKPIT · ${agent.name} (${agent.address})`
                  : "COCKPIT INSPECTOR"
              )
          )
          .child(
            this.actionBtn(t, "ck-ins-refresh", "Refresh", (_e, cx) => {
              this.refreshCockpit(cx, true);
              this.refreshInspector(cx);
            })
          )
          .child(
            this.actionBtn(t, "ck-ins-doctor", "Doctor", (_e, cx) =>
              this.runDoctorCheck(cx)
            )
          )
          .child(
            this.actionBtn(t, "ck-ins-close", "×", () =>
              run("layout.hide", { id: "cockpit-inspector" })
            )
          )
      )
      .when(Boolean(this.note), (el) =>
        el.child(
          h_flex()
            .px(12)
            .py(5)
            .bg(t.raised)
            .border_b(1)
            .border_color(t.border)
            .text_size(11)
            .text_color(t.accent)
            .justify_between()
            .child(this.note)
            .child(
              div()
                .id("ck-clear-note")
                .cursor_pointer()
                .text_color(t.dim)
                .on_click((_e, cx) => {
                  this.note = "";
                  cx.notify();
                })
                .child("dismiss")
            )
        )
      )
      .child(
        v_flex()
          .flex_1()
          .min_h(0)
          .overflow_y_scrollbar()
          .p(12)
          .gap(8)
          .when(!agent, (el) =>
            el.child(
              cardBox().child(
                div()
                  .text_size(12)
                  .text_color(t.muted)
                  .child("Select an agent or open a terminal to inspect its context.")
              )
            )
          )
          .when(Boolean(agent), (el) =>
            el
              // 0. Supervisor Operator Action Card (top of inspector when pending)
              .when(Boolean(actionCard), (el2) =>
                el2
                  .child(sectionTitle("OPERATOR ACTION CARD"))
                  .child(
                    v_flex()
                      .p(11)
                      .gap(8)
                      .rounded(8)
                      .border(1)
                      .border_color(t.blocked)
                      .bg(t.blocked + "12")
                      .child(
                        h_flex()
                          .gap(6)
                          .items_center()
                          .justify_between()
                          .child(
                            h_flex()
                              .gap(6)
                              .items_center()
                              .child(
                                this.badge(
                                  t,
                                  urgencyLabel,
                                  urgencyColor,
                                  urgencyColor + "24"
                                )
                              )
                              .child(
                                this.badge(
                                  t,
                                  actionCard.detected_state ??
                                    "WAITING_CONFIRMATION",
                                  t.foreground,
                                  t.raised
                                )
                              )
                          )
                          .when(Boolean(actionCard.triggered_at), (el3) =>
                            el3.child(
                              div()
                                .text_size(10.5)
                                .text_color(t.dim)
                                .child(formatTimestamp(actionCard.triggered_at))
                            )
                          )
                      )
                      .child(
                        div()
                          .text_size(12)
                          .font_semibold()
                          .text_color(t.foreground)
                          .child(
                            actionCard.reason_for_operator ||
                              "Atenção do operador necessária."
                          )
                      )
                      .when(Boolean(actionCard.updated_summary), (el3) =>
                        el3.child(
                          div()
                            .text_size(11.5)
                            .text_color(t.muted)
                            .child(actionCard.updated_summary)
                        )
                      )
                      .child(
                        div()
                          .text_size(10.5)
                          .font_semibold()
                          .text_color(t.dim)
                          .child("Command Preview (clique para editar):")
                      )
                      .child(
                        div()
                          .id("ck-action-cmd-input")
                          .px(9)
                          .py(6)
                          .rounded(6)
                          .border(1)
                          .border_color(
                            this.editingField === "command"
                              ? t.accent
                              : t.border
                          )
                          .bg(t.background)
                          .text_size(12)
                          .text_color(
                            this.commandDraft ? t.foreground : t.dim
                          )
                          .cursor_text()
                          .on_click((_e, cx) => {
                            this.editingField = "command";
                            this.handle.focus();
                            cx.notify();
                          })
                          .child(
                            this.commandDraft
                              ? this.commandDraft +
                                  (this.editingField === "command" ? "▏" : "")
                              : (this.editingField === "command" ? "▏" : "") +
                                  "Digite o comando para enviar via hc…"
                          )
                      )
                      .child(
                        h_flex()
                          .gap(8)
                          .items_center()
                          .child(
                            this.actionBtn(
                              t,
                              "ck-action-approve",
                              "Aprovar e Enviar",
                              (_e, cx) =>
                                this.approveActionCard(cx, agent.address),
                              true
                            )
                          )
                          .child(
                            this.actionBtn(
                              t,
                              "ck-action-ignore",
                              "Ignorar",
                              (_e, cx) =>
                                this.ignoreActionCard(cx, agent.address),
                              false,
                              true
                            )
                          )
                      )
                  )
              )
              // 1. Agent Controls Card (Focus, Peek, Pin, Interrupt, Send Prompt)
              .child(sectionTitle("AGENT CONTROLS"))
              .child(
                cardBox()
                  .child(
                    h_flex()
                      .gap(6)
                      .flex_wrap()
                      .child(
                        this.actionBtn(
                          t,
                          "ck-ctl-focus",
                          "Focus Terminal",
                          () => {
                            if (agent.pane)
                              run("pane.focus", { pane: agent.pane });
                            else
                              run("agent.focus", { terminal: agent.address });
                          },
                          true
                        )
                      )
                      .child(
                        this.actionBtn(t, "ck-ctl-peek", "Peek", () =>
                          run("terminal.peek", { terminal: agent.address })
                        )
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-ctl-pin",
                          agent.pinned ? "Unpin" : "Pin",
                          () =>
                            run("terminal.pin", {
                              terminal: agent.address,
                              pinned: !agent.pinned,
                            })
                        )
                      )
                      .child(
                        this.actionBtn(t, "ck-ctl-int", "Interrupt", (_e, cx) => {
                          hc(["interrupt", agent.address]);
                          this.note = `Interrupted ${agent.address}`;
                          cx.notify();
                        })
                      )
                      .child(
                        this.actionBtn(t, "ck-ctl-split", "Split Shell", () => {
                          if (agent.pane)
                            run("pane.split", {
                              pane: agent.pane,
                              direction: "down",
                            });
                        })
                      )
                  )
                  .child(
                    h_flex()
                      .gap(6)
                      .pt(4)
                      .items_center()
                      .child(
                        div()
                          .id("ck-prompt-box")
                          .flex_1()
                          .px(9)
                          .py(5)
                          .rounded(6)
                          .border(1)
                          .border_color(
                            this.editingField === "prompt"
                              ? t.accent
                              : t.border
                          )
                          .bg(t.background)
                          .text_size(12)
                          .text_color(
                            this.promptText ? t.foreground : t.dim
                          )
                          .cursor_text()
                          .on_click((_e, cx) => {
                            this.editingField = "prompt";
                            this.handle.focus();
                            cx.notify();
                          })
                          .child(
                            this.promptText
                              ? this.promptText +
                                  (this.editingField === "prompt" ? "▏" : "")
                              : (this.editingField === "prompt" ? "▏" : "") +
                                  `Send message to ${agent.address}…`
                          )
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-prompt-send",
                          "Send ↵",
                          (_e, cx) => this.sendPromptGui(cx, agent.address),
                          Boolean(this.promptText.trim())
                        )
                      )
                  )
              )
              // 1b. Supervisor & Auto-Reply Audit Log
              .child(sectionTitle("SUPERVISOR & AUTO-REPLY AUDIT LOG"))
              .child(
                cardBox()
                  .child(
                    h_flex()
                      .justify_between()
                      .items_center()
                      .gap(6)
                      .child(
                        this.badge(
                          t,
                          auditBadge || "Auto-assisted (0/2)",
                          t.accent,
                          t.raised
                        )
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-audit-eval",
                          "⚡ Evaluate Now",
                          (_e, cx) => this.tickSupervisor(cx)
                        )
                      )
                  )
                  .when(Boolean(rollingSummary), (el2) =>
                    el2.child(
                      div()
                        .text_size(11.5)
                        .text_color(t.muted)
                        .child(rollingSummary)
                    )
                  )
                  .when(auditLog.length === 0, (el2) =>
                    el2.child(
                      div()
                        .text_size(11.5)
                        .text_color(t.dim)
                        .child("Nenhuma auto-resposta registrada para este agente.")
                    )
                  )
                  .children(
                    auditLog.map((entry, idx) =>
                      v_flex()
                        .id(`ck-audit-entry-${idx}`)
                        .p(7)
                        .gap(3)
                        .rounded(6)
                        .bg(t.raised)
                        .border(1)
                        .border_color(t.border)
                        .child(
                          h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                              div()
                                .text_size(11)
                                .font_semibold()
                                .text_color(t.accent)
                                .child(`hc send "${entry.payload ?? ""}"`)
                            )
                            .child(
                              div()
                                .text_size(10.5)
                                .text_color(t.dim)
                                .child(formatTimestamp(entry.timestamp))
                            )
                        )
                        .when(Boolean(entry.summary), (el2) =>
                          el2.child(
                            div()
                              .text_size(11)
                              .text_color(t.muted)
                              .child(entry.summary)
                          )
                        )
                    )
                  )
              )
              // 1c. Output Preview (Last 25 Lines)
              .when(cleanedPreviewLines.length > 0, (el) =>
                el
                  .child(sectionTitle("OUTPUT PREVIEW (LAST 25 LINES)"))
                  .child(
                    cardBox().child(
                      v_flex()
                        .p(8)
                        .gap(2)
                        .max_h(160)
                        .overflow_y_scrollbar()
                        .rounded(6)
                        .bg(t.background)
                        .border(1)
                        .border_color(t.border)
                        .children(
                          cleanedPreviewLines.map((line) =>
                            div()
                              .text_size(11)
                              .text_color(t.muted)
                              .truncate()
                              .child(line)
                          )
                        )
                    )
                  )
              )
              // 2. Provenance & Labels Management Card (GUI replacement for `cockpit label`)
              .child(sectionTitle("CORRELATION & LABELS"))
              .child(
                cardBox()
                  .child(
                    h_flex()
                      .justify_between()
                      .items_center()
                      .text_size(11.5)
                      .child(
                        div()
                          .text_color(t.muted)
                          .child(
                            `Branch: ${agent.branch ?? "none"}`
                          )
                      )
                      .child(
                        this.badge(
                          t,
                          (
                            detailed?.correlation_tier ?? "local_only"
                          ).toUpperCase(),
                          t.accent,
                          t.raised
                        )
                      )
                  )
                  .child(
                    div()
                      .truncate()
                      .text_size(11)
                      .text_color(t.dim)
                      .child(agent.cwd ?? "")
                  )
                  .child(
                    h_flex()
                      .gap(4)
                      .flex_wrap()
                      .children(
                        Object.entries(labels)
                          .filter(([k]) => k.startsWith("cockpit."))
                          .map(([k, v]) =>
                            h_flex()
                              .id(`ck-lbl-chip-${k}`)
                              .px(6)
                              .py(2)
                              .gap(5)
                              .items_center()
                              .rounded(5)
                              .bg(t.raised)
                              .border(1)
                              .border_color(t.border)
                              .text_size(11)
                              .child(`${k.replace("cockpit.", "")}=${v}`)
                              .child(
                                div()
                                  .id(`ck-lbl-del-${k}`)
                                  .cursor_pointer()
                                  .text_color(t.dim)
                                  .hover((s) => s.text_color(t.blocked))
                                  .on_click((_e, cx) =>
                                    this.applyLabelGui(cx, agent.address, k, "")
                                  )
                                  .child("×")
                              )
                          )
                      )
                  )
                  .child(
                    h_flex()
                      .gap(4)
                      .pt(4)
                      .items_center()
                      .children(
                        ["issue", "pr", "slack", "runbook"].map((k) =>
                          this.actionBtn(
                            t,
                            `ck-lbl-type-${k}`,
                            k,
                            (_e, cx) => {
                              this.labelKey = k;
                              this.editingField = "label";
                              this.handle.focus();
                              cx.notify();
                            },
                            this.labelKey === k
                          )
                        )
                      )
                  )
                  .child(
                    h_flex()
                      .gap(6)
                      .items_center()
                      .child(
                        div()
                          .id("ck-lbl-input")
                          .flex_1()
                          .px(8)
                          .py(4)
                          .rounded(6)
                          .border(1)
                          .border_color(
                            this.editingField === "label"
                              ? t.accent
                              : t.border
                          )
                          .bg(t.background)
                          .text_size(11.5)
                          .text_color(
                            this.labelValue ? t.foreground : t.dim
                          )
                          .cursor_text()
                          .on_click((_e, cx) => {
                            this.editingField = "label";
                            this.handle.focus();
                            cx.notify();
                          })
                          .child(
                            this.labelValue
                              ? `${this.labelKey}=${this.labelValue}` +
                                  (this.editingField === "label" ? "▏" : "")
                              : (this.editingField === "label" ? "▏" : "") +
                                  `Set cockpit.${this.labelKey} (e.g. ENG-42)…`
                          )
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-lbl-save",
                          "Tag",
                          (_e, cx) =>
                            this.applyLabelGui(
                              cx,
                              agent.address,
                              this.labelKey,
                              this.labelValue.trim()
                            ),
                          Boolean(this.labelValue.trim())
                        )
                      )
                  )
              )
              // 3. Pull Request / CI / Reviews / Deployments Card
              .child(sectionTitle("PULL REQUEST & CI CHECKS"))
              .child(
                pr
                  ? cardBox()
                      .child(
                        h_flex()
                          .justify_between()
                          .items_center()
                          .child(
                            div()
                              .flex_1()
                              .truncate()
                              .text_size(12)
                              .font_semibold()
                              .child(`#${pr.number} · ${pr.title}`)
                          )
                          .child(
                            this.actionBtn(
                              t,
                              "ck-open-pr",
                              "Open PR ↗",
                              () => openExternalUrl(pr.url)
                            )
                          )
                      )
                      .child(
                        h_flex()
                          .gap(6)
                          .items_center()
                          .child(
                            this.badge(
                              t,
                              pr.state ?? "OPEN",
                              t.foreground,
                              t.raised
                            )
                          )
                          .child(
                            this.badge(
                              t,
                              `CI: ${pr.ci_rollup ?? "PENDING"}`,
                              pr.ci_rollup === "FAILURE"
                                ? t.blocked
                                : t.done,
                              t.raised
                            )
                          )
                          .when(Boolean(pr.review_decision), (el2) =>
                            el2.child(
                              this.badge(
                                t,
                                pr.review_decision,
                                t.muted,
                                t.raised
                              )
                            )
                          )
                      )
                      .children(
                        (pr.checks ?? []).slice(0, 6).map((chk, idx) =>
                          h_flex()
                            .id(`ck-chk-${idx}`)
                            .justify_between()
                            .items_center()
                            .text_size(11)
                            .child(
                              div()
                                .flex_1()
                                .truncate()
                                .text_color(
                                  chk.status === "FAILURE"
                                    ? t.blocked
                                    : t.muted
                                )
                                .child(`• ${chk.name} (${chk.status})`)
                            )
                            .when(Boolean(chk.details_url), (el2) =>
                              el2.child(
                                div()
                                  .id(`ck-chk-url-${idx}`)
                                  .cursor_pointer()
                                  .text_color(t.accent)
                                  .on_click(() =>
                                    openExternalUrl(chk.details_url)
                                  )
                                  .child("logs ↗")
                              )
                            )
                        )
                      )
                  : cardBox().child(
                      div()
                        .text_size(11.5)
                        .text_color(t.dim)
                        .child("No open Pull/Merge Request linked to this branch.")
                    )
              )
              // 4. Tickets (Jira / Linear) & Slack Threads
              .child(sectionTitle("ISSUES & SLACK THREADS"))
              .child(
                cardBox()
                  .when(issues.length === 0 && slackThreads.length === 0, (e) =>
                    e.child(
                      div()
                        .text_size(11.5)
                        .text_color(t.dim)
                        .child(
                          "No linked Jira/Linear tickets or Slack threads. Tag an issue above or include PROJ-123 in your branch name."
                        )
                    )
                  )
                  .children(
                    issues.map((iss, i) =>
                      h_flex()
                        .id(`ck-iss-${i}`)
                        .justify_between()
                        .items_center()
                        .text_size(11.5)
                        .child(
                          div()
                            .flex_1()
                            .truncate()
                            .child(
                              `${iss.key} · ${iss.title} [${iss.status}]`
                            )
                        )
                        .when(Boolean(iss.url), (e) =>
                          e.child(
                            this.actionBtn(t, `ck-iss-open-${i}`, "Open ↗", () =>
                              openExternalUrl(iss.url)
                            )
                          )
                        )
                    )
                  )
                  .children(
                    slackThreads.map((th, i) =>
                      h_flex()
                        .id(`ck-slk-${i}`)
                        .justify_between()
                        .items_center()
                        .text_size(11.5)
                        .child(
                          div()
                            .flex_1()
                            .truncate()
                            .child(
                              `Slack #${th.channel_id} (${th.reply_count ?? 0} replies)`
                            )
                        )
                        .child(
                          this.actionBtn(t, `ck-slk-open-${i}`, "Thread ↗", () =>
                            openExternalUrl(th.url)
                          )
                        )
                    )
                  )
              )
              // 5. Local Markdown Runbooks
              .child(sectionTitle("LOCAL RUNBOOKS"))
              .child(
                cardBox()
                  .when(runbooks.length === 0, (e) =>
                    e.child(
                      div()
                        .text_size(11.5)
                        .text_color(t.dim)
                        .child(
                          "No matching RUNBOOK.md or docs/runbooks/*.md found."
                        )
                    )
                  )
                  .children(
                    runbooks.map((rb, i) =>
                      v_flex()
                        .id(`ck-rb-${i}`)
                        .gap(2)
                        .child(
                          h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                              div()
                                .text_size(11.5)
                                .font_semibold()
                                .child(rb.title)
                            )
                            .child(
                              div()
                                .text_size(10.5)
                                .text_color(t.dim)
                                .child(rb.match_reason ?? "")
                            )
                        )
                        .child(
                          div()
                            .truncate()
                            .text_size(10.5)
                            .text_color(t.muted)
                            .child(rb.path)
                        )
                    )
                  )
              )
              // 6. Cockpit System & Provider Health (GUI replacement for `cockpit doctor` / `cockpit setup`)
              .child(sectionTitle("COCKPIT SYSTEM & PROVIDERS"))
              .child(
                cardBox()
                  .child(
                    h_flex()
                      .gap(6)
                      .flex_wrap()
                      .child(
                        this.actionBtn(
                          t,
                          "ck-sys-setup",
                          "Verify / Repair Hooks",
                          (_e, cx) => this.runSetupGui(cx)
                        )
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-sys-files",
                          "Files / Diff Panel",
                          () => run("layout.toggle", { id: "panel" })
                        )
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-sys-layout",
                          "Switch Layout",
                          () => run("layout.switch")
                        )
                      )
                  )
                  .children(
                    Object.entries(providers).map(([prov, ph]) =>
                      h_flex()
                        .justify_between()
                        .items_center()
                        .text_size(11)
                        .child(div().text_color(t.muted).child(prov))
                        .child(
                          div()
                            .text_color(
                              ph?.state === "active"
                                ? t.done
                                : ph?.state === "inactive"
                                  ? t.dim
                                  : t.blocked
                            )
                            .child((ph?.state ?? "unknown").toUpperCase())
                        )
                    )
                  )
              )
          )
      );
  }

  renderStatus() {
    const t = theme();
    const space = spaces().find((s) => s.active);
    const list = agents();
    const snap = shared("cockpit.snapshot");
    const supSnap = shared("supervisor.snapshot");
    const providers = snap?.providers ?? {};
    const blockedCount = list.filter((a) => a.status === "blocked").length;
    const workingCount = list.filter((a) => a.status === "working").length;
    const modeLabel = supSnap?.mode_label ?? "Semi-Auto (Whitelist)";
    const vramText = supSnap?.model_loaded
      ? "CPU-only (num_gpu: 0) · RAM ~1.2 GB"
      : "CPU-only (num_gpu: 0) · 0 MB";
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );
    const mac = win()?.platform === "macos";
    const b = keys().find(
      (k) => k.section === "global" && k.action === "app.palette"
    );
    const paletteHint = b ? b.keys.toUpperCase() : mac ? "⌘K" : "CTRL+SHIFT+P";

    return h_flex()
      .size_full()
      .px(14)
      .gap(12)
      .items_center()
      .bg(t.chrome)
      .border_t(1)
      .border_color(t.border)
      .text_size(11.5)
      .text_color(t.muted)
      .child(
        h_flex()
          .gap(6)
          .items_center()
          .child(
            div()
              .size(7)
              .rounded(4)
              .bg(blockedCount > 0 ? t.blocked : t.done)
          )
          .child(
            div()
              .font_semibold()
              .text_color(t.foreground)
              .child("Cockpit")
          )
      )
      .child(
        div()
          .id("ck-status-mode")
          .px(6)
          .py(1)
          .rounded(4)
          .bg(t.raised)
          .cursor_pointer()
          .hover((s) => s.bg(t.hover).text_color(t.foreground))
          .on_click((_e, cx) =>
            this.runSupervisorAction(cx, ["supervisor", "mode", "cycle"])
          )
          .child(modeLabel)
      )
      .child(div().text_color(t.dim).child(vramText))
      .child(div().child(space?.machine ?? "Local machine"))
      .when(Boolean(space?.branch), (el) =>
        el.child(div().child(`⎇ ${space.branch}`))
      )
      .child(
        div().child(
          `${list.length} agents (${workingCount} working)`
        )
      )
      .when(blockedCount > 0, (el) =>
        el.child(
          div()
            .px(6)
            .py(1)
            .rounded(4)
            .bg(t.blocked + "26")
            .text_color(t.blocked)
            .font_semibold()
            .child(`● ${blockedCount} needs you`)
        )
      )
      .children(
        Object.entries(providers).map(([prov, ph]) =>
          this.badge(
            t,
            `${prov}: ${(ph?.state ?? "unknown").toUpperCase()}`,
            ph?.state === "active"
              ? t.done
              : ph?.state === "inactive"
                ? t.dim
                : t.blocked,
            t.raised
          )
        )
      )
      .child(div().flex_1())
      .child(
        div()
          .id("ck-status-inspector")
          .cursor_pointer()
          .text_color(inspectorHidden ? t.muted : t.accent)
          .hover((s) => s.text_color(t.foreground))
          .on_click(() => run("layout.toggle", { id: "cockpit-inspector" }))
          .child(inspectorHidden ? "Show Inspector" : "Hide Inspector")
      )
      .child(
        div()
          .id("ck-status-palette")
          .cursor_pointer()
          .text_color(t.dim)
          .hover((s) => s.text_color(t.foreground))
          .on_click(() => run("app.palette"))
          .child(`${paletteHint} commands`)
      );
  }

  render(cx) {
    const role = props()?.role ?? "fleet";
    if (role === "pill") return this.renderPill(cx);
    if (role === "inspector") return this.renderInspector(cx);
    if (role === "status") return this.renderStatus();
    return this.renderFleet(cx);
  }
}
