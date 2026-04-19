import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import "./styles.css";
import { confirmMove, renderApp } from "./ui.ts";
import type { LoadedScopes, MovePreview, MoveRequest } from "./types.ts";

const state: { scopes: LoadedScopes | null; projectDir: string | null; busy: boolean } = {
  scopes: null,
  projectDir: null,
  busy: false,
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
      await load(projectDir);
    } catch (err) {
      alert(`Move failed: ${err}`);
    } finally {
      state.busy = false;
      render();
    }
  } finally {
    moveInFlight = false;
  }
}

function render(): void {
  const root = document.getElementById("app");
  if (!root) return;
  renderApp(root, {
    scopes: state.scopes,
    projectDir: state.projectDir,
    busy: state.busy,
    onPickProject: pickProject,
    onReload: reload,
    onMove: moveRule,
  });
}

load(null);
