import type { RestorePreview } from "./types.ts";

/**
 * Dependencies injected into [`runRestoreFlow`]. Pulling these out of the
 * function lets the test harness exercise the recovery branches without a
 * live Tauri context, DOM, or module-level state. All callbacks are kept
 * tiny so the wiring in main.ts stays mechanical — see `makeRestoreFlowDeps`.
 */
export interface RestoreFlowDeps {
  /** Surface a user-visible error (real path: `window.alert`). */
  alert: (msg: string) => void;
  /** Resolves true if the user confirmed the preview, false on cancel. */
  confirmRestore: (preview: RestorePreview, trigger?: HTMLElement) => Promise<boolean>;
  /**
   * Re-fetch scopes + audit status from the backend. Called both after a
   * successful apply (so the UI reflects the new file state) AND after a
   * failed apply (so a stale undo/redo target in the topbar is replaced
   * with whatever the post-failure log actually offers — #175).
   */
  reload: () => Promise<void>;
  /** Flip `state.busy = true` and re-render — called once before apply. */
  beginBusy: () => void;
  /** Module-level guard: is another move/restore already running? */
  isMoveInFlight: () => boolean;
  /** Flip the guard on entry / off in `finally`. */
  setMoveInFlight: (v: boolean) => void;
  /**
   * True iff an external-file-change listener fired during this flow and
   * the caller's reload finished without conflicting state. Consumed (and
   * cleared) when returning true so the trailing `void reload()` runs at
   * most once.
   */
  consumeExternalReload: () => boolean;
}

/**
 * Run an Undo / Redo / Restore-to-point flow end-to-end:
 *
 *   1. Fetch the preview via `fetchPreview` (typically a backend IPC).
 *   2. Show the confirmation modal via `deps.confirmRestore`; bail on cancel.
 *   3. Call `applyRestore(preview)` to commit; the backend may refuse with a
 *      staleness error (`expected_id` / `expected_tail_id` / `ops_spanned`
 *      mismatch — the audit log moved between preview and apply).
 *   4. **Always** reload after a confirmed apply attempt, whether success or
 *      failure. On success the UI shows the new state; on failure the stale
 *      undo/redo target in the topbar is replaced with whatever the
 *      post-failure log actually offers, so re-pressing Ctrl+Z can't loop
 *      back into the same failing flow (#175).
 *
 * `fetchPreview` / `applyRestore` are passed in because the three callers
 * hit different IPC commands; everything else — the guard, modal, busy
 * state, reload — is identical, which is the whole reason this helper
 * exists.
 */
export async function runRestoreFlow(
  label: string,
  fetchPreview: () => Promise<RestorePreview>,
  applyRestore: (preview: RestorePreview) => Promise<void>,
  trigger: HTMLElement | undefined,
  deps: RestoreFlowDeps,
): Promise<void> {
  if (deps.isMoveInFlight()) return;
  deps.setMoveInFlight(true);
  try {
    let preview: RestorePreview;
    try {
      preview = await fetchPreview();
    } catch (err) {
      deps.alert(`${label} failed: ${err}`);
      return;
    }

    const apply = await deps.confirmRestore(preview, trigger);
    if (!apply) return;

    deps.beginBusy();
    let applyError: unknown = null;
    try {
      await applyRestore(preview);
    } catch (err) {
      applyError = err;
    }
    // Reload unconditionally on the apply-attempted path: the success
    // case needs it so the UI reflects the new disk state; the failure
    // case needs it so the topbar's stale undo/redo target gets a
    // chance to refresh against the actual post-failure log (#175). The
    // alert fires *after* the reload so the user's "OK" dismisses onto
    // a UI that already shows current truth.
    await deps.reload();
    if (applyError !== null) {
      deps.alert(`${label} failed: ${applyError}`);
    }
  } finally {
    deps.setMoveInFlight(false);
    if (deps.consumeExternalReload()) {
      void deps.reload();
    }
  }
}
