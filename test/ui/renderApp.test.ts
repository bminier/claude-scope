import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MoveLeafRequest, MoveOptions, Scope, Theme } from "../../src/types.ts";
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
  onMoveLeaf?: (req: MoveLeafRequest, trigger?: HTMLElement, opts?: MoveOptions) => void;
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
    onMoveLeaf: overrides.onMoveLeaf ?? vi.fn(),
    onChangeKind: vi.fn(),
    onDeleteLeaf: vi.fn(),
    onAddLeaf: vi.fn(),
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

  it("invokes onMoveLeaf with a path-based request when a per-scope move button is clicked", () => {
    // Permission rules now live as tree leaves under `permissions.allow[*]`,
    // so the move button rides on the leaf's `.rule .rule-moves .move-btn`
    // and the request payload carries a JSON path instead of {rule, kind}.
    const onMoveLeaf = vi.fn();
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes, onMoveLeaf }));
    const buttons = Array.from(
      root.querySelectorAll<HTMLButtonElement>(".rule.rule-allow .rule-moves .move-btn"),
    );
    const toUser = buttons.find((b) => b.textContent === "→ User");
    expect(toUser).toBeDefined();
    toUser?.click();
    expect(onMoveLeaf).toHaveBeenCalledTimes(1);
    const [req] = onMoveLeaf.mock.calls[0];
    expect(req).toEqual({
      path: ["permissions", "allow", 0],
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

describe("context menu (#8)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    root = makeRoot();
  });

  afterEach(() => {
    // Sweep any open context menu the test left behind so the next test
    // starts clean. The menu lives at document.body level outside `root`
    // and survives `clearBody`'s wipe only if a global listener kept it.
    for (const m of Array.from(document.querySelectorAll(".context-menu"))) m.remove();
    clearBody();
  });

  function rightClick(el: HTMLElement): void {
    el.dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 50, clientY: 50 }),
    );
  }

  function findMenuItem(label: string): HTMLButtonElement | null {
    for (const btn of document.querySelectorAll<HTMLButtonElement>(".context-menu-item")) {
      // Trim because the submenu arrow span adds whitespace via its
      // textContent — `Move to ▸` would otherwise need an exact match
      // including the unicode arrow.
      if (btn.textContent?.replace(/\s+/g, " ").trim().startsWith(label)) return btn;
    }
    return null;
  }

  it("opens a context menu with Copy / Delete / Change kind / Move to on a rule leaf", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    expect(ruleRow).not.toBeNull();
    if (!ruleRow) return;
    rightClick(ruleRow);
    const labels = Array.from(document.querySelectorAll<HTMLButtonElement>(".context-menu-item"))
      .map((b) => b.textContent?.replace(/\s+/g, " ").trim() ?? "")
      // Strip the trailing submenu arrow so the assertion isn't tied to its glyph.
      .map((l) => l.replace(/\s*▸$/, "").trim());
    expect(labels).toEqual(["Copy", "Delete", "Change kind", "Move to"]);
  });

  it("calls onDeleteLeaf with the leaf's path when Delete is activated", () => {
    const onDeleteLeaf = vi.fn();
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    const props = { ...makeProps({ scopes }), onDeleteLeaf };
    renderApp(root, props);
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Delete")?.click();
    expect(onDeleteLeaf).toHaveBeenCalledTimes(1);
    expect(onDeleteLeaf.mock.calls[0][0]).toEqual({
      path: ["permissions", "allow", 0],
      from: "project",
    });
  });

  it("disables the current kind in the Change-kind submenu", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Change kind")?.click();
    // Two menus open now: root + submenu. Find the kind buttons in the second.
    const menus = document.querySelectorAll<HTMLElement>(".context-menu");
    expect(menus.length).toBe(2);
    const kindButtons = Array.from(
      menus[1].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    );
    const allow = kindButtons.find((b) => b.textContent === "Allow");
    const deny = kindButtons.find((b) => b.textContent === "Deny");
    expect(allow?.disabled).toBe(true);
    expect(deny?.disabled).toBe(false);
  });

  it("calls onChangeKind with the new kind when a Change-kind option is activated", () => {
    const onChangeKind = vi.fn();
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    const props = { ...makeProps({ scopes }), onChangeKind };
    renderApp(root, props);
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Change kind")?.click();
    const kindButtons = Array.from(
      document
        .querySelectorAll<HTMLButtonElement>(".context-menu")[1]
        .querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    );
    kindButtons.find((b) => b.textContent === "Deny")?.click();
    expect(onChangeKind).toHaveBeenCalledTimes(1);
    const [path, scope, newKind] = onChangeKind.mock.calls[0];
    expect(path).toEqual(["permissions", "allow", 0]);
    expect(scope).toBe("project");
    expect(newKind).toBe("deny");
  });

  it("nests visible scopes under the current project in the Move-to submenu", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
      project_dir: "/tmp/myproject",
    });
    renderApp(root, makeProps({ scopes }));
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Move to")?.click();
    const menus = document.querySelectorAll<HTMLElement>(".context-menu");
    expect(menus.length).toBe(2);
    const moveItems = Array.from(
      menus[1].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    // User and User-Local are global; "myproject" is the basename of project_dir
    // and hosts a Local/Project submenu — Project is filtered out because the
    // source rule already lives in Project.
    expect(moveItems).toContain("User");
    expect(moveItems).toContain("User-Local");
    expect(moveItems).toContain("myproject");
  });

  it("closes the context menu on Escape", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    expect(document.querySelector(".context-menu")).not.toBeNull();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(document.querySelector(".context-menu")).toBeNull();
  });

  it("attaches a Paste-as menu to the scope column chrome", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const col = root.querySelector<HTMLElement>(".col");
    if (!col) throw new Error("expected column");
    rightClick(col);
    const labels = Array.from(
      document.querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    expect(labels).toContain("Paste as");
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
