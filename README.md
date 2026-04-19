# ClaudeScope

[![CI](https://github.com/bminier/claude-scope/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/bminier/claude-scope/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-informational.svg)](./LICENSE)

Desktop GUI for promoting [Claude Code](https://docs.claude.com/en/docs/claude-code) permission rules between scopes — without hand-editing JSON.

> For the original problem statement, design goals, and non-goals, see [`CLAUDE.md`](./CLAUDE.md). It's the project brief; the README is the "how to use and hack on it" doc.

## What it does

Claude Code reads settings from JSON files at several scopes. Moving a permission rule from Project to User (or anywhere else) today means editing both JSON files by hand, which is tedious and drops formatting. ClaudeScope shows every rule across the three recognized scopes (Local / Project / User) side-by-side and moves rules between them with a click.

### Features

- **Three-column scope view** — Local / Project / User, with the "effective" merged view on top
- **Atomic, safe writes** — serialize → JSON-revalidate → tempfile + rename, with one-shot `.bak` backup per file per session
- **Diff preview modal** — shows before/after for both sides of a move with a proper diff, Esc/Enter/Tab-trap keyboard handling, focus restored to the triggering button on close
- **Rule search / filter** — press `/` anywhere to focus, case-insensitive substring, `m/n` match counts per group
- **Auto-reload** — `notify`-based file watcher picks up external edits (hand-edited JSON, another editor, etc.) and refreshes the UI without losing state
- **Shape-level lint** — subtle ⚠ badge on rules that don't match a recognized shape (`Bash(...)`, `WebFetch(domain:...)`, `mcp__server__tool`, etc.), with tooltips explaining why

### Safety model

- Writes are atomic: serialize → revalidate JSON → tempfile in the same directory → `rename` over target.
- On the first write of a given file per session, the original is copied to `<file>.bak`. Existing `.bak` files from previous runs are preserved, not clobbered.
- A move writes the destination first, then removes from the source. If the source write fails, the destination write is rolled back so the rule ends up in exactly one scope rather than being lost *or* duplicated. If the rollback itself fails the user sees an explicit error.
- `~/.claude/settings.local.json` is **not** treated as a scope.

## Stack

- **[Tauri 2](https://tauri.app)** — Rust backend + webview
- **Vanilla TypeScript + Vite** — front end (intentionally tiny, no framework)
- **[`serde_json`](https://docs.rs/serde_json)** with `preserve_order` — key order survives round-trips
- **[`notify-debouncer-mini`](https://docs.rs/notify-debouncer-mini)** — cross-platform file watching with built-in debouncing
- **[Biome 2](https://biomejs.dev)** — formatter + linter + import sorter for the front end

## Layout

```
claude-scope/
├── biome.json                  Biome config (formatter + linter)
├── index.html                  Vite entry
├── src/                        Front-end (TypeScript)
│   ├── main.ts                 State + Tauri command wiring + keybindings
│   ├── ui.ts                   DOM rendering, diff modal, search UI
│   ├── lint.ts                 Shape-level rule-string lint
│   ├── types.ts                Shared types with Rust
│   └── styles.css
├── src-tauri/                  Rust backend
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/
│   └── src/
│       ├── main.rs             Tauri entry
│       ├── lib.rs              Builder + managed state + command registration
│       ├── scope.rs            Scope discovery (walk-up, resolve paths)
│       ├── io_atomic.rs        Atomic read/write + .bak tracker
│       ├── model.rs            SettingsDoc, permission add/remove, render
│       ├── commands.rs         load_scopes / diff_move / apply_move
│       └── watcher.rs          notify-based file watcher
└── .github/workflows/ci.yml    build matrix (Linux/Windows/macOS) + MSRV + lint
```

## Develop

### Prerequisites

- **Rust** — stable, 1.88+ (imposed by Tauri 2.10's transitive deps — `darling`, `serde_with`, `time`)
- **Node.js** 20+ and **npm**
- **Linux build deps** (only on Linux):
  ```sh
  sudo apt-get install libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
  ```
- **pre-commit** (optional, strongly recommended — CI runs the same hooks):
  ```sh
  pip install --user pre-commit
  pre-commit install
  ```

### Run

```sh
npm install
npm run tauri dev
```

### Tests

```sh
# Rust unit tests (scope discovery, atomic writes, move logic, watcher plan)
(cd src-tauri && cargo test)

# Front-end type check
npx tsc --noEmit
```

### Lint / format

```sh
# Check everything (TS/JS/JSON/CSS via Biome; Rust via fmt + clippy).
# This is what the `lint` CI job runs.
pre-commit run --all-files

# Or just the TS side via npm scripts:
npm run lint     # biome check (lint + format check)
npm run format   # biome format --write (applies formatting)

# Rust side by hand:
(cd src-tauri && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings)
```

### Build

```sh
npm run tauri build
```

### Keyboard shortcuts

| Key | Action |
| --- | --- |
| `/` | Focus the rule search input |
| `Esc` (in search) | Clear the filter |
| `Esc` (in diff modal) | Cancel the move |
| `Enter` (in diff modal) | Activate the focused button (Apply is the default focus) |
| `Tab` / `Shift+Tab` (in diff modal) | Cycle focus within the modal |

## CI

Every push to `dev` and every PR against `dev` runs three jobs in parallel, producing five check runs total:

- `build` — a matrix job across `ubuntu-24.04`, `windows-latest`, `macos-latest`. Each runs `npm ci`, `tsc --noEmit`, `vite build`, `cargo test --lib --locked`.
- `msrv (1.88)` — validates the declared MSRV via `cargo check --lib --tests --locked` on Rust 1.88.0
- `lint` — runs `pre-commit/action@v3` (Biome, rustfmt, clippy, hygiene hooks)

## Automated dependency updates

[Dependabot](./.github/dependabot.yml) opens weekly grouped PRs for Cargo, npm, and GitHub Actions minor/patch bumps. Major bumps land as separate PRs.

## Releasing

Pushing a tag of the form `v*.*.*` triggers [`.github/workflows/release.yml`](./.github/workflows/release.yml), which builds installers for all three platforms and uploads them to a GitHub Release as a draft. The maintainer reviews + publishes from the GitHub UI.

To cut a release:

1. **Bump the version in three places** (they have to stay in sync — `tauri-action` reads `tauri.conf.json`, but `cargo` and `npm` each have their own copy):
   - `src-tauri/tauri.conf.json` → `version`
   - `src-tauri/Cargo.toml` → `version`
   - `package.json` → `version`
2. Commit and merge that bump into `dev` (and whatever downstream branch actually ships).
3. Tag and push:
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
4. Watch [Actions](https://github.com/bminier/claude-scope/actions/workflows/release.yml). When all three matrix legs finish, a draft release appears at [Releases](https://github.com/bminier/claude-scope/releases) with:
   - **Windows**: `.msi` (WiX installer) and `.exe` (NSIS)
   - **macOS**: `.dmg` and `.app.tar.gz` (universal binary — runs on Intel and Apple Silicon)
   - **Linux**: `.deb`, `.rpm`, and `.AppImage`
5. Review the artifacts, edit release notes if you want, click **Publish release**.

### Known limitation: unsigned builds

v0.1 releases are **unsigned**. That means:

- **Windows**: first launch shows a SmartScreen warning (click "More info" → "Run anyway").
- **macOS**: first launch shows a Gatekeeper warning (right-click the app → Open → Open).
- **Linux**: no warning.

Signing would require a Windows code-signing certificate and/or an Apple Developer ID + notarization. Planned, but out of scope until there's demand.

You can also kick off a dry-run release from the Actions UI via **Run workflow** — it'll produce a draft release you can delete afterwards.

## License

[MIT](./LICENSE) © Brian Minier
