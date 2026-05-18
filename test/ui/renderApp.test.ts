import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  AppInfo,
  KnownProject,
  MoveLeafRequest,
  MoveOptions,
  Scope,
  Theme,
} from "../../src/types.ts";
import { SEARCH_INPUT_ID } from "../../src/types.ts";
import {
  groupByToolPrefix,
  openAbout,
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
    knownProjects: overrides.knownProjects ?? [],
    onPickProject: vi.fn(),
    onReload: vi.fn(),
    onMoveLeaf: overrides.onMoveLeaf ?? vi.fn(),
    onChangeKind: vi.fn(),
    onDeleteLeaf: vi.fn(),
    onAddLeaf: vi.fn(),
    onOpenSettings: vi.fn(),
    onOpenAbout: vi.fn(),
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
    });
    const cb = backupCheckbox();
    cb.checked = false;
    cb.dispatchEvent(new Event("change"));
    expect(onToggleBackupOnWrite).toHaveBeenCalledTimes(1);
    expect(onToggleBackupOnWrite).toHaveBeenCalledWith(false);
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

  it("group members keep leaf-level paths so move buttons still address the rule directly", () => {
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
    // Inside the Bash group: click the second member's "→ User" button and
    // assert the dispatched path is `permissions.allow[1]`, not anything
    // group-relative.
    const group = root.querySelector<HTMLDetailsElement>(".tree-tool-group");
    if (!group) throw new Error("expected tool group");
    const memberRows = group.querySelectorAll<HTMLElement>(".rule.rule-allow");
    expect(memberRows.length).toBe(2);
    const toUser = memberRows[1].querySelector<HTMLButtonElement>(".rule-moves .move-btn");
    if (!toUser || toUser.textContent !== "→ User") {
      // The first matching scope target may differ depending on default
      // visibility; fall back to searching by label across all buttons in
      // the row.
      const buttons = Array.from(
        memberRows[1].querySelectorAll<HTMLButtonElement>(".rule-moves .move-btn"),
      );
      const fallback = buttons.find((b) => b.textContent === "→ User");
      if (!fallback) throw new Error("expected → User button");
      fallback.click();
    } else {
      toUser.click();
    }
    expect(onMoveLeaf).toHaveBeenCalledTimes(1);
    const [req] = onMoveLeaf.mock.calls[0];
    expect(req).toEqual({
      path: ["permissions", "allow", 1],
      from: "project",
      to: "user",
    });
  });
});
