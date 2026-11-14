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
  { key: "alert-only", match: "AlertOnly", label: "Alert Only" },
  { key: "paused", match: "Paused", label: "Paused" },
];

function withAlpha(color, alphaHex, fallback) {
  if (typeof color === "string") {
    if (/^#[0-9a-fA-F]{6}$/.test(color)) {
      return `${color}${alphaHex}`;
    }
    if (/^#[0-9a-fA-F]{8}$/.test(color)) {
      return `${color.slice(0, 7)}${alphaHex}`;
    }
  }
  return fallback ?? color;
}

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

function fmtBytes(n) {
  const b = Number(n) || 0;
  if (b >= 1e9) return `${(b / 1e9).toFixed(1)} GB`;
  return `${Math.round(b / 1e6)} MB`;
}

// Compact LLM residency label from /api/ps: "1.3 GB RAM" or "1.1 GB VRAM · 0.2 GB RAM".
// Returns "" when the snapshot carries no usage numbers (non-Ollama backend / older binary).
function llmMemoryLabel(snap) {
  const ram = snap?.llm_ram_bytes;
  const vram = snap?.llm_vram_bytes;
  if (ram == null && vram == null) return "";
  const parts = [];
  if (Number(vram) > 0) parts.push(`${fmtBytes(vram)} VRAM`);
  if (Number(ram) > 0 || parts.length === 0) parts.push(`${fmtBytes(ram)} RAM`);
  return parts.join(" · ");
}

function gpuOn(snap) {
  return Boolean(snap?.allow_gpu) || Number(snap?.num_gpu ?? 0) !== 0;
}

function gpuPillLabel(snap) {
  return gpuOn(snap) ? "num_gpu: all (GPU)" : "num_gpu: 0 (CPU)";
}

function gpuModeText(snap) {
  return gpuOn(snap) ? "num_gpu=all (GPU offload)" : "num_gpu=0 (CPU-only, 0 MB VRAM)";
}

function parseSnapshotOutput(stdout) {
  if (!stdout) return null;
  const parsed = JSON.parse(stdout);
  return parsed?.snapshot ?? parsed ?? null;
}

/**
 * Truncates long strings in JS so text inside GPUI `v_flex()` never gets clipped vertically by `.truncate()`.
 */
function ellipsize(str, maxLen = 72) {
  const s = String(str ?? "");
  return s.length > maxLen ? s.slice(0, maxLen - 1) + "…" : s;
}

function pathBasename(p) {
  const clean = String(p ?? "").replace(/[\\/]+$/, "");
  if (!clean) return "";
  const parts = clean.split(/[\\/]/);
  return parts[parts.length - 1] || clean;
}

function extractRepoName(cwd, fallback = "") {
  const base = pathBasename(cwd);
  return base || fallback;
}

const KNOWN_AGENTS = new Set([
  "claude",
  "codex",
  "pi",
  "opencode",
  "aider",
  "goose",
  "cursor",
  "amp",
]);

function resolveProgramName(rawProg, labels, enrichedKind) {
  const labelAgent = String(labels?.["cockpit.agent"] ?? "")
    .trim()
    .toLowerCase();
  const rawCandidate =
    labelAgent ||
    (typeof rawProg === "string"
      ? rawProg
      : rawProg?.name ?? rawProg?.command ?? enrichedKind ?? "");
  const firstToken = String(rawCandidate ?? "")
    .trim()
    .split(/\s+/)[0]
    .split(/[\\/]/)
    .pop()
    .replace(/^-/, "")
    .toLowerCase();
  if (!firstToken) return "";
  for (const known of KNOWN_AGENTS) {
    if (firstToken === known || firstToken.includes(known)) {
      return known;
    }
  }
  return firstToken;
}

function resolveDisplayName(rawName, projectName, address) {
  const nameStr = String(rawName ?? "").trim();
  const isTerminalId =
    !nameStr ||
    nameStr === address ||
    /^t\d+[a-z]*$/i.test(nameStr) ||
    /^[\w-]+\/t\d+[a-z]*$/i.test(nameStr);
  if (isTerminalId && projectName && projectName !== address) {
    return projectName;
  }
  return nameStr || projectName || address;
}

/**
 * Merges live Herdr `spaces()` with `homeSnap.projects` and live agent counts.
 */
export function buildProjectsList(
  homeSnap,
  liveSpaces = [],
  allTerms = [],
  snap = null,
  supSnap = null
) {
  const mergedFleet = mergeFleetRows(agents(), allTerms, snap, supSnap);
  const tasks = homeSnap?.tasks ?? [];
  const homeProjects = homeSnap?.projects ?? [];
  const byKey = new Map();

  const normalizeKey = (name, p) => {
    const base = pathBasename(p);
    return (name || base || p || "").trim().toLowerCase();
  };

  for (const proj of homeProjects) {
    const key = normalizeKey(proj.name, proj.path);
    if (!key) continue;
    byKey.set(key, {
      name: proj.name || pathBasename(proj.path) || "Project",
      spaceId: null,
      isActiveSpace: false,
      branch: proj.git_branch ?? null,
      path: proj.path ?? "",
      dirtyCount: proj.git_dirty_count ?? 0,
      lastCommit: proj.last_commit_subject ?? null,
      activeAgents: proj.active_agents ?? 0,
      blockedAgents: proj.blocked_agents ?? 0,
      openTasks: proj.open_tasks ?? 0,
      status: "idle",
    });
  }

  for (const sp of liveSpaces) {
    const spName = sp?.name || pathBasename(sp?.cwd) || "Space";
    const keyByName = spName.trim().toLowerCase();
    const keyByCwd = pathBasename(sp?.cwd).trim().toLowerCase();
    const existing =
      byKey.get(keyByName) || (keyByCwd ? byKey.get(keyByCwd) : null);

    if (existing) {
      existing.spaceId = sp?.id ?? existing.spaceId;
      existing.isActiveSpace = existing.isActiveSpace || Boolean(sp?.active);
      existing.branch = sp?.branch ?? existing.branch;
      if (!existing.path && sp?.cwd) existing.path = sp.cwd;
    } else {
      byKey.set(keyByName || sp?.id || spName, {
        name: spName,
        spaceId: sp?.id ?? null,
        isActiveSpace: Boolean(sp?.active),
        branch: sp?.branch ?? null,
        path: sp?.cwd ?? "",
        dirtyCount: 0,
        lastCommit: null,
        activeAgents: 0,
        blockedAgents: 0,
        openTasks: 0,
        status: "idle",
      });
    }
  }

  const list = Array.from(byKey.values()).map((proj) => {
    const pKey = proj.name.trim().toLowerCase();
    const pBase = pathBasename(proj.path).trim().toLowerCase();

    const matchingAgents = mergedFleet.filter((a) => {
      const aSpace = String(a.space_name ?? "").trim().toLowerCase();
      const aBase = pathBasename(a.cwd).trim().toLowerCase();
      return (
        (proj.spaceId && a.space_id === proj.spaceId) ||
        (aSpace && aSpace === pKey) ||
        (aBase && pBase && aBase === pBase)
      );
    });

    const liveActive = matchingAgents.filter(
      (a) => a.lifecycle?.state !== "finished" && a.status !== "finished"
    ).length;
    const liveBlocked = matchingAgents.filter(
      (a) => a.status === "blocked" || Boolean(a.action_card)
    ).length;

    const matchingOpenTasks = tasks.filter(
      (t) =>
        !t.done &&
        t.project &&
        String(t.project).trim().toLowerCase() === pKey
    ).length;

    const activeAgents = Math.max(proj.activeAgents, liveActive);
    const blockedAgents = Math.max(proj.blockedAgents, liveBlocked);
    const openTasks =
      tasks.length > 0 ? matchingOpenTasks : proj.openTasks;

    const status =
      blockedAgents > 0
        ? "blocked"
        : activeAgents > 0
          ? "active"
          : openTasks > 0
            ? "open_tasks"
            : "idle";

    return {
      ...proj,
      activeAgents,
      blockedAgents,
      openTasks,
      status,
    };
  });

  const statusRank = { blocked: 0, active: 1, open_tasks: 2, idle: 3 };
  list.sort((a, b) => {
    if (a.isActiveSpace !== b.isActiveSpace) return a.isActiveSpace ? -1 : 1;
    const ra = statusRank[a.status] ?? 3;
    const rb = statusRank[b.status] ?? 3;
    if (ra !== rb) return ra - rb;
    return a.name.localeCompare(b.name);
  });

  return list;
}

function normProgToken(val) {
  return String(val ?? "")
    .trim()
    .toLowerCase()
    .split(/[\\/]/)
    .pop()
    .replace(/^-/, "");
}

function extractAttachTargetFromArgs(args, fallbackProgram = null) {
  if (!Array.isArray(args) || args.length === 0) return null;
  const first = normProgToken(args[0]);
  const fb = normProgToken(fallbackProgram);
  const isAttachCli =
    first === "herdr-core" ||
    first === "hc" ||
    fb === "herdr-core" ||
    fb === "hc";
  if (!isAttachCli) return null;

  let attachIdx = -1;
  for (let i = 0; i < args.length; i += 1) {
    const raw = String(args[i] ?? "").trim();
    if (raw === "--") break;
    if (normProgToken(raw) === "attach") {
      attachIdx = i;
      break;
    }
  }
  if (attachIdx < 0) return null;

  for (let i = attachIdx + 1; i < args.length; i += 1) {
    const raw = String(args[i] ?? "").trim();
    if (!raw || raw === "--" || raw.startsWith("-")) continue;
    return raw;
  }
  return "";
}

function extractAttachTargetFromTerminal(term, programName = null) {
  if (!term) return null;
  const fgTarget = extractAttachTargetFromArgs(
    term?.foreground?.argv,
    term?.foreground?.name ?? programName
  );
  if (fgTarget !== null) return fgTarget;
  return extractAttachTargetFromArgs(term?.argv, term?.program ?? programName);
}

function isPlainShellRow(name, programName, labels, term = null) {
  const norm = normProgToken;
  if (extractAttachTargetFromTerminal(term, programName) !== null) {
    return true;
  }
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
    "herdr-core",
    "hc",
  ]);
  const fgShells = new Set([
    "zsh",
    "bash",
    "fish",
    "sh",
    "nu",
    "nushell",
    "dash",
    "herdr-core",
    "hc",
  ]);
  const knownAgents = new Set([
    "claude",
    "claude-code",
    "claudecode",
    "opencode",
    "pi",
    "codex",
    "aider",
    "goose",
    "cursor",
    "cursor-agent",
    "amp",
  ]);

  const fgName = norm(term?.foreground?.name);
  if (fgName && fgShells.has(fgName)) {
    const fgArgv = Array.isArray(term?.foreground?.argv)
      ? term.foreground.argv
      : [];
    const hasKnownAgentArg = fgArgv.some((arg) => knownAgents.has(norm(arg)));
    const stState = String(term?.status?.state ?? "").trim().toLowerCase();
    const stApp = norm(term?.status?.app);
    const hasActiveNonShellStatus =
      ["working", "blocked", "done", "error"].includes(stState) &&
      Boolean(stApp) &&
      !plainShells.has(stApp);
    if (!hasKnownAgentArg && !hasActiveNonShellStatus) {
      return true;
    }
  }

  const agentLabel = String(labels?.["cockpit.agent"] ?? "")
    .trim()
    .toLowerCase();
  if (agentLabel && agentLabel !== "shell") {
    return false;
  }
  const baseProg = norm(programName);
  if (baseProg && plainShells.has(baseProg)) {
    return true;
  }
  return /\((zsh|bash|fish|sh|nu|dash|tmux)\)$/i.test(String(name ?? "").trim());
}

async function delayMs(ms) {
  if (typeof globalThis.setTimeout === "function") {
    return new Promise((resolve) => globalThis.setTimeout(resolve, ms));
  }
  const secs = String(Math.max(0.05, ms / 1000).toFixed(2));
  await exec("sleep", [secs], { timeout: Math.max(1000, ms + 500) }).catch(
    () => null
  );
}

function resolveCoreTermName(term, fallbackName, address) {
  const direct = String(term?.name ?? "").trim();
  if (direct && direct !== address) return direct;
  const fb = String(fallbackName ?? "").trim();
  if (
    fb &&
    fb !== address &&
    !fb.includes(" / ") &&
    !/\s\([^)]+\)$/.test(fb) &&
    !fb.includes(" ")
  ) {
    return fb;
  }
  return direct || null;
}

function findPaneForTerminal(
  allPanes,
  allTerminals,
  address,
  termName = null,
  knownPaneId = null
) {
  const paneList = Array.isArray(allPanes) ? allPanes : [];
  if (knownPaneId) {
    const byId = paneList.find((p) => (p.pane ?? p.id) === knownPaneId);
    if (byId) return byId;
  }
  const direct = paneList.find((p) => p.terminal === address);
  if (direct) return direct;

  const termList = Array.isArray(allTerminals) ? allTerminals : [];
  const attachMatchesTarget = (target) => {
    if (!target) return false;
    const shortTarget = target.includes("/") ? target.split("/").pop() : target;
    return (
      target === address ||
      shortTarget === address ||
      (Boolean(termName) && (target === termName || shortTarget === termName))
    );
  };
  const wrapperAddrs = new Set(
    termList
      .filter((t) => attachMatchesTarget(extractAttachTargetFromTerminal(t)))
      .map((t) => t.address)
  );
  if (wrapperAddrs.size > 0) {
    const wrappedPane = paneList.find((p) => wrapperAddrs.has(p.terminal));
    if (wrappedPane) return wrappedPane;
  }

  return knownPaneId ? { pane: knownPaneId } : null;
}

function resolveAttachedTargetAddress(
  wrapperTermAddr,
  allTerminals,
  extraAgents = []
) {
  if (!wrapperTermAddr) return wrapperTermAddr;
  const termList = Array.isArray(allTerminals) ? allTerminals : [];
  const wrapper = termList.find((t) => t.address === wrapperTermAddr);
  const rawTarget = extractAttachTargetFromTerminal(wrapper);
  if (!rawTarget) return wrapperTermAddr;

  const shortTarget = rawTarget.includes("/")
    ? rawTarget.split("/").pop()
    : rawTarget;
  const matchedTerm = termList.find(
    (t) =>
      t &&
      t.address !== wrapperTermAddr &&
      extractAttachTargetFromTerminal(t) === null &&
      (t.address === rawTarget ||
        t.address === shortTarget ||
        t.name === rawTarget ||
        t.name === shortTarget)
  );
  if (matchedTerm?.address) return matchedTerm.address;

  const matchedExtra = (Array.isArray(extraAgents) ? extraAgents : []).find(
    (a) =>
      a &&
      a.address !== wrapperTermAddr &&
      (a.address === rawTarget ||
        a.address === shortTarget ||
        a.name === rawTarget ||
        a.name === shortTarget ||
        a.term_name === rawTarget ||
        a.term_name === shortTarget ||
        a.attach_target === rawTarget ||
        a.attach_target === shortTarget)
  );
  return matchedExtra?.address ?? wrapperTermAddr;
}

export function mergeFleetRows(liveAgents, allTerminals, snap, supSnap = null) {
  const snapMap = new Map(
    (snap?.agents ?? []).map((a) => [a.address, a])
  );
  const supMap = new Map(
    (supSnap?.agents ?? []).map((a) => [a.agent_id, a])
  );
  const hasLiveTermList = Array.isArray(allTerminals) && allTerminals.length > 0;
  const termMap = new Map(allTerminals.map((t) => [t.address, t]));
  const allPanes = panes() ?? [];
  const seen = new Set();
  const rows = [];

  for (const a of liveAgents) {
    const term = termMap.get(a.address) ?? {};
    const enriched = snapMap.get(a.address) ?? {};
    const sup = supMap.get(a.address) ?? null;
    const rawName =
      enriched.name ||
      sup?.session_name ||
      a.tab_name ||
      a.title ||
      term.name ||
      a.address;
    const rowProgram = a.program ?? term.program;
    const rowLabels = term?.labels ?? {};
    if (isPlainShellRow(rawName, rowProgram, rowLabels, term)) {
      continue;
    }
    seen.add(a.address);
    const termName = resolveCoreTermName(
      term,
      enriched.name || a.tab_name || a.title,
      a.address
    );
    const pane = findPaneForTerminal(
      allPanes,
      allTerminals,
      a.address,
      termName,
      a.pane
    );
    const resolvedCwd =
      (pane?.terminal === a.address ? pane?.cwd : null) ??
      enriched?.workspace?.cwd ??
      term?.cwd ??
      pane?.cwd ??
      null;
    const projectName =
      a.space_name || extractRepoName(resolvedCwd, "") || rawName;
    const programLabel = resolveProgramName(
      rowProgram,
      rowLabels,
      enriched?.agent_kind
    );
    const displayName = resolveDisplayName(rawName, projectName, a.address);
    const isFinished =
      term?.state === "finished" || enriched?.lifecycle?.state === "finished";
    const baseStatus =
      a.status && a.status !== "idle"
        ? a.status
        : enriched?.attention_state?.state &&
            enriched.attention_state.state !== "idle"
          ? enriched.attention_state.state
          : rowLabels?.["cockpit.state"] || a.status || "idle";
    const status = isFinished
      ? "done"
      : sup?.action_card
        ? "blocked"
        : baseStatus;
    rows.push({
      address: a.address,
      pane: a.pane ?? pane?.pane ?? null,
      term_name: termName,
      attach_target: termName || a.address,
      space_name: a.space_name ?? "",
      tab_name: a.tab_name ?? a.title ?? a.address,
      name: displayName,
      project_name: projectName,
      program_label: programLabel,
      status,
      kind: isFinished
        ? null
        : enriched?.attention_state?.kind ??
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
      cwd: resolvedCwd,
      labels: rowLabels,
      correlation_tier: enriched?.correlation_tier ?? null,
      context: enriched?.context ?? null,
      blocked_children: enriched?.blocked_children ?? [],
      lifecycle: isFinished ? { state: "finished" } : { state: "live" },
      focused: Boolean(a.focused),
      pinned: Boolean(a.pinned),
      seq: a.seq ?? 0,
      sup_lifecycle: sup?.lifecycle_state ?? null,
      action_card: isFinished ? null : (sup?.action_card ?? null),
      audit_badge: sup?.audit_badge ?? null,
      auto_reply_log: sup?.auto_reply_log ?? [],
      rolling_summary: sup?.rolling_summary ?? null,
      last_output_preview: sup?.last_output_preview ?? "",
    });
  }

  for (const sa of snap?.agents ?? []) {
    if (seen.has(sa.address)) continue;
    if (hasLiveTermList && !termMap.has(sa.address)) continue;
    const term = termMap.get(sa.address) ?? {};
    const sup = supMap.get(sa.address) ?? null;
    const rawName = sa.name || sup?.session_name || sa.address;
    const rowLabels = term?.labels ?? {};
    const isFinished =
      sa?.lifecycle?.state === "finished" || term?.state === "finished";
    if (!isFinished && isPlainShellRow(rawName, term.program, rowLabels, term)) {
      continue;
    }
    seen.add(sa.address);
    const termName = resolveCoreTermName(term, sa.name, sa.address);
    const pane = findPaneForTerminal(
      allPanes,
      allTerminals,
      sa.address,
      termName,
      null
    );
    const resolvedCwd =
      sa?.workspace?.cwd ?? term?.cwd ?? pane?.cwd ?? null;
    const projectName = extractRepoName(resolvedCwd, "") || rawName;
    const programLabel = resolveProgramName(
      term.program,
      rowLabels,
      sa?.agent_kind
    );
    const displayName = resolveDisplayName(rawName, projectName, sa.address);
    const baseStatus = sa?.attention_state?.state ?? "idle";
    const status = isFinished
      ? "done"
      : sup?.action_card
        ? "blocked"
        : baseStatus;
    rows.push({
      address: sa.address,
      pane: pane?.pane ?? null,
      term_name: termName,
      attach_target: termName || sa.address,
      space_name: "",
      tab_name: rawName,
      name: displayName,
      project_name: projectName,
      program_label: programLabel,
      status,
      kind: isFinished
        ? null
        : sa?.attention_state?.kind ??
          (sup?.action_card ? "permission" : null),
      message:
        sa?.attention_state?.message ??
        sup?.action_card?.reason_for_operator ??
        null,
      program: term.program ?? null,
      branch: sa?.workspace?.git_branch ?? pane?.branch ?? null,
      cwd: resolvedCwd,
      labels: rowLabels,
      correlation_tier: sa?.correlation_tier ?? null,
      context: sa?.context ?? null,
      blocked_children: sa?.blocked_children ?? [],
      lifecycle: isFinished
        ? { state: "finished" }
        : (sa?.lifecycle ?? { state: term.state ?? "live" }),
      focused: false,
      pinned: false,
      seq: 0,
      sup_lifecycle: sup?.lifecycle_state ?? null,
      action_card: isFinished ? null : (sup?.action_card ?? null),
      audit_badge: sup?.audit_badge ?? null,
      auto_reply_log: sup?.auto_reply_log ?? [],
      rolling_summary: sup?.rolling_summary ?? null,
      last_output_preview: sup?.last_output_preview ?? "",
    });
  }

  for (const sup of supSnap?.agents ?? []) {
    if (seen.has(sup.agent_id)) continue;
    if (hasLiveTermList && !termMap.has(sup.agent_id)) continue;
    const term = termMap.get(sup.agent_id) ?? {};
    if (term?.state === "finished" || sup.lifecycle_state === "COMPLETED") {
      continue;
    }
    const rawName = sup.session_name || term.name || sup.agent_id;
    const rowProgram = term.program ?? null;
    const rowLabels = term?.labels ?? {};
    if (isPlainShellRow(rawName, rowProgram, rowLabels, term)) {
      continue;
    }
    if (
      !liveAgents.some((a) => a.address === sup.agent_id) &&
      !snapMap.has(sup.agent_id)
    ) {
      continue;
    }
    seen.add(sup.agent_id);
    const termName = resolveCoreTermName(term, sup.session_name, sup.agent_id);
    const pane = findPaneForTerminal(
      allPanes,
      allTerminals,
      sup.agent_id,
      termName,
      null
    );
    const resolvedCwd = term?.cwd ?? pane?.cwd ?? null;
    const projectName = extractRepoName(resolvedCwd, "") || rawName;
    const programLabel = resolveProgramName(rowProgram, rowLabels, null);
    const displayName = resolveDisplayName(rawName, projectName, sup.agent_id);
    rows.push({
      address: sup.agent_id,
      pane: pane?.pane ?? null,
      term_name: termName,
      attach_target: termName || sup.agent_id,
      space_name: "",
      tab_name: rawName,
      name: displayName,
      project_name: projectName,
      program_label: programLabel,
      status: sup.action_card ? "blocked" : "working",
      kind: sup.action_card ? "permission" : null,
      message: sup.action_card?.reason_for_operator ?? null,
      program: rowProgram,
      branch: pane?.branch ?? null,
      cwd: resolvedCwd,
      labels: rowLabels,
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

  for (const term of allTerminals ?? []) {
    const addr = term?.address;
    if (!addr || seen.has(addr)) continue;
    if (term?.state === "finished") continue;
    const rawName = term.name || term.title || addr;
    const rowProgram = term.program ?? term?.foreground?.name ?? null;
    const rowLabels = term?.labels ?? {};
    if (isPlainShellRow(rawName, rowProgram, rowLabels, term)) {
      continue;
    }
    seen.add(addr);
    const termName = resolveCoreTermName(term, term.name || term.title, addr);
    const pane = findPaneForTerminal(
      allPanes,
      allTerminals,
      addr,
      termName,
      null
    );
    const resolvedCwd = term?.cwd ?? pane?.cwd ?? null;
    const projectName = extractRepoName(resolvedCwd, "") || rawName;
    const programLabel = resolveProgramName(rowProgram, rowLabels, null);
    const displayName = resolveDisplayName(rawName, projectName, addr);
    const rawTermStatus =
      typeof term?.status === "string" ? term.status : term?.status?.state;
    const status =
      rawTermStatus ||
      term?.report?.state ||
      rowLabels?.["cockpit.state"] ||
      "idle";
    rows.push({
      address: addr,
      pane: pane?.pane ?? null,
      term_name: termName,
      attach_target: termName || addr,
      space_name: "",
      tab_name: rawName,
      name: displayName,
      project_name: projectName,
      program_label: programLabel,
      status,
      kind: term?.report?.kind ?? term?.status?.kind ?? null,
      message: term?.report?.msg ?? term?.status?.msg ?? null,
      program: rowProgram,
      branch: pane?.branch ?? null,
      cwd: resolvedCwd,
      labels: rowLabels,
      correlation_tier: null,
      context: null,
      blocked_children: [],
      lifecycle: { state: term?.state ?? "live" },
      focused: false,
      pinned: false,
      seq: 0,
      sup_lifecycle: null,
      action_card: null,
      audit_badge: null,
      auto_reply_log: [],
      rolling_summary: null,
      last_output_preview: "",
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
    this.fetchingHome = false;
    this.note = "";
    this.promptText = "";
    this.labelKey = "issue";
    this.labelValue = "";
    this.commandDraft = "";
    this.commandDraftKey = "";
    this.editingField = null; // "prompt" | "label" | "command" | "task" | null
    this.inspectCache = {}; // address -> InspectResponse
    this.doctorReport = null;
    this.homeSnap = shared("cockpit.home") ?? null;
    this.usageSnap = shared("cockpit.usage") ?? null;
    this.fetchingUsage = false;
    this.usageFetchedAtMs = Number(shared("cockpit.usageFetchedAtMs") ?? 0);
    this.usageOpen = Boolean(shared("cockpit.usageOpen"));
    this.usageOnlyDrawer = Boolean(shared("cockpit.usageOnlyDrawer"));
    this.usageMenuOpen = false;
    this.selectedUsageEntryId = shared("cockpit.usageEntryId") ?? null;
    this.taskDraft = "";
    this.taskPriority = "normal";
    this.taskProject = "";
    this.taskFilter = "pending"; // "pending" | "all" | "done"
    const initialViewMode = shared("cockpit.viewMode");
    this.inspectorTab =
      initialViewMode && initialViewMode !== "usage"
        ? initialViewMode
        : "inspector"; // "home" | "inspector" | "doctor"
    this.handle = cx.focus_handle();

    const role = props()?.role ?? "fleet";
    if (role === "fleet") {
      this.refreshCockpit(cx, false);
      this.fetchSupervisorStatus(cx);
      this.fetchHomeSnapshot(cx);
      cx.timer.every(5000, (cx) => this.refreshCockpit(cx, false));
      cx.timer.every(12000, (cx) => this.tickSupervisor(cx));
    } else if (role === "home") {
      this.fetchHomeSnapshot(cx);
      if (!shared("supervisor.snapshot")) {
        this.fetchSupervisorStatus(cx);
      }
      cx.timer.every(8000, (cx) => this.fetchHomeSnapshot(cx));
    } else if (role === "inspector") {
      this.refreshInspector(cx);
      this.fetchHomeSnapshot(cx);
      this.fetchUsageSnapshot(cx, false, false);
      this.runDoctorCheckSilent(cx);
      if (!shared("supervisor.snapshot")) {
        this.fetchSupervisorStatus(cx);
      }
      cx.timer.every(6000, (cx) => this.refreshInspector(cx));
      cx.timer.every(60000, (cx) => {
        const inspectorHidden = Boolean(
          findNode(layout(), "cockpit-inspector")?.hidden
        );
        if (Boolean(shared("cockpit.usageOpen")) && !inspectorHidden) {
          this.fetchUsageSnapshot(cx, false, false);
        }
      });
    } else if (role === "pill" || role === "status") {
      if (!shared("supervisor.snapshot")) {
        this.fetchSupervisorStatus(cx);
      }
    }
  }

  selectedAddress() {
    const f = focused();
    const rawFTerm = f?.terminal ?? null;
    const fTerm = rawFTerm
      ? resolveAttachedTargetAddress(
          rawFTerm,
          terminals() ?? [],
          shared("cockpit.snapshot")?.agents ?? []
        )
      : null;
    const lastFocus = shared("cockpit.lastFocusedTerminal") ?? null;
    if (fTerm && fTerm !== lastFocus) {
      setShared("cockpit.lastFocusedTerminal", fTerm);
      setShared("cockpit.selected", fTerm);
      return fTerm;
    }
    const sharedSel = shared("cockpit.selected");
    if (typeof sharedSel === "string" && sharedSel) return sharedSel;
    if (fTerm) return fTerm;
    const first = agents()[0]?.address;
    if (first) return first;
    const supSnap = shared("supervisor.snapshot");
    const firstAlert = (supSnap?.agents ?? []).find((a) => a.action_card)?.agent_id;
    return firstAlert ?? supSnap?.agents?.[0]?.agent_id ?? null;
  }

  async focusOrAttachTerminal(cx, agentRow, forceAttach = false) {
    const addr = agentRow?.address;
    if (!addr) return;
    const allTerms = terminals() ?? [];
    const allPanes = panes() ?? [];
    const term = allTerms.find((t) => t.address === addr);
    const termName =
      agentRow.term_name ??
      resolveCoreTermName(term, agentRow.tab_name ?? agentRow.name, addr);
    const attachTarget = agentRow.attach_target || termName || addr;

    const openAttachTab = () => {
      this.attachingTargets ??= new Map();
      const now = Date.now();
      const lastAttempt = this.attachingTargets.get(addr) ?? 0;
      if (now - lastAttempt < 1200) return;
      this.attachingTargets.set(addr, now);
      const sp = (spaces() ?? []).find((s) => s.active);
      run("tab.new", {
        space: sp?.id,
        name: termName || addr,
        argv: ["herdr-core", "attach", attachTarget],
      });
      cx?.notify?.();
    };

    if (forceAttach) {
      openAttachTab();
      return;
    }

    const livePane = findPaneForTerminal(
      allPanes,
      allTerms,
      addr,
      termName,
      agentRow.pane ?? null
    );
    if (livePane?.pane) {
      run("pane.focus", { pane: livePane.pane });
    } else {
      run("agent.focus", { terminal: addr });
    }

    await delayMs(200);

    const termsAfter = terminals() ?? [];
    const panesAfter = panes() ?? [];
    const paneAfter = findPaneForTerminal(
      panesAfter,
      termsAfter,
      addr,
      termName,
      null
    );
    const rawFocusedTerm = focused()?.terminal ?? null;
    const mappedFocused = rawFocusedTerm
      ? resolveAttachedTargetAddress(
          rawFocusedTerm,
          termsAfter,
          shared("cockpit.snapshot")?.agents ?? []
        )
      : null;

    if (!paneAfter?.pane && mappedFocused !== addr) {
      openAttachTab();
    }
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

  tickSupervisor(cx, force = false) {
    if (this.pollingSupervisor) return;
    const merged = mergeFleetRows(
      agents(),
      terminals(),
      shared("cockpit.snapshot"),
      shared("supervisor.snapshot")
    );
    const liveMerged = merged.filter(
      (a) => a.lifecycle?.state !== "finished" && a.state !== "finished"
    );
    const supSnap = shared("supervisor.snapshot");

    if (!force && liveMerged.length === 0) {
      const needsIdleCleanup =
        Boolean(supSnap?.model_loaded) ||
        (supSnap?.agents?.length ?? 0) > 0 ||
        (Boolean(supSnap?.runtime_status) && supSnap?.runtime_status !== "Idle");
      if (!needsIdleCleanup) {
        return;
      }
    }

    this.pollingSupervisor = true;
    if (force) {
      setShared("supervisor.forceEvaluating", true);
    }
    if (force || liveMerged.length > 0) {
      setShared("supervisor.evaluating", true);
      cx.notify();
    }

    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["supervisor", "tick", "--json"], {
          timeout: 60000,
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
        setShared("supervisor.forceEvaluating", false);
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

  runDoctorCheckSilent(cx) {
    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["doctor", "--json"], {
          timeout: 10000,
        });
        if (res.code === 0 && res.stdout) {
          this.doctorReport = JSON.parse(res.stdout);
          cx.notify();
        }
      } catch (_e) {}
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
    hc(["send", "--force", address, text]);
  }

  fetchHomeSnapshot(cx) {
    if (this.fetchingHome) return;
    this.fetchingHome = true;
    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["home", "--json"], {
          timeout: 8000,
        });
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          this.homeSnap = parsed;
          setShared("cockpit.home", parsed);
        }
      } catch (_e) {
        // Fallback smoothly to live Herdr spaces() / agents()
      } finally {
        this.fetchingHome = false;
        cx.notify();
      }
    });
  }

  fetchUsageSnapshot(cx, force = false, detect = false) {
    if (this.fetchingUsage || shared("cockpit.usageFetching")) return;
    this.fetchingUsage = true;
    setShared("cockpit.usageFetching", true);
    if (force || detect) {
      this.note = detect
        ? "Detectando contas locais no ai-usagebar…"
        : "Atualizando limites via ai-usagebar…";
    }
    cx.notify();

    cx.spawn(async (cx) => {
      let snap = null;
      try {
        const args = ["usage", "--json"];
        if (detect) args.push("--detect");
        const res = await exec("cockpit", args, { timeout: 22000 });
        if (res.code === 0 && res.stdout) {
          snap = JSON.parse(res.stdout);
        }
      } catch (_e) {
        // Fallback directly to ai-usagebar CLI if cockpit binary lacks `usage`
      }

      // Direct fallback if cockpit failed or didn't find ai-usagebar in its PATH
      if (!snap || !snap.installed) {
        try {
          if (detect) {
            const detRes = await exec("ai-usagebar", ["detect", "--json"], {
              timeout: 10000,
            }).catch(() => null);
            if (!detRes || detRes.code !== 0) {
              await exec("ai-usagebar", ["detect"], { timeout: 10000 }).catch(
                () => null
              );
            }
          }
          const [vRes, uRes] = await Promise.all([
            exec("ai-usagebar", ["vendors", "--json"], { timeout: 6000 }).catch(
              () => null
            ),
            exec("ai-usagebar", ["usage", "--json"], { timeout: 18000 }),
          ]);

          let vendors = [];
          if (vRes?.stdout?.trim()) {
            try {
              const vp = JSON.parse(vRes.stdout);
              vendors = Array.isArray(vp) ? vp : vp?.vendors ?? [];
            } catch (_ve) {}
          }

          let primary = null;
          let overview_vendors = null;
          let entries = [];
          let errMsg = null;
          if (uRes?.stdout?.trim()) {
            const up = JSON.parse(uRes.stdout);
            primary = up?.primary ?? null;
            overview_vendors = up?.overview_vendors ?? null;
            entries = Array.isArray(up?.entries) ? up.entries : [];
          } else if (uRes?.stderr?.trim()) {
            errMsg = uRes.stderr.trim();
          }

          snap = {
            installed: true,
            binary_path: "ai-usagebar",
            tui_binary_path: "ai-usagebar-tui",
            fetched_at_secs: Math.floor(Date.now() / 1000),
            error: errMsg,
            primary,
            overview_vendors,
            entries,
            vendors,
          };
        } catch (_fallbackErr) {
          if (!snap) {
            snap = {
              installed: false,
              binary_path: null,
              tui_binary_path: null,
              fetched_at_secs: Math.floor(Date.now() / 1000),
              error:
                "ai-usagebar não encontrado no sistema. Instale com `cargo binstall ai-usagebar` ou `yay -S ai-usagebar-bin`.",
              primary: null,
              overview_vendors: null,
              entries: [],
              vendors: [],
            };
          }
        }
      }

      if (snap) {
        this.usageSnap = snap;
        const nowMs = Date.now();
        this.usageFetchedAtMs = nowMs;
        setShared("cockpit.usageFetchedAtMs", nowMs);
        setShared("cockpit.usage", snap);
        if (force || detect) {
          if (!snap.installed) {
            this.note = "ai-usagebar não encontrado no sistema";
          } else if (snap.error && (!snap.entries || snap.entries.length === 0)) {
            this.note = ellipsize(snap.error, 56);
          } else {
            this.note = `Uso atualizado (${snap.entries?.length ?? 0} contas)`;
          }
        }
      }
      this.fetchingUsage = false;
      setShared("cockpit.usageFetching", false);
      cx.notify();
    });
  }

  runTaskCommand(cx, args, onSuccess) {
    if (this.busy) return;
    this.busy = true;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const res = await exec("cockpit", ["task", ...args], { timeout: 8000 });
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          this.homeSnap = parsed;
          setShared("cockpit.home", parsed);
          if (onSuccess) onSuccess(parsed);
        } else if (res.stderr) {
          this.note = res.stderr.trim();
        }
      } catch (e) {
        this.note = `Task error: ${String(e)}`;
      } finally {
        this.busy = false;
        cx.notify();
      }
    });
  }

  addTaskGui(cx) {
    const title = this.taskDraft.trim();
    if (!title || this.busy) return;
    const args = ["add", "--priority", this.taskPriority || "normal"];
    if (this.taskProject && this.taskProject !== "Geral") {
      args.push("--project", this.taskProject);
    }
    args.push("--json", title);
    this.taskDraft = "";
    this.editingField = null;
    this.runTaskCommand(cx, args, () => {
      this.note = `Tarefa adicionada: ${ellipsize(title, 36)}`;
    });
  }

  toggleTaskGui(cx, id) {
    if (!id) return;
    this.runTaskCommand(cx, ["toggle", id, "--json"]);
  }

  removeTaskGui(cx, id) {
    if (!id) return;
    this.runTaskCommand(cx, ["remove", id, "--json"]);
  }

  clearDoneTasksGui(cx) {
    this.runTaskCommand(cx, ["clear-done", "--json"], () => {
      this.note = "Tarefas concluídas removidas";
    });
  }

  handleKeyDown(e, cx, agent = null) {
    if (!this.editingField) return;
    const m = e.modifiers;
    if (e.key === "escape") {
      this.editingField = null;
    } else if (e.key === "enter") {
      if (this.editingField === "task") {
        this.addTaskGui(cx);
      } else if (this.editingField === "command" && agent) {
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
      if (this.editingField === "task") {
        this.taskDraft = this.taskDraft.slice(0, -1);
      } else if (this.editingField === "command") {
        this.commandDraft = this.commandDraft.slice(0, -1);
      } else if (this.editingField === "prompt") {
        this.promptText = this.promptText.slice(0, -1);
      } else {
        this.labelValue = this.labelValue.slice(0, -1);
      }
    } else if (e.key_char && !m?.control && !m?.platform) {
      if (this.editingField === "task") {
        this.taskDraft += e.key_char;
      } else if (this.editingField === "command") {
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
  }

  sectionTitle(t, title, subtitleOrCount = null) {
    return h_flex()
      .pt(10)
      .pb(5)
      .gap(6)
      .items_center()
      .justify_between()
      .border_b(1)
      .border_color(t.border)
      .child(
        h_flex()
          .gap(6)
          .items_center()
          .child(div().size(6).rounded(3).bg(t.accent))
          .child(
            div()
              .text_size(11.5)
              .font_semibold()
              .text_color(t.foreground)
              .child(title)
          )
      )
      .when(subtitleOrCount !== null && subtitleOrCount !== undefined, (el) =>
        el.child(
          div()
            .text_size(11)
            .text_color(t.muted)
            .child(String(subtitleOrCount))
        )
      );
  }

  cardBox(t) {
    return v_flex()
      .p(11)
      .gap(7)
      .rounded(8)
      .border(1)
      .border_color(t.border)
      .bg(t.surface);
  }

  pillBadge(t, text, fgColor, borderAlpha = "58", bgAlpha = "1F") {
    return div()
      .flex_none()
      .px(8)
      .py(2)
      .rounded(999)
      .border(1)
      .border_color(withAlpha(fgColor, borderAlpha, t.border))
      .bg(withAlpha(fgColor, bgAlpha, t.raised))
      .text_size(10.5)
      .font_semibold()
      .text_color(fgColor)
      .child(text);
  }

  badge(t, text, fg, bg) {
    return div()
      .flex_none()
      .px(7)
      .py(1)
      .rounded(999)
      .border(1)
      .border_color(withAlpha(fg, "48", t.border))
      .bg(bg ?? withAlpha(fg, "1F", t.raised))
      .text_size(10.5)
      .font_semibold()
      .text_color(fg)
      .child(text);
  }

  segmentedTrack(t) {
    return h_flex()
      .p(2)
      .gap(2)
      .items_center()
      .rounded(6)
      .border(1)
      .border_color(t.border)
      .bg(t.background);
  }

  segmentBtn(t, id, label, isActive, onClick, activeColor = null) {
    const accentCol = activeColor ?? t.accent;
    return h_flex()
      .id(id)
      .flex_none()
      .h(22)
      .px(8)
      .items_center()
      .justify_center()
      .rounded(4)
      .border(1)
      .border_color(
        isActive ? withAlpha(accentCol, "66", accentCol) : t.background
      )
      .bg(isActive ? t.selected : t.background)
      .text_size(11)
      .font_semibold()
      .text_color(isActive ? accentCol : t.muted)
      .cursor_pointer()
      .hover((s) =>
        s.bg(isActive ? t.selected : t.hover).text_color(t.foreground)
      )
      .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
      .on_click(onClick)
      .child(label);
  }

  toggleUsagePopup(cx) {
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );
    const usageOpen = Boolean(shared("cockpit.usageOpen") ?? this.usageOpen);
    if (usageOpen && !inspectorHidden) {
      this.closeUsagePopup(cx);
      return;
    }
    this.usageOpen = true;
    this.usageMenuOpen = false;
    setShared("cockpit.usageOpen", true);
    if (inspectorHidden) {
      this.usageOnlyDrawer = true;
      setShared("cockpit.usageOnlyDrawer", true);
      run("layout.toggle", { id: "cockpit-inspector" });
    } else {
      this.usageOnlyDrawer = false;
      setShared("cockpit.usageOnlyDrawer", false);
    }
    const curSnap = shared("cockpit.usage") ?? this.usageSnap;
    const fetchedAt = Number(
      shared("cockpit.usageFetchedAtMs") ?? this.usageFetchedAtMs ?? 0
    );
    if (!curSnap || Date.now() - fetchedAt > 60000) {
      this.fetchUsageSnapshot(cx, false, false);
    }
    cx?.notify();
  }

  closeUsagePopup(cx) {
    const onlyDrawer = Boolean(
      shared("cockpit.usageOnlyDrawer") ?? this.usageOnlyDrawer
    );
    this.usageOpen = false;
    this.usageMenuOpen = false;
    this.usageOnlyDrawer = false;
    setShared("cockpit.usageOpen", false);
    setShared("cockpit.usageOnlyDrawer", false);
    if (onlyDrawer && !findNode(layout(), "cockpit-inspector")?.hidden) {
      run("layout.hide", { id: "cockpit-inspector" });
    }
    cx?.notify();
  }

  openDrawerView(cx, tab, toggleIfActive = false) {
    const normalizedTab = tab === "usage" ? "inspector" : tab;
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );
    const onlyUsage = Boolean(
      shared("cockpit.usageOnlyDrawer") ?? this.usageOnlyDrawer
    );
    const rawView =
      shared("cockpit.viewMode") || this.inspectorTab || "inspector";
    const currentView = rawView === "usage" ? "inspector" : rawView;
    if (
      toggleIfActive &&
      !inspectorHidden &&
      !onlyUsage &&
      currentView === normalizedTab
    ) {
      this.usageOpen = false;
      this.usageOnlyDrawer = false;
      this.usageMenuOpen = false;
      setShared("cockpit.usageOpen", false);
      setShared("cockpit.usageOnlyDrawer", false);
      run("layout.toggle", { id: "cockpit-inspector" });
      cx?.notify();
      return;
    }
    if (onlyUsage) {
      this.usageOnlyDrawer = false;
      setShared("cockpit.usageOnlyDrawer", false);
    }
    setShared("cockpit.viewMode", normalizedTab);
    this.inspectorTab = normalizedTab;
    if (normalizedTab === "home") this.fetchHomeSnapshot(cx);
    else if (normalizedTab === "doctor" && !this.doctorReport) {
      this.runDoctorCheck(cx);
    } else if (normalizedTab === "inspector") {
      this.refreshInspector(cx);
    }
    if (inspectorHidden) {
      run("layout.toggle", { id: "cockpit-inspector" });
    }
    cx?.notify();
  }

  actionBtn(t, id, label, onClick, accent = false, danger = false) {
    const borderCol = danger
      ? withAlpha(t.blocked, "88", t.blocked)
      : accent
        ? withAlpha(t.accent, "88", t.accent)
        : t.border;
    const bgCol = danger
      ? withAlpha(t.blocked, "22", t.raised)
      : accent
        ? t.selected
        : t.raised;
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
        return { fg: t.blocked, bg: withAlpha(t.blocked, "24", t.raised) };
      case "STUCK_LOOP":
      case "ERROR":
        return { fg: t.blocked, bg: withAlpha(t.blocked, "30", t.raised) };
      case "COMPLETED":
        return { fg: t.done, bg: withAlpha(t.done, "24", t.raised) };
      case "WORKING":
      default:
        return { fg: t.working, bg: withAlpha(t.working, "22", t.raised) };
    }
  }

  renderPill(cx) {
    const t = theme();
    const supSnap = shared("supervisor.snapshot");
    const live = agents();
    const merged = mergeFleetRows(
      live,
      terminals(),
      shared("cockpit.snapshot"),
      supSnap
    );
    const liveMerged = merged.filter(
      (a) => a.lifecycle?.state !== "finished" && a.state !== "finished"
    );
    const isSupIdle =
      supSnap?.runtime_status === "Idle" ||
      supSnap?.runtime_status?.Watching === 0 ||
      liveMerged.length === 0;
    const watchingCount = isSupIdle
      ? 0
      : (supSnap?.runtime_status?.Watching ?? liveMerged.length);
    const alertCount = (supSnap?.agents ?? []).filter((a) => a.action_card).length;
    const blockedCount = liveMerged.filter((a) => a.status === "blocked").length;
    const evaluating =
      (Boolean(shared("supervisor.evaluating")) ||
        supSnap?.runtime_status === "Evaluating") &&
      (liveMerged.length > 0 || Boolean(shared("supervisor.forceEvaluating")));
    const activeMode = supSnap?.mode ?? "SemiAuto";
    const isPaused = activeMode === "Paused";
    const llmModel = supSnap?.llm_model || "lfm3:1.7b";
    const llmBackend = supSnap?.llm_backend || "ollama";
    const llmConnected = supSnap ? Boolean(supSnap.llm_connected) : false;
    const modelInstalled = supSnap ? Boolean(supSnap.model_installed) : false;
    const modelLoaded = Boolean(supSnap?.model_loaded);

    let llmPillText = `◌ LLM: ${llmModel} (verificando…)`;
    let llmPillColor = t.muted;
    if (llmConnected && modelInstalled) {
      llmPillText = `● LLM: ${llmModel} (${llmBackend} · ${modelLoaded ? llmMemoryLabel(supSnap) || "EM RAM" : "PRONTO"})`;
      llmPillColor = t.done;
    } else if (llmConnected && !modelInstalled) {
      llmPillText = `⚠ LLM: ${llmModel} (NÃO INSTALADO)`;
      llmPillColor = t.working;
    } else if (supSnap) {
      llmPillText = `○ LLM: ${llmModel} (OFFLINE)`;
      llmPillColor = t.blocked;
    }
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );
    const onlyUsage = Boolean(
      shared("cockpit.usageOnlyDrawer") ?? this.usageOnlyDrawer
    );
    const rawView =
      shared("cockpit.viewMode") || this.inspectorTab || "inspector";
    const rightView = rawView === "usage" ? "inspector" : rawView;
    const w = win();
    const macNoFs = w?.platform === "macos" && !w?.fullscreen;

    const watchColor = t.project ?? t.accent;
    let statusPillText = "Supervisor: Idle";
    let statusPillColor = t.done;
    let statusPillBg = withAlpha(t.done, "1F", t.surface);
    let statusPillBorder = withAlpha(t.done, "58", t.border);
    let dotColor = t.done;

    if (isPaused) {
      statusPillText = "Supervisor: Paused";
      statusPillColor = t.muted;
      statusPillBg = t.raised;
      statusPillBorder = t.border;
      dotColor = t.dim;
    } else if (evaluating) {
      statusPillText = "Supervisor: Evaluating";
      statusPillColor = t.accent;
      statusPillBg = withAlpha(t.accent, "1F", t.surface);
      statusPillBorder = withAlpha(t.accent, "66", t.accent);
      dotColor = t.accent;
    } else if (watchingCount > 0) {
      statusPillText = `Supervisor: Watching (${watchingCount})`;
      statusPillColor = t.foreground;
      statusPillBg = withAlpha(watchColor, "1F", t.surface);
      statusPillBorder = withAlpha(watchColor, "58", t.border);
      dotColor = watchColor;
    }

    if (alertCount > 0 || blockedCount > 0) {
      statusPillBorder = withAlpha(t.blocked, "88", t.blocked);
    }

    return h_flex()
      .size_full()
      .pl(macNoFs ? 76 : 10)
      .pr(10)
      .gap(8)
      .items_center()
      .bg(t.chrome)
      .border_b(1)
      .border_color(t.border)
      // Left Cluster: Live Supervisor Status Pill + Telemetry Badges.
      // Left and right clusters share flex_1 (basis 0) so the center cluster
      // stays pinned to the middle regardless of the supervisor state text.
      .child(
        h_flex()
          .flex_1()
          .min_w(0)
          .overflow_hidden()
          .gap(6)
          .items_center()
          .child(
            h_flex()
              .id("ck-status-pill")
              .flex_none()
              // Fixed floor fits "Supervisor: Watching (99)" + dot/spinner so
              // the badges to its right don't jitter between states.
              .min_w(196)
              .h(24)
              .px(10)
              .gap(6)
              .items_center()
              .rounded(999)
              .border(1)
              .border_color(statusPillBorder)
              .bg(statusPillBg)
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
                  .text_size(11)
                  .font_semibold()
                  .text_color(statusPillColor)
                  .child(statusPillText)
              )
          )
          .child(this.pillBadge(t, llmPillText, llmPillColor))
          .child(
            this.pillBadge(
              t,
              gpuPillLabel(supSnap),
              gpuOn(supSnap) ? t.accent : t.done
            )
              .id("ck-gpu-toggle")
              .cursor_pointer()
              .hover((s) => s.bg(t.hover))
              .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
              .on_click((_e, cx) =>
                this.runSupervisorAction(
                  cx,
                  ["supervisor", "gpu", gpuOn(supSnap) ? "off" : "on"],
                  gpuOn(supSnap)
                    ? "GPU desligada: LLM em CPU (num_gpu 0)"
                    : "GPU ligada: LLM com offload total"
                )
              )
          )
          .when(alertCount > 0, (el) =>
            el.child(
              this.pillBadge(
                t,
                `${alertCount} Alert${alertCount > 1 ? "s" : ""}`,
                t.blocked
              )
            )
          )
          .when(blockedCount > 0, (el) =>
            el.child(this.pillBadge(t, `${blockedCount} Needs You`, t.blocked))
          )
      )
      // Center Cluster: Segmented Mode Switcher + Poll Now + Unload LLM
      .child(
        h_flex()
          .flex_none()
          .gap(6)
          .items_center()
          .child(
            this.segmentedTrack(t).children(
              MODE_OPTIONS.map((m) =>
                this.segmentBtn(
                  t,
                  `ck-mode-${m.key}`,
                  m.label,
                  activeMode === m.match,
                  (_e, cx) =>
                    this.runSupervisorAction(
                      cx,
                      ["supervisor", "mode", m.key],
                      `Modo: ${m.label}`
                    )
                )
              )
            )
          )
          .child(
            this.actionBtn(t, "ck-pill-poll", "⚡ Poll Now", (_e, cx) => {
              this.refreshCockpit(cx, true);
              this.tickSupervisor(cx, true);
            })
          )
          .child(
            this.actionBtn(t, "ck-pill-unload", "⏻ Unload LLM", (_e, cx) =>
              this.runSupervisorAction(
                cx,
                ["supervisor", "unload"],
                "Modelo Ollama descarregado"
              )
            )
          )
      )
      // Right Cluster: Unified Segmented View Switcher + Switch Layout
      .child(
        h_flex()
          .flex_1()
          .min_w(0)
          .justify_end()
          .gap(6)
          .items_center()
          .child(
            this.segmentedTrack(t)
              .child(
                this.segmentBtn(
                  t,
                  "ck-pill-view-home",
                  "🏠 Home",
                  rightView === "home" && !inspectorHidden && !onlyUsage,
                  (_e, cx) => this.openDrawerView(cx, "home", true)
                )
              )
              .child(
                this.segmentBtn(
                  t,
                  "ck-pill-view-inspector",
                  "🔍 Inspector",
                  rightView === "inspector" && !inspectorHidden && !onlyUsage,
                  (_e, cx) => this.openDrawerView(cx, "inspector", true)
                )
              )
              .child(
                this.segmentBtn(
                  t,
                  "ck-pill-view-doctor",
                  "🩺 Doctor",
                  rightView === "doctor" && !inspectorHidden && !onlyUsage,
                  (_e, cx) => this.openDrawerView(cx, "doctor", true)
                )
              )
          )
          .child(
            this.actionBtn(t, "ck-pill-layout", "Switch Layout", () =>
              run("layout.switch")
            )
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
    const liveCount = parts.blocked.length + parts.active.length;

    const orderedAddrs = [
      ...parts.blocked.map((a) => a.address),
      ...parts.active.map((a) => a.address),
    ];
    if (orderedAddrs.length) setAgentOrder(orderedAddrs);

    const selAddr = this.selectedAddress();

    const filterSeg = (key, label) =>
      this.segmentBtn(
        t,
        `ck-filter-${key}`,
        label,
        this.filterTab === key,
        (_e, cx) => {
          this.filterTab = key;
          cx.notify();
        },
        t.foreground
      )
        .flex_1()
        .justify_center()
        .px(4);

    const renderAgentCard = (a) => {
      const isSelected = a.address === selAddr;
      const isFinished =
        a.lifecycle?.state === "finished" || a.state === "finished";
      const isBlocked = !isFinished && a.status === "blocked";
      const isWorking = !isFinished && a.status === "working";
      const isDone = isFinished || a.status === "done";
      const hasAlert = !isFinished && Boolean(a.action_card);
      const statusLabel = isBlocked
        ? (a.kind ?? "NEEDS YOU").toUpperCase()
        : isWorking
          ? "WORKING"
          : isDone
            ? "DONE"
            : "IDLE";
      const statusColor = isBlocked
        ? t.blocked
        : isWorking
          ? t.working
          : isDone
            ? t.done
            : t.muted;
      const pr = a.context?.pull_request;
      const issue = a.context?.issues?.[0]?.key ?? a.labels?.["cockpit.issue"];
      const tier = a.correlation_tier;
      const tierLabel =
        tier === "exact"
          ? "Exact"
          : tier === "inferred"
            ? "Inferred"
            : tier === "ambiguous"
              ? "Ambiguous"
              : "Local";
      const tierColor =
        tier === "exact"
          ? t.done
          : tier === "inferred"
            ? t.accent
            : tier === "ambiguous"
              ? t.blocked
              : t.muted;
      const summaryLine = a.message || a.rolling_summary || "";
      const cardBorder = isSelected
        ? t.accent
        : isBlocked || hasAlert
          ? withAlpha(t.blocked, "88", t.blocked)
          : t.border;

      return v_flex()
        .id(`ck-row-${a.address}`)
        .mx(10)
        .p(10)
        .gap(5)
        .rounded(8)
        .border(1)
        .border_color(cardBorder)
        .bg(isSelected ? t.selected : t.surface)
        .cursor_pointer()
        .hover((s) => s.bg(t.hover))
        .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
        .on_click((_e, cx) => {
          const curFocus = focused()?.terminal ?? null;
          const resolvedCurFocus = curFocus
            ? resolveAttachedTargetAddress(
                curFocus,
                terminals() ?? [],
                shared("cockpit.snapshot")?.agents ?? []
              )
            : null;
          setShared("cockpit.lastFocusedTerminal", resolvedCurFocus);
          setShared("cockpit.selected", a.address);
          setShared("cockpit.viewMode", "inspector");
          setShared("cockpit.usageOnlyDrawer", false);
          this.inspectorTab = "inspector";
          if (findNode(layout(), "cockpit-inspector")?.hidden) {
            run("layout.toggle", { id: "cockpit-inspector" });
          }
          void this.focusOrAttachTerminal(cx, a, false);
          this.refreshInspector(cx);
          cx.notify();
        })
        // Row 1: Dot/Spinner + Terminal ID + Program Badge + Status Pill
        .child(
          h_flex()
            .gap(6)
            .items_center()
            .when(isWorking, (el) =>
              el.child(
                svg("mods/workspace-chrome/spinner.svg")
                  .size(11)
                  .text_color(t.working)
                  .spin(900)
              )
            )
            .when(!isWorking, (el) =>
              el.child(div().size(7).rounded(4).bg(statusColor))
            )
            .child(
              div()
                .text_size(12)
                .font_semibold()
                .text_color(t.address ?? t.accent)
                .child(a.address)
            )
            .child(
              div()
                .text_size(12)
                .font_semibold()
                .text_color(t.foreground)
                .child(ellipsize(a.program_label || "agent", 14))
            )
            .child(div().flex_1())
            .child(this.pillBadge(t, statusLabel, statusColor))
        )
        // Row 2: Project & Summary
        .child(
          div()
            .text_size(12.5)
            .font_semibold()
            .text_color(t.foreground)
            .child(ellipsize(a.project_name || a.name, 34))
        )
        .when(Boolean(summaryLine), (el) =>
          el.child(
            div()
              .text_size(11.5)
              .text_color(isBlocked ? t.foreground : t.muted)
              .child(ellipsize(summaryLine, 52))
          )
        )
        .when(a.blocked_children?.length > 0, (el) =>
          el.child(
            div()
              .text_size(11)
              .text_color(t.blocked)
              .child(
                ellipsize(
                  `↳ Subagent blocked: ${a.blocked_children.join(", ")}`,
                  48
                )
              )
          )
        )
        // Row 3: Branch & Correlation Footer
        .child(
          h_flex()
            .gap(6)
            .items_center()
            .pt(1)
            .when(Boolean(a.branch), (el) =>
              el.child(
                div()
                  .flex_1()
                  .min_w(0)
                  .text_size(11)
                  .text_color(t.accent)
                  .child(`⎇ ${ellipsize(a.branch, 22)}`)
              )
            )
            .when(!a.branch, (el) =>
              el.child(
                div()
                  .flex_1()
                  .text_size(11)
                  .text_color(t.dim)
                  .child("Local / No branch")
              )
            )
            .when(Boolean(issue), (el) =>
              el.child(this.pillBadge(t, issue, t.accent))
            )
            .when(Boolean(pr), (el) =>
              el.child(
                this.pillBadge(
                  t,
                  `#${pr.number} ${pr.ci_rollup ?? ""}`.trim(),
                  pr.ci_rollup === "FAILURE" ? t.blocked : t.done
                )
              )
            )
            .when(!issue && !pr, (el) =>
              el.child(this.pillBadge(t, tierLabel, tierColor))
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
        // Header Bar (only COCKPIT FLEET + count pill + [+ Agent] + [↻])
        h_flex()
          .h(36)
          .px(12)
          .gap(6)
          .items_center()
          .border_b(1)
          .border_color(t.border)
          .child(
            div()
              .text_size(11.5)
              .font_semibold()
              .text_color(t.muted)
              .child("COCKPIT FLEET")
          )
          .child(this.pillBadge(t, `${liveCount}`, t.accent))
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
      )
      .when(this.showLauncher, (el) =>
        el.child(
          v_flex()
            .p(10)
            .gap(6)
            .bg(t.raised)
            .border_b(1)
            .border_color(t.border)
            .child(
              div()
                .text_size(11.5)
                .text_color(t.muted)
                .child("Launch in active space:")
            )
            .child(
              h_flex()
                .gap(5)
                .flex_wrap()
                .children(
                  LAUNCH_AGENTS.map(([label, argv], idx) =>
                    this.actionBtn(t, `ck-launch-${idx}`, label, (_e, cx) => {
                      const sp = spaces().find((s) => s.active);
                      run("tab.new", {
                        space: sp?.id,
                        name: label.toLowerCase(),
                        argv: argv ?? undefined,
                      });
                      this.showLauncher = false;
                      cx.notify();
                    })
                  )
                )
            )
        )
      )
      .child(
        // 4-Column Segmented Filter Bar
        this.segmentedTrack(t)
          .mx(10)
          .mt(8)
          .mb(6)
          .child(filterSeg("all", `Todos (${liveCount})`))
          .child(filterSeg("blocked", `Needs (${parts.blocked.length})`))
          .child(filterSeg("active", `Work (${parts.active.length})`))
          .child(filterSeg("finished", `Done (${parts.finished.length})`))
      )
      .child(
        // Fleet list
        v_flex()
          .flex_1()
          .min_h(0)
          .overflow_y_scrollbar()
          .py(4)
          .gap(7)
          .when(
            liveCount === 0 &&
              this.filterTab !== "finished" &&
              !this.showFinished,
            (el) =>
            el.child(
              this.cardBox(t)
                .mx(10)
                .child(
                  div()
                    .text_size(13)
                    .font_semibold()
                    .text_color(t.foreground)
                    .child("Nenhum agente ativo no momento")
                )
                .child(
                  div()
                    .text_size(12)
                    .text_color(t.muted)
                    .child(
                      "Shells comuns (zsh, bash, fish) ficam ocultos automaticamente para manter o foco nos seus agentes de código."
                    )
                )
                .child(
                  h_flex()
                    .gap(6)
                    .flex_wrap()
                    .pt(2)
                    .child(
                      this.actionBtn(
                        t,
                        "ck-empty-home",
                        "🏠 Home Overview",
                        (_e, cx) => this.openDrawerView(cx, "home", false),
                        true
                      )
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
          .when(
            this.filterTab === "finished" && parts.finished.length > 0,
            (el) =>
              el.child(
                h_flex()
                  .mx(10)
                  .px(4)
                  .items_center()
                  .justify_between()
                  .child(
                    div()
                      .text_size(11)
                      .text_color(t.dim)
                      .child(`Finished (${parts.finished.length})`)
                  )
                  .child(
                    this.actionBtn(
                      t,
                      "ck-clear-finished",
                      "Clear Done",
                      (_e, cx) => {
                        const addrs = parts.finished.map((a) => a.address);
                        if (addrs.length > 0) {
                          hc(["rm", ...addrs]);
                          this.refreshCockpit(cx, true);
                        }
                      }
                    )
                  )
              )
          )
          .children(showFinished.map(renderAgentCard))
      )
      .child(
        // Pinned Bottom Quick-Launcher Strip
        v_flex()
          .border_t(1)
          .border_color(t.border)
          .p(8)
          .gap(5)
          .bg(t.chrome)
          .child(
            div()
              .text_size(10.5)
              .font_semibold()
              .text_color(t.dim)
              .child("+ AGENT QUICK-LAUNCHER")
          )
          .child(
            h_flex()
              .gap(4)
              .flex_wrap()
              .children(
                LAUNCH_AGENTS.map(([label, argv], idx) =>
                  this.actionBtn(t, `ck-quick-${idx}`, `+${label}`, () => {
                    const sp = spaces().find((s) => s.active);
                    run("tab.new", {
                      space: sp?.id,
                      name: label.toLowerCase(),
                      argv: argv ?? undefined,
                    });
                  })
                )
              )
          )
      );
  }

  renderHomeBody(cx, themeArg = null) {
    const t = themeArg ?? theme();
    const homeSnap = this.homeSnap ?? shared("cockpit.home") ?? null;
    const snap = shared("cockpit.snapshot");
    const supSnap = shared("supervisor.snapshot");
    const liveSpaces = spaces();
    const allTerms = terminals();
    const liveAgents = agents();
    const mergedFleet = mergeFleetRows(liveAgents, allTerms, snap, supSnap);
    const liveMergedFleet = mergedFleet.filter(
      (a) => a.lifecycle?.state !== "finished" && a.state !== "finished"
    );

    const projects = buildProjectsList(
      homeSnap,
      liveSpaces,
      allTerms,
      snap,
      supSnap
    );

    const tasks = homeSnap?.tasks ?? [];
    const totalTasksCount = tasks.length;
    const doneTasksCount = tasks.filter((tk) => tk.done).length;
    const pendingTasksCount = totalTasksCount - doneTasksCount;
    const pct =
      totalTasksCount > 0
        ? Math.round((doneTasksCount / totalTasksCount) * 100)
        : 0;

    const activeProjectsCount = projects.filter(
      (p) => p.activeAgents > 0 || p.blockedAgents > 0 || p.isActiveSpace
    ).length;
    const blockedAgents = mergedFleet.filter(
      (a) => a.status === "blocked" || Boolean(a.action_card)
    );
    const blockedAgentsCount = blockedAgents.length;

    const activeSpace = liveSpaces.find((s) => s.active);
    const projectChoices = [
      "Geral",
      ...Array.from(new Set(projects.map((p) => p.name).filter(Boolean))),
    ];
    const currentProjectTag =
      this.taskProject || activeSpace?.name || "Geral";

    const filteredTasks = tasks.filter((tk) => {
      if (this.taskFilter === "pending") return !tk.done;
      if (this.taskFilter === "done") return Boolean(tk.done);
      return true;
    });

    const kpiCard = (label, value, sub, valueColor = t.foreground) =>
      v_flex()
        .flex_1()
        .min_w(0)
        .p(10)
        .gap(4)
        .rounded(10)
        .border(1)
        .border_color(t.border)
        .bg(t.surface)
        .child(
          div()
            .text_size(10.5)
            .font_semibold()
            .text_color(t.muted)
            .child(label)
        )
        .child(
          div()
            .text_size(15)
            .font_semibold()
            .text_color(valueColor)
            .child(ellipsize(value, 24))
        )
        .when(Boolean(sub), (el) =>
          el.child(
            div()
              .text_size(11)
              .text_color(t.muted)
              .child(ellipsize(sub, 32))
          )
        );

    const priorityColor = (prio) => {
      if (prio === "high") return t.blocked;
      if (prio === "low") return t.muted;
      return t.accent;
    };

    return v_flex()
      .flex_1()
      .min_h(0)
      .overflow_y_scrollbar()
      .p(12)
      .gap(10)
      // 1. 4 KPI Cards (2x2 grid)
      .child(
        v_flex()
          .gap(8)
          .child(
            h_flex()
              .gap(8)
              .child(
                kpiCard(
                  "PROJETOS ATIVOS",
                  `${activeProjectsCount} / ${projects.length}`,
                  "Spaces & repositórios",
                  t.foreground
                )
              )
              .child(
                kpiCard(
                  "FROTA DE AGENTES",
                  `${liveMergedFleet.length}`,
                  `(${blockedAgentsCount} blk)`,
                  blockedAgentsCount > 0 ? t.blocked : t.foreground
                )
              )
          )
          .child(
            h_flex()
              .gap(8)
              .child(
                kpiCard(
                  "TAREFAS DO DIA",
                  `${doneTasksCount} / ${totalTasksCount} (${pct}%)`,
                  `${pendingTasksCount} pendentes`,
                  t.done
                )
              )
              .child(
                kpiCard(
                  "SUPERVISOR DURÁVEL",
                  supSnap
                    ? supSnap.llm_connected
                      ? `${supSnap.llm_model || "lfm3:1.7b"} · ON`
                      : `${supSnap.llm_model || "lfm3:1.7b"} · OFF`
                    : `${supSnap?.llm_model || "lfm3:1.7b"} · …`,
                  `${supSnap?.llm_backend || "ollama"} · ${gpuModeText(supSnap)}`,
                  supSnap?.llm_connected ? t.done : supSnap ? t.blocked : t.muted
                )
              )
          )
      )
      // 2. Card 1: TAREFAS DO DIA
      .child(
        this.sectionTitle(
          t,
          "📋 TAREFAS DO DIA",
          "cockpit task"
        )
      )
      .child(
        this.cardBox(t)
          .rounded(10)
          .child(
            h_flex()
              .gap(6)
              .items_center()
              .child(
                div()
                  .id("ck-task-input")
                  .flex_1()
                  .min_w(0)
                  .px(9)
                  .py(6)
                  .rounded(6)
                  .border(1)
                  .border_color(
                    this.editingField === "task" ? t.accent : t.border
                  )
                  .bg(t.background)
                  .text_size(12)
                  .text_color(this.taskDraft ? t.foreground : t.dim)
                  .cursor_text()
                  .on_click((_e, cx) => {
                    this.editingField = "task";
                    this.handle.focus();
                    cx.notify();
                  })
                  .child(
                    this.taskDraft
                      ? ellipsize(this.taskDraft, 56) +
                          (this.editingField === "task" ? "▏" : "")
                      : (this.editingField === "task" ? "▏" : "") +
                          "Nova tarefa (ex: revisar PR #94)…"
                  )
              )
              .child(
                h_flex()
                  .id("ck-task-add")
                  .h(26)
                  .px(10)
                  .items_center()
                  .rounded(6)
                  .bg(t.accent)
                  .text_size(11.5)
                  .font_semibold()
                  .text_color(t.background)
                  .cursor_pointer()
                  .hover((s) => s.bg(withAlpha(t.accent, "CC", t.accent)))
                  .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
                  .on_click((_e, cx) => this.addTaskGui(cx))
                  .child("+ Add")
              )
          )
          // Priority & Project selector pills
          .child(
            h_flex()
              .gap(5)
              .flex_wrap()
              .items_center()
              .child(
                this.segmentedTrack(t).children(
                  ["high", "normal", "low"].map((key) =>
                    this.segmentBtn(
                      t,
                      `ck-task-prio-${key}`,
                      key,
                      this.taskPriority === key,
                      (_e, cx) => {
                        this.taskPriority = key;
                        cx.notify();
                      },
                      priorityColor(key)
                    )
                  )
                )
              )
              .child(div().flex_1())
              .child(
                h_flex()
                  .id("ck-task-project-cycle")
                  .h(24)
                  .px(8)
                  .gap(4)
                  .items_center()
                  .rounded(999)
                  .border(1)
                  .border_color(t.border)
                  .bg(t.background)
                  .text_size(11)
                  .text_color(t.accent)
                  .cursor_pointer()
                  .hover((s) => s.bg(t.hover).text_color(t.foreground))
                  .on_click((_e, cx) => {
                    const idx = projectChoices.indexOf(currentProjectTag);
                    const next =
                      projectChoices[(idx + 1) % projectChoices.length] ??
                      "Geral";
                    this.taskProject = next;
                    cx.notify();
                  })
                  .child(`📁 ${ellipsize(currentProjectTag, 18)}`)
              )
          )
          // Filter strip
          .child(
            h_flex()
              .gap(6)
              .flex_wrap()
              .items_center()
              .child(
                this.segmentedTrack(t)
                  .child(
                    this.segmentBtn(
                      t,
                      "ck-task-filter-pending",
                      `Pendentes (${pendingTasksCount})`,
                      this.taskFilter === "pending",
                      (_e, cx) => {
                        this.taskFilter = "pending";
                        cx.notify();
                      }
                    )
                  )
                  .child(
                    this.segmentBtn(
                      t,
                      "ck-task-filter-all",
                      `Todas (${totalTasksCount})`,
                      this.taskFilter === "all",
                      (_e, cx) => {
                        this.taskFilter = "all";
                        cx.notify();
                      }
                    )
                  )
                  .child(
                    this.segmentBtn(
                      t,
                      "ck-task-filter-done",
                      `Concluídas (${doneTasksCount})`,
                      this.taskFilter === "done",
                      (_e, cx) => {
                        this.taskFilter = "done";
                        cx.notify();
                      }
                    )
                  )
              )
              .child(div().flex_1())
              .when(doneTasksCount > 0, (el) =>
                el.child(
                  this.actionBtn(
                    t,
                    "ck-task-clear-done",
                    "Limpar concluídas",
                    (_e, cx) => this.clearDoneTasksGui(cx)
                  )
                )
              )
          )
          // Task rows
          .when(filteredTasks.length === 0, (el) =>
            el.child(
              div()
                .py(6)
                .text_size(12)
                .text_color(t.muted)
                .child(
                  this.taskFilter === "done"
                    ? "Nenhuma tarefa concluída ainda."
                    : "Nenhuma tarefa pendente. Adicione acima o que vai atacar hoje!"
                )
            )
          )
          .children(
            filteredTasks.map((tk) => {
              const prioCol = priorityColor(tk.priority);
              const projObj = tk.project
                ? projects.find(
                    (p) =>
                      p.name.toLowerCase() ===
                      String(tk.project).trim().toLowerCase()
                  )
                : null;
              return h_flex()
                .id(`ck-task-row-${tk.id}`)
                .px(10)
                .py(7)
                .gap(7)
                .items_center()
                .rounded(6)
                .border(1)
                .border_color(t.border)
                .bg(t.background)
                .child(
                  div()
                    .id(`ck-task-toggle-${tk.id}`)
                    .px(5)
                    .py(1)
                    .rounded(4)
                    .bg(tk.done ? withAlpha(t.done, "24", t.raised) : t.surface)
                    .border(1)
                    .border_color(tk.done ? t.done : t.border)
                    .text_size(11)
                    .font_semibold()
                    .text_color(tk.done ? t.done : t.muted)
                    .cursor_pointer()
                    .on_click((_e, cx) => this.toggleTaskGui(cx, tk.id))
                    .child(tk.done ? "[✓]" : "[ ]")
                )
                .child(
                  div()
                    .flex_1()
                    .min_w(0)
                    .text_size(12)
                    .text_color(tk.done ? t.dim : t.foreground)
                    .child(ellipsize(tk.title, 54))
                )
                .when(Boolean(tk.project), (el) =>
                  el.child(
                    div()
                      .id(`ck-task-proj-${tk.id}`)
                      .cursor_pointer()
                      .on_click(() => {
                        if (projObj?.spaceId) {
                          run("space.focus", { space: projObj.spaceId });
                        }
                      })
                      .child(
                        this.pillBadge(
                          t,
                          ellipsize(tk.project, 14),
                          t.accent
                        )
                      )
                  )
                )
                .child(
                  this.pillBadge(
                    t,
                    String(tk.priority || "normal").toUpperCase(),
                    prioCol
                  )
                )
                .child(
                  div()
                    .id(`ck-task-del-${tk.id}`)
                    .px(5)
                    .py(1)
                    .rounded(4)
                    .text_size(12)
                    .text_color(t.muted)
                    .cursor_pointer()
                    .hover((s) => s.bg(t.hover).text_color(t.blocked))
                    .on_click((_e, cx) => this.removeTaskGui(cx, tk.id))
                    .child("×")
                );
            })
          )
      )
      // 3. Card 2: PROJETOS EM DESENVOLVIMENTO
      .child(
        this.sectionTitle(
          t,
          "🚀 PROJETOS EM DESENVOLVIMENTO",
          `${projects.length} projetos`
        )
      )
      .when(projects.length === 0, (el) =>
        el.child(
          this.cardBox(t)
            .rounded(10)
            .child(
              div()
                .text_size(12)
                .text_color(t.muted)
                .child("Nenhum Space ou projeto git detectado ainda.")
            )
        )
      )
      .children(
        projects.map((proj, idx) =>
          v_flex()
            .id(`ck-home-proj-${idx}`)
            .p(10)
            .gap(5)
            .rounded(8)
            .border(1)
            .border_color(
              proj.status === "blocked"
                ? withAlpha(t.blocked, "88", t.blocked)
                : proj.isActiveSpace
                  ? t.accent
                  : t.border
            )
            .bg(t.background)
            .child(
              h_flex()
                .gap(6)
                .items_center()
                .child(
                  div()
                    .text_size(13)
                    .font_semibold()
                    .text_color(t.foreground)
                    .child(ellipsize(proj.name, 22))
                )
                .when(proj.isActiveSpace, (el) =>
                  el.child(this.pillBadge(t, "ATIVO", t.accent))
                )
                .child(div().flex_1())
                .child(
                  div()
                    .text_size(11)
                    .text_color(t.muted)
                    .child(
                      `${proj.dirtyCount} dirty · ${proj.activeAgents} active`
                    )
                )
            )
            .child(
              div()
                .text_size(11.5)
                .text_color(t.accent)
                .child(`⎇ ${ellipsize(proj.branch || "main", 34)}`)
            )
            .when(Boolean(proj.lastCommit || proj.path), (el) =>
              el.child(
                div()
                  .text_size(11)
                  .text_color(t.dim)
                  .child(ellipsize(proj.lastCommit || proj.path, 54))
              )
            )
            .child(
              h_flex()
                .gap(5)
                .pt(2)
                .flex_wrap()
                .when(Boolean(proj.spaceId), (el) =>
                  el.child(
                    this.actionBtn(t, `ck-proj-open-${idx}`, "Abrir Space", () =>
                      run("space.focus", { space: proj.spaceId })
                    )
                  )
                )
                .child(
                  this.actionBtn(
                    t,
                    `ck-proj-task-${idx}`,
                    "+ Tarefa",
                    (_e, cx) => {
                      this.taskProject = proj.name;
                      this.editingField = "task";
                      this.handle.focus();
                      cx.notify();
                    }
                  )
                )
                .child(
                  this.actionBtn(
                    t,
                    `ck-proj-agent-${idx}`,
                    "+ Claude",
                    () => {
                      const targetSpace =
                        proj.spaceId ?? activeSpace?.id ?? undefined;
                      run("tab.new", {
                        space: targetSpace,
                        name: "claude",
                        argv: ["claude"],
                      });
                    },
                    true
                  )
                )
            )
        )
      )
      // 4. Card 3: ATENÇÃO IMEDIATA & FROTA
      .child(
        this.sectionTitle(
          t,
          "⚡ ATENÇÃO IMEDIATA & FROTA",
          blockedAgents.length > 0
            ? `${blockedAgents.length} aguardando`
            : "nominal"
        )
      )
      .when(blockedAgents.length === 0, (el) =>
        el.child(
          div()
            .p(10)
            .rounded(8)
            .border(1)
            .border_color(withAlpha(t.done, "58", t.done))
            .bg(withAlpha(t.done, "18", t.surface))
            .text_size(11.5)
            .font_semibold()
            .text_color(t.done)
            .child("✓ Nenhum agente bloqueado. Frota operando nominalmente.")
        )
      )
      .children(
        blockedAgents.map((a) =>
          v_flex()
            .id(`ck-home-alert-${a.address}`)
            .p(10)
            .gap(5)
            .rounded(8)
            .border(1)
            .border_color(withAlpha(t.blocked, "77", t.blocked))
            .bg(t.background)
            .child(
              h_flex()
                .gap(6)
                .items_center()
                .justify_between()
                .child(
                  h_flex()
                    .gap(5)
                    .items_center()
                    .child(
                      div()
                        .text_size(12)
                        .font_semibold()
                        .text_color(t.blocked)
                        .child(a.address)
                    )
                    .child(
                      div()
                        .text_size(12)
                        .font_semibold()
                        .text_color(t.foreground)
                        .child(ellipsize(a.program_label || "agent", 14))
                    )
                    .child(
                      div()
                        .text_size(11.5)
                        .text_color(t.muted)
                        .child(ellipsize(`· ${a.project_name || a.name}`, 24))
                    )
                )
                .child(
                  this.actionBtn(
                    t,
                    `ck-home-inspect-${a.address}`,
                    "Inspector →",
                    (_e, cx) => {
                      setShared("cockpit.selected", a.address);
                      setShared("cockpit.viewMode", "inspector");
                      setShared("cockpit.usageOnlyDrawer", false);
                      this.inspectorTab = "inspector";
                      this.refreshInspector(cx);
                      if (findNode(layout(), "cockpit-inspector")?.hidden) {
                        run("layout.toggle", { id: "cockpit-inspector" });
                      }
                      cx.notify();
                    },
                    true
                  )
                )
            )
            .when(
              Boolean(
                a.action_card?.reason_for_operator ||
                  a.message ||
                  a.rolling_summary
              ),
              (el2) =>
                el2.child(
                  div()
                    .text_size(11.5)
                    .text_color(t.muted)
                    .child(
                      ellipsize(
                        a.action_card?.reason_for_operator ||
                          a.message ||
                          a.rolling_summary,
                        84
                      )
                    )
                )
            )
        )
      );
  }

  renderHome(cx) {
    const t = theme();
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );

    return v_flex()
      .id("ck-home-root")
      .size_full()
      .track_focus(this.handle)
      .bg(t.chrome)
      .text_color(t.foreground)
      .rounded(10)
      .border(1)
      .border_color(t.border)
      .overflow_hidden()
      .on_key_down((e, cx) => this.handleKeyDown(e, cx, null))
      .child(
        // Top Header Bar
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
              .min_w(0)
              .text_size(13)
              .font_semibold()
              .text_color(t.foreground)
              .child(ellipsize("🏠 COCKPIT HOME · RESUMO DO DIA", 34))
          )
          .child(
            this.actionBtn(t, "ck-home-refresh", "↻ Atualizar", (_e, cx) => {
              this.fetchHomeSnapshot(cx);
              this.refreshCockpit(cx, true);
            })
          )
          .child(
            this.actionBtn(
              t,
              "ck-home-inspector",
              inspectorHidden ? "Inspector ◁" : "Inspector ▷",
              () => run("layout.toggle", { id: "cockpit-inspector" }),
              !inspectorHidden
            )
          )
          .child(
            this.actionBtn(t, "ck-home-close", "×", () =>
              run("layout.hide", { id: "cockpit-home" })
            )
          )
      )
      .when(Boolean(this.note), (el) =>
        el.child(
          h_flex()
            .px(12)
            .py(6)
            .bg(t.raised)
            .border_b(1)
            .border_color(t.border)
            .text_size(11.5)
            .text_color(t.accent)
            .justify_between()
            .child(ellipsize(this.note, 56))
            .child(
              div()
                .id("ck-home-clear-note")
                .cursor_pointer()
                .text_color(t.muted)
                .on_click((_e, cx) => {
                  this.note = "";
                  cx.notify();
                })
                .child("dismiss")
            )
        )
      )
      .child(this.renderHomeBody(cx));
  }

  renderDoctorBody(cx, themeArg = null) {
    const t = themeArg ?? theme();
    const snap = shared("cockpit.snapshot");
    const supSnap = shared("supervisor.snapshot");
    const report = this.doctorReport;
    const overall = (report?.overall_status ?? "unknown").toUpperCase();
    const overallCol =
      overall === "PASS" || overall === "OK"
        ? t.done
        : overall === "WARN"
          ? t.working
          : overall === "FAIL"
            ? t.blocked
            : t.muted;

    const defaultChecks = [
      {
        name: "hc_runtime",
        status: "pass",
        detail: "herdr-core runtime & IPC socket",
      },
      {
        name: "hooks",
        status: "unknown",
        detail: "Claude / shell hook installation",
      },
      {
        name: "herdr_mod",
        status: "pass",
        detail: "cockpit mod & layout loaded",
      },
      {
        name: "storage_ipc",
        status: "unknown",
        detail: "SQLite WAL cache & local store",
      },
      {
        name: "supervisor",
        status: supSnap ? "pass" : "unknown",
        detail: `${supSnap?.llm_model || "lfm3:1.7b"} · ${gpuModeText(supSnap)}`,
      },
    ];
    const checks = report?.checks?.length ? report.checks : defaultChecks;
    const providers = report?.providers ?? snap?.providers ?? {};

    const checkStatusColor = (st) => {
      const s = String(st ?? "").toLowerCase();
      if (s === "pass" || s === "ok" || s === "active" || s === "ready")
        return t.done;
      if (
        s === "warn" ||
        s === "degraded" ||
        s === "rate_limited" ||
        s === "missing_scope" ||
        s === "auth_expired"
      )
        return t.working;
      if (s === "fail" || s === "error" || s === "offline") return t.blocked;
      if (s === "inactive") return t.muted;
      return t.muted;
    };

    return v_flex()
      .flex_1()
      .min_h(0)
      .overflow_y_scrollbar()
      .p(12)
      .gap(10)
      // Header Summary Card
      .child(
        v_flex()
          .p(11)
          .gap(8)
          .rounded(10)
          .border(1)
          .border_color(t.border)
          .bg(t.surface)
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
                    div()
                      .text_size(13)
                      .font_semibold()
                      .text_color(t.foreground)
                      .child("🩺 SYSTEM & PROVIDER DIAGNOSTICS")
                  )
              )
              .child(
                this.pillBadge(
                  t,
                  report ? overall : "READY",
                  report ? overallCol : t.accent
                )
              )
          )
          .child(
            div()
              .text_size(11.5)
              .text_color(t.muted)
              .child(
                `cockpit v${report?.cockpit_version ?? version} · supervisor ${supSnap?.llm_model || "lfm3:1.7b"} · ${gpuModeText(supSnap)}`
              )
          )
          .child(
            h_flex()
              .gap(6)
              .flex_wrap()
              .child(
                this.actionBtn(
                  t,
                  "ck-doc-run",
                  "Run Doctor",
                  (_e, cx) => this.runDoctorCheck(cx),
                  true
                )
              )
              .child(
                this.actionBtn(
                  t,
                  "ck-doc-setup",
                  "Verify / Repair Hooks",
                  (_e, cx) => this.runSetupGui(cx)
                )
              )
          )
      )
      // System Checks
      .child(
        this.sectionTitle(
          t,
          "SYSTEM CHECKS",
          `${checks.length} checks`
        )
      )
      .child(
        v_flex()
          .gap(6)
          .children(
            checks.map((chk, idx) => {
              const col = checkStatusColor(chk.status);
              return v_flex()
                .id(`ck-doc-chk-${idx}`)
                .p(9)
                .gap(3)
                .rounded(8)
                .border(1)
                .border_color(t.border)
                .bg(t.background)
                .child(
                  h_flex()
                    .justify_between()
                    .items_center()
                    .gap(6)
                    .child(
                      div()
                        .text_size(12)
                        .font_semibold()
                        .text_color(t.foreground)
                        .child(chk.name)
                    )
                    .child(
                      this.pillBadge(
                        t,
                        String(chk.status ?? "unknown").toUpperCase(),
                        col
                      )
                    )
                )
                .when(Boolean(chk.detail), (el) =>
                  el.child(
                    div()
                      .text_size(11)
                      .text_color(t.muted)
                      .child(ellipsize(chk.detail, 72))
                  )
                );
            })
          )
      )
      // Provider Health Cards
      .child(
        this.sectionTitle(
          t,
          "PROVIDER HEALTH",
          `${Object.keys(providers).length} providers`
        )
      )
      .when(Object.keys(providers).length === 0, (el) =>
        el.child(
          this.cardBox(t)
            .rounded(10)
            .child(
              div()
                .text_size(11.5)
                .text_color(t.dim)
                .child(
                  "Local-First Read-Only · Click 'Run Doctor' to probe configured providers."
                )
            )
        )
      )
      .children(
        Object.entries(providers).map(([prov, ph], idx) => {
          const rawStatus = ph?.status ?? ph?.state ?? "unknown";
          const col = checkStatusColor(rawStatus);
          return v_flex()
            .id(`ck-doc-prov-${idx}`)
            .p(9)
            .gap(3)
            .rounded(8)
            .border(1)
            .border_color(t.border)
            .bg(t.background)
            .child(
              h_flex()
                .justify_between()
                .items_center()
                .gap(6)
                .child(
                  div()
                    .text_size(12)
                    .font_semibold()
                    .text_color(t.foreground)
                    .child(prov)
                )
                .child(
                  this.pillBadge(
                    t,
                    String(rawStatus).toUpperCase(),
                    col
                  )
                )
            )
            .when(Boolean(ph?.detail || ph?.message), (el) =>
              el.child(
                div()
                  .text_size(11)
                  .text_color(t.muted)
                  .child(ellipsize(ph.detail || ph.message, 72))
              )
            );
        })
      );
  }

  renderUsagePopover(cx, themeArg = null, standalone = true) {
    const t = themeArg ?? theme();
    const usageSnap = shared("cockpit.usage") ?? this.usageSnap ?? null;
    const entries = Array.isArray(usageSnap?.entries) ? usageSnap.entries : [];
    const vendors = Array.isArray(usageSnap?.vendors) ? usageSnap.vendors : [];
    const isFetching = Boolean(
      this.fetchingUsage || shared("cockpit.usageFetching")
    );

    const vendorById = new Map();
    for (const v of vendors) {
      if (v?.id) vendorById.set(String(v.id), v);
    }

    const selectedId =
      this.selectedUsageEntryId ?? shared("cockpit.usageEntryId") ?? null;
    const selectedEntry =
      entries.find((e) => e?.id === selectedId) ??
      (usageSnap?.primary
        ? entries.find((e) => e?.id === usageSnap.primary)
        : null) ??
      entries[0] ??
      null;

    const usageVendorIcon = (entry) => {
      const key = String(
        entry?.brand || entry?.id || entry?.name || ""
      ).toLowerCase();
      if (key.includes("antigravity")) return "✨";
      if (key.includes("opencode")) return "</>";
      if (key.includes("deepseek")) return "🐟";
      if (key.includes("anthropic") || key.includes("claude")) return "✦";
      if (key.includes("openai") || key.includes("codex")) return "◎";
      if (key.includes("gemini") || key.includes("google")) return "✧";
      if (key.includes("copilot") || key.includes("github")) return "◈";
      if (key.includes("cursor")) return "⌘";
      if (key.includes("zai") || key.includes("z.ai")) return "⚡";
      const rawIcon = String(entry?.icon ?? "").trim();
      if (
        rawIcon &&
        !/[\uE000-\uF8FF]/u.test(rawIcon) &&
        !/[\u{F0000}-\u{10FFFF}]/u.test(rawIcon)
      ) {
        return rawIcon;
      }
      return "◆";
    };

    const metricColor = (severity, pct) => {
      const s = String(severity ?? "").toLowerCase();
      const n = Number(pct ?? 0);
      if (s === "critical" || n >= 95) return t.blocked;
      if (s === "high" || n >= 80) return t.working;
      return t.accent;
    };

    const progressBar24 = (pct, col, hPx = 5, radius = 3, trackAlpha = "66") => {
      const clamped = Math.max(0, Math.min(100, Number(pct) || 0));
      const filled =
        clamped > 0 ? Math.max(1, Math.round((clamped / 100) * 24)) : 0;
      const trackCol = withAlpha(t.border, trackAlpha, t.background);
      const segs = [];
      for (let i = 0; i < 24; i++) {
        segs.push(
          div()
            .flex_1()
            .h(hPx)
            .bg(i < filled ? col : trackCol)
        );
      }
      return h_flex().gap(0).rounded(radius).overflow_hidden().children(segs);
    };

    const formatRemainingSecs = (secs) => {
      const total = Math.max(0, Math.floor(secs));
      const d = Math.floor(total / 86400);
      const h = Math.floor((total % 86400) / 3600);
      const m = Math.floor((total % 3600) / 60);
      if (d > 0) return `${d}d ${h}h`;
      if (h > 0) return `${h}h ${m}m`;
      return `${Math.max(1, m)}m`;
    };

    const parseMetricFooter = (sec) => {
      const raw = String(sec?.detail ?? "").trim();
      const parts = raw
        .split("·")
        .map((p) => p.trim())
        .filter(Boolean);

      let elapsedPct = null;
      let resetText = null;
      let pacingText = null;
      let isOver = false;
      const extraParts = [];

      for (const p of parts) {
        const elMatch = p.match(/(\d+(?:\.\d+)?)\s*%\s*elapsed/i);
        if (elMatch) {
          elapsedPct = Math.max(0, Math.min(100, Number(elMatch[1])));
          continue;
        }
        const resetMatch = p.match(/resets?\s+in\s+(.+)/i);
        if (resetMatch) {
          resetText = resetMatch[1].trim();
          continue;
        }
        if (/^\d+[dhms](?:\s+\d+[dhms])*$/i.test(p) && !resetText) {
          resetText = p;
          continue;
        }
        if (/\b(under|over)\b/i.test(p)) {
          pacingText = p;
          isOver = /\bover\b/i.test(p);
          continue;
        }
        extraParts.push(p);
      }

      if (sec?.reset_at) {
        const resetMs = Date.parse(sec.reset_at);
        if (Number.isFinite(resetMs)) {
          const remainingSecs = Math.max(0, (resetMs - Date.now()) / 1000);
          if (!resetText && remainingSecs > 0) {
            resetText = formatRemainingSecs(remainingSecs);
          }
          const winSecs = Number(sec?.window_secs ?? 0);
          if (elapsedPct === null && winSecs > 0) {
            elapsedPct = Math.max(
              0,
              Math.min(100, ((winSecs - remainingSecs) / winSecs) * 100)
            );
          }
        }
      }

      return {
        elapsedPct,
        resetText,
        pacingText,
        isOver,
        fallbackDetail:
          !resetText && !pacingText && extraParts.length > 0
            ? ellipsize(extraParts.join(" · "), 52)
            : null,
      };
    };

    const launchTabCmd = (tabName, cmdArgs) => {
      const sp = spaces().find((s) => s.active);
      run("tab.new", {
        space: sp?.id,
        name: tabName,
        argv: cmdArgs,
      });
    };

    const headerIconBtn = (id, label, onClick, active = false) =>
      h_flex()
        .id(id)
        .flex_none()
        .h(24)
        .w(24)
        .items_center()
        .justify_center()
        .rounded(6)
        .border(1)
        .border_color(
          active ? withAlpha(t.accent, "88", t.accent) : t.border
        )
        .bg(active ? t.selected : t.surface)
        .text_size(12)
        .text_color(active ? t.accent : t.muted)
        .cursor_pointer()
        .hover((s) => s.bg(t.hover).text_color(t.foreground))
        .on_mouse_down("left", (_e, clickCx) => clickCx.stop_propagation())
        .on_click(onClick)
        .child(label);

    const selEntryId = String(selectedEntry?.id ?? "");
    const selBrandKey = String(
      selectedEntry?.brand || selEntryId.split("@")[0] || ""
    );
    const selVendorMeta = vendorById.get(selBrandKey);

    const rawSections =
      Array.isArray(selectedEntry?.sections) && selectedEntry.sections.length > 0
        ? selectedEntry.sections
        : (Array.isArray(selectedEntry?.metrics)
            ? selectedEntry.metrics
            : []
          ).map((m) => ({
            type: "metric",
            ...m,
          }));
    const metricSections = rawSections.filter((s) => s && s.type === "metric");
    const textSections = rawSections.filter((s) => s && s.type === "text");

    let root = v_flex().id("ck-usage-popover").p(10).gap(8).bg(t.chrome);
    if (standalone) {
      root = root.rounded(10).border(1).border_color(t.border);
    }

    return root
      // 1. Compact Header (`AI Usage` + [↻] [⋯] [×])
      .child(
        h_flex()
          .gap(6)
          .items_center()
          .justify_between()
          .child(
            h_flex()
              .gap(6)
              .items_center()
              .child(div().text_size(13).text_color(t.accent).child("🧠"))
              .child(
                div()
                  .text_size(12.5)
                  .font_semibold()
                  .text_color(t.foreground)
                  .child("AI Usage")
              )
              .when(Boolean(selectedEntry?.stale), (el) =>
                el.child(this.badge(t, "STALE", t.working))
              )
          )
          .child(
            h_flex()
              .flex_none()
              .gap(4)
              .items_center()
              .child(
                headerIconBtn(
                  "ck-usage-refresh",
                  isFetching ? "…" : "↻",
                  (_e, clickCx) =>
                    this.fetchUsageSnapshot(clickCx ?? cx, true, false)
                )
              )
              .child(
                headerIconBtn(
                  "ck-usage-more",
                  "⋯",
                  (_e, clickCx) => {
                    this.usageMenuOpen = !this.usageMenuOpen;
                    (clickCx ?? cx)?.notify();
                  },
                  Boolean(this.usageMenuOpen)
                )
              )
              .child(
                headerIconBtn("ck-usage-close", "×", (_e, clickCx) =>
                  this.closeUsagePopup(clickCx ?? cx)
                )
              )
          )
      )
      // 2. Quick Actions Menu (`⋯`)
      .when(Boolean(this.usageMenuOpen), (el) =>
        el.child(
          h_flex()
            .gap(6)
            .flex_wrap()
            .items_center()
            .child(
              this.actionBtn(
                t,
                "ck-usage-detect",
                "🔍 Detectar Contas",
                (_e, clickCx) => {
                  this.usageMenuOpen = false;
                  this.fetchUsageSnapshot(clickCx ?? cx, true, true);
                },
                true
              )
            )
            .when(usageSnap?.installed !== false, (el2) =>
              el2.child(
                this.actionBtn(t, "ck-usage-tui", "🖥 Abrir TUI", () => {
                  this.usageMenuOpen = false;
                  launchTabCmd("ai-usagebar", [
                    usageSnap?.tui_binary_path || "ai-usagebar-tui",
                  ]);
                })
              )
            )
            .when(Boolean(selVendorMeta?.login), (el2) =>
              el2.child(
                this.actionBtn(t, "ck-usage-menu-login", "🔑 Login", () => {
                  this.usageMenuOpen = false;
                  launchTabCmd(
                    selBrandKey || "login",
                    String(selVendorMeta.login).trim().split(/\s+/)
                  );
                })
              )
            )
        )
      )
      // 3. Provider Sub-tabs (one selected account at a time)
      .when(usageSnap?.installed !== false && entries.length > 0, (el) =>
        el.child(
          h_flex()
            .gap(6)
            .flex_wrap()
            .items_center()
            .children(
              entries.map((entry, idx) => {
                const isSel = entry?.id === selectedEntry?.id;
                const tabName = ellipsize(
                  entry?.display_name || entry?.name || entry?.id || "AI",
                  16
                );
                const icon = usageVendorIcon(entry);
                return h_flex()
                  .id(`ck-usage-tab-${idx}`)
                  .flex_none()
                  .h(24)
                  .px(8)
                  .gap(4)
                  .items_center()
                  .justify_center()
                  .rounded(6)
                  .border(1)
                  .border_color(
                    isSel ? withAlpha(t.accent, "99", t.accent) : t.border
                  )
                  .bg(isSel ? t.selected : t.surface)
                  .text_size(11)
                  .font_semibold()
                  .text_color(isSel ? t.accent : t.muted)
                  .cursor_pointer()
                  .hover((s) =>
                    s
                      .bg(isSel ? t.selected : t.hover)
                      .text_color(t.foreground)
                  )
                  .on_mouse_down("left", (_e, clickCx) =>
                    clickCx.stop_propagation()
                  )
                  .on_click((_e, clickCx) => {
                    this.selectedUsageEntryId = entry?.id ?? null;
                    setShared("cockpit.usageEntryId", this.selectedUsageEntryId);
                    (clickCx ?? cx)?.notify();
                  })
                  .child(`${icon} ${tabName}`);
              })
            )
        )
      )
      // 4. Missing Binary State
      .when(Boolean(usageSnap && !usageSnap.installed), (el) =>
        el.child(
          v_flex()
            .p(10)
            .gap(6)
            .rounded(10)
            .border(1)
            .border_color(t.border)
            .bg(t.surface)
            .child(
              div()
                .text_size(11.5)
                .font_semibold()
                .text_color(t.blocked)
                .child("ai-usagebar não encontrado")
            )
            .child(
              div()
                .text_size(10.5)
                .text_color(t.muted)
                .child(
                  ellipsize(
                    usageSnap?.error ||
                      "Instale ai-usagebar para visualizar os limites.",
                    110
                  )
                )
            )
            .child(
              h_flex()
                .gap(6)
                .flex_wrap()
                .child(
                  this.actionBtn(
                    t,
                    "ck-usage-inst-binstall",
                    "cargo binstall",
                    () =>
                      launchTabCmd("install-usagebar", [
                        "cargo",
                        "binstall",
                        "ai-usagebar",
                      ]),
                    true
                  )
                )
                .child(
                  this.actionBtn(t, "ck-usage-inst-yay", "yay AUR", () =>
                    launchTabCmd("install-usagebar", [
                      "yay",
                      "-S",
                      "ai-usagebar-bin",
                    ])
                  )
                )
            )
        )
      )
      // 5. Loading State
      .when(!usageSnap && isFetching, (el) =>
        el.child(
          v_flex()
            .p(10)
            .rounded(10)
            .border(1)
            .border_color(t.border)
            .bg(t.surface)
            .child(
              div()
                .text_size(11.5)
                .text_color(t.muted)
                .child("Consultando limites no ai-usagebar…")
            )
        )
      )
      // 6. Empty State (installed, 0 entries)
      .when(Boolean(usageSnap?.installed && entries.length === 0), (el) =>
        el.child(
          v_flex()
            .p(10)
            .gap(6)
            .rounded(10)
            .border(1)
            .border_color(t.border)
            .bg(t.surface)
            .child(
              div()
                .text_size(11.5)
                .font_semibold()
                .text_color(t.foreground)
                .child("Nenhuma conta habilitada no ai-usagebar.")
            )
            .child(
              h_flex()
                .gap(6)
                .flex_wrap()
                .child(
                  this.actionBtn(
                    t,
                    "ck-usage-empty-detect",
                    "🔍 Detectar Contas",
                    (_e, clickCx) =>
                      this.fetchUsageSnapshot(clickCx ?? cx, true, true),
                    true
                  )
                )
                .child(
                  this.actionBtn(t, "ck-usage-empty-tui", "⚙ Abrir TUI", () =>
                    launchTabCmd("ai-usagebar", [
                      usageSnap?.tui_binary_path || "ai-usagebar-tui",
                    ])
                  )
                )
            )
        )
      )
      // 7. Single Selected Account Card
      .when(Boolean(usageSnap?.installed && selectedEntry), (el) =>
        el.child(
          v_flex()
            .p(10)
            .gap(9)
            .rounded(10)
            .border(1)
            .border_color(t.border)
            .bg(t.surface)
            .when(Boolean(selectedEntry?.error), (el2) =>
              el2.child(
                v_flex()
                  .gap(6)
                  .child(
                    div()
                      .text_size(11)
                      .text_color(t.blocked)
                      .child(ellipsize(selectedEntry.error, 140))
                  )
                  .when(Boolean(selVendorMeta?.login), (el3) =>
                    el3.child(
                      h_flex().child(
                        this.actionBtn(
                          t,
                          "ck-usage-entry-login",
                          `🔑 ${selVendorMeta.login}`,
                          () =>
                            launchTabCmd(
                              selBrandKey || "login",
                              String(selVendorMeta.login).trim().split(/\s+/)
                            )
                        )
                      )
                    )
                  )
              )
            )
            .when(!selectedEntry?.error && metricSections.length > 0, (el2) =>
              el2.children(
                metricSections.map((sec, mIdx) => {
                  const rawPct = Number(sec?.percent ?? 0);
                  const pct = Math.max(
                    0,
                    Math.min(
                      100,
                      Number.isFinite(rawPct) ? Math.round(rawPct) : 0
                    )
                  );
                  const col = metricColor(sec?.severity, pct);
                  const rightVal =
                    sec?.headline === "value" && sec?.value
                      ? `${sec.value} (${pct}%)`
                      : `${pct}%`;
                  const {
                    elapsedPct,
                    resetText,
                    pacingText,
                    isOver,
                    fallbackDetail,
                  } = parseMetricFooter(sec);
                  const leftFooter = resetText
                    ? `⏱ ${resetText}`
                    : fallbackDetail || "";
                  const rightFooter = pacingText
                    ? `${isOver ? "⚠" : "✓"} ${pacingText}`
                    : "";

                  return v_flex()
                    .id(`ck-usage-metric-${mIdx}`)
                    .gap(4)
                    .child(
                      h_flex()
                        .gap(6)
                        .items_center()
                        .justify_between()
                        .child(
                          div()
                            .text_size(11.5)
                            .font_semibold()
                            .text_color(t.foreground)
                            .child(ellipsize(sec?.label || "Quota", 28))
                        )
                        .child(
                          div()
                            .flex_none()
                            .text_size(11.5)
                            .font_semibold()
                            .text_color(pct >= 80 ? col : t.foreground)
                            .child(ellipsize(rightVal, 20))
                        )
                    )
                    .child(
                      v_flex()
                        .gap(2)
                        .child(progressBar24(pct, col, 5, 3, "66"))
                        .when(elapsedPct !== null, (el3) =>
                          el3.child(progressBar24(elapsedPct, t.muted, 2, 1, "30"))
                        )
                    )
                    .when(Boolean(leftFooter || rightFooter), (el3) =>
                      el3.child(
                        h_flex()
                          .gap(6)
                          .items_center()
                          .justify_between()
                          .child(
                            div()
                              .text_size(10.5)
                              .text_color(t.muted)
                              .child(ellipsize(leftFooter, 36))
                          )
                          .child(
                            div()
                              .flex_none()
                              .text_size(10.5)
                              .text_color(isOver ? t.working : t.muted)
                              .child(ellipsize(rightFooter, 24))
                          )
                      )
                    );
                })
              )
            )
            .when(
              !selectedEntry?.error &&
                metricSections.length === 0 &&
                textSections.length > 0,
              (el2) =>
                el2.children(
                  textSections.slice(0, 4).map((sec) =>
                    h_flex()
                      .gap(6)
                      .items_center()
                      .justify_between()
                      .child(
                        div()
                          .text_size(11)
                          .text_color(t.muted)
                          .child(ellipsize(sec?.label || "Info", 22))
                      )
                      .child(
                        div()
                          .text_size(11)
                          .font_semibold()
                          .text_color(t.foreground)
                          .child(ellipsize(sec?.value ?? "", 34))
                      )
                  )
                )
            )
            .when(
              !selectedEntry?.error &&
                metricSections.length === 0 &&
                textSections.length === 0,
              (el2) =>
                el2.child(
                  div()
                    .text_size(11)
                    .text_color(t.muted)
                    .child("Sem métricas reportadas para esta conta.")
                )
            )
        )
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
    if (addr && this.lastInspectedAddr !== addr) {
      this.lastInspectedAddr = addr;
      this.refreshInspector(cx);
    }
    const agent =
      merged.find((a) => a.address === addr) ?? merged[0] ?? null;
    const detailed =
      (addr && this.inspectCache[addr]?.agent) ||
      (snap?.agents ?? []).find((a) => a.address === agent?.address) ||
      agent;
    const rawView =
      shared("cockpit.viewMode") || this.inspectorTab || "inspector";
    const activeView = rawView === "usage" ? "inspector" : rawView;
    const usageOpen = Boolean(shared("cockpit.usageOpen") ?? this.usageOpen);
    const usageOnlyDrawer = Boolean(
      shared("cockpit.usageOnlyDrawer") ?? this.usageOnlyDrawer
    );

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
          : t.working;
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
      .filter((line) => {
        const trimmed = line.trim();
        if (trimmed.length === 0) return false;
        if (/^[\s│┃╎╏¦|─━╭╮╯╰┌┐┘└├┤┬┴┼═║╔╗╚╝]+$/.test(line)) return false;
        if (
          trimmed.includes("Monitorando sessão ativa do agente.") ||
          trimmed.includes("Nenhuma auto-resposta registrada") ||
          trimmed.includes("OUTPUT PREVIEW (LAST 25 LINES)") ||
          trimmed.includes("AUTO-REPLY AUDIT LOG") ||
          trimmed.includes("DURABLE SUPERVISOR") ||
          /^t\w+\s+\(.+\)\s+\[(WORKING|BLOCKED|IDLE|COMPLETED)\]$/i.test(trimmed)
        ) {
          return false;
        }
        return true;
      })
      .slice(-25);

    const sectionTitle = (title, subtitle = null) =>
      this.sectionTitle(t, title, subtitle);

    const pr = detailed?.context?.pull_request;
    const issues = detailed?.context?.issues ?? [];
    const slackThreads = detailed?.context?.slack_threads ?? [];
    const runbooks = detailed?.context?.runbooks ?? [];
    const labels = agent?.labels ?? {};

    const agentStatusLabel = agent
      ? agent.status === "blocked"
        ? (agent.kind ?? "NEEDS YOU").toUpperCase()
        : agent.status === "working"
          ? "WORKING"
          : agent.status === "done" || agent.lifecycle?.state === "finished"
            ? "DONE"
            : "IDLE"
      : "IDLE";
    const agentStatusColor = agent
      ? agent.status === "blocked"
        ? t.blocked
        : agent.status === "working"
          ? t.working
          : agent.status === "done" || agent.lifecycle?.state === "finished"
            ? t.done
            : t.muted
      : t.muted;

    const corrTier = detailed?.correlation_tier ?? "local_only";
    const corrTierLabel =
      corrTier === "exact"
        ? "Exact"
        : corrTier === "inferred"
          ? "Inferred"
          : corrTier === "ambiguous"
            ? "Ambiguous"
            : "Local / No PR";
    const corrTierColor =
      corrTier === "exact"
        ? t.done
        : corrTier === "inferred"
          ? t.accent
          : corrTier === "ambiguous"
            ? t.blocked
            : t.muted;

    if (usageOpen && usageOnlyDrawer) {
      return v_flex()
        .id("ck-inspector-root")
        .size_full()
        .track_focus(this.handle)
        .text_color(t.foreground)
        .justify_end()
        .on_key_down((e, cx) => this.handleKeyDown(e, cx, agent))
        .child(div().flex_1())
        .child(this.renderUsagePopover(cx, t, true));
    }

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
      .on_key_down((e, cx) => this.handleKeyDown(e, cx, agent))
      .child(
        // Top Drawer Header (40px)
        h_flex()
          .h(40)
          .px(12)
          .gap(6)
          .items_center()
          .border_b(1)
          .border_color(t.border)
          .bg(t.chrome)
          .child(
            this.segmentedTrack(t)
              .child(
                this.segmentBtn(
                  t,
                  "ck-ins-tab-home",
                  "🏠 Home",
                  activeView === "home",
                  (_e, cx) => {
                    setShared("cockpit.viewMode", "home");
                    setShared("cockpit.usageOnlyDrawer", false);
                    this.inspectorTab = "home";
                    this.fetchHomeSnapshot(cx);
                    cx.notify();
                  }
                )
              )
              .child(
                this.segmentBtn(
                  t,
                  "ck-ins-tab-inspector",
                  "🔍 Inspector",
                  activeView === "inspector",
                  (_e, cx) => {
                    setShared("cockpit.viewMode", "inspector");
                    setShared("cockpit.usageOnlyDrawer", false);
                    this.inspectorTab = "inspector";
                    this.refreshInspector(cx);
                    cx.notify();
                  }
                )
              )
              .child(
                this.segmentBtn(
                  t,
                  "ck-ins-tab-doctor",
                  "🩺 Doctor",
                  activeView === "doctor",
                  (_e, cx) => {
                    setShared("cockpit.viewMode", "doctor");
                    setShared("cockpit.usageOnlyDrawer", false);
                    this.inspectorTab = "doctor";
                    if (!this.doctorReport) this.runDoctorCheck(cx);
                    cx.notify();
                  }
                )
              )
          )
          .child(div().flex_1())
          .child(
            this.actionBtn(t, "ck-ins-refresh", "↻", (_e, cx) => {
              this.refreshCockpit(cx, true);
              this.refreshInspector(cx);
              this.fetchHomeSnapshot(cx);
              if (usageOpen) this.fetchUsageSnapshot(cx, true, false);
              if (activeView === "doctor") this.runDoctorCheck(cx);
            })
          )
          .child(
            this.actionBtn(t, "ck-ins-close", "×", () => {
              this.usageOpen = false;
              this.usageOnlyDrawer = false;
              this.usageMenuOpen = false;
              setShared("cockpit.usageOpen", false);
              setShared("cockpit.usageOnlyDrawer", false);
              run("layout.hide", { id: "cockpit-inspector" });
            })
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
            .text_size(11.5)
            .text_color(t.accent)
            .justify_between()
            .child(ellipsize(this.note, 56))
            .child(
              div()
                .id("ck-clear-note")
                .cursor_pointer()
                .text_color(t.muted)
                .on_click((_e, cx) => {
                  this.note = "";
                  cx.notify();
                })
                .child("dismiss")
            )
        )
      )
      .when(activeView === "home", (el) =>
        el.child(this.renderHomeBody(cx, t))
      )
      .when(activeView === "doctor", (el) =>
        el.child(this.renderDoctorBody(cx, t))
      )
      .when(activeView === "inspector", (el) =>
        el.child(
          v_flex()
            .flex_1()
            .min_h(0)
            .overflow_y_scrollbar()
            .p(12)
            .gap(10)
            .when(!agent, (el2) =>
              el2.child(
                this.cardBox(t)
                  .rounded(10)
                  .child(
                    div()
                      .text_size(12)
                      .text_color(t.muted)
                      .child(
                        "Select an agent or open a terminal to inspect its context."
                      )
                  )
              )
            )
            .when(Boolean(agent), (el2) =>
              el2
                // 1. Selected Agent Telemetry Header Card
                .child(
                  v_flex()
                    .p(11)
                    .gap(8)
                    .rounded(10)
                    .border(1)
                    .border_color(t.border)
                    .bg(t.surface)
                    .child(
                      h_flex()
                        .gap(6)
                        .items_center()
                        .child(
                          div()
                            .text_size(12.5)
                            .font_semibold()
                            .text_color(t.address ?? t.accent)
                            .child(agent.address)
                        )
                        .child(
                          div()
                            .text_size(12.5)
                            .font_semibold()
                            .text_color(t.foreground)
                            .child(ellipsize(agent.program_label || "agent", 14))
                        )
                        .child(
                          div()
                            .flex_1()
                            .min_w(0)
                            .text_size(12)
                            .text_color(t.muted)
                            .child(
                              ellipsize(
                                `· ${agent.project_name || agent.name}`,
                                24
                              )
                            )
                        )
                        .child(
                          this.pillBadge(t, agentStatusLabel, agentStatusColor)
                        )
                    )
                    .child(
                      h_flex()
                        .gap(8)
                        .items_center()
                        .text_size(11)
                        .text_color(t.muted)
                        .child(`branch: ⎇ ${ellipsize(agent.branch ?? "none", 22)}`)
                        .child("·")
                        .child(`cwd: ${ellipsize(agent.cwd ?? "", 36)}`)
                    )
                    .child(
                      h_flex()
                        .gap(5)
                        .flex_wrap()
                        .child(
                          this.actionBtn(
                            t,
                            "ck-ctl-focus",
                            "Focus Terminal",
                            () => {
                              void this.focusOrAttachTerminal(
                                cx,
                                agent,
                                false
                              );
                            },
                            true
                          )
                        )
                        .when(!agent.pane, (el) =>
                          el.child(
                            this.actionBtn(
                              t,
                              "ck-ctl-attach",
                              "Attach Tab",
                              () => {
                                void this.focusOrAttachTerminal(
                                  cx,
                                  agent,
                                  true
                                );
                              }
                            )
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
                          this.actionBtn(
                            t,
                            "ck-ctl-int",
                            "Interrupt",
                            (_e, cx) => {
                              hc(["interrupt", agent.address]);
                              this.note = `Interrupted ${agent.address}`;
                              cx.notify();
                            }
                          )
                        )
                        .child(
                          this.actionBtn(t, "ck-ctl-split", "Split", () => {
                            if (agent.pane)
                              run("pane.split", {
                                pane: agent.pane,
                                direction: "down",
                              });
                          })
                        )
                        .child(
                          this.actionBtn(t, "ck-ctl-kill", "Kill", (_e, cx) => {
                            hc(["kill", agent.address]);
                            this.note = `Killed ${agent.address}`;
                            this.refreshCockpit(cx, true);
                            this.tickSupervisor(cx, true);
                            cx.notify();
                          })
                        )
                    )
                    .child(
                      h_flex()
                        .gap(6)
                        .items_center()
                        .child(
                          div()
                            .id("ck-prompt-box")
                            .flex_1()
                            .min_w(0)
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
                                ? ellipsize(this.promptText, 48) +
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
                // 2. Supervisor Action Card / Status (SUPERVISOR & GUARDRAILS)
                .child(sectionTitle("SUPERVISOR & GUARDRAILS"))
                .child(
                  v_flex()
                    .p(10)
                    .gap(4)
                    .rounded(8)
                    .border(1)
                    .border_color(t.border)
                    .bg(t.background)
                    .child(
                      h_flex()
                        .justify_between()
                        .items_center()
                        .gap(6)
                        .child(
                          div()
                            .text_size(11.5)
                            .font_semibold()
                            .text_color(t.foreground)
                            .child(
                              `🧠 ${supSnap?.llm_model || "lfm3:1.7b"} (${supSnap?.llm_backend || "ollama"} · ${gpuOn(supSnap) ? "GPU" : "CPU"}${llmMemoryLabel(supSnap) ? ` · ${llmMemoryLabel(supSnap)}` : ""})`
                            )
                        )
                        .child(
                          this.pillBadge(
                            t,
                            supSnap?.llm_connected
                              ? supSnap?.model_loaded
                                ? "CONECTADO · EM RAM"
                                : supSnap?.model_installed
                                  ? "CONECTADO · PRONTO"
                                  : "MODELO AUSENTE"
                              : supSnap
                                ? "OFFLINE"
                                : "VERIFICANDO…",
                            supSnap?.llm_connected
                              ? supSnap?.model_loaded
                                ? t.accent
                                : supSnap?.model_installed
                                  ? t.done
                                  : t.working
                              : supSnap
                                ? t.blocked
                                : t.muted
                          )
                        )
                    )
                    .child(
                      div()
                        .text_size(11)
                        .text_color(t.muted)
                        .child(
                          ellipsize(
                            supSnap?.llm_status_detail ||
                              "Verificando conexão com o motor LLM local…",
                            76
                          )
                        )
                    )
                )
                .when(Boolean(actionCard), (el3) =>
                  el3.child(
                    v_flex()
                      .p(11)
                      .gap(8)
                      .rounded(10)
                      .border(1)
                      .border_color(withAlpha(urgencyColor, "77", urgencyColor))
                      .bg(t.surface)
                      .child(
                        h_flex()
                          .gap(6)
                          .items_center()
                          .justify_between()
                          .child(
                            this.pillBadge(
                              t,
                              `Urgency::${urgencyLabel}`,
                              urgencyColor
                            )
                          )
                          .child(
                            div()
                              .text_size(10.5)
                              .text_color(t.muted)
                              .child("Action Card")
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
                      .when(Boolean(actionCard.updated_summary), (el4) =>
                        el4.child(
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
                          .child("hc_command_payload (editável):")
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
                            h_flex()
                              .id("ck-action-approve")
                              .flex_1()
                              .h(26)
                              .px(10)
                              .items_center()
                              .justify_center()
                              .rounded(6)
                              .border(1)
                              .border_color(withAlpha(t.done, "66", t.done))
                              .bg(withAlpha(t.done, "24", t.raised))
                              .text_size(11.5)
                              .font_semibold()
                              .text_color(t.done)
                              .cursor_pointer()
                              .hover((s) =>
                                s.bg(withAlpha(t.done, "38", t.hover))
                              )
                              .on_mouse_down("left", (_e, cx) =>
                                cx.stop_propagation()
                              )
                              .on_click((_e, cx) =>
                                this.approveActionCard(cx, agent.address)
                              )
                              .child("Aprovar e Enviar")
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
                .when(!actionCard, (el3) =>
                  el3.child(
                    v_flex()
                      .p(11)
                      .gap(7)
                      .rounded(10)
                      .border(1)
                      .border_color(t.border)
                      .bg(t.surface)
                      .child(
                        h_flex()
                          .justify_between()
                          .items_center()
                          .gap(6)
                          .child(
                            div()
                              .text_size(12)
                              .font_semibold()
                              .text_color(t.done)
                              .child("✓ Sem Action Cards pendentes")
                          )
                          .child(
                            this.pillBadge(
                              t,
                              auditBadge || "Auto-assisted",
                              t.accent
                            )
                          )
                      )
                      .when(Boolean(rollingSummary), (el4) =>
                        el4.child(
                          div()
                            .text_size(11.5)
                            .text_color(t.muted)
                            .child(rollingSummary)
                        )
                      )
                      .child(
                        h_flex()
                          .justify_end()
                          .child(
                            this.actionBtn(
                              t,
                              "ck-audit-eval",
                              "⚡ Evaluate Now",
                              (_e, cx) => this.tickSupervisor(cx, true)
                            )
                          )
                      )
                  )
                )
                // 3. Output Preview (always visible when an agent is selected)
                .child(sectionTitle("OUTPUT PREVIEW · LAST 25 LINES · SANITIZED"))
                .child(
                  v_flex()
                    .p(10)
                    .gap(3)
                    .rounded(8)
                    .border(1)
                    .border_color(t.border)
                    .bg(t.background)
                    .when(cleanedPreviewLines.length === 0, (el3) =>
                      el3.child(
                        div()
                          .text_size(11.5)
                          .text_color(t.dim)
                          .child(
                            "Nenhuma saída recente capturada para este terminal."
                          )
                      )
                    )
                    .children(
                      cleanedPreviewLines.map((line, idx) => {
                        const isCmd = line.trim().startsWith("$");
                        const isWarn =
                          line.includes("⚠") || line.includes("Interceptado");
                        const isOk = line.includes("✓");
                        const lineCol = isWarn
                          ? t.blocked
                          : isOk
                            ? t.done
                            : isCmd
                              ? t.foreground
                              : t.muted;
                        return h_flex()
                          .id(`ck-preview-line-${idx}`)
                          .gap(8)
                          .items_start()
                          .child(
                            div()
                              .w(20)
                              .flex_none()
                              .text_size(10.5)
                              .text_color(t.dim)
                              .child(String(idx + 1))
                          )
                          .child(
                            div()
                              .flex_1()
                              .min_w(0)
                              .text_size(11)
                              .text_color(lineCol)
                              .child(ellipsize(line, 92))
                          );
                      })
                    )
                )
                // 4. Auto-Reply Audit Log
                .child(
                  sectionTitle(
                    "AUTO-REPLY AUDIT LOG",
                    `${auditLog.length} eventos`
                  )
                )
                .child(
                  v_flex()
                    .p(10)
                    .gap(6)
                    .rounded(10)
                    .border(1)
                    .border_color(t.border)
                    .bg(t.surface)
                    .when(auditLog.length === 0, (el3) =>
                      el3.child(
                        div()
                          .text_size(11.5)
                          .text_color(t.dim)
                          .child(
                            "Nenhuma auto-resposta registrada para este agente."
                          )
                      )
                    )
                    .children(
                      auditLog.map((entry, idx) => {
                        const dec = entry.decision || "AutoReply";
                        const decCol =
                          dec === "AlertOperator"
                            ? t.blocked
                            : dec === "ApprovedByHuman"
                              ? t.done
                              : dec === "IgnoredByHuman"
                                ? t.muted
                                : t.accent;
                        return v_flex()
                          .id(`ck-audit-entry-${idx}`)
                          .p(8)
                          .gap(4)
                          .rounded(6)
                          .border(1)
                          .border_color(t.border)
                          .bg(t.background)
                          .child(
                            h_flex()
                              .justify_between()
                              .items_center()
                              .gap(6)
                              .child(
                                div()
                                  .text_size(11)
                                  .font_semibold()
                                  .text_color(t.accent)
                                  .child(
                                    `${entry.agent_id ?? agent.address}`
                                  )
                              )
                              .child(this.pillBadge(t, dec, decCol))
                              .child(
                                div()
                                  .text_size(10.5)
                                  .text_color(t.dim)
                                  .child(formatTimestamp(entry.timestamp))
                              )
                          )
                          .child(
                            div()
                              .text_size(11)
                              .text_color(t.foreground)
                              .child(
                                ellipsize(
                                  `payload: "${entry.payload ?? ""}"`,
                                  64
                                )
                              )
                          )
                          .when(
                            Boolean(entry.reason || entry.summary),
                            (el3) =>
                              el3.child(
                                div()
                                  .text_size(10.5)
                                  .text_color(t.muted)
                                  .child(
                                    ellipsize(
                                      entry.reason || entry.summary,
                                      72
                                    )
                                  )
                              )
                          );
                      })
                    )
                )
                // 5. Read-Only Correlation
                .child(
                  h_flex()
                    .pt(10)
                    .pb(5)
                    .gap(6)
                    .items_center()
                    .justify_between()
                    .border_b(1)
                    .border_color(t.border)
                    .child(
                      h_flex()
                        .gap(6)
                        .items_center()
                        .child(div().size(6).rounded(3).bg(t.accent))
                        .child(
                          div()
                            .text_size(11.5)
                            .font_semibold()
                            .text_color(t.foreground)
                            .child("CORRELAÇÃO READ-ONLY")
                        )
                    )
                    .child(this.pillBadge(t, corrTierLabel, corrTierColor))
                )
                .child(
                  v_flex()
                    .p(10)
                    .gap(8)
                    .rounded(10)
                    .border(1)
                    .border_color(t.border)
                    .bg(t.surface)
                    // Pull Request & CI
                    .child(
                      pr
                        ? v_flex()
                            .p(8)
                            .gap(5)
                            .rounded(6)
                            .border(1)
                            .border_color(t.border)
                            .bg(t.background)
                            .child(
                              h_flex()
                                .justify_between()
                                .items_center()
                                .gap(6)
                                .child(
                                  div()
                                    .flex_1()
                                    .min_w(0)
                                    .text_size(12)
                                    .font_semibold()
                                    .text_color(t.foreground)
                                    .child(
                                      ellipsize(
                                        `PR #${pr.number} · ${pr.title}`,
                                        40
                                      )
                                    )
                                )
                                .child(
                                  this.actionBtn(
                                    t,
                                    "ck-open-pr",
                                    "Open ↗",
                                    () => openExternalUrl(pr.url)
                                  )
                                )
                            )
                            .child(
                              h_flex()
                                .gap(6)
                                .items_center()
                                .child(
                                  this.pillBadge(
                                    t,
                                    pr.state ?? "OPEN",
                                    t.foreground
                                  )
                                )
                                .child(
                                  this.pillBadge(
                                    t,
                                    `CI: ${pr.ci_rollup ?? "PENDING"}`,
                                    pr.ci_rollup === "FAILURE"
                                      ? t.blocked
                                      : t.done
                                  )
                                )
                            )
                            .children(
                              (pr.checks ?? []).slice(0, 5).map((chk, idx) =>
                                h_flex()
                                  .id(`ck-chk-${idx}`)
                                  .justify_between()
                                  .items_center()
                                  .text_size(11)
                                  .child(
                                    div()
                                      .flex_1()
                                      .min_w(0)
                                      .text_color(
                                        chk.status === "FAILURE"
                                          ? t.blocked
                                          : t.muted
                                      )
                                      .child(
                                        ellipsize(
                                          `• ${chk.name} (${chk.status})`,
                                          44
                                        )
                                      )
                                  )
                                  .when(Boolean(chk.details_url), (el3) =>
                                    el3.child(
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
                        : div()
                            .p(8)
                            .rounded(6)
                            .border(1)
                            .border_color(t.border)
                            .bg(t.background)
                            .text_size(11.5)
                            .text_color(t.dim)
                            .child(
                              "No open Pull/Merge Request linked to this branch."
                            )
                    )
                    // Jira / Linear Issues
                    .when(issues.length > 0, (el3) =>
                      el3.children(
                        issues.map((iss, i) =>
                          h_flex()
                            .id(`ck-iss-${i}`)
                            .p(8)
                            .gap(6)
                            .justify_between()
                            .items_center()
                            .rounded(6)
                            .border(1)
                            .border_color(t.border)
                            .bg(t.background)
                            .child(
                              div()
                                .flex_1()
                                .min_w(0)
                                .text_size(11.5)
                                .text_color(t.foreground)
                                .child(
                                  ellipsize(
                                    `${iss.key} · ${iss.title} [${iss.status}]`,
                                    44
                                  )
                                )
                            )
                            .when(Boolean(iss.url), (e) =>
                              e.child(
                                this.actionBtn(
                                  t,
                                  `ck-iss-open-${i}`,
                                  "Open ↗",
                                  () => openExternalUrl(iss.url)
                                )
                              )
                            )
                        )
                      )
                    )
                    // Slack Threads
                    .when(slackThreads.length > 0, (el3) =>
                      el3.children(
                        slackThreads.map((th, i) =>
                          h_flex()
                            .id(`ck-slk-${i}`)
                            .p(8)
                            .gap(6)
                            .justify_between()
                            .items_center()
                            .rounded(6)
                            .border(1)
                            .border_color(t.border)
                            .bg(t.background)
                            .child(
                              div()
                                .flex_1()
                                .min_w(0)
                                .text_size(11.5)
                                .text_color(t.foreground)
                                .child(
                                  ellipsize(
                                    `Slack #${th.channel_id} (${th.reply_count ?? 0} replies)`,
                                    42
                                  )
                                )
                            )
                            .child(
                              this.actionBtn(
                                t,
                                `ck-slk-open-${i}`,
                                "Thread ↗",
                                () => openExternalUrl(th.url)
                              )
                            )
                        )
                      )
                    )
                    // Local Runbooks
                    .when(runbooks.length > 0, (el3) =>
                      el3.children(
                        runbooks.map((rb, i) =>
                          v_flex()
                            .id(`ck-rb-${i}`)
                            .p(8)
                            .gap(2)
                            .rounded(6)
                            .border(1)
                            .border_color(t.border)
                            .bg(t.background)
                            .child(
                              h_flex()
                                .justify_between()
                                .items_center()
                                .child(
                                  div()
                                    .text_size(11.5)
                                    .font_semibold()
                                    .text_color(t.foreground)
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
                                .text_size(11)
                                .text_color(t.muted)
                                .child(ellipsize(rb.path, 52))
                            )
                        )
                      )
                    )
                    // Labels (`cockpit.*`) + Segmented Tagger
                    .child(
                      v_flex()
                        .p(8)
                        .gap(6)
                        .rounded(6)
                        .border(1)
                        .border_color(t.border)
                        .bg(t.background)
                        .when(
                          Object.keys(labels).some((k) =>
                            k.startsWith("cockpit.")
                          ),
                          (el3) =>
                            el3.child(
                              h_flex()
                                .gap(4)
                                .flex_wrap()
                                .children(
                                  Object.entries(labels)
                                    .filter(([k]) => k.startsWith("cockpit."))
                                    .map(([k, v]) =>
                                      h_flex()
                                        .id(`ck-lbl-chip-${k}`)
                                        .px(7)
                                        .py(2)
                                        .gap(5)
                                        .items_center()
                                        .rounded(999)
                                        .bg(t.surface)
                                        .border(1)
                                        .border_color(t.border)
                                        .text_size(10.5)
                                        .child(
                                          `${k.replace("cockpit.", "")}=${v}`
                                        )
                                        .child(
                                          div()
                                            .id(`ck-lbl-del-${k}`)
                                            .cursor_pointer()
                                            .text_color(t.dim)
                                            .hover((s) =>
                                              s.text_color(t.blocked)
                                            )
                                            .on_click((_e, cx) =>
                                              this.applyLabelGui(
                                                cx,
                                                agent.address,
                                                k,
                                                ""
                                              )
                                            )
                                            .child("×")
                                        )
                                    )
                                )
                            )
                        )
                        .child(
                          this.segmentedTrack(t).children(
                            ["issue", "pr", "slack", "runbook"].map((k) =>
                              this.segmentBtn(
                                t,
                                `ck-lbl-type-${k}`,
                                k,
                                this.labelKey === k,
                                (_e, cx) => {
                                  this.labelKey = k;
                                  this.editingField = "label";
                                  this.handle.focus();
                                  cx.notify();
                                }
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
                                .min_w(0)
                                .px(8)
                                .py(4)
                                .rounded(6)
                                .border(1)
                                .border_color(
                                  this.editingField === "label"
                                    ? t.accent
                                    : t.border
                                )
                                .bg(t.surface)
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
                                        (this.editingField === "label"
                                          ? "▏"
                                          : "")
                                    : (this.editingField === "label"
                                        ? "▏"
                                        : "") +
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
                )
            )
        )
      )
      .when(usageOpen, (el) =>
        el.child(
          v_flex()
            .flex_none()
            .border_t(1)
            .border_color(t.border)
            .child(this.renderUsagePopover(cx, t, false))
        )
      );
  }

  renderStatus(cx = null) {
    const t = theme();
    const space = spaces().find((s) => s.active);
    const list = agents();
    const allTerms = terminals();
    const snap = shared("cockpit.snapshot");
    const supSnap = shared("supervisor.snapshot");
    const merged = mergeFleetRows(list, allTerms, snap, supSnap);
    const liveMerged = merged.filter(
      (a) => a.lifecycle?.state !== "finished" && a.state !== "finished"
    );
    const providers = snap?.providers ?? {};
    const activeProviders = Object.entries(providers).filter(([, ph]) => {
      const st = String(ph?.status ?? ph?.state ?? "").toLowerCase();
      return st === "active" || st === "ready";
    });
    const llmModel = supSnap?.llm_model || "lfm3:1.7b";
    const llmConnected = supSnap ? Boolean(supSnap.llm_connected) : false;
    const numGpu = supSnap?.num_gpu ?? 0;
    const blockedCount = liveMerged.filter((a) => a.status === "blocked").length;
    const workingCount = liveMerged.filter((a) => a.status === "working").length;
    const modeLabel = supSnap?.mode_label ?? "Semi-Auto (Whitelist)";
    const inspectorHidden = Boolean(
      findNode(layout(), "cockpit-inspector")?.hidden
    );
    const onlyUsage = Boolean(
      shared("cockpit.usageOnlyDrawer") ?? this.usageOnlyDrawer
    );
    const usageOpen = Boolean(shared("cockpit.usageOpen") ?? this.usageOpen);
    const rawView =
      shared("cockpit.viewMode") || this.inspectorTab || "inspector";
    const rightView = rawView === "usage" ? "inspector" : rawView;
    const mac = win()?.platform === "macos";
    const b = keys().find(
      (k) => k.section === "global" && k.action === "app.palette"
    );
    const paletteHint = b ? b.keys.toUpperCase() : mac ? "⌘K" : "CTRL+SHIFT+P";

    const viewLink = (id, label, tab) =>
      div()
        .id(id)
        .cursor_pointer()
        .text_color(
          !inspectorHidden && !onlyUsage && rightView === tab
            ? t.accent
            : t.muted
        )
        .hover((s) => s.text_color(t.foreground))
        .on_click((_e, clickCx) => this.openDrawerView(clickCx ?? cx, tab, true))
        .child(label);

    return h_flex()
      .size_full()
      .px(12)
      .gap(10)
      .items_center()
      .bg(t.chrome)
      .border_t(1)
      .border_color(t.border)
      .text_size(11)
      .text_color(t.muted)
      // Left side
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
              .child(`Cockpit v${version}`)
          )
      )
      .child(
        div()
          .id("ck-status-mode")
          .px(6)
          .py(1)
          .rounded(4)
          .bg(t.raised)
          .border(1)
          .border_color(t.border)
          .cursor_pointer()
          .hover((s) => s.bg(t.hover).text_color(t.foreground))
          .on_click((_e, clickCx) =>
            this.runSupervisorAction(clickCx ?? cx, ["supervisor", "mode", "cycle"])
          )
          .child(`MODE: ${modeLabel}`)
      )
      .child(
        div()
          .text_color(llmConnected ? t.done : supSnap ? t.blocked : t.muted)
          .child(
            `LLM: ${llmModel} (${llmConnected ? "ONLINE" : supSnap ? "OFFLINE" : "…"} · ${gpuOn(supSnap) ? "GPU" : "CPU num_gpu=0"})`
          )
      )
      .child(
        div()
          .text_color(t.accent)
          .child(`⎇ ${ellipsize(space?.branch ?? "main", 22)}`)
      )
      .child(div().child(`${liveMerged.length} agents (${workingCount} working)`))
      .when(activeProviders.length > 0, (el) =>
        el.children(
          activeProviders.map(([prov, ph]) =>
            this.pillBadge(
              t,
              `${prov}: ${String(ph?.status ?? ph?.state ?? "active").toUpperCase()}`,
              t.done
            )
          )
        )
      )
      .when(activeProviders.length === 0, (el) =>
        el.child(this.pillBadge(t, "Local-First Read-Only", t.muted))
      )
      .child(div().flex_1())
      // Right side: Quick view links
      .child(
        div()
          .id("ck-status-usage")
          .cursor_pointer()
          .text_color(!inspectorHidden && usageOpen ? t.accent : t.muted)
          .hover((s) => s.text_color(t.foreground))
          .on_click((_e, clickCx) => this.toggleUsagePopup(clickCx ?? cx))
          .child("Uso")
      )
      .child(div().text_color(t.dim).child("·"))
      .child(viewLink("ck-status-home", "Home", "home"))
      .child(div().text_color(t.dim).child("·"))
      .child(viewLink("ck-status-inspector", "Inspector", "inspector"))
      .child(div().text_color(t.dim).child("·"))
      .child(viewLink("ck-status-doctor", "Doctor", "doctor"))
      .child(div().text_color(t.dim).child("·"))
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
    if (role === "home") return this.renderHome(cx);
    if (role === "inspector") return this.renderInspector(cx);
    if (role === "status") return this.renderStatus(cx);
    return this.renderFleet(cx);
  }
}
