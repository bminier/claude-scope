# Contributing

This document covers local dev setup, the test suites, and the
release flow. For the original project brief — problem statement,
must-have behavior, non-goals — see [`CLAUDE.md`](https://github.com/bminier/claude-scope/blob/dev/CLAUDE.md)
at the repo root.

## Develop

### Prerequisites

- **Rust** — stable, 1.88+ (imposed by Tauri 2's transitive deps).
  Install via [rustup](https://rustup.rs):
  - **Windows:** `winget install Rustlang.Rustup` (then restart your
    terminal).
  - **macOS / Linux:**
    `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`.

  > The Tauri CLI is bundled as an npm dev dependency —
  > `cargo install tauri-cli` is **not** needed. Use
  > `npm run tauri dev` (not `cargo tauri dev`).
- **Node.js** 20+ and **npm**.
- **Linux build deps** (only on Linux):
  ```sh
  sudo apt-get install libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
  ```
- **pre-commit** (optional, strongly recommended — CI runs the same
  hooks):
  ```sh
  pip install --user pre-commit
  pre-commit install
  ```

### Run

```sh
npm install
npm run tauri dev
```

For a non-destructive dogfood loop against a throwaway home, see
[Moving rules → Sandbox mode](./user-guide/moving-rules.md#sandbox-mode).

### Tests

```sh
# Rust unit tests (scope discovery, atomic writes, audit log, …).
(cd src-tauri && cargo test)

# Front-end type check + Vitest.
npx tsc --noEmit
npx vitest run
```

A manual smoke pass for UI changes lives in
[`docs/SMOKE_TEST.md`](https://github.com/bminier/claude-scope/blob/dev/docs/SMOKE_TEST.md)
at the repo root — a ~5-minute checklist covering the move flow,
watcher reload, scope visibility, and side-effect verification.

### Lint / format

```sh
# Run everything the `lint` CI job runs (Biome, rustfmt, clippy,
# hygiene hooks).
pre-commit run --all-files

# Front-end only:
npm run lint       # biome check (lint + format check)
npm run format     # biome format --write

# Rust side:
(cd src-tauri && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings)
```

### Build

```sh
npm run tauri build
```

### Docs

This site is built with [mdBook](https://rust-lang.github.io/mdBook/).
Edit the Markdown under `docs/src/` and either of:

```sh
# Live-reload preview at http://localhost:3000.
mdbook serve docs/

# One-shot build to docs/book/ (the CI artifact).
mdbook build docs/
```

`mdbook-linkcheck` runs as a hard gate during CI — broken internal
links fail the build. External link rot is monitored separately by
the lychee workflow on the same Markdown surface.

## CI

Every push to `dev` and every PR against `dev` runs four jobs in
parallel:

- **`build`** — matrix across `ubuntu-24.04`, `windows-latest`,
  `macos-latest`. Each runs `npm ci`, `tsc --noEmit`, `vite build`,
  `cargo test --lib --locked`.
- **`msrv (1.88)`** — validates the declared MSRV via
  `cargo check --lib --tests --locked` on Rust 1.88.0.
- **`lint`** — `pre-commit/action@v3` (Biome, rustfmt, clippy,
  hygiene hooks).
- **`security`** — `rustsec/audit-check` against the RustSec
  advisory DB + `npm audit --package-lock-only --audit-level=high`
  (reads the lockfile directly; no `node_modules` install, no
  lifecycle scripts).

## Releasing

Version bumps are automated by `release-please` but **manually
triggered**. Run **Actions → Release Please → Run workflow** and
release-please walks `dev` since the last tag, opens a "release PR"
that bumps every version-carrying file in lockstep (`package.json`,
`package-lock.json`, `src-tauri/Cargo.toml`,
`src-tauri/tauri.conf.json`, plus `src-tauri/Cargo.lock` via a
follow-up sync step). Merging that PR pushes a `vX.Y.Z` tag, which
triggers `release.yml` to build and upload installers +
`SHA256SUMS.txt`. Release notes live on the GitHub Release itself —
`skip-changelog` is set in `release-please-config.json`, so there is
no tracked `CHANGELOG.md`.

If you ever need to bump versions by hand (e.g. release-please is
broken), do **not** regenerate `package-lock.json` with
`npm install --package-lock-only` on Windows — npm drops Linux-only
optional deps (e.g. `@emnapi/*`, `@napi-rs/*`) and `npm ci` then
fails on the Linux CI runner. Edit only the two `"version"` fields
in the lockfile and leave the dependency tree alone.
`scripts/sync-cargo-lock.py` updates the `claude-scope` entry in
`Cargo.lock` without touching anything else.

### Known limitation: unsigned builds

Releases are currently **unsigned**:

- **Windows**: first launch shows a SmartScreen warning (click "More
  info" → "Run anyway").
- **macOS**: first launch shows a Gatekeeper warning (right-click
  the app → Open → Open).
- **Linux**: no warning.

Signing would require a Windows code-signing certificate and/or an
Apple Developer ID + notarization. Planned, but out of scope until
there's demand.
