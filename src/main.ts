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

async function pickProject(): Promise<void> {
  const picked = await openDialog({ directory: true, multiple: false });
  if (typeof picked === "string") {
    await load(picked);
  }
}

async function moveRule(req: MoveRequest): Promise<void> {
  const projectDir = state.projectDir;
  state.busy = true;
  render();
  try {
    const preview = await invoke<MovePreview>("diff_move", { req, project_dir: projectDir });
    const apply = await confirmMove(preview);
    if (!apply) return;
    await invoke("apply_move", { req, project_dir: projectDir });
    await load(projectDir);
  } catch (err) {
    alert(`Move failed: ${err}`);
  } finally {
    state.busy = false;
    render();
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
    onReload: () => load(state.projectDir),
    onMove: moveRule,
  });
}

load(null);
