import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { homeDir } from "@tauri-apps/api/path";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import "./styles.css";
import { type RestoreFlowDeps, runRestoreFlow } from "./restore-flow.ts";
import type {
  AddLeafPreview,
  AddLeafRequest,
  AppInfo,
  AuditLogPage,
  AuditRecordView,
  DeleteLeafPreview,
  DeleteLeafRequest,
  KnownProject,
  LoadedScopes,
  MoveLeafPreview,
  MoveLeafRequest,
  MoveOptions,
  PathSeg,
  PermissionKind,
  Preferences,
  RestorePreview,
  RuntimeInfo,
  Scope,
  Theme,
  UndoRedoStatus,
} from "./types.ts";
import { SCOPES, SEARCH_INPUT_ID } from "./types.ts";
import {
  confirmAddLeaf,
  confirmDeleteLeaf,
  confirmMoveLeaf,
  confirmRestore,
  openAbout,
  openHistory,
  openSettings,
  renderApp,
} from "./ui.ts";

// Mirror of the Rust `Preferences::default()` — used until the real payload
// arrives from the backend so renders before load_preferences() resolves
// still have something complete to work with. Derived from SCOPES so adding
// a new scope can't leave this list out of sync.
const DEFAULT_PREFERENCES: Preferences = {
  visible_scopes: [...SCOPES],
  theme: "auto",
  backup_on_write: true,
  recent_projects: [],
  audit_log_rotate: true,
  audit_log_max_size_mb: 10,
  group_rules_at: 2,
  combined_panel_collapsed: true,
};

const state: {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  query: string;
  preferences: Preferences;
  runtime: RuntimeInfo;
  knownProjects: KnownProject[];
  appInfo: AppInfo | null;
  undoStatus: UndoRedoStatus | null;
} = {
  scopes: null,
  projectDir: null,
  busy: false,
  query: "",
  preferences: DEFAULT_PREFERENCES,
  // Default to "no overrides" until load_runtime_info resolves; the
  // sandbox banner is simply omitted in that case, so a slow IPC boot
  // doesn't flash a misleading "Sandbox: …" line.
  runtime: { home_override: null, project_override: null },
  // Populated by list_known_projects at bootstrap and refreshed when the
  // user picks a new project (#106). Defaults to empty so the Move-to
  // submenu degrades gracefully before the IPC resolves.
  knownProjects: [],
  // Populated once at bootstrap by `get_app_info` (#21). Stays null on
  // discovery failure; `openAbout` renders a Loading… stub in that case
  // rather than treating it as an error.
  appInfo: null,
  // Undo/redo availability (#124), refreshed by `refreshUndoStatus` after
  // every load. Null until the first fetch resolves — the topbar buttons
  // render disabled in the meantime.
  undoStatus: null,
};

// Set by the scopes-changed listener when it fires while another load or
// move is already in flight. The load that finishes last checks this flag
// in its finally block and kicks off one deferred reload, so we don't miss
// external edits that happen during a load without needing a full job
// queue.
let externalReloadPending = false;

async function load(projectDir: string | null): Promise<void> {
  state.busy = true;
  render();
  try {
    const scopes = await invoke<LoadedScopes>("load_scopes", { project_dir: projectDir });
    state.scopes = scopes;
    state.projectDir = scopes.project_dir;
    // Refresh the known-projects list alongside the scope load so the
    // Move-to submenu reflects projects that appeared since launch (e.g.
    // the user just ran Claude in a new directory). Tolerated failure:
    // an empty list still lets all other UI work.
    refreshKnownProjects();
    // Re-read preferences so the recent-projects dropdown (#47) reflects
    // the LRU update the backend just persisted alongside this load.
    // Fire-and-forget — a stale list is purely cosmetic and self-heals on
    // the next successful load.
    refreshPreferences();
    // Refresh undo/redo availability (#124): every move / add / delete /
    // undo runs through load(), so this one call keeps the topbar
    // buttons current after any write.
    refreshUndoStatus();
  } catch (err) {
    alert(`Failed to load settings: ${err}`);
  } finally {
    state.busy = false;
    render();
    if (externalReloadPending && !moveInFlight) {
      externalReloadPending = false;
      void load(state.projectDir);
    }
  }
}

function refreshKnownProjects(): void {
  invoke<KnownProject[]>("list_known_projects")
    .then((projects) => {
      state.knownProjects = projects;
      render();
    })
    .catch((err) => {
      console.warn("failed to list known projects:", err);
    });
}

function refreshPreferences(): void {
  invoke<Preferences>("load_preferences")
    .then((prefs) => {
      state.preferences = prefs;
      render();
    })
    .catch((err) => {
      console.warn("failed to refresh preferences:", err);
    });
}

function refreshUndoStatus(): void {
  invoke<UndoRedoStatus>("audit_undo_status")
    .then((status) => {
      state.undoStatus = status;
      render();
    })
    .catch((err) => {
      // Non-fatal: a failed status fetch just leaves the undo/redo
      // buttons disabled until the next load retries.
      console.warn("failed to read undo/redo status:", err);
    });
}

// Guards against a second move flow starting while one is still running
// (e.g. double-click on a button), and against pickProject/load swapping
// state.projectDir mid-move (which would let apply_move target the original
// dir while the UI reloads to a new one). Lives outside `state` because
// flipping it must NOT trigger a re-render — re-rendering would destroy the
// trigger button confirmMove() needs alive for focus restoration.
let moveInFlight = false;

async function pickProject(): Promise<void> {
  if (moveInFlight) return;
  // Default to ~ so the picker doesn't open in whatever deep subdir the OS
  // last remembered (often Documents/ on Windows). homeDir() is a Tauri
  // path API that resolves to the platform-correct home dir.
  const picked = await openDialog({
    directory: true,
    multiple: false,
    defaultPath: await homeDir(),
  });
  if (typeof picked === "string") {
    // Reset the filter when the user explicitly picks a different project —
    // a query that matched rules in the old project would silently hide the
    // new project's rules otherwise. Plain Reload keeps the filter intact
    // so move → reload flows don't clobber the user's context.
    state.query = "";
    await load(picked);
  }
}

async function reload(): Promise<void> {
  if (moveInFlight) return;
  await load(state.projectDir);
}

/**
 * Load a project the user picked from the recent-projects dropdown (#47).
 * Behaves like `pickProject` minus the OS picker: same query-reset, same
 * move-in-flight guard, and a no-op when the selected entry is already the
 * loaded project (clicking your current project shouldn't trigger a reload
 * surprise).
 */
async function pickRecentProject(projectDir: string): Promise<void> {
  if (moveInFlight) return;
  if (projectDir === state.projectDir) return;
  state.query = "";
  await load(projectDir);
}

async function moveLeaf(
  req: MoveLeafRequest,
  trigger?: HTMLElement,
  opts?: MoveOptions,
): Promise<void> {
  if (moveInFlight) return;
  moveInFlight = true;
  try {
    // The source side always lives in the currently-viewed project
    // (the rule the user is clicking on is rendered against
    // `state.projectDir`). The destination side may be a different
    // project when the Move-to submenu's cross-project items are
    // picked — `opts.projectDirTo` carries that override (#179). For
    // same-project moves both sides resolve to the same root and the
    // backend's `resolve_move_paths` collapses them into one
    // `ScopePaths` so the IPC stays zero-extra-cost on the hot path.
    const projectDirFrom = state.projectDir ?? undefined;
    const projectDirTo = opts?.projectDirTo ?? projectDirFrom;
    // Drag-and-drop drops set `skipConfirm`: the user already expressed
    // intent by dragging onto a target column, so the diff/confirm modal
    // becomes friction (#70). Click-to-move stays gated on the modal as
    // the safer default for the less-explicit click gesture.
    if (!opts?.skipConfirm) {
      // Intentionally *don't* flip state.busy / re-render before
      // diff_move_leaf: it's fast, the modal itself blocks interaction once
      // open, and keeping the triggering button alive lets confirmMoveLeaf
      // restore focus to it on Cancel/Esc.
      let preview: MoveLeafPreview;
      try {
        preview = await invoke<MoveLeafPreview>("diff_move_leaf", {
          req,
          project_dir_from: projectDirFrom,
          project_dir_to: projectDirTo,
        });
      } catch (err) {
        alert(`Move failed: ${err}`);
        return;
      }

      const apply = await confirmMoveLeaf(preview, trigger);
      if (!apply) return;
    }

    state.busy = true;
    render();
    try {
      await invoke("apply_move_leaf", {
        req,
        project_dir_from: projectDirFrom,
        project_dir_to: projectDirTo,
      });
      // load() owns busy cleanup + final render on success — don't
      // duplicate that work in a finally block.
      //
      // Reload the CURRENTLY VIEWED project, not the projectDir used
      // for the move. Cross-project Move-to (#111) uses
      // opts.projectDir to redirect the write into a different
      // project, but the user is still looking at state.projectDir —
      // reloading the override would silently context-switch the UI
      // to the other project (scopes panel, watcher, recent-projects
      // LRU). The source side of the move still belonged to
      // state.projectDir (or User scope, which is project-independent),
      // so reloading state.projectDir is the right refresh for what
      // the user sees. Codex/code-review post-fix finding.
      await load(state.projectDir);
    } catch (err) {
      alert(`Move failed: ${err}`);
      state.busy = false;
      render();
    }
  } finally {
    moveInFlight = false;
    // A scopes-changed event that landed while the diff modal was open
    // set externalReloadPending but couldn't trigger its own load (we
    // were in the middle of a move). Drain it here so an external edit
    // during the confirm step still gets picked up after the modal
    // closes. load()'s finally does the same thing for the load case.
    if (externalReloadPending && !state.busy) {
      externalReloadPending = false;
      void load(state.projectDir);
    }
  }
}

/**
 * Reclassify a permission rule between allow / deny / ask (#8). Same-scope
 * change-kind rides on the existing `move_leaf` primitive with `from === to`
 * and `to_kind` set; cross-scope reclassification (drag a rule into a
 * different column AND change its kind) goes through the same path. We
 * route through `moveLeaf` so the busy guard, confirm modal, and reload all
 * stay in one place.
 */
async function changeKind(
  path: PathSeg[],
  scope: Scope,
  newKind: PermissionKind,
  trigger?: HTMLElement,
): Promise<void> {
  await moveLeaf({ path, from: scope, to: scope, to_kind: newKind }, trigger);
}

async function deleteLeaf(req: DeleteLeafRequest, trigger?: HTMLElement): Promise<void> {
  if (moveInFlight) return;
  moveInFlight = true;
  try {
    const projectDir = state.projectDir;
    let preview: DeleteLeafPreview;
    try {
      preview = await invoke<DeleteLeafPreview>("diff_delete_leaf", {
        req,
        project_dir: projectDir,
      });
    } catch (err) {
      alert(`Delete failed: ${err}`);
      return;
    }
    const apply = await confirmDeleteLeaf(preview, trigger);
    if (!apply) return;

    state.busy = true;
    render();
    try {
      await invoke("apply_delete_leaf", { req, project_dir: projectDir });
      await load(projectDir);
    } catch (err) {
      alert(`Delete failed: ${err}`);
      state.busy = false;
      render();
    }
  } finally {
    moveInFlight = false;
    if (externalReloadPending && !state.busy) {
      externalReloadPending = false;
      void load(state.projectDir);
    }
  }
}

async function addLeaf(req: AddLeafRequest, trigger?: HTMLElement): Promise<void> {
  if (moveInFlight) return;
  moveInFlight = true;
  try {
    const projectDir = state.projectDir;
    let preview: AddLeafPreview;
    try {
      preview = await invoke<AddLeafPreview>("diff_add_leaf", {
        req,
        project_dir: projectDir,
      });
    } catch (err) {
      alert(`Paste failed: ${err}`);
      return;
    }
    const apply = await confirmAddLeaf(preview, trigger);
    if (!apply) return;

    state.busy = true;
    render();
    try {
      await invoke("apply_add_leaf", { req, project_dir: projectDir });
      await load(projectDir);
    } catch (err) {
      alert(`Paste failed: ${err}`);
      state.busy = false;
      render();
    }
  } finally {
    moveInFlight = false;
    if (externalReloadPending && !state.busy) {
      externalReloadPending = false;
      void load(state.projectDir);
    }
  }
}

/**
 * Drive an undo / redo / restore-to-point (#124 / #125): fetch the preview,
 * route it through the restore-confirm modal, and on confirm apply it and
 * reload. Shares the `moveInFlight` guard and deferred-reload drain with the
 * move/add/delete flows so a write can't start mid-restore.
 *
 * `fetchPreview` / `applyRestore` are passed in because the three flows hit
 * different IPC commands; everything else — the guard, modal, busy state,
 * reload — is identical.
 */
function makeRestoreFlowDeps(): RestoreFlowDeps {
  return {
    alert: (msg) => {
      window.alert(msg);
    },
    confirmRestore: (preview, trigger) => confirmRestore(preview, trigger),
    reload: () => load(state.projectDir),
    beginBusy: () => {
      state.busy = true;
      render();
    },
    isMoveInFlight: () => moveInFlight,
    setMoveInFlight: (v) => {
      moveInFlight = v;
    },
    consumeExternalReload: () => {
      if (externalReloadPending && !state.busy) {
        externalReloadPending = false;
        return true;
      }
      return false;
    },
  };
}

function handleUndo(trigger?: HTMLElement): void {
  // `expected_id` lets the backend refuse if the audit log moved since the
  // preview (a concurrent CLI write) rather than acting on a different
  // entry than the one the user just confirmed.
  void runRestoreFlow(
    "Undo",
    () => invoke<RestorePreview>("audit_undo_preview"),
    (preview) => invoke("audit_apply_undo", { expected_id: preview.target.id }),
    trigger,
    makeRestoreFlowDeps(),
  );
}

function handleRedo(trigger?: HTMLElement): void {
  void runRestoreFlow(
    "Redo",
    () => invoke<RestorePreview>("audit_redo_preview"),
    (preview) => invoke("audit_apply_redo", { expected_id: preview.target.id }),
    trigger,
    makeRestoreFlowDeps(),
  );
}

/**
 * Restore every file affected by `rec` (and the entries after it) back to
 * its pre-`rec` state (#125). Invoked from a History-row "Restore" button;
 * the History dialog closes itself first so the confirm modal opens clean.
 */
function handleRestoreToPoint(rec: AuditRecordView): void {
  // `expected_ops_spanned` + `expected_tail_id` guard against the log
  // changing between preview and apply. Ops_spanned catches the common
  // case (an extra op was appended → window grows); tail_id catches the
  // rarer same-length case (a concurrent rotate+append left the window
  // with the same count but different records — #171).
  void runRestoreFlow(
    "Restore",
    () => invoke<RestorePreview>("audit_restore_to_point_preview", { target_id: rec.id }),
    (preview) =>
      invoke("audit_apply_restore_to_point", {
        target_id: rec.id,
        expected_ops_spanned: preview.ops_spanned,
        expected_tail_id: preview.tail_id ?? null,
      }),
    undefined,
    makeRestoreFlowDeps(),
  );
}

function setQuery(next: string): void {
  if (state.query === next) return;
  state.query = next;
  render();
}

// `prefers-color-scheme` listener used while the active theme is "auto".
// We attach at most one — re-attaching on every render would either leak
// listeners or require addEventListener-with-a-stable-reference contortions.
// `null` means "no listener installed right now" (theme is light or dark).
const prefersDarkMql =
  typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-color-scheme: dark)")
    : null;
let autoThemeListener: ((evt: MediaQueryListEvent) => void) | null = null;

function resolveTheme(theme: Theme): "light" | "dark" {
  if (theme === "auto") {
    return prefersDarkMql?.matches ? "dark" : "light";
  }
  return theme;
}

/**
 * Apply the chosen theme to the document and (de)attach the OS-tracking
 * listener. Safe to call repeatedly — the listener bookkeeping ensures we
 * never end up with two listeners installed.
 */
function applyTheme(theme: Theme): void {
  document.documentElement.setAttribute("data-theme", resolveTheme(theme));

  if (!prefersDarkMql) return;
  if (theme === "auto") {
    if (autoThemeListener) return;
    autoThemeListener = () => {
      // The user's preference is still "auto" by construction (we only
      // keep this listener attached while that's true), so re-resolve and
      // repaint without touching state.preferences.
      document.documentElement.setAttribute("data-theme", resolveTheme("auto"));
    };
    prefersDarkMql.addEventListener("change", autoThemeListener);
  } else if (autoThemeListener) {
    prefersDarkMql.removeEventListener("change", autoThemeListener);
    autoThemeListener = null;
  }
}

// Serialize preference saves: at most one in-flight call, with the latest
// pending state always winning. Without this, rapid toggles could produce
// overlapping `save_preferences` invokes whose completion order isn't
// guaranteed to match UI order — so a slower earlier save resolving after
// a faster later save would leave disk out of sync with the visible state.
let preferencesSaveInFlight: Promise<void> | null = null;
let pendingPreferencesSave: Preferences | null = null;

async function persistPreferences(next: Preferences): Promise<void> {
  // Update state + render immediately so the UI feels instant; persist in
  // the background. If the save fails, warn the user — stale in-memory
  // state is easier to reason about than a silent mismatch with disk.
  const prev = state.preferences;
  state.preferences = next;
  if (prev.theme !== next.theme) {
    applyTheme(next.theme);
  }
  render();
  pendingPreferencesSave = next;
  if (preferencesSaveInFlight) return;
  preferencesSaveInFlight = (async () => {
    try {
      while (pendingPreferencesSave) {
        const prefs = pendingPreferencesSave;
        pendingPreferencesSave = null;
        try {
          await invoke("save_preferences", { prefs });
        } catch (err) {
          alert(`Failed to save preferences: ${err}`);
        }
      }
    } finally {
      preferencesSaveInFlight = null;
    }
  })();
}

function onToggleScopeVisibility(scope: Scope, visible: boolean): void {
  const cur = state.preferences.visible_scopes;
  const next = visible
    ? [...cur, scope].filter((s, i, a) => a.indexOf(s) === i)
    : cur.filter((s) => s !== scope);
  void persistPreferences({ ...state.preferences, visible_scopes: next });
}

function onChangeTheme(theme: Theme): void {
  if (state.preferences.theme === theme) return;
  void persistPreferences({ ...state.preferences, theme });
}

function onToggleBackupOnWrite(enabled: boolean): void {
  if (state.preferences.backup_on_write === enabled) return;
  void persistPreferences({ ...state.preferences, backup_on_write: enabled });
}

function onToggleAuditLogRotate(enabled: boolean): void {
  if (state.preferences.audit_log_rotate === enabled) return;
  void persistPreferences({ ...state.preferences, audit_log_rotate: enabled });
}

function onChangeAuditLogMaxSizeMb(mb: number): void {
  if (state.preferences.audit_log_max_size_mb === mb) return;
  void persistPreferences({ ...state.preferences, audit_log_max_size_mb: mb });
}

function onChangeGroupRulesAt(value: number | null): void {
  if (state.preferences.group_rules_at === value) return;
  void persistPreferences({ ...state.preferences, group_rules_at: value });
}

function onToggleCombinedPanelCollapsed(collapsed: boolean): void {
  if (state.preferences.combined_panel_collapsed === collapsed) return;
  void persistPreferences({ ...state.preferences, combined_panel_collapsed: collapsed });
}

function onOpenSettings(trigger?: HTMLElement): void {
  void openSettings(
    {
      preferences: state.preferences,
      onToggleScopeVisibility,
      onChangeTheme,
      onToggleBackupOnWrite,
      onToggleAuditLogRotate,
      onChangeAuditLogMaxSizeMb,
      onChangeGroupRulesAt,
    },
    trigger,
  );
}

function render(): void {
  const root = document.getElementById("app");
  if (!root) return;
  renderApp(root, {
    scopes: state.scopes,
    projectDir: state.projectDir,
    busy: state.busy,
    query: state.query,
    preferences: state.preferences,
    runtime: state.runtime,
    knownProjects: state.knownProjects,
    undoStatus: state.undoStatus,
    onPickProject: pickProject,
    onPickRecentProject: pickRecentProject,
    onReload: reload,
    onUndo: handleUndo,
    onRedo: handleRedo,
    onMoveLeaf: moveLeaf,
    onChangeKind: changeKind,
    onDeleteLeaf: deleteLeaf,
    onAddLeaf: addLeaf,
    onOpenSettings,
    onOpenAbout: handleOpenAbout,
    onOpenHistory: handleOpenHistory,
    onToggleCombinedPanelCollapsed,
    onQueryChange: setQuery,
  });
}

function handleOpenAbout(trigger?: HTMLElement): void {
  openAbout(state.appInfo, trigger ?? null);
}

/**
 * Fetch the audit log from the backend and open the History dialog
 * (#19 phase 2). Fetched on each click — the log grows over time and
 * the dialog should reflect every write, including ones made in this
 * session. On IPC failure, open the dialog with `null` so the user sees
 * an error message rather than a silent no-op (the dialog handles that
 * branch).
 */
async function handleOpenHistory(trigger?: HTMLElement): Promise<void> {
  let page: AuditLogPage | null = null;
  try {
    page = await invoke<AuditLogPage>("list_audit_records");
  } catch (err) {
    console.warn("failed to read audit log:", err);
  }
  openHistory(page, { trigger: trigger ?? null, onRestoreToPoint: handleRestoreToPoint });
}

// Pressing "/" anywhere focuses the rule-search input, GitHub / Gmail style —
// but skip when the user is already typing in a text field or interacting
// with a modal, so we don't steal their keystroke.
document.addEventListener("keydown", (e) => {
  if (e.key !== "/" || e.ctrlKey || e.metaKey || e.altKey) return;
  // Bail out if something else already handled this keystroke so we don't
  // hijack future shortcuts that happen to include "/".
  if (e.defaultPrevented) return;
  const target = e.target as HTMLElement | null;
  if (
    target &&
    (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)
  ) {
    return;
  }
  if (document.querySelector(".modal-backdrop")) return;
  const search = document.getElementById(SEARCH_INPUT_ID) as HTMLInputElement | null;
  // Don't swallow the keystroke if the input is missing or currently
  // disabled (e.g. before any project has loaded).
  if (!search || search.disabled) return;
  e.preventDefault();
  search.focus();
  search.select();
});

// Ctrl+Z / Cmd+Z undo, Ctrl+Shift+Z / Cmd+Shift+Z redo (#124). Global, but
// — like the "/" shortcut above — skipped while a text field is focused or
// a modal is open, so we never steal a keystroke the user meant elsewhere.
document.addEventListener("keydown", (e) => {
  if (e.key !== "z" && e.key !== "Z") return;
  // Require exactly the platform modifier; Alt+Ctrl+Z is left alone.
  if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
  if (e.defaultPrevented) return;
  const target = e.target as HTMLElement | null;
  if (
    target &&
    (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)
  ) {
    return;
  }
  if (document.querySelector(".modal-backdrop")) return;
  // A load or move/restore in flight: drop the keystroke rather than queue
  // a second overlapping flow.
  if (moveInFlight || state.busy) return;
  if (e.shiftKey) {
    if (!state.undoStatus?.redo) return;
    e.preventDefault();
    handleRedo();
  } else {
    if (!state.undoStatus?.undo) return;
    e.preventDefault();
    handleUndo();
  }
});

// The Rust watcher (`src-tauri/src/watcher.rs`) emits `scopes-changed` when
// any of the three settings files mutates externally. Reload the data so the
// UI mirrors what's on disk.
//
// If a load or move is already in flight, skip this reload but set a sticky
// flag — the current load's finally block will drain it with a single
// follow-up reload. That prevents overlapping `load_scopes` invokes and the
// out-of-order state writes that would come with them, without needing a
// full job queue.
listen("scopes-changed", () => {
  if (moveInFlight || state.busy) {
    externalReloadPending = true;
    return;
  }
  void load(state.projectDir);
}).catch((err) => {
  console.error("failed to register scopes-changed listener", err);
});

// Backend emits this when the file watcher fails to install — auto-reload is
// a non-fatal nice-to-have, so we just log to devtools rather than hijacking
// the UI with an alert. Windows release builds discard stderr, so this is
// how the failure stays observable in production.
listen<string>("watcher-error", (evt) => {
  console.warn("ClaudeScope watcher install failed:", evt.payload);
}).catch((err) => {
  console.error("failed to register watcher-error listener", err);
});

async function bootstrap(): Promise<void> {
  // Load preferences + runtime info before the first scope load so the
  // initial render applies them (column visibility, sandbox banner)
  // instead of flashing defaults first and then switching. Both calls are
  // independent — fire them in parallel and tolerate either failing.
  const [prefsResult, runtimeResult, appInfoResult] = await Promise.allSettled([
    invoke<Preferences>("load_preferences"),
    invoke<RuntimeInfo>("load_runtime_info"),
    invoke<AppInfo>("get_app_info"),
  ]);
  if (prefsResult.status === "fulfilled") {
    state.preferences = prefsResult.value;
  } else {
    console.warn("failed to load preferences, using defaults:", prefsResult.reason);
  }
  // Apply theme as soon as we know it — before the scope load runs, so the
  // load-busy spinner paints in the correct palette. The default `:root`
  // CSS variables are dark, so this is also the moment any cold-start
  // flash flips to light when the user has light selected.
  applyTheme(state.preferences.theme);
  if (runtimeResult.status === "fulfilled") {
    state.runtime = runtimeResult.value;
  } else {
    console.warn("failed to load runtime info:", runtimeResult.reason);
  }
  if (appInfoResult.status === "fulfilled") {
    state.appInfo = appInfoResult.value;
  } else {
    // Non-fatal — About dialog will render "Loading…" instead.
    console.warn("failed to load app info:", appInfoResult.reason);
  }
  await load(null);
}

void bootstrap();
