import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import "./styles.css";
import { confirmMove, renderApp } from "./ui.ts";
import type { LoadedScopes, MovePreview, MoveRequest } from "./types.ts";

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
    (target.tagName === "INPUT" ||
      target.tagName === "TEXTAREA" ||
      target.isContentEditable)
  ) {
    return;
  }
  if (document.querySelector(".modal-backdrop")) return;
  const search = document.getElementById("rule-search") as HTMLInputElement | null;
  if (!search) return;
  e.preventDefault();
  search.focus();
  search.select();
});

load(null);
