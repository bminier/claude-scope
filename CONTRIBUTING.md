# Contributing to ClaudeScope

Thanks for the patch. ClaudeScope is a small Tauri 2 desktop app written in
Rust + TypeScript; the README and `.github/copilot-instructions.md` cover
build commands and architecture, so this file sticks to commit hygiene and
how releases are cut.

## Branches

`dev` is the default branch and the merge target for every PR. There is no
`main`. Named `prerelease/<name>` and `release/<name>` branches are cut from
`dev` for staging and release respectively.

## Conventional Commits

Every commit on `dev` (and therefore every PR title that gets squash-merged)
must follow [Conventional Commits](https://www.conventionalcommits.org/).
The release pipeline reads commit subjects to decide the next version and
to populate the GitHub Release notes.

The types release-please cares about:

| Type        | Bump on `dev` | Appears in release notes | Notes                                             |
|-------------|---------------|--------------------------|---------------------------------------------------|
| `feat:`     | minor         | **Features**             | New user-visible functionality.                   |
| `fix:`      | patch         | **Bug Fixes**            | Bug fixes.                                        |
| `perf:`     | patch         | **Performance**          | Perf improvements without behavior change.        |
| `revert:`   | patch         | **Reverts**              | Reverts a previous commit.                        |
| `refactor:` | none          | hidden                   | Internal refactor; no user-visible change.        |
| `test:`     | none          | hidden                   | Test-only changes.                                |
| `docs:`     | none          | hidden                   | Documentation changes.                            |
| `build:`    | none          | hidden                   | Build-system / packaging tweaks.                  |
| `ci:`       | none          | hidden                   | CI workflow / pipeline changes.                   |
| `chore:`    | none          | hidden                   | Anything else (deps bumps, infra, housekeeping).  |

A `!` after the type (e.g. `feat!:` / `fix!:`) or a `BREAKING CHANGE:` footer
forces a major bump — except pre-1.0, where it bumps the minor instead
(release-please's default behavior, matching semver's "anything goes
before 1.0" guidance).

A scope is encouraged but optional: `feat(ui): drag-and-drop applies the move`
reads cleaner in the changelog than a bare `feat:`.

## Releases

Releases use [release-please](https://github.com/googleapis/release-please-action)
plus the existing tag-driven `release.yml`. The cut is **maintainer-triggered**
— release-please does not open a release PR on every merge into `dev`.

The flow:

1. Land conventional-commit PRs into `dev`.
2. When ready to cut a release, manually run **Actions → Release Please →
   Run workflow** on `dev`. release-please walks `dev` since the last tag,
   computes the next semver bump from commit subjects, and opens a "release
   PR" that bumps the version in `package.json`, `package-lock.json`,
   `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, and
   `src-tauri/Cargo.lock`. Release notes are generated onto the GitHub
   Release itself rather than into a tracked `CHANGELOG.md`
   (`skip-changelog` in `release-please-config.json`).
3. Merge the release PR. release-please then pushes the `vX.Y.Z` tag and
   creates the GitHub Release.
4. The tag push triggers `.github/workflows/release.yml`, which builds the
   Tauri installers for Linux / Windows / macOS-universal and uploads them
   plus a `SHA256SUMS.txt` to that release as a draft.
5. Review the draft release in the GitHub UI and click "Publish".

If you re-run the workflow after additional commits land on `dev`, the
existing release PR is rebased to include them — same machinery, just
triggered on demand instead of on every push.

### Caveats

- **Cargo.lock sync race.** When release-please opens a fresh release PR,
  CI runs immediately on the bumped tree. A follow-up job in
  `release-please.yml` pushes a `chore: sync Cargo.lock` commit so that
  `cargo --locked` passes — but pushes from the default `GITHUB_TOKEN`
  do not retrigger CI (GitHub policy, prevents recursion). If the first CI
  run on a release PR fails on stale `Cargo.lock`, click "Re-run failed
  jobs" on the PR after the sync commit lands. Subsequent rebases trigger
  CI cleanly because they come from external pushes to `dev`.

- **Builds are unsigned.** Release artifacts are not code-signed for either
  Windows or macOS (revisit before 1.0). `SHA256SUMS.txt` on the release
  is the integrity check.

- **No `main` branch.** release-please runs on `dev` (`target-branch: dev`
  in the workflow). Don't follow the upstream "release-please runs on main"
  template snippets — they don't apply here.

## Tooling

- **commitlint** is not currently enforced. Convention is via review for now.
- **Pre-commit hooks** lint the diff (`biome`, `cargo fmt`, etc.). Run
  `pre-commit install` once after cloning.
- See `.github/copilot-instructions.md` for build and dev commands.
