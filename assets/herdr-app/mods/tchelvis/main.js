// Tchelvis Herdr Desktop Mod (`gpui-kit` + `herdr` runtime)
// Version: 0.1.0 | Schema Version: 1

import { View, div, svg, image } from "gpui-kit";
import * as base from "gpui-base";
import {
  v_flex,
  h_flex,
  InputState,
  Input,
  TextareaState,
  Textarea,
} from "gpui-base";
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

export const id = "tchelvis";
export const version = "0.1.0";
export const schemaVersion = 1;

export const ICONS = {
  home: "house",
  inspector: "search",
  llm: "brain",
  doctor: "stethoscope",
  chat: "message-square",
  tasks: "clipboard-list",
  folder: "folder",
  projects: "rocket",
  poll: "zap",
  warning: "triangle-alert",
  key: "key-round",
  tui: "monitor",
  settings: "settings",
  timer: "timer",
  antigravity: "sparkles",
  deepseek: "fish",
};

export function iconPath(name) {
  if (!name || !Object.hasOwn(ICONS, name)) return null;
  return `mods/tchelvis/icons/${ICONS[name]}.svg`;
}

function icon(name, size, color) {
  const path = iconPath(name);
  if (!path) return null;
  let el = svg(path).size(size).flex_none();
  if (color) el = el.text_color(color);
  return el;
}

function iconLabel(name, text, { size = 12, color, gap = 4 } = {}) {
  const ic = icon(name, size, color);
  if (!ic) return div().child(text);
  return h_flex().items_center().gap(gap).child(ic).child(text);
}

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

function isRowFinished(row) {
  return row?.lifecycle?.state === "finished" || row?.state === "finished";
}

function isRowBlocked(row) {
  if (isRowFinished(row)) return false;
  return (
    row?.status === "blocked" || row?.attention_state?.state === "blocked"
  );
}

function compareBlockedRows(a, b) {
  const rankA =
    BLOCKED_KIND_ORDER[a?.attention_state?.kind ?? a?.kind ?? "question"] ?? 3;
  const rankB =
    BLOCKED_KIND_ORDER[b?.attention_state?.kind ?? b?.kind ?? "question"] ?? 3;
  if (rankA !== rankB) return rankA - rankB;
  return (
    (a?.attention_state?.since_ms ?? a?.status_ms ?? 0) -
    (b?.attention_state?.since_ms ?? b?.status_ms ?? 0)
  );
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
    if (isRowFinished(agent)) {
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

  blocked.sort(compareBlockedRows);

  active.sort((a, b) => (b?.updated_at ?? b?.seq ?? 0) - (a?.updated_at ?? a?.seq ?? 0));
  finished.sort((a, b) => (b?.updated_at ?? b?.seq ?? 0) - (a?.updated_at ?? a?.seq ?? 0));

  return { blocked, active, finished };
}

function resolveRootAddress(row, byAddr) {
  const startAddr = row?.address;
  if (!startAddr) return null;
  const visited = new Set([startAddr]);
  let cur = row;
  let rootAddr = null;
  while (
    cur?.parent &&
    cur.parent !== cur.address &&
    byAddr.has(cur.parent)
  ) {
    const parentAddr = cur.parent;
    if (visited.has(parentAddr)) {
      return null;
    }
    visited.add(parentAddr);
    rootAddr = parentAddr;
    cur = byAddr.get(parentAddr);
  }
  return rootAddr;
}

export function groupFleetRows(rows = []) {
  const list = Array.isArray(rows) ? rows : [];
  const byAddr = new Map();
  for (const row of list) {
    if (row?.address && !byAddr.has(row.address)) {
      byAddr.set(row.address, row);
    }
  }

  const groups = [];
  const groupByAddr = new Map();
  const rootAddrs = list.map((row) => resolveRootAddress(row, byAddr));

  for (let i = 0; i < list.length; i += 1) {
    const row = list[i];
    if (!rootAddrs[i]) {
      const group = { row, children: [] };
      groups.push(group);
      if (row?.address && !groupByAddr.has(row.address)) {
        groupByAddr.set(row.address, group);
      }
    }
  }

  for (let i = 0; i < list.length; i += 1) {
    const row = list[i];
    const rootAddr = rootAddrs[i];
    if (rootAddr) {
      const parentGroup = groupByAddr.get(rootAddr);
      if (parentGroup) {
        parentGroup.children.push(row);
      } else {
        groups.push({ row, children: [] });
      }
    }
  }

  return groups;
}

export function partitionFleetGroups(groups = []) {
  const blocked = [];
  const active = [];
  const finished = [];

  for (const group of Array.isArray(groups) ? groups : []) {
    const parent = group?.row;
    if (!parent) continue;
    const rawChildren = Array.isArray(group.children) ? group.children : [];
    const blockedChildren = [];
    const otherChildren = [];
    for (const child of rawChildren) {
      if (isRowBlocked(child)) blockedChildren.push(child);
      else otherChildren.push(child);
    }
    const children = [...blockedChildren, ...otherChildren];
    const normalized = { row: parent, children };
    const allRows = [parent, ...children];

    if (allRows.some(isRowBlocked)) {
      blocked.push(normalized);
    } else if (allRows.every(isRowFinished)) {
      finished.push(normalized);
    } else {
      active.push(normalized);
    }
  }

  const blockedLead = (g) =>
    isRowBlocked(g.row) ? g.row : (g.children.find(isRowBlocked) ?? g.row);

  blocked.sort((a, b) => compareBlockedRows(blockedLead(a), blockedLead(b)));
  active.sort(
    (a, b) =>
      (b.row?.updated_at ?? b.row?.seq ?? 0) -
      (a.row?.updated_at ?? a.row?.seq ?? 0)
  );
  finished.sort(
    (a, b) =>
      (b.row?.updated_at ?? b.row?.seq ?? 0) -
      (a.row?.updated_at ?? a.row?.seq ?? 0)
  );

  return { blocked, active, finished };
}

export function childConnector(index, count) {
  return index >= count - 1 ? "└" : "├";
}

export function blockedChildrenBadge(group) {
  const count = (group?.children ?? []).filter(isRowBlocked).length;
  if (count === 0) return null;
  return count === 1 ? "1 filho" : `${count} filhos`;
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
          ? `Tchelvis schema mismatch (found v${snapshot.schema_version}, expected v${schemaVersion}). Run 'tchelvis setup'.`
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

function agentGlyph(kindOrLabel, color) {
  const key = String(kindOrLabel ?? "").trim().toLowerCase();
  let glyph = "›_";
  if (key.includes("claude")) glyph = "✦";
  else if (key.includes("codex")) glyph = "◎";
  else if (key === "pi" || key.includes("π")) glyph = "π";
  else if (key.includes("opencode")) glyph = "</>";
  else if (key.includes("antigravity") || key.includes("agy")) glyph = "✧";
  return div()
    .text_size(11.5)
    .text_color(color)
    .flex_none()
    .child(glyph);
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

export function modelRowActions(row, selectedId) {
  if (!row || row.in_use || (selectedId && row.id === selectedId)) {
    return { canHide: false, canUnhide: false, canDelete: false };
  }
  const isHidden = Boolean(row.hidden);
  return {
    canHide: !isHidden,
    canUnhide: isHidden,
    canDelete: true,
  };
}

export function deleteClick(pending, id, now, windowMs = 4000) {
  if (!id) {
    return { pending: null, confirmed: false };
  }
  if (
    pending &&
    pending.id === id &&
    now >= pending.at &&
    now - pending.at <= windowMs
  ) {
    return { pending: null, confirmed: true };
  }
  return { pending: { id, at: now }, confirmed: false };
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

// Top-bar LLM pill: keep the model name short so the backend/RAM label after it stays visible.
// Shortens in the middle so both the family and the quant ("…-Q4_K_M") survive.
export function pillModelName(name, maxLen = 28) {
  const s = String(name ?? "");
  if (s.length <= maxLen) return s;
  const tail = Math.floor((maxLen - 1) / 2);
  const head = maxLen - 1 - tail;
  return s.slice(0, head) + "…" + s.slice(s.length - tail);
}

function extractHostname(rawHost) {
  const s = String(rawHost ?? "").trim();
  if (!s) return "";
  try {
    return new URL(s).hostname || s;
  } catch {
    return s;
  }
}

export function chatHistoryForRequest(messages, maxMessages = 12) {
  const list = Array.isArray(messages) ? messages : [];
  const filtered = list
    .filter((m) => m && (m.role === "user" || m.role === "assistant"))
    .map((m) => ({ role: m.role, content: String(m.content ?? "") }));
  if (maxMessages <= 0) return [];
  return filtered.slice(-maxMessages);
}

export function chatRequestArgv(messages, question) {
  return [
    "chat",
    "ask",
    "--json",
    "--request",
    JSON.stringify({
      question: String(question ?? ""),
      history: chatHistoryForRequest(messages),
    }),
  ];
}

export function parseChatResult(code, stdout) {
  let parsed = null;
  try {
    parsed = JSON.parse(String(stdout ?? "").trim());
  } catch {
    return {
      ok: false,
      code: "llm_error",
      error: "Resposta inválida do tchelvis",
    };
  }
  if (!parsed || typeof parsed !== "object") {
    return {
      ok: false,
      code: "llm_error",
      error: "Resposta inválida do tchelvis",
    };
  }
  if (code === 0 && typeof parsed.answer === "string") {
    return {
      ok: true,
      answer: parsed.answer,
      model: String(parsed.model ?? ""),
      viaFallback: Boolean(parsed.via_fallback),
      fallbackHost: parsed.fallback_host ? String(parsed.fallback_host) : null,
    };
  }
  if (typeof parsed.error === "string" || typeof parsed.code === "string") {
    return {
      ok: false,
      code: String(parsed.code || "llm_error"),
      error: String(parsed.error || "Erro desconhecido"),
    };
  }
  return {
    ok: false,
    code: "llm_error",
    error: "Resposta inválida do tchelvis",
  };
}

export function chatHeaderLabel(supSnap) {
  const model = pillModelName(supSnap?.llm_model || "");
  if (!supSnap?.fallback_active) return model;
  const host = extractHostname(supSnap?.fallback_host);
  return host
    ? `${model ? `${model} ` : ""}(Fallback · ${host})`
    : `${model ? `${model} ` : ""}(Fallback)`;
}

export function fallbackMarker(result) {
  if (!result?.viaFallback) return "";
  const host = extractHostname(result?.fallbackHost);
  return host ? `via Fallback · ${host}` : "via Fallback";
}

export function chatElapsedLabel(startedAtMs, nowMs = Date.now()) {
  const secs = Math.floor(
    Math.max(0, Number(nowMs) - Number(startedAtMs)) / 1000
  );
  return secs >= 1 ? `◌ Gerando… ${secs}s` : "◌ Gerando…";
}

function formatShortHm(ts) {
  if (ts == null) return "--:--";
  const d = new Date(Number(ts) * 1000);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

export function fallbackPill(supSnap, localModels) {
  if (!supSnap?.fallback_active) return null;
  const model = pillModelName(
    fallbackModelLabel(supSnap.fallback_model || supSnap.llm_model || "", localModels)
  );
  return {
    text: `● LLM: ${model} (Fallback)`,
    tone: "working",
  };
}

function blobHex(name) {
  const m = /^(?:sha256-)?([0-9a-f]{32,})$/i.exec(String(name || ""));
  return m ? m[1].toLowerCase() : null;
}

/**
 * Readable name for a fallback model id. A llama-server started without `--alias` reports the
 * model's file path (`/models/<blob hash>`); the hash is matched against the local model scan.
 * API ids (`org/model`) are returned unchanged.
 */
export function fallbackModelLabel(id, localModels) {
  const raw = String(id || "").trim();
  if (!raw.startsWith("/")) return raw;
  const base = raw.split("/").filter(Boolean).pop() || raw;
  const hex = blobHex(base);
  if (!hex) return base;
  const hit = (Array.isArray(localModels) ? localModels : []).find((m) => {
    const file = String(m?.path || "").split("/").pop();
    return blobHex(file) === hex;
  });
  return hit?.display_name || `modelo sem nome · ${hex.slice(0, 8)}`;
}

export function fallbackSection(supSnap) {
  if (!supSnap?.fallback_key_present) {
    return { state: "no_key", label: "Sem chave" };
  }
  const hasHost = Boolean(String(supSnap.fallback_host || "").trim());
  const hasModel = Boolean(String(supSnap.fallback_model || "").trim());
  if (!hasHost || !hasModel) {
    return { state: "needs_endpoint", label: "Configuração pendente" };
  }
  if (supSnap.fallback_active) {
    const why =
      supSnap.fallback_reason === "load_failed"
        ? "load falhou"
        : "health falhou";
    return {
      state: "active",
      label: `ATIVO — ${why} ${formatShortHm(supSnap.fallback_since)}`,
    };
  }
  return { state: "standby", label: "Em espera" };
}

export function primaryRowBadge(row, supSnap) {
  if (!row?.in_use) return null;
  return supSnap?.fallback_active ? "falhou" : "em uso";
}

function fallbackHostLabel(host) {
  const raw = String(host || "").trim();
  if (!raw) return "";
  try {
    return new URL(raw).host || raw;
  } catch (_e) {
    return raw.replace(/^https?:\/\//i, "").split("/")[0] || raw;
  }
}

export function effectiveTaskTag(chosen, activeSpaceName, choices) {
  const list = Array.isArray(choices) ? choices : [];
  if (chosen && list.includes(chosen)) return chosen;
  if (activeSpaceName && list.includes(activeSpaceName)) return activeSpaceName;
  return "Geral";
}

export function taskProjectArgs(tag) {
  const t = String(tag ?? "").trim();
  if (!t || t === "Geral") return [];
  return ["--project", t];
}

export function isBlankDraft(draft) {
  return String(draft ?? "").trim().length === 0;
}

export function draftDisplayLines(draft, maxLines = 6) {
  const lines = String(draft ?? "").split("\n");
  if (lines.length <= maxLines) return lines;
  if (maxLines <= 1) return ["…"];
  return ["…", ...lines.slice(lines.length - (maxLines - 1))];
}

export function taskEditorArgv(draft, priority, tag) {
  const args = [
    "tchelvis",
    "task",
    "add",
    "--edit",
    "--priority",
    priority || "normal",
    ...taskProjectArgs(tag),
  ];
  if (!isBlankDraft(draft)) {
    args.push("--", String(draft));
  }
  return args;
}

export function hasNewTask(prevIds, tasks) {
  const prev = new Set(Array.isArray(prevIds) ? prevIds : []);
  return (Array.isArray(tasks) ? tasks : []).some((t) => t?.id && !prev.has(t.id));
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
  "antigravity",
  "agy",
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
    "tchelvis",
    "herdr-supervisor",
    "tchelvis-supervisor",
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
    "antigravity",
    "agy",
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

// Attach by the core terminal address (e.g. `t11g`): it is unique and always
// resolvable. Names are not — agent-spawned terminals have no core name, and
// the tab/agent label (`agy`, `claude`) makes `hc attach` fail with not_found,
// closing the tab before anything is shown.
export function attachTabArgv(address) {
  return ["hc", "attach", address];
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

// Fleet card click: focus a pane only when one in the *current* pane list shows the terminal
// (directly or through an `hc attach` tab); otherwise attach. No focus-then-guess: `agent.focus`
// on a background terminal could "succeed" without showing anything, and a stale `pane` id from
// the fleet row made `pane.focus` fail silently — both left the click with no visible effect.
export function planFleetClick(allPanes, allTerminals, address, termName, knownPaneId) {
  const paneList = Array.isArray(allPanes) ? allPanes : [];
  const found = findPaneForTerminal(paneList, allTerminals, address, termName, knownPaneId);
  const isLive = found?.pane && paneList.some((p) => (p.pane ?? p.id) === found.pane);
  return isLive ? { action: "focus-pane", pane: found.pane } : { action: "attach" };
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
      attach_target: a.address,
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
      parent: enriched?.parent || term?.parent || a?.parent || null,
      lifecycle: isFinished ? { state: "finished" } : { state: "live" },
      focused: Boolean(a.focused),
      pinned: Boolean(a.pinned),
      seq: a.seq ?? 0,
      sup_lifecycle: sup?.lifecycle_state ?? null,
      prompt_destructive: Boolean(enriched?.prompt_destructive),
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
      attach_target: sa.address,
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
      parent: sa?.parent || term?.parent || null,
      lifecycle: isFinished
        ? { state: "finished" }
        : (sa?.lifecycle ?? { state: term.state ?? "live" }),
      focused: false,
      pinned: false,
      seq: 0,
      sup_lifecycle: sup?.lifecycle_state ?? null,
      prompt_destructive: Boolean(sa?.prompt_destructive),
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
      attach_target: sup.agent_id,
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
      parent: term?.parent || sup?.parent || null,
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
      attach_target: addr,
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
      parent: term?.parent || null,
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
    this.labelKey = "issue";
    this.commandDraftKey = "";
    this.activeAgentAddress = null;
    this.fallbackHostPrefilled = "";
    this.editingField = null; // "prompt" | "label" | "command" | "task" | "fallbackHost" | "modelFilter" | "chat" | null
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
    this.modelPickerOpen = Boolean(shared("cockpit.modelPickerOpen"));
    this.modelPickerOnlyDrawer = Boolean(shared("cockpit.modelPickerOnlyDrawer"));
    this.modelList = shared("cockpit.modelList") ?? null;
    this.modelListLoading = Boolean(shared("cockpit.modelListLoading"));
    this.modelListError = String(shared("cockpit.modelListError") ?? "");
    this.modelListShowAll = Boolean(shared("cockpit.modelListShowAll"));
    this.modelSwitchingId = shared("cockpit.modelSwitchingId") ?? null;
    this.modelDeletePending = shared("cockpit.modelDeletePending") ?? null;
    this.modelDeletingId = shared("cockpit.modelDeletingId") ?? null;
    this.fallbackReturning = false;
    this.fallbackReturnError = "";
    this.fallbackModelsOpen = false;
    this.fallbackModels = null;
    this.fallbackModelsLoading = false;
    this.fallbackModelsError = "";
    this.fallbackSettingModel = "";
    this.taskPriority = "normal";
    this.taskProject = "";
    this.taskFilter = "pending"; // "pending" | "all" | "done"
    this.benchLast = null;
    this.benchRunning = false;
    this.benchProgress = null;
    this.benchError = "";
    this.benchStartedAtMs = 0;
    this.benchPollInFlight = false;
    const initialViewMode = shared("cockpit.viewMode");
    this.inspectorTab =
      initialViewMode && initialViewMode !== "usage"
        ? initialViewMode
        : "inspector"; // "home" | "inspector" | "doctor" | "chat"
    this.chatMessages = [];
    this.chatPending = false;
    this.chatStartedAtMs = 0;
    this.wasChatVisible = false;
    this.chatScroll = base.ScrollHandle?.new?.() ?? null;
    this.handle = cx.focus_handle();

    const taskInput = TextareaState.new({
      placeholder: "Nova tarefa (Shift+Enter: nova linha)…",
      value: "",
      rows: 1,
    });
    taskInput.set_auto_grow(1, 6);

    const chatInput = TextareaState.new({
      placeholder: "Pergunte sobre agentes ou tarefas (Shift+Enter: nova linha)…",
      value: "",
      rows: 1,
    });
    chatInput.set_auto_grow(1, 6);

    const fallbackHostInput = InputState.new({
      placeholder: "Endpoint API (ex: https://openrouter.ai/api)",
      value: "",
    });
    const modelFilterInput = InputState.new({
      placeholder: "Filtrar modelos…",
      value: "",
    });
    const commandInput = InputState.new({
      placeholder: "Digite o comando para enviar via hc…",
      value: "",
    });
    const promptInput = InputState.new({
      placeholder: "Send message to agent…",
      value: "",
    });
    const labelInput = InputState.new({
      placeholder: "Valor (ex: ENG-42)…",
      value: "",
    });

    this.inputs = {
      task: taskInput,
      chat: chatInput,
      fallbackHost: fallbackHostInput,
      modelFilter: modelFilterInput,
      command: commandInput,
      prompt: promptInput,
      label: labelInput,
    };

    for (const [name, state] of Object.entries(this.inputs)) {
      state.on("focus", (_e, evCx) => {
        this.editingField = name;
        evCx?.notify();
      });
      state.on("blur", (_e, evCx) => {
        if (this.editingField === name) {
          this.editingField = null;
        }
        evCx?.notify();
      });
    }
    for (const name of ["chat", "modelFilter", "prompt", "label"]) {
      this.inputs[name].on("change", (_e, evCx) => evCx?.notify());
    }
    taskInput.on("submit", (_e, evCx) => this.addTaskGui(evCx));
    chatInput.on("submit", (_e, evCx) => this.sendChatMessage(evCx));
    fallbackHostInput.on("submit", (_e, evCx) => this.submitFallbackHost(evCx));
    modelFilterInput.on("submit", (_e, evCx) => {
      if (this.fallbackModelsOpen) {
        this.fetchFallbackModels(evCx);
      } else {
        this.setEditingField(null);
        evCx?.notify();
      }
    });
    commandInput.on("submit", (_e, evCx) => {
      const addr = this.activeAgentAddress ?? this.selectedAddress();
      if (addr) this.approveActionCard(evCx, addr);
    });
    promptInput.on("submit", (_e, evCx) => {
      const addr = this.activeAgentAddress ?? this.selectedAddress();
      if (addr) this.sendPromptGui(evCx, addr);
    });
    labelInput.on("submit", (_e, evCx) => {
      const addr = this.activeAgentAddress ?? this.selectedAddress();
      const val = this.fieldValue("label");
      if (addr && !isBlankDraft(val)) {
        this.applyLabelGui(evCx, addr, this.labelKey, val.trim());
      }
    });

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
      this.fetchBenchLast(cx);
      if (!shared("supervisor.snapshot")) {
        this.fetchSupervisorStatus(cx);
      }
      cx.timer.every(1000, (cx) => {
        if (this.benchRunning) this.pollBenchProgress(cx);
      });
      cx.timer.every(6000, (cx) => this.refreshInspector(cx));
      cx.timer.every(60000, (cx) => {
        const inspectorHidden = Boolean(
          findNode(layout(), "tchelvis-inspector")?.hidden
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

    const recordDebug = (
      action,
      {
        livePane = null,
        paneAfter = null,
        rawFocusedTerm = null,
        mappedFocused = null,
      } = {}
    ) => {
      setShared("cockpit.attachDebug", {
        addr,
        livePane: livePane?.pane ?? null,
        paneAfter: paneAfter?.pane ?? null,
        rawFocusedTerm,
        mappedFocused,
        action,
        at: Date.now(),
      });
    };

    const isFinished =
      term?.state === "finished" ||
      ((agentRow?.status === "done" ||
        agentRow?.status === "error" ||
        agentRow?.lifecycle?.state === "finished" ||
        agentRow?.state === "finished") &&
        (!term || term.state === "finished"));

    if (isFinished) {
      const sp = (spaces() ?? []).find((s) => s.active);
      const displayName = termName || agentRow.name || addr;
      run("tab.new", {
        space: sp?.id,
        name: `${displayName} (log)`,
        argv: ["sh", "-c", 'herdr-core read "$1" | less -R', "sh", addr],
      });
      recordDebug("finished-transcript");
      cx?.notify?.();
      return;
    }

    const openAttachTab = (debugCtx = {}) => {
      this.attachingTargets ??= new Map();
      const now = Date.now();
      const lastAttempt = this.attachingTargets.get(addr) ?? 0;
      if (now - lastAttempt < 1200) {
        recordDebug("debounced", debugCtx);
        cx?.notify?.();
        return;
      }
      this.attachingTargets.set(addr, now);
      const sp = (spaces() ?? []).find((s) => s.active);
      run("tab.new", {
        space: sp?.id,
        name: termName || addr,
        argv: attachTabArgv(addr),
      });
      recordDebug("attach-tab", debugCtx);
      cx?.notify?.();
    };

    if (forceAttach) {
      openAttachTab();
      return;
    }

    const plan = planFleetClick(
      allPanes,
      allTerms,
      addr,
      termName,
      agentRow.pane ?? null
    );
    if (plan.action === "focus-pane") {
      run("pane.focus", { pane: plan.pane });
      recordDebug("focused-pane", { livePane: plan });
      cx?.notify?.();
    } else {
      openAttachTab();
    }
  }

  fetchSupervisorStatus(cx) {
    if (this.pollingSupervisor) return;
    this.pollingSupervisor = true;
    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["supervisor", "status", "--json"], {
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
        const res = await exec("tchelvis", ["supervisor", "tick", "--json"], {
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

  runSupervisorAction(cx, args, successNote, timeoutMs = 15000) {
    if (this.busy) return;
    this.busy = true;
    cx.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", args, { timeout: timeoutMs });
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

  loadSupervisorLlm(cx, onLoaded = null) {
    if (this.loadingLlm || this.busy) return;
    this.loadingLlm = true;
    this.busy = true;
    cx.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["supervisor", "load"], {
          timeout: 300000,
        });
        if (res.code === 0 && res.stdout) {
          const snap = parseSnapshotOutput(res.stdout);
          if (snap) {
            setShared("supervisor.snapshot", snap);
            setShared("supervisor.lastError", "");
          }
          this.note = "LLM carregada";
          if (onLoaded) onLoaded(cx);
        } else {
          this.note = res.stderr?.trim() || `Command failed (${res.code})`;
        }
      } catch (e) {
        this.note = `Error: ${String(e)}`;
      } finally {
        this.loadingLlm = false;
        this.busy = false;
        cx.notify();
      }
    });
  }

  returnToPrimary(cx) {
    if (this.fallbackReturning || this.loadingLlm || this.busy) return;
    this.fallbackReturning = true;
    this.fallbackReturnError = "";
    this.busy = true;
    cx?.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["supervisor", "fallback", "off"], {
          timeout: 120000,
        });
        if (res.code === 0) {
          if (res.stdout) {
            const snap = parseSnapshotOutput(res.stdout);
            if (snap) {
              setShared("supervisor.snapshot", snap);
              setShared("supervisor.lastError", "");
            }
          }
          this.fallbackReturnError = "";
          this.note = "Primário restaurado";
        } else {
          const err =
            res.stderr?.trim() ||
            `Falha ao voltar ao principal (${res.code})`;
          this.fallbackReturnError = err;
          this.note = err;
        }
      } catch (e) {
        const err = `Error: ${String(e)}`;
        this.fallbackReturnError = err;
        this.note = err;
      } finally {
        this.fallbackReturning = false;
        this.busy = false;
        this.fetchSupervisorStatus(cx);
        if (Boolean(shared("cockpit.modelPickerOpen") ?? this.modelPickerOpen)) {
          this.fetchModelList(cx, null);
        }
        cx.notify();
      }
    });
  }

  submitFallbackHost(cx) {
    const raw = this.fieldValue("fallbackHost");
    if (isBlankDraft(raw) || this.busy) return;
    const host = raw.trim();
    this.setEditingField(null);
    this.fallbackReturnError = "";
    this.busy = true;
    cx?.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec(
          "tchelvis",
          ["supervisor", "fallback", "set", "--host", host],
          { timeout: 15000 }
        );
        if (res.code === 0) {
          this.note = `Fallback host: ${host}`;
          this.fallbackModelsOpen = true;
          this.fetchFallbackModels(cx);
        } else {
          this.fallbackReturnError =
            res.stderr?.trim() || `Falha ao definir host (${res.code})`;
        }
      } catch (e) {
        this.fallbackReturnError = `Error: ${String(e)}`;
      } finally {
        this.busy = false;
        this.fetchSupervisorStatus(cx);
        cx.notify();
      }
    });
  }

  toggleFallbackModels(cx) {
    if (this.fallbackModelsOpen) {
      this.fallbackModelsOpen = false;
      if (this.editingField === "modelFilter") {
        this.setEditingField(null);
      }
      cx?.notify();
      return;
    }
    this.fallbackModelsOpen = true;
    this.fetchFallbackModels(cx);
  }

  fetchFallbackModels(cx) {
    if (this.fallbackModelsLoading) return;
    this.fallbackModelsLoading = true;
    this.fallbackModelsError = "";
    cx?.notify();

    const filterText = this.fieldValue("modelFilter").trim();
    const args = ["supervisor", "fallback", "models", "--json"];
    if (filterText) {
      args.push("--filter", filterText);
    }

    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", args, { timeout: 20000 });
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          this.fallbackModels = Array.isArray(parsed) ? parsed : [];
          this.fallbackModelsError = "";
        } else {
          this.fallbackModels = [];
          this.fallbackModelsError =
            res.stderr?.trim() || `Falha ao listar modelos (${res.code})`;
        }
      } catch (e) {
        this.fallbackModels = [];
        this.fallbackModelsError = `Error: ${String(e)}`;
      } finally {
        this.fallbackModelsLoading = false;
        cx.notify();
      }
    });
  }

  selectFallbackModel(cx, modelId) {
    if (!modelId || this.fallbackSettingModel) return;
    this.fallbackSettingModel = modelId;
    this.fallbackModelsError = "";
    this.fallbackReturnError = "";
    cx?.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec(
          "tchelvis",
          ["supervisor", "fallback", "set", "--model", modelId],
          { timeout: 15000 }
        );
        if (res.code === 0) {
          this.fallbackModelsOpen = false;
          if (this.editingField === "modelFilter") {
            this.setEditingField(null);
          }
          this.note = `Fallback model: ${modelId}`;
        } else {
          this.fallbackModelsError =
            res.stderr?.trim() || `Falha ao definir modelo (${res.code})`;
        }
      } catch (e) {
        this.fallbackModelsError = `Error: ${String(e)}`;
      } finally {
        this.fallbackSettingModel = "";
        this.fetchSupervisorStatus(cx);
        cx.notify();
      }
    });
  }

  approveActionCard(cx, agentId) {
    if (!agentId) return;
    const payload = this.fieldValue("command");
    this.setEditingField(null);
    this.runSupervisorAction(
      cx,
      ["supervisor", "approve", agentId, payload],
      `Aprovado e enviado para ${agentId}`
    );
  }

  ignoreActionCard(cx, agentId) {
    if (!agentId) return;
    this.setEditingField(null);
    this.runSupervisorAction(
      cx,
      ["supervisor", "ignore", agentId],
      `Alerta ignorado para ${agentId}`
    );
  }

  respondPendingPrompt(cx, agentId, action) {
    if (!agentId || this.busy) return;
    this.busy = true;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const res = await exec(
          "tchelvis",
          ["supervisor", "respond", agentId, action],
          { timeout: 15000 }
        );
        if (res.code === 0 && res.stdout) {
          const snap = parseSnapshotOutput(res.stdout);
          if (snap) {
            setShared("supervisor.snapshot", snap);
            setShared("supervisor.lastError", "");
          }
          this.note =
            action === "approve"
              ? `Prompt aprovado em ${agentId}`
              : `Prompt negado em ${agentId}`;
          this.refreshCockpit(cx, true);
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

  refreshCockpit(cx, forceRefresh = false) {
    if (this.fetchingSnap) return;
    this.fetchingSnap = true;
    cx.spawn(async (cx) => {
      try {
        const args = ["status", "--json", "--no-spawn"];
        if (forceRefresh) args.push("--refresh");
        const res = await exec("tchelvis", args, { timeout: 8000 });
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
        const res = await exec("tchelvis", ["inspect", addr, "--json"], {
          timeout: 8000,
        });
        if (res.code === 0 && res.stdout) {
          this.inspectCache[addr] = JSON.parse(res.stdout);
        }
      } catch (_e) {
        // Ignore if terminal is plain shell or tchelvis binary is absent
      } finally {
        this.fetchingInspect = false;
        cx.notify();
      }
    });
  }

  runDoctorCheckSilent(cx) {
    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["doctor", "--json"], {
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
    this.note = "Running tchelvis doctor…";
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["doctor", "--json"], {
          timeout: 10000,
        });
        if (res.code === 0 && res.stdout) {
          this.doctorReport = JSON.parse(res.stdout);
          this.note = `Doctor: ${this.doctorReport.overall_status.toUpperCase()}`;
        } else {
          this.note = "tchelvis doctor failed";
        }
      } catch (e) {
        this.note = `Error: ${String(e)}`;
      } finally {
        this.busy = false;
        cx.notify();
      }
    });
  }

  fetchBenchLast(cx) {
    cx.spawn(async (cx) => {
      try {
        const res = await exec(
          "tchelvis",
          ["supervisor", "bench", "--last", "--json"],
          { timeout: 10000 }
        );
        if (res.code === 0 && res.stdout) {
          this.benchLast = JSON.parse(res.stdout);
        } else {
          this.benchLast = null;
        }
        cx.notify();
      } catch (_e) {}
    });
  }

  pollBenchProgress(cx) {
    if (!this.benchRunning || this.benchPollInFlight) return;
    this.benchPollInFlight = true;
    cx.spawn(async (cx) => {
      try {
        const res = await exec(
          "tchelvis",
          ["supervisor", "bench", "--progress", "--json"],
          { timeout: 5000 }
        );
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          if (parsed && typeof parsed === "object" && parsed.phase) {
            this.benchProgress = parsed;
          }
        }
        cx.notify();
      } catch (_e) {
      } finally {
        this.benchPollInFlight = false;
      }
    });
  }

  runBench(cx) {
    if (this.benchRunning) return;
    this.benchRunning = true;
    this.benchProgress = null;
    this.benchError = "";
    this.benchStartedAtMs = Date.now();
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["supervisor", "bench", "--json"], {
          timeout: 900000,
        });
        if (res.code === 0 && res.stdout) {
          this.benchLast = JSON.parse(res.stdout);
          this.benchError = "";
        } else {
          const errText = (res.stderr || res.stdout || "").trim();
          this.benchError = errText || `Benchmark exited with code ${res.code}`;
        }
      } catch (e) {
        this.benchError = String(e);
      } finally {
        this.benchRunning = false;
        this.benchProgress = null;
        cx.notify();
      }
    });
  }

  runSetupGui(cx) {
    if (this.busy) return;
    this.busy = true;
    this.note = "Running tchelvis setup…";
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["setup"], { timeout: 10000 });
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
        this.clearField("label");
        this.setEditingField(null);
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
    const raw = this.fieldValue("prompt");
    if (!address || isBlankDraft(raw)) return;
    const text = raw.trim();
    this.clearField("prompt");
    this.setEditingField(null);
    this.note = `Sent to ${address}`;
    cx.notify();
    hc(["send", "--force", address, text]);
  }

  fetchHomeSnapshot(cx) {
    if (this.fetchingHome) return;
    this.fetchingHome = true;
    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", ["home", "--json"], {
          timeout: 8000,
        });
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          this.homeSnap = parsed;
          setShared("cockpit.home", parsed);
          if (
            this.taskEditorPrevIds &&
            hasNewTask(this.taskEditorPrevIds, parsed.tasks ?? [])
          ) {
            this.clearField("task");
            this.taskEditorPrevIds = null;
          }
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
        const res = await exec("tchelvis", args, { timeout: 22000 });
        if (res.code === 0 && res.stdout) {
          snap = JSON.parse(res.stdout);
        }
      } catch (_e) {
        // Fallback directly to ai-usagebar CLI if tchelvis binary lacks `usage`
      }

      // Direct fallback if tchelvis failed or didn't find ai-usagebar in its PATH
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
        const res = await exec("tchelvis", ["task", ...args], { timeout: 8000 });
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          this.homeSnap = parsed;
          setShared("cockpit.home", parsed);
          if (
            this.taskEditorPrevIds &&
            hasNewTask(this.taskEditorPrevIds, parsed.tasks ?? [])
          ) {
            this.clearField("task");
            this.taskEditorPrevIds = null;
          }
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
    const draft = this.fieldValue("task");
    if (isBlankDraft(draft) || this.busy) return;
    const title = draft.trim();
    const args = [
      "add",
      "--priority",
      this.taskPriority || "normal",
      ...taskProjectArgs(this.currentTaskTag),
      "--json",
      "--",
      title,
    ];
    this.clearField("task");
    this.setEditingField(null);
    this.taskTagMenuOpen = false;
    this.runTaskCommand(cx, args, () => {
      this.note = `Tarefa adicionada: ${ellipsize(title.split("\n")[0], 36)}`;
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

  fieldValue(name) {
    return this.inputs?.[name]?.value() ?? "";
  }

  clearField(name) {
    this.inputs?.[name]?.set_value("");
  }

  focusField(name) {
    this.editingField = name;
    this.inputs?.[name]?.focus();
  }

  scrollChatToBottom() {
    if (!this.chatScroll) return;
    const len = Array.isArray(this.chatMessages) ? this.chatMessages.length : 0;
    const lastIndex = this.chatPending ? len : Math.max(0, len - 1);
    this.chatScroll.scroll_to_item(lastIndex);
  }

  setEditingField(name) {
    if (!name) {
      const hadEditing = this.editingField !== null;
      this.editingField = null;
      if (hadEditing) {
        this.handle?.focus?.();
      }
      return;
    }
    this.focusField(name);
  }

  handleKeyDown(e, cx) {
    if (!this.editingField) return;
    if (this.editingField === "chat" && this.chatPending) return;
    if (e.key === "escape") {
      this.setEditingField(null);
      cx.stop_propagation();
      cx.notify();
    }
  }

  sectionTitle(t, title, subtitleOrCount = null, iconName = null) {
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
          .child(
            iconName
              ? icon(iconName, 12, t.accent)
              : div().size(6).rounded(3).bg(t.accent)
          )
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
      findNode(layout(), "tchelvis-inspector")?.hidden
    );
    const usageOpen = Boolean(shared("cockpit.usageOpen") ?? this.usageOpen);
    if (usageOpen && !inspectorHidden) {
      this.closeUsagePopup(cx);
      return;
    }
    this.modelPickerOpen = false;
    this.modelPickerOnlyDrawer = false;
    setShared("cockpit.modelPickerOpen", false);
    setShared("cockpit.modelPickerOnlyDrawer", false);
    this.usageOpen = true;
    this.usageMenuOpen = false;
    setShared("cockpit.usageOpen", true);
    if (inspectorHidden) {
      this.usageOnlyDrawer = true;
      setShared("cockpit.usageOnlyDrawer", true);
      run("layout.toggle", { id: "tchelvis-inspector" });
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
    if (onlyDrawer && !findNode(layout(), "tchelvis-inspector")?.hidden) {
      run("layout.hide", { id: "tchelvis-inspector" });
    }
    cx?.notify();
  }

  toggleModelPicker(cx) {
    const inspectorHidden = Boolean(
      findNode(layout(), "tchelvis-inspector")?.hidden
    );
    const pickerOpen = Boolean(
      shared("cockpit.modelPickerOpen") ?? this.modelPickerOpen
    );
    if (pickerOpen && !inspectorHidden) {
      this.closeModelPicker(cx);
      return;
    }
    this.usageOpen = false;
    this.usageMenuOpen = false;
    this.usageOnlyDrawer = false;
    setShared("cockpit.usageOpen", false);
    setShared("cockpit.usageOnlyDrawer", false);

    this.modelPickerOpen = true;
    this.modelListShowAll = false;
    this.modelDeletePending = null;
    setShared("cockpit.modelPickerOpen", true);
    setShared("cockpit.modelListShowAll", false);
    setShared("cockpit.modelDeletePending", null);
    if (inspectorHidden) {
      this.modelPickerOnlyDrawer = true;
      setShared("cockpit.modelPickerOnlyDrawer", true);
      run("layout.toggle", { id: "tchelvis-inspector" });
    } else {
      this.modelPickerOnlyDrawer = false;
      setShared("cockpit.modelPickerOnlyDrawer", false);
    }
    this.fetchModelList(cx, false);
    this.fetchSupervisorStatus(cx);
    cx?.notify();
  }

  closeModelPicker(cx) {
    const onlyDrawer = Boolean(
      shared("cockpit.modelPickerOnlyDrawer") ?? this.modelPickerOnlyDrawer
    );
    this.modelPickerOpen = false;
    this.modelPickerOnlyDrawer = false;
    this.modelListShowAll = false;
    this.modelDeletePending = null;
    this.fallbackModelsOpen = false;
    this.clearField("modelFilter");
    if (
      this.editingField === "fallbackHost" ||
      this.editingField === "modelFilter"
    ) {
      this.setEditingField(null);
    }
    setShared("cockpit.modelPickerOpen", false);
    setShared("cockpit.modelPickerOnlyDrawer", false);
    setShared("cockpit.modelListShowAll", false);
    setShared("cockpit.modelDeletePending", null);
    if (onlyDrawer && !findNode(layout(), "tchelvis-inspector")?.hidden) {
      run("layout.hide", { id: "tchelvis-inspector" });
    }
    cx?.notify();
  }

  _runModelListCmd(cx, args) {
    this.modelListLoading = true;
    this.modelListError = "";
    setShared("cockpit.modelListLoading", true);
    setShared("cockpit.modelListError", "");
    cx?.notify();

    cx.spawn(async (cx) => {
      try {
        const res = await exec("tchelvis", args, { timeout: 30000 });
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          this.modelList = parsed;
          setShared("cockpit.modelList", parsed);
          this.modelListError = "";
          setShared("cockpit.modelListError", "");
        } else {
          const err = res.stderr?.trim() || `Command failed (${res.code})`;
          this.modelListError = err;
          setShared("cockpit.modelListError", err);
        }
      } catch (e) {
        const err = `Error: ${String(e)}`;
        this.modelListError = err;
        setShared("cockpit.modelListError", err);
      } finally {
        this.modelListLoading = false;
        setShared("cockpit.modelListLoading", false);
        cx.notify();
      }
    });
  }

  fetchModelList(cx, showAll = null) {
    const useAll =
      showAll !== null
        ? Boolean(showAll)
        : Boolean(shared("cockpit.modelListShowAll") ?? this.modelListShowAll);
    this.modelDeletePending = null;
    setShared("cockpit.modelDeletePending", null);
    const args = ["supervisor", "model", "list", "--json"];
    if (useAll) args.push("--all");
    this._runModelListCmd(cx, args);
  }

  setModelFilter(cx, f) {
    const switching = shared("cockpit.modelSwitchingId") ?? this.modelSwitchingId;
    const deleting = shared("cockpit.modelDeletingId") ?? this.modelDeletingId;
    if (switching || deleting) return;
    this.modelListShowAll = false;
    this.modelDeletePending = null;
    setShared("cockpit.modelListShowAll", false);
    setShared("cockpit.modelDeletePending", null);
    this._runModelListCmd(cx, ["supervisor", "model", "filter", f]);
  }

  selectModel(cx, id) {
    const switching = shared("cockpit.modelSwitchingId") ?? this.modelSwitchingId;
    const deleting = shared("cockpit.modelDeletingId") ?? this.modelDeletingId;
    if (switching || deleting || !id) return;
    this.modelDeletePending = null;
    this.modelSwitchingId = id;
    this.modelListError = "";
    setShared("cockpit.modelDeletePending", null);
    setShared("cockpit.modelSwitchingId", id);
    setShared("cockpit.modelListError", "");
    cx?.notify();

    cx.spawn(async (cx) => {
      let ok = false;
      try {
        const res = await exec("tchelvis", ["supervisor", "model", "set", id], {
          timeout: 300000,
        });
        if (res.code === 0 && res.stdout) {
          const snap = parseSnapshotOutput(res.stdout);
          if (snap) {
            setShared("supervisor.snapshot", snap);
            setShared("supervisor.lastError", "");
          }
          ok = true;
        } else {
          const err = res.stderr?.trim() || `Command failed (${res.code})`;
          this.modelListError = err;
          setShared("cockpit.modelListError", err);
        }
      } catch (e) {
        const err = `Error: ${String(e)}`;
        this.modelListError = err;
        setShared("cockpit.modelListError", err);
      } finally {
        this.modelSwitchingId = null;
        setShared("cockpit.modelSwitchingId", null);
        if (ok) {
          this.fetchModelList(cx, null);
        } else {
          cx.notify();
        }
      }
    });
  }

  hideModel(cx, id) {
    const switching = shared("cockpit.modelSwitchingId") ?? this.modelSwitchingId;
    const deleting = shared("cockpit.modelDeletingId") ?? this.modelDeletingId;
    if (switching || deleting || !id) return;
    const useAll = Boolean(
      shared("cockpit.modelListShowAll") ?? this.modelListShowAll
    );
    const args = ["supervisor", "model", "hide", id];
    if (useAll) args.push("--all");
    this._runModelListCmd(cx, args);
  }

  unhideModel(cx, id) {
    const switching = shared("cockpit.modelSwitchingId") ?? this.modelSwitchingId;
    const deleting = shared("cockpit.modelDeletingId") ?? this.modelDeletingId;
    if (switching || deleting || !id) return;
    const useAll = Boolean(
      shared("cockpit.modelListShowAll") ?? this.modelListShowAll
    );
    const args = ["supervisor", "model", "unhide", id];
    if (useAll) args.push("--all");
    this._runModelListCmd(cx, args);
  }

  deleteModel(cx, id) {
    const switching = shared("cockpit.modelSwitchingId") ?? this.modelSwitchingId;
    const deleting = shared("cockpit.modelDeletingId") ?? this.modelDeletingId;
    if (switching || deleting || !id) return;
    const curPending =
      shared("cockpit.modelDeletePending") ?? this.modelDeletePending ?? null;
    const next = deleteClick(curPending, id, Date.now());
    this.modelDeletePending = next.pending;
    setShared("cockpit.modelDeletePending", next.pending);
    if (!next.confirmed) {
      cx?.notify();
      return;
    }

    this.modelDeletingId = id;
    this.modelListError = "";
    setShared("cockpit.modelDeletingId", id);
    setShared("cockpit.modelListError", "");
    cx?.notify();

    cx.spawn(async (cx) => {
      let ok = false;
      try {
        const res = await exec(
          "tchelvis",
          ["supervisor", "model", "rm", id, "--yes", "--json"],
          { timeout: 120000 }
        );
        if (res.code === 0 && res.stdout) {
          const parsed = JSON.parse(res.stdout);
          if (parsed?.deleted) {
            ok = true;
          } else {
            const err = "Model was not deleted";
            this.modelListError = err;
            setShared("cockpit.modelListError", err);
          }
        } else {
          const err =
            res.stderr?.trim() ||
            res.stdout?.trim() ||
            `Command failed (${res.code})`;
          this.modelListError = err;
          setShared("cockpit.modelListError", err);
        }
      } catch (e) {
        const err = `Error: ${String(e)}`;
        this.modelListError = err;
        setShared("cockpit.modelListError", err);
      } finally {
        this.modelDeletingId = null;
        setShared("cockpit.modelDeletingId", null);
        if (ok) {
          this.fetchModelList(cx, null);
        } else {
          cx.notify();
        }
      }
    });
  }

  openDrawerView(cx, tab, toggleIfActive = false) {
    const normalizedTab = tab === "usage" ? "inspector" : tab;
    const inspectorHidden = Boolean(
      findNode(layout(), "tchelvis-inspector")?.hidden
    );
    const onlyUsage = Boolean(
      shared("cockpit.usageOnlyDrawer") ?? this.usageOnlyDrawer
    );
    const onlyModel = Boolean(
      shared("cockpit.modelPickerOnlyDrawer") ?? this.modelPickerOnlyDrawer
    );
    const rawView =
      shared("cockpit.viewMode") || this.inspectorTab || "inspector";
    const currentView = rawView === "usage" ? "inspector" : rawView;
    if (
      toggleIfActive &&
      !inspectorHidden &&
      !onlyUsage &&
      !onlyModel &&
      currentView === normalizedTab
    ) {
      this.usageOpen = false;
      this.usageOnlyDrawer = false;
      this.usageMenuOpen = false;
      this.modelPickerOpen = false;
      this.modelPickerOnlyDrawer = false;
      setShared("cockpit.usageOpen", false);
      setShared("cockpit.usageOnlyDrawer", false);
      setShared("cockpit.modelPickerOpen", false);
      setShared("cockpit.modelPickerOnlyDrawer", false);
      run("layout.toggle", { id: "tchelvis-inspector" });
      cx?.notify();
      return;
    }
    if (onlyUsage) {
      this.usageOnlyDrawer = false;
      setShared("cockpit.usageOnlyDrawer", false);
    }
    if (onlyModel) {
      this.modelPickerOnlyDrawer = false;
      setShared("cockpit.modelPickerOnlyDrawer", false);
    }
    setShared("cockpit.viewMode", normalizedTab);
    this.inspectorTab = normalizedTab;
    if (normalizedTab === "home") this.fetchHomeSnapshot(cx);
    else if (normalizedTab === "doctor") {
      if (!this.doctorReport) this.runDoctorCheck(cx);
      if (!this.benchLast && !this.benchRunning) this.fetchBenchLast(cx);
    } else if (normalizedTab === "inspector") {
      this.refreshInspector(cx);
    } else if (normalizedTab === "chat") {
      if (!shared("supervisor.snapshot")) this.fetchSupervisorStatus(cx);
      if ((props()?.role ?? "fleet") === "inspector") {
        this.scrollChatToBottom();
        this.focusField("chat");
      }
    }
    if (inspectorHidden) {
      run("layout.toggle", { id: "tchelvis-inspector" });
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
    const llmModel = pillModelName(supSnap?.llm_model || "lfm3:1.7b");
    const llmBackend = supSnap?.llm_backend || "ollama";
    const llmConnected = supSnap ? Boolean(supSnap.llm_connected) : false;
    const modelInstalled = supSnap ? Boolean(supSnap.model_installed) : false;
    const modelLoaded = Boolean(supSnap?.model_loaded);

    const llmManaged = Boolean(supSnap?.llm_managed);
    const llmContainerState = supSnap?.llm_container_state || "missing";
    const llmContainerRunning = llmContainerState === "running";

    const fbPill = fallbackPill(
      supSnap,
      (shared("cockpit.modelList") ?? this.modelList)?.models
    );
    let llmPillText = `◌ LLM: ${llmModel} (verificando…)`;
    let llmPillColor = t.muted;
    let llmPillIcon = null;
    if (fbPill) {
      llmPillText = fbPill.text;
      llmPillColor = t[fbPill.tone] ?? t.working;
    } else if (llmManaged) {
      if (!llmContainerRunning) {
        llmPillText = `○ LLM: ${llmModel} (DESATIVADO)`;
        llmPillColor = t.muted;
      } else if (!llmConnected) {
        llmPillText = `◌ LLM: ${llmModel} (INICIANDO…)`;
        llmPillColor = t.working;
      } else {
        llmPillText = `● LLM: ${llmModel} (llama.cpp · ${llmMemoryLabel(supSnap) || "ATIVO"})`;
        llmPillColor = t.done;
      }
    } else if (llmConnected && modelInstalled) {
      llmPillText = `● LLM: ${llmModel} (${llmBackend} · ${modelLoaded ? llmMemoryLabel(supSnap) || "EM RAM" : "PRONTO"})`;
      llmPillColor = t.done;
    } else if (llmConnected && !modelInstalled) {
      llmPillText = `LLM: ${llmModel} (NÃO INSTALADO)`;
      llmPillColor = t.working;
      llmPillIcon = "warning";
    } else if (supSnap) {
      llmPillText = `○ LLM: ${llmModel} (OFFLINE)`;
      llmPillColor = t.blocked;
    }
    const inspectorHidden = Boolean(
      findNode(layout(), "tchelvis-inspector")?.hidden
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
          .child(
            this.pillBadge(
              t,
              llmPillIcon
                ? iconLabel(llmPillIcon, llmPillText, {
                    size: 11,
                    color: llmPillColor,
                    gap: 4,
                  })
                : llmPillText,
              llmPillColor
            )
              .id("ck-llm-pill")
              .cursor_pointer()
              .hover((s) => s.bg(t.hover))
              .on_mouse_down("left", (_e, clickCx) => clickCx.stop_propagation())
              .on_click((_e, clickCx) => this.toggleModelPicker(clickCx ?? cx))
          )
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
            this.actionBtn(
              t,
              "ck-pill-poll",
              iconLabel("poll", "Poll Now", { size: 11.5, color: t.muted }),
              (_e, cx) => {
              if (
                llmManaged &&
                !llmContainerRunning &&
                !supSnap?.fallback_active
              ) {
                this.loadSupervisorLlm(cx, (loadedCx) => {
                  this.refreshCockpit(loadedCx, true);
                  this.tickSupervisor(loadedCx, true);
                });
                return;
              }
              this.refreshCockpit(cx, true);
              this.tickSupervisor(cx, true);
            })
          )
          .child(
            (() => {
              const isFallbackActive = Boolean(supSnap?.fallback_active);
              const showLoadBtn =
                isFallbackActive || (llmManaged && !llmContainerRunning);
              const isReturning = Boolean(this.fallbackReturning);
              const unloadLabel = isReturning
                ? "◌ Voltando…"
                : this.loadingLlm
                  ? "◌ Carregando…"
                  : showLoadBtn
                    ? "▶ Load LLM"
                    : "⏻ Unload LLM";
              return this.actionBtn(
                t,
                "ck-pill-unload",
                unloadLabel,
                (_e, cx) => {
                  if (this.loadingLlm || this.fallbackReturning) return;
                  if (isFallbackActive) {
                    this.returnToPrimary(cx);
                  } else if (showLoadBtn) {
                    this.loadSupervisorLlm(cx);
                  } else {
                    this.runSupervisorAction(
                      cx,
                      ["supervisor", "unload"],
                      llmManaged
                        ? "Container LLM descarregado"
                        : "Modelo Ollama descarregado"
                    );
                  }
                }
              );
            })()
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
                (() => {
                  const active = rightView === "home" && !inspectorHidden && !onlyUsage;
                  return this.segmentBtn(
                    t,
                    "ck-pill-view-home",
                    iconLabel("home", "Home", {
                      size: 11,
                      color: active ? t.accent : t.muted,
                    }),
                    active,
                    (_e, cx) => this.openDrawerView(cx, "home", true)
                  );
                })()
              )
              .child(
                (() => {
                  const active =
                    rightView === "inspector" && !inspectorHidden && !onlyUsage;
                  return this.segmentBtn(
                    t,
                    "ck-pill-view-inspector",
                    iconLabel("inspector", "Inspector", {
                      size: 11,
                      color: active ? t.accent : t.muted,
                    }),
                    active,
                    (_e, cx) => this.openDrawerView(cx, "inspector", true)
                  );
                })()
              )
              .child(
                (() => {
                  const active =
                    rightView === "doctor" && !inspectorHidden && !onlyUsage;
                  return this.segmentBtn(
                    t,
                    "ck-pill-view-doctor",
                    iconLabel("doctor", "Doctor", {
                      size: 11,
                      color: active ? t.accent : t.muted,
                    }),
                    active,
                    (_e, cx) => this.openDrawerView(cx, "doctor", true)
                  );
                })()
              )
              .child(
                (() => {
                  const active =
                    rightView === "chat" && !inspectorHidden && !onlyUsage;
                  return this.segmentBtn(
                    t,
                    "ck-pill-view-chat",
                    iconLabel("chat", "Chat", {
                      size: 11,
                      color: active ? t.accent : t.muted,
                    }),
                    active,
                    (_e, cx) => this.openDrawerView(cx, "chat", true)
                  );
                })()
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
    const parts = partitionFleetGroups(groupFleetRows(merged));
    const countRows = (groups) =>
      groups.reduce((sum, g) => sum + 1 + (g.children?.length ?? 0), 0);
    const blockedCount = countRows(parts.blocked);
    const activeCount = countRows(parts.active);
    const finishedCount = countRows(parts.finished);
    const liveCount = blockedCount + activeCount;

    const flattenGroupAddrs = (groups) =>
      groups.flatMap((g) => [
        g.row.address,
        ...(g.children ?? []).map((c) => c.address),
      ]);
    const orderedAddrs = [
      ...flattenGroupAddrs(parts.blocked),
      ...flattenGroupAddrs(parts.active),
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

    const renderAgentCard = (a, { blockedBadge = null, isChild = false } = {}) => {
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

      const cardBase = v_flex()
        .id(`ck-row-${a.address}`)
        .p(10)
        .gap(5)
        .rounded(8)
        .border(1)
        .border_color(cardBorder)
        .bg(isSelected ? t.selected : t.surface)
        .cursor_pointer();
      const sizedCard = isChild
        ? cardBase.flex_1().min_w(0)
        : cardBase.mx(10);

      return sizedCard
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
          if (findNode(layout(), "tchelvis-inspector")?.hidden) {
            run("layout.toggle", { id: "tchelvis-inspector" });
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
              programIcon(a.program, 13, t.muted) ??
                agentGlyph(a.program_label, t.muted)
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
        .when(Boolean(blockedBadge), (el) =>
          el.child(
            div()
              .text_size(11)
              .font_semibold()
              .text_color(t.blocked)
              .child(
                iconLabel("warning", blockedBadge, {
                  size: 11,
                  color: t.blocked,
                  gap: 4,
                })
              )
          )
        )
        .when(!blockedBadge && a.blocked_children?.length > 0, (el) =>
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

    const renderGroup = (group) => {
      const badge = blockedChildrenBadge(group);
      const parentCard = renderAgentCard(group.row, { blockedBadge: badge });
      const children = group.children ?? [];
      if (children.length === 0) return parentCard;
      return v_flex()
        .gap(5)
        .child(parentCard)
        .children(
          children.map((child, idx) =>
            h_flex()
              .pl(16)
              .pr(10)
              .gap(4)
              .items_center()
              .child(
                div()
                  .w(12)
                  .flex_none()
                  .text_size(12)
                  .text_color(t.muted)
                  .child(childConnector(idx, children.length))
              )
              .child(renderAgentCard(child, { isChild: true }))
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
        // Header Bar (only TCHELVIS FLEET + count pill + [+ Agent] + [↻])
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
              .child("TCHELVIS FLEET")
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
          .child(filterSeg("blocked", `Needs (${blockedCount})`))
          .child(filterSeg("active", `Work (${activeCount})`))
          .child(filterSeg("finished", `Done (${finishedCount})`))
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
                        iconLabel("home", "Home Overview", {
                          size: 11.5,
                          color: t.foreground,
                        }),
                        (_e, cx) => this.openDrawerView(cx, "home", false),
                        true
                      )
                    )
                )
            )
          )
          .children(showBlocked.map(renderGroup))
          .children(showActive.map(renderGroup))
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
                  .child(`Recent Finished (${finishedCount})`)
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
                      .child(`Finished (${finishedCount})`)
                  )
                  .child(
                    this.actionBtn(
                      t,
                      "ck-clear-finished",
                      "Clear Done",
                      (_e, cx) => {
                        const addrs = flattenGroupAddrs(parts.finished);
                        if (addrs.length > 0) {
                          hc(["rm", ...addrs]);
                          this.refreshCockpit(cx, true);
                        }
                      }
                    )
                  )
              )
          )
          .children(showFinished.map(renderGroup))
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
    const currentProjectTag = effectiveTaskTag(
      this.taskProject,
      activeSpace?.name,
      projectChoices
    );
    this.currentTaskTag = currentProjectTag;

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
                      : supSnap.llm_managed && supSnap.llm_container_state !== "running"
                        ? `${supSnap.llm_model || "lfm3:1.7b"} · DESATIVADO`
                        : `${supSnap.llm_model || "lfm3:1.7b"} · OFF`
                    : `${supSnap?.llm_model || "lfm3:1.7b"} · …`,
                  `${supSnap?.llm_backend || "ollama"} · ${gpuModeText(supSnap)}`,
                  supSnap?.llm_connected
                    ? t.done
                    : supSnap?.llm_managed && supSnap?.llm_container_state !== "running"
                      ? t.muted
                      : supSnap
                        ? t.blocked
                        : t.muted
                )
              )
          )
      )
      // 2. Card 1: TAREFAS DO DIA
      .child(
        this.sectionTitle(t, "TAREFAS DO DIA", "tchelvis task", "tasks")
      )
      .child(
        this.cardBox(t)
          .rounded(10)
          .child(
            h_flex()
              .gap(6)
              .items_start()
              .child(
                Textarea.new(this.inputs.task)
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
                  .text_color(t.foreground)
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
              .child(
                h_flex()
                  .id("ck-task-edit")
                  .h(26)
                  .px(8)
                  .items_center()
                  .justify_center()
                  .rounded(6)
                  .border(1)
                  .border_color(t.border)
                  .bg(t.raised)
                  .text_size(12)
                  .text_color(t.muted)
                  .cursor_pointer()
                  .hover((s) => s.bg(t.hover).text_color(t.foreground))
                  .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
                  .on_click((_e, cx) => {
                    this.taskEditorPrevIds = tasks.map((tk) => tk.id);
                    this.setEditingField(null);
                    this.taskTagMenuOpen = false;
                    run("tab.new", {
                      space: activeSpace?.id,
                      name: "nova tarefa",
                      argv: taskEditorArgv(
                        this.fieldValue("task"),
                        this.taskPriority || "normal",
                        this.currentTaskTag
                      ),
                    });
                    cx.notify();
                  })
                  .child("✎")
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
                  .id("ck-task-project")
                  .h(24)
                  .px(8)
                  .gap(4)
                  .items_center()
                  .rounded(999)
                  .border(1)
                  .border_color(this.taskTagMenuOpen ? t.accent : t.border)
                  .bg(this.taskTagMenuOpen ? t.selected : t.background)
                  .text_size(11)
                  .text_color(t.accent)
                  .cursor_pointer()
                  .hover((s) => s.bg(t.hover).text_color(t.foreground))
                  .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
                  .on_click((_e, cx) => {
                    this.taskTagMenuOpen = !this.taskTagMenuOpen;
                    cx.notify();
                  })
                  .child(icon("folder", 11, t.accent))
                  .child(currentProjectTag)
              )
          )
          .when(Boolean(this.taskTagMenuOpen), (el) =>
            el.child(
              h_flex()
                .gap(5)
                .flex_wrap()
                .items_center()
                .children(
                  projectChoices.map((tag, idx) =>
                    this.actionBtn(
                      t,
                      `ck-task-tag-${idx}`,
                      tag,
                      (_e, cx) => {
                        this.taskProject = tag;
                        this.taskTagMenuOpen = false;
                        cx.notify();
                      },
                      tag === currentProjectTag
                    )
                  )
                )
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
                .items_start()
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
                  v_flex()
                    .flex_1()
                    .min_w(0)
                    .gap(2)
                    .text_size(12)
                    .text_color(tk.done ? t.dim : t.foreground)
                    .children(
                      String(tk.title ?? "")
                        .split("\n")
                        .map((line) => div().child(line || " "))
                    )
                )
                .child(
                  h_flex()
                    .gap(4)
                    .flex_wrap()
                    .items_center()
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
                          .child(this.pillBadge(t, tk.project, t.accent))
                      )
                    )
                    .child(
                      this.pillBadge(
                        t,
                        String(tk.priority || "normal").toUpperCase(),
                        prioCol
                      )
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
          "PROJETOS EM DESENVOLVIMENTO",
          `${projects.length} projetos`,
          "projects"
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
                      this.focusField("task");
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
          "ATENÇÃO IMEDIATA & FROTA",
          blockedAgents.length > 0
            ? `${blockedAgents.length} aguardando`
            : "nominal",
          "poll"
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
                      if (findNode(layout(), "tchelvis-inspector")?.hidden) {
                        run("layout.toggle", { id: "tchelvis-inspector" });
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
      findNode(layout(), "tchelvis-inspector")?.hidden
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
            h_flex()
              .flex_1()
              .min_w(0)
              .gap(6)
              .items_center()
              .text_size(13)
              .font_semibold()
              .text_color(t.foreground)
              .child(icon("home", 13, t.accent))
              .child(ellipsize("TCHELVIS HOME · RESUMO DO DIA", 34))
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
              () => run("layout.toggle", { id: "tchelvis-inspector" }),
              !inspectorHidden
            )
          )
          .child(
            this.actionBtn(t, "ck-home-close", "×", () =>
              run("layout.hide", { id: "tchelvis-home" })
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
        detail: "tchelvis mod & layout loaded",
      },
      {
        name: "storage_ipc",
        status: "unknown",
        detail: "SQLite WAL cache & local store",
      },
      {
        name: "supervisor",
        status: supSnap ? "pass" : "unknown",
        detail: `${supSnap?.llm_model || "lfm3:1.7b"} · ${gpuModeText(supSnap)}${
          supSnap?.gpu_detail || supSnap?.gpu_backend
            ? ` · ${supSnap.gpu_detail || supSnap.gpu_backend}`
            : ""
        }`,
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
                  .child(icon("doctor", 13, t.accent))
                  .child(
                    div()
                      .text_size(13)
                      .font_semibold()
                      .text_color(t.foreground)
                      .child("SYSTEM & PROVIDER DIAGNOSTICS")
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
                `tchelvis v${report?.cockpit_version ?? version} · supervisor ${supSnap?.llm_model || "lfm3:1.7b"} · ${gpuModeText(supSnap)}${
                  supSnap?.gpu_detail || supSnap?.gpu_backend
                    ? ` · ${supSnap.gpu_detail || supSnap.gpu_backend}`
                    : ""
                }`
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
      )
      .child(this.renderBenchCard(cx, t, supSnap));
  }

  renderBenchCard(cx, t, supSnap) {
    const res = this.benchLast;
    const p = this.benchProgress;
    const fmtTps = (v) =>
      v != null && Number.isFinite(Number(v)) ? Number(v).toFixed(1) : "—";
    const fmtSecs = (v) =>
      v != null && Number.isFinite(Number(v)) ? Number(v).toFixed(2) : "—";

    const gpuBk =
      supSnap?.gpu_backend && supSnap.gpu_backend !== "none"
        ? supSnap.gpu_backend
        : "";
    const modeLabel =
      p?.mode === "gpu" ? (gpuBk ? `GPU (${gpuBk})` : "GPU") : "CPU";
    let progressText = "Starting benchmark…";
    if (p?.phase === "loading") progressText = `Loading ${modeLabel}…`;
    else if (p?.phase === "warmup") progressText = `Warm-up ${modeLabel}…`;
    else if (p?.phase === "measuring") {
      progressText = `Measuring ${modeLabel} ${p.run}/${p.runs}…`;
    } else if (p?.phase === "restoring") {
      progressText = "Restoring container…";
    } else if (p?.phase === "done") {
      progressText = "Finishing…";
    }

    const elapsedSecs = this.benchRunning
      ? p?.started_at
        ? Math.max(0, Math.floor(Date.now() / 1000 - p.started_at))
        : Math.max(0, Math.floor((Date.now() - this.benchStartedAtMs) / 1000))
      : 0;

    const runBtn = this.benchRunning
      ? h_flex()
          .id("ck-doc-bench-run")
          .h(25)
          .px(8)
          .gap(5)
          .items_center()
          .rounded(6)
          .border(1)
          .border_color(t.border)
          .bg(t.raised)
          .text_size(11.5)
          .text_color(t.dim)
          .child("Running benchmark…")
      : this.actionBtn(
          t,
          "ck-doc-bench-run",
          "Run benchmark",
          (_e, cx) => this.runBench(cx),
          true
        );

    const renderModeRow = (id, label, modeRes) =>
      h_flex()
        .id(id)
        .gap(6)
        .py(4)
        .items_center()
        .border_b(1)
        .border_color(t.border)
        .child(
          div()
            .w(110)
            .flex_none()
            .text_size(11)
            .font_semibold()
            .text_color(t.foreground)
            .child(label)
        )
        .when(Boolean(modeRes?.error), (el) =>
          el.child(
            div()
              .flex_1()
              .text_size(11)
              .text_color(t.blocked)
              .child(ellipsize(modeRes.error, 120))
          )
        )
        .when(!modeRes?.error, (el) =>
          el
            .child(
              div()
                .flex_1()
                .text_size(11)
                .text_color(t.foreground)
                .child(fmtTps(modeRes?.prompt_tps))
            )
            .child(
              div()
                .flex_1()
                .text_size(11)
                .text_color(t.foreground)
                .child(fmtTps(modeRes?.gen_tps))
            )
            .child(
              div()
                .w(64)
                .flex_none()
                .text_size(11)
                .text_color(t.foreground)
                .child(fmtSecs(modeRes?.eval_secs))
            )
        );

    return v_flex()
      .id("ck-doc-bench-card")
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
            div()
              .text_size(12.5)
              .font_semibold()
              .text_color(t.foreground)
              .child("CPU vs GPU benchmark")
          )
          .child(runBtn)
      )
      .child(
        div()
          .text_size(10.5)
          .text_color(t.dim)
          .child("Takes 1–5 min; the first GPU run may download the GPU image.")
      )
      .when(this.benchRunning, (el) =>
        el.child(
          h_flex()
            .gap(6)
            .items_center()
            .justify_between()
            .p(7)
            .rounded(6)
            .border(1)
            .border_color(t.border)
            .bg(t.background)
            .child(
              div()
                .text_size(11)
                .text_color(t.accent)
                .child(progressText)
            )
            .child(
              div()
                .text_size(11)
                .text_color(t.muted)
                .child(`${elapsedSecs}s`)
            )
        )
      )
      .when(Boolean(this.benchError), (el) =>
        el.child(
          div()
            .p(7)
            .rounded(6)
            .border(1)
            .border_color(withAlpha(t.blocked, "66", t.border))
            .bg(withAlpha(t.blocked, "18", t.background))
            .text_size(11)
            .text_color(t.blocked)
            .child(ellipsize(this.benchError, 240))
        )
      )
      .when(!res && !this.benchRunning && !this.benchError, (el) =>
        el.child(
          div()
            .text_size(11.5)
            .text_color(t.muted)
            .child("No benchmark yet")
        )
      )
      .when(Boolean(res), (el) =>
        el.child(
          v_flex()
            .gap(6)
            .child(
              div()
                .text_size(11)
                .text_color(t.muted)
                .child(
                  `${res.model || "unknown"}${res.quant ? ` (${res.quant})` : ""} · ${formatTimestamp(res.at)}`
                )
            )
            .child(
              v_flex()
                .gap(2)
                .p(8)
                .rounded(8)
                .border(1)
                .border_color(t.border)
                .bg(t.background)
                .child(
                  h_flex()
                    .gap(6)
                    .pb(4)
                    .border_b(1)
                    .border_color(t.border)
                    .child(
                      div()
                        .w(110)
                        .flex_none()
                        .text_size(10.5)
                        .font_semibold()
                        .text_color(t.muted)
                        .child("Mode")
                    )
                    .child(
                      div()
                        .flex_1()
                        .text_size(10.5)
                        .font_semibold()
                        .text_color(t.muted)
                        .child("Prompt tok/s")
                    )
                    .child(
                      div()
                        .flex_1()
                        .text_size(10.5)
                        .font_semibold()
                        .text_color(t.muted)
                        .child("Gen tok/s")
                    )
                    .child(
                      div()
                        .w(64)
                        .flex_none()
                        .text_size(10.5)
                        .font_semibold()
                        .text_color(t.muted)
                        .child("Eval (s)")
                    )
                )
                .child(renderModeRow("ck-doc-bench-cpu", "CPU", res.cpu))
                .child(
                  renderModeRow(
                    "ck-doc-bench-gpu",
                    `GPU (${res.gpu?.backend || "none"})`,
                    res.gpu
                  )
                )
            )
            .when(res.speedup_gen != null, (el2) =>
              el2.child(
                div()
                  .text_size(11.5)
                  .font_semibold()
                  .text_color(t.done)
                  .child(`GPU ≈ ${res.speedup_gen}× faster at generation`)
              )
            )
        )
      );
  }

  sendChatMessage(cx) {
    const raw = this.fieldValue("chat");
    if (this.chatPending || isBlankDraft(raw)) return;
    const question = raw.trim();
    const history = chatHistoryForRequest(this.chatMessages);
    this.clearField("chat");
    this.chatMessages = [
      ...this.chatMessages,
      { role: "user", content: question },
    ];
    this.chatPending = true;
    this.chatStartedAtMs = Date.now();
    this.scrollChatToBottom();
    cx?.notify();

    const elapsedTimer = cx?.timer?.every?.(1000, (tickCx) => {
      if (this.chatPending) (tickCx ?? cx)?.notify();
    });

    cx.spawn(async (cx) => {
      try {
        const res = await exec(
          "tchelvis",
          chatRequestArgv(history, question),
          { timeout: 130000 }
        );
        const parsed = parseChatResult(res.code, res.stdout);
        if (parsed.ok) {
          this.chatMessages = [
            ...this.chatMessages,
            {
              role: "assistant",
              content: parsed.answer,
              model: parsed.model,
              viaFallback: parsed.viaFallback,
              fallbackHost: parsed.fallbackHost,
            },
          ];
        } else if (parsed.code === "llm_stopped") {
          this.chatMessages = [
            ...this.chatMessages,
            {
              role: "error",
              code: "llm_stopped",
              content: parsed.error || "LLM desligada",
            },
          ];
        } else {
          this.chatMessages = [
            ...this.chatMessages,
            {
              role: "error",
              code: parsed.code,
              content: parsed.error || "Erro desconhecido",
            },
          ];
        }
      } catch (_e) {
        this.chatMessages = [
          ...this.chatMessages,
          {
            role: "error",
            code: "llm_error",
            content: "Sem resposta do tchelvis",
          },
        ];
      } finally {
        elapsedTimer?.cancel?.();
        this.chatPending = false;
        this.chatStartedAtMs = 0;
        this.scrollChatToBottom();
        cx.notify();
      }
    });
  }

  renderChatBody(cx, themeArg = null) {
    const t = themeArg ?? theme();
    const supSnap = shared("supervisor.snapshot");
    const headerLabel = chatHeaderLabel(supSnap) || "Chat";
    const isFallback = Boolean(supSnap?.fallback_active);
    const messages = Array.isArray(this.chatMessages) ? this.chatMessages : [];

    return v_flex()
      .flex_1()
      .min_h(0)
      .p(12)
      .gap(10)
      // Header bar: model / fallback indicator + Limpar
      .child(
        h_flex()
          .gap(8)
          .px(10)
          .py(7)
          .rounded(8)
          .border(1)
          .border_color(
            isFallback ? withAlpha(t.working, "77", t.border) : t.border
          )
          .bg(t.surface)
          .items_center()
          .justify_between()
          .child(
            h_flex()
              .gap(6)
              .items_center()
              .flex_1()
              .min_w(0)
              .child(
                div()
                  .text_size(11.5)
                  .font_semibold()
                  .text_color(isFallback ? t.working : t.muted)
                  .child(headerLabel)
              )
          )
          .child(
            this.actionBtn(t, "ck-chat-clear", "Limpar", (_e, cx) => {
              this.chatMessages = [];
              cx.notify();
            })
          )
      )
      // Messages list (scrollable)
      .child(
        v_flex()
          .flex_1()
          .min_h(0)
          .overflow_y_scrollbar()
          .when(Boolean(this.chatScroll), (el) =>
            el.track_scroll(this.chatScroll)
          )
          .gap(8)
          .when(messages.length === 0 && !this.chatPending, (el) =>
            el.child(
              this.cardBox(t)
                .rounded(10)
                .child(
                  div()
                    .text_size(12)
                    .text_color(t.muted)
                    .child(
                      "Pergunte sobre os agentes ativos ou as Tarefas do dia. Somente leitura."
                    )
                )
            )
          )
          .children(
            messages.map((m, idx) => {
              if (m.role === "user") {
                return v_flex()
                  .id(`ck-chat-msg-${idx}`)
                  .p(9)
                  .gap(4)
                  .rounded(8)
                  .border(1)
                  .border_color(withAlpha(t.accent, "55", t.border))
                  .bg(t.selected)
                  .child(
                    div()
                      .text_size(10)
                      .font_semibold()
                      .text_color(t.accent)
                      .child("VOCÊ")
                  )
                  .child(
                    div()
                      .text_size(12)
                      .text_color(t.foreground)
                      .child(String(m.content ?? ""))
                  );
              }
              if (m.role === "error") {
                return v_flex()
                  .id(`ck-chat-msg-${idx}`)
                  .p(9)
                  .gap(6)
                  .rounded(8)
                  .border(1)
                  .border_color(withAlpha(t.blocked, "66", t.blocked))
                  .bg(withAlpha(t.blocked, "18", t.surface))
                  .child(
                    div()
                      .text_size(12)
                      .text_color(t.blocked)
                      .child(String(m.content ?? "Erro"))
                  )
                  .when(m.code === "llm_stopped", (el) =>
                    el.child(
                      h_flex().child(
                        this.actionBtn(
                          t,
                          `ck-chat-load-llm-${idx}`,
                          this.loadingLlm ? "◌ Carregando…" : "▶ Load LLM",
                          (_e, cx) => this.loadSupervisorLlm(cx),
                          true
                        )
                      )
                    )
                  );
              }
              const fbMark = fallbackMarker(m);
              const content = String(m.content ?? "");
              return v_flex()
                .id(`ck-chat-msg-${idx}`)
                .p(9)
                .gap(4)
                .rounded(8)
                .border(1)
                .border_color(t.border)
                .bg(t.surface)
                .child(
                  h_flex()
                    .gap(6)
                    .items_center()
                    .justify_between()
                    .child(
                      div()
                        .text_size(10)
                        .font_semibold()
                        .text_color(t.muted)
                        .child(
                          m.model ? `SUPERVISOR · ${m.model}` : "SUPERVISOR"
                        )
                    )
                    .when(Boolean(fbMark), (el) =>
                      el.child(
                        div()
                          .text_size(10.5)
                          .font_semibold()
                          .text_color(t.working)
                          .child(fbMark)
                      )
                    )
                )
                .child(
                  typeof base.TextView?.markdown === "function"
                    ? div()
                        .text_size(12)
                        .text_color(t.foreground)
                        .child(base.TextView.markdown(`ck-chat-md-${idx}`, content))
                    : div()
                        .text_size(12)
                        .text_color(t.foreground)
                        .child(content)
                );
            })
          )
          .when(Boolean(this.chatPending), (el) => {
            const label = chatElapsedLabel(this.chatStartedAtMs, Date.now());
            const textOnly = label.replace(/^◌\s*/, "");
            const spinnerEl = div().text_size(12).text_color(t.accent);
            const hasFrames = typeof spinnerEl.frames === "function";
            const indicator = hasFrames
              ? spinnerEl.frames(
                  ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
                  90
                )
              : spinnerEl.child("◌");
            return el.child(
              h_flex()
                .id("ck-chat-pending")
                .p(9)
                .gap(6)
                .items_center()
                .rounded(8)
                .border(1)
                .border_color(t.border)
                .bg(t.surface)
                .child(indicator)
                .child(
                  div()
                    .text_size(12)
                    .text_color(t.muted)
                    .child(textOnly)
                )
            );
          })
      )
      // Input bar styled like Daily Tasks input
      .child(
        (() => {
          const chatDisabled =
            this.chatPending || isBlankDraft(this.fieldValue("chat"));
          return h_flex()
            .flex_none()
            .gap(6)
            .items_start()
            .child(
              Textarea.new(this.inputs.chat)
                .flex_1()
                .min_w(0)
                .px(9)
                .py(6)
                .rounded(6)
                .border(1)
                .border_color(
                  this.editingField === "chat" ? t.accent : t.border
                )
                .bg(t.background)
                .text_size(12)
                .text_color(t.foreground)
            )
            .child(
              h_flex()
                .id("ck-chat-send")
                .h(26)
                .px(10)
                .items_center()
                .rounded(6)
                .bg(chatDisabled ? t.raised : t.accent)
                .border(1)
                .border_color(chatDisabled ? t.border : t.accent)
                .text_size(11.5)
                .font_semibold()
                .text_color(chatDisabled ? t.dim : t.background)
                .cursor_pointer()
                .on_mouse_down("left", (_e, cx) => cx.stop_propagation())
                .on_click((_e, cx) => this.sendChatMessage(cx))
                .child(this.chatPending ? "…" : "Enviar")
            );
        })()
      );
  }

  renderModelPicker(cx, themeArg = null, standalone = true) {
    const t = themeArg ?? theme();
    const supSnap = shared("supervisor.snapshot") ?? null;
    const listData = shared("cockpit.modelList") ?? this.modelList ?? null;
    const isLoading = Boolean(
      shared("cockpit.modelListLoading") ?? this.modelListLoading
    );
    const listError = String(
      shared("cockpit.modelListError") ?? this.modelListError ?? ""
    ).trim();
    const showAll = Boolean(
      shared("cockpit.modelListShowAll") ?? this.modelListShowAll
    );
    const switchingId =
      shared("cockpit.modelSwitchingId") ?? this.modelSwitchingId ?? null;
    const deletingId =
      shared("cockpit.modelDeletingId") ?? this.modelDeletingId ?? null;
    const pendingDelete =
      shared("cockpit.modelDeletePending") ?? this.modelDeletePending ?? null;

    const models = Array.isArray(listData?.models) ? listData.models : [];
    const hiddenCount = Number(listData?.hidden_count ?? 0);
    const userHiddenCount = Number(listData?.user_hidden_count ?? 0);
    const selectedId = listData?.selected_id ?? null;
    const activeFilter = String(listData?.filter || "4b").toLowerCase();
    const managed = listData ? listData.managed !== false : true;
    const envWarning = listData?.env_override_ignored
      ? String(listData.env_override_ignored)
      : "";

    const fb = fallbackSection(supSnap);
    const fbHostRaw = String(supSnap?.fallback_host || "").trim();
    if (
      fbHostRaw &&
      this.editingField !== "fallbackHost" &&
      !this.fieldValue("fallbackHost") &&
      this.fallbackHostPrefilled !== fbHostRaw
    ) {
      this.inputs.fallbackHost.set_value(fbHostRaw);
      this.fallbackHostPrefilled = fbHostRaw;
    }
    const fbHostLabel = fallbackHostLabel(fbHostRaw);
    const fbModel = String(supSnap?.fallback_model || "").trim();
    const isReturning = Boolean(this.fallbackReturning);
    const fbFilterQuery = this.fieldValue("modelFilter").trim().toLowerCase();
    const fbModelsAll = Array.isArray(this.fallbackModels)
      ? this.fallbackModels
      : [];
    const localModelsForLabels = Array.isArray(
      (shared("cockpit.modelList") ?? this.modelList)?.models
    )
      ? (shared("cockpit.modelList") ?? this.modelList).models
      : [];
    const fbLabel = (id) => fallbackModelLabel(id, localModelsForLabels);
    const fbModelsFiltered = fbFilterQuery
      ? fbModelsAll.filter(
          (id) =>
            id.toLowerCase().includes(fbFilterQuery) ||
            fbLabel(id).toLowerCase().includes(fbFilterQuery)
        )
      : fbModelsAll;

    const filterOpts = [
      ["2b", "≤2B"],
      ["4b", "≤4B"],
      ["8b", "≤8B"],
      ["all", "All"],
    ];

    let root = v_flex().id("ck-model-picker").p(10).gap(8).bg(t.chrome);
    if (standalone) {
      root = root.rounded(10).border(1).border_color(t.border);
    }

    return root
      .child(
        h_flex()
          .gap(6)
          .items_center()
          .justify_between()
          .child(
            h_flex()
              .gap(6)
              .items_center()
              .child(icon("llm", 13, t.accent))
              .child(
                div()
                  .text_size(12.5)
                  .font_semibold()
                  .text_color(t.foreground)
                  .child("Supervisor model")
              )
          )
          .child(
            h_flex()
              .flex_none()
              .gap(6)
              .items_center()
              .child(
                this.segmentedTrack(t).children(
                  filterOpts.map(([val, label]) =>
                    this.segmentBtn(
                      t,
                      `ck-model-filter-${val}`,
                      label,
                      activeFilter === val,
                      (_e, clickCx) => this.setModelFilter(clickCx ?? cx, val)
                    )
                  )
                )
              )
              .child(
                h_flex()
                  .id("ck-model-close")
                  .flex_none()
                  .h(24)
                  .w(24)
                  .items_center()
                  .justify_center()
                  .rounded(6)
                  .border(1)
                  .border_color(t.border)
                  .bg(t.surface)
                  .text_size(12)
                  .text_color(t.muted)
                  .cursor_pointer()
                  .hover((s) => s.bg(t.hover).text_color(t.foreground))
                  .on_mouse_down("left", (_e, clickCx) =>
                    clickCx.stop_propagation()
                  )
                  .on_click((_e, clickCx) =>
                    this.closeModelPicker(clickCx ?? cx)
                  )
                  .child("×")
              )
          )
      )
      .when(managed, (el) =>
        el.child(
          h_flex()
            .pt(2)
            .gap(6)
            .items_center()
            .child(
              div()
                .text_size(10)
                .font_semibold()
                .text_color(t.muted)
                .child("PRINCIPAL (local)")
            )
            .child(div().flex_1().h(1).bg(t.border))
        )
      )
      .when(Boolean(envWarning), (el) =>
        el.child(
          h_flex()
            .px(8)
            .py(5)
            .rounded(6)
            .border(1)
            .border_color(withAlpha(t.working, "66", t.working))
            .bg(withAlpha(t.working, "1A", t.surface))
            .text_size(11)
            .text_color(t.working)
            .child(
              `${envWarning} is set in the environment and ignored in container mode`
            )
        )
      )
      .when(Boolean(listError), (el) =>
        el.child(
          h_flex()
            .px(8)
            .py(5)
            .rounded(6)
            .border(1)
            .border_color(withAlpha(t.blocked, "66", t.blocked))
            .bg(withAlpha(t.blocked, "1A", t.surface))
            .text_size(11)
            .text_color(t.blocked)
            .child(listError)
        )
      )
      .when(isLoading && !listData, (el) =>
        el.child(
          div()
            .py(10)
            .text_size(11.5)
            .text_color(t.muted)
            .child("Scanning…")
        )
      )
      .when(!isLoading && listData && !managed, (el) =>
        el.child(
          div()
            .py(6)
            .text_size(11.5)
            .text_color(t.muted)
            .child(
              "Model selection applies to the managed LLM container (TCHELVIS_LLM_CONTAINER=on)."
            )
        )
      )
      .when(listData && managed && models.length === 0 && !isLoading, (el) =>
        el.child(
          div()
            .py(6)
            .text_size(11.5)
            .text_color(t.muted)
            .child(
              "No local GGUF models found. Add folders with [llm_container] model_dirs or pull a model with Ollama."
            )
        )
      )
      .when(models.length > 0, (el) =>
        el.child(
          v_flex()
            .gap(4)
            .max_h(260)
            .overflow_y_scrollbar()
            .children(
              models.map((m, idx) => {
                const isSwitching = switchingId === m.id;
                const isDeleting = deletingId === m.id;
                const isNonChat = Boolean(m.kind && m.kind !== "chat");
                const isHiddenRow = Boolean(m.hidden);
                const isClickable = !isNonChat && !switchingId && !deletingId;
                const actions = modelRowActions(m, selectedId);
                const rowBadge = primaryRowBadge(m, supSnap);
                const isFailedRow = rowBadge === "falhou";
                const isPendingDelete = Boolean(
                  pendingDelete &&
                    pendingDelete.id === m.id &&
                    Date.now() - pendingDelete.at <= 4000
                );
                const deleteLabel = isPendingDelete
                  ? Number(m.size_bytes) > 0
                    ? `Delete ${fmtBytes(m.size_bytes)}?`
                    : "Delete?"
                  : "Delete";
                const metaParts = [
                  m.params_label || "?",
                  m.source || "",
                  Number(m.size_bytes) > 0 ? fmtBytes(m.size_bytes) : "",
                  m.quant || "",
                  isNonChat ? String(m.kind) : "",
                  isHiddenRow ? "hidden" : "",
                ].filter(Boolean);

                let row = h_flex()
                  .id(`ck-model-row-${idx}`)
                  .px(8)
                  .py(6)
                  .gap(8)
                  .items_center()
                  .justify_between()
                  .rounded(6)
                  .border(1)
                  .border_color(
                    isFailedRow
                      ? withAlpha(t.blocked, "66", t.blocked)
                      : m.in_use
                        ? withAlpha(t.accent, "66", t.accent)
                        : t.border
                  )
                  .bg(m.in_use ? t.selected : t.surface);

                if (isClickable) {
                  row = row
                    .cursor_pointer()
                    .hover((s) =>
                      s.bg(m.in_use ? t.selected : t.hover)
                    )
                    .on_mouse_down("left", (_e, clickCx) =>
                      clickCx.stop_propagation()
                    )
                    .on_click((_e, clickCx) =>
                      this.selectModel(clickCx ?? cx, m.id)
                    );
                }

                // Name and details stack in the left column; only the badge and the buttons sit on
                // the right. Side by side, Hide + Delete squeezed the name over the details.
                const metaColor = isDeleting
                  ? t.blocked
                  : isSwitching
                    ? t.accent
                    : isNonChat || isHiddenRow
                      ? t.dim
                      : t.muted;
                const metaText = isDeleting
                  ? "◌ Deleting…"
                  : isSwitching
                    ? "◌ Switching…"
                    : ellipsize(metaParts.join(" · "), 44);
                return row
                  .child(
                    v_flex()
                      .flex_1()
                      .min_w(0)
                      .overflow_hidden()
                      .gap(2)
                      .child(
                        h_flex()
                          .gap(6)
                          .items_center()
                          .child(
                            div()
                              .flex_none()
                              .text_size(11.5)
                              .text_color(
                                isNonChat || isHiddenRow
                                  ? t.dim
                                  : isFailedRow
                                    ? t.blocked
                                    : m.in_use
                                      ? t.accent
                                      : t.muted
                              )
                              .child(m.in_use ? "●" : "○")
                          )
                          .child(
                            div()
                              .text_size(11.5)
                              .font_semibold()
                              .text_color(
                                isNonChat || isHiddenRow ? t.dim : t.foreground
                              )
                              .child(ellipsize(m.display_name || m.id, 30))
                          )
                      )
                      .child(
                        div()
                          .pl(17)
                          .text_size(10.5)
                          .text_color(metaColor)
                          .child(metaText)
                      )
                  )
                  .child(
                    h_flex()
                      .flex_none()
                      .gap(6)
                      .items_center()
                      .when(Boolean(rowBadge) && !isSwitching && !isDeleting, (el) =>
                        el.child(
                          div()
                            .px(5)
                            .py(1)
                            .rounded(4)
                            .border(1)
                            .border_color(
                              withAlpha(
                                isFailedRow ? t.blocked : t.accent,
                                "66",
                                t.border
                              )
                            )
                            .text_size(10)
                            .font_semibold()
                            .text_color(isFailedRow ? t.blocked : t.accent)
                            .child(rowBadge)
                        )
                      )
                      .when(
                        !isSwitching && !isDeleting && actions.canHide,
                        (el) =>
                          el.child(
                            this.actionBtn(
                              t,
                              `ck-model-hide-${idx}`,
                              "Hide",
                              (_e, clickCx) =>
                                this.hideModel(clickCx ?? cx, m.id)
                            )
                          )
                      )
                      .when(
                        !isSwitching && !isDeleting && actions.canUnhide,
                        (el) =>
                          el.child(
                            this.actionBtn(
                              t,
                              `ck-model-show-${idx}`,
                              "Show",
                              (_e, clickCx) =>
                                this.unhideModel(clickCx ?? cx, m.id)
                            )
                          )
                      )
                      .when(
                        !isSwitching && !isDeleting && actions.canDelete,
                        (el) =>
                          el.child(
                            this.actionBtn(
                              t,
                              `ck-model-del-${idx}`,
                              deleteLabel,
                              (_e, clickCx) =>
                                this.deleteModel(clickCx ?? cx, m.id),
                              false,
                              isPendingDelete
                            )
                          )
                      )
                  );
              })
            )
        )
      )
      .child(
        h_flex()
          .pt(2)
          .items_center()
          .justify_between()
          .gap(8)
          .child(
            (hiddenCount > 0 || userHiddenCount > 0) && !showAll
              ? div()
                  .id("ck-model-show-hidden")
                  .text_size(11)
                  .text_color(t.muted)
                  .cursor_pointer()
                  .hover((s) => s.text_color(t.foreground))
                  .on_mouse_down("left", (_e, clickCx) =>
                    clickCx.stop_propagation()
                  )
                  .on_click((_e, clickCx) => {
                    this.modelListShowAll = true;
                    setShared("cockpit.modelListShowAll", true);
                    this.fetchModelList(clickCx ?? cx, true);
                  })
                  .child(
                    [
                      hiddenCount > 0 ? `${hiddenCount} hidden by filter` : "",
                      userHiddenCount > 0
                        ? `${userHiddenCount} hidden by you`
                        : "",
                    ]
                      .filter(Boolean)
                      .join(" · ")
                  )
              : div()
          )
          .child(
            this.actionBtn(
              t,
              "ck-model-rescan",
              isLoading ? "Scanning…" : "↻ Rescan",
              (_e, clickCx) => {
                if (!isLoading) this.fetchModelList(clickCx ?? cx, null);
              }
            )
          )
      )
      .when(managed, (el) =>
        el.child(
          v_flex()
            .pt(4)
            .gap(6)
            .child(
              h_flex()
                .gap(6)
                .items_center()
                .child(
                  div()
                    .text_size(10)
                    .font_semibold()
                    .text_color(t.muted)
                    .child("FALLBACK (API)")
                )
                .child(div().flex_1().h(1).bg(t.border))
            )
            .when(fb.state === "no_key", (fbEl) =>
              fbEl.child(
                div()
                  .text_size(11)
                  .text_color(t.muted)
                  .child(
                    'Sem chave: rode "tchelvis supervisor fallback key" e cole a chave (stdin).'
                  )
              )
            )
            .when(fb.state === "needs_endpoint", (fbEl) =>
              fbEl.child(
                v_flex()
                  .gap(5)
                  .child(
                    h_flex()
                      .gap(6)
                      .items_center()
                      .on_mouse_down("left", (_e, clickCx) =>
                        clickCx.stop_propagation()
                      )
                      .child(
                        Input.new(this.inputs.fallbackHost)
                          .flex_1()
                          .min_w(0)
                          .h(28)
                          .px(8)
                          .rounded(6)
                          .border(1)
                          .border_color(
                            this.editingField === "fallbackHost"
                              ? t.accent
                              : t.border
                          )
                          .bg(t.background)
                          .text_size(11)
                          .text_color(t.foreground)
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-fallback-host-save",
                          "Salvar",
                          (_e, clickCx) => {
                            if (!this.fieldValue("fallbackHost") && fbHostRaw) {
                              this.inputs.fallbackHost.set_value(fbHostRaw);
                            }
                            this.submitFallbackHost(clickCx ?? cx);
                          }
                        )
                      )
                      .when(Boolean(fbHostRaw), (el2) =>
                        el2.child(
                          this.actionBtn(
                            t,
                            "ck-fallback-trocar-empty",
                            this.fallbackModelsOpen ? "Fechar" : "Trocar",
                            (_e, clickCx) =>
                              this.toggleFallbackModels(clickCx ?? cx)
                          )
                        )
                      )
                  )
                  .child(
                    div()
                      .text_size(10.5)
                      .text_color(t.dim)
                      .child(
                        "Ex: https://opencode.ai/inference/go/openai/v1 · https://openrouter.ai/api"
                      )
                  )
              )
            )
            .when(
              fb.state === "standby" || fb.state === "active",
              (fbEl) =>
                fbEl.child(
                  v_flex()
                    .gap(5)
                    .child(
                      h_flex()
                        .gap(6)
                        .items_center()
                        .justify_between()
                        .child(
                          div()
                            .flex_1()
                            .min_w(0)
                            .text_size(11.5)
                            .font_semibold()
                            .text_color(t.foreground)
                            .child(
                              ellipsize(
                                `${fbHostLabel || fbHostRaw} · ${fbLabel(fbModel)}`,
                                42
                              )
                            )
                        )
                        .child(
                          this.actionBtn(
                            t,
                            "ck-fallback-trocar",
                            this.fallbackModelsOpen ? "Fechar" : "Trocar",
                            (_e, clickCx) =>
                              this.toggleFallbackModels(clickCx ?? cx)
                          )
                        )
                    )
                    .child(
                      h_flex()
                        .gap(6)
                        .items_center()
                        .justify_between()
                        .child(
                          div()
                            .flex_1()
                            .min_w(0)
                            .text_size(11)
                            .font_semibold()
                            .text_color(
                              fb.state === "active" ? t.working : t.muted
                            )
                            .child(fb.label)
                        )
                        .when(fb.state === "active", (el2) =>
                          el2.child(
                            this.actionBtn(
                              t,
                              "ck-fallback-return",
                              isReturning
                                ? "◌ Voltando…"
                                : "Voltar ao principal",
                              (_e, clickCx) => {
                                if (!isReturning) {
                                  this.returnToPrimary(clickCx ?? cx);
                                }
                              }
                            )
                          )
                        )
                    )
                )
            )
            .when(Boolean(this.fallbackReturnError), (fbEl) =>
              fbEl.child(
                div()
                  .px(8)
                  .py(5)
                  .rounded(6)
                  .border(1)
                  .border_color(withAlpha(t.blocked, "66", t.blocked))
                  .bg(withAlpha(t.blocked, "1A", t.surface))
                  .text_size(10.5)
                  .text_color(t.blocked)
                  .child(ellipsize(this.fallbackReturnError, 120))
              )
            )
            .when(
              !this.fallbackReturnError &&
                fb.state === "active" &&
                Boolean(supSnap?.fallback_detail),
              (fbEl) =>
                fbEl.child(
                  div()
                    .text_size(10.5)
                    .text_color(t.muted)
                    .child(ellipsize(String(supSnap.fallback_detail), 80))
                )
            )
            .when(this.fallbackModelsOpen, (fbEl) =>
              fbEl.child(
                v_flex()
                  .p(6)
                  .gap(5)
                  .rounded(6)
                  .border(1)
                  .border_color(t.border)
                  .bg(t.surface)
                  .child(
                    h_flex()
                      .gap(6)
                      .items_center()
                      .on_mouse_down("left", (_e, clickCx) =>
                        clickCx.stop_propagation()
                      )
                      .child(
                        Input.new(this.inputs.modelFilter)
                          .flex_1()
                          .min_w(0)
                          .h(26)
                          .px(7)
                          .rounded(5)
                          .border(1)
                          .border_color(
                            this.editingField === "modelFilter"
                              ? t.accent
                              : t.border
                          )
                          .bg(t.background)
                          .text_size(11)
                          .text_color(t.foreground)
                      )
                      .child(
                        this.actionBtn(
                          t,
                          "ck-fallback-models-refresh",
                          this.fallbackModelsLoading ? "…" : "↻",
                          (_e, clickCx) => {
                            if (!this.fallbackModelsLoading) {
                              this.fetchFallbackModels(clickCx ?? cx);
                            }
                          }
                        )
                      )
                  )
                  .when(this.fallbackModelsLoading, (el2) =>
                    el2.child(
                      div()
                        .py(4)
                        .text_size(11)
                        .text_color(t.muted)
                        .child("Buscando modelos…")
                    )
                  )
                  .when(Boolean(this.fallbackModelsError), (el2) =>
                    el2.child(
                      div()
                        .py(4)
                        .text_size(11)
                        .text_color(t.blocked)
                        .child(ellipsize(this.fallbackModelsError, 120))
                    )
                  )
                  .when(
                    !this.fallbackModelsLoading &&
                      !this.fallbackModelsError &&
                      fbModelsFiltered.length === 0 &&
                      Array.isArray(this.fallbackModels),
                    (el2) =>
                      el2.child(
                        div()
                          .py(4)
                          .text_size(11)
                          .text_color(t.muted)
                          .child("Nenhum modelo encontrado.")
                      )
                  )
                  .when(
                    !this.fallbackModelsLoading && fbModelsFiltered.length > 0,
                    (el2) =>
                      el2.child(
                        v_flex()
                          .gap(3)
                          .max_h(160)
                          .overflow_y_scrollbar()
                          .children(
                            fbModelsFiltered.map((modelId, idx) => {
                              const isCurrent = modelId === fbModel;
                              const isPicking =
                                this.fallbackSettingModel === modelId;
                              return h_flex()
                                .id(`ck-fb-model-row-${idx}`)
                                .px(7)
                                .py(4)
                                .gap(6)
                                .items_center()
                                .justify_between()
                                .rounded(5)
                                .border(1)
                                .border_color(
                                  isCurrent
                                    ? withAlpha(t.accent, "66", t.accent)
                                    : t.border
                                )
                                .bg(isCurrent ? t.selected : t.background)
                                .cursor_pointer()
                                .hover((s) =>
                                  s.bg(isCurrent ? t.selected : t.hover)
                                )
                                .on_mouse_down("left", (_e, clickCx) =>
                                  clickCx.stop_propagation()
                                )
                                .on_click((_e, clickCx) =>
                                  this.selectFallbackModel(
                                    clickCx ?? cx,
                                    modelId
                                  )
                                )
                                .child(
                                  div()
                                    .flex_1()
                                    .min_w(0)
                                    .text_size(11)
                                    .font_semibold()
                                    .text_color(t.foreground)
                                    .child(ellipsize(fbLabel(modelId), 44))
                                )
                                .child(
                                  div()
                                    .flex_none()
                                    .text_size(10)
                                    .text_color(
                                      isPicking ? t.accent : t.muted
                                    )
                                    .child(
                                      isPicking
                                        ? "◌ Salvando…"
                                        : isCurrent
                                          ? "ativo"
                                          : "Usar"
                                    )
                                );
                            })
                          )
                      )
                  )
              )
            )
        )
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
      if (key.includes("antigravity") || key.includes("agy"))
        return { icon: "antigravity" };
      if (key.includes("opencode")) return { text: "</>" };
      if (key.includes("deepseek")) return { icon: "deepseek" };
      if (key.includes("anthropic") || key.includes("claude"))
        return { text: "✦" };
      if (key.includes("openai") || key.includes("codex")) return { text: "◎" };
      if (key.includes("gemini") || key.includes("google")) return { text: "✧" };
      if (key.includes("copilot") || key.includes("github"))
        return { text: "◈" };
      if (key.includes("cursor")) return { text: "⌘" };
      if (key.includes("zai") || key.includes("z.ai")) return { icon: "poll" };
      const rawIcon = String(entry?.icon ?? "").trim();
      if (
        rawIcon &&
        !/[\uE000-\uF8FF]/u.test(rawIcon) &&
        !/[\u{F0000}-\u{10FFFF}]/u.test(rawIcon) &&
        !/\p{Extended_Pictographic}/u.test(rawIcon)
      ) {
        return { text: rawIcon };
      }
      return { text: "◆" };
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
              .child(icon("llm", 13, t.accent))
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
                iconLabel("inspector", "Detectar Contas", {
                  size: 11.5,
                  color: t.foreground,
                }),
                (_e, clickCx) => {
                  this.usageMenuOpen = false;
                  this.fetchUsageSnapshot(clickCx ?? cx, true, true);
                },
                true
              )
            )
            .when(usageSnap?.installed !== false, (el2) =>
              el2.child(
                this.actionBtn(
                  t,
                  "ck-usage-tui",
                  iconLabel("tui", "Abrir TUI", {
                    size: 11.5,
                    color: t.muted,
                  }),
                  () => {
                    this.usageMenuOpen = false;
                    launchTabCmd("ai-usagebar", [
                      usageSnap?.tui_binary_path || "ai-usagebar-tui",
                    ]);
                  }
                )
              )
            )
            .when(Boolean(selVendorMeta?.login), (el2) =>
              el2.child(
                this.actionBtn(
                  t,
                  "ck-usage-menu-login",
                  iconLabel("key", "Login", {
                    size: 11.5,
                    color: t.muted,
                  }),
                  () => {
                    this.usageMenuOpen = false;
                    launchTabCmd(
                      selBrandKey || "login",
                      String(selVendorMeta.login).trim().split(/\s+/)
                    );
                  }
                )
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
                const tabColor = isSel ? t.accent : t.muted;
                const tabName = ellipsize(
                  entry?.display_name || entry?.name || entry?.id || "AI",
                  16
                );
                const vIcon = usageVendorIcon(entry);
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
                  .text_color(tabColor)
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
                  .child(
                    vIcon.icon ? icon(vIcon.icon, 11, tabColor) : vIcon.text
                  )
                  .child(tabName);
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
                    iconLabel("inspector", "Detectar Contas", {
                      size: 11.5,
                      color: t.foreground,
                    }),
                    (_e, clickCx) =>
                      this.fetchUsageSnapshot(clickCx ?? cx, true, true),
                    true
                  )
                )
                .child(
                  this.actionBtn(
                    t,
                    "ck-usage-empty-tui",
                    iconLabel("settings", "Abrir TUI", {
                      size: 11.5,
                      color: t.muted,
                    }),
                    () =>
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
                          iconLabel("key", selVendorMeta.login, {
                            size: 11.5,
                            color: t.muted,
                          }),
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
                  const hasLeftFooter = Boolean(resetText || fallbackDetail);
                  const hasRightFooter = Boolean(pacingText);

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
                    .when(hasLeftFooter || hasRightFooter, (el3) =>
                      el3.child(
                        h_flex()
                          .gap(6)
                          .items_center()
                          .justify_between()
                          .child(
                            div()
                              .text_size(10.5)
                              .text_color(t.muted)
                              .child(
                                resetText
                                  ? iconLabel("timer", ellipsize(resetText, 34), {
                                      size: 10.5,
                                      color: t.muted,
                                      gap: 4,
                                    })
                                  : ellipsize(fallbackDetail || "", 36)
                              )
                          )
                          .child(
                            div()
                              .flex_none()
                              .text_size(10.5)
                              .text_color(isOver ? t.working : t.muted)
                              .child(
                                pacingText
                                  ? isOver
                                    ? iconLabel(
                                        "warning",
                                        ellipsize(pacingText, 22),
                                        {
                                          size: 10.5,
                                          color: t.working,
                                          gap: 4,
                                        }
                                      )
                                    : ellipsize(`✓ ${pacingText}`, 24)
                                  : ""
                              )
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
    this.activeAgentAddress = agent?.address ?? null;
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
    const modelPickerOpen = Boolean(
      shared("cockpit.modelPickerOpen") ?? this.modelPickerOpen
    );
    const modelPickerOnlyDrawer = Boolean(
      shared("cockpit.modelPickerOnlyDrawer") ?? this.modelPickerOnlyDrawer
    );
    const inspectorHidden = Boolean(
      findNode(layout(), "tchelvis-inspector")?.hidden
    );
    const chatVisible =
      !inspectorHidden &&
      activeView === "chat" &&
      !(usageOpen && usageOnlyDrawer) &&
      !(modelPickerOpen && modelPickerOnlyDrawer);
    if (chatVisible && !this.wasChatVisible) {
      this.wasChatVisible = true;
      this.scrollChatToBottom();
      this.focusField("chat");
    } else if (!chatVisible) {
      this.wasChatVisible = false;
    }

    const supAgent =
      (supSnap?.agents ?? []).find((a) => a.agent_id === agent?.address) ??
      null;
    const actionCard = supAgent?.action_card ?? agent?.action_card ?? null;
    const evalError = supAgent?.eval_error ?? agent?.eval_error ?? null;
    if (actionCard && agent) {
      const nextKey = `${agent.address}:${actionCard.triggered_at ?? ""}`;
      if (this.editingField !== "command" && this.commandDraftKey !== nextKey) {
        this.inputs.command.set_value(actionCard.hc_command_payload ?? "");
        this.commandDraftKey = nextKey;
      }
    } else if (this.editingField !== "command") {
      if (this.commandDraftKey || this.fieldValue("command")) {
        this.clearField("command");
      }
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

    if (modelPickerOpen && modelPickerOnlyDrawer) {
      return v_flex()
        .id("ck-inspector-root")
        .size_full()
        .track_focus(this.handle)
        .text_color(t.foreground)
        .on_key_down((e, cx) => this.handleKeyDown(e, cx, agent))
        .child(this.renderModelPicker(cx, t, true));
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
          .child(div().flex_1())
          .child(
            this.actionBtn(t, "ck-ins-refresh", "↻", (_e, cx) => {
              this.refreshCockpit(cx, true);
              this.refreshInspector(cx);
              this.fetchHomeSnapshot(cx);
              if (usageOpen) this.fetchUsageSnapshot(cx, true, false);
              if (modelPickerOpen) this.fetchModelList(cx, null);
              if (activeView === "doctor") {
                this.runDoctorCheck(cx);
                if (!this.benchRunning) this.fetchBenchLast(cx);
              }
              if (activeView === "chat") {
                this.fetchSupervisorStatus(cx);
              }
            })
          )
          .child(
            this.actionBtn(t, "ck-ins-close", "×", () => {
              this.usageOpen = false;
              this.usageOnlyDrawer = false;
              this.usageMenuOpen = false;
              this.modelPickerOpen = false;
              this.modelPickerOnlyDrawer = false;
              setShared("cockpit.usageOpen", false);
              setShared("cockpit.usageOnlyDrawer", false);
              setShared("cockpit.modelPickerOpen", false);
              setShared("cockpit.modelPickerOnlyDrawer", false);
              run("layout.hide", { id: "tchelvis-inspector" });
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
      .when(modelPickerOpen, (el) =>
        el.child(
          v_flex()
            .flex_none()
            .border_b(1)
            .border_color(t.border)
            .child(this.renderModelPicker(cx, t, false))
        )
      )
      .when(activeView === "home", (el) =>
        el.child(this.renderHomeBody(cx, t))
      )
      .when(activeView === "doctor", (el) =>
        el.child(this.renderDoctorBody(cx, t))
      )
      .when(activeView === "chat", (el) =>
        el.child(this.renderChatBody(cx, t))
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
                          Input.new(this.inputs.prompt)
                            .flex_1()
                            .min_w(0)
                            .h(28)
                            .px(9)
                            .rounded(6)
                            .border(1)
                            .border_color(
                              this.editingField === "prompt"
                                ? t.accent
                                : t.border
                            )
                            .bg(t.background)
                            .text_size(12)
                            .text_color(t.foreground)
                        )
                        .child(
                          this.actionBtn(
                            t,
                            "ck-prompt-send",
                            "Send ↵",
                            (_e, cx) => this.sendPromptGui(cx, agent.address),
                            !isBlankDraft(this.fieldValue("prompt"))
                          )
                        )
                    )
                    .when(Boolean(shared("cockpit.attachDebug")), (el) => {
                      const dbg = shared("cockpit.attachDebug");
                      const focusedLabel =
                        dbg.mappedFocused || dbg.rawFocusedTerm || "none";
                      const paneLabel = dbg.paneAfter ?? dbg.livePane ?? "none";
                      return el.child(
                        div()
                          .text_size(10.5)
                          .text_color(t.muted)
                          .child(
                            `attach: ${dbg.action ?? "none"} · focused=${focusedLabel} · pane=${paneLabel}`
                          )
                      );
                    })
                )
                // 2. Supervisor Action Card / Status (SUPERVISOR & GUARDRAILS)
                .child(sectionTitle("SUPERVISOR & GUARDRAILS"))
                .child(
                  v_flex()
                    .id("ck-inspector-llm-card")
                    .p(10)
                    .gap(4)
                    .rounded(8)
                    .border(1)
                    .border_color(t.border)
                    .bg(t.background)
                    .cursor_pointer()
                    .hover((s) => s.bg(t.hover))
                    .on_mouse_down("left", (_e, clickCx) =>
                      clickCx.stop_propagation()
                    )
                    .on_click((_e, clickCx) =>
                      this.toggleModelPicker(clickCx ?? cx)
                    )
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
                              iconLabel(
                                "llm",
                                `${supSnap?.llm_model || "lfm3:1.7b"} (${supSnap?.llm_backend || "ollama"} · ${gpuOn(supSnap) ? "GPU" : "CPU"}${llmMemoryLabel(supSnap) ? ` · ${llmMemoryLabel(supSnap)}` : ""})`,
                                { size: 12, color: t.accent, gap: 5 }
                              )
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
                              : supSnap?.llm_managed && supSnap?.llm_container_state !== "running"
                                ? "DESATIVADO"
                                : supSnap?.llm_managed
                                  ? "INICIANDO…"
                                  : supSnap
                                    ? "OFFLINE"
                                    : "VERIFICANDO…",
                            supSnap?.llm_connected
                              ? supSnap?.model_loaded
                                ? t.accent
                                : supSnap?.model_installed
                                  ? t.done
                                  : t.working
                              : supSnap?.llm_managed && supSnap?.llm_container_state !== "running"
                                ? t.muted
                                : supSnap?.llm_managed
                                  ? t.working
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
                    .when(Boolean(evalError), (el3) =>
                      el3.child(
                        div()
                          .text_size(11)
                          .text_color(t.working)
                          .child(
                            iconLabel(
                              "warning",
                              `LLM evaluation failed: ${evalError}`,
                              { size: 11, color: t.working, gap: 4 }
                            )
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
                        Input.new(this.inputs.command)
                          .w_full()
                          .h(30)
                          .px(9)
                          .rounded(6)
                          .border(1)
                          .border_color(
                            this.editingField === "command"
                              ? t.accent
                              : t.border
                          )
                          .bg(t.background)
                          .text_size(12)
                          .text_color(t.foreground)
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
                .when(
                  !actionCard &&
                    agent?.status === "blocked" &&
                    ["permission", "question", "auth"].includes(
                      String(agent?.kind ?? "").toLowerCase()
                    ),
                  (el3) => {
                    const supMode = supSnap?.mode ?? "SemiAuto";
                    const isSemiAuto = supMode === "SemiAuto";
                    const isDestructive = Boolean(agent?.prompt_destructive);
                    const kindLabel = String(agent?.kind ?? "blocked").toUpperCase();
                    return el3.child(
                      v_flex()
                        .id("ck-pending-prompt-card")
                        .p(11)
                        .gap(8)
                        .rounded(10)
                        .border(1)
                        .border_color(withAlpha(t.blocked, "77", t.blocked))
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
                                .child(this.pillBadge(t, kindLabel, t.blocked))
                                .when(isDestructive, (el4) =>
                                  el4.child(
                                    this.pillBadge(t, "destrutivo", t.blocked)
                                  )
                                )
                            )
                            .child(
                              div()
                                .text_size(10.5)
                                .text_color(t.muted)
                                .child("Pending prompt")
                            )
                        )
                        .child(
                          div()
                            .text_size(12)
                            .font_semibold()
                            .text_color(t.foreground)
                            .child(
                              agent?.message ||
                                "Aguardando resposta no terminal."
                            )
                        )
                        .child(
                          h_flex()
                            .gap(8)
                            .items_center()
                            .when(isSemiAuto && !isDestructive, (el4) =>
                              el4.child(
                                this.actionBtn(
                                  t,
                                  "ck-pending-approve",
                                  "Approve",
                                  (_e, cx) =>
                                    this.respondPendingPrompt(
                                      cx,
                                      agent.address,
                                      "approve"
                                    ),
                                  true,
                                  false
                                )
                              )
                            )
                            .when(isSemiAuto, (el4) =>
                              el4.child(
                                this.actionBtn(
                                  t,
                                  "ck-pending-deny",
                                  "Deny",
                                  (_e, cx) =>
                                    this.respondPendingPrompt(
                                      cx,
                                      agent.address,
                                      "deny"
                                    ),
                                  false,
                                  isDestructive
                                )
                              )
                            )
                            .child(
                              this.actionBtn(
                                t,
                                "ck-pending-open",
                                "Open",
                                (_e, cx) =>
                                  void this.focusOrAttachTerminal(
                                    cx,
                                    agent,
                                    false
                                  ),
                                false,
                                false
                              )
                            )
                        )
                    );
                  }
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
                              iconLabel("poll", "Evaluate Now", {
                                size: 11.5,
                                color: t.muted,
                              }),
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
                          line.includes("\u26a0") || line.includes("Interceptado");
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
                                  this.focusField("label");
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
                              Input.new(this.inputs.label)
                                .flex_1()
                                .min_w(0)
                                .h(26)
                                .px(8)
                                .rounded(6)
                                .border(1)
                                .border_color(
                                  this.editingField === "label"
                                    ? t.accent
                                    : t.border
                                )
                                .bg(t.surface)
                                .text_size(11.5)
                                .text_color(t.foreground)
                            )
                            .child(
                              this.actionBtn(
                                t,
                                "ck-lbl-save",
                                "Tag",
                                (_e, cx) => {
                                  const val = this.fieldValue("label");
                                  if (isBlankDraft(val)) return;
                                  this.applyLabelGui(
                                    cx,
                                    agent.address,
                                    this.labelKey,
                                    val.trim()
                                  );
                                },
                                !isBlankDraft(this.fieldValue("label"))
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
    const llmManagedStopped =
      Boolean(supSnap?.llm_managed) && supSnap?.llm_container_state !== "running";
    const numGpu = supSnap?.num_gpu ?? 0;
    const blockedCount = liveMerged.filter((a) => a.status === "blocked").length;
    const workingCount = liveMerged.filter((a) => a.status === "working").length;
    const modeLabel = supSnap?.mode_label ?? "Semi-Auto (Whitelist)";
    const inspectorHidden = Boolean(
      findNode(layout(), "tchelvis-inspector")?.hidden
    );
    const usageOpen = Boolean(shared("cockpit.usageOpen") ?? this.usageOpen);
    const mac = win()?.platform === "macos";
    const b = keys().find(
      (k) => k.section === "global" && k.action === "app.palette"
    );
    const paletteHint = b ? b.keys.toUpperCase() : mac ? "⌘K" : "CTRL+SHIFT+P";

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
              .child(`Tchelvis v${version}`)
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
          .text_color(
            llmConnected
              ? t.done
              : llmManagedStopped
                ? t.muted
                : supSnap
                  ? t.blocked
                  : t.muted
          )
          .child(
            `LLM: ${llmModel} (${llmConnected ? "ONLINE" : llmManagedStopped ? "DESATIVADO" : supSnap ? "OFFLINE" : "…"} · ${gpuOn(supSnap) ? "GPU" : "CPU num_gpu=0"})`
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
      // Right side: Usage and command palette hint
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
