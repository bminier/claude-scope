import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import "./styles.css";
import type {
  LoadedScopes,
  MoveKeyPreview,
  MoveKeyRequest,
  MovePreview,
  MoveRequest,
} from "./types.ts";
import { SEARCH_INPUT_ID } from "./types.ts";
import { confirmMove, confirmMoveKey, renderApp } from "./ui.ts";

const state: {
  scopes: LoadedScopes | null;
  projectDir: string | null;
  busy: boolean;
  query: string;
} = {
  scopes: null,
  projectDir: null,
  busy: false,
  query: "",
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

// Guards against a second move flow starting while one is still running
// (e.g. double-click on a button), and against pickProject/load swapping
// state.projectDir mid-move (which would let apply_move target the original
// dir while the UI reloads to a new one). Lives outside `state` because
// flipping it must NOT trigger a re-render — re-rendering would destroy the
// trigger button confirmMove() needs alive for focus restoration.
let moveInFlight = false;

async function pickProject(): Promise<void> {
  if (moveInFlight) return;
  const picked = await openDialog({ directory: true, multiple: false });
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

async function moveRule(req: MoveRequest, trigger?: HTMLElement): Promise<void> {
  if (moveInFlight) return;
  moveInFlight = true;
  try {
    const projectDir = state.projectDir;
    // Intentionally *don't* flip state.busy / re-render before diff_move:
    // it's fast, the modal itself blocks interaction once open, and keeping
    // the triggering button alive lets confirmMove restore focus to it on
    // Cancel/Esc.
    let preview: MovePreview;
    try {
      preview = await invoke<MovePreview>("diff_move", { req, project_dir: projectDir });
    } catch (err) {
      alert(`Move failed: ${err}`);
      return;
    }

    const apply = await confirmMove(preview, trigger);
    if (!apply) return;

    state.busy = true;
    render();
    try {
      await invoke("apply_move", { req, project_dir: projectDir });
      // load() owns busy cleanup + final render on success — don't
      // duplicate that work in a finally block.
      await load(projectDir);
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

async function moveKey(req: MoveKeyRequest, trigger?: HTMLElement): Promise<void> {
  if (moveInFlight) return;
  moveInFlight = true;
  try {
    const projectDir = state.projectDir;
    let preview: MoveKeyPreview;
    try {
      preview = await invoke<MoveKeyPreview>("diff_move_key", { req, project_dir: projectDir });
    } catch (err) {
      alert(`Move failed: ${err}`);
      return;
    }

    const apply = await confirmMoveKey(preview, trigger);
    if (!apply) return;

    state.busy = true;
    render();
    try {
      await invoke("apply_move_key", { req, project_dir: projectDir });
      await load(projectDir);
    } catch (err) {
      alert(`Move failed: ${err}`);
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

function setQuery(next: string): void {
  if (state.query === next) return;
  state.query = next;
  render();
}

function render(): void {
  const root = document.getElementById("app");
  if (!root) return;
  renderApp(root, {
    scopes: state.scopes,
    projectDir: state.projectDir,
    busy: state.busy,
    query: state.query,
    onPickProject: pickProject,
    onReload: reload,
    onMove: moveRule,
    onMoveKey: moveKey,
    onQueryChange: setQuery,
  });
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

load(null);
