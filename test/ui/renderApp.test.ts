import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MoveOptions, MoveRequest, Scope, Theme } from "../../src/types.ts";
import { SEARCH_INPUT_ID } from "../../src/types.ts";
import { openSettings, renderApp } from "../../src/ui.ts";
import { buildLoadedScopes, buildPreferences, buildRuntimeInfo } from "../fixtures/loadedScopes.ts";

function makeRoot(): HTMLElement {
  const root = document.createElement("div");
  root.id = "app";
  document.body.appendChild(root);
  return root;
}

function clearBody(): void {
  while (document.body.firstChild) document.body.removeChild(document.body.firstChild);
}

interface PropsOverrides {
  scopes?: ReturnType<typeof buildLoadedScopes> | null;
  projectDir?: string | null;
  busy?: boolean;
  query?: string;
  preferences?: ReturnType<typeof buildPreferences>;
  runtime?: ReturnType<typeof buildRuntimeInfo>;
  onMove?: (req: MoveRequest, trigger?: HTMLElement, opts?: MoveOptions) => void;
}

function makeProps(overrides: PropsOverrides = {}) {
  const scopes = overrides.scopes ?? null;
  // Default `projectDir` from the loaded scopes' `project_dir` so tests
  // exercise the same render path the real app does (header text, the
  // `lastRenderedProjectDir` reset that clears `openTreeNodes` when the
  // project changes). Callers can still pass `projectDir: null` explicitly
  // to cover the no-project branch.
  const projectDir =
    overrides.projectDir !== undefined ? overrides.projectDir : (scopes?.project_dir ?? null);
  return {
    scopes,
    projectDir,
    busy: overrides.busy ?? false,
    query: overrides.query ?? "",
    preferences: overrides.preferences ?? buildPreferences(),
    runtime: overrides.runtime ?? buildRuntimeInfo(),
    onPickProject: vi.fn(),
    onReload: vi.fn(),
    onMove: overrides.onMove ?? vi.fn(),
    onMoveKey: vi.fn(),
    onOpenSettings: vi.fn(),
    onQueryChange: vi.fn(),
  };
}

describe("renderApp", () => {
  let root: HTMLElement;

  beforeEach(() => {
    root = makeRoot();
  });

  afterEach(() => {
    clearBody();
  });

  it("shows the loading message when busy and no scopes are loaded", () => {
    renderApp(root, makeProps({ busy: true }));
    const empty = root.querySelector(".empty");
    expect(empty?.textContent).toBe("Loading…");
  });

  it("shows the empty message when no scopes are loaded and not busy", () => {
    renderApp(root, makeProps({ busy: false }));
    const empty = root.querySelector(".empty");
    expect(empty?.textContent).toBe("No settings loaded.");
  });

  it("renders the combined permissions panel with allow/deny/ask counts", () => {
    const scopes = buildLoadedScopes({
      scopes: [
        {
          scope: "project",
          permissions: { allow: ["Bash(git status)", "Read(**)"], deny: ["Bash(rm -rf *)"] },
        },
        {
          scope: "user",
          permissions: { ask: ["WebFetch(domain:example.com)"] },
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const labels = Array.from(root.querySelectorAll(".combo-label")).map((el) => el.textContent);
    expect(labels).toEqual(["allow (2)", "deny (1)", "ask (1)"]);
    const allowChips = root.querySelectorAll(".combo-allow .chip");
    expect(Array.from(allowChips).map((c) => c.textContent)).toEqual([
      "Bash(git status)",
      "Read(**)",
    ]);
  });

  it("renders matched/total counts when a query filters the combined panel", () => {
    const scopes = buildLoadedScopes({
      scopes: [
        {
          scope: "project",
          permissions: { allow: ["Bash(git status)", "Read(**)", "Bash(npm test)"] },
        },
      ],
    });
    renderApp(root, makeProps({ scopes, query: "git" }));
    const allowLabel = root.querySelector(".combo-allow .combo-label");
    expect(allowLabel?.textContent).toBe("allow (1/3)");
  });

  it("preserves search input focus and caret across a re-render", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const input = document.getElementById(SEARCH_INPUT_ID) as HTMLInputElement | null;
    expect(input).not.toBeNull();
    if (!input) return;
    input.value = "git";
    input.focus();
    input.setSelectionRange(2, 2);
    renderApp(root, makeProps({ scopes, query: "git" }));
    const after = document.getElementById(SEARCH_INPUT_ID) as HTMLInputElement | null;
    expect(document.activeElement).toBe(after);
    expect(after?.selectionStart).toBe(2);
    expect(after?.selectionEnd).toBe(2);
  });

  it("invokes onMove with the expected request when a per-scope move button is clicked", () => {
    const onMove = vi.fn();
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes, onMove }));
    // The project column has buttons targeting every other visible scope.
    // Pick the one targeting "user" so the click is unambiguous.
    const buttons = Array.from(
      root.querySelectorAll<HTMLButtonElement>(".rule-group .rule-moves .move-btn"),
    );
    const toUser = buttons.find((b) => b.textContent === "→ User");
    expect(toUser).toBeDefined();
    toUser?.click();
    expect(onMove).toHaveBeenCalledTimes(1);
    const [req] = onMove.mock.calls[0];
    expect(req).toEqual({
      rule: "Bash(git status)",
      kind: "allow",
      from: "project",
      to: "user",
    });
  });

  it("renders a lint warning badge for a malformed rule", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    // Lower-case `bash` falls into the lint's "Unknown tool" branch and the
    // badge is the only warning in the tree, so a single check across the
    // whole root is enough.
    const badges = root.querySelectorAll(".lint-warn");
    expect(badges.length).toBeGreaterThanOrEqual(1);
    expect(badges[0].textContent).toBe("⚠");
  });

  it("hides scope columns the user has marked invisible", () => {
    const visible: Scope[] = ["project"];
    const scopes = buildLoadedScopes({
      scopes: [
        { scope: "project", permissions: { allow: ["Bash(git status)"] } },
        { scope: "user", permissions: { allow: ["Bash(ls)"] } },
      ],
    });
    renderApp(
      root,
      makeProps({ scopes, preferences: buildPreferences({ visible_scopes: visible }) }),
    );
    const headings = Array.from(root.querySelectorAll(".col h3")).map((el) => el.textContent);
    expect(headings).toEqual(["Project"]);
  });
});

describe("openSettings – theme radios", () => {
  afterEach(() => {
    clearBody();
  });

  function makeSettingsProps(theme: Theme = "auto", onChangeTheme = vi.fn()) {
    return {
      preferences: buildPreferences({ theme }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme,
    };
  }

  function getThemeRadios(): HTMLInputElement[] {
    return Array.from(document.querySelectorAll<HTMLInputElement>('input[name="settings-theme"]'));
  }

  it("renders three theme radios (auto, light, dark)", () => {
    openSettings(makeSettingsProps());
    const radios = getThemeRadios();
    expect(radios).toHaveLength(3);
    expect(radios.map((r) => r.value)).toEqual(["auto", "light", "dark"]);
  });

  it("checks the radio matching the current theme preference", () => {
    for (const theme of ["auto", "light", "dark"] as Theme[]) {
      clearBody();
      openSettings(makeSettingsProps(theme));
      const checked = getThemeRadios().find((r) => r.checked);
      expect(checked?.value).toBe(theme);
    }
  });

  it("calls onChangeTheme with the selected value when a radio is changed", () => {
    const onChangeTheme = vi.fn();
    openSettings(makeSettingsProps("auto", onChangeTheme));
    const darkRadio = getThemeRadios().find((r) => r.value === "dark");
    expect(darkRadio).toBeDefined();
    if (!darkRadio) return;
    darkRadio.checked = true;
    darkRadio.dispatchEvent(new Event("change"));
    expect(onChangeTheme).toHaveBeenCalledTimes(1);
    expect(onChangeTheme).toHaveBeenCalledWith("dark");
  });

  it("radio group has ARIA radiogroup role with label pointing to the heading", () => {
    openSettings(makeSettingsProps());
    const group = document.querySelector(
      '[role="radiogroup"][aria-labelledby="settings-theme-heading"]',
    );
    expect(group).not.toBeNull();
    const heading = document.getElementById("settings-theme-heading");
    expect(heading?.textContent).toBe("Theme");
  });
});
