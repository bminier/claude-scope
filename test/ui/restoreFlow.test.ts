import { describe, expect, it, vi } from "vitest";
import type { RestoreFlowDeps } from "../../src/restore-flow.ts";
import { runRestoreFlow } from "../../src/restore-flow.ts";
import type { AuditRecordView, RestorePreview } from "../../src/types.ts";

function makePreview(): RestorePreview {
  // Minimal shape — the test only inspects identity, not content.
  return {
    direction: "undo",
    target: { record: {}, ts_ms: 0 } as unknown as AuditRecordView,
    sides: [],
    ops_spanned: 1,
  } as unknown as RestorePreview;
}

function makeDeps(overrides: Partial<RestoreFlowDeps> = {}): RestoreFlowDeps {
  let inFlight = false;
  const defaults: RestoreFlowDeps = {
    alert: vi.fn(),
    confirmRestore: vi.fn(async () => true),
    reload: vi.fn(async () => {}),
    beginBusy: vi.fn(),
    isMoveInFlight: () => inFlight,
    setMoveInFlight: (v) => {
      inFlight = v;
    },
    consumeExternalReload: () => false,
  };
  return { ...defaults, ...overrides };
}

describe("runRestoreFlow", () => {
  it("reloads after a successful apply so the UI reflects the new state", async () => {
    const reload = vi.fn(async () => {});
    const deps = makeDeps({ reload });
    const fetchPreview = vi.fn(async () => makePreview());
    const applyRestore = vi.fn(async () => {});

    await runRestoreFlow("Undo", fetchPreview, applyRestore, undefined, deps);

    expect(applyRestore).toHaveBeenCalledTimes(1);
    expect(reload).toHaveBeenCalledTimes(1);
    expect(deps.alert).not.toHaveBeenCalled();
  });

  it("reloads after a staleness rejection so a stale undo target can refresh (#175)", async () => {
    // Before #175, the catch path called only alert() + reset busy. The
    // topbar's `audit_undo_status` was never refetched, so the cached
    // undo target stayed pointed at the now-gone log entry. Ctrl+Z
    // (which routes through handleUndo → runRestoreFlow) would then
    // re-enter the same failing flow on a loop until the user clicked
    // Reload by hand. The fix is to call reload() in the catch branch
    // too; this test pins it.
    const reload = vi.fn(async () => {});
    const alert = vi.fn();
    const deps = makeDeps({ reload, alert });
    const fetchPreview = vi.fn(async () => makePreview());
    const applyRestore = vi.fn(async () => {
      // Mirrors the backend's "expected_id mismatch — the audit log
      // changed since the preview" rejection.
      throw new Error("audit log changed since the preview");
    });

    await runRestoreFlow("Undo", fetchPreview, applyRestore, undefined, deps);

    expect(applyRestore).toHaveBeenCalledTimes(1);
    expect(reload).toHaveBeenCalledTimes(1);
    expect(alert).toHaveBeenCalledWith(
      expect.stringContaining("Undo failed: Error: audit log changed since the preview"),
    );
  });

  it("reloads BEFORE alerting so the user dismisses onto current truth", async () => {
    // Ordering matters: the alert is modal in a browser context, so if
    // we alert first the user is staring at the dialog while the UI
    // behind it still shows the stale undo target. Reloading first
    // means the topbar is already refreshed by the time the user
    // clicks OK.
    const events: string[] = [];
    const deps = makeDeps({
      reload: vi.fn(async () => {
        events.push("reload");
      }),
      alert: vi.fn(() => {
        events.push("alert");
      }),
    });
    const fetchPreview = vi.fn(async () => makePreview());
    const applyRestore = vi.fn(async () => {
      throw new Error("staleness");
    });

    await runRestoreFlow("Redo", fetchPreview, applyRestore, undefined, deps);

    expect(events).toEqual(["reload", "alert"]);
  });

  it("does not reload when the preview fetch itself fails (no apply was attempted)", async () => {
    // fetchPreview failures don't necessarily mean the topbar is
    // stale — the backend's status query is independent. Skip the
    // reload here; the apply-attempted branch is the only one that
    // needs the #175 fix.
    const reload = vi.fn(async () => {});
    const deps = makeDeps({ reload });
    const fetchPreview = vi.fn(async () => {
      throw new Error("preview IPC failed");
    });
    const applyRestore = vi.fn();

    await runRestoreFlow("Undo", fetchPreview, applyRestore, undefined, deps);

    expect(applyRestore).not.toHaveBeenCalled();
    expect(reload).not.toHaveBeenCalled();
    expect(deps.alert).toHaveBeenCalled();
  });

  it("does not reload when the user cancels the confirm modal", async () => {
    // No write was attempted, so log state is unchanged.
    const reload = vi.fn(async () => {});
    const deps = makeDeps({
      reload,
      confirmRestore: vi.fn(async () => false),
    });
    const fetchPreview = vi.fn(async () => makePreview());
    const applyRestore = vi.fn();

    await runRestoreFlow("Undo", fetchPreview, applyRestore, undefined, deps);

    expect(applyRestore).not.toHaveBeenCalled();
    expect(reload).not.toHaveBeenCalled();
    expect(deps.alert).not.toHaveBeenCalled();
  });

  it("refuses to start a second flow while one is in flight (double-click guard)", async () => {
    // moveInFlight gates both move and restore flows so a fast
    // double-press of Ctrl+Z can't open two confirm modals against
    // racing previews.
    let inFlight = false;
    const calls = { confirm: 0 };
    const deps: RestoreFlowDeps = {
      alert: vi.fn(),
      confirmRestore: vi.fn(async () => {
        calls.confirm += 1;
        return false; // cancel both so the test finishes promptly.
      }),
      reload: vi.fn(async () => {}),
      beginBusy: vi.fn(),
      isMoveInFlight: () => inFlight,
      setMoveInFlight: (v) => {
        inFlight = v;
      },
      consumeExternalReload: () => false,
    };

    const first = runRestoreFlow(
      "Undo",
      async () => makePreview(),
      async () => {},
      undefined,
      deps,
    );
    // Second call enters synchronously while the first is awaiting
    // its fetchPreview microtask — `inFlight` is already true.
    await runRestoreFlow(
      "Undo",
      async () => makePreview(),
      async () => {},
      undefined,
      deps,
    );
    await first;

    expect(calls.confirm).toBe(1); // only the first flow reached confirm
  });
});
