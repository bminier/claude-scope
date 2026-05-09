import { lintRule } from "./lint.ts";
import type {
  JsonValue,
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
import { SCOPES, SEARCH_INPUT_ID } from "./types.ts";

interface AppProps {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  query: string;
  preferences: Preferences;
  runtime: RuntimeInfo;
  onPickProject: () => void;
  onReload: () => void;
  onMoveLeaf: (req: MoveLeafRequest, trigger?: HTMLElement, opts?: MoveOptions) => void;
  onOpenSettings: (trigger?: HTMLElement) => void;
  onQueryChange: (next: string) => void;
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
    if (path.length === 3 && typeof path[1] === "string" && typeof path[2] === "number") {
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
  // The full DOM wipe just destroyed any pinned lint popover; clear the
  // tracking state so a stale `openLintWrap` doesn't survive re-render and
  // confuse the next outside-click / Escape.
  closeLintPopover();
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
    lastRenderedProjectDir = props.projectDir;
  }
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

  // Lowercase the query once per render instead of per rule; scopeGrid/
  // combinedPanel push this down into every filter call.
  const lowerQuery = props.query.toLowerCase();
  root.appendChild(combinedPanel(props.scopes, props.query, lowerQuery));
  root.appendChild(scopeGrid(props, lowerQuery));
  restoreSearchFocus(preserveSearchFocus, caret);
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

function header(props: AppProps): HTMLElement {
  const bar = document.createElement("header");
  bar.className = "topbar";

  const title = document.createElement("div");
  title.className = "title";
  title.innerHTML =
    "<strong>ClaudeScope</strong><span class='subtitle'>Promote Claude Code settings between scopes</span>";
  bar.appendChild(title);

  const dir = document.createElement("div");
  dir.className = "project-dir";
  dir.textContent = props.projectDir ? `Project: ${props.projectDir}` : "No project selected";
  bar.appendChild(dir);

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

  const settings = document.createElement("button");
  settings.textContent = "Settings";
  settings.setAttribute("aria-label", "Open settings");
  settings.onclick = (e) => props.onOpenSettings(e.currentTarget as HTMLElement);
  actions.appendChild(settings);

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

function combinedPanel(loaded: LoadedScopes, query: string, lowerQuery: string): HTMLElement {
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
    group.appendChild(label);
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
    if (openLintWrap && openLintWrap !== wrap) closeLintPopover();
    const isOpen = wrap.classList.toggle("is-open");
    if (isOpen) {
      openLintWrap = wrap;
      ensureLintGlobalListeners();
    } else if (openLintWrap === wrap) {
      openLintWrap = null;
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

// Pinned-popover state. Hover/focus reveals are handled purely in CSS; this
// state only tracks popovers that the user clicked to keep open.
let lintPopoverSeq = 0;
let openLintWrap: HTMLElement | null = null;
let lintGlobalListenersAttached = false;

function closeLintPopover(): void {
  if (!openLintWrap) return;
  // The wrap may already be detached (e.g. after a renderApp() rebuild);
  // touching classList is harmless but the state still needs nulling.
  if (openLintWrap.isConnected) openLintWrap.classList.remove("is-open");
  openLintWrap = null;
}

function ensureLintGlobalListeners(): void {
  if (lintGlobalListenersAttached) return;
  lintGlobalListenersAttached = true;
  document.addEventListener("click", (e) => {
    if (!openLintWrap) return;
    if (!openLintWrap.contains(e.target as Node)) closeLintPopover();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || !openLintWrap) return;
    // Skip if another handler already consumed Escape (e.g. the search
    // input clears its value on Escape and calls preventDefault), so the
    // pinned popover doesn't close as a side-effect.
    if (e.defaultPrevented) return;
    // A visible modal owns Escape — otherwise closing a pinned popover
    // here would consume the keystroke and the dialog would stay open.
    if (document.querySelector(".modal-backdrop")) return;
    const btn = openLintWrap.querySelector<HTMLButtonElement>(".lint-warn");
    closeLintPopover();
    // Skip focus restore if the badge was torn down by a re-render between
    // pin and Escape — focusing a detached node is a no-op in some
    // browsers and a stray scroll/focus jump in others.
    if (btn?.isConnected) btn.focus();
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

function clearDragState(): void {
  dragSource = null;
  // Belt-and-suspenders: a drop on a non-target column doesn't fire its
  // own dragleave, so a stale `.col-drop-active` could survive into the
  // next render. Sweep them all here on any drag-state reset.
  for (const el of document.querySelectorAll<HTMLElement>(".col-drop-active")) {
    el.classList.remove("col-drop-active");
  }
}

function setupLeafDragSource(el: HTMLElement, scope: Scope, path: PathSeg[]): void {
  el.draggable = true;
  el.addEventListener("dragstart", (e) => {
    dragSource = { path, from: scope, el };
    if (e.dataTransfer) {
      // Custom MIME type used in dragover to reject foreign drags from other
      // apps or browser tabs before checking dragSource. The payload lives in
      // dragSource; the MIME value is just a discriminator.
      e.dataTransfer.setData("application/x-claude-scope-move", "leaf");
      e.dataTransfer.effectAllowed = "move";
    }
  });
  el.addEventListener("dragend", clearDragState);
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

  const summary = document.createElement("summary");
  summary.className = "tree-summary";
  const name = document.createElement("span");
  name.className = "tree-key";
  name.textContent = label;
  // Make the key span (not the whole <summary>) the drag source for movable
  // branches: starting a drag on the inner span lets the browser suppress
  // the `<details>` toggle that would otherwise fire on click, and isolates
  // the affordance from nested key labels which aren't movable.
  if (props && !props.busy && isMovablePath(path)) {
    setupLeafDragSource(name, scope, path);
  }
  summary.appendChild(name);
  const peek = document.createElement("span");
  peek.className = "tree-peek";
  peek.textContent = treeBranchPeek(path, value, lowerQuery);
  summary.appendChild(peek);
  if (props && isMovablePath(path)) {
    summary.appendChild(leafMoveButtons(scope, path, props));
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
      value.forEach((child, i) => {
        // Permission rule arrays carry their kind via the parent path; child
        // construction passes `props` and `lowerQuery` through so the leaf
        // gets its move buttons + drag + lint badge + filter test.
        children.appendChild(treeNode(scope, [...path, i], `[${i}]`, child, props, lowerQuery));
      });
    } else {
      for (const [k, v] of Object.entries(value)) {
        children.appendChild(treeNode(scope, [...path, k], k, v, props, lowerQuery));
      }
    }
  }

  // Auto-expand permission branches under the unified tree (parent is "permissions")
  // so the user doesn't need to click into the tree to see allow / deny / ask
  // — that was the old single-pane visibility before the migration.
  const isPermissionsChild = path.length === 2 && path[0] === "permissions";
  const shouldAutoOpen =
    openTreeNodes.has(key) ||
    (path.length === 1 && path[0] === "permissions") ||
    isPermissionsChild;
  if (shouldAutoOpen) {
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
      row.appendChild(leafMoveButtons(scope, path, props));
    }
    return row;
  }

  const row = document.createElement("div");
  row.className = "tree-node tree-leaf";
  const name = document.createElement("span");
  name.className = "tree-key";
  name.textContent = label;
  if (props && !props.busy && isMovablePath(path)) {
    setupLeafDragSource(name, scope, path);
  }
  row.appendChild(name);
  const val = document.createElement("span");
  val.className = `tree-value tree-value-${leafType(value)}`;
  val.textContent = formatLeaf(value);
  row.appendChild(val);
  if (props && isMovablePath(path)) {
    row.appendChild(leafMoveButtons(scope, path, props));
  }
  return row;
}

function leafMoveButtons(scope: Scope, path: PathSeg[], props: AppProps): HTMLElement {
  const moveBtns = document.createElement("div");
  moveBtns.className = "rule-moves tree-key-moves";
  const describe = describePath(path);
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
      `Move ${describe} from ${SCOPE_LABELS[scope]} to ${SCOPE_LABELS[target]}`,
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
      let matched = 0;
      for (const item of value) {
        if (typeof item === "string" && matchesLoweredQuery(item, lowerQuery)) matched++;
      }
      return `[${matched}/${value.length}]`;
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

  const head = document.createElement("div");
  head.className = "col-head";
  const h = document.createElement("h3");
  h.textContent = SCOPE_LABELS[view.scope];
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
  const subtitle = document.createDocumentFragment();
  const target = document.createElement("code");
  target.className = "chip";
  target.textContent = leafSubtitleLabel(preview);
  subtitle.appendChild(target);
  subtitle.appendChild(
    document.createTextNode(
      ` from ${SCOPE_LABELS[preview.from.scope]} to ${SCOPE_LABELS[preview.to.scope]}`,
    ),
  );

  const diff = document.createElement("div");
  diff.className = "modal-diff";
  diff.appendChild(leafDiffSide(preview, "remove"));
  diff.appendChild(leafDiffSide(preview, "add"));

  return openConfirmModal({
    titleText: leafModalTitle(preview),
    subtitle,
    body: diff,
    trigger,
  });
}

function leafModalTitle(preview: MoveLeafPreview): string {
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
 * delta between this side's `key_before` / `key_after`, not by membership
 * in the source's list — for a `permission_list` move the destination may
 * already share rules with the source, and those rules are *not* "added"
 * (the array-union skips them). The remove side highlights rules that
 * disappear (`before` minus `after`); the add side highlights rules that
 * appear (`after` minus `before`).
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
  const beforeSet = new Set(itemsBefore);
  const afterSet = new Set(itemsAfter);

  for (const rule of items) {
    const li = document.createElement("li");
    const code = document.createElement("code");
    code.textContent = rule;
    const isDelta = mode === "remove" ? !afterSet.has(rule) : !beforeSet.has(rule);
    if (isDelta) {
      li.className = mode === "remove" ? "diff-removed" : "diff-added";
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

interface SettingsProps {
  preferences: Preferences;
  onToggleScopeVisibility: (scope: Scope, visible: boolean) => void;
  onChangeTheme: (theme: Theme) => void;
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
