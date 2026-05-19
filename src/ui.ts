import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";
import { lintRule } from "./lint.ts";
import type {
  AddLeafPreview,
  AddLeafRequest,
  AppInfo,
  AuditLogPage,
  AuditRecordView,
  AuditSide,
  DeleteLeafPreview,
  DeleteLeafRequest,
  JsonValue,
  KnownProject,
  LoadedScopes,
  MoveLeafKind,
  MoveLeafPreview,
  MoveLeafRequest,
  MoveLeafSide,
  MoveOptions,
  PathSeg,
  PermissionKind,
  Preferences,
  RuntimeInfo,
  Scope,
  ScopeView,
  Theme,
} from "./types.ts";
import { AUDIT_LOG_MAX_SIZE_MB, SCOPES, SEARCH_INPUT_ID } from "./types.ts";

interface AppProps {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  query: string;
  preferences: Preferences;
  runtime: RuntimeInfo;
  /** Every Claude project discovered on this machine (#106). Sourced from
   *  the Rust backend's `list_known_projects` IPC; empty until that
   *  resolves and after a discovery failure. The Move-to submenu folds the
   *  current project in via `getKnownProjects` so the menu is never empty
   *  even on a fresh install. */
  knownProjects: KnownProject[];
  onPickProject: () => void;
  /** Load a project the user picked from the recent-projects dropdown (#47).
   *  Same query-reset semantics as `onPickProject`; the caller no-ops when
   *  the picked path equals the currently-loaded project. */
  onPickRecentProject: (projectDir: string) => void;
  onReload: () => void;
  onMoveLeaf: (req: MoveLeafRequest, trigger?: HTMLElement, opts?: MoveOptions) => void;
  /** Reclassify a permission rule between allow / deny / ask within the
   *  same scope (#8). Routes through the move-leaf primitive on the
   *  backend with `from === to` and `to_kind` set. */
  onChangeKind: (
    path: PathSeg[],
    scope: Scope,
    newKind: PermissionKind,
    trigger?: HTMLElement,
  ) => void;
  onDeleteLeaf: (req: DeleteLeafRequest, trigger?: HTMLElement) => void;
  onAddLeaf: (req: AddLeafRequest, trigger?: HTMLElement) => void;
  onOpenSettings: (trigger?: HTMLElement) => void;
  /** Open the About dialog (#21). Reads from `state.appInfo` populated at
   *  bootstrap; the caller routes the click through main.ts so it can also
   *  refresh the cache if it's stale. */
  onOpenAbout: (trigger?: HTMLElement) => void;
  /** Open the audit-log History dialog (#19 phase 2). main.ts fetches the
   *  records via `list_audit_records` IPC and hands them to the renderer
   *  in this callback's body, mirroring the openSettings / openAbout
   *  trigger-pattern so focus restore lands on the History button. */
  onOpenHistory: (trigger?: HTMLElement) => void;
  onQueryChange: (next: string) => void;
}

/**
 * Build the list of known Claude projects feeding the Move-to submenu.
 * Sources from the backend's discovery (`props.knownProjects`) but always
 * folds in the currently loaded project — so a freshly cloned repo with no
 * transcripts yet still surfaces *somewhere* in the menu, and the user
 * never has to leave-and-return to land the current project under their
 * cursor.
 */
function getKnownProjects(props: AppProps): KnownProject[] {
  const merged: KnownProject[] = [...props.knownProjects];
  if (props.projectDir) {
    const root = props.projectDir;
    const sepIdx = Math.max(root.lastIndexOf("/"), root.lastIndexOf("\\"));
    const name = sepIdx >= 0 ? root.slice(sepIdx + 1) || root : root;
    // Case-insensitive de-dupe against the backend list. On Windows the
    // discovery side's path may differ in case from the picker's, but they
    // refer to the same directory; surfacing both would put two identical
    // entries side-by-side in the submenu.
    const norm = root.toLowerCase();
    if (!merged.some((p) => p.root.toLowerCase() === norm)) {
      merged.push({ name, root });
    }
  }
  merged.sort((a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
  return merged;
}

const PERMISSION_KINDS: ReadonlyArray<PermissionKind> = ["allow", "deny", "ask"];

/**
 * Mirror of Rust's `validate_movable_path`: which JSON paths the move-leaf
 * primitive accepts. Three shapes for v1:
 *   1. `[<top-level-key>]` (any key except `permissions`)
 *   2. `["permissions", "allow"|"deny"|"ask"]` — whole rule list
 *   3. `["permissions", "allow"|"deny"|"ask", <index>]` — single rule
 * Other intermediate sub-paths (e.g. `env.PATH`) are explicitly out of
 * scope for issue #67 and a backend rejection would round-trip as a
 * confusing error toast — better to gate the affordance here.
 */
function isMovablePath(path: PathSeg[]): boolean {
  if (path.length === 1) return path[0] !== "permissions" && typeof path[0] === "string";
  if (path[0] === "permissions") {
    if (path.length === 2 && typeof path[1] === "string") {
      return PERMISSION_KINDS.includes(path[1] as PermissionKind);
    }
    if (
      path.length === 3 &&
      typeof path[1] === "string" &&
      // Constrain to non-negative integers — the Rust side deserializes
      // path indices into `usize`, so floats / negatives would be rejected
      // at the IPC boundary. Today every index we generate comes from
      // tree-walking so it's already a non-negative integer; this guard
      // keeps it that way under future refactors.
      typeof path[2] === "number" &&
      Number.isInteger(path[2]) &&
      path[2] >= 0
    ) {
      return PERMISSION_KINDS.includes(path[1] as PermissionKind);
    }
  }
  return false;
}

/**
 * The kind under `permissions.<kind>...` for paths that target permission
 * data, or `null` for any other path. Drives the allow/deny/ask styling on
 * permission tree leaves and the chip-row diff in the confirm modal.
 */
function permissionKindForPath(path: PathSeg[]): PermissionKind | null {
  if (path.length < 2 || path[0] !== "permissions") return null;
  const kind = path[1];
  if (typeof kind !== "string") return null;
  if (PERMISSION_KINDS.includes(kind as PermissionKind)) return kind as PermissionKind;
  return null;
}

function matchesLoweredQuery(rule: string, lowerQuery: string): boolean {
  if (lowerQuery === "") return true;
  return rule.toLowerCase().includes(lowerQuery);
}

/**
 * Default threshold above which a tool's rules collapse under a
 * synthetic group node in the per-scope tree view (#68). Matches
 * `Preferences.group_rules_at`'s default — the runtime value comes from
 * the user's preference (#115) via [`resolveGroupThreshold`]. This
 * constant is retained as the default for unit-test call sites that
 * exercise the grouping helper directly without an `AppProps` in scope.
 */
const TOOL_GROUP_THRESHOLD = 2;

/**
 * Resolve the user's `group_rules_at` preference to the integer
 * threshold the grouping helper expects. `null` (= "never group" per
 * the user's choice) is mapped to `Infinity` so the helper's
 * `count < threshold` check never fires and every rule renders as a
 * `Single`. Same call site contract as the default-2 constant.
 */
function resolveGroupThreshold(prefs: Preferences): number {
  return prefs.group_rules_at ?? Number.POSITIVE_INFINITY;
}

/**
 * Entry in the per-kind rule list after grouping. `Single` keeps the
 * original index so the leaf's path stays addressable; `Group` carries
 * every member's index so the children can each rebuild their path.
 *
 * Children inside a group preserve their original disk index in `members`
 * — the backend still addresses rules by their position in the on-disk
 * array, and the UI grouping is purely visual.
 */
export type GroupedRuleEntry =
  | { kind: "single"; index: number; rule: string }
  | { kind: "group"; tool: string; members: Array<{ index: number; rule: string }> };

/**
 * Group a flat permission-rule list by the `Tool` prefix in each
 * `Tool(args)` rule. Rules that don't match `Tool(...)` syntax (no
 * parentheses, or anything else the regex misses) stay flat as singles.
 *
 * Order rule: emit each tool's group at the position of its first member,
 * absorb later members silently. Singles emit at their original position.
 * That preserves the user's top-down reading order — a rule never moves
 * past a sibling that came after it on disk.
 *
 * Single-member "groups" collapse back to a `Single` entry so a tool with
 * only one rule doesn't render a one-child fold.
 */
export function groupByToolPrefix(
  rules: string[],
  threshold: number = TOOL_GROUP_THRESHOLD,
): GroupedRuleEntry[] {
  // First pass: count rules per tool so the second pass knows whether a
  // tool meets the threshold without rescanning the tail of the list each
  // time it sees a member.
  const counts = new Map<string, number>();
  const tools: Array<string | null> = rules.map((rule) => {
    const tool = toolPrefixOf(rule);
    if (tool !== null) counts.set(tool, (counts.get(tool) ?? 0) + 1);
    return tool;
  });

  const emitted = new Set<string>();
  const out: GroupedRuleEntry[] = [];
  for (let i = 0; i < rules.length; i++) {
    const rule = rules[i];
    const tool = tools[i];
    if (tool === null || (counts.get(tool) ?? 0) < threshold) {
      out.push({ kind: "single", index: i, rule });
      continue;
    }
    if (emitted.has(tool)) continue;
    emitted.add(tool);
    const members: Array<{ index: number; rule: string }> = [];
    for (let j = i; j < rules.length; j++) {
      if (tools[j] === tool) members.push({ index: j, rule: rules[j] });
    }
    out.push({ kind: "group", tool, members });
  }
  return out;
}

/**
 * Extract the `Tool` part of a `Tool(args)` rule. Returns null when the
 * rule doesn't have a `(` (malformed or shorthand the lint flags), so the
 * caller falls back to rendering it as a plain single.
 *
 * Whitespace-tolerant on either side of the `(` to match the lint's
 * permissive parsing — we'd rather group `handoff (copilot *)` with the
 * rest of the handoff family than orphan it on a literal whitespace
 * mismatch.
 */
function toolPrefixOf(rule: string): string | null {
  const paren = rule.indexOf("(");
  if (paren <= 0) return null;
  const tool = rule.slice(0, paren).trim();
  return tool === "" ? null : tool;
}

const SCOPE_LABELS: Record<Scope, string> = {
  local: "Local",
  project: "Project",
  user_local: "User-Local",
  user: "User",
};

const KIND_LABELS: Record<PermissionKind, string> = {
  allow: "allow",
  deny: "deny",
  ask: "ask",
};

/**
 * Help-tooltip content shape (#9). One-sentence `summary` is the main
 * payload; `docs` is the optional link the popover footer surfaces. Keep
 * summaries to ~one sentence — anything longer belongs on the docs site.
 */
export interface HelpContent {
  summary: string;
  docs?: string;
}

/** Canonical Claude Code settings docs link, shared by every popover that
 *  doesn't specify its own. Saves maintaining 12+ deep-links that would rot
 *  whenever Anthropic reshuffles the docs site. */
const HELP_DOCS_URL = "https://docs.claude.com/en/docs/claude-code/settings";

/**
 * One-sentence explanations for the top-level Claude Code settings keys
 * ClaudeScope surfaces. Unknown keys (anything not in this map) render
 * without a tooltip — silence beats a confident guess.
 */
const KEY_HELP: Record<string, HelpContent> = {
  permissions: {
    summary:
      "Rules controlling which tools Claude Code can use, and which require explicit confirmation.",
  },
  env: {
    summary: "Environment variables Claude Code injects into every tool invocation in this scope.",
  },
  hooks: {
    summary: "Shell commands run on lifecycle events (PreToolUse, Stop, SessionStart, …).",
  },
  theme: {
    summary: "Color theme for Claude Code's terminal UI (dark, light, dark-daltonized, …).",
  },
  model: {
    summary: "Default Claude model id used by sessions started in this scope.",
  },
  apiKeyHelper: {
    summary: "Path to a script Claude Code runs to fetch a fresh API key on demand.",
  },
  statusLine: {
    summary: "Customizes the status line rendered below the prompt during a session.",
  },
  enableAllProjectMcpServers: {
    summary:
      "When true, every MCP server defined in the project is enabled without per-server prompts.",
  },
  enabledMcpjsonServers: {
    summary:
      "Explicit allow-list of MCP servers (from a project's `.mcp.json`) enabled in this scope.",
  },
  disabledMcpjsonServers: {
    summary: "Explicit deny-list of MCP servers — overrides the enabled list.",
  },
  outputStyle: {
    summary: "Output formatting preset for Claude's responses (default, explanatory, learning, …).",
  },
  forceLoginMethod: {
    summary: "Pins the login flow to a specific provider (claude-ai, anthropic-api, console).",
  },
  cleanupPeriodDays: {
    summary: "Days before Claude Code cleans up chat history (0 disables cleanup).",
  },
  includeCoAuthoredBy: {
    summary: "Adds a `Co-Authored-By: Claude` trailer to git commits Claude creates.",
  },
  spinnerTipsEnabled: {
    summary: "Shows rotating tips below the spinner during long tool runs.",
  },
  alwaysThinkingEnabled: {
    summary: "Sends every prompt through Claude's extended-thinking mode by default.",
  },
};

/**
 * Per-kind help for `permissions.allow` / `.deny` / `.ask`. Surfaces on
 * the combined panel's kind label and on the kind's `tree-key` span in
 * each per-scope tree branch.
 */
const KIND_HELP: Record<PermissionKind, HelpContent> = {
  allow: {
    summary: "Rules Claude Code may match and use without asking the user first.",
  },
  deny: {
    summary: "Rules Claude Code is forbidden from using — deny always wins over allow.",
  },
  ask: {
    summary: "Rules Claude Code may use only after explicit per-invocation confirmation.",
  },
};

/**
 * Per-scope help on the column header. The precedence sentence shows up
 * in every scope's tooltip so the relationship is reinforced from
 * wherever the user happens to hover.
 */
const SCOPE_HELP: Record<Scope, HelpContent> = {
  local: {
    summary:
      "Project-local override (`.claude/settings.local.json`). Gitignored. Highest precedence — wins over Project, User-Local, and User.",
  },
  project: {
    summary:
      "Project-wide settings (`.claude/settings.json`). Committed to the repo. Overrides User-Local and User; overridden by Local.",
  },
  user_local: {
    summary:
      "Machine-local override (`~/.claude/settings.local.json`). Overrides User; overridden by Project and Local.",
  },
  user: {
    summary:
      "Machine-global settings (`~/.claude/settings.json`). Lowest precedence — every other scope wins over it.",
  },
};

/** Look up help for a top-level settings key. Returns null for unknown
 *  keys so the caller can skip the affordance entirely (the issue's
 *  "absence is better than a misleading guess"). */
export function lookupKeyHelp(key: string): HelpContent | null {
  // `key in KEY_HELP` instead of Object.hasOwn / hasOwnProperty: avoids
  // tsconfig's lib level requirement for ES2022 and dodges the no-
  // prototype-builtins rule biome would otherwise re-format around. The
  // map is a typed object literal — no inherited keys collide.
  return key in KEY_HELP ? KEY_HELP[key] : null;
}

export function lookupKindHelp(kind: PermissionKind): HelpContent {
  return KIND_HELP[kind];
}

export function lookupScopeHelp(scope: Scope): HelpContent {
  return SCOPE_HELP[scope];
}

/**
 * Resolve help content for a tree branch by its JSON path. Handles both
 * the top-level key case (`[<key>]`) and the permission-kind case
 * (`["permissions", "allow"|"deny"|"ask"]`). Anything deeper returns
 * null — leaves themselves carry rule-shaped content that doesn't need
 * generic help.
 */
function lookupBranchHelp(path: PathSeg[]): HelpContent | null {
  if (path.length === 1 && typeof path[0] === "string") {
    return lookupKeyHelp(path[0]);
  }
  if (path.length === 2 && path[0] === "permissions" && typeof path[1] === "string") {
    const kind = path[1];
    if (PERMISSION_KINDS.includes(kind as PermissionKind)) {
      return lookupKindHelp(kind as PermissionKind);
    }
  }
  return null;
}

let helpPopoverSeq = 0;

/**
 * Decorate `trigger` with the standard "ⓘ next to it" help affordance
 * (#9). Appends two siblings to `trigger`'s parent in `parent`:
 *   1. A focusable `ⓘ` button (the click/keyboard pin target).
 *   2. A `role="tooltip"` popover holding the summary + docs link.
 *
 * `trigger` itself receives a `has-help` class so the dotted underline
 * shows on the label, and `aria-describedby` so screen readers announce
 * the popover content. Hover and focus reveals are CSS-driven on the
 * shared `.popover-wrap`; click pins via the global popover singleton.
 */
export function attachHelpTooltip(trigger: HTMLElement, content: HelpContent): HTMLElement {
  // The trigger needs to live inside a positioned wrap so the absolutely-
  // positioned popover anchors against it. Build the wrap, move the
  // trigger inside, and return the wrap so callers can substitute it for
  // the bare trigger in their layout.
  const wrap = document.createElement("span");
  wrap.className = "popover-wrap help-wrap";
  trigger.classList.add("has-help");
  wrap.appendChild(trigger);

  const popoverId = `help-popover-${++helpPopoverSeq}`;
  trigger.setAttribute("aria-describedby", popoverId);

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "help-info";
  btn.textContent = "ⓘ";
  // Short `aria-label` only — the descriptive content is on the popover
  // via `aria-describedby`. Without the short label, screen readers
  // would announce the glyph as "circled latin small letter i", which
  // is useless.
  btn.setAttribute("aria-label", "Help");
  btn.setAttribute("aria-describedby", popoverId);

  const pop = document.createElement("span");
  pop.id = popoverId;
  pop.className = "help-popover";
  pop.setAttribute("role", "tooltip");

  const summary = document.createElement("span");
  summary.className = "help-popover-summary";
  summary.textContent = content.summary;
  pop.appendChild(summary);

  const docsLink = document.createElement("a");
  docsLink.className = "help-popover-docs";
  docsLink.href = content.docs ?? HELP_DOCS_URL;
  docsLink.textContent = "Open settings docs ↗";
  // `target=_blank` opens via Tauri's URL handler in the OS browser
  // instead of inside the webview. `noopener` for the standard
  // anti-tabnabbing reason.
  docsLink.target = "_blank";
  docsLink.rel = "noopener noreferrer";
  pop.appendChild(docsLink);

  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (openPinnedPopover === wrap) {
      closePinnedPopover();
    } else {
      pinPopover(wrap);
    }
  });

  wrap.appendChild(btn);
  wrap.appendChild(pop);
  return wrap;
}

let modalIdCounter = 0;

// Set at the top of every `renderApp` call so the tree walkers can cheaply
// check "should this scope be rendered?" without each one rebuilding a Set
// from the visible_scopes array. Rendering is synchronous, so this
// module-level variable is effectively render-scoped.
let currentVisibleScopes: Set<Scope> = new Set();

function isScopeVisible(scope: Scope): boolean {
  return currentVisibleScopes.has(scope);
}

export function renderApp(root: HTMLElement, props: AppProps): void {
  // Full re-render destroys the DOM, including the search input the user
  // is typing into. Snapshot its focus + selection before we wipe, restore
  // after we rebuild — otherwise focus jumps to body on every keystroke and
  // the input becomes unusable.
  const active = document.activeElement;
  const preserveSearchFocus = active instanceof HTMLInputElement && active.id === SEARCH_INPUT_ID;
  const caret = preserveSearchFocus
    ? { start: active.selectionStart, end: active.selectionEnd }
    : null;

  root.innerHTML = "";
  // The full DOM wipe just destroyed any pinned popover (lint or help);
  // clear the tracking state so a stale `openPinnedPopover` doesn't
  // survive re-render and confuse the next outside-click / Escape.
  closePinnedPopover();
  // Same reasoning for an in-flight drag: if a re-render lands mid-drag
  // (e.g. an external file change reloads scopes), the source chip is
  // detached and `dragend` may not fire — drop the singleton so the next
  // gesture starts clean.
  clearDragState();
  // Compute the visible-scope set once here — every helper below reads
  // `isScopeVisible` instead of rebuilding this Set per scope/rule/tree
  // node, which mattered for payloads with many rules.
  currentVisibleScopes = new Set(props.preferences.visible_scopes);
  // Project changed (or first load) — drop any tree-view expansions from
  // the previous project so they don't bleed across into a new tree where
  // the same scope+path could mean something different.
  if (props.projectDir !== lastRenderedProjectDir) {
    openTreeNodes.clear();
    // Cross-pane highlight (#49) is project-scoped: a rule that
    // existed in the previous project's `Bash(...)` allowlist might
    // be totally unrelated to one with the same string in the new
    // project. Clear to avoid surprising the user with a highlight
    // they didn't request in the new context.
    highlightedRule = null;
    lastRenderedProjectDir = props.projectDir;
  }
  // Wire the document-level click/keydown handlers exactly once. The
  // handlers themselves survive `innerHTML = ""` rebuilds; this just
  // guarantees they're installed by the first render.
  ensureRuleHighlightDelegation();
  root.appendChild(header(props));
  const banner = sandboxBanner(props.runtime);
  if (banner) root.appendChild(banner);

  if (!props.scopes) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = props.busy ? "Loading…" : "No settings loaded.";
    root.appendChild(empty);
    restoreSearchFocus(preserveSearchFocus, caret);
    return;
  }

  // Seed `openTreeNodes` with the default-open permission branches once per
  // project, on the first render that has scopes available. Done here
  // (rather than inside `treeBranch`'s own auto-open clause) so a manual
  // collapse via `<details>.toggle` is *authoritative* — without this,
  // any subsequent render after a move or filter change would reopen the
  // branch and the collapsed state would be impossible to persist.
  // `lastSeededProjectDir` decouples the seed from `lastRenderedProjectDir`
  // because scopes can lag a project change by one render (busy=true,
  // scopes=null first; scopes arrive on the next render).
  if (lastSeededProjectDir !== props.projectDir) {
    lastSeededProjectDir = props.projectDir;
    seedDefaultOpenPermissions(props.scopes, resolveGroupThreshold(props.preferences));
  }

  // Lowercase the query once per render instead of per rule; scopeGrid/
  // combinedPanel push this down into every filter call.
  const lowerQuery = props.query.toLowerCase();
  root.appendChild(combinedPanel(props.scopes, props, lowerQuery));
  root.appendChild(scopeGrid(props, lowerQuery));
  restoreSearchFocus(preserveSearchFocus, caret);
  // Re-apply the cross-pane highlight (#49) once the new chips are
  // in the DOM. Self-heals if the previously-highlighted rule no
  // longer appears anywhere (e.g. just-moved last copy).
  applyRuleHighlight();
}

function restoreSearchFocus(
  shouldRestore: boolean,
  caret: { start: number | null; end: number | null } | null,
): void {
  if (!shouldRestore) return;
  const input = document.getElementById(SEARCH_INPUT_ID) as HTMLInputElement | null;
  if (!input) return;
  input.focus();
  if (caret && caret.start !== null && caret.end !== null) {
    try {
      input.setSelectionRange(caret.start, caret.end);
    } catch {
      // `type="search"` supports this on all major browsers, but some
      // embedded webviews might not — fall through silently.
    }
  }
}

/**
 * Sandbox banner shown when the user launched with `--home` /
 * `CLAUDE_SCOPE_HOME` (or the project counterpart). Sits directly under
 * the toolbar, full-width, persistent — the issue's acceptance criterion
 * is "the user can't forget they're in scratch mode," so we deliberately
 * don't make this dismissible.
 */
function sandboxBanner(runtime: RuntimeInfo): HTMLElement | null {
  if (!runtime.home_override && !runtime.project_override) return null;
  const banner = document.createElement("div");
  banner.className = "sandbox-banner";
  // `role="note"` (not "status"): the banner is supplementary context,
  // not a live status update. renderApp wipes the DOM on every keystroke,
  // so a polite live region (which "status" implies) would re-announce
  // the same scratch-mode message to screen readers on every render.
  banner.setAttribute("role", "note");

  const label = document.createElement("strong");
  label.textContent = "Sandbox mode";
  banner.appendChild(label);

  const detail = document.createElement("span");
  const parts: string[] = [];
  if (runtime.home_override) parts.push(`home: ${runtime.home_override}`);
  if (runtime.project_override) parts.push(`project: ${runtime.project_override}`);
  detail.textContent = ` — ${parts.join(" · ")}`;
  banner.appendChild(detail);
  return banner;
}

/**
 * Project label rendered as a dropdown of recently-opened projects (#47).
 * Replaces the static "Project: …" label so users toggling between a small
 * working set of repos don't have to round-trip the OS picker every time.
 *
 * Layout: the button itself shows the current project path (ellipsised by
 * `.project-dir` styles); clicking it opens the existing `openContextMenu`
 * primitive — same keyboard nav, outside-click dismissal, and focus-restore
 * behavior the right-click menu already ships, so there's only one mental
 * model to learn for menu UX in the app.
 *
 * Menu contents:
 *   - Each entry in `preferences.recent_projects` except the currently
 *     loaded path (an entry that just reloads what you're on would be
 *     misleading affordance).
 *   - A separator.
 *   - "Open project…" — same callback as the standalone Open button, so
 *     users who prefer the picker still have a one-click path to it.
 *
 * When `recent_projects` is empty (fresh install, or only the current
 * project has been opened), the menu collapses to just the picker entry —
 * still a useful affordance, never a dead-end empty menu.
 */
function projectDirDropdown(props: AppProps): HTMLElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "project-dir project-dir-trigger";
  btn.setAttribute("aria-haspopup", "menu");
  btn.setAttribute("aria-expanded", "false");
  btn.disabled = props.busy;
  const label = props.projectDir ? `Project: ${props.projectDir}` : "No project selected";
  btn.textContent = label;
  // Title attribute carries the full path so the truncated middle of a
  // long path is still discoverable on hover, the way GitHub does it.
  btn.title = label;

  // Arrow caret kept inside the button (separate span) so the title /
  // text-overflow ellipsis on the parent doesn't swallow it when the path
  // overflows the column. The space before `▾` is part of the button's
  // text content for visual breathing room without an extra flex layout.
  const caret = document.createElement("span");
  caret.className = "project-dir-caret";
  caret.textContent = " ▾";
  caret.setAttribute("aria-hidden", "true");
  btn.appendChild(caret);

  btn.addEventListener("click", (e) => {
    e.preventDefault();
    const items = buildRecentProjectMenuItems(props);
    if (items.length === 0) return;
    const rect = btn.getBoundingClientRect();
    openContextMenu(items, rect.left, rect.bottom, btn);
  });

  return btn;
}

/**
 * Items for the project-dir dropdown. Exported indirectly via the surface
 * area of the button click handler above; kept as a separate function so
 * vitest can drive it without going through `openContextMenu`'s real DOM
 * positioning (which is awkward to assert against in a JSDOM environment).
 */
function buildRecentProjectMenuItems(props: AppProps): MenuItem[] {
  const items: MenuItem[] = [];
  const current = props.projectDir;
  for (const path of props.preferences.recent_projects) {
    if (path === current) continue;
    items.push({
      label: path,
      onClick: () => props.onPickRecentProject(path),
    });
  }
  if (items.length > 0) {
    items.push({ separator: true });
  }
  items.push({
    label: "Open project…",
    onClick: () => props.onPickProject(),
  });
  return items;
}

function header(props: AppProps): HTMLElement {
  const bar = document.createElement("header");
  bar.className = "topbar";

  const title = document.createElement("div");
  title.className = "title";
  title.innerHTML =
    "<strong>ClaudeScope</strong><span class='subtitle'>Promote Claude Code settings between scopes</span>";
  bar.appendChild(title);

  bar.appendChild(projectDirDropdown(props));

  bar.appendChild(searchBox(props));

  const actions = document.createElement("div");
  actions.className = "actions";

  const pick = document.createElement("button");
  pick.textContent = "Open project…";
  pick.onclick = props.onPickProject;
  pick.disabled = props.busy;
  actions.appendChild(pick);

  const reload = document.createElement("button");
  reload.textContent = "Reload";
  reload.onclick = props.onReload;
  reload.disabled = props.busy || !props.scopes;
  actions.appendChild(reload);

  const history = document.createElement("button");
  history.textContent = "History";
  history.setAttribute("aria-label", "Open audit history");
  history.onclick = (e) => props.onOpenHistory(e.currentTarget as HTMLElement);
  actions.appendChild(history);

  const settings = document.createElement("button");
  settings.textContent = "Settings";
  settings.setAttribute("aria-label", "Open settings");
  settings.onclick = (e) => props.onOpenSettings(e.currentTarget as HTMLElement);
  actions.appendChild(settings);

  const about = document.createElement("button");
  about.textContent = "About";
  about.setAttribute("aria-label", "About ClaudeScope");
  about.onclick = (e) => props.onOpenAbout(e.currentTarget as HTMLElement);
  actions.appendChild(about);

  bar.appendChild(actions);
  return bar;
}

function searchBox(props: AppProps): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "search";

  const input = document.createElement("input");
  input.type = "search";
  input.className = "search-input";
  input.id = SEARCH_INPUT_ID;
  input.placeholder = "Filter rules…  (press / to focus)";
  input.value = props.query;
  input.setAttribute("aria-label", "Filter permission rules across scopes");
  input.autocomplete = "off";
  input.spellcheck = false;
  input.disabled = !props.scopes;
  input.addEventListener("input", () => props.onQueryChange(input.value));
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && input.value !== "") {
      e.preventDefault();
      input.value = "";
      props.onQueryChange("");
    }
  });
  wrap.appendChild(input);

  if (props.query !== "") {
    const clear = document.createElement("button");
    clear.type = "button";
    clear.className = "search-clear";
    clear.setAttribute("aria-label", "Clear filter");
    clear.textContent = "×";
    clear.onclick = () => {
      // Focus the input first so the pre-render snapshot in renderApp() sees
      // it as the active element and restores focus there — otherwise focus
      // jumps to body after the clear.
      input.focus();
      props.onQueryChange("");
    };
    wrap.appendChild(clear);
  }

  return wrap;
}

function combinedPanel(loaded: LoadedScopes, props: AppProps, lowerQuery: string): HTMLElement {
  const query = props.query;
  const panel = document.createElement("section");
  panel.className = "combined";
  const title = document.createElement("h2");
  title.textContent = "Combined permissions";
  panel.appendChild(title);
  const subtitle = document.createElement("p");
  subtitle.className = "combined-subtitle";
  subtitle.textContent = "Union across scopes — not a precedence-aware evaluation.";
  panel.appendChild(subtitle);

  // Three side-by-side group columns instead of stacked rows. The grid
  // mirrors the scope-grid breakpoint below (auto-fit + minmax) so the
  // panel collapses to a single column on narrow viewports in lockstep
  // with the rest of the layout.
  const groupsWrap = document.createElement("div");
  groupsWrap.className = "combo-groups";

  const kinds: PermissionKind[] = ["allow", "deny", "ask"];
  for (const kind of kinds) {
    const all = loaded.combined_permissions[kind];
    const allOrigins = loaded.combined_origins[kind];
    // Iterate `all` by index so each chip can pull its parallel origin
    // entry. Track match count separately for the "(m/n)" label without
    // building a filtered copy.
    let matchedCount = 0;
    const isFiltering = lowerQuery !== "";
    if (!isFiltering) {
      matchedCount = all.length;
    } else {
      for (const rule of all) {
        if (matchesLoweredQuery(rule, lowerQuery)) matchedCount++;
      }
    }
    const group = document.createElement("div");
    group.className = `combo-group combo-${kind}`;
    const label = document.createElement("span");
    label.className = "combo-label";
    // Show matched/total when a filter is active AND the group isn't empty —
    // otherwise "(0/0)" reads as noise. Unfiltered groups and empty groups
    // fall back to the plain "(N)" format.
    label.textContent =
      query === "" || all.length === 0
        ? `${KIND_LABELS[kind]} (${all.length})`
        : `${KIND_LABELS[kind]} (${matchedCount}/${all.length})`;
    // Wrap with help tooltip (#9) so a hover/focus on the kind label
    // explains what allow/deny/ask actually do.
    group.appendChild(attachHelpTooltip(label, lookupKindHelp(kind)));
    for (let i = 0; i < all.length; i++) {
      const rule = all[i];
      if (isFiltering && !matchesLoweredQuery(rule, lowerQuery)) continue;
      // Chip + optional badge wrap as a single flex item. Originally added
      // because `.combo-group` used `flex-wrap: wrap` (badges could orphan to
      // a new line without their chip); the group is now a vertical stack,
      // but the wrap still keeps badge + chip baseline-aligned and lets the
      // hover popover anchor relative to the pair.
      const chipWrap = document.createElement("span");
      chipWrap.className = "chip-wrap";
      const chip = document.createElement("code");
      chip.className = "chip";
      chip.textContent = rule;
      const originWrap = wrapWithOriginTooltip(chip, allOrigins[i] ?? []);
      chipWrap.appendChild(originWrap);
      const badge = lintBadge(rule);
      if (badge) chipWrap.appendChild(badge);
      attachContextMenu(chipWrap, () =>
        combinedChipContextMenuItems(rule, allOrigins[i] ?? [], props),
      );
      group.appendChild(chipWrap);
    }
    groupsWrap.appendChild(group);
  }
  panel.appendChild(groupsWrap);
  return panel;
}

/**
 * Returns a small warning element when the rule fails shape lint, or null
 * when the rule looks well-formed. The lint is intentionally lenient — we
 * can't match Claude Code's real parser precisely — so we flag rather than
 * block.
 *
 * The badge is a focusable button paired with an inline popover so the help
 * text surfaces consistently across platforms (the old `title=` tooltip was
 * at the mercy of each OS's implementation and easy to miss). The popover
 * opens on hover, focus, and click; click pins it, and Escape or an outside
 * click closes it.
 */
function lintBadge(rule: string): HTMLElement | null {
  const result = lintRule(rule);
  if (result.ok) return null;
  const reason = result.reason ?? "Rule shape not recognized.";

  const wrap = document.createElement("span");
  wrap.className = "lint-warn-wrap";

  const popoverId = `lint-popover-${++lintPopoverSeq}`;

  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "lint-warn";
  btn.textContent = "⚠";
  btn.setAttribute("aria-label", "Rule shape check");
  // `aria-describedby` is the canonical tooltip relationship. Keep the
  // detailed reason here only — `aria-label` is intentionally short so
  // screen readers don't announce the full message twice (once as the
  // accessible name, once as the description). No `aria-expanded` /
  // `aria-controls`: the popover also reveals on hover and focus, so a
  // disclosure-style expanded flag would go out of sync with the visible
  // state for keyboard users.
  btn.setAttribute("aria-describedby", popoverId);

  const pop = document.createElement("span");
  pop.id = popoverId;
  pop.className = "lint-warn-popover";
  pop.setAttribute("role", "tooltip");
  // Two-part body: reason on top, then a muted disclaimer that this is a
  // best-effort shape check rather than a verdict from Claude Code itself.
  // The popover is a known authority cue (yellow ⚠), so without the
  // disclaimer users can read it as canonical validation.
  const reasonLine = document.createElement("span");
  reasonLine.className = "lint-warn-popover-reason";
  reasonLine.textContent = reason;
  const note = document.createElement("span");
  note.className = "lint-warn-popover-note";
  note.textContent = "Best-effort shape check — Claude Code may still accept this rule.";
  pop.appendChild(reasonLine);
  pop.appendChild(note);

  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    if (openPinnedPopover === wrap) {
      closePinnedPopover();
    } else {
      pinPopover(wrap);
    }
  });

  wrap.appendChild(btn);
  wrap.appendChild(pop);
  return wrap;
}

/**
 * Wrap a rule chip so it carries a hover/focus tooltip listing the scope(s)
 * the rule originates from. Returns a positioned wrapper that should be
 * inserted in place of the bare chip; the chip itself is appended inside.
 *
 * `scopes` is expected in precedence order (highest first); for combined
 * rule rows that's all contributing scopes, for per-scope rule rows it's
 * just that one scope. The chip is given `tabindex=0` so keyboard users
 * can reveal the tooltip without a pointer (issue #82's a11y AC).
 */
function wrapWithOriginTooltip(chip: HTMLElement, scopes: Scope[]): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "rule-origin-wrap";
  wrap.appendChild(chip);
  if (scopes.length === 0) {
    // No origin data — render the wrap shell only so the layout stays
    // consistent. Skip the popover and tabindex/aria wiring entirely so
    // we don't introduce a focus stop with nothing behind it.
    return wrap;
  }
  chip.tabIndex = 0;
  const popoverId = `rule-origin-${++ruleOriginPopoverSeq}`;
  chip.setAttribute("aria-describedby", popoverId);

  const pop = document.createElement("span");
  pop.id = popoverId;
  pop.className = "rule-origin-popover";
  pop.setAttribute("role", "tooltip");

  const heading = document.createElement("span");
  heading.className = "rule-origin-popover-heading";
  heading.textContent = scopes.length === 1 ? "Defined in" : "Defined in (precedence order)";
  pop.appendChild(heading);

  const list = document.createElement("ul");
  list.className = "rule-origin-popover-list";
  for (const scope of scopes) {
    const li = document.createElement("li");
    li.textContent = SCOPE_LABELS[scope];
    list.appendChild(li);
  }
  pop.appendChild(list);
  wrap.appendChild(pop);
  return wrap;
}

let ruleOriginPopoverSeq = 0;

// Pinned-popover singleton. Hover/focus reveals are handled purely in CSS;
// this state only tracks popovers the user clicked to keep open. Shared
// across every popover kind in the UI (lint badges on rule chips, help
// tooltips on settings keys, …) so only one ever sits pinned at a time —
// opening a help tooltip auto-closes a pinned lint badge and vice versa.
let lintPopoverSeq = 0;
let openPinnedPopover: HTMLElement | null = null;
let popoverGlobalListenersAttached = false;

/**
 * Pin `wrap` as the currently-open popover, closing whatever else was
 * pinned. Registers the global click/Escape listeners on first use so
 * pinning is the only event that has to attach them.
 */
function pinPopover(wrap: HTMLElement): void {
  if (openPinnedPopover && openPinnedPopover !== wrap) closePinnedPopover();
  wrap.classList.add("is-open");
  openPinnedPopover = wrap;
  ensurePopoverGlobalListeners();
}

function closePinnedPopover(): void {
  if (!openPinnedPopover) return;
  // The wrap may already be detached (e.g. after a renderApp() rebuild);
  // touching classList is harmless but the state still needs nulling.
  if (openPinnedPopover.isConnected) openPinnedPopover.classList.remove("is-open");
  openPinnedPopover = null;
}

/**
 * Best-effort focus restore when a popover closes via Escape. Each popover
 * kind owns a different trigger element — lint warns on a `.lint-warn`
 * button, help tooltips on a `.help-info` button — so we probe a small
 * known set inside the wrap. Falls through silently when the wrap was
 * already torn down by a re-render between pin and Escape: focusing a
 * detached node is a no-op in some browsers and a stray scroll/focus
 * jump in others.
 */
function restorePopoverFocus(wrap: HTMLElement): void {
  const trigger =
    wrap.querySelector<HTMLButtonElement>(".lint-warn") ??
    wrap.querySelector<HTMLButtonElement>(".help-info");
  if (trigger?.isConnected) trigger.focus();
}

function ensurePopoverGlobalListeners(): void {
  if (popoverGlobalListenersAttached) return;
  popoverGlobalListenersAttached = true;
  document.addEventListener("click", (e) => {
    if (!openPinnedPopover) return;
    if (!openPinnedPopover.contains(e.target as Node)) closePinnedPopover();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || !openPinnedPopover) return;
    // Skip if another handler already consumed Escape (e.g. the search
    // input clears its value on Escape and calls preventDefault), so the
    // pinned popover doesn't close as a side-effect.
    if (e.defaultPrevented) return;
    // A visible modal owns Escape — otherwise closing a pinned popover
    // here would consume the keystroke and the dialog would stay open.
    if (document.querySelector(".modal-backdrop")) return;
    const wrap = openPinnedPopover;
    closePinnedPopover();
    restorePopoverFocus(wrap);
  });
}

// Tracks which tree-view branches the user has expanded. The set is keyed by
// "<scope>:<JSON-encoded path>" so state survives a full renderApp rebuild
// (the DOM is thrown away but this module-level set isn't). JSON encoding
// disambiguates string keys from numeric indices and tolerates dots in keys
// (e.g. env var names like "FOO.BAR"); a naive join would conflate them.
const openTreeNodes = new Set<string>();

// Project-scoped: clearing on pickProject avoids restoring stale expansions
// in a different project that happens to share scope+path strings.
let lastRenderedProjectDir: string | null = null;

// Tracks the project directory we last seeded with default-open permission
// branches. Distinct from `lastRenderedProjectDir` because the first render
// after a project pick is usually `busy: true` with no scopes yet; the seed
// has to wait for the second render where scopes are available.
let lastSeededProjectDir: string | null = null;

/**
 * One-shot seed of `openTreeNodes` for the permission branches we want to
 * default-open on a fresh project load: `permissions` itself plus each
 * non-empty `permissions.<kind>` array. Mirrors the old single-pane
 * visibility from before the unified-tree migration so the user lands on
 * the same allow / deny / ask content without clicking. After this, the
 * `<details>.toggle` listener in `treeBranch` is the only writer of
 * `openTreeNodes`, so a manual collapse persists across re-renders.
 */
function seedDefaultOpenPermissions(loaded: LoadedScopes, threshold: number): void {
  for (const view of loaded.scopes) {
    const perms = view.values.permissions;
    if (!perms || typeof perms !== "object" || Array.isArray(perms)) continue;
    openTreeNodes.add(treeKey(view.scope, ["permissions"]));
    const permsObj = perms as { [k: string]: JsonValue };
    for (const kind of PERMISSION_KINDS) {
      const arr = permsObj[kind];
      // Skip empty arrays — opening a branch that has nothing under it
      // is just visual noise on a fresh project load. The user can still
      // open it manually if they want to add rules.
      if (Array.isArray(arr) && arr.length > 0) {
        const kindPath: PathSeg[] = ["permissions", kind];
        openTreeNodes.add(treeKey(view.scope, kindPath));
        // Seed open state for tool groups (#68) so the synthetic
        // `<details>` nodes inside an already-open permissions branch
        // start expanded — users opened the kind branch to read its
        // rules, so collapsed groups would just hide them again. The
        // grouping has to use the user's current threshold (#115) so
        // we don't seed open state for groups the renderer won't form.
        const ruleStrings = arr.map((v) => (typeof v === "string" ? v : ""));
        for (const entry of groupByToolPrefix(ruleStrings, threshold)) {
          if (entry.kind === "group") {
            openTreeNodes.add(treeKey(view.scope, [...kindPath, "__group__", entry.tool]));
          }
        }
      }
    }
  }
}

// Cross-pane rule-highlight state (#49). One rule string at a time;
// clicking the same chip toggles off, clicking a different chip swaps,
// clicking outside any chip clears. Survives normal re-renders so a
// watcher reload doesn't drop the highlight, but auto-clears when the
// highlighted rule no longer appears anywhere on screen (e.g. after a
// successful move that took the last copy off the grid).
//
// Match policy is exact string equality — no glob subsumption. That's
// #17's job; the highlight is deliberately predictable.
let highlightedRule: string | null = null;
let highlightDelegationInstalled = false;

/**
 * Walk every per-scope rule chip and combined-panel chip on the page
 * and add `.rule-highlight` to those whose textContent matches the
 * current highlight. Called from `renderApp` after the tree is built,
 * and from `setHighlightedRule` when the toggle fires without a full
 * re-render. Idempotent — running twice in a row is a no-op.
 *
 * Self-healing: if `highlightedRule` is set but no chip matches it,
 * the singleton is cleared. That covers the "moved the last copy"
 * case without the move flow needing to know about highlights.
 */
function applyRuleHighlight(): void {
  for (const el of document.querySelectorAll<HTMLElement>(".rule-highlight")) {
    el.classList.remove("rule-highlight");
  }
  if (highlightedRule === null) return;
  let matched = false;
  for (const el of document.querySelectorAll<HTMLElement>(".rule-text, .chip")) {
    if (el.textContent === highlightedRule) {
      el.classList.add("rule-highlight");
      matched = true;
    }
  }
  if (!matched) highlightedRule = null;
}

/**
 * Set or toggle the highlighted rule. Passing the same string twice
 * clears (the issue spec's "second click toggles off" behavior).
 * Passing `null` clears unconditionally — the click-outside and
 * Escape paths use that form.
 */
function setHighlightedRule(rule: string | null): void {
  if (rule !== null && highlightedRule === rule) {
    highlightedRule = null;
  } else {
    highlightedRule = rule;
  }
  applyRuleHighlight();
}

function clearRuleHighlight(): void {
  if (highlightedRule === null) return;
  highlightedRule = null;
  applyRuleHighlight();
}

/**
 * Wire document-level click + keydown listeners that drive the
 * cross-pane highlight. Delegation rather than per-chip handlers so
 * the wiring survives `renderApp`'s `innerHTML = ""` rebuild — listeners
 * stay attached, state survives, render-time `applyRuleHighlight`
 * re-applies the class to whichever chips currently exist.
 *
 * Idempotent at module level: a flag prevents double-registration
 * across reload-style renderApp calls.
 */
function ensureRuleHighlightDelegation(): void {
  if (highlightDelegationInstalled) return;
  highlightDelegationInstalled = true;

  document.addEventListener("click", (e) => {
    const target = e.target as HTMLElement | null;
    if (!target) return;
    const chip = target.closest<HTMLElement>(".rule-text, .chip");
    if (chip) {
      setHighlightedRule(chip.textContent);
      return;
    }
    // Clicks on the chip's neighborhood (lint badge `.lint-info`, the
    // chip-wrap shell, the rule row) shouldn't clear — the user is
    // still interacting with the highlighted rule's UI. Only truly
    // off-rule clicks dismiss.
    if (target.closest(".chip-wrap, .rule")) return;
    clearRuleHighlight();
  });

  document.addEventListener("keydown", (e) => {
    // Defer to anything that already claimed the keystroke — modals
    // (Escape closes), context menus (Escape pops one level), etc.
    if (e.defaultPrevented) return;
    if (e.key === "Escape") {
      if (highlightedRule !== null) clearRuleHighlight();
      return;
    }
    if (e.key === "h" || e.key === "H") {
      const active = document.activeElement as HTMLElement | null;
      if (!active) return;
      // Skip while a text input has focus — `h` is a letter, the user
      // is typing.
      if (active.tagName === "INPUT" || active.tagName === "TEXTAREA") return;
      if (active.isContentEditable) return;
      const chip = active.closest<HTMLElement>(".rule-text, .chip");
      if (!chip) return;
      e.preventDefault();
      setHighlightedRule(chip.textContent);
    }
  });
}

// HTML5 drag-and-drop source state. Set by `dragstart` on a movable tree
// node, cleared by `dragend` (or by renderApp on a re-render that tears
// down the source mid-drag). Lives at module scope because `dragover` on
// drop targets needs to read the source without a closure over the source
// element, and the lifecycle is one short user gesture — same shape as
// `openTreeNodes` / `lastRenderedProjectDir`.
//
// `path` mirrors the Rust `Vec<PathSeg>` wire shape; `el` is the source
// node, kept so the move flow can restore focus to it after a confirm
// modal dismiss.
interface DragSource {
  path: PathSeg[];
  from: Scope;
  el: HTMLElement;
}
let dragSource: DragSource | null = null;
// Distinguishes a keyboard pickup (#41) from a mouse drag using the same
// `dragSource` payload. Drives screen-reader announcements (mouse hover
// doesn't need narration; keyboard target focus does) and the
// `.col-drop-available` highlight that previews valid targets — mouse
// drags only highlight the *active* hover, not every available target.
let keyboardActive = false;
// Label captured at pickup so the announcer can name the rule when it
// announces target focus, even after the source element is unmounted
// mid-drop. Read from the source's textContent at pickup time.
let keyboardPickupLabel = "";

function clearDragState(): void {
  if (dragSource?.el) {
    dragSource.el.removeAttribute("aria-pressed");
  }
  dragSource = null;
  keyboardActive = false;
  keyboardPickupLabel = "";
  // Belt-and-suspenders: a drop on a non-target column doesn't fire its
  // own dragleave, so a stale `.col-drop-active` could survive into the
  // next render. Sweep them all here on any drag-state reset. Same for
  // `.col-drop-available` (keyboard-pickup target hints) and the
  // tabindex we added to columns so they could receive focus during
  // pickup — the column shouldn't stay in the tab order once the
  // pickup ends.
  for (const el of document.querySelectorAll<HTMLElement>(".col-drop-active")) {
    el.classList.remove("col-drop-active");
  }
  for (const el of document.querySelectorAll<HTMLElement>(".col-drop-available")) {
    el.classList.remove("col-drop-available");
    el.removeAttribute("tabindex");
  }
}

/**
 * Lazy aria-live region for the keyboard DnD flow (#41). Used to
 * announce pickup, target focus, and cancel — actual move outcomes are
 * surfaced by the existing scope-reload UI, so the live region stays
 * focused on the transient gestures the visual UI doesn't otherwise
 * narrate.
 *
 * Idempotent: subsequent calls return the existing node. Sits as a
 * body-level sibling of the app root so it survives `renderApp`'s
 * `innerHTML = ""` rebuild (which only clears the app container).
 * Visually hidden via the `.sr-only` class.
 */
function announcerEl(): HTMLElement {
  let el = document.getElementById("a11y-announcer");
  if (el) return el;
  el = document.createElement("div");
  el.id = "a11y-announcer";
  el.className = "sr-only";
  el.setAttribute("role", "status");
  el.setAttribute("aria-live", "polite");
  el.setAttribute("aria-atomic", "true");
  document.body.appendChild(el);
  return el;
}

/**
 * Push a message into the polite live region. Clearing the textContent
 * before setting the new value forces screen readers to re-announce
 * even when the new message is identical to the last one — without
 * that, the second pickup of the same rule would be silent.
 */
function announce(message: string): void {
  const el = announcerEl();
  el.textContent = "";
  // Microtask so the clear-then-set is observable to AT instead of
  // collapsing into a single set. `requestAnimationFrame` would also
  // work; setTimeout(0) is the smallest hammer that's portable.
  setTimeout(() => {
    el.textContent = message;
  }, 0);
}

function setupLeafDragSource(el: HTMLElement, scope: Scope, path: PathSeg[]): void {
  el.draggable = true;
  el.addEventListener("dragstart", (e) => {
    dragSource = { path, from: scope, el };
    keyboardActive = false;
    if (e.dataTransfer) {
      // Custom MIME type used in dragover to reject foreign drags from other
      // apps or browser tabs before checking dragSource. The payload lives in
      // dragSource; the MIME value is just a discriminator.
      e.dataTransfer.setData("application/x-claude-scope-move", "leaf");
      e.dataTransfer.effectAllowed = "move";
    }
  });
  el.addEventListener("dragend", clearDragState);

  // Keyboard pickup (#41). Sources `setupLeafDragSource` runs on are
  // movable by construction — same set the mouse-DnD pipeline accepts —
  // so the keyboard surface mirrors the mouse surface without a separate
  // affordance map. `tabIndex` may already be set by
  // `wrapWithOriginTooltip` on rule chips; setting it again is idempotent.
  if (el.tabIndex < 0) el.tabIndex = 0;
  el.addEventListener("keydown", (e) => {
    if (e.key !== " " && e.key !== "Enter") return;
    // Allow Enter to activate the lint badge / help info button when the
    // user has tabbed into one of those — they're inside the row but
    // shouldn't pick up the rule. Only initiate pickup when the event
    // target is the source element itself.
    if (e.target !== el) return;
    e.preventDefault();
    e.stopPropagation();
    beginKeyboardPickup(el, scope, path);
  });
}

/**
 * Pick up a rule via keyboard (#41). Mirrors `dragstart` but doesn't
 * route through `DataTransfer` — there's no native equivalent for a
 * non-pointer drag and we don't need one. Sets the same `dragSource`
 * the mouse pipeline uses so the drop path in `column()` accepts both
 * mouse and keyboard pickups identically, with the same `skipConfirm:
 * true` behavior #70 chose.
 *
 * Refuses pickup when another pickup is already active (the active one
 * has to be cancelled with Escape first) — keyboard pickup is supposed
 * to be deliberate, and silently swapping sources would undermine that.
 */
function beginKeyboardPickup(el: HTMLElement, scope: Scope, path: PathSeg[]): void {
  if (dragSource) return;
  dragSource = { path, from: scope, el };
  keyboardActive = true;
  keyboardPickupLabel = (el.textContent ?? "").trim() || "this item";
  el.setAttribute("aria-pressed", "true");
  highlightAvailableTargets(scope);
  const firstTarget = document.querySelector<HTMLElement>(".col-drop-available");
  if (firstTarget) {
    firstTarget.focus();
    announce(
      `Picked up ${keyboardPickupLabel} from ${SCOPE_LABELS[scope]}. ` +
        `Use Left and Right arrow keys to choose a scope, Enter to drop, Escape to cancel.`,
    );
  } else {
    // No valid targets (other scopes all hidden / busy / same scope) —
    // back out without leaving the source in a half-picked-up state.
    dragSource = null;
    keyboardActive = false;
    keyboardPickupLabel = "";
    el.removeAttribute("aria-pressed");
    announce("No other scope is available as a drop target.");
  }
}

/**
 * Mark every scope column other than `fromScope` as a candidate target,
 * giving each one a `tabindex` so arrow nav and focus-on-Tab work. The
 * existing `.col-drop-active` class is reserved for the *current* hover
 * (mouse) or focus (keyboard); `.col-drop-available` is the "ambient"
 * highlight that previews every valid target during keyboard pickup so
 * the user can see where they can land before they navigate.
 */
function highlightAvailableTargets(fromScope: Scope): void {
  for (const col of document.querySelectorAll<HTMLElement>(".col")) {
    const scope = col.dataset.scope as Scope | undefined;
    if (!scope || scope === fromScope) continue;
    col.classList.add("col-drop-available");
    col.tabIndex = 0;
  }
}

function treeKey(scope: Scope, path: PathSeg[]): string {
  return `${scope}:${JSON.stringify(path)}`;
}

function treeNode(
  scope: Scope,
  path: PathSeg[],
  label: string,
  value: JsonValue,
  props?: AppProps,
  lowerQuery = "",
): HTMLElement {
  if (value !== null && typeof value === "object") {
    return treeBranch(scope, path, label, value, props, lowerQuery);
  }
  return treeLeaf(scope, path, label, value, props, lowerQuery);
}

function treeBranch(
  scope: Scope,
  path: PathSeg[],
  label: string,
  value: JsonValue[] | { [key: string]: JsonValue },
  props: AppProps | undefined,
  lowerQuery = "",
): HTMLElement {
  const details = document.createElement("details");
  details.className = "tree-node tree-branch";
  // Permission lists wear the allow/deny/ask color class on both the branch
  // summary and the children, so the leaves don't need to redo it
  // individually. Path-driven so the rule of "permissions.<kind>...
  // inherits the kind class" lives in one place.
  const permKind = permissionKindForPath(path);
  if (permKind) details.classList.add(`tree-perm-${permKind}`);
  const key = treeKey(scope, path);

  // Permission-list branches with any non-string entries can't be moved
  // — the backend's `merge_at_path` rejects them so the renderer + combined
  // panel + count helpers stay consistent (only string entries count as
  // rules). Suppress the affordance here so the action isn't offered before
  // the IPC says no. The check fires only at the `permissions.<kind>` depth
  // (length 2) where the value is the rule array itself.
  const isMalformedPermissionListBranch =
    permKind !== null &&
    path.length === 2 &&
    Array.isArray(value) &&
    value.some((item) => typeof item !== "string");
  const offerMoveAffordance = isMovablePath(path) && !isMalformedPermissionListBranch;

  const summary = document.createElement("summary");
  summary.className = "tree-summary";
  const name = document.createElement("span");
  name.className = "tree-key";
  name.textContent = label;
  // Make the key span (not the whole <summary>) the drag source for movable
  // branches: starting a drag on the inner span lets the browser suppress
  // the `<details>` toggle that would otherwise fire on click, and isolates
  // the affordance from nested key labels which aren't movable.
  if (props && !props.busy && offerMoveAffordance) {
    setupLeafDragSource(name, scope, path);
  }
  // Help tooltip (#9) on recognized top-level keys and on permission-kind
  // branches. Wrap before append so the popover positioning anchor lives
  // alongside the label rather than getting absorbed by `<summary>`'s
  // flexbox math.
  const branchHelp = lookupBranchHelp(path);
  if (branchHelp) {
    summary.appendChild(attachHelpTooltip(name, branchHelp));
  } else {
    summary.appendChild(name);
  }
  const peek = document.createElement("span");
  peek.className = "tree-peek";
  peek.textContent = treeBranchPeek(path, value, lowerQuery);
  summary.appendChild(peek);
  if (props && offerMoveAffordance) {
    summary.appendChild(leafMoveButtons(scope, path, props));
    attachContextMenu(summary, () => leafContextMenuItems(scope, path, value, props));
  }
  details.appendChild(summary);

  const children = document.createElement("div");
  children.className = "tree-children";
  details.appendChild(children);

  // Lazy-render children: defer DOM construction until the branch is first
  // opened. Keeps initial render cheap for large `env` / `hooks` payloads —
  // a deeply nested object that's collapsed contributes only the summary
  // row to the DOM, not its full subtree.
  let populated = false;
  function populate(): void {
    if (populated) return;
    populated = true;
    if (Array.isArray(value)) {
      // Permission-list arrays (`permissions.<kind>`) collapse rules that
      // share a `Tool(...)` prefix under synthetic group nodes (#68). Non-
      // permission arrays fall through to the unchanged flat iteration so
      // `env` / `hooks` / arbitrary user keys keep today's shape.
      const isPermissionList = permKind !== null && path.length === 2;
      if (isPermissionList) {
        populatePermissionList(scope, path, value, children, props, lowerQuery);
      } else {
        value.forEach((child, i) => {
          children.appendChild(treeNode(scope, [...path, i], `[${i}]`, child, props, lowerQuery));
        });
      }
    } else {
      for (const [k, v] of Object.entries(value)) {
        children.appendChild(treeNode(scope, [...path, k], k, v, props, lowerQuery));
      }
    }
  }

  // `openTreeNodes` is the single source of truth for which branches are
  // open. `seedDefaultOpenPermissions` (in `renderApp`) pre-populates it
  // with `permissions` and each non-empty `permissions.<kind>` on the
  // first render per project, so the unified tree starts in the same
  // shape as the old single-pane view; subsequent renders defer to
  // whatever the user toggled.
  if (openTreeNodes.has(key)) {
    details.open = true;
    populate();
  }
  details.addEventListener("toggle", () => {
    if (details.open) {
      openTreeNodes.add(key);
      populate();
    } else {
      openTreeNodes.delete(key);
    }
  });

  return details;
}

/**
 * Populate a `permissions.<kind>` branch's children using the tool-prefix
 * grouping helper (#68). Each `Single` from the grouper renders exactly
 * like the pre-#68 flat branch did — `treeNode` with the original index in
 * the path. Each `Group` renders as a synthetic `<details>` whose
 * children are the group's members at their original indices, so move /
 * drag / context menu wiring stays leaf-local.
 */
function populatePermissionList(
  scope: Scope,
  path: PathSeg[],
  value: JsonValue[],
  children: HTMLElement,
  props: AppProps | undefined,
  lowerQuery: string,
): void {
  // Only string-typed entries participate in tool-prefix grouping — a hand-
  // edited settings.json can put e.g. an object in `permissions.allow[2]`,
  // and grouping `null` or a struct under a fake tool name would be more
  // surprise than help. Keep the malformed entries inline at their
  // original index so the existing leaf renderer can flag them.
  const ruleStrings: string[] = value.map((v) => (typeof v === "string" ? v : ""));
  // `props` can be undefined for previews / tests that render the
  // tree without a full AppProps; in that case fall back to the
  // default threshold so the tree still groups the same way #68
  // shipped with.
  const threshold = props ? resolveGroupThreshold(props.preferences) : TOOL_GROUP_THRESHOLD;
  const grouped = groupByToolPrefix(ruleStrings, threshold);
  for (const entry of grouped) {
    if (entry.kind === "single") {
      children.appendChild(
        treeNode(
          scope,
          [...path, entry.index],
          `[${entry.index}]`,
          value[entry.index],
          props,
          lowerQuery,
        ),
      );
    } else {
      children.appendChild(toolGroupNode(scope, path, entry, value, props, lowerQuery));
    }
  }
}

/**
 * Render a synthetic `<details>` for a tool group (#68). The group has no
 * backend identity — its members each retain their original
 * `permissions.<kind>[i]` path — but its open/close state is tracked via
 * an `openTreeNodes` key that uses a `"__group__"` sentinel segment so it
 * can't collide with a real key path (permission lists are arrays, never
 * objects with a `__group__` key).
 *
 * Filter behavior: when a query is active, hide the group entirely if no
 * member matches, otherwise show it with the non-matching members
 * filtered out. The summary's count badge mirrors the combined panel's
 * `(matched/total)` format so the matched-count signal is consistent
 * across the two surfaces.
 */
function toolGroupNode(
  scope: Scope,
  path: PathSeg[],
  group: Extract<GroupedRuleEntry, { kind: "group" }>,
  rawValue: JsonValue[],
  props: AppProps | undefined,
  lowerQuery: string,
): HTMLElement {
  const permKind = permissionKindForPath(path);
  const matchedMembers =
    lowerQuery === ""
      ? group.members
      : group.members.filter((m) => matchesLoweredQuery(m.rule, lowerQuery));
  // If nothing in the group matches the active filter, return an empty
  // hidden node so the children container doesn't grow a "ghost" group
  // summary with no visible members beneath it.
  if (lowerQuery !== "" && matchedMembers.length === 0) {
    const skip = document.createElement("span");
    skip.hidden = true;
    return skip;
  }

  const groupPath: PathSeg[] = [...path, "__group__", group.tool];
  const key = treeKey(scope, groupPath);

  const details = document.createElement("details");
  details.className = "tree-node tree-branch tree-tool-group";
  if (permKind) details.classList.add(`tree-perm-${permKind}`);

  const summary = document.createElement("summary");
  summary.className = "tree-summary";
  const name = document.createElement("span");
  name.className = "tree-key";
  name.textContent = group.tool;
  summary.appendChild(name);
  const peek = document.createElement("span");
  peek.className = "tree-peek";
  peek.textContent =
    lowerQuery === ""
      ? `(${group.members.length})`
      : `(${matchedMembers.length}/${group.members.length})`;
  summary.appendChild(peek);
  details.appendChild(summary);

  const childrenWrap = document.createElement("div");
  childrenWrap.className = "tree-children";
  details.appendChild(childrenWrap);

  let populated = false;
  function populate(): void {
    if (populated) return;
    populated = true;
    for (const member of matchedMembers) {
      childrenWrap.appendChild(
        treeNode(
          scope,
          [...path, member.index],
          `[${member.index}]`,
          rawValue[member.index],
          props,
          lowerQuery,
        ),
      );
    }
  }

  // Default open: tool groups inside an already-open `permissions.<kind>`
  // branch should reveal their rules without an extra click — users opened
  // the parent to read the rules. `seedDefaultOpenPermissions` seeds the
  // synthetic key for each group at first render; manual collapses then
  // override.
  if (openTreeNodes.has(key)) {
    details.open = true;
    populate();
  }
  details.addEventListener("toggle", () => {
    if (details.open) {
      openTreeNodes.add(key);
      populate();
    } else {
      openTreeNodes.delete(key);
    }
  });

  return details;
}

function treeLeaf(
  scope: Scope,
  path: PathSeg[],
  label: string,
  value: JsonValue,
  props?: AppProps,
  lowerQuery = "",
): HTMLElement {
  const permKind = permissionKindForPath(path);
  const isPermissionRule = permKind !== null && path.length === 3 && typeof value === "string";

  // Permission rule leaves under `permissions.<kind>` get the rule-chip
  // styling, lint badge, origin tooltip, drag handle, and per-target move
  // buttons that the old `ruleRow` used to render. Filter the row out
  // entirely when the search query is active and doesn't match — that drop
  // is what `treeBranchPeek` then surfaces as the `m/N` count on the
  // parent branch summary.
  if (isPermissionRule && permKind) {
    const rule = value as string;
    if (lowerQuery !== "" && !matchesLoweredQuery(rule, lowerQuery)) {
      const skip = document.createElement("span");
      skip.hidden = true;
      return skip;
    }
    const row = document.createElement("div");
    row.className = `tree-node tree-leaf rule rule-${permKind}`;
    const code = document.createElement("code");
    code.className = "rule-text";
    code.textContent = rule;
    if (props && !props.busy) {
      setupLeafDragSource(code, scope, path);
    }
    // Per-scope rule rows share the same tooltip helper as the combined
    // panel for consistency (issue #82). Origins is just `[scope]` here
    // since the rule lives in exactly this scope's file.
    row.appendChild(wrapWithOriginTooltip(code, [scope]));
    const badge = lintBadge(rule);
    if (badge) row.appendChild(badge);
    if (props) {
      // Pass the rule string as the move-button label so the screen
      // reader announces "Move Bash(git status) from …" instead of
      // the meaningless path "permissions.allow[0]".
      row.appendChild(leafMoveButtons(scope, path, props, rule));
      attachContextMenu(row, () => leafContextMenuItems(scope, path, value, props));
    }
    return row;
  }

  // Permission paths can be malformed in hand-edited settings.json: a
  // whole list at `permissions.<kind>` (length 2) might be a string or
  // object instead of an array, and a single rule slot (length 3) might
  // be a number / null instead of a string. The backend's `merge_at_path`
  // rejects both of those at the IPC, so suppress the affordances here
  // rather than offer an action that's guaranteed to fail.
  //
  // Reaching `treeLeaf` for a length-2 permissions path implies the value
  // is non-array (otherwise `treeNode` would have routed to `treeBranch`),
  // so any length-2 permissions leaf is by construction malformed.
  const isMalformedPermissionList = permKind !== null && path.length === 2;
  const isMalformedPermissionRule =
    permKind !== null && path.length === 3 && typeof value !== "string";
  const offerMoveAffordance =
    !isMalformedPermissionList && !isMalformedPermissionRule && isMovablePath(path);

  const row = document.createElement("div");
  row.className = "tree-node tree-leaf";
  const name = document.createElement("span");
  name.className = "tree-key";
  name.textContent = label;
  if (props && !props.busy && offerMoveAffordance) {
    setupLeafDragSource(name, scope, path);
  }
  row.appendChild(name);
  const val = document.createElement("span");
  val.className = `tree-value tree-value-${leafType(value)}`;
  val.textContent = formatLeaf(value);
  row.appendChild(val);
  if (props && offerMoveAffordance) {
    row.appendChild(leafMoveButtons(scope, path, props));
    attachContextMenu(row, () => leafContextMenuItems(scope, path, value, props));
  }
  return row;
}

function leafMoveButtons(
  scope: Scope,
  path: PathSeg[],
  props: AppProps,
  // Optional override for the screen-reader label. Permission rule rows
  // pass the rule string so the button announces something meaningful;
  // top-level key and rule-list rows fall back to the path's
  // dotted-bracket form, which is informative at that level.
  ariaSubject?: string,
): HTMLElement {
  const moveBtns = document.createElement("div");
  moveBtns.className = "rule-moves tree-key-moves";
  const subject = ariaSubject ?? describePath(path);
  for (const target of SCOPES) {
    if (target === scope) continue;
    // Mirror scopeGrid: don't offer moves into columns the user hid — the
    // result would land in a column they can't see without re-enabling it.
    if (!isScopeVisible(target)) continue;
    const btn = document.createElement("button");
    btn.className = "move-btn";
    btn.type = "button";
    btn.textContent = `→ ${SCOPE_LABELS[target]}`;
    btn.setAttribute(
      "aria-label",
      `Move ${subject} from ${SCOPE_LABELS[scope]} to ${SCOPE_LABELS[target]}`,
    );
    btn.disabled = props.busy;
    btn.addEventListener("click", (e) => {
      // Clicks on the summary element would otherwise toggle the <details>;
      // the move action is a distinct intent, so swallow propagation.
      e.preventDefault();
      e.stopPropagation();
      props.onMoveLeaf({ path, from: scope, to: target }, e.currentTarget as HTMLElement);
    });
    moveBtns.appendChild(btn);
  }
  return moveBtns;
}

/**
 * Render a path slice as a human-readable label for ARIA + error messages.
 * Mirrors Rust's `describe_path`: `permissions.allow[2]` for a rule, `env`
 * for a top-level key, `permissions.allow` for a whole rule list.
 */
function describePath(path: PathSeg[]): string {
  let out = "";
  for (const seg of path) {
    if (typeof seg === "number") {
      out += `[${seg}]`;
    } else {
      out += out === "" ? seg : `.${seg}`;
    }
  }
  return out || "(root)";
}

/**
 * Compact summary shown in the branch row's right gutter. For most
 * branches it's the unfiltered shape — `[N]` for arrays, `{N}` for
 * objects. For a `permissions.<kind>` array under an active filter it
 * becomes `[m/N]` so the user can see how many rules in this list match,
 * mirroring the old rule-group `(m/n)` indicator that lived above each
 * kind section before the unified-tree migration.
 */
function treeBranchPeek(
  path: PathSeg[],
  value: JsonValue[] | { [key: string]: JsonValue },
  lowerQuery: string,
): string {
  if (Array.isArray(value)) {
    if (
      lowerQuery !== "" &&
      path.length === 2 &&
      path[0] === "permissions" &&
      typeof path[1] === "string" &&
      PERMISSION_KINDS.includes(path[1] as PermissionKind)
    ) {
      // Both the numerator and denominator count only string entries —
      // matches `countPermissionRules` / `countMatchingRules` /
      // `extractPermissionList`, which all treat non-string array items
      // (possible in hand-edited JSON) as non-rules. Mixing the two
      // would surface an `m/N` where N over-counts items that the rest
      // of the UI hides.
      let total = 0;
      let matched = 0;
      for (const item of value) {
        if (typeof item !== "string") continue;
        total++;
        if (matchesLoweredQuery(item, lowerQuery)) matched++;
      }
      // Fall back to plain `[N]` when there are no string entries to
      // match — `[0/0]` next to a non-empty array of malformed items
      // would suggest the branch is empty when expanding it would still
      // show those entries.
      if (total === 0) return `[${value.length}]`;
      return `[${matched}/${total}]`;
    }
    return `[${value.length}]`;
  }
  return `{${Object.keys(value).length}}`;
}

function leafType(value: JsonValue): string {
  if (value === null) return "null";
  if (typeof value === "boolean") return "bool";
  if (typeof value === "number") return "num";
  return "str";
}

function formatLeaf(value: JsonValue): string {
  if (value === null) return "null";
  if (typeof value === "string") return JSON.stringify(value);
  return String(value);
}

function scopeGrid(props: AppProps, lowerQuery: string): HTMLElement {
  const grid = document.createElement("section");
  grid.className = "grid";
  // `renderApp` already bailed when props.scopes is null, so this is
  // effectively an assert — but narrow locally rather than lean on `!` so
  // a future caller can't crash at runtime.
  const loaded = props.scopes;
  if (!loaded) return grid;
  for (const scope of SCOPES) {
    if (!isScopeVisible(scope)) continue;
    const view = loaded.scopes.find((s) => s.scope === scope);
    if (!view) continue;
    grid.appendChild(scopeColumn(view, props, lowerQuery));
  }
  return grid;
}

function scopeColumn(view: ScopeView, props: AppProps, lowerQuery: string): HTMLElement {
  const col = document.createElement("div");
  col.className = "col";
  // Tag the scope on the DOM node so the keyboard-pickup pipeline (#41)
  // can pick out which columns are valid targets without re-walking the
  // ScopeView list it doesn't have a handle to.
  col.dataset.scope = view.scope;
  // Column-level context menu (#8 paste). Leaf and chip handlers
  // stopPropagation on contextmenu, so this only fires when the user
  // right-clicks on the column chrome itself (header, status row, empty
  // area below the tree).
  attachContextMenu(col, () => scopeColumnContextMenuItems(view.scope, props));

  // Drop target wiring. Only highlight + accept when the active drag came
  // from a different scope — same-scope drops are intra-scope reorders,
  // explicitly out of scope here (#43). Hidden scopes don't render this
  // column at all (scopeGrid filters them), so no extra `isScopeVisible`
  // check is needed here.
  col.addEventListener("dragover", (e) => {
    // Normalize `DataTransfer.types` before sniffing the marker. The current
    // spec says it's a frozen `Array<string>` (so `.includes` works), but
    // older WebKit exposed it as a `DOMStringList` with `.contains` instead.
    // Tauri 2 supports macOS 10.15+ where that older shape can still surface,
    // so be defensive: `Array.from` accepts either and gives us `.includes`.
    const types = e.dataTransfer ? Array.from(e.dataTransfer.types) : [];
    if (!types.includes("application/x-claude-scope-move")) return;
    if (!dragSource || dragSource.from === view.scope || props.busy) return;
    // Calling preventDefault is what makes a target "droppable" in HTML5 DnD.
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    col.classList.add("col-drop-active");
  });
  col.addEventListener("dragleave", (e) => {
    // dragleave fires on every transition into a child element, so checking
    // relatedTarget is the only way to distinguish "actually left the
    // column" from "moved between two children of the column". Without
    // this guard the highlight flickers on every chip the cursor crosses.
    const next = e.relatedTarget as Node | null;
    if (next && col.contains(next)) return;
    col.classList.remove("col-drop-active");
  });
  col.addEventListener("drop", (e) => {
    e.preventDefault();
    col.classList.remove("col-drop-active");
    if (!dragSource || dragSource.from === view.scope || props.busy) return;
    const src = dragSource;
    // Clear the singleton before dispatching the move so a stale
    // `dragSource` doesn't linger past the dispatch.
    dragSource = null;
    const trigger = src.el.isConnected ? src.el : undefined;
    // Drop on a target column expresses intent unambiguously, so skip the
    // diff/confirm modal (#70). Click-to-move keeps the modal as the
    // safer default for the less-explicit click gesture. The
    // per-destination `.bak` is the recovery path until an audit log /
    // undo lands (#19).
    const opts: MoveOptions = { skipConfirm: true };
    props.onMoveLeaf({ path: src.path, from: src.from, to: view.scope }, trigger, opts);
  });

  // Keyboard drop / nav (#41). The column only takes keystrokes when a
  // keyboard pickup is active; if a mouse user happens to tab onto a
  // column with `.col-drop-available` set (they shouldn't be able to —
  // we only add `tabindex` during pickup — but defensive), the column
  // is otherwise inert.
  col.addEventListener("keydown", (e) => {
    if (!keyboardActive || !dragSource) return;
    if (e.key === "Escape") {
      e.preventDefault();
      const src = dragSource;
      const restoreTarget = src.el.isConnected ? src.el : null;
      clearDragState();
      announce("Move cancelled.");
      restoreTarget?.focus();
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      // Don't fire on the source's own column — it can't be a target
      // (same-scope drops are explicitly out of scope per #43). The
      // available-target highlighting filters those out, but tabbing
      // past the visible cue is still possible.
      if (dragSource.from === view.scope || props.busy) return;
      e.preventDefault();
      const src = dragSource;
      const trigger = src.el.isConnected ? src.el : undefined;
      clearDragState();
      props.onMoveLeaf({ path: src.path, from: src.from, to: view.scope }, trigger, {
        skipConfirm: true,
      });
      return;
    }
    if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      e.preventDefault();
      const targets = Array.from(document.querySelectorAll<HTMLElement>(".col-drop-available"));
      if (targets.length === 0) return;
      const idx = targets.indexOf(col);
      const step = e.key === "ArrowRight" ? 1 : -1;
      const next = targets[(idx + step + targets.length) % targets.length];
      next.focus();
    }
  });

  // Announce when focus lands on a valid target column during keyboard
  // pickup. Fires for both initial focus (set programmatically in
  // `beginKeyboardPickup`) and arrow nav — `focus` is the natural
  // intersection of both paths.
  col.addEventListener("focus", () => {
    if (!keyboardActive || !dragSource) return;
    if (dragSource.from === view.scope) return;
    col.classList.add("col-drop-active");
    announce(`${SCOPE_LABELS[view.scope]} scope. Press Enter to drop, Escape to cancel.`);
  });
  col.addEventListener("blur", () => {
    col.classList.remove("col-drop-active");
  });

  const head = document.createElement("div");
  head.className = "col-head";
  const h = document.createElement("h3");
  // Nest the label in a span so the help-tooltip wrap (inline) doesn't
  // sit directly inside the h3 — the span becomes the popover anchor,
  // the h3 stays semantically the column heading.
  const headLabel = document.createElement("span");
  headLabel.className = "col-head-label";
  headLabel.textContent = SCOPE_LABELS[view.scope];
  h.appendChild(attachHelpTooltip(headLabel, lookupScopeHelp(view.scope)));
  head.appendChild(h);

  const pathEl = document.createElement("div");
  pathEl.className = "col-path";
  pathEl.textContent = view.path ?? "(no path)";
  head.appendChild(pathEl);

  const status = document.createElement("div");
  status.className = "col-status";
  if (view.parse_error) {
    status.textContent = `Parse error: ${view.parse_error}`;
    status.classList.add("err");
  } else if (!view.exists) {
    status.textContent = "(file not present)";
    status.classList.add("muted");
  } else {
    const totals = countPermissionRules(view.values);
    status.textContent = `${totals} permission rule${totals === 1 ? "" : "s"}`;
  }
  head.appendChild(status);
  col.appendChild(head);

  // Render every top-level key — `permissions` included — through the
  // shared tree walker. Permissions render with leaf-level move buttons
  // and allow/deny/ask styling thanks to `permissionKindForPath` /
  // `treeLeaf`'s rule-leaf branch; other keys keep today's whole-key move
  // behavior.
  const keys = Object.keys(view.values);
  if (keys.length === 0) {
    if (view.exists && !view.parse_error) {
      const empty = document.createElement("div");
      empty.className = "col-empty";
      empty.textContent = "(empty)";
      col.appendChild(empty);
    }
    return col;
  }

  const tree = document.createElement("div");
  tree.className = "scope-tree";
  for (const key of keys) {
    tree.appendChild(treeNode(view.scope, [key], key, view.values[key], props, lowerQuery));
  }
  col.appendChild(tree);

  // Filter feedback. The tree leaves filter themselves silently when the
  // query doesn't match a rule string; surface a column-level placeholder
  // when a permissions block exists but every rule was filtered out, so
  // empty-looking columns aren't mysterious.
  if (lowerQuery !== "") {
    const totalRules = countPermissionRules(view.values);
    if (totalRules > 0 && countMatchingRules(view.values, lowerQuery) === 0) {
      const none = document.createElement("div");
      none.className = "col-no-matches";
      none.textContent = `No rules match “${props.query}”.`;
      col.appendChild(none);
    }
  }

  return col;
}

function countPermissionRules(values: { [key: string]: JsonValue }): number {
  const perms = values.permissions;
  if (!perms || typeof perms !== "object" || Array.isArray(perms)) return 0;
  let total = 0;
  for (const kind of PERMISSION_KINDS) {
    const list = (perms as { [k: string]: JsonValue })[kind];
    if (!Array.isArray(list)) continue;
    // Mirror Rust's `permissions_from_values` and `countMatchingRules`:
    // only string entries count as permission rules. Hand-edited files
    // can put numbers / objects in the array, and counting those would
    // make the column status disagree with the renderer + filter UI.
    for (const item of list) {
      if (typeof item === "string") total++;
    }
  }
  return total;
}

function countMatchingRules(values: { [key: string]: JsonValue }, lowerQuery: string): number {
  const perms = values.permissions;
  if (!perms || typeof perms !== "object" || Array.isArray(perms)) return 0;
  let matched = 0;
  for (const kind of PERMISSION_KINDS) {
    const list = (perms as { [k: string]: JsonValue })[kind];
    if (!Array.isArray(list)) continue;
    for (const item of list) {
      if (typeof item === "string" && matchesLoweredQuery(item, lowerQuery)) matched++;
    }
  }
  return matched;
}

/**
 * Show a modal diff confirm and resolve to whether the user applied the move.
 *
 * Keyboard:
 *   - Escape cancels.
 *   - Enter activates the focused button (Apply by default, since that's the
 *     initial focus — but Tab-to-Cancel followed by Enter cancels, matching
 *     platform button convention).
 *   - Tab / Shift+Tab cycle focus within the dialog.
 *
 * If `trigger` is passed and still live in the DOM when the dialog closes,
 * focus is returned to it.
 */
export function confirmMoveLeaf(
  preview: MoveLeafPreview,
  trigger?: HTMLElement | null,
): Promise<boolean> {
  const isSameScopeChangeKind =
    preview.to_kind !== undefined && preview.from.scope === preview.to.scope;

  const subtitle = document.createDocumentFragment();
  const target = document.createElement("code");
  target.className = "chip";
  target.textContent = leafSubtitleLabel(preview);
  subtitle.appendChild(target);
  if (isSameScopeChangeKind && preview.to_kind !== undefined) {
    const fromKind = permissionKindForPath(preview.path);
    subtitle.appendChild(
      document.createTextNode(
        ` — ${fromKind ? KIND_LABELS[fromKind] : ""} → ${KIND_LABELS[preview.to_kind]} in ${SCOPE_LABELS[preview.from.scope]}`,
      ),
    );
  } else {
    subtitle.appendChild(
      document.createTextNode(
        ` from ${SCOPE_LABELS[preview.from.scope]} to ${SCOPE_LABELS[preview.to.scope]}`,
      ),
    );
    if (preview.to_kind !== undefined) {
      subtitle.appendChild(
        document.createTextNode(` (reclassified as ${KIND_LABELS[preview.to_kind]})`),
      );
    }
  }

  const diff = document.createElement("div");
  diff.className = isSameScopeChangeKind ? "modal-diff modal-diff-single" : "modal-diff";
  if (isSameScopeChangeKind) {
    // Same-scope change-kind writes one file in one shot — render only the
    // source side so the user isn't presented with two visually identical
    // panes. The single side's `key_before` / `key_after` already reflect
    // both the remove (from old kind) and the add (to new kind).
    diff.appendChild(leafDiffSide(preview, "remove"));
  } else {
    diff.appendChild(leafDiffSide(preview, "remove"));
    diff.appendChild(leafDiffSide(preview, "add"));
  }

  return openConfirmModal({
    titleText: leafModalTitle(preview),
    subtitle,
    body: diff,
    trigger,
  });
}

function leafModalTitle(preview: MoveLeafPreview): string {
  if (preview.to_kind !== undefined) {
    return preview.from.scope === preview.to.scope
      ? "Reclassify rule"
      : "Reclassify rule across scopes";
  }
  switch (preview.kind) {
    case "permission_rule": {
      const kind = permissionKindForPath(preview.path);
      return `Move ${kind ? KIND_LABELS[kind] : ""} rule`.trim();
    }
    case "permission_list": {
      const kind = permissionKindForPath(preview.path);
      return `Move ${kind ? KIND_LABELS[kind] : ""} rule list`.trim();
    }
    case "top_level_key":
      return "Move settings key";
  }
}

function leafSubtitleLabel(preview: MoveLeafPreview): string {
  // For a single rule the subtitle chip carries the rule string itself; for
  // a list or top-level key it's the path so the user reads "Move env from
  // ..." or "Move permissions.allow from ...".
  if (preview.kind === "permission_rule") {
    const rule = leafValueAtPath(preview.from.key_before, preview.path);
    if (typeof rule === "string") return rule;
  }
  return describePath(preview.path);
}

/**
 * Drill into a `key_before` / `key_after` value using the leaf path
 * (segments after the affected top-level key). Returns the leaf value or
 * `undefined` if any segment misses — same skip-on-absent contract as the
 * IPC.
 */
function leafValueAtPath(keyValue: JsonValue | undefined, path: PathSeg[]): JsonValue | undefined {
  if (keyValue === undefined) return undefined;
  let cur: JsonValue | undefined = keyValue;
  for (let i = 1; i < path.length; i++) {
    const seg = path[i];
    if (cur === null || cur === undefined) return undefined;
    if (typeof seg === "number") {
      if (!Array.isArray(cur)) return undefined;
      cur = cur[seg];
    } else {
      if (typeof cur !== "object" || Array.isArray(cur)) return undefined;
      cur = (cur as { [k: string]: JsonValue })[seg];
    }
  }
  return cur;
}

function leafDiffSide(preview: MoveLeafPreview, mode: "add" | "remove"): HTMLElement {
  const side = mode === "remove" ? preview.from : preview.to;
  const col = document.createElement("div");
  col.className = `modal-side modal-side-${mode}`;

  const head = document.createElement("div");
  head.className = "modal-side-head";
  const label = document.createElement("h3");
  label.textContent = SCOPE_LABELS[side.scope];
  head.appendChild(label);

  const filePath = document.createElement("div");
  filePath.className = "modal-side-path";
  filePath.textContent = side.file_path;
  head.appendChild(filePath);

  const verdict = document.createElement("div");
  verdict.className = "modal-side-verdict";
  if (!side.will_write) {
    verdict.textContent = "(no change)";
    verdict.classList.add("muted");
  } else if (mode === "remove") {
    verdict.textContent = removeVerdict(preview.kind);
    verdict.classList.add("removed");
  } else {
    // `key_before === undefined` is the absence sentinel (Rust skipped the
    // field). For permission shapes the affected key is `permissions`,
    // which usually pre-exists, so "merged" is the more accurate verdict
    // than "added" — only show "added" when the whole affected top-level
    // key is being created from nothing.
    const created = side.key_before === undefined;
    verdict.textContent = addVerdict(preview.kind, created);
    verdict.classList.add("added");
  }
  head.appendChild(verdict);
  col.appendChild(head);

  if (side.note) {
    const note = document.createElement("div");
    note.className = "modal-side-note";
    note.textContent = side.note;
    col.appendChild(note);
  }

  // Body: chip-list rendering for permission shapes (preserves today's
  // confirmMove readability), JSON dump for top-level key moves
  // (preserves today's confirmMoveKey shape).
  const body = document.createElement("div");
  body.className = "modal-side-value";
  if (preview.kind === "top_level_key") {
    const pre = document.createElement("pre");
    pre.className = "modal-side-json";
    pre.textContent = formatValue(mode === "remove" ? side.key_before : side.key_after);
    body.appendChild(pre);
  } else {
    body.appendChild(permissionListDiff(preview, side, mode));
  }
  col.appendChild(body);

  return col;
}

/**
 * Chip-list before/after for permission moves. Highlights are driven by the
 * multiset delta between this side's `key_before` / `key_after`, not by
 * set membership — for a `permission_list` move the destination may
 * already share rules with the source (those are *not* "added"; array-
 * union dedupes), and a hand-edited file with the same rule listed twice
 * may have one copy removed (set membership would miss that, since the
 * value still appears). Walk `before` (or `after`) and consume entries
 * from the delta bag in iteration order so the highlight maps onto the
 * actual entries that change.
 */
function permissionListDiff(
  preview: MoveLeafPreview,
  side: MoveLeafSide,
  mode: "add" | "remove",
): HTMLElement {
  const list = document.createElement("ul");
  list.className = "modal-diff-list";
  const kind = permissionKindForPath(preview.path);
  if (!kind) {
    const li = document.createElement("li");
    li.className = "diff-empty";
    li.textContent = "(unknown permission shape)";
    list.appendChild(li);
    return list;
  }
  const itemsBefore = extractPermissionList(side.key_before, kind);
  const itemsAfter = extractPermissionList(side.key_after, kind);
  const items = mode === "remove" ? itemsBefore : itemsAfter;
  // Multiset subtraction: B \ A retains duplicates correctly. Computed
  // once outside the loop, then drained by the per-item walk so each
  // highlighted chip corresponds to exactly one delta entry.
  const deltaBag =
    mode === "remove"
      ? multisetSubtract(itemsBefore, itemsAfter)
      : multisetSubtract(itemsAfter, itemsBefore);

  for (const rule of items) {
    const li = document.createElement("li");
    const code = document.createElement("code");
    code.textContent = rule;
    const remaining = deltaBag.get(rule) ?? 0;
    if (remaining > 0) {
      li.className = mode === "remove" ? "diff-removed" : "diff-added";
      if (remaining === 1) {
        deltaBag.delete(rule);
      } else {
        deltaBag.set(rule, remaining - 1);
      }
    }
    li.appendChild(code);
    list.appendChild(li);
  }
  if (list.children.length === 0) {
    const li = document.createElement("li");
    li.className = "diff-empty";
    li.textContent = "(empty)";
    list.appendChild(li);
  }
  return list;
}

/**
 * Multiset subtraction `a - b`: returns a `Map<string, number>` whose
 * entries are the per-string surplus counts in `a` that aren't covered
 * by `b`. Used by `permissionListDiff` so duplicate rule strings get
 * counted, not collapsed by set semantics.
 */
function multisetSubtract(a: readonly string[], b: readonly string[]): Map<string, number> {
  const out = new Map<string, number>();
  for (const x of a) out.set(x, (out.get(x) ?? 0) + 1);
  for (const x of b) {
    const c = out.get(x);
    if (c === undefined) continue;
    if (c <= 1) out.delete(x);
    else out.set(x, c - 1);
  }
  return out;
}

function removeVerdict(kind: MoveLeafKind): string {
  switch (kind) {
    case "top_level_key":
      return "key removed";
    case "permission_list":
      return "list removed";
    case "permission_rule":
      return "rule removed";
  }
}

function addVerdict(kind: MoveLeafKind, created: boolean): string {
  switch (kind) {
    case "top_level_key":
      return created ? "key added" : "key merged";
    case "permission_list":
      return created ? "list added" : "list merged";
    case "permission_rule":
      // Single-rule destination is always a list-append; the parent
      // `permissions` map's pre-existence is irrelevant to the verdict.
      return "rule added";
  }
}

function extractPermissionList(
  keyValue: JsonValue | undefined,
  kind: PermissionKind,
): readonly string[] {
  if (keyValue === undefined || keyValue === null) return [];
  if (typeof keyValue !== "object" || Array.isArray(keyValue)) return [];
  const arr = (keyValue as { [k: string]: JsonValue })[kind];
  if (!Array.isArray(arr)) return [];
  return arr.filter((v): v is string => typeof v === "string");
}

function openConfirmModal(opts: {
  titleText: string;
  subtitle: Node;
  body: HTMLElement;
  trigger?: HTMLElement | null;
}): Promise<boolean> {
  return new Promise((resolve) => {
    let resolved = false;
    const resolveOnce = (result: boolean) => {
      if (resolved) return;
      resolved = true;
      resolve(result);
    };
    openModal({
      titleText: opts.titleText,
      subtitle: opts.subtitle,
      body: opts.body,
      trigger: opts.trigger,
      actions: [
        {
          label: "Cancel",
          className: "btn-cancel",
          activate: (close) => {
            resolveOnce(false);
            close();
          },
        },
        {
          label: "Apply",
          className: "btn-apply",
          focus: true,
          activate: (close) => {
            resolveOnce(true);
            close();
          },
        },
      ],
      onEscape: (close) => {
        resolveOnce(false);
        close();
      },
      onEnter: (close) => {
        resolveOnce(true);
        close();
      },
      onBackdropClick: (close) => {
        resolveOnce(false);
        close();
      },
      onClose: () => resolveOnce(false),
    });
  });
}

/**
 * Shared modal shell. Handles backdrop, panel ARIA wiring, keyboard trap,
 * Escape / backdrop-click / Enter behavior, and focus restoration — so
 * callers only supply the body and the action buttons they need.
 *
 * The goal is to keep `confirmMoveLeaf` / `openSettings`
 * from drifting on accessibility details over time. Each caller passes
 * its own activation callbacks that receive a `close` function; the
 * helper never closes on its own except when the caller asks it to.
 */
interface ModalAction {
  label: string;
  className?: string;
  /** Mark the button that should receive initial focus. Falls back to the
   *  first action if no button is flagged. */
  focus?: boolean;
  activate: (close: () => void) => void;
}

function openModal(opts: {
  titleText: string;
  subtitle?: Node;
  body: HTMLElement;
  actions: ModalAction[];
  trigger?: HTMLElement | null;
  panelClassName?: string;
  /** Defaults to closing the modal. */
  onEscape?: (close: () => void) => void;
  /** Defaults to a no-op so Enter doesn't unexpectedly activate anything
   *  when the modal has no obvious "default" action. */
  onEnter?: (close: () => void) => void;
  /** Defaults to closing the modal. */
  onBackdropClick?: (close: () => void) => void;
  /** Fired when the modal closes for any reason, after DOM teardown. Useful
   *  when the caller needs to resolve a pending promise that the actions
   *  might not have resolved (e.g. the user dismisses without picking). */
  onClose?: () => void;
}): void {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";

  const titleId = `modal-title-${++modalIdCounter}`;

  const panel = document.createElement("div");
  panel.className = opts.panelClassName ? `modal ${opts.panelClassName}` : "modal";
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-modal", "true");
  panel.setAttribute("aria-labelledby", titleId);

  const title = document.createElement("h2");
  title.id = titleId;
  title.className = "modal-title";
  title.textContent = opts.titleText;
  panel.appendChild(title);

  if (opts.subtitle) {
    const subtitle = document.createElement("div");
    subtitle.className = "modal-subtitle";
    subtitle.appendChild(opts.subtitle);
    panel.appendChild(subtitle);
  }

  panel.appendChild(opts.body);

  const actionsEl = document.createElement("div");
  actionsEl.className = "modal-actions";
  const actionButtons: HTMLButtonElement[] = [];
  for (const action of opts.actions) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.textContent = action.label;
    if (action.className) btn.className = action.className;
    btn.addEventListener("click", () => action.activate(close));
    actionsEl.appendChild(btn);
    actionButtons.push(btn);
  }
  panel.appendChild(actionsEl);

  backdrop.appendChild(panel);

  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    document.removeEventListener("keydown", onKey);
    backdrop.remove();
    if (opts.trigger && document.body.contains(opts.trigger)) {
      opts.trigger.focus();
    }
    opts.onClose?.();
  };

  const focusableSelector =
    'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      (opts.onEscape ?? ((c) => c()))(close);
      return;
    }
    if (e.key === "Enter") {
      // Only treat Enter as an activation when it isn't already firing a
      // focused button — otherwise the button's own click handler runs.
      if (opts.onEnter && !(document.activeElement instanceof HTMLButtonElement)) {
        e.preventDefault();
        opts.onEnter(close);
      }
      return;
    }
    if (e.key === "Tab") {
      const focusables = Array.from(panel.querySelectorAll<HTMLElement>(focusableSelector));
      if (focusables.length === 0) {
        e.preventDefault();
        return;
      }
      const first = focusables[0];
      const last = focusables[focusables.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey) {
        if (active === first || !panel.contains(active)) {
          e.preventDefault();
          last.focus();
        }
      } else {
        if (active === last || !panel.contains(active)) {
          e.preventDefault();
          first.focus();
        }
      }
    }
  };
  document.addEventListener("keydown", onKey);
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) {
      (opts.onBackdropClick ?? ((c) => c()))(close);
    }
  });

  document.body.appendChild(backdrop);
  const initial = actionButtons.find((_, i) => opts.actions[i].focus) ?? actionButtons[0];
  initial?.focus();
}

function formatValue(v: JsonValue | undefined): string {
  // `undefined` is the absence sentinel (Rust skipped the field); a literal
  // JSON `null` should stringify as "null", not collapse to "(absent)".
  if (v === undefined) return "(absent)";
  return JSON.stringify(v, null, 2);
}

// -- Context menu primitive (#8) ---------------------------------------------

/** One item in a context menu — leaf, separator, or nested submenu. */
type MenuItem =
  | { label: string; onClick: () => void; disabled?: boolean }
  | { label: string; submenu: MenuItem[]; disabled?: boolean }
  | { separator: true };

let openContextMenuClose: (() => void) | null = null;

function closeOpenContextMenu(): void {
  openContextMenuClose?.();
}

/**
 * Open a cursor-positioned context menu (#8). Supports nested submenus that
 * fly out to the right (or left when there isn't room), keyboard navigation
 * (arrows, Enter, Esc), and dismiss on click-outside or a second
 * `contextmenu` event elsewhere. Focus restores to `trigger` when the menu
 * closes.
 *
 * Coexistence with the lint popover: opens close any pinned popover so the
 * two transient surfaces don't compete for outside-click handlers.
 */
function openContextMenu(items: MenuItem[], x: number, y: number, trigger?: HTMLElement): void {
  closeOpenContextMenu();
  closePinnedPopover();

  let closed = false;
  const stack: HTMLElement[] = [];

  function close(): void {
    if (closed) return;
    closed = true;
    for (const el of stack) el.remove();
    stack.length = 0;
    document.removeEventListener("keydown", onKey, true);
    document.removeEventListener("mousedown", onOutsideMouse, true);
    document.removeEventListener("contextmenu", onOutsideContext, true);
    openContextMenuClose = null;
    if (trigger && document.body.contains(trigger)) trigger.focus();
  }
  openContextMenuClose = close;

  function buttonsIn(menu: HTMLElement): HTMLButtonElement[] {
    return Array.from(
      menu.querySelectorAll<HTMLButtonElement>("button.context-menu-item:not(:disabled)"),
    );
  }

  function closeSubmenusBelow(depth: number): void {
    while (stack.length > depth + 1) {
      const el = stack.pop();
      el?.remove();
    }
  }

  function buildMenu(items: MenuItem[], anchor: DOMRect | null, depth: number): HTMLElement {
    const menu = document.createElement("div");
    menu.className = "context-menu";
    menu.setAttribute("role", "menu");
    for (const item of items) {
      if ("separator" in item) {
        const sep = document.createElement("div");
        sep.className = "context-menu-sep";
        sep.setAttribute("role", "separator");
        menu.appendChild(sep);
        continue;
      }
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "context-menu-item";
      btn.setAttribute("role", "menuitem");
      btn.textContent = item.label;
      if ("submenu" in item) {
        btn.classList.add("context-menu-submenu-trigger");
        const arrow = document.createElement("span");
        arrow.className = "context-menu-arrow";
        arrow.textContent = "▸";
        btn.appendChild(arrow);
        if (item.disabled) {
          btn.disabled = true;
        } else {
          btn.addEventListener("click", (e) => {
            e.preventDefault();
            e.stopPropagation();
            openSubmenu(item.submenu, btn, depth);
          });
          btn.addEventListener("mouseenter", () => {
            openSubmenu(item.submenu, btn, depth);
          });
        }
      } else {
        if (item.disabled) {
          btn.disabled = true;
        } else {
          btn.addEventListener("click", (e) => {
            e.preventDefault();
            e.stopPropagation();
            item.onClick();
            close();
          });
          btn.addEventListener("mouseenter", () => closeSubmenusBelow(depth));
        }
      }
      menu.appendChild(btn);
    }

    document.body.appendChild(menu);
    const rect = menu.getBoundingClientRect();
    let left: number;
    let top: number;
    if (anchor) {
      left = anchor.right;
      top = anchor.top;
      if (left + rect.width > window.innerWidth) {
        left = Math.max(0, anchor.left - rect.width);
      }
    } else {
      left = x;
      top = y;
      if (left + rect.width > window.innerWidth) {
        left = Math.max(0, window.innerWidth - rect.width);
      }
    }
    if (top + rect.height > window.innerHeight) {
      top = Math.max(0, window.innerHeight - rect.height);
    }
    menu.style.left = `${left}px`;
    menu.style.top = `${top}px`;
    buttonsIn(menu)[0]?.focus();
    return menu;
  }

  function openSubmenu(items: MenuItem[], anchor: HTMLElement, depth: number): void {
    closeSubmenusBelow(depth);
    const sub = buildMenu(items, anchor.getBoundingClientRect(), depth + 1);
    stack.push(sub);
  }

  const root = buildMenu(items, null, 0);
  stack.push(root);

  function focusedDepth(): number {
    const active = document.activeElement as Element | null;
    if (!active) return -1;
    return stack.findIndex((m) => m.contains(active));
  }

  function onKey(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      if (stack.length > 1) {
        const top = stack.pop();
        top?.remove();
        const parent = stack[stack.length - 1];
        const t = parent.querySelector<HTMLButtonElement>(
          "button.context-menu-item.context-menu-submenu-trigger",
        );
        t?.focus();
      } else {
        close();
      }
      return;
    }
    const depth = focusedDepth();
    if (depth < 0) return;
    const menu = stack[depth];
    const btns = buttonsIn(menu);
    const active = document.activeElement as HTMLButtonElement | null;
    const idx = active ? btns.indexOf(active) : -1;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      btns[(idx + 1 + btns.length) % btns.length]?.focus();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      btns[(idx - 1 + btns.length) % btns.length]?.focus();
    } else if (e.key === "ArrowRight") {
      if (active?.classList.contains("context-menu-submenu-trigger")) {
        e.preventDefault();
        active.click();
      }
    } else if (e.key === "ArrowLeft") {
      if (stack.length > 1) {
        e.preventDefault();
        const top = stack.pop();
        top?.remove();
        const parent = stack[stack.length - 1];
        const t = parent.querySelector<HTMLButtonElement>(
          "button.context-menu-item.context-menu-submenu-trigger",
        );
        t?.focus();
      }
    }
  }

  function onOutsideMouse(e: MouseEvent): void {
    const target = e.target as Node;
    if (stack.some((m) => m.contains(target))) return;
    close();
  }
  function onOutsideContext(e: MouseEvent): void {
    const target = e.target as Node;
    if (stack.some((m) => m.contains(target))) return;
    close();
  }

  document.addEventListener("keydown", onKey, true);
  document.addEventListener("mousedown", onOutsideMouse, true);
  document.addEventListener("contextmenu", onOutsideContext, true);
}

/**
 * Wire `contextmenu` and Shift+F10 / Menu-key keyboard activation on `el`
 * to open a context menu built lazily by `build`. The build function is
 * called on each activation so menu state (clipboard contents, current
 * scope visibility, etc.) is fresh.
 */
function attachContextMenu(el: HTMLElement, build: () => MenuItem[]): void {
  el.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const items = build();
    if (items.length === 0) return;
    openContextMenu(items, e.clientX, e.clientY, el);
  });
  el.addEventListener("keydown", (e) => {
    if ((e.key === "F10" && e.shiftKey) || e.key === "ContextMenu") {
      e.preventDefault();
      e.stopPropagation();
      const items = build();
      if (items.length === 0) return;
      const rect = el.getBoundingClientRect();
      openContextMenu(items, rect.left, rect.bottom, el);
    }
  });
}

/**
 * Build the Move-to submenu structure (#8): User / User-Local at top,
 * then a separator, then each known project as a nested submenu of its
 * Local / Project scopes. `from` (and optional `fromProject`) drive the
 * "skip the source" filter; `isScopeVisible` hides columns the user
 * collapsed.
 *
 * `fromProject` is the project root the source belongs to when known —
 * for v1 there's only the current project so this is just `props.projectDir`,
 * but #106 will give us the choice of multiple projects and the source-
 * skip will need the project identity to disambiguate Local-of-A from
 * Local-of-B.
 */
function buildMoveToSubmenu(
  props: AppProps,
  from: Scope,
  onPick: (target: Scope) => void,
): MenuItem[] {
  const items: MenuItem[] = [];
  // Machine-global scopes first.
  for (const target of ["user", "user_local"] as Scope[]) {
    if (target === from) continue;
    if (!isScopeVisible(target)) continue;
    items.push({ label: SCOPE_LABELS[target], onClick: () => onPick(target) });
  }
  const projects = getKnownProjects(props);
  if (projects.length > 0 && items.length > 0) {
    items.push({ separator: true });
  }
  for (const project of projects) {
    const inner: MenuItem[] = [];
    for (const target of ["local", "project"] as Scope[]) {
      if (target === from) continue;
      if (!isScopeVisible(target)) continue;
      inner.push({ label: SCOPE_LABELS[target], onClick: () => onPick(target) });
    }
    if (inner.length === 0) continue;
    items.push({ label: project.name, submenu: inner });
  }
  return items;
}

/**
 * Copy text to the OS clipboard via Tauri's clipboard plugin (#8). Goes
 * through the native side so the webview never prompts the user for
 * `navigator.clipboard` permissions — desktop UX shouldn't ask "may
 * localhost access your clipboard?" on every right-click.
 */
function copyToClipboard(text: string): void {
  void writeText(text).catch((err) => {
    console.warn("clipboard write failed:", err);
  });
}

/**
 * Confirm modal for a delete-leaf action (#8). Mirrors `confirmMoveLeaf`'s
 * shape but renders only the source side, since there's no destination.
 */
export function confirmDeleteLeaf(
  preview: DeleteLeafPreview,
  trigger?: HTMLElement | null,
): Promise<boolean> {
  const subtitle = document.createDocumentFragment();
  const target = document.createElement("code");
  target.className = "chip";
  if (preview.kind === "permission_rule") {
    const rule = leafValueAtPath(preview.from.key_before, preview.path);
    target.textContent = typeof rule === "string" ? rule : describePath(preview.path);
  } else {
    target.textContent = describePath(preview.path);
  }
  subtitle.appendChild(target);
  subtitle.appendChild(document.createTextNode(` from ${SCOPE_LABELS[preview.from.scope]}`));

  const diff = document.createElement("div");
  diff.className = "modal-diff modal-diff-single";
  // Reuse leafDiffSide by handing it a synthesized two-sided preview where
  // only the `remove` side is read. The `to` field has to be present for
  // the type, but `leafDiffSide` never touches it when mode === "remove".
  const synthetic: MoveLeafPreview = {
    path: preview.path,
    kind: preview.kind,
    from: preview.from,
    to: preview.from,
  };
  diff.appendChild(leafDiffSide(synthetic, "remove"));

  return openConfirmModal({
    titleText: deleteModalTitle(preview.kind),
    subtitle,
    body: diff,
    trigger,
  });
}

/** Confirm modal for an add-leaf action (#8) — paste destination side only. */
export function confirmAddLeaf(
  preview: AddLeafPreview,
  trigger?: HTMLElement | null,
): Promise<boolean> {
  const subtitle = document.createDocumentFragment();
  const target = document.createElement("code");
  target.className = "chip";
  if (preview.kind === "permission_rule") {
    const rule = leafValueAtPath(preview.to.key_after, preview.path);
    target.textContent = typeof rule === "string" ? rule : describePath(preview.path);
  } else {
    target.textContent = describePath(preview.path);
  }
  subtitle.appendChild(target);
  subtitle.appendChild(document.createTextNode(` into ${SCOPE_LABELS[preview.to.scope]}`));

  const diff = document.createElement("div");
  diff.className = "modal-diff modal-diff-single";
  const synthetic: MoveLeafPreview = {
    path: preview.path,
    kind: preview.kind,
    from: preview.to,
    to: preview.to,
  };
  diff.appendChild(leafDiffSide(synthetic, "add"));

  return openConfirmModal({
    titleText: addModalTitle(preview.kind),
    subtitle,
    body: diff,
    trigger,
  });
}

/**
 * Build the menu items for a permission rule leaf, top-level key leaf, or
 * movable branch (#8). Includes Copy, Delete, Change-kind (rules only),
 * and Move-to. Disabled actions still render so users see why something
 * isn't available.
 */
function leafContextMenuItems(
  scope: Scope,
  path: PathSeg[],
  value: JsonValue,
  props: AppProps,
): MenuItem[] {
  const items: MenuItem[] = [];
  const permKind = permissionKindForPath(path);
  const isRule = permKind !== null && path.length === 3 && typeof value === "string";

  items.push({
    label: "Copy",
    onClick: () => {
      // Rule strings copy as plain text; everything else copies as
      // pretty-printed JSON so it round-trips through paste in another
      // editor.
      copyToClipboard(isRule ? (value as string) : JSON.stringify(value, null, 2));
    },
  });

  items.push({
    label: "Delete",
    onClick: () => props.onDeleteLeaf({ path, from: scope }),
    disabled: props.busy,
  });

  if (isRule && permKind) {
    const sub: MenuItem[] = [];
    for (const kind of PERMISSION_KINDS) {
      sub.push({
        label: capitalize(KIND_LABELS[kind]),
        onClick: () => props.onChangeKind(path, scope, kind),
        disabled: props.busy || kind === permKind,
      });
    }
    items.push({ label: "Change kind", submenu: sub });
  }

  const moveItems = buildMoveToSubmenu(props, scope, (target) => {
    props.onMoveLeaf({ path, from: scope, to: target });
  });
  items.push({
    label: "Move to",
    submenu:
      moveItems.length > 0
        ? moveItems
        : [{ label: "(no other scopes)", onClick: () => {}, disabled: true }],
    disabled: props.busy,
  });

  return items;
}

/** Menu items for a chip in the combined-permissions panel (#8). */
function combinedChipContextMenuItems(
  rule: string,
  originScopes: Scope[],
  props: AppProps,
): MenuItem[] {
  const items: MenuItem[] = [{ label: "Copy", onClick: () => copyToClipboard(rule) }];
  // Highest-precedence origin acts as the implicit source for a Move-to
  // from the combined panel — that's the chip the user actually sees in
  // the effective view, and the one that would shadow the others if they
  // disagreed.
  const sourceScope = originScopes[0];
  if (sourceScope) {
    const sourcePath = findPermissionPath(props, rule, sourceScope);
    if (sourcePath) {
      const moveItems = buildMoveToSubmenu(props, sourceScope, (target) => {
        props.onMoveLeaf({ path: sourcePath, from: sourceScope, to: target });
      });
      items.push({
        label: `Move to (from ${SCOPE_LABELS[sourceScope]})`,
        submenu:
          moveItems.length > 0
            ? moveItems
            : [{ label: "(no other scopes)", onClick: () => {}, disabled: true }],
        disabled: props.busy,
      });
    }
  }
  return items;
}

/**
 * Resolve a rule string back to its `permissions.<kind>[i]` path in a
 * specific scope. Used by the combined-panel context menu to construct
 * the concrete source path for a Move-to driven by an aggregated chip.
 */
function findPermissionPath(props: AppProps, rule: string, scope: Scope): PathSeg[] | null {
  const view = props.scopes?.scopes.find((s) => s.scope === scope);
  if (!view) return null;
  const perms = view.values.permissions;
  if (!perms || typeof perms !== "object" || Array.isArray(perms)) return null;
  for (const kind of PERMISSION_KINDS) {
    const list = (perms as { [k: string]: JsonValue })[kind];
    if (!Array.isArray(list)) continue;
    const idx = list.indexOf(rule);
    if (idx >= 0) return ["permissions", kind, idx];
  }
  return null;
}

/** Menu items for a scope column's empty area (#8 paste). */
function scopeColumnContextMenuItems(scope: Scope, props: AppProps): MenuItem[] {
  const sub: MenuItem[] = [];
  for (const kind of PERMISSION_KINDS) {
    sub.push({
      label: capitalize(KIND_LABELS[kind]),
      onClick: () => {
        // Read the clipboard on activation rather than ahead of time —
        // there's no synchronous way to inspect it during `contextmenu`.
        // The promise resolves before the next tick on success; on failure
        // (denied permission, no clipboard API) we surface the error.
        void (async () => {
          let text = "";
          try {
            // Tauri's clipboard plugin reads from the OS via Rust, so this
            // doesn't go through the webview's permissions prompt.
            text = (await readText()) ?? "";
          } catch (err) {
            alert(`Couldn't read clipboard: ${err}`);
            return;
          }
          const trimmed = text.trim();
          if (trimmed === "") {
            alert("Clipboard is empty — nothing to paste.");
            return;
          }
          props.onAddLeaf({
            path: ["permissions", kind, 0],
            to: scope,
            value: trimmed,
          });
        })();
      },
      disabled: props.busy,
    });
  }
  return [{ label: "Paste as", submenu: sub, disabled: props.busy }];
}

function capitalize(s: string): string {
  return s.length === 0 ? s : s.charAt(0).toUpperCase() + s.slice(1);
}

function deleteModalTitle(kind: MoveLeafKind): string {
  switch (kind) {
    case "permission_rule":
      return "Delete rule";
    case "permission_list":
      return "Delete rule list";
    case "top_level_key":
      return "Delete settings key";
  }
}

function addModalTitle(kind: MoveLeafKind): string {
  switch (kind) {
    case "permission_rule":
      return "Paste rule";
    case "permission_list":
      return "Paste rule list";
    case "top_level_key":
      return "Paste settings key";
  }
}

interface SettingsProps {
  preferences: Preferences;
  onToggleScopeVisibility: (scope: Scope, visible: boolean) => void;
  onChangeTheme: (theme: Theme) => void;
  /** Toggle `.bak` creation on writes (#88). Same persist-on-click model
   *  as the other settings — the new pref hits the backend immediately so
   *  the very next move respects it without a dialog round-trip. */
  onToggleBackupOnWrite: (enabled: boolean) => void;
  /** Toggle audit-log rotation (#127). When off, `audit.jsonl` grows
   *  unbounded; when on, it rotates to a year-month archive once it
   *  passes `audit_log_max_size_mb`. */
  onToggleAuditLogRotate: (enabled: boolean) => void;
  /** Change the rotation size threshold in MB (#127). The backend
   *  clamps to `[1, 1000]`; the UI also enforces those bounds at the
   *  input level so the user sees the limits inline. */
  onChangeAuditLogMaxSizeMb: (mb: number) => void;
  /** Set the tool-grouping threshold (#115). `null` = never group;
   *  otherwise group when this many rules share a tool prefix. */
  onChangeGroupRulesAt: (value: number | null) => void;
}

const THEME_OPTIONS: ReadonlyArray<{ value: Theme; label: string }> = [
  { value: "auto", label: "Match system" },
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
];

/**
 * Open the settings dialog. Changes persist as the user clicks — there's no
 * Apply/Cancel dance here, so the dialog only exposes a single "Close"
 * action. Focus is restored to `trigger` when the dialog closes, like the
 * rule-move confirm flow.
 *
 * Rides on the shared `openModal` so the keyboard trap and focus machinery
 * stay identical to the confirm modal (Tab cycling, Escape closes, backdrop
 * click closes). Enter is deliberately unset — the settings body has
 * checkboxes and the panel has no default action worth activating on
 * stray keypresses.
 */
export function openSettings(props: SettingsProps, trigger?: HTMLElement | null): void {
  const body = document.createElement("div");
  body.appendChild(settingsThemeSection(props));
  body.appendChild(settingsColumnsSection(props));
  body.appendChild(settingsGroupRulesSection(props));
  body.appendChild(settingsBackupSection(props));
  body.appendChild(settingsAuditRotationSection(props));

  openModal({
    titleText: "Settings",
    body,
    actions: [
      {
        label: "Close",
        className: "btn-apply",
        focus: true,
        activate: (close) => close(),
      },
    ],
    panelClassName: "modal-settings",
    trigger,
  });
}

function settingsThemeSection(props: SettingsProps): HTMLElement {
  const section = document.createElement("section");
  section.className = "settings-section";

  const heading = document.createElement("h3");
  heading.id = "settings-theme-heading";
  heading.className = "settings-heading";
  heading.textContent = "Theme";
  section.appendChild(heading);

  const hint = document.createElement("p");
  hint.className = "settings-hint";
  hint.textContent = "Match the OS or pin a palette. Persists across launches.";
  section.appendChild(hint);

  // Shared `name` so the radios behave as a single group with arrow-key
  // navigation. Keep the value local so subsequent re-renders within this
  // dialog could read it back (today there are none, but the
  // settings-checklist sibling already follows that pattern).
  const list = document.createElement("div");
  list.className = "settings-checklist";
  list.setAttribute("role", "radiogroup");
  list.setAttribute("aria-labelledby", "settings-theme-heading");
  const groupName = "settings-theme";
  for (const opt of THEME_OPTIONS) {
    const row = document.createElement("label");
    row.className = "settings-check";
    const input = document.createElement("input");
    input.type = "radio";
    input.name = groupName;
    input.value = opt.value;
    input.checked = props.preferences.theme === opt.value;
    input.addEventListener("change", () => {
      if (input.checked) props.onChangeTheme(opt.value);
    });
    row.appendChild(input);
    const label = document.createElement("span");
    label.textContent = opt.label;
    row.appendChild(label);
    list.appendChild(row);
  }
  section.appendChild(list);
  return section;
}

function settingsColumnsSection(props: SettingsProps): HTMLElement {
  const section = document.createElement("section");
  section.className = "settings-section";

  const heading = document.createElement("h3");
  heading.className = "settings-heading";
  heading.textContent = "Scope columns";
  section.appendChild(heading);

  const hint = document.createElement("p");
  hint.className = "settings-hint";
  hint.textContent = "Hide scope columns you don't need. Preferences persist across launches.";
  section.appendChild(hint);

  // Local to this dialog — not the render-wide currentVisibleScopes set,
  // because settings can change while the dialog is open and we want the
  // checkbox state to reflect the latest click, not the last render.
  const visible = new Set(props.preferences.visible_scopes);
  const list = document.createElement("div");
  list.className = "settings-checklist";
  for (const scope of SCOPES) {
    const row = document.createElement("label");
    row.className = "settings-check";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = visible.has(scope);
    // Guard: don't let the user uncheck the last visible column. The grid
    // would otherwise render empty with no obvious way back from inside the
    // dialog.
    cb.addEventListener("change", () => {
      if (!cb.checked && visible.size === 1 && visible.has(scope)) {
        cb.checked = true;
        return;
      }
      if (cb.checked) visible.add(scope);
      else visible.delete(scope);
      props.onToggleScopeVisibility(scope, cb.checked);
    });
    row.appendChild(cb);
    const label = document.createElement("span");
    label.textContent = SCOPE_LABELS[scope];
    row.appendChild(label);
    list.appendChild(row);
  }
  section.appendChild(list);
  return section;
}

/**
 * Backup toggle section in the Settings dialog (#88). One checkbox,
 * persisted on click via the same `onToggleBackupOnWrite` callback the
 * other settings rows use. The hint copy spells out *why* the safety
 * exists so a user who's about to flip it sees the trade-off before
 * removing the net — recoverability from a misclicked move.
 */
function settingsBackupSection(props: SettingsProps): HTMLElement {
  const section = document.createElement("section");
  section.className = "settings-section";

  const heading = document.createElement("h3");
  heading.id = "settings-backup-heading";
  heading.className = "settings-heading";
  heading.textContent = "Backups";
  section.appendChild(heading);

  const hint = document.createElement("p");
  hint.className = "settings-hint";
  hint.textContent =
    "On the first write of each session, ClaudeScope can drop a `.bak` next to each settings file. Disable if your `.claude/` directory is version-controlled or you don't want the extra files.";
  section.appendChild(hint);

  const list = document.createElement("div");
  list.className = "settings-checklist";
  const row = document.createElement("label");
  row.className = "settings-check";
  const cb = document.createElement("input");
  cb.type = "checkbox";
  cb.checked = props.preferences.backup_on_write;
  cb.addEventListener("change", () => {
    props.onToggleBackupOnWrite(cb.checked);
  });
  row.appendChild(cb);
  const label = document.createElement("span");
  label.textContent = "Create `.bak` files on save";
  row.appendChild(label);
  list.appendChild(row);
  section.appendChild(list);
  return section;
}

/**
 * Audit-log rotation controls (#127). A toggle for whether to rotate at
 * all, and a number input for the cap in megabytes. Disabled state when
 * the toggle is off — the number input is purely cosmetic when rotation
 * is disabled, so the visual greys-out and the input goes read-only.
 *
 * The number input's `change` event is what fires the dispatch (not
 * `input`), so the user isn't billed for every keystroke while they're
 * typing "2", "20", "200". `change` fires on commit (blur or Enter),
 * which lines up with how the rest of the settings dialog works.
 */
/**
 * Tool-grouping threshold radios (#115). Three discrete states — group
 * at 2, group at 3, or never — chosen over a free number input because
 * the meaningful range is small and users disagree on the right value;
 * spelling out the three options inline makes the trade-off legible.
 *
 * The wire shape (`number | null`) admits values >= 4 if a future
 * config edit asks for one; we just don't expose a UI for them. That
 * keeps the schema forward-compatible without crowding the dialog.
 */
function settingsGroupRulesSection(props: SettingsProps): HTMLElement {
  const section = document.createElement("section");
  section.className = "settings-section";

  const heading = document.createElement("h3");
  heading.id = "settings-group-rules-heading";
  heading.className = "settings-heading";
  heading.textContent = "Rule grouping";
  section.appendChild(heading);

  const hint = document.createElement("p");
  hint.className = "settings-hint";
  hint.textContent =
    "When rules share a tool prefix, fold them into a collapsible group. Lower thresholds fold more aggressively; 'Never' shows every rule flat.";
  section.appendChild(hint);

  const list = document.createElement("div");
  list.className = "settings-checklist";
  list.setAttribute("role", "radiogroup");
  list.setAttribute("aria-labelledby", "settings-group-rules-heading");
  const groupName = "settings-group-rules";

  const options: Array<{ value: number | null; label: string }> = [
    { value: 2, label: "Group at 2 (most aggressive)" },
    { value: 3, label: "Group at 3" },
    { value: null, label: "Never group" },
  ];

  for (const opt of options) {
    const row = document.createElement("label");
    row.className = "settings-check";
    const input = document.createElement("input");
    input.type = "radio";
    input.name = groupName;
    // Use a sentinel string for `null` so the radio value round-trips
    // cleanly. The click handler maps it back to `null` before
    // dispatching.
    input.value = opt.value === null ? "never" : String(opt.value);
    input.checked = props.preferences.group_rules_at === opt.value;
    input.addEventListener("change", () => {
      if (input.checked) props.onChangeGroupRulesAt(opt.value);
    });
    row.appendChild(input);
    const label = document.createElement("span");
    label.textContent = opt.label;
    row.appendChild(label);
    list.appendChild(row);
  }

  section.appendChild(list);
  return section;
}

function settingsAuditRotationSection(props: SettingsProps): HTMLElement {
  const section = document.createElement("section");
  section.className = "settings-section";

  const heading = document.createElement("h3");
  heading.className = "settings-heading";
  heading.textContent = "Audit log rotation";
  section.appendChild(heading);

  const hint = document.createElement("p");
  hint.className = "settings-hint";
  hint.textContent =
    "When the audit log passes the size threshold, ClaudeScope renames it to a year-month archive and starts fresh. Archives stay on disk indefinitely — delete them by hand if you want. Turn rotation off to keep one growing file instead.";
  section.appendChild(hint);

  const list = document.createElement("div");
  list.className = "settings-checklist";

  const toggleRow = document.createElement("label");
  toggleRow.className = "settings-check";
  const toggle = document.createElement("input");
  toggle.type = "checkbox";
  toggle.checked = props.preferences.audit_log_rotate;
  toggleRow.appendChild(toggle);
  const toggleLabel = document.createElement("span");
  toggleLabel.textContent = "Rotate audit log when it grows large";
  toggleRow.appendChild(toggleLabel);
  list.appendChild(toggleRow);

  // Size-cap row reuses the .settings-check layout for vertical rhythm,
  // but the actual input is a `<input type="number">` not a checkbox.
  const sizeRow = document.createElement("label");
  sizeRow.className = "settings-check settings-check-number";
  const sizeInput = document.createElement("input");
  sizeInput.type = "number";
  sizeInput.className = "settings-number-input";
  sizeInput.min = String(AUDIT_LOG_MAX_SIZE_MB.min);
  sizeInput.max = String(AUDIT_LOG_MAX_SIZE_MB.max);
  sizeInput.step = "1";
  sizeInput.value = String(props.preferences.audit_log_max_size_mb);
  sizeInput.disabled = !props.preferences.audit_log_rotate;
  sizeRow.appendChild(sizeInput);
  const sizeLabel = document.createElement("span");
  sizeLabel.textContent = `Rotate at this many MB (${AUDIT_LOG_MAX_SIZE_MB.min}–${AUDIT_LOG_MAX_SIZE_MB.max})`;
  sizeRow.appendChild(sizeLabel);
  list.appendChild(sizeRow);

  toggle.addEventListener("change", () => {
    sizeInput.disabled = !toggle.checked;
    props.onToggleAuditLogRotate(toggle.checked);
  });
  sizeInput.addEventListener("change", () => {
    // Clamp to the same bounds the backend enforces — without this, the
    // server would silently clamp out-of-range values to the default,
    // which is a less observable result than the visible clamp here.
    const raw = Number.parseInt(sizeInput.value, 10);
    if (!Number.isFinite(raw)) {
      sizeInput.value = String(props.preferences.audit_log_max_size_mb);
      return;
    }
    const clamped = Math.min(AUDIT_LOG_MAX_SIZE_MB.max, Math.max(AUDIT_LOG_MAX_SIZE_MB.min, raw));
    if (clamped !== raw) {
      sizeInput.value = String(clamped);
    }
    props.onChangeAuditLogMaxSizeMb(clamped);
  });

  section.appendChild(list);
  return section;
}

const REPO_URL = "https://github.com/bminier/claude-scope";

/**
 * Open the About dialog (#21). Renders the diagnostic block, repo / Claude
 * Code docs links, and a Copy-diagnostics button that drops a Markdown
 * block onto the OS clipboard ready to paste into a bug report.
 *
 * Rides the shared `openModal` so focus trap / Escape / backdrop click
 * stay identical to Settings. The dialog is purely presentational — no
 * IPC fires from inside it; `info` is supplied by the caller out of
 * cached state. A null `info` is rendered as a "Loading…" stub instead of
 * being treated as an error, since the bootstrap fetch is best-effort.
 */
export function openAbout(info: AppInfo | null, trigger?: HTMLElement | null): void {
  const body = document.createElement("div");
  body.className = "about-body";

  const intro = document.createElement("p");
  intro.className = "about-intro";
  intro.textContent = "Desktop GUI for promoting Claude Code settings between scopes.";
  body.appendChild(intro);

  body.appendChild(aboutDiagnosticsSection(info));
  body.appendChild(aboutLinksSection());

  const ack = document.createElement("p");
  ack.className = "about-ack";
  ack.textContent = "Built on Tauri. Licensed under MIT — see the LICENSE file at the repo root.";
  body.appendChild(ack);

  // Status line for the Copy-diagnostics button so a successful click
  // doesn't feel like a no-op. Kept as a sibling so screen-readers
  // announce the change without losing modal focus.
  const status = document.createElement("div");
  status.className = "about-copy-status";
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  body.appendChild(status);

  openModal({
    titleText: "About ClaudeScope",
    body,
    actions: [
      {
        label: "Copy diagnostics",
        className: "btn-copy",
        // Don't close the modal — copying is a side action; the user may
        // still want to read the links or copy again.
        activate: () => {
          // `info` is captured by the closure; render a sensible message
          // if the bootstrap fetch never resolved.
          if (!info) {
            status.textContent = "Diagnostics not loaded yet.";
            return;
          }
          void copyDiagnostics(info, status);
        },
      },
      {
        label: "Close",
        className: "btn-apply",
        focus: true,
        activate: (close) => close(),
      },
    ],
    panelClassName: "modal-about",
    trigger,
  });
}

function aboutDiagnosticsSection(info: AppInfo | null): HTMLElement {
  const section = document.createElement("section");
  section.className = "about-section";

  const heading = document.createElement("h3");
  heading.className = "about-heading";
  heading.textContent = "Diagnostics";
  section.appendChild(heading);

  const dl = document.createElement("dl");
  dl.className = "about-diagnostics";
  if (info) {
    appendKV(dl, "Version", versionDisplay(info));
    appendKV(dl, "Tauri", info.tauri_version);
    appendKV(dl, "WebView", info.webview_version ?? "unknown");
    appendKV(dl, "Rust (MSRV)", info.rust_version);
    appendKV(dl, "Platform", `${info.os} ${info.arch}`);
  } else {
    const loading = document.createElement("p");
    loading.className = "about-loading";
    loading.textContent = "Loading…";
    section.appendChild(loading);
    return section;
  }
  section.appendChild(dl);
  return section;
}

function versionDisplay(info: AppInfo): string {
  return info.git_sha ? `${info.version} (${info.git_sha})` : info.version;
}

function appendKV(dl: HTMLElement, key: string, value: string): void {
  const dt = document.createElement("dt");
  dt.textContent = key;
  const dd = document.createElement("dd");
  dd.textContent = value;
  dl.appendChild(dt);
  dl.appendChild(dd);
}

function aboutLinksSection(): HTMLElement {
  const section = document.createElement("section");
  section.className = "about-section";

  const heading = document.createElement("h3");
  heading.className = "about-heading";
  heading.textContent = "Links";
  section.appendChild(heading);

  const list = document.createElement("ul");
  list.className = "about-links";
  const links: Array<[string, string]> = [
    ["Source on GitHub", REPO_URL],
    ["Report an issue", `${REPO_URL}/issues/new`],
    ["Claude Code documentation", "https://docs.claude.com/en/docs/claude-code"],
  ];
  for (const [label, href] of links) {
    const li = document.createElement("li");
    const a = document.createElement("a");
    a.href = href;
    a.textContent = label;
    // `target=_blank` opens in the default browser via Tauri's link
    // hijacking — keeps the user out of the webview's history stack.
    a.target = "_blank";
    a.rel = "noopener noreferrer";
    li.appendChild(a);
    list.appendChild(li);
  }
  section.appendChild(list);
  return section;
}

/**
 * Render the diagnostic block in the same Markdown shape Rust emits
 * (`AppInfo::to_markdown`) and drop it on the OS clipboard via the Tauri
 * plugin. Going through the plugin (not `navigator.clipboard`) dodges the
 * webview's permission prompt — same trick #8's Copy/Paste rule action
 * uses. Status feedback updates the `status` node so the click feels
 * acknowledged without stealing focus.
 */
async function copyDiagnostics(info: AppInfo, status: HTMLElement): Promise<void> {
  const md = renderDiagnosticsMarkdown(info);
  try {
    await writeText(md);
    status.textContent = "Diagnostics copied.";
  } catch (err) {
    status.textContent = `Copy failed: ${err}`;
  }
}

/**
 * TS mirror of `AppInfo::to_markdown` so the About dialog and the CLI
 * produce byte-identical bug-report blocks. The Rust side is the source
 * of truth; if the two ever drift, both `app_info::tests` and the UI
 * test below should catch it.
 */
export function renderDiagnosticsMarkdown(info: AppInfo): string {
  const sha = info.git_sha ?? "unknown";
  const webview = info.webview_version ?? "unknown";
  return [
    `- ClaudeScope: ${info.version} (${sha})`,
    `- Tauri: ${info.tauri_version}, WebView: ${webview}`,
    `- Rust (MSRV): ${info.rust_version}`,
    `- OS: ${info.os} ${info.arch}`,
  ].join("\n");
}

/**
 * Open the audit-log History dialog (#19 phase 2). Read-only: lists every
 * write ClaudeScope has made since the audit log was first written, in
 * reverse-chronological order. The "Restore to before this" action the
 * issue spec mentions lands with #19 phase 3+; this dialog deliberately
 * omits it so the read path can ship first.
 *
 * `page` arrives null when the bootstrap fetch failed (e.g. the audit
 * file was unreadable) — the dialog still opens, with an explanatory
 * status line, rather than swallowing the click. Empty `records` arrays
 * are common (fresh install, no writes yet) and render as a friendly
 * empty state.
 */
export function openHistory(page: AuditLogPage | null, trigger?: HTMLElement | null): void {
  const body = document.createElement("div");
  body.className = "history-body";

  if (page === null) {
    const err = document.createElement("p");
    err.className = "history-error";
    err.textContent = "Audit log could not be read. See the app log for details.";
    body.appendChild(err);
  } else if (page.records.length === 0) {
    const empty = document.createElement("p");
    empty.className = "history-empty";
    empty.textContent =
      "No audit entries yet. Every move / add / delete you make from here will appear in this list.";
    body.appendChild(empty);
  } else {
    body.appendChild(historyList(page.records));
    if (page.skipped > 0) {
      const warn = document.createElement("p");
      warn.className = "history-skipped";
      warn.textContent = `${page.skipped} unreadable ${
        page.skipped === 1 ? "entry was" : "entries were"
      } skipped — see the app log for details.`;
      body.appendChild(warn);
    }
  }

  openModal({
    titleText: "History",
    body,
    actions: [
      {
        label: "Close",
        className: "btn-apply",
        focus: true,
        activate: (close) => close(),
      },
    ],
    panelClassName: "modal-history",
    trigger,
  });
}

/**
 * Build the reverse-chronological list of audit entries. The records
 * arrive in file order from the IPC (which is append order = ULID-sorted
 * = chronological); flipping here keeps "most recent first" — the most
 * useful ordering for a "what just happened?" view — without paying for
 * a backend sort.
 */
function historyList(records: AuditRecordView[]): HTMLElement {
  const ul = document.createElement("ul");
  ul.className = "history-list";
  // Slice before reverse so we don't mutate the caller's array — the
  // History dialog is intentionally side-effect-free.
  for (const rec of records.slice().reverse()) {
    ul.appendChild(historyRow(rec));
  }
  return ul;
}

function historyRow(rec: AuditRecordView): HTMLElement {
  const li = document.createElement("li");
  li.className = "history-row";

  const head = document.createElement("div");
  head.className = "history-row-head";

  const verb = document.createElement("span");
  verb.className = `history-verb history-verb-${rec.kind}`;
  verb.textContent = historyVerbLabel(rec);
  head.appendChild(verb);

  const ts = document.createElement("time");
  ts.className = "history-ts";
  // Pin to ISO + locale: the ISO form goes in `datetime` so assistive
  // tech reads the unambiguous absolute time, while the visible text is
  // the locale-formatted version a user can scan. Avoids the "is that
  // 06/05 May or June?" ambiguity that any pure-date display invites.
  const date = new Date(rec.ts_ms);
  ts.dateTime = date.toISOString();
  ts.textContent = date.toLocaleString();
  head.appendChild(ts);

  li.appendChild(head);

  const detail = document.createElement("div");
  detail.className = "history-row-detail";
  detail.appendChild(historyScopeArrow(rec));
  const rule = historyRuleSummary(rec);
  if (rule) detail.appendChild(rule);
  li.appendChild(detail);

  if (rec.project_dir) {
    const proj = document.createElement("div");
    proj.className = "history-project";
    proj.textContent = `Project: ${rec.project_dir}`;
    li.appendChild(proj);
  }

  return li;
}

/**
 * Human-readable verb label for a record. Composes
 * (`kind`, `leaf_kind`, `to_kind`) into one phrase so the History row
 * reads as "Move permission rule" / "Delete top-level key" /
 * "Change kind to deny" instead of forcing the user to reconcile two
 * fields visually.
 */
function historyVerbLabel(rec: AuditRecordView): string {
  if (rec.kind === "change_kind") {
    // Same-scope reclassification: name the destination kind explicitly
    // so the user sees the "what changed" at a glance.
    return rec.to_kind ? `Change kind → ${rec.to_kind}` : "Change kind";
  }
  const noun = historyLeafNoun(rec.leaf_kind);
  switch (rec.kind) {
    case "move":
      return `Move ${noun}`;
    case "add":
      return `Add ${noun}`;
    case "delete":
      return `Delete ${noun}`;
  }
}

function historyLeafNoun(leafKind: AuditRecordView["leaf_kind"]): string {
  switch (leafKind) {
    case "permission_rule":
      return "permission rule";
    case "permission_list":
      return "permission list";
    case "top_level_key":
      return "top-level key";
  }
}

/** "Project → User" arrow for moves, single-scope label for adds/deletes. */
function historyScopeArrow(rec: AuditRecordView): HTMLElement {
  const span = document.createElement("span");
  span.className = "history-scopes";
  const from = rec.from?.scope;
  const to = rec.to?.scope;
  if (from && to && from !== to) {
    span.textContent = `${scopeLabel(from)} → ${scopeLabel(to)}`;
  } else if (from) {
    span.textContent = scopeLabel(from);
  } else if (to) {
    span.textContent = scopeLabel(to);
  } else {
    span.textContent = "—";
  }
  return span;
}

function scopeLabel(scope: Scope): string {
  switch (scope) {
    case "user":
      return "User";
    case "user_local":
      return "User-Local";
    case "project":
      return "Project";
    case "local":
      return "Local";
  }
}

/**
 * Extract the rule string (or key name) the op touched, by diffing the
 * before/after snapshots. For a permission-rule op, the rule appears in
 * either `from.key_before − from.key_after` (move/delete) or
 * `to.key_after − to.key_before` (add). For a top-level key op, the
 * `path[0]` segment IS the key name and a snapshot diff isn't needed.
 *
 * Falls back to `null` (no element appended) when the diff can't
 * identify a single rule — better to render an unannotated row than to
 * lie about what changed.
 */
function historyRuleSummary(rec: AuditRecordView): HTMLElement | null {
  if (rec.leaf_kind === "top_level_key") {
    const key = typeof rec.path[0] === "string" ? rec.path[0] : null;
    if (!key) return null;
    return makeRuleSpan(key);
  }
  // Permission rule or list. For a list op, the path[1] is the kind
  // (allow/deny/ask) and that's the most informative summary.
  if (rec.leaf_kind === "permission_list") {
    const kind = typeof rec.path[1] === "string" ? rec.path[1] : null;
    return kind ? makeRuleSpan(`permissions.${kind}`) : null;
  }
  // PermissionRule: diff the snapshots to recover the rule string.
  const rule = extractRuleFromDiff(rec);
  return rule ? makeRuleSpan(rule) : null;
}

function makeRuleSpan(text: string): HTMLElement {
  const span = document.createElement("span");
  span.className = "history-rule";
  span.textContent = text;
  return span;
}

/**
 * Diff the before/after rule arrays on the side that contains the
 * change, returning the single rule string the op acted on. Picks the
 * side based on op kind:
 *
 *   - Move / ChangeKind: source side loses the rule, so before − after
 *     on `from` is authoritative.
 *   - Add: destination gains the rule; after − before on `to`.
 *   - Delete: source loses the rule; before − after on `from`.
 *
 * Returns the first matching string; if the diff yields multiple (a
 * batch op the schema doesn't yet model), this still surfaces a
 * representative rule rather than nothing.
 */
function extractRuleFromDiff(rec: AuditRecordView): string | null {
  const kindArg = typeof rec.path[1] === "string" ? rec.path[1] : null;
  if (!kindArg) return null;
  if (rec.kind === "add") {
    const before = rulesForKind(rec.to, kindArg);
    const after = rulesForKind(rec.to, kindArg, true);
    return firstNewIn(after, before);
  }
  // Move / Delete / ChangeKind: rule disappears from the source side.
  const before = rulesForKind(rec.from, kindArg);
  const after = rulesForKind(rec.from, kindArg, true);
  return firstNewIn(before, after);
}

/**
 * Pull the `permissions.<kind>` array off a side's snapshot. `useAfter`
 * picks `key_after` vs `key_before` — kept as a flag rather than passing
 * the right field directly so the caller's diff logic reads top-to-bottom
 * without branching on which side.
 */
function rulesForKind(side: AuditSide | undefined, kind: string, useAfter = false): string[] {
  if (!side) return [];
  const snapshot = useAfter ? side.key_after : side.key_before;
  if (!snapshot || typeof snapshot !== "object" || Array.isArray(snapshot)) return [];
  const arr = (snapshot as { [key: string]: JsonValue })[kind];
  if (!Array.isArray(arr)) return [];
  return arr.filter((v): v is string => typeof v === "string");
}

/** First element of `a` that doesn't appear in `b`, or null. */
function firstNewIn(a: string[], b: string[]): string | null {
  const bSet = new Set(b);
  for (const v of a) {
    if (!bSet.has(v)) return v;
  }
  return null;
}
