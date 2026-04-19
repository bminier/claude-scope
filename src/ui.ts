import type {
  LoadedScopes,
  MovePreview,
  MoveRequest,
  MoveSide,
  PermissionKind,
  Scope,
  ScopeView,
} from "./types.ts";
import { SCOPES } from "./types.ts";

interface AppProps {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  onPickProject: () => void;
  onReload: () => void;
  onMove: (req: MoveRequest) => void;
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

export function renderApp(root: HTMLElement, props: AppProps): void {
  root.innerHTML = "";
  root.appendChild(header(props));

  if (!props.scopes) {
    const empty = document.createElement("div");
    empty.className = "empty";
    empty.textContent = props.busy ? "Loading…" : "No settings loaded.";
    root.appendChild(empty);
    return;
  }

  root.appendChild(effectivePanel(props.scopes));
  root.appendChild(scopeGrid(props));
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

function effectivePanel(loaded: LoadedScopes): HTMLElement {
  const panel = document.createElement("section");
  panel.className = "effective";
  const title = document.createElement("h2");
  title.textContent = "Effective permissions";
  panel.appendChild(title);

  const kinds: PermissionKind[] = ["allow", "deny", "ask"];
  for (const kind of kinds) {
    const group = document.createElement("div");
    group.className = `eff-group eff-${kind}`;
    const label = document.createElement("span");
    label.className = "eff-label";
    label.textContent = `${KIND_LABELS[kind]} (${loaded.effective_permissions[kind].length})`;
    group.appendChild(label);
    for (const rule of loaded.effective_permissions[kind]) {
      const chip = document.createElement("code");
      chip.className = "chip";
      chip.textContent = rule;
      group.appendChild(chip);
    }
    panel.appendChild(group);
  }
  return panel;
}

function scopeGrid(props: AppProps): HTMLElement {
  const grid = document.createElement("section");
  grid.className = "grid";
  for (const scope of SCOPES) {
    const view = props.scopes!.scopes.find((s) => s.scope === scope)!;
    grid.appendChild(scopeColumn(view, props));
  }
  return grid;
}

function scopeColumn(view: ScopeView, props: AppProps): HTMLElement {
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
  for (const kind of kinds) {
    const rules = view.permissions[kind];
    if (rules.length === 0) continue;
    const section = document.createElement("div");
    section.className = `rule-group rule-${kind}`;
    const label = document.createElement("h4");
    label.textContent = `${KIND_LABELS[kind]} (${rules.length})`;
    section.appendChild(label);
    for (const rule of rules) {
      section.appendChild(ruleRow(view.scope, kind, rule, props));
    }
    col.appendChild(section);
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
    btn.onclick = () => props.onMove({ rule, kind, from: scope, to: target });
    moveBtns.appendChild(btn);
  }
  row.appendChild(moveBtns);

  return row;
}

/**
 * Show a modal diff confirm and resolve to whether the user applied the move.
 * Escape cancels, Enter applies.
 */
export function confirmMove(preview: MovePreview): Promise<boolean> {
  return new Promise((resolve) => {
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";

    const panel = document.createElement("div");
    panel.className = "modal";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    panel.setAttribute("aria-labelledby", "modal-title");

    const title = document.createElement("h2");
    title.id = "modal-title";
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

    const previouslyFocused = document.activeElement as HTMLElement | null;

    const close = (result: boolean) => {
      document.removeEventListener("keydown", onKey);
      backdrop.remove();
      previouslyFocused?.focus?.();
      resolve(result);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close(false);
      } else if (e.key === "Enter") {
        e.preventDefault();
        close(true);
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

  const verdict = document.createElement("div");
  verdict.className = "modal-side-verdict";
  if (!side.will_write) {
    verdict.textContent = "(no change)";
    verdict.classList.add("muted");
  } else if (mode === "add") {
    verdict.textContent = "+1 rule";
    verdict.classList.add("added");
  } else {
    verdict.textContent = "−1 rule";
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

  const list = document.createElement("ul");
  list.className = "modal-diff-list";
  for (const rule of side.rules_before) {
    const li = document.createElement("li");
    const code = document.createElement("code");
    code.textContent = rule;
    if (mode === "remove" && rule === movingRule) {
      li.className = "diff-removed";
    }
    li.appendChild(code);
    list.appendChild(li);
  }
  if (mode === "add" && side.will_write) {
    const li = document.createElement("li");
    li.className = "diff-added";
    const code = document.createElement("code");
    code.textContent = movingRule;
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
