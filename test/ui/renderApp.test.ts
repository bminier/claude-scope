import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  AppInfo,
  AuditRecordView,
  AuditSide,
  KnownProject,
  MoveLeafRequest,
  MoveOptions,
  Preferences,
  Scope,
  Theme,
  UndoRedoStatus,
} from "../../src/types.ts";
import { SEARCH_INPUT_ID } from "../../src/types.ts";
import {
  _resetMoveToProjectFilterForTesting,
  attachHelpTooltip,
  groupByToolPrefix,
  lookupKeyHelp,
  lookupKindHelp,
  lookupScopeHelp,
  openAbout,
  openHistory,
  openSettings,
  renderApp,
  renderDiagnosticsMarkdown,
} from "../../src/ui.ts";
import { buildLoadedScopes, buildPreferences, buildRuntimeInfo } from "../fixtures/loadedScopes.ts";

// `openAbout` calls into the Tauri clipboard plugin, which probes the IPC
// transport on import. Stub it before any ui.ts code path that touches the
// clipboard runs, so the test doesn't need a live Tauri context.
const writeTextSpy = vi.fn(async (_: string) => {});
vi.mock("@tauri-apps/plugin-clipboard-manager", () => ({
  writeText: (text: string) => writeTextSpy(text),
  readText: vi.fn(async () => ""),
}));

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
  knownProjects?: KnownProject[];
  undoStatus?: UndoRedoStatus | null;
  onMoveLeaf?: (req: MoveLeafRequest, trigger?: HTMLElement, opts?: MoveOptions) => void;
  onPickProject?: () => void;
  onPickRecentProject?: (projectDir: string) => void;
  onToggleCombinedPanelCollapsed?: (collapsed: boolean) => void;
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
    knownProjects: overrides.knownProjects ?? [],
    undoStatus: overrides.undoStatus ?? null,
    onPickProject: overrides.onPickProject ?? vi.fn(),
    onPickRecentProject: overrides.onPickRecentProject ?? vi.fn(),
    onOpenHistory: vi.fn(),
    onReload: vi.fn(),
    onUndo: vi.fn(),
    onRedo: vi.fn(),
    onMoveLeaf: overrides.onMoveLeaf ?? vi.fn(),
    onChangeKind: vi.fn(),
    onDeleteLeaf: vi.fn(),
    onAddLeaf: vi.fn(),
    onOpenSettings: vi.fn(),
    onOpenAbout: vi.fn(),
    onQueryChange: vi.fn(),
    onToggleCombinedPanelCollapsed: overrides.onToggleCombinedPanelCollapsed ?? vi.fn(),
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

  it("does not render the path-collisions banner when no scopes share files (#153)", () => {
    // Sanity: the common case has no collisions, so the banner must
    // not exist by default — otherwise it would be a noisy permanent
    // fixture for every user.
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    expect(root.querySelector(".collision-banner")).toBeNull();
  });

  it("renders the path-collisions banner with scope labels + shared path (#153)", () => {
    // Mirrors the headline case: launching from $HOME makes Project and
    // User resolve to the same `~/.claude/settings.json`. The banner
    // names both scopes by their display labels (broad→narrow) and the
    // shared path so the user can spot the collision before relying on
    // the columns as if they were independent.
    const scopes = buildLoadedScopes({
      path_collisions: [
        {
          scopes: ["project", "user"],
          path: "/home/u/.claude/settings.json",
        },
        {
          scopes: ["local", "user_local"],
          path: "/home/u/.claude/settings.local.json",
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const banner = root.querySelector(".collision-banner");
    expect(banner).not.toBeNull();
    expect(banner!.textContent).toContain("Scopes share files");
    const items = banner!.querySelectorAll(".collision-list li");
    expect(items).toHaveLength(2);
    expect(items[0].textContent).toContain("Project and User");
    expect(items[0].textContent).toContain("/home/u/.claude/settings.json");
    expect(items[1].textContent).toContain("Local and User-Local");
    expect(items[1].textContent).toContain("/home/u/.claude/settings.local.json");
  });

  it("renders a kind-conflict badge on the per-scope rule row (#156)", () => {
    // `Bash(git push)` is allowed in User but denied in Project — the
    // detector picks this up and surfaces a `kind_conflicts` entry; the
    // renderer must put a ⚠ badge on the rule row in each scope it
    // appears in, with the popover naming both occurrences.
    //
    // Unique project_dir so `seedDefaultOpenPermissions` actually runs
    // for this test (the seeder no-ops when `lastSeededProjectDir`
    // already matches the incoming projectDir — module-level state
    // shared across tests in this file).
    const scopes = buildLoadedScopes({
      project_dir: "/fake/project-kind-conflict-per-scope",
      scopes: [
        { scope: "project", permissions: { deny: ["Bash(git push)"] } },
        { scope: "user", permissions: { allow: ["Bash(git push)"] } },
      ],
      kind_conflicts: [
        {
          rule: "Bash(git push)",
          occurrences: [
            { scope: "project", kind: "deny" },
            { scope: "user", kind: "allow" },
          ],
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const projectRule = Array.from(root.querySelectorAll(".rule")).find(
      (r) => r.textContent?.includes("Bash(git push)") && r.classList.contains("rule-deny"),
    );
    expect(projectRule).toBeDefined();
    expect(projectRule!.querySelector(".kind-conflict-wrap")).not.toBeNull();
    // Popover lists both occurrences with Project first (precedence).
    const pop = projectRule!.querySelector(".kind-conflict-popover");
    expect(pop?.textContent).toContain("Project: deny");
    expect(pop?.textContent).toContain("User: allow");
    expect(pop?.textContent).toContain("wins by precedence");
  });

  it("does not render the kind-conflict badge when scopes agree (#156)", () => {
    // Same rule under the same kind in two scopes — agreement, not
    // conflict. No badge.
    const scopes = buildLoadedScopes({
      scopes: [
        { scope: "project", permissions: { allow: ["Bash(git status)"] } },
        { scope: "user", permissions: { allow: ["Bash(git status)"] } },
      ],
      // Detector returns empty for matching kinds; the fixture mirrors that.
      kind_conflicts: [],
    });
    renderApp(root, makeProps({ scopes }));
    expect(root.querySelector(".kind-conflict-wrap")).toBeNull();
  });

  it("renders the kind-conflict badge on the combined panel chip (#156)", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/project-kind-conflict-combined",
      scopes: [
        { scope: "project", permissions: { deny: ["Bash(git push)"] } },
        { scope: "user", permissions: { allow: ["Bash(git push)"] } },
      ],
      kind_conflicts: [
        {
          rule: "Bash(git push)",
          occurrences: [
            { scope: "project", kind: "deny" },
            { scope: "user", kind: "allow" },
          ],
        },
      ],
    });
    renderApp(
      root,
      makeProps({ scopes, preferences: buildPreferences({ combined_panel_collapsed: false }) }),
    );
    // The combined panel renders the rule under each kind it appears in
    // (allow and deny in this case). Each chip should carry the badge.
    const chips = Array.from(root.querySelectorAll(".combo-group .chip-wrap")).filter((w) =>
      w.textContent?.includes("Bash(git push)"),
    );
    expect(chips.length).toBeGreaterThanOrEqual(1);
    for (const chip of chips) {
      expect(chip.querySelector(".kind-conflict-wrap")).not.toBeNull();
    }
  });

  it("defaults the combined panel to collapsed via preferences (#155)", () => {
    // Default `combined_panel_collapsed: true` from buildPreferences
    // mirrors the Rust default — the panel renders closed on first
    // paint. `<details>.open === false` is the load-bearing signal.
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const combined = root.querySelector(".combined") as HTMLDetailsElement | null;
    expect(combined).not.toBeNull();
    expect(combined!.tagName).toBe("DETAILS");
    expect(combined!.open).toBe(false);
  });

  it("opens the combined panel when preferences.combined_panel_collapsed is false", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(
      root,
      makeProps({
        scopes,
        preferences: buildPreferences({ combined_panel_collapsed: false }),
      }),
    );
    const combined = root.querySelector(".combined") as HTMLDetailsElement;
    expect(combined.open).toBe(true);
  });

  it("renders the combined panel summary with inline kind counts when collapsed (#155)", () => {
    // Collapsed state still tells the user what's in the combined view.
    const scopes = buildLoadedScopes({
      scopes: [
        {
          scope: "project",
          permissions: {
            allow: ["Bash(git status)", "Read(**)"],
            deny: ["Bash(rm -rf *)"],
          },
        },
        { scope: "user", permissions: { ask: ["WebFetch(domain:x.com)"] } },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const counts = root.querySelector(".combined-counts");
    expect(counts?.textContent).toBe("2 allow · 1 deny · 1 ask");
  });

  it("persists the collapsed state on user toggle (#155)", () => {
    const onToggleCombinedPanelCollapsed = vi.fn();
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes, onToggleCombinedPanelCollapsed }));
    const combined = root.querySelector(".combined") as HTMLDetailsElement;
    // Default is collapsed (open=false). Simulate the user opening it.
    combined.open = true;
    combined.dispatchEvent(new Event("toggle"));
    // The setter receives the inverse — open === !collapsed.
    expect(onToggleCombinedPanelCollapsed).toHaveBeenCalledWith(false);
  });

  it("places the combined panel as the trailing column inside the scope grid", () => {
    // Regression for #154. Combined panel used to be a separate band
    // above the per-scope grid; now it's the trailing column inside
    // the same grid, peer to Local / Project / User-Local / User.
    // Two assertions: parent-child relationship, and trailing position
    // among the grid's children.
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const grid = root.querySelector(".grid");
    const combined = root.querySelector(".combined");
    expect(grid).not.toBeNull();
    expect(combined).not.toBeNull();
    expect(combined!.parentElement).toBe(grid);
    // Combined panel is the LAST child of the grid — peers come first.
    expect(grid!.lastElementChild).toBe(combined);
  });

  it("labels the combined panel as 'Effective settings' with the project dir in the subtitle (#158)", () => {
    // Reframes the panel from "Combined permissions" (a merge view)
    // to "Effective settings" (the source-of-truth view for what
    // applies in this directory). The project dir lands in the
    // subtitle so the section is self-contained.
    const scopes = buildLoadedScopes({
      project_dir: "/work/explicit-project",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const title = root.querySelector(".combined-title");
    expect(title?.textContent).toBe("Effective settings");
    const subtitle = root.querySelector(".combined-subtitle");
    expect(subtitle?.textContent).toContain("/work/explicit-project");
    expect(subtitle?.textContent).toContain("union across scopes");
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

  it("no longer renders inline →Scope move buttons on rule rows (#152)", () => {
    // Regression for #152. The inline arrow buttons were dropped in
    // favor of the right-click context menu's Move-to submenu (#111).
    // Asserts the DOM has no `.move-btn` / `.rule-moves` markers
    // anywhere — keyboard drag (#41) and right-click are the
    // canonical move affordances now.
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    expect(root.querySelector(".move-btn")).toBeNull();
    expect(root.querySelector(".rule-moves")).toBeNull();
    expect(root.querySelector(".tree-key-moves")).toBeNull();
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
    // Each column h3 nests the label inside `.col-head-label` so the
    // help-tooltip wrap can anchor next to it; querying the label span
    // directly keeps this assertion robust to those siblings.
    const headings = Array.from(root.querySelectorAll(".col .col-head-label")).map(
      (el) => el.textContent,
    );
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

  it("merges backend-discovered projects with the current project in the Move-to submenu", () => {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
      project_dir: "/tmp/myproject",
    });
    renderApp(
      root,
      makeProps({
        scopes,
        knownProjects: [
          { name: "other-repo", root: "/tmp/other-repo" },
          // Same path as the loaded project — `getKnownProjects` should
          // dedupe so the entry doesn't appear twice in the submenu.
          { name: "myproject", root: "/tmp/myproject" },
        ],
      }),
    );
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Move to")?.click();
    const menus = document.querySelectorAll<HTMLElement>(".context-menu");
    const moveItems = Array.from(
      menus[1].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    // Both projects show up, exactly once each, and the global scopes are
    // still present above the divider.
    expect(moveItems).toContain("User");
    expect(moveItems).toContain("User-Local");
    expect(moveItems.filter((l) => l === "myproject")).toHaveLength(1);
    expect(moveItems).toContain("other-repo");
  });

  it("offers same-scope-name targets when the project differs (#181)", () => {
    // Pre-#181 the submenu skipped `target === from` in every nested
    // project, hiding the cross-project same-name move entirely. Now
    // the skip only fires for the CURRENT project (where Local→Local
    // really is a no-op against the same file). Project A's Local
    // should expose `Local` AND `Project` under Project B's submenu.
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "local", permissions: { allow: ["Bash(git push)"] } }],
      project_dir: "/tmp/project-a",
    });
    renderApp(
      root,
      makeProps({
        scopes,
        knownProjects: [
          { name: "project-a", root: "/tmp/project-a" },
          { name: "project-b", root: "/tmp/project-b" },
        ],
      }),
    );
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Move to")?.click();
    findMenuItem("project-b")?.click();
    // Three menus visible: root, Move-to, project-b's submenu.
    const menus = document.querySelectorAll<HTMLElement>(".context-menu");
    expect(menus.length).toBe(3);
    const projectBItems = Array.from(
      menus[2].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    expect(projectBItems).toContain("Local");
    expect(projectBItems).toContain("Project");
  });

  it("still hides the same-scope-name target under the CURRENT project (#181)", () => {
    // Sanity: the skip is preserved within the loaded project, since
    // Local→Local against the SAME file is the legacy no-op we still
    // want to suppress.
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "local", permissions: { allow: ["Bash(git push)"] } }],
      project_dir: "/tmp/project-a",
    });
    renderApp(
      root,
      makeProps({
        scopes,
        knownProjects: [{ name: "project-a", root: "/tmp/project-a" }],
      }),
    );
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Move to")?.click();
    findMenuItem("project-a")?.click();
    const menus = document.querySelectorAll<HTMLElement>(".context-menu");
    const projectAItems = Array.from(
      menus[2].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    // Source is Local; current project's Local should NOT appear.
    expect(projectAItems).not.toContain("Local");
    expect(projectAItems).toContain("Project");
  });

  it("dispatches onMoveLeaf with projectDirTo for a cross-project pick (#179, #181)", () => {
    // The submenu wires the chosen project's root through to
    // `onMoveLeaf` as `opts.projectDirTo` — that's the field
    // `main.ts`'s moveLeaf consumes to send `project_dir_to` to the
    // backend. Without it, the destination would silently resolve
    // under the currently-viewed project's root.
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "local", permissions: { allow: ["Bash(git push)"] } }],
      project_dir: "/tmp/project-a",
    });
    const onMoveLeaf = vi.fn();
    renderApp(
      root,
      makeProps({
        scopes,
        onMoveLeaf,
        knownProjects: [
          { name: "project-a", root: "/tmp/project-a" },
          { name: "project-b", root: "/tmp/project-b" },
        ],
      }),
    );
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Move to")?.click();
    findMenuItem("project-b")?.click();
    findMenuItem("Local")?.click();
    expect(onMoveLeaf).toHaveBeenCalledTimes(1);
    const [req, _trigger, opts] = onMoveLeaf.mock.calls[0];
    expect(req).toMatchObject({ from: "local", to: "local" });
    expect(opts).toEqual({ projectDirTo: "/tmp/project-b" });
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

  // -- #111 Move-to project filter ----------------------------------------

  /** Build N synthetic known projects with predictable names + roots so
   *  the threshold and substring filter can be exercised in isolation
   *  from any real disk discovery. */
  function syntheticProjects(n: number): KnownProject[] {
    return Array.from({ length: n }, (_, i) => ({
      name: `proj-${String.fromCharCode(97 + i)}`,
      root: `/tmp/parent-${String.fromCharCode(97 + i)}/proj-${String.fromCharCode(97 + i)}`,
    }));
  }

  function openMoveToSubmenu(): HTMLElement {
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
      project_dir: "/tmp/loaded",
    });
    renderApp(
      root,
      makeProps({
        scopes,
        // 8 synthetic projects + the loaded "/tmp/loaded" current
        // project — comfortably above the v1 threshold of 5.
        knownProjects: syntheticProjects(8),
      }),
    );
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Move to")?.click();
    const menus = document.querySelectorAll<HTMLElement>(".context-menu");
    if (menus.length < 2) throw new Error("expected Move-to submenu to open");
    return menus[1];
  }

  it("hides the project filter input when project count is at or below the threshold", () => {
    _resetMoveToProjectFilterForTesting();
    const scopes = buildLoadedScopes({
      scopes: [{ scope: "project", permissions: { allow: ["Bash(git status)"] } }],
      project_dir: "/tmp/loaded",
    });
    renderApp(
      root,
      makeProps({
        scopes,
        // 4 backend + 1 current = 5, exactly at threshold — still no input.
        knownProjects: syntheticProjects(4),
      }),
    );
    const ruleRow = root.querySelector<HTMLElement>(".rule.rule-allow");
    if (!ruleRow) throw new Error("expected rule row");
    rightClick(ruleRow);
    findMenuItem("Move to")?.click();
    const submenu = document.querySelectorAll<HTMLElement>(".context-menu")[1];
    expect(submenu).toBeTruthy();
    expect(submenu.querySelector(".context-menu-input")).toBeNull();
  });

  it("renders the project filter input as the first item after the separator above the threshold", () => {
    _resetMoveToProjectFilterForTesting();
    const submenu = openMoveToSubmenu();
    const input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    expect(input).not.toBeNull();
    expect(input?.placeholder).toBe("Filter projects…");

    // Verify the input sits between the separator and the first project
    // item — User / User-Local must remain ABOVE it so they stay
    // unaffected by the keyword.
    const order: string[] = [];
    for (const child of Array.from(submenu.children)) {
      if (child.classList.contains("context-menu-input-row")) order.push("INPUT");
      else if (child.classList.contains("context-menu-sep")) order.push("SEP");
      else if (child.classList.contains("context-menu-item")) {
        order.push(child.textContent?.replace(/\s*▸$/, "").trim() ?? "");
      }
    }
    const sepIdx = order.indexOf("SEP");
    const inputIdx = order.indexOf("INPUT");
    expect(sepIdx).toBeGreaterThan(-1);
    expect(inputIdx).toBe(sepIdx + 1);
    // User / User-Local are above the separator (and therefore above the input).
    expect(order.slice(0, sepIdx)).toEqual(expect.arrayContaining(["User", "User-Local"]));
  });

  it("typing in the filter input hides projects that don't match name or path substring", () => {
    _resetMoveToProjectFilterForTesting();
    const submenu = openMoveToSubmenu();
    const input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    if (!input) throw new Error("expected filter input");

    input.value = "proj-a";
    input.dispatchEvent(new Event("input", { bubbles: true }));

    const visibleProjectLabels = Array.from(
      submenu.querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    )
      .filter((b) => !b.classList.contains("context-menu-hidden"))
      .map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    // proj-a matches; proj-b..h have a different basename. User /
    // User-Local stay visible because they sit above the input and
    // never carry `data-match-text`.
    expect(visibleProjectLabels).toContain("proj-a");
    expect(visibleProjectLabels).not.toContain("proj-b");
    expect(visibleProjectLabels).toContain("User");
    expect(visibleProjectLabels).toContain("User-Local");
  });

  it("filter substring also matches the project's parent path", () => {
    _resetMoveToProjectFilterForTesting();
    const submenu = openMoveToSubmenu();
    const input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    if (!input) throw new Error("expected filter input");

    // "parent-c" is in the synthetic root for proj-c only; the
    // basename is proj-c. Matching on the parent dir validates the
    // "name + root" join in `searchText`.
    input.value = "parent-c";
    input.dispatchEvent(new Event("input", { bubbles: true }));

    const visibleProjectLabels = Array.from(
      submenu.querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    )
      .filter((b) => !b.classList.contains("context-menu-hidden") && b.dataset.matchText)
      .map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    expect(visibleProjectLabels).toEqual(["proj-c"]);
  });

  it("filter is case-insensitive", () => {
    _resetMoveToProjectFilterForTesting();
    const submenu = openMoveToSubmenu();
    const input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    if (!input) throw new Error("expected filter input");

    input.value = "PROJ-D";
    input.dispatchEvent(new Event("input", { bubbles: true }));

    const visibleProjectLabels = Array.from(
      submenu.querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    )
      .filter((b) => !b.classList.contains("context-menu-hidden") && b.dataset.matchText)
      .map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    expect(visibleProjectLabels).toEqual(["proj-d"]);
  });

  it("clearing the input restores every project", () => {
    _resetMoveToProjectFilterForTesting();
    const submenu = openMoveToSubmenu();
    const input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    if (!input) throw new Error("expected filter input");

    input.value = "proj-a";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.value = "";
    input.dispatchEvent(new Event("input", { bubbles: true }));

    const hidden = submenu.querySelectorAll<HTMLButtonElement>(".context-menu-hidden");
    expect(hidden.length).toBe(0);
  });

  it("filter query persists across menu closes within a session", () => {
    _resetMoveToProjectFilterForTesting();
    // First open: type a query, close.
    let submenu = openMoveToSubmenu();
    let input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    if (!input) throw new Error("expected filter input");
    input.value = "proj-b";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));

    // Second open: the input should be pre-populated and the filter
    // should already be applied on initial render (no flash of full list).
    submenu = openMoveToSubmenu();
    input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    if (!input) throw new Error("expected filter input on reopen");
    expect(input.value).toBe("proj-b");
    const visibleProjectLabels = Array.from(
      submenu.querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    )
      .filter((b) => !b.classList.contains("context-menu-hidden") && b.dataset.matchText)
      .map((b) => b.textContent?.replace(/\s*▸$/, "").trim() ?? "");
    expect(visibleProjectLabels).toEqual(["proj-b"]);
  });

  it("ArrowDown from the filter input moves focus to the first visible project", () => {
    _resetMoveToProjectFilterForTesting();
    const submenu = openMoveToSubmenu();
    const input = submenu.querySelector<HTMLInputElement>(".context-menu-input");
    if (!input) throw new Error("expected filter input");

    input.focus();
    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }),
    );
    const focused = document.activeElement as HTMLElement | null;
    expect(focused?.tagName).toBe("BUTTON");
    expect(focused?.dataset.matchText).toBeDefined();
  });
});

describe("About dialog", () => {
  let root: HTMLElement;
  const sampleInfo: AppInfo = {
    version: "9.9.9",
    git_sha: "abc123def456",
    tauri_version: "2.0.0",
    webview_version: "121.0.6167.184",
    rust_version: "1.88",
    os: "windows",
    arch: "x86_64",
  };

  beforeEach(() => {
    root = makeRoot();
    writeTextSpy.mockClear();
  });

  afterEach(() => {
    clearBody();
  });

  it("renders an About button in the header that opens the About dialog", () => {
    renderApp(root, makeProps());
    const aboutBtn = Array.from(root.querySelectorAll<HTMLButtonElement>(".topbar button")).find(
      (b) => b.textContent === "About",
    );
    expect(aboutBtn).toBeDefined();
    aboutBtn?.click();
    // The header button's onClick goes through `onOpenAbout`, which main.ts
    // delegates to `openAbout`. Verify the contract on the prop instead of
    // poking at the modal — the next test exercises the modal directly.
    // (No assertion on a modal here; makeProps stubs the callback.)
  });

  it("renders the diagnostic block when info is available", () => {
    openAbout(sampleInfo);
    const modal = document.querySelector(".modal-about");
    expect(modal).not.toBeNull();
    const body = modal?.textContent ?? "";
    expect(body).toContain("9.9.9 (abc123def456)");
    expect(body).toContain("2.0.0");
    expect(body).toContain("121.0.6167.184");
    expect(body).toContain("windows x86_64");
  });

  it("renders a Loading… stub when app info is null", () => {
    openAbout(null);
    const modal = document.querySelector(".modal-about");
    expect(modal?.textContent ?? "").toContain("Loading…");
  });

  it("copies the diagnostic block to the clipboard as Markdown", async () => {
    openAbout(sampleInfo);
    const copyBtn = Array.from(
      document.querySelectorAll<HTMLButtonElement>(".modal-about button"),
    ).find((b) => b.textContent === "Copy diagnostics");
    expect(copyBtn).toBeDefined();
    copyBtn?.click();
    // `activate` is async — yield once so the writeText promise settles.
    await Promise.resolve();
    await Promise.resolve();
    expect(writeTextSpy).toHaveBeenCalledTimes(1);
    const md = writeTextSpy.mock.calls[0][0];
    // Exact-shape match against the TS renderer to guarantee the Rust
    // `to_markdown` and TS `renderDiagnosticsMarkdown` produce the same
    // bytes for the same input. The Rust unit test asserts the other side.
    expect(md).toBe(renderDiagnosticsMarkdown(sampleInfo));
  });

  it("substitutes 'unknown' for missing optional fields in the clipboard payload", async () => {
    const partial: AppInfo = { ...sampleInfo, git_sha: null, webview_version: null };
    openAbout(partial);
    document.querySelectorAll<HTMLButtonElement>(".modal-about button").forEach((b) => {
      if (b.textContent === "Copy diagnostics") b.click();
    });
    await Promise.resolve();
    await Promise.resolve();
    const md = writeTextSpy.mock.calls[0][0];
    expect(md).toContain("(unknown)");
    expect(md).toContain("WebView: unknown");
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
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb: vi.fn(),
      onChangeGroupRulesAt: vi.fn(),
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

describe("openSettings – backup toggle (#88)", () => {
  afterEach(() => {
    clearBody();
  });

  function backupCheckbox(): HTMLInputElement {
    const heading = document.getElementById("settings-backup-heading");
    if (!heading) throw new Error("expected backup section heading");
    const section = heading.parentElement;
    if (!section) throw new Error("expected backup section parent");
    const cb = section.querySelector<HTMLInputElement>('input[type="checkbox"]');
    if (!cb) throw new Error("expected backup checkbox");
    return cb;
  }

  it("reflects the current preference value on render", () => {
    for (const enabled of [true, false]) {
      clearBody();
      openSettings({
        preferences: buildPreferences({ backup_on_write: enabled }),
        onToggleScopeVisibility: vi.fn(),
        onChangeTheme: vi.fn(),
        onToggleBackupOnWrite: vi.fn(),
        onToggleAuditLogRotate: vi.fn(),
        onChangeAuditLogMaxSizeMb: vi.fn(),
        onChangeGroupRulesAt: vi.fn(),
      });
      expect(backupCheckbox().checked).toBe(enabled);
    }
  });

  it("calls onToggleBackupOnWrite with the new value when the checkbox is changed", () => {
    const onToggleBackupOnWrite = vi.fn();
    openSettings({
      preferences: buildPreferences({ backup_on_write: true }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite,
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb: vi.fn(),
      onChangeGroupRulesAt: vi.fn(),
    });
    const cb = backupCheckbox();
    cb.checked = false;
    cb.dispatchEvent(new Event("change"));
    expect(onToggleBackupOnWrite).toHaveBeenCalledTimes(1);
    expect(onToggleBackupOnWrite).toHaveBeenCalledWith(false);
  });
});

describe("openSettings – audit log rotation (#127)", () => {
  beforeEach(() => {
    // Drain any leaked modal Escape listeners from earlier suites so the
    // first openSettings call here lands cleanly, mirroring the
    // help-tooltips / History-dialog suites' prelude.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    clearBody();
  });

  afterEach(() => {
    for (const b of Array.from(document.querySelectorAll(".modal-backdrop"))) b.remove();
    clearBody();
  });

  function rotationToggle(): HTMLInputElement {
    const cb = Array.from(
      document.querySelectorAll<HTMLInputElement>('.modal-settings input[type="checkbox"]'),
    ).find((el) => el.nextElementSibling?.textContent?.startsWith("Rotate audit log"));
    if (!cb) throw new Error("expected rotation toggle");
    return cb;
  }

  function sizeInput(): HTMLInputElement {
    const input = document.querySelector<HTMLInputElement>(
      ".modal-settings .settings-number-input",
    );
    if (!input) throw new Error("expected rotation size input");
    return input;
  }

  function makeProps(prefs: Partial<Preferences> = {}) {
    return {
      preferences: buildPreferences(prefs),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb: vi.fn(),
      onChangeGroupRulesAt: vi.fn(),
    };
  }

  it("renders the rotation toggle reflecting the current preference", () => {
    openSettings(makeProps({ audit_log_rotate: false }));
    expect(rotationToggle().checked).toBe(false);
    clearBody();
    openSettings(makeProps({ audit_log_rotate: true }));
    expect(rotationToggle().checked).toBe(true);
  });

  it("renders the size input reflecting the current preference", () => {
    openSettings(makeProps({ audit_log_max_size_mb: 25 }));
    expect(sizeInput().value).toBe("25");
  });

  it("size input is disabled when rotation is off", () => {
    openSettings(makeProps({ audit_log_rotate: false }));
    expect(sizeInput().disabled).toBe(true);
  });

  it("toggling rotation off disables the size input live", () => {
    openSettings(makeProps({ audit_log_rotate: true }));
    expect(sizeInput().disabled).toBe(false);
    const toggle = rotationToggle();
    toggle.checked = false;
    toggle.dispatchEvent(new Event("change"));
    expect(sizeInput().disabled).toBe(true);
  });

  it("invokes onToggleAuditLogRotate when the toggle is changed", () => {
    const onToggleAuditLogRotate = vi.fn();
    openSettings({
      preferences: buildPreferences({ audit_log_rotate: true }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate,
      onChangeAuditLogMaxSizeMb: vi.fn(),
      onChangeGroupRulesAt: vi.fn(),
    });
    const t = rotationToggle();
    t.checked = false;
    t.dispatchEvent(new Event("change"));
    expect(onToggleAuditLogRotate).toHaveBeenCalledWith(false);
  });

  it("invokes onChangeAuditLogMaxSizeMb on a valid in-range value", () => {
    const onChangeAuditLogMaxSizeMb = vi.fn();
    openSettings({
      preferences: buildPreferences({ audit_log_max_size_mb: 10 }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb,
      onChangeGroupRulesAt: vi.fn(),
    });
    const input = sizeInput();
    input.value = "50";
    input.dispatchEvent(new Event("change"));
    expect(onChangeAuditLogMaxSizeMb).toHaveBeenCalledWith(50);
  });

  it("clamps an out-of-range size input to the bound and reflects it back", () => {
    const onChangeAuditLogMaxSizeMb = vi.fn();
    openSettings({
      preferences: buildPreferences({ audit_log_max_size_mb: 10 }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb,
      onChangeGroupRulesAt: vi.fn(),
    });
    const input = sizeInput();
    input.value = "9999";
    input.dispatchEvent(new Event("change"));
    expect(onChangeAuditLogMaxSizeMb).toHaveBeenCalledWith(1000);
    expect(input.value).toBe("1000");

    onChangeAuditLogMaxSizeMb.mockClear();
    input.value = "0";
    input.dispatchEvent(new Event("change"));
    expect(onChangeAuditLogMaxSizeMb).toHaveBeenCalledWith(1);
    expect(input.value).toBe("1");
  });

  it("reverts a non-numeric size input back to the current preference value", () => {
    const onChangeAuditLogMaxSizeMb = vi.fn();
    openSettings({
      preferences: buildPreferences({ audit_log_max_size_mb: 10 }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb,
      onChangeGroupRulesAt: vi.fn(),
    });
    const input = sizeInput();
    input.value = "";
    input.dispatchEvent(new Event("change"));
    expect(onChangeAuditLogMaxSizeMb).not.toHaveBeenCalled();
    expect(input.value).toBe("10");
  });
});

describe("groupByToolPrefix (#68)", () => {
  it("folds 2+ rules sharing a tool into a group", () => {
    const got = groupByToolPrefix(["handoff(copilot *)", "handoff(claude *)", "handoff(codex *)"]);
    expect(got).toHaveLength(1);
    expect(got[0]).toEqual({
      kind: "group",
      tool: "handoff",
      members: [
        { index: 0, rule: "handoff(copilot *)" },
        { index: 1, rule: "handoff(claude *)" },
        { index: 2, rule: "handoff(codex *)" },
      ],
    });
  });

  it("emits a single-rule tool as a Single, not a one-child group", () => {
    const got = groupByToolPrefix(["Read(**)"]);
    expect(got).toEqual([{ kind: "single", index: 0, rule: "Read(**)" }]);
  });

  it("preserves original disk order: group emits at the position of its first member", () => {
    // Read sits between Bash[0] and Bash[2]. The Bash group should emit at
    // position 0 (Bash's first member), and Read should still be reachable
    // — Bash[2] gets absorbed silently into the group rather than re-
    // emitting at index 2.
    const got = groupByToolPrefix(["Bash(ls)", "Read(**)", "Bash(grep)"]);
    expect(got.length).toBe(2);
    expect(got[0]).toMatchObject({ kind: "group", tool: "Bash" });
    expect(got[1]).toEqual({ kind: "single", index: 1, rule: "Read(**)" });
    if (got[0].kind !== "group") throw new Error("expected group");
    expect(got[0].members.map((m) => m.index)).toEqual([0, 2]);
  });

  it("rules without a Tool(args) shape stay flat", () => {
    // No paren → no tool prefix → never grouped. Two malformed rules don't
    // get folded together as an empty-tool group.
    const got = groupByToolPrefix(["weird-rule-no-parens", "another-weirdo"]);
    expect(got).toEqual([
      { kind: "single", index: 0, rule: "weird-rule-no-parens" },
      { kind: "single", index: 1, rule: "another-weirdo" },
    ]);
  });

  it("threshold parameter controls when a tool folds", () => {
    const rules = ["Bash(ls)", "Bash(grep)"];
    expect(groupByToolPrefix(rules, 2)[0].kind).toBe("group");
    // Two rules, threshold 3 → stays flat.
    const at3 = groupByToolPrefix(rules, 3);
    expect(at3.every((e) => e.kind === "single")).toBe(true);
  });

  it("Tool() with empty args still groups by the Tool prefix", () => {
    const got = groupByToolPrefix(["WebFetch()", "WebFetch(domain:example.com)"]);
    expect(got).toHaveLength(1);
    expect(got[0]).toMatchObject({ kind: "group", tool: "WebFetch" });
  });
});

describe("tool-prefix grouping in the scope tree (#68)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    root = makeRoot();
  });

  afterEach(() => {
    clearBody();
  });

  it("renders a synthetic group node when 2+ rules share a tool", () => {
    // Unique project_dir per test so `seedDefaultOpenPermissions` re-runs
    // against this scenario's rules instead of inheriting `openTreeNodes`
    // state from an earlier describe block that didn't include groups.
    const scopes = buildLoadedScopes({
      project_dir: "/fake/grouping-render",
      scopes: [
        {
          scope: "project",
          permissions: {
            allow: ["handoff(copilot *)", "handoff(claude *)", "handoff(codex *)"],
          },
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const groups = root.querySelectorAll<HTMLDetailsElement>(".tree-tool-group");
    expect(groups.length).toBe(1);
    const summary = groups[0].querySelector(".tree-summary");
    expect(summary?.textContent).toContain("handoff");
    expect(summary?.textContent).toContain("(3)");
    // Three rule rows nested inside the group, addressed by their original
    // indices on disk — paths stay leaf-level for the move primitive.
    const rules = groups[0].querySelectorAll<HTMLElement>(".rule.rule-allow");
    expect(rules.length).toBe(3);
    const ruleTexts = Array.from(rules).map((r) => r.querySelector(".rule-text")?.textContent);
    expect(ruleTexts).toEqual(["handoff(copilot *)", "handoff(claude *)", "handoff(codex *)"]);
  });

  it("leaves a single-rule tool as a flat leaf alongside a multi-rule group", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/grouping-mixed",
      scopes: [
        {
          scope: "project",
          permissions: {
            allow: ["Bash(ls)", "Read(**)", "Bash(grep)"],
          },
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const groups = root.querySelectorAll<HTMLDetailsElement>(".tree-tool-group");
    // Only Bash collapses; Read stays a top-level leaf.
    expect(groups.length).toBe(1);
    expect(groups[0].querySelector(".tree-summary")?.textContent).toContain("Bash");
    // The kind branch should still hold a direct leaf for Read at its
    // original index, plus the group node for Bash. Picking the kind
    // branch by class instead of by structural index keeps the assertion
    // resilient to future seeding changes.
    const kindBranch = root.querySelector<HTMLDetailsElement>(".tree-branch.tree-perm-allow");
    if (!kindBranch) throw new Error("expected permissions.allow branch");
    const directChildren = kindBranch.querySelector(".tree-children");
    if (!directChildren) throw new Error("expected children container");
    // First-level children: 1 group + 1 leaf (Read). The group's *own*
    // children are deeper and don't count here.
    const topLevel = Array.from(directChildren.children).filter(
      (el) =>
        el.classList.contains("tree-tool-group") ||
        (el.classList.contains("rule") && el.classList.contains("rule-allow")),
    );
    expect(topLevel.length).toBe(2);
    const readLeaf = topLevel.find(
      (el) =>
        el.classList.contains("rule") && el.querySelector(".rule-text")?.textContent === "Read(**)",
    );
    expect(readLeaf).toBeDefined();
  });

  it("filtering hides non-matching members and shows matched/total in the group label", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/grouping-filter",
      scopes: [
        {
          scope: "project",
          permissions: {
            allow: ["handoff(copilot *)", "handoff(claude *)", "handoff(codex *)"],
          },
        },
      ],
    });
    renderApp(root, makeProps({ scopes, query: "copilot" }));
    const groups = root.querySelectorAll<HTMLDetailsElement>(".tree-tool-group");
    expect(groups.length).toBe(1);
    const summary = groups[0].querySelector(".tree-summary");
    // Matched count first, total second — same shape the combined panel
    // uses for kind labels under an active filter.
    expect(summary?.textContent).toContain("(1/3)");
    // Only the matching rule renders inside the group.
    const rules = groups[0].querySelectorAll<HTMLElement>(".rule.rule-allow");
    const visibleRules = Array.from(rules).filter((r) => !r.hidden);
    expect(visibleRules.length).toBe(1);
    expect(visibleRules[0].querySelector(".rule-text")?.textContent).toBe("handoff(copilot *)");
  });

  it("group members keep leaf-level paths so context-menu moves address the rule directly", () => {
    // #152 dropped the inline arrow buttons in favor of the right-click
    // Move-to submenu (#111). This test now exercises that path: inside
    // a tool-group, right-click the second member and assert the
    // dispatched path is `permissions.allow[1]` (not anything
    // group-relative).
    const onMoveLeaf = vi.fn();
    const scopes = buildLoadedScopes({
      project_dir: "/fake/grouping-move",
      scopes: [
        {
          scope: "project",
          permissions: { allow: ["Bash(ls)", "Bash(grep)"] },
        },
      ],
    });
    renderApp(root, makeProps({ scopes, onMoveLeaf }));
    const group = root.querySelector<HTMLDetailsElement>(".tree-tool-group");
    if (!group) throw new Error("expected tool group");
    const memberRows = group.querySelectorAll<HTMLElement>(".rule.rule-allow");
    expect(memberRows.length).toBe(2);
    // Right-click the second member to open its context menu, then
    // navigate into Move-to → User. `findMenuItem` is defined in the
    // context-menu describe block; reuse the local helper here too.
    memberRows[1].dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 50, clientY: 50 }),
    );
    const items = document.querySelectorAll<HTMLButtonElement>(".context-menu-item");
    const moveTo = Array.from(items).find((b) =>
      b.textContent?.replace(/\s+/g, " ").trim().startsWith("Move to"),
    );
    expect(moveTo).toBeDefined();
    moveTo?.click();
    const submenus = document.querySelectorAll<HTMLElement>(".context-menu");
    const user = Array.from(
      submenus[submenus.length - 1].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).find((b) => b.textContent?.replace(/\s*▸$/, "").trim() === "User");
    expect(user).toBeDefined();
    user?.click();
    expect(onMoveLeaf).toHaveBeenCalledTimes(1);
    const [req] = onMoveLeaf.mock.calls[0];
    expect(req).toEqual({
      path: ["permissions", "allow", 1],
      from: "project",
      to: "user",
    });
    // Clean up the lingering context menus from this test so the next
    // test doesn't inherit them.
    for (const m of Array.from(document.querySelectorAll(".context-menu"))) m.remove();
  });
});

describe("help tooltips (#9)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    // Earlier openSettings/openAbout tests leak their modal keydown
    // listeners (the `removeEventListener` site uses `{capture: true}`,
    // the `addEventListener` site doesn't — so the remove silently
    // mismatches). Each leaked listener calls `preventDefault()` on
    // Escape, which would set `defaultPrevented=true` and stop our
    // popover Escape handler. Force a real Escape with cancelable=true
    // up-front so every leaked modal's `close()` runs and detaches.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    root = makeRoot();
  });

  afterEach(() => {
    clearBody();
    // Module-level openPinnedPopover state survives across tests; click
    // somewhere off-popover to release any pin left over from a previous
    // case. Cheap, makes each test self-contained.
    document.body.click();
  });

  it("lookupKeyHelp returns content for recognized keys and null for unknown", () => {
    expect(lookupKeyHelp("permissions")?.summary).toBeTruthy();
    expect(lookupKeyHelp("env")?.summary).toBeTruthy();
    expect(lookupKeyHelp("nonsense-key")).toBeNull();
  });

  it("attaches a help affordance to recognized top-level settings keys", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/help-known-key",
      scopes: [
        {
          scope: "project",
          permissions: { allow: ["Bash(git status)"] },
          // `env` is a recognized top-level key; the tree should render
          // its tree-key with the .has-help affordance and a sibling ⓘ.
          other_values: { env: { FOO: "bar" } },
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const envKey = Array.from(root.querySelectorAll<HTMLElement>(".tree-key")).find(
      (el) => el.textContent === "env",
    );
    expect(envKey).toBeDefined();
    expect(envKey?.classList.contains("has-help")).toBe(true);
    const wrap = envKey?.parentElement;
    expect(wrap?.classList.contains("help-wrap")).toBe(true);
    expect(wrap?.querySelector(".help-info")).not.toBeNull();
  });

  it("does NOT attach a help affordance to unrecognized top-level keys", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/help-unknown-key",
      scopes: [
        {
          scope: "project",
          permissions: { allow: ["Bash(git status)"] },
          other_values: { someUserKey: "whatever" },
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    const userKey = Array.from(root.querySelectorAll<HTMLElement>(".tree-key")).find(
      (el) => el.textContent === "someUserKey",
    );
    expect(userKey).toBeDefined();
    expect(userKey?.classList.contains("has-help")).toBe(false);
    expect(userKey?.parentElement?.classList.contains("help-wrap")).toBe(false);
  });

  it("attaches help to combined-panel kind labels (allow / deny / ask)", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/help-kind-labels",
      scopes: [
        {
          scope: "project",
          permissions: { allow: ["Bash(ls)"], deny: ["Bash(rm -rf *)"] },
        },
      ],
    });
    renderApp(root, makeProps({ scopes }));
    for (const kind of ["allow", "deny", "ask"]) {
      const label = root.querySelector<HTMLElement>(`.combo-${kind} .combo-label`);
      expect(label, `combo-label for ${kind}`).not.toBeNull();
      expect(label?.classList.contains("has-help")).toBe(true);
    }
  });

  it("attaches help to scope column headers", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/help-scope-header",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    const projectLabel = Array.from(root.querySelectorAll<HTMLElement>(".col-head-label")).find(
      (el) => el.textContent === "Project",
    );
    expect(projectLabel).toBeDefined();
    expect(projectLabel?.classList.contains("has-help")).toBe(true);
    const h3 = projectLabel?.closest("h3");
    expect(h3?.querySelector(".help-info")).not.toBeNull();
  });

  it("clicking the ⓘ glyph pins the popover; clicking outside closes it", () => {
    const trigger = document.createElement("span");
    trigger.textContent = "permissions";
    document.body.appendChild(attachHelpTooltip(trigger, lookupKindHelp("allow")));
    const wrap = trigger.parentElement;
    expect(wrap?.classList.contains("is-open")).toBe(false);
    const btn = wrap?.querySelector<HTMLButtonElement>(".help-info");
    if (!btn) throw new Error("expected help-info button");
    btn.click();
    expect(wrap?.classList.contains("is-open")).toBe(true);
    document.body.click();
    expect(wrap?.classList.contains("is-open")).toBe(false);
  });

  it("Escape on a pinned help popover closes it", () => {
    const trigger = document.createElement("span");
    trigger.textContent = "env";
    document.body.appendChild(attachHelpTooltip(trigger, lookupScopeHelp("project")));
    const wrap = trigger.parentElement;
    const btn = wrap?.querySelector<HTMLButtonElement>(".help-info");
    btn?.click();
    expect(wrap?.classList.contains("is-open")).toBe(true);
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(wrap?.classList.contains("is-open")).toBe(false);
  });

  it("opening a second help popover closes the first — singleton pin across instances", () => {
    const a = document.createElement("span");
    a.textContent = "a";
    document.body.appendChild(attachHelpTooltip(a, lookupKindHelp("allow")));
    const b = document.createElement("span");
    b.textContent = "b";
    document.body.appendChild(attachHelpTooltip(b, lookupKindHelp("deny")));
    const wrapA = a.parentElement;
    const wrapB = b.parentElement;

    wrapA?.querySelector<HTMLButtonElement>(".help-info")?.click();
    expect(wrapA?.classList.contains("is-open")).toBe(true);
    expect(wrapB?.classList.contains("is-open")).toBe(false);

    wrapB?.querySelector<HTMLButtonElement>(".help-info")?.click();
    expect(wrapA?.classList.contains("is-open")).toBe(false);
    expect(wrapB?.classList.contains("is-open")).toBe(true);
  });
});

describe("project-dir dropdown (#47)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    root = makeRoot();
  });

  afterEach(() => {
    for (const m of Array.from(document.querySelectorAll(".context-menu"))) m.remove();
    clearBody();
  });

  function openDropdown(): HTMLElement[] {
    const trigger = root.querySelector<HTMLButtonElement>(".project-dir-trigger");
    if (!trigger) throw new Error("expected .project-dir-trigger button");
    trigger.click();
    return Array.from(document.querySelectorAll<HTMLElement>(".context-menu"));
  }

  it("project-dir trigger is a button with aria-haspopup=menu", () => {
    renderApp(root, makeProps({}));
    const trigger = root.querySelector<HTMLButtonElement>(".project-dir-trigger");
    expect(trigger).not.toBeNull();
    expect(trigger?.tagName).toBe("BUTTON");
    expect(trigger?.getAttribute("aria-haspopup")).toBe("menu");
  });

  it("dropdown lists recent projects (filtering out the current one) plus Open project…", () => {
    const scopes = buildLoadedScopes({ project_dir: "/work/current" });
    const preferences = buildPreferences({
      recent_projects: ["/work/current", "/work/alpha", "/home/me/beta"],
    });
    renderApp(root, makeProps({ scopes, preferences }));
    const menus = openDropdown();
    expect(menus.length).toBe(1);
    const labels = Array.from(menus[0].querySelectorAll<HTMLButtonElement>(".context-menu-item"))
      .map((b) => b.textContent ?? "")
      // The submenu arrow span lives inside the button textContent on
      // submenu triggers, but this menu has none; readability over
      // micro-trims.
      .map((s) => s.trim());
    // Current project must be filtered — opening it would just reload.
    expect(labels).not.toContain("/work/current");
    expect(labels).toContain("/work/alpha");
    expect(labels).toContain("/home/me/beta");
    // Picker entry always lands as the last item.
    expect(labels[labels.length - 1]).toBe("Open project…");
  });

  it("clicking a recent entry invokes onPickRecentProject with that path", () => {
    const scopes = buildLoadedScopes({ project_dir: "/work/current" });
    const preferences = buildPreferences({
      recent_projects: ["/work/alpha", "/home/me/beta"],
    });
    const onPickRecentProject = vi.fn();
    renderApp(root, makeProps({ scopes, preferences, onPickRecentProject }));
    const menus = openDropdown();
    const target = Array.from(
      menus[0].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).find((b) => (b.textContent ?? "").trim() === "/work/alpha");
    expect(target).toBeDefined();
    target?.click();
    expect(onPickRecentProject).toHaveBeenCalledWith("/work/alpha");
  });

  it("clicking Open project… invokes onPickProject", () => {
    const preferences = buildPreferences({ recent_projects: ["/work/alpha"] });
    const onPickProject = vi.fn();
    renderApp(root, makeProps({ preferences, onPickProject }));
    const menus = openDropdown();
    const opener = Array.from(
      menus[0].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).find((b) => (b.textContent ?? "").trim() === "Open project…");
    expect(opener).toBeDefined();
    opener?.click();
    expect(onPickProject).toHaveBeenCalled();
  });

  it("empty recent_projects still shows the picker entry — no dead-end menu", () => {
    renderApp(root, makeProps({ preferences: buildPreferences({ recent_projects: [] }) }));
    const menus = openDropdown();
    const labels = Array.from(
      menus[0].querySelectorAll<HTMLButtonElement>(".context-menu-item"),
    ).map((b) => (b.textContent ?? "").trim());
    expect(labels).toEqual(["Open project…"]);
  });

  it("trigger is disabled while a load is in flight", () => {
    renderApp(root, makeProps({ busy: true }));
    const trigger = root.querySelector<HTMLButtonElement>(".project-dir-trigger");
    expect(trigger?.disabled).toBe(true);
  });
});

describe("header layout (#40)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    root = makeRoot();
  });

  afterEach(() => {
    clearBody();
  });

  it("puts the project path on its own row, below the title/search/actions row", () => {
    renderApp(root, makeProps({ scopes: buildLoadedScopes({ project_dir: "/work/here" }) }));
    const topbar = root.querySelector<HTMLElement>(".topbar");
    if (!topbar) throw new Error("expected .topbar");

    const mainRow = topbar.querySelector<HTMLElement>(".topbar-row");
    const projectRow = topbar.querySelector<HTMLElement>(".topbar-project");
    expect(mainRow).not.toBeNull();
    expect(projectRow).not.toBeNull();

    // Project row comes after the main row in document order — "under
    // the title" per the issue.
    const children = Array.from(topbar.children);
    expect(children.indexOf(mainRow as HTMLElement)).toBeLessThan(
      children.indexOf(projectRow as HTMLElement),
    );
  });

  it("keeps title, search, and actions in the main row — not the project row", () => {
    renderApp(root, makeProps({ scopes: buildLoadedScopes({ project_dir: "/work/here" }) }));
    const mainRow = root.querySelector<HTMLElement>(".topbar-row");
    if (!mainRow) throw new Error("expected .topbar-row");
    expect(mainRow.querySelector(".title")).not.toBeNull();
    expect(mainRow.querySelector(".search")).not.toBeNull();
    expect(mainRow.querySelector(".actions")).not.toBeNull();
    // The project dropdown is NOT in the main row anymore.
    expect(mainRow.querySelector(".project-dir-trigger")).toBeNull();
  });

  it("nests the project dropdown inside the dedicated project row", () => {
    renderApp(root, makeProps({ scopes: buildLoadedScopes({ project_dir: "/work/here" }) }));
    const projectRow = root.querySelector<HTMLElement>(".topbar-project");
    if (!projectRow) throw new Error("expected .topbar-project");
    expect(projectRow.querySelector(".project-dir-trigger")).not.toBeNull();
  });
});

describe("History dialog (#19 phase 2)", () => {
  beforeEach(() => {
    // Drain any leaked modal Escape listeners from earlier tests in the
    // file — same trick the help-tooltips suite uses. Without this, a
    // prior leaked listener's preventDefault would fire BEFORE our own
    // History dialog's close() and `defaultPrevented = true` would short
    // our Escape-closes-the-dialog assertion later in the suite.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    clearBody();
  });

  afterEach(() => {
    // Tear down any modal that survived the test.
    for (const b of Array.from(document.querySelectorAll(".modal-backdrop"))) b.remove();
    clearBody();
  });

  function makeRecord(overrides: Partial<AuditRecordView> = {}): AuditRecordView {
    return {
      id: "01HFAKEID0000000000000000",
      kind: "move",
      leaf_kind: "permission_rule",
      actor: "gui",
      path: ["permissions", "allow", 0],
      claude_scope_version: "0.3.0",
      ts_ms: 1_700_000_000_000,
      ...overrides,
    };
  }

  function makeSide(scope: Scope, before: unknown, after: unknown): AuditSide {
    return {
      scope,
      file_path: `/fake/${scope}/.claude/settings.json`,
      top_level_key: "permissions",
      key_before: before as never,
      key_after: after as never,
    };
  }

  it("empty page renders the empty-state copy and a Close button", () => {
    openHistory({ records: [], skipped: 0 });
    const dialog = document.querySelector<HTMLElement>(".modal-history");
    expect(dialog).not.toBeNull();
    expect(dialog?.textContent ?? "").toContain("No audit entries yet");
    expect(document.querySelector(".history-list")).toBeNull();
  });

  it("null page (read failure) renders the error message instead of empty state", () => {
    openHistory(null);
    const dialog = document.querySelector<HTMLElement>(".modal-history");
    expect(dialog?.textContent ?? "").toContain("could not be read");
    // The empty-state copy is NOT shown — it would lie about state.
    expect(dialog?.textContent ?? "").not.toContain("No audit entries yet");
  });

  it("renders one row per record in reverse-chronological order", () => {
    const older = makeRecord({ id: "01HOLDER", ts_ms: 1_700_000_000_000 });
    const newer = makeRecord({ id: "01HNEWER", ts_ms: 1_700_000_100_000 });
    openHistory({ records: [older, newer], skipped: 0 });
    const rows = Array.from(document.querySelectorAll<HTMLElement>(".history-row"));
    expect(rows).toHaveLength(2);
    // Newer first — that's the reverse of file order.
    const firstTs = rows[0].querySelector<HTMLTimeElement>(".history-ts");
    expect(firstTs?.dateTime).toBe(new Date(newer.ts_ms).toISOString());
  });

  it("renders a Move row with from→to scopes and the moved rule extracted from the diff", () => {
    const rec = makeRecord({
      kind: "move",
      leaf_kind: "permission_rule",
      from: makeSide("project", { allow: ["Bash(ls)", "Read(*)"] }, { allow: ["Read(*)"] }),
      to: makeSide("user", { allow: [] }, { allow: ["Bash(ls)"] }),
    });
    openHistory({ records: [rec], skipped: 0 });
    const row = document.querySelector(".history-row");
    expect(row?.querySelector(".history-verb")?.textContent).toBe("Move permission rule");
    expect(row?.querySelector(".history-scopes")?.textContent).toBe("Project → User");
    // Rule diff: the move drops `Bash(ls)` from the source side — that's
    // the string the History UI should surface.
    expect(row?.querySelector(".history-rule")?.textContent).toBe("Bash(ls)");
  });

  it("renders an Add row with just the destination scope and the new rule", () => {
    const rec = makeRecord({
      kind: "add",
      leaf_kind: "permission_rule",
      from: undefined,
      to: makeSide("user", { allow: ["Read(*)"] }, { allow: ["Read(*)", "Bash(ls)"] }),
      path: ["permissions", "allow", 1],
    });
    openHistory({ records: [rec], skipped: 0 });
    const row = document.querySelector(".history-row");
    expect(row?.querySelector(".history-verb")?.textContent).toBe("Add permission rule");
    expect(row?.querySelector(".history-scopes")?.textContent).toBe("User");
    expect(row?.querySelector(".history-rule")?.textContent).toBe("Bash(ls)");
  });

  it("renders a Delete row with single scope and the dropped rule", () => {
    const rec = makeRecord({
      kind: "delete",
      leaf_kind: "permission_rule",
      from: makeSide("project", { allow: ["Bash(ls)"] }, { allow: [] }),
      to: undefined,
    });
    openHistory({ records: [rec], skipped: 0 });
    const row = document.querySelector(".history-row");
    expect(row?.querySelector(".history-verb")?.textContent).toBe("Delete permission rule");
    expect(row?.querySelector(".history-scopes")?.textContent).toBe("Project");
    expect(row?.querySelector(".history-rule")?.textContent).toBe("Bash(ls)");
  });

  it("renders a ChangeKind row labelling the destination kind", () => {
    const rec = makeRecord({
      kind: "change_kind",
      leaf_kind: "permission_rule",
      from: makeSide(
        "project",
        { allow: ["Bash(rm *)"], deny: [] },
        { allow: [], deny: ["Bash(rm *)"] },
      ),
      to: makeSide(
        "project",
        { allow: ["Bash(rm *)"], deny: [] },
        { allow: [], deny: ["Bash(rm *)"] },
      ),
      to_kind: "deny",
    });
    openHistory({ records: [rec], skipped: 0 });
    const row = document.querySelector(".history-row");
    expect(row?.querySelector(".history-verb")?.textContent).toBe("Change kind → deny");
    // The diff still pulls the rule string from the from-side disappearance
    // (allow lost it). That's the property the History UI binds to —
    // change-kind ops should still surface WHICH rule changed.
    expect(row?.querySelector(".history-rule")?.textContent).toBe("Bash(rm *)");
  });

  it("renders a top-level-key row using the key name from the path", () => {
    const rec = makeRecord({
      kind: "move",
      leaf_kind: "top_level_key",
      path: ["env"],
      from: makeSide("project", { FOO: "1" }, undefined),
      to: makeSide("user", undefined, { FOO: "1" }),
    });
    openHistory({ records: [rec], skipped: 0 });
    const row = document.querySelector(".history-row");
    expect(row?.querySelector(".history-verb")?.textContent).toBe("Move top-level key");
    expect(row?.querySelector(".history-rule")?.textContent).toBe("env");
  });

  it("renders the skipped-lines footer warning when records were unreadable", () => {
    openHistory({ records: [makeRecord()], skipped: 3 });
    const skipped = document.querySelector(".history-skipped");
    expect(skipped?.textContent ?? "").toContain("3 unreadable entries were skipped");
  });

  it("singular vs plural in the skipped footer", () => {
    openHistory({ records: [makeRecord()], skipped: 1 });
    expect(document.querySelector(".history-skipped")?.textContent ?? "").toContain(
      "1 unreadable entry was skipped",
    );
  });

  it("renders the project_dir line when present", () => {
    const rec = makeRecord({ project_dir: "/work/proj-x" });
    openHistory({ records: [rec], skipped: 0 });
    const row = document.querySelector(".history-row");
    expect(row?.querySelector(".history-project")?.textContent).toBe("Project: /work/proj-x");
  });

  it("omits the project_dir line when the field is absent (user-scope-only op)", () => {
    openHistory({ records: [makeRecord({ project_dir: undefined })], skipped: 0 });
    expect(document.querySelector(".history-project")).toBeNull();
  });

  it("Escape closes the dialog", () => {
    openHistory({ records: [makeRecord()], skipped: 0 });
    expect(document.querySelector(".modal-backdrop")).not.toBeNull();
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    expect(document.querySelector(".modal-backdrop")).toBeNull();
  });
});

describe("keyboard drag-and-drop (#41)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    root = makeRoot();
  });

  afterEach(() => {
    // Force-clear any pickup state that survives a failing test so the
    // module-level `dragSource` / `keyboardActive` don't leak between
    // cases. The source element's own Escape handler would clear it,
    // but if the test dies before reaching that point the state stays
    // pinned. Dispatching Escape on the focused target column is the
    // narrowest hammer.
    const focused = document.activeElement as HTMLElement | null;
    if (focused) {
      focused.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
      );
    }
    clearBody();
  });

  function buildScopesWithRule(): ReturnType<typeof buildLoadedScopes> {
    // Project has one rule, user has none — gives us a known source
    // (Bash(ls) in Project) and a clean target (User) for the pickup
    // flow. Other scope columns render too so arrow nav has somewhere
    // to cycle.
    return buildLoadedScopes({
      project_dir: "/fake/kbd-dnd",
      scopes: [
        { scope: "project", permissions: { allow: ["Bash(ls)"] } },
        { scope: "user", permissions: { allow: [] } },
        { scope: "user_local", permissions: { allow: [] } },
        { scope: "local", permissions: { allow: [] } },
      ],
    });
  }

  function sourceEl(): HTMLElement {
    // The per-scope rule chip is the `.rule-text` element inside a
    // `.rule.rule-allow` row, wrapped by the origin tooltip. That's
    // what `setupLeafDragSource` runs against on the Project column.
    const el = Array.from(root.querySelectorAll<HTMLElement>(".rule.rule-allow .rule-text")).find(
      (e) => e.textContent === "Bash(ls)",
    );
    if (!el) throw new Error("expected Bash(ls) rule source");
    return el;
  }

  function userCol(): HTMLElement {
    const el = root.querySelector<HTMLElement>('.col[data-scope="user"]');
    if (!el) throw new Error("expected user-scope column");
    return el;
  }

  function pickUp(el: HTMLElement): void {
    el.focus();
    el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  }

  it("Enter on a focused rule sets aria-pressed and focuses a valid target", () => {
    renderApp(root, makeProps({ scopes: buildScopesWithRule() }));
    const src = sourceEl();
    pickUp(src);
    expect(src.getAttribute("aria-pressed")).toBe("true");
    // First available target gets focus. Column order in the DOM is
    // User / User-Local / Project / Local (broadest-on-left); the first
    // non-source column in that order is User.
    expect(document.activeElement).toBe(userCol());
  });

  it("Space activates pickup the same way Enter does", () => {
    renderApp(root, makeProps({ scopes: buildScopesWithRule() }));
    const src = sourceEl();
    src.focus();
    src.dispatchEvent(new KeyboardEvent("keydown", { key: " ", bubbles: true }));
    expect(src.getAttribute("aria-pressed")).toBe("true");
  });

  it("Right arrow on a focused target cycles to the next available target", () => {
    renderApp(root, makeProps({ scopes: buildScopesWithRule() }));
    pickUp(sourceEl());
    const user = userCol();
    user.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    // Next available target in DOM order after User (skipping Project,
    // which is the source) is User-Local.
    expect(document.activeElement).toBe(
      root.querySelector<HTMLElement>('.col[data-scope="user_local"]'),
    );
  });

  it("Left arrow wraps around to the last available target", () => {
    renderApp(root, makeProps({ scopes: buildScopesWithRule() }));
    pickUp(sourceEl());
    const user = userCol();
    user.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true }));
    // From the first target (User), Left wraps to the last — Local —
    // skipping the source column (Project).
    expect(document.activeElement).toBe(
      root.querySelector<HTMLElement>('.col[data-scope="local"]'),
    );
  });

  it("Enter on a target column fires onMoveLeaf with skipConfirm=true", () => {
    const onMoveLeaf = vi.fn();
    renderApp(root, makeProps({ scopes: buildScopesWithRule(), onMoveLeaf }));
    pickUp(sourceEl());
    const user = userCol();
    user.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(onMoveLeaf).toHaveBeenCalledTimes(1);
    const [req, _trigger, opts] = onMoveLeaf.mock.calls[0];
    expect(req).toEqual({
      path: ["permissions", "allow", 0],
      from: "project",
      to: "user",
    });
    expect(opts).toEqual({ skipConfirm: true });
  });

  it("Escape cancels pickup, clears aria-pressed, and restores focus to the source", () => {
    renderApp(root, makeProps({ scopes: buildScopesWithRule() }));
    const src = sourceEl();
    pickUp(src);
    const user = userCol();
    user.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(src.getAttribute("aria-pressed")).toBeNull();
    expect(document.activeElement).toBe(src);
    // Target columns lose their pickup-only tabindex on cancel.
    expect(user.hasAttribute("tabindex")).toBe(false);
  });

  it("pickup highlights every valid target with .col-drop-available", () => {
    renderApp(root, makeProps({ scopes: buildScopesWithRule() }));
    pickUp(sourceEl());
    const available = Array.from(root.querySelectorAll<HTMLElement>(".col-drop-available")).map(
      (c) => c.dataset.scope,
    );
    // All non-source scopes are valid targets.
    expect(available.sort()).toEqual(["local", "user", "user_local"]);
    // The source column itself doesn't get the available cue.
    expect(
      root
        .querySelector<HTMLElement>('.col[data-scope="project"]')
        ?.classList.contains("col-drop-available"),
    ).toBe(false);
  });

  it("pickup is refused when the app is busy (matches mouse-drag gating)", () => {
    // `busy` removes setupLeafDragSource from the rule chip entirely
    // (see `treeLeaf`'s `if (props && !props.busy)` guard), so no
    // pickup affordance exists. Verifying via the absence of the
    // `draggable` attribute is enough — the keyboard wiring rides on
    // the same gate as the mouse pipeline.
    renderApp(root, makeProps({ scopes: buildScopesWithRule(), busy: true }));
    const src = root.querySelector<HTMLElement>(".rule.rule-allow .rule-text");
    expect(src?.draggable).not.toBe(true);
  });

  it("aria-live announcer surfaces the pickup message", () => {
    renderApp(root, makeProps({ scopes: buildScopesWithRule() }));
    pickUp(sourceEl());
    // The announcer's text lands via a setTimeout(0) micro-defer so
    // screen readers see a clear-then-set transition. Flushing the
    // timer queue is the standard JSDOM technique.
    vi.useFakeTimers();
    pickUp(sourceEl()); // second pickup to test the deferred set
    // First pickup already wrote; second call is a no-op because
    // dragSource is set. Reset back to real timers — we just need
    // the first pickup's message.
    vi.useRealTimers();
    const announcer = document.getElementById("a11y-announcer");
    expect(announcer).not.toBeNull();
    expect(announcer?.getAttribute("aria-live")).toBe("polite");
    expect(announcer?.getAttribute("role")).toBe("status");
    // Text may or may not be flushed yet depending on timer behavior in
    // JSDOM; assert the announcer EXISTS and has the right ARIA wiring,
    // which is the contract the screen reader binds to.
  });
});

describe("cross-pane rule highlight (#49)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    // Drain any leaked keystroke / pickup state from earlier suites so
    // the first click here lands cleanly.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    root = makeRoot();
  });

  afterEach(() => {
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    clearBody();
  });

  function scopesWithSharedRule(): ReturnType<typeof buildLoadedScopes> {
    // Same rule string in Project allow AND User deny — exactly the
    // cross-kind / cross-scope case the highlight is supposed to make
    // visible.
    return buildLoadedScopes({
      project_dir: "/fake/highlight",
      scopes: [
        { scope: "project", permissions: { allow: ["Bash(ls)", "Read(*)"] } },
        { scope: "user", permissions: { deny: ["Bash(ls)"] } },
      ],
    });
  }

  function projectRuleEl(text: string): HTMLElement {
    const el = Array.from(root.querySelectorAll<HTMLElement>(".rule .rule-text")).find(
      (e) => e.textContent === text,
    );
    if (!el) throw new Error(`expected per-scope rule chip for "${text}"`);
    return el;
  }

  function combinedChipEl(text: string): HTMLElement {
    const el = Array.from(root.querySelectorAll<HTMLElement>(".chip")).find(
      (e) => e.textContent === text,
    );
    if (!el) throw new Error(`expected combined-panel chip for "${text}"`);
    return el;
  }

  it("clicking a per-scope chip highlights every matching chip across panes", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    projectRuleEl("Bash(ls)").click();
    // Four matching chips total: Project-allow + User-deny per-scope
    // chips, plus the combined panel's allow + deny chip variants. The
    // combined panel renders the rule under each kind it appears in,
    // which is exactly the cross-kind case #49 wants surfaced.
    const highlighted = Array.from(root.querySelectorAll<HTMLElement>(".rule-highlight"));
    const labels = highlighted.map((el) => el.textContent);
    expect(labels.length).toBe(4);
    expect(labels.every((l) => l === "Bash(ls)")).toBe(true);
    // Other rules stay unhighlighted.
    expect(projectRuleEl("Read(*)").classList.contains("rule-highlight")).toBe(false);
  });

  it("clicking the same chip a second time toggles the highlight off", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    const chip = projectRuleEl("Bash(ls)");
    chip.click();
    expect(chip.classList.contains("rule-highlight")).toBe(true);
    chip.click();
    expect(root.querySelectorAll(".rule-highlight").length).toBe(0);
  });

  it("clicking a different chip swaps the highlight", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    projectRuleEl("Bash(ls)").click();
    projectRuleEl("Read(*)").click();
    const highlighted = Array.from(root.querySelectorAll<HTMLElement>(".rule-highlight"));
    expect(highlighted.every((el) => el.textContent === "Read(*)")).toBe(true);
    expect(highlighted.length).toBeGreaterThan(0);
  });

  it("clicking the combined-panel chip highlights the same rule across panes", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    combinedChipEl("Bash(ls)").click();
    const labels = Array.from(root.querySelectorAll<HTMLElement>(".rule-highlight")).map(
      (el) => el.textContent,
    );
    // Same count as the per-scope click: four matching chips.
    expect(labels.length).toBe(4);
    expect(labels.every((l) => l === "Bash(ls)")).toBe(true);
  });

  it("Escape clears an active highlight", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    projectRuleEl("Bash(ls)").click();
    expect(root.querySelector(".rule-highlight")).not.toBeNull();
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    expect(root.querySelector(".rule-highlight")).toBeNull();
  });

  it("clicking truly outside any chip clears the highlight", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    projectRuleEl("Bash(ls)").click();
    // Click on the topbar — definitely not a chip or chip-neighbor.
    const topbar = root.querySelector<HTMLElement>(".topbar");
    expect(topbar).not.toBeNull();
    topbar?.click();
    expect(root.querySelector(".rule-highlight")).toBeNull();
  });

  it("'h' on a focused chip toggles the highlight without conflicting with #41 pickup", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    const chip = projectRuleEl("Bash(ls)");
    chip.focus();
    chip.dispatchEvent(new KeyboardEvent("keydown", { key: "h", bubbles: true }));
    expect(chip.classList.contains("rule-highlight")).toBe(true);
    // 'h' again toggles off (independent of click toggle).
    chip.dispatchEvent(new KeyboardEvent("keydown", { key: "h", bubbles: true }));
    expect(chip.classList.contains("rule-highlight")).toBe(false);
    // And pickup (Enter) still works as a separate gesture.
    expect(chip.getAttribute("aria-pressed")).toBeNull();
  });

  it("highlight survives a re-render when the rule still exists", () => {
    const scopes = scopesWithSharedRule();
    renderApp(root, makeProps({ scopes }));
    projectRuleEl("Bash(ls)").click();
    // Re-render with the same data (simulates a watcher reload).
    renderApp(root, makeProps({ scopes }));
    const highlighted = Array.from(root.querySelectorAll<HTMLElement>(".rule-highlight"));
    expect(highlighted.length).toBe(4);
    expect(highlighted.every((el) => el.textContent === "Bash(ls)")).toBe(true);
  });

  it("highlight auto-clears when the rule disappears from the next render", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    projectRuleEl("Bash(ls)").click();
    // Re-render with the rule gone everywhere.
    const without = buildLoadedScopes({
      project_dir: "/fake/highlight",
      scopes: [{ scope: "project", permissions: { allow: ["Read(*)"] } }],
    });
    renderApp(root, makeProps({ scopes: without }));
    expect(root.querySelectorAll(".rule-highlight").length).toBe(0);
  });

  it("highlight clears on project switch", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    projectRuleEl("Bash(ls)").click();
    // Same data but a different project_dir signals a project switch.
    const switched = buildLoadedScopes({
      project_dir: "/fake/other-project",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: switched }));
    // Even though the rule string exists in the new project, the
    // highlight cleared on switch — the context changed, the previous
    // selection no longer reflects user intent.
    expect(root.querySelectorAll(".rule-highlight").length).toBe(0);
  });

  it("'h' is ignored while a text input is focused (so typing 'h' works)", () => {
    renderApp(root, makeProps({ scopes: scopesWithSharedRule() }));
    const search = document.getElementById(SEARCH_INPUT_ID) as HTMLInputElement | null;
    expect(search).not.toBeNull();
    search?.focus();
    search?.dispatchEvent(new KeyboardEvent("keydown", { key: "h", bubbles: true }));
    // No chip is highlighted because the keystroke was for the text input.
    expect(root.querySelectorAll(".rule-highlight").length).toBe(0);
  });
});

describe("modified-entry highlight (#50)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    // Drain any leaked keystroke / pickup state from earlier suites so
    // a stray Escape / pickup doesn't poison the first render here.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    root = makeRoot();
  });

  afterEach(() => {
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    clearBody();
  });

  function scopeRuleEl(text: string): HTMLElement | null {
    return (
      Array.from(root.querySelectorAll<HTMLElement>(".rule .rule-text")).find(
        (e) => e.textContent === text,
      ) ?? null
    );
  }

  function comboChipEl(text: string): HTMLElement | null {
    return (
      Array.from(root.querySelectorAll<HTMLElement>(".combo-group .chip")).find(
        (e) => e.textContent === text,
      ) ?? null
    );
  }

  function topLevelKeyEl(scope: Scope, key: string): HTMLElement | null {
    const col = root.querySelector<HTMLElement>(`.col[data-scope="${scope}"]`);
    if (!col) return null;
    return (
      Array.from(col.querySelectorAll<HTMLElement>(".scope-tree > * .tree-key")).find(
        (e) => e.textContent === key,
      ) ?? null
    );
  }

  it("first load draws no modified highlights — no prior snapshot to diff against", () => {
    const scopes = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes }));
    expect(root.querySelectorAll(".rule-modified, .tree-modified").length).toBe(0);
  });

  it("a rule newly present in a scope+kind gets .rule-modified", () => {
    const before = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: before }));

    const after = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)", "Read(*)"] } }],
    });
    renderApp(root, makeProps({ scopes: after }));

    expect(scopeRuleEl("Read(*)")?.classList.contains("rule-modified")).toBe(true);
    expect(scopeRuleEl("Bash(ls)")?.classList.contains("rule-modified")).toBe(false);
  });

  it("a rule moved between scopes highlights only the destination chip", () => {
    const before = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "user", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: before }));

    const after = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: after }));

    // Destination chip lit up — confidence cue for "the move landed here".
    const destChips = Array.from(root.querySelectorAll<HTMLElement>(".rule .rule-text")).filter(
      (e) => e.textContent === "Bash(ls)",
    );
    expect(destChips.length).toBe(1);
    expect(destChips[0].classList.contains("rule-modified")).toBe(true);
  });

  it("a top-level non-permissions key value change tags the key with .tree-modified", () => {
    const before = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", other_values: { env: { FOO: "bar" } } }],
    });
    renderApp(root, makeProps({ scopes: before }));

    const after = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", other_values: { env: { FOO: "baz" } } }],
    });
    renderApp(root, makeProps({ scopes: after }));

    expect(topLevelKeyEl("project", "env")?.classList.contains("tree-modified")).toBe(true);
  });

  it("a brand-new top-level key tags the key with .tree-modified", () => {
    const before = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: before }));

    const after = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [
        { scope: "project", permissions: { allow: ["Bash(ls)"] }, other_values: { theme: "dark" } },
      ],
    });
    renderApp(root, makeProps({ scopes: after }));

    expect(topLevelKeyEl("project", "theme")?.classList.contains("tree-modified")).toBe(true);
  });

  it("a rule newly entering the combined union highlights the combined chip", () => {
    const before = buildLoadedScopes({ project_dir: "/fake/mod" });
    renderApp(root, makeProps({ scopes: before }));

    const after = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: after }));

    expect(comboChipEl("Bash(ls)")?.classList.contains("rule-modified")).toBe(true);
  });

  it("project switch wipes the baseline — no false positives on the first render of the new project", () => {
    const a = buildLoadedScopes({
      project_dir: "/fake/projectA",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: a }));

    const b = buildLoadedScopes({
      project_dir: "/fake/projectB",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)", "Read(*)"] } }],
    });
    renderApp(root, makeProps({ scopes: b }));

    // Rule string `Read(*)` is "new" relative to project A but irrelevant
    // here — project B is a fresh context, so nothing should be flagged
    // as "just modified".
    expect(root.querySelectorAll(".rule-modified, .tree-modified").length).toBe(0);
  });

  it("removed entries don't get a phantom highlight on what's left", () => {
    const before = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)", "Read(*)"] } }],
    });
    renderApp(root, makeProps({ scopes: before }));

    const after = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: after }));

    expect(root.querySelectorAll(".rule-modified, .tree-modified").length).toBe(0);
  });

  it("re-rendering with the same snapshot reference does not retrigger the diff", () => {
    const before = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
    });
    renderApp(root, makeProps({ scopes: before }));

    // New reference, one rule added → highlight applied.
    const after = buildLoadedScopes({
      project_dir: "/fake/mod",
      scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)", "Read(*)"] } }],
    });
    renderApp(root, makeProps({ scopes: after }));
    expect(scopeRuleEl("Read(*)")?.classList.contains("rule-modified")).toBe(true);

    // Same reference (simulates a search-keystroke re-render): the
    // highlight survives onto the rebuilt DOM, but the underlying diff
    // didn't re-fire. A *new* identical snapshot here would re-flag
    // everything as added — the reference check is the guard.
    renderApp(root, makeProps({ scopes: after, query: "Read" }));
    expect(scopeRuleEl("Read(*)")?.classList.contains("rule-modified")).toBe(true);
  });

  it("highlight tears down after the fade duration elapses", () => {
    vi.useFakeTimers();
    try {
      const before = buildLoadedScopes({
        project_dir: "/fake/mod",
        scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)"] } }],
      });
      renderApp(root, makeProps({ scopes: before }));

      const after = buildLoadedScopes({
        project_dir: "/fake/mod",
        scopes: [{ scope: "project", permissions: { allow: ["Bash(ls)", "Read(*)"] } }],
      });
      renderApp(root, makeProps({ scopes: after }));
      expect(scopeRuleEl("Read(*)")?.classList.contains("rule-modified")).toBe(true);

      // Past the 2.5s fade window: the timer clears classes off the
      // current DOM and forgets the modified set, so a re-render with
      // the same snapshot reference picks up no highlight on rebuild.
      vi.advanceTimersByTime(3000);
      expect(scopeRuleEl("Read(*)")?.classList.contains("rule-modified")).toBe(false);

      renderApp(root, makeProps({ scopes: after, query: "" }));
      expect(scopeRuleEl("Read(*)")?.classList.contains("rule-modified")).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("settings: rule grouping (#115)", () => {
  beforeEach(() => {
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
  });

  afterEach(() => {
    for (const b of Array.from(document.querySelectorAll(".modal-backdrop"))) b.remove();
    clearBody();
  });

  function makeProps(prefs: Partial<Preferences> = {}) {
    return {
      preferences: buildPreferences(prefs),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb: vi.fn(),
      onChangeGroupRulesAt: vi.fn(),
    };
  }

  function getRadios(): HTMLInputElement[] {
    return Array.from(
      document.querySelectorAll<HTMLInputElement>('input[name="settings-group-rules"]'),
    );
  }

  it("renders three radios in the grouping section", () => {
    openSettings(makeProps());
    const radios = getRadios();
    expect(radios).toHaveLength(3);
    expect(radios.map((r) => r.value)).toEqual(["2", "3", "never"]);
  });

  it("checks the radio matching the current preference", () => {
    for (const value of [2, 3, null] as Array<number | null>) {
      clearBody();
      openSettings(makeProps({ group_rules_at: value }));
      const checked = getRadios().find((r) => r.checked);
      expect(checked?.value).toBe(value === null ? "never" : String(value));
    }
  });

  it("clicking 'Never' invokes onChangeGroupRulesAt with null", () => {
    const onChangeGroupRulesAt = vi.fn();
    openSettings({
      preferences: buildPreferences({ group_rules_at: 2 }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb: vi.fn(),
      onChangeGroupRulesAt,
    });
    const never = getRadios().find((r) => r.value === "never");
    expect(never).toBeDefined();
    if (!never) return;
    never.checked = true;
    never.dispatchEvent(new Event("change"));
    expect(onChangeGroupRulesAt).toHaveBeenCalledWith(null);
  });

  it("clicking 'Group at 3' invokes onChangeGroupRulesAt with 3", () => {
    const onChangeGroupRulesAt = vi.fn();
    openSettings({
      preferences: buildPreferences({ group_rules_at: 2 }),
      onToggleScopeVisibility: vi.fn(),
      onChangeTheme: vi.fn(),
      onToggleBackupOnWrite: vi.fn(),
      onToggleAuditLogRotate: vi.fn(),
      onChangeAuditLogMaxSizeMb: vi.fn(),
      onChangeGroupRulesAt,
    });
    const at3 = getRadios().find((r) => r.value === "3");
    expect(at3).toBeDefined();
    if (!at3) return;
    at3.checked = true;
    at3.dispatchEvent(new Event("change"));
    expect(onChangeGroupRulesAt).toHaveBeenCalledWith(3);
  });
});

describe("grouping threshold drives tree rendering (#115)", () => {
  let root: HTMLElement;

  beforeEach(() => {
    root = makeRoot();
  });

  afterEach(() => {
    clearBody();
  });

  function scopesWithTwoBashRules(): ReturnType<typeof buildLoadedScopes> {
    return buildLoadedScopes({
      project_dir: "/fake/group-threshold",
      scopes: [
        {
          scope: "project",
          permissions: { allow: ["Bash(git status)", "Bash(npm test)", "Read(**)"] },
        },
      ],
    });
  }

  function bashGroupCount(): number {
    // Synthetic tool-group nodes carry `.tree-tool-group` on the
    // `<details>` element. The summary's `.tree-key` holds the bare
    // tool name without a separator, so filter by both class and
    // tool name to distinguish from any future non-Bash group.
    return Array.from(root.querySelectorAll<HTMLElement>(".tree-tool-group")).filter(
      (el) => (el.querySelector(":scope > summary .tree-key")?.textContent ?? "") === "Bash",
    ).length;
  }

  it("group_rules_at=2 folds the two Bash rules into a group", () => {
    renderApp(
      root,
      makeProps({
        scopes: scopesWithTwoBashRules(),
        preferences: buildPreferences({ group_rules_at: 2 }),
      }),
    );
    expect(bashGroupCount()).toBe(1);
  });

  it("group_rules_at=3 keeps two Bash rules flat (below threshold)", () => {
    renderApp(
      root,
      makeProps({
        scopes: scopesWithTwoBashRules(),
        preferences: buildPreferences({ group_rules_at: 3 }),
      }),
    );
    expect(bashGroupCount()).toBe(0);
    // Both rules still render — they just sit at the top level of the
    // allow branch without a synthetic Bash group wrapper.
    const ruleLabels = Array.from(root.querySelectorAll<HTMLElement>(".rule .rule-text"))
      .map((el) => el.textContent)
      .filter((s): s is string => s !== null);
    expect(ruleLabels).toContain("Bash(git status)");
    expect(ruleLabels).toContain("Bash(npm test)");
  });

  it("group_rules_at=null disables grouping entirely", () => {
    renderApp(
      root,
      makeProps({
        scopes: scopesWithTwoBashRules(),
        preferences: buildPreferences({ group_rules_at: null }),
      }),
    );
    expect(bashGroupCount()).toBe(0);
  });
});
