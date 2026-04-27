import { lintRule } from "./lint.ts";
import type {
  JsonValue,
  LoadedScopes,
  MoveKeyPreview,
  MoveKeyRequest,
  MoveKeySide,
  MovePreview,
  MoveRequest,
  MoveSide,
  PermissionKind,
  Preferences,
  Scope,
  ScopeView,
} from "./types.ts";
import { SCOPES, SEARCH_INPUT_ID } from "./types.ts";

interface AppProps {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  query: string;
  preferences: Preferences;
  onPickProject: () => void;
  onReload: () => void;
  onMove: (req: MoveRequest, trigger?: HTMLElement) => void;
  onMoveKey: (req: MoveKeyRequest, trigger?: HTMLElement) => void;
  onOpenSettings: (trigger?: HTMLElement) => void;
  onQueryChange: (next: string) => void;
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
    // Fast path when the filter is empty — no allocation, no iteration.
    const matched = lowerQuery === "" ? all : all.filter((r) => matchesLoweredQuery(r, lowerQuery));
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
        : `${KIND_LABELS[kind]} (${matched.length}/${all.length})`;
    group.appendChild(label);
    for (const rule of matched) {
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
      chipWrap.appendChild(chip);
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

// HTML5 drag-and-drop source state. Set by `dragstart` on a chip or
// top-level tree key, cleared by `dragend` (or by renderApp on a re-render
// that tears down the source mid-drag). Lives at module scope because
// `dragover` on drop targets needs to read the source scope without a
// closure over the source element, and the lifecycle is one short user
// gesture — same shape as `openTreeNodes` / `lastRenderedProjectDir`.
type DragSource =
  | { kind: "rule"; rule: string; ruleKind: PermissionKind; from: Scope; el: HTMLElement }
  | { kind: "key"; key: string; from: Scope; el: HTMLElement };
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

function setupRuleDragSource(
  el: HTMLElement,
  rule: string,
  kind: PermissionKind,
  scope: Scope,
): void {
  el.draggable = true;
  el.addEventListener("dragstart", (e) => {
    dragSource = { kind: "rule", rule, ruleKind: kind, from: scope, el };
    if (e.dataTransfer) {
      // Custom MIME type used in dragover to reject foreign drags from other
      // apps or browser tabs before checking dragSource. The payload lives in
      // dragSource; the MIME value is just a discriminator.
      e.dataTransfer.setData("application/x-claude-scope-move", "rule");
      e.dataTransfer.effectAllowed = "move";
    }
  });
  el.addEventListener("dragend", clearDragState);
}

function setupKeyDragSource(el: HTMLElement, scope: Scope, key: string): void {
  el.draggable = true;
  el.addEventListener("dragstart", (e) => {
    dragSource = { kind: "key", key, from: scope, el };
    if (e.dataTransfer) {
      e.dataTransfer.setData("application/x-claude-scope-move", "key");
      e.dataTransfer.effectAllowed = "move";
    }
  });
  el.addEventListener("dragend", clearDragState);
}

function treeKey(scope: Scope, path: (string | number)[]): string {
  return `${scope}:${JSON.stringify(path)}`;
}

function treeNode(
  scope: Scope,
  path: (string | number)[],
  label: string,
  value: JsonValue,
  props?: AppProps,
): HTMLElement {
  if (value !== null && typeof value === "object") {
    return treeBranch(scope, path, label, value, props);
  }
  return treeLeaf(scope, path, label, value, props);
}

function treeBranch(
  scope: Scope,
  path: (string | number)[],
  label: string,
  value: JsonValue[] | { [key: string]: JsonValue },
  props: AppProps | undefined,
): HTMLElement {
  const details = document.createElement("details");
  details.className = "tree-node tree-branch";
  const key = treeKey(scope, path);

  const summary = document.createElement("summary");
  summary.className = "tree-summary";
  const name = document.createElement("span");
  name.className = "tree-key";
  name.textContent = label;
  // Make the key span (not the whole <summary>) the drag source for whole
  // top-level keys: starting a drag on the inner span lets the browser
  // suppress the `<details>` toggle that would otherwise fire on click,
  // and isolates the affordance from nested key labels which never become
  // drag sources.
  if (path.length === 1 && props && !props.busy) {
    setupKeyDragSource(name, scope, String(path[0]));
  }
  summary.appendChild(name);
  const peek = document.createElement("span");
  peek.className = "tree-peek";
  peek.textContent = Array.isArray(value) ? `[${value.length}]` : `{${Object.keys(value).length}}`;
  summary.appendChild(peek);
  // Move-target buttons only make sense for whole top-level keys; nested
  // subtree moves aren't in scope for this PR. props is only provided at
  // the top level, which keeps the guard implicit and cheap.
  if (path.length === 1 && props) {
    summary.appendChild(keyMoveButtons(scope, String(path[0]), props));
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
        children.appendChild(treeNode(scope, [...path, i], `[${i}]`, child));
      });
    } else {
      for (const [k, v] of Object.entries(value)) {
        children.appendChild(treeNode(scope, [...path, k], k, v));
      }
    }
  }

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
  path: (string | number)[],
  label: string,
  value: JsonValue,
  props?: AppProps,
): HTMLElement {
  const row = document.createElement("div");
  row.className = "tree-node tree-leaf";
  const name = document.createElement("span");
  name.className = "tree-key";
  name.textContent = label;
  if (path.length === 1 && props && !props.busy) {
    setupKeyDragSource(name, scope, String(path[0]));
  }
  row.appendChild(name);
  const val = document.createElement("span");
  val.className = `tree-value tree-value-${leafType(value)}`;
  val.textContent = formatLeaf(value);
  row.appendChild(val);
  if (path.length === 1 && props) {
    row.appendChild(keyMoveButtons(scope, String(path[0]), props));
  }
  return row;
}

function keyMoveButtons(scope: Scope, key: string, props: AppProps): HTMLElement {
  const moveBtns = document.createElement("div");
  moveBtns.className = "rule-moves tree-key-moves";
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
      `Move settings key ${key} from ${SCOPE_LABELS[scope]} to ${SCOPE_LABELS[target]}`,
    );
    btn.disabled = props.busy;
    btn.addEventListener("click", (e) => {
      // Clicks on the summary element would otherwise toggle the <details>;
      // the move action is a distinct intent, so swallow propagation.
      e.preventDefault();
      e.stopPropagation();
      props.onMoveKey({ key, from: scope, to: target }, e.currentTarget as HTMLElement);
    });
    moveBtns.appendChild(btn);
  }
  return moveBtns;
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
    // Clear the singleton before dispatching the move — onMove can open a
    // modal synchronously, and we don't want a stale `dragSource` lingering
    // through the user's confirm interaction.
    dragSource = null;
    const trigger = src.el.isConnected ? src.el : undefined;
    if (src.kind === "rule") {
      props.onMove({ rule: src.rule, kind: src.ruleKind, from: src.from, to: view.scope }, trigger);
    } else {
      props.onMoveKey({ key: src.key, from: src.from, to: view.scope }, trigger);
    }
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
    const totals =
      view.permissions.allow.length + view.permissions.deny.length + view.permissions.ask.length;
    status.textContent = `${totals} permission rule${totals === 1 ? "" : "s"}`;
  }
  head.appendChild(status);
  col.appendChild(head);

  const kinds: PermissionKind[] = ["allow", "deny", "ask"];
  const isFiltering = lowerQuery !== "";
  // Compute all the groups up front so we can decide between the per-kind
  // view and the column-level "no matches" placeholder without re-filtering.
  const groups = kinds.map((kind) => {
    const rules = view.permissions[kind];
    const matched = isFiltering ? rules.filter((r) => matchesLoweredQuery(r, lowerQuery)) : rules;
    return { kind, rules, matched };
  });
  const totalAll = groups.reduce((s, g) => s + g.rules.length, 0);
  const totalMatched = groups.reduce((s, g) => s + g.matched.length, 0);

  if (isFiltering && totalAll > 0 && totalMatched === 0) {
    // Nothing matched anywhere in this scope — replace the group headers with
    // a single column-level placeholder so the user doesn't see three empty
    // "(0/N)" headers stacked on top of each other.
    const none = document.createElement("div");
    none.className = "col-no-matches";
    none.textContent = `No rules match “${props.query}”.`;
    col.appendChild(none);
  } else {
    for (const { kind, rules, matched } of groups) {
      // Skip empty kinds outright. When filtering, kinds that exist but
      // have 0 matches still render a header so the m/n count makes the
      // hidden rules visible to the user.
      if (rules.length === 0) continue;
      const section = document.createElement("div");
      section.className = `rule-group rule-${kind}`;
      const label = document.createElement("h4");
      label.textContent = isFiltering
        ? `${KIND_LABELS[kind]} (${matched.length}/${rules.length})`
        : `${KIND_LABELS[kind]} (${rules.length})`;
      section.appendChild(label);
      for (const rule of matched) {
        section.appendChild(ruleRow(view.scope, kind, rule, props));
      }
      col.appendChild(section);
    }
  }

  const otherKeys = Object.keys(view.other_values);
  if (otherKeys.length > 0) {
    const tree = document.createElement("div");
    tree.className = "other-tree";
    const heading = document.createElement("h4");
    heading.className = "other-tree-heading";
    heading.textContent = "Other settings";
    tree.appendChild(heading);
    for (const key of otherKeys) {
      tree.appendChild(treeNode(view.scope, [key], key, view.other_values[key], props));
    }
    col.appendChild(tree);
  }

  return col;
}

function ruleRow(scope: Scope, kind: PermissionKind, rule: string, props: AppProps): HTMLElement {
  const row = document.createElement("div");
  row.className = "rule";

  const code = document.createElement("code");
  code.className = "rule-text";
  code.textContent = rule;
  // Drag affordance lives on the chip itself, not the row, so the move
  // buttons stay clickable and the row keeps its hover semantics intact.
  // Disabled while a write is in flight, mirroring how `move-btn` is
  // gated on `props.busy` further down.
  if (!props.busy) {
    setupRuleDragSource(code, rule, kind, scope);
  }
  row.appendChild(code);

  const badge = lintBadge(rule);
  if (badge) row.appendChild(badge);

  const moveBtns = document.createElement("div");
  moveBtns.className = "rule-moves";
  for (const target of SCOPES) {
    if (target === scope) continue;
    // Same reasoning as `keyMoveButtons`: hidden columns can't be move
    // targets, since the result would be immediately invisible.
    if (!isScopeVisible(target)) continue;
    const btn = document.createElement("button");
    btn.className = "move-btn";
    btn.textContent = `→ ${SCOPE_LABELS[target]}`;
    btn.setAttribute(
      "aria-label",
      `Move ${KIND_LABELS[kind]} rule ${rule} from ${SCOPE_LABELS[scope]} to ${SCOPE_LABELS[target]}`,
    );
    btn.disabled = props.busy;
    btn.onclick = (e) =>
      props.onMove({ rule, kind, from: scope, to: target }, e.currentTarget as HTMLElement);
    moveBtns.appendChild(btn);
  }
  row.appendChild(moveBtns);

  return row;
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
export function confirmMove(preview: MovePreview, trigger?: HTMLElement | null): Promise<boolean> {
  const subtitle = document.createDocumentFragment();
  const ruleCode = document.createElement("code");
  ruleCode.className = "chip";
  ruleCode.textContent = preview.rule;
  subtitle.appendChild(ruleCode);
  subtitle.appendChild(
    document.createTextNode(
      ` from ${SCOPE_LABELS[preview.from.scope]} to ${SCOPE_LABELS[preview.to.scope]}`,
    ),
  );

  const diff = document.createElement("div");
  diff.className = "modal-diff";
  diff.appendChild(diffSide(preview.from, "remove", preview.rule));
  diff.appendChild(diffSide(preview.to, "add", preview.rule));

  return openConfirmModal({
    titleText: `Move ${KIND_LABELS[preview.kind]} rule`,
    subtitle,
    body: diff,
    trigger,
  });
}

export function confirmMoveKey(
  preview: MoveKeyPreview,
  trigger?: HTMLElement | null,
): Promise<boolean> {
  const subtitle = document.createDocumentFragment();
  const keyCode = document.createElement("code");
  keyCode.className = "chip";
  keyCode.textContent = preview.key;
  subtitle.appendChild(keyCode);
  subtitle.appendChild(
    document.createTextNode(
      ` from ${SCOPE_LABELS[preview.from.scope]} to ${SCOPE_LABELS[preview.to.scope]}`,
    ),
  );

  const diff = document.createElement("div");
  diff.className = "modal-diff";
  diff.appendChild(keyDiffSide(preview.from, "remove"));
  diff.appendChild(keyDiffSide(preview.to, "add"));

  return openConfirmModal({
    titleText: "Move settings key",
    subtitle,
    body: diff,
    trigger,
  });
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
 * The goal is to keep `confirmMove` / `confirmMoveKey` / `openSettings`
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

function keyDiffSide(side: MoveKeySide, mode: "add" | "remove"): HTMLElement {
  const col = document.createElement("div");
  col.className = `modal-side modal-side-${mode}`;

  const head = document.createElement("div");
  head.className = "modal-side-head";
  const label = document.createElement("h3");
  label.textContent = SCOPE_LABELS[side.scope];
  head.appendChild(label);

  const path = document.createElement("div");
  path.className = "modal-side-path";
  path.textContent = side.path;
  head.appendChild(path);

  const verdict = document.createElement("div");
  verdict.className = "modal-side-verdict";
  if (!side.will_write) {
    verdict.textContent = "(no change)";
    verdict.classList.add("muted");
  } else if (mode === "remove") {
    verdict.textContent = "key removed";
    verdict.classList.add("removed");
  } else {
    // `undefined` means the backend skipped the field because the key was
    // absent; a present-but-null value arrives as `null` and counts as a
    // real existing value to merge against.
    verdict.textContent = side.value_before === undefined ? "key added" : "key merged";
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

  const body = document.createElement("div");
  body.className = "modal-side-value";
  const pre = document.createElement("pre");
  pre.className = "modal-side-json";
  pre.textContent = formatValue(mode === "remove" ? side.value_before : side.value_after);
  body.appendChild(pre);
  col.appendChild(body);

  return col;
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
}

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
  openModal({
    titleText: "Settings",
    body: settingsColumnsSection(props),
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

function diffSide(side: MoveSide, mode: "add" | "remove", movingRule: string): HTMLElement {
  const col = document.createElement("div");
  col.className = `modal-side modal-side-${mode}`;

  const head = document.createElement("div");
  head.className = "modal-side-head";
  const label = document.createElement("h3");
  label.textContent = SCOPE_LABELS[side.scope];
  head.appendChild(label);

  const path = document.createElement("div");
  path.className = "modal-side-path";
  path.textContent = side.path;
  head.appendChild(path);

  // Derive the verdict from the actual list lengths so duplicates (the
  // backend removes *every* occurrence) and any future changes to the move
  // semantics stay in sync with what the modal claims will happen.
  const delta = side.rules_after.length - side.rules_before.length;
  const verdict = document.createElement("div");
  verdict.className = "modal-side-verdict";
  if (delta === 0) {
    verdict.textContent = "(no change)";
    verdict.classList.add("muted");
  } else if (delta > 0) {
    verdict.textContent = `+${delta} rule${delta === 1 ? "" : "s"}`;
    verdict.classList.add("added");
  } else {
    const n = -delta;
    verdict.textContent = `−${n} rule${n === 1 ? "" : "s"}`;
    verdict.classList.add("removed");
  }
  head.appendChild(verdict);
  col.appendChild(head);

  if (side.note) {
    const note = document.createElement("div");
    note.className = "modal-side-note";
    note.textContent = side.note;
    col.appendChild(note);
  }

  // Remove side shows the pre-move list with the moved rule struck through
  // (diff context, not the post-write contents — apply_move will actually
  // persist rules_after there). Add side shows the backend's rules_after so
  // the dest column mirrors exactly what will be written.
  const rules = mode === "remove" ? side.rules_before : side.rules_after;
  const list = document.createElement("ul");
  list.className = "modal-diff-list";
  for (const rule of rules) {
    const li = document.createElement("li");
    const code = document.createElement("code");
    code.textContent = rule;
    if (rule === movingRule) {
      if (mode === "remove") {
        li.className = "diff-removed";
      } else if (side.will_write) {
        // will_write=false means the rule was already present on the dest;
        // render it neutrally so it doesn't look like a fresh addition.
        li.className = "diff-added";
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
  col.appendChild(list);

  return col;
}
