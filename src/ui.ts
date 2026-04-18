import type { LoadedScopes, MoveRequest, PermissionKind, Scope, ScopeView } from "./types.ts";
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
    btn.disabled = props.busy;
    btn.onclick = () => props.onMove({ rule, kind, from: scope, to: target });
    moveBtns.appendChild(btn);
  }
  row.appendChild(moveBtns);

  return row;
}
