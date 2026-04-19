import type {
  LoadedScopes,
  MovePreview,
  MoveRequest,
  MoveSide,
  PermissionKind,
  Scope,
  ScopeView,
} from "./types.ts";
import { SCOPES, SEARCH_INPUT_ID } from "./types.ts";
import { lintRule } from "./lint.ts";

interface AppProps {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  query: string;
  onPickProject: () => void;
  onReload: () => void;
  onMove: (req: MoveRequest, trigger?: HTMLElement) => void;
  onQueryChange: (next: string) => void;
}

function matchesLoweredQuery(rule: string, lowerQuery: string): boolean {
  if (lowerQuery === "") return true;
  return rule.toLowerCase().includes(lowerQuery);
}

const SCOPE_LABELS: Record<Scope, string> = {
  local: "Local",
  project: "Project",
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
  const caret =
    preserveSearchFocus
      ? { start: active.selectionStart, end: active.selectionEnd }
      : null;

  root.innerHTML = "";
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
  title.innerHTML = "<strong>ClaudeScope</strong><span class='subtitle'>Promote Claude Code settings between scopes</span>";
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

function effectivePanel(
  loaded: LoadedScopes,
  query: string,
  lowerQuery: string,
): HTMLElement {
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
      const chip = document.createElement("code");
      chip.className = "chip";
      chip.textContent = rule;
      group.appendChild(chip);
      const badge = lintBadge(rule);
      if (badge) group.appendChild(badge);
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
 */
function lintBadge(rule: string): HTMLElement | null {
  const result = lintRule(rule);
  if (result.ok) return null;
  const warn = document.createElement("span");
  warn.className = "lint-warn";
  warn.textContent = "⚠";
  warn.setAttribute("role", "img");
  const reason = result.reason ?? "Rule shape not recognized.";
  warn.setAttribute("aria-label", `Rule warning: ${reason}`);
  warn.title = reason;
  return warn;
}

function scopeGrid(props: AppProps, lowerQuery: string): HTMLElement {
  const grid = document.createElement("section");
  grid.className = "grid";
  for (const scope of SCOPES) {
    const view = props.scopes!.scopes.find((s) => s.scope === scope)!;
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
      view.permissions.allow.length +
      view.permissions.deny.length +
      view.permissions.ask.length;
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
    const matched =
      isFiltering ? rules.filter((r) => matchesLoweredQuery(r, lowerQuery)) : rules;
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

  if (view.other_keys.length > 0) {
    const other = document.createElement("div");
    other.className = "other-keys";
    other.textContent = `Other keys: ${view.other_keys.join(", ")}`;
    col.appendChild(other);
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
      props.onMove(
        { rule, kind, from: scope, to: target },
        e.currentTarget as HTMLElement,
      );
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
export function confirmMove(
  preview: MovePreview,
  trigger?: HTMLElement | null,
): Promise<boolean> {
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
    title.textContent = `Move ${KIND_LABELS[preview.kind]} rule`;
    panel.appendChild(title);

    const subtitle = document.createElement("div");
    subtitle.className = "modal-subtitle";
    const ruleCode = document.createElement("code");
    ruleCode.className = "chip";
    ruleCode.textContent = preview.rule;
    subtitle.appendChild(ruleCode);
    subtitle.appendChild(document.createTextNode(` from ${SCOPE_LABELS[preview.from.scope]} to ${SCOPE_LABELS[preview.to.scope]}`));
    panel.appendChild(subtitle);

    const diff = document.createElement("div");
    diff.className = "modal-diff";
    diff.appendChild(diffSide(preview.from, "remove", preview.rule));
    diff.appendChild(diffSide(preview.to, "add", preview.rule));
    panel.appendChild(diff);

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
      if (trigger && document.body.contains(trigger)) {
        trigger.focus();
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
        const focusables = Array.from(
          panel.querySelectorAll<HTMLElement>(focusableSelector),
        );
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
