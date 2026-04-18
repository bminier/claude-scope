# ClaudeScope

Desktop GUI for promoting Claude Code settings between scopes.

> Status: v0.1 bootstrap — three-column read/write view with atomic JSON
> writes. Primary target is Windows; macOS and Linux should also build. See
> [`CLAUDE.md`](./CLAUDE.md) for the full problem statement and roadmap.

## What it does

Claude Code reads settings from several JSON files at different scopes. Moving
a permission rule from one scope to another today means hand-editing JSON in
each file. ClaudeScope shows every rule across the three recognized scopes
(Local / Project / User) and moves rules between them with a click, writing
atomically with a one-shot `.bak` backup.

## Stack

- **Tauri 2** (Rust backend + webview)
- **Vanilla TypeScript + Vite** (front end)
- `serde_json` with `preserve_order` (key order survives a round-trip)

## Layout

```
claude-scope/
├── index.html               Vite entry
├── src/                     Front-end (TS)
│   ├── main.ts              State + command wiring
│   ├── ui.ts                DOM rendering
│   ├── types.ts             Shared types with Rust
│   └── styles.css
└── src-tauri/
    ├── Cargo.toml
    ├── tauri.conf.json
    ├── capabilities/default.json
    └── src/
        ├── main.rs          Tauri entry
        ├── lib.rs           Builder + command registration
        ├── scope.rs         Scope discovery
        ├── io_atomic.rs     Atomic read/write + .bak tracker
        ├── model.rs         SettingsDoc, permission add/remove, render
        └── commands.rs      load_scopes / diff_move / apply_move
```

## Develop

### Prerequisites

- Rust (stable, 1.88+ — imposed by Tauri 2.10's transitive deps)
- Node.js 20+ and npm
- Linux build deps (only on Linux):
  `sudo apt-get install libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev`

### Run

```sh
npm install
npm run tauri dev
```

### Tests

```sh
# Rust unit tests (scope discovery, atomic writes, move logic)
(cd src-tauri && cargo test)

# Front-end type check
npx tsc --noEmit
```

### Build

```sh
npm run tauri build
```

## Safety model

- Writes are atomic: serialize → revalidate JSON → tempfile in the same
  directory → `rename` over target.
- On the first write of a given file per session, the original is copied to
  `<file>.bak`.
- A move writes the destination first, then removes from the source. If the
  source write fails, the destination write is rolled back so the rule ends
  up in exactly one scope rather than being lost *or* duplicated. If the
  rollback itself fails the user sees an explicit error.
- `~/.claude/settings.local.json` is **not** treated as a scope.
