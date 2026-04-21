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
  Scope,
  ScopeView,
} from "./types.ts";
import { SCOPES, SEARCH_INPUT_ID } from "./types.ts";

interface AppProps {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  query: string;
  onPickProject: () => void;
  onReload: () => void;
  onMove: (req: MoveRequest, trigger?: HTMLElement) => void;
  onMoveKey: (req: MoveKeyRequest, trigger?: HTMLElement) => void;
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
  // effectivePanel push this down into every filter call.
  const lowerQuery = props.query.toLowerCase();
  root.appendChild(effectivePanel(props.scopes, props.query, lowerQuery));
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

function effectivePanel(loaded: LoadedScopes, query: string, lowerQuery: string): HTMLElement {
  const panel = document.createElement("section");
  panel.className = "effective";
  const title = document.createElement("h2");
  title.textContent = "Effective permissions";
  panel.appendChild(title);

  const kinds: PermissionKind[] = ["allow", "deny", "ask"];
  for (const kind of kinds) {
    const all = loaded.effective_permissions[kind];
    // Fast path when the filter is empty — no allocation, no iteration.
    const matched = lowerQuery === "" ? all : all.filter((r) => matchesLoweredQuery(r, lowerQuery));
    const group = document.createElement("div");
    group.className = `eff-group eff-${kind}`;
    const label = document.createElement("span");
    label.className = "eff-label";
    // Show matched/total when a filter is active AND the group isn't empty —
    // otherwise "(0/0)" reads as noise. Unfiltered groups and empty groups
    // fall back to the plain "(N)" format.
    label.textContent =
      query === "" || all.length === 0
        ? `${KIND_LABELS[kind]} (${all.length})`
        : `${KIND_LABELS[kind]} (${matched.length}/${all.length})`;
    group.appendChild(label);
    for (const rule of matched) {
      // Chip + optional badge wrap as a single flex item. Without the
      // wrapper, the badge can break onto a new line without its chip
      // because `.eff-group` uses `flex-wrap: wrap`, and it would then
      // be ambiguous which rule the warning belongs to.
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
    panel.appendChild(group);
  }
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
  btn.setAttribute("aria-label", "Rule warning");
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
  pop.textContent = reason;

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
    const view = loaded.scopes.find((s) => s.scope === scope);
    if (!view) continue;
    grid.appendChild(scopeColumn(view, props, lowerQuery));
  }
  return grid;
}

function scopeColumn(view: ScopeView, props: AppProps, lowerQuery: string): HTMLElement {
  const col = document.createElement("div");
  col.className = "col";

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
  row.appendChild(code);

  const badge = lintBadge(rule);
  if (badge) row.appendChild(badge);

  const moveBtns = document.createElement("div");
  moveBtns.className = "rule-moves";
  for (const target of SCOPES) {
    if (target === scope) continue;
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
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";

    const titleId = `modal-title-${++modalIdCounter}`;

    const panel = document.createElement("div");
    panel.className = "modal";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    panel.setAttribute("aria-labelledby", titleId);

    const title = document.createElement("h2");
    title.id = titleId;
    title.className = "modal-title";
    title.textContent = opts.titleText;
    panel.appendChild(title);

    const subtitle = document.createElement("div");
    subtitle.className = "modal-subtitle";
    subtitle.appendChild(opts.subtitle);
    panel.appendChild(subtitle);

    panel.appendChild(opts.body);

    const actions = document.createElement("div");
    actions.className = "modal-actions";
    const cancel = document.createElement("button");
    cancel.textContent = "Cancel";
    cancel.className = "btn-cancel";
    const apply = document.createElement("button");
    apply.textContent = "Apply";
    apply.className = "btn-apply";
    actions.append(cancel, apply);
    panel.appendChild(actions);

    backdrop.appendChild(panel);

    const close = (result: boolean) => {
      document.removeEventListener("keydown", onKey);
      backdrop.remove();
      if (opts.trigger && document.body.contains(opts.trigger)) {
        opts.trigger.focus();
      }
      resolve(result);
    };
    const focusableSelector =
      'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close(false);
        return;
      }
      if (e.key === "Enter") {
        // Only treat Enter as "apply" when it isn't already activating a
        // focused button — otherwise the button's own click handler fires.
        if (!(document.activeElement instanceof HTMLButtonElement)) {
          e.preventDefault();
          close(true);
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
      if (e.target === backdrop) close(false);
    });
    cancel.addEventListener("click", () => close(false));
    apply.addEventListener("click", () => close(true));

    document.body.appendChild(backdrop);
    apply.focus();
  });
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
