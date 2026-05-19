# Architecture overview

ClaudeScope is a [Tauri 2](https://tauri.app) app — Rust backend, web
front end, single binary output. The split:

```
┌─────────────────────────────────────────────────────┐
│              Vanilla TS + Vite (front end)          │
│  src/main.ts       state + IPC + keybindings        │
│  src/ui.ts         DOM rendering, diff modal, …     │
│  src/lint.ts       shape-level rule-string lint     │
│  src/types.ts      shared types mirrored from Rust  │
└────────────────────┬────────────────────────────────┘
                     │  Tauri IPC (#[tauri::command])
┌────────────────────┴────────────────────────────────┐
│                     Rust backend                     │
│  src-tauri/src/                                      │
│    main.rs       Tauri entry                         │
│    lib.rs        Builder + managed state + IPC reg.  │
│    scope.rs      Scope discovery (walk-up, paths)    │
│    projects.rs   ~/.claude/projects/ enumeration     │
│    io_atomic.rs  Atomic read/write + .bak tracker    │
│    model.rs      SettingsDoc, perm add/remove        │
│    commands.rs   #[tauri::command] handlers          │
│    watcher.rs    notify-based file watcher           │
│    audit.rs      JSONL audit log + rotation          │
│    preferences.rs Per-user config (theme, LRU, …)    │
│    app_info.rs   Build/runtime diagnostics           │
│    runtime.rs    --home / --project override layer   │
└──────────────────────────────────────────────────────┘
```

A second binary `claude-scope-cli` (`src-tauri/src/bin/cli.rs`) re-uses
the same library — same scope discovery, same atomic-write machinery,
same audit log. Anything the GUI can do via IPC, the CLI eventually
exposes as a subcommand. See [CLI](../reference/cli.md) for the
current surface.

## Boundaries

- **Front end never writes files directly.** Every file-touching
  operation goes through a Tauri command. That keeps the
  atomic-write invariants concentrated on the Rust side where they
  can't be reordered by a re-render.
- **Wire types live in one place per direction.** Rust structs in
  `commands.rs` serialize to JSON; `src/types.ts` mirrors the same
  shapes by hand. A schema generator would be over-engineering for
  the surface area today.
- **The audit log is the source of truth for history; the filesystem
  is the source of truth for state.** They can diverge — if a user
  hand-edits a settings file between sessions, the log is stale, and
  restore (when it lands) re-reads the current file before computing
  any delta.

## Scope discovery

`scope::resolve_with_home()` walks up from a starting directory
looking for a `.git` entry (preferred), or a `.claude/` directory, to
identify the project root. From there it derives:

- `project_dir/.claude/settings.local.json` → **Local**
- `project_dir/.claude/settings.json` → **Project**
- `<home>/.claude/settings.local.json` → **User-Local**
- `<home>/.claude/settings.json` → **User**

The `<home>` argument defaults to `dirs::home_dir()` but is
overridable for sandbox runs (`--home`, `CLAUDE_SCOPE_HOME`). See
[Moving rules → Sandbox mode](../user-guide/moving-rules.md#sandbox-mode).

## Stack

- **[Tauri 2](https://tauri.app)** — Rust backend + webview.
- **Vanilla TypeScript + Vite** — front end (intentionally tiny, no
  framework).
- **[`serde_json`](https://docs.rs/serde_json)** with `preserve_order`
   — key order survives round-trips.
- **[`notify-debouncer-mini`](https://docs.rs/notify-debouncer-mini)**
   — cross-platform file watching with built-in debouncing.
- **[`ulid`](https://docs.rs/ulid)** — chronologically-sortable IDs
  for audit-log records.
- **[Biome 2](https://biomejs.dev)** — formatter + linter + import
  sorter for the front end.
