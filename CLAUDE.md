# ClaudeScope

Desktop GUI for promoting Claude Code settings between scopes

## GitHub

- Repo: https://github.com/bminier/claude-scope
- Default branch: `dev`
- Issues: tracked on GitHub

## Workflow

Work on feature branches. Open PRs targeting `dev`. Follow conventional commits.

## Versioning

A version bump must update four files in lockstep: `package.json`,
`package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`.
`src-tauri/Cargo.lock` regenerates on the next `cargo` run.

Do **not** regenerate `package-lock.json` with `npm install --package-lock-only`
on Windows — npm drops Linux-only optional deps (e.g. `@emnapi/*`,
`@napi-rs/*`) and `npm ci` then fails on the Linux CI runner. For a pure
version bump, edit only the two top-level `"version"` fields in the lockfile;
leave the dependency tree alone.

## Bootstrap

## Origin

Drafted in the Assistant session on 2026-04-18 after a conversation about Claude
Code settings management. Brian was frustrated hand-editing JSON to move
permission rules between scopes and asked for a "pretty little GUI". This
project is also the pilot for the new `launch.py --bootstrap --remote` flow —
so the first implementation session is expected to run autonomously in Claude
Code Web with only this document as context.

## Problem

Claude Code reads settings from several JSON files, each at a different scope.
The scopes (in precedence order, highest first) are:

1. **Managed** — enterprise-deployed (out of scope for this project)
2. **Local** — `./.claude/settings.local.json` (project-local, gitignored)
3. **Project** — `./.claude/settings.json` (committed with the project)
4. **User-Local** — `~/.claude/settings.local.json` (machine-local override)
5. **User** — `~/.claude/settings.json` (machine-global)

`~/.claude/settings.local.json` was originally excluded here, but Claude Code
is observed to create and use it when the user's home directory is itself
inside a git repo, so ClaudeScope treats it as a first-class scope on par
with the other three.

Moving a rule from one scope to another today requires hand-editing the JSON in
each file, which is tedious, easy to break, and loses JSON comments / ordering.

## Goal

A small desktop-feeling app that lets the user view every settings entry
across the recognized scopes and move entries between scopes with one click,
without breaking the JSON.

Primary user: Brian. Primary machine: Windows 11. Secondary: macOS.

## Must-have behavior

- Show the *effective* merged view (what Claude Code actually sees) as well as
  the per-scope raw view.
- List every permission rule (`permissions.allow`, `permissions.deny`, and any
  future `ask`) with its scope and let the user promote/demote rules to any
  other recognized scope.
- Support other top-level settings (e.g. `env`, `hooks`, `theme`) — at minimum
  view them; editing is a stretch goal, but scope transfer for them is in scope
  if it's not much extra work.
- Write changes atomically: read file, patch in memory, write to tempfile,
  rename. Never leave a partial file behind. Back up the original alongside
  the new file (`settings.json.bak` next to the target) on first write in a
  session.
- Validate JSON after any write. If the result would be invalid, refuse.
- Respect that `settings.local.json` is gitignored and `settings.json` is
  committed; don't be surprised by either being absent.
- Honor existing file formatting as best we can (preserve key ordering, indent
  width). Parse with a JSON-with-comments library if one is convenient; plain
  JSON is acceptable for v1.

## Nice-to-have

- Diff preview before any write.
- Inline validation of permission rule syntax (`WebFetch(domain:...)`,
  `Bash(git *)`, etc.) with a link to the docs.
- Quick search / filter across rules.
- Auto-reload when files change on disk.
- Support running from a project directory picker (choose which project's
  `.claude/` to load).

## Non-goals for v1

- Editing managed (enterprise) settings.
- Authoring new permissions from scratch — that can come later; v1 is a
  **promote/demote** tool.
- Multi-user / cloud sync.
- Bundled distribution (installers, auto-update).

## Architecture suggestion

Tauri (Rust + webview front end) is the recommended stack. Reasoning:

- Small binary and low overhead compared to Electron.
- First-class Windows support (primary target).
- File system access on the native side, so atomic writes are trivial.
- Single binary output makes "install from source" low-friction.

Alternatives considered:

- **Electron** — works, but heavyweight for such a small tool.
- **Local web app (Flask + browser)** — rejected; needs a separate browser
  window and background process.
- **TUI (Textual / ratatui)** — cleaner for Brian's workflow, but he asked for
  a GUI, so respect that.

Front end: any reasonable web-frame (React/Svelte/Solid). Pick one; don't
overthink it. Tailwind or vanilla CSS — whichever lets the first cut ship.

Back end (Rust): one module for scope discovery, one for read/write/diff, one
for validation. Keep the JSON parse lenient, the write path paranoid.

## Definition of done for v1

- Launches on Windows (`cargo tauri dev` works; `cargo tauri build` produces a
  runnable binary).
- Opens to a multi-column view covering every recognized scope, laid out
  broadest-on-the-left to narrowest-on-the-right (User / User-Local /
  Project / Local).
- User can drag or click-to-move a permission rule from one column to another,
  and the backing JSON files update atomically with a `.bak` backup.
- Round-trip test: after a move, `claude` reads the new effective config
  without complaint.

## First steps for the agent picking this up

1. `cargo tauri init` (or whichever Tauri scaffold is current).
2. Wire up scope discovery: find `~/.claude/settings.json`, plus the nearest
   `.claude/settings.json` and `.claude/settings.local.json` walking up from
   cwd.
3. Read + parse + render a read-only three-column list.
4. Add the move action. Keep writes behind a confirm step until the diff-view
   lands.

Open a draft PR early so Brian can steer before too much is built.
