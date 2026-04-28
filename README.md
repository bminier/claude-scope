# ClaudeScope

[![CI](https://github.com/bminier/claude-scope/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/bminier/claude-scope/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-informational.svg)](./LICENSE)

Desktop GUI for promoting [Claude Code](https://docs.claude.com/en/docs/claude-code) permission rules between scopes — without hand-editing JSON.

> For the original problem statement, design goals, and non-goals, see [`CLAUDE.md`](./CLAUDE.md). It's the project brief; the README is the "how to use and hack on it" doc.

## What it does

Claude Code reads settings from JSON files at several scopes. Moving a permission rule from Project to User (or anywhere else) today means editing both JSON files by hand, which is tedious and drops formatting. ClaudeScope shows every rule across the four recognized scopes (User / User-Local / Project / Local) side-by-side and moves rules between them with a click.

### Features

- **Four-column scope view** — User / User-Local / Project / Local, laid out broadest-on-the-left, with a "Combined permissions" panel above them. That panel is a union across scopes; it is not a full precedence-aware evaluation of what Claude Code resolves at runtime, and the subtitle says so.
- **Atomic writes with revalidation** — serialize → re-parse the produced JSON → tempfile + rename, with one-shot `.bak` backup per file per session. See [Write strategy](#write-strategy) for the fine print.
- **Diff preview modal** — shows before/after for both sides of a move with a proper diff, Esc/Enter/Tab-trap keyboard handling, focus restored to the triggering button on close
- **Rule search / filter** — press `/` anywhere to focus, case-insensitive substring, `m/n` match counts per group
- **Auto-reload** — `notify`-based file watcher picks up external edits (hand-edited JSON, another editor, etc.) and refreshes the UI without losing state
- **Heuristic shape lint** — subtle ⚠ badge on rules that don't match the shapes this UI knows (`Bash(...)`, `WebFetch(domain:...)`, `mcp__server__tool`, etc.). Best-effort only: Claude Code's full rule grammar isn't publicly documented, so flagged rules may still work — the popover names the specific heuristic that tripped and disclaims its scope.

### Write strategy

- Writes funnel through a single `atomic_write_json` chokepoint: serialize → re-parse the produced bytes → tempfile in the same directory → `rename` over target. Bad bytes never reach disk.
- The rename is filesystem-atomic. On Unix the parent directory is also `fsync`'d so the rename is crash-durable; on Windows the parent-dir flush is skipped (see the doc-comment on `atomic_write_json` for the rationale — `std::fs` cannot open a directory handle for flushing without a small raw-winapi wrapper, and NTFS journals rename metadata in `MoveFileEx`).
- A `FileStamp` (mtime + length) captured at load time is rechecked just before the rename. If the file changed on disk between load and save, the write is refused with a "reload and retry" error rather than overwriting the external edit. The check has a microsecond-scale TOCTOU window between the recheck and `persist`; a writer that lands inside that window will still be overwritten.
- On the first write of a given file per session, the original is copied to `<file>.bak`. Existing `.bak` files from previous runs are preserved, not clobbered. This is a one-shot backup attempt, not a best-effort write-through: if backup creation is required and the copy fails, the error is surfaced and the write is not attempted.
- A move writes the destination first, then removes from the source. If the source write fails, the destination write is rolled back so the rule ends up in exactly one scope rather than being lost *or* duplicated. If the rollback itself fails the user sees an explicit error.

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

- **Rust** — stable, 1.88+ (imposed by Tauri 2.10's transitive deps).
  Install via [rustup](https://rustup.rs):
  - **Windows:** `winget install Rustlang.Rustup` (then restart your terminal)
  - **macOS / Linux:** `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`

  > The Tauri CLI is bundled as an npm dev dependency — `cargo install tauri-cli` is **not** needed. Use `npm run tauri dev` (not `cargo tauri dev`).
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

### Testing safely (sandbox / scratch home)

Running ClaudeScope against your real `~/.claude/` while developing risks corrupting your actual Claude Code settings if a write path regresses. Two ways to dogfood without that risk (#66):

#### `--home` / `--project` flags + `CLAUDE_SCOPE_*` env vars

Override scope discovery so all "user" / "user-local" lookups root at a throwaway directory instead of your real `$HOME` / `%USERPROFILE%`. The current project root can be pinned with `--project` (otherwise the front-end picker / cwd walk-up behave normally).

For `npm run tauri dev`, **use the env vars** — the npm → tauri-cli → cargo chain mangles `--` separators and tauri-cli forwards trailing args as cargo flags (so `cargo run --home …` errors out). Env vars sidestep the whole chain. CLI flags work fine against a built binary.

```sh
# Dev build (Linux / macOS):
CLAUDE_SCOPE_HOME=/tmp/scratch CLAUDE_SCOPE_PROJECT=/tmp/scratch/project npm run tauri dev

# Dev build (Windows PowerShell):
$env:CLAUDE_SCOPE_HOME = "$env:TEMP\cs-scratch"
$env:CLAUDE_SCOPE_PROJECT = "$env:TEMP\cs-scratch\project"
npm run tauri dev

# Built binary — CLI flags:
claude-scope --home /tmp/scratch --project /tmp/scratch/project
```

When either override is active, a yellow **Sandbox mode** banner sits under the toolbar showing the active paths, so you can't forget you're in scratch mode. The picker still works — `--home` keeps redirecting user-scope writes regardless of which project you switch to mid-session.

#### `scripts/scratch-home.py`

One-shot helper that seeds a fresh scratch dir with realistic example settings (allow / deny / ask rules across all four scopes, plus `env` / `hooks` / `theme` blocks so the move-key flow has data) and prints the launch command.

```sh
python scripts/scratch-home.py            # seeds a temp dir, prints launch line
python scripts/scratch-home.py /tmp/sb    # seeds /tmp/sb explicitly, idempotent
python scripts/scratch-home.py /tmp/sb --force   # wipe + reseed
```

### Tests

```sh
# Rust unit tests (scope discovery, atomic writes, move logic, watcher plan)
(cd src-tauri && cargo test)

# Front-end type check
npx tsc --noEmit
```

For UI changes, also run the manual smoke pass in [`docs/SMOKE_TEST.md`](./docs/SMOKE_TEST.md) — it's a ~5-minute checklist covering the move flow, watcher reload, scope visibility, and side-effect verification.

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

Every push to `dev` and every PR against `dev` runs four jobs in parallel, producing six check runs total:

- `build` — a matrix job across `ubuntu-24.04`, `windows-latest`, `macos-latest`. Each runs `npm ci`, `tsc --noEmit`, `vite build`, `cargo test --lib --locked`.
- `msrv (1.88)` — validates the declared MSRV via `cargo check --lib --tests --locked` on Rust 1.88.0
- `lint` — runs `pre-commit/action@v3` (Biome, rustfmt, clippy, hygiene hooks)
- `security` — `rustsec/audit-check` against the RustSec advisory DB + `npm audit --package-lock-only --audit-level=high` (reads the lockfile directly; no `node_modules` install, no lifecycle scripts)

## Automated dependency updates

[Dependabot](./.github/dependabot.yml) opens weekly grouped PRs for Cargo, npm, and GitHub Actions minor/patch bumps. Major bumps land as separate PRs.

## Reporting security issues

Please use [GitHub Private Vulnerability Reporting](https://github.com/bminier/claude-scope/security/advisories/new) — see [`SECURITY.md`](./SECURITY.md) for details.

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

Releases are currently **unsigned**. That means:

- **Windows**: first launch shows a SmartScreen warning (click "More info" → "Run anyway").
- **macOS**: first launch shows a Gatekeeper warning (right-click the app → Open → Open).
- **Linux**: no warning.

Signing would require a Windows code-signing certificate and/or an Apple Developer ID + notarization. Planned, but out of scope until there's demand.

### Manual dispatch

The release workflow is also wired up for `workflow_dispatch` — useful for rebuilding an existing tag or testing workflow changes before cutting a real release. From the Actions UI, click **Run workflow** on the Release workflow and pick a branch:

- **Leave `tag_name` blank** → the workflow builds off the dispatched branch's HEAD, generates a `nightly-<full-sha>` tag (full 40-char commit SHA, so two nightlies on the same commit share a draft but different commits never collide), and creates a draft pre-release titled `ClaudeScope nightly-<full-sha>`. Delete the draft when you're done.
- **Set `tag_name` to an existing tag** (e.g. `v0.1.0`) → the workflow checks out that exact tag and rebuilds its draft release. Useful for re-uploading artifacts if a platform job was flaky.

## License

[MIT](./LICENSE) © Brian Minier
