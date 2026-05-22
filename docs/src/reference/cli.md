# CLI

`claude-scope-cli` is a terminal front end that re-uses the same Rust
backend the GUI does — scope discovery, atomic writes, the move-leaf
primitive, the audit log. It's installed alongside the GUI by the
release installers and is also available via `cargo build --bin
claude-scope-cli` for source builds.

The Claude Code skill side (#13) wraps this binary; the JSON output of
every subcommand is stable enough that a script or skill can rely on
it.

## Global flags

Every subcommand accepts:

- `--project-dir <PATH>` — pin the project root. Without this,
  ClaudeScope walks up from `cwd` for the nearest `.git` or `.claude/`.
- `--home-dir <PATH>` — sandbox mode. Redirects user / user-local
  scope lookups (and the audit log) under this directory.

## Subcommands

### `scopes`

List recognized scopes and their on-disk state.

```sh
claude-scope-cli scopes [--json]
```

### `show`

Print one scope's raw contents, or the effective merged view.

```sh
claude-scope-cli show --scope user [--json]
claude-scope-cli show --effective [--json]
```

`--scope` and `--effective` are mutually exclusive.

### `list-rules`

List permission rules across scopes.

```sh
claude-scope-cli list-rules [--scope <SCOPE>] [--kind <KIND>] [--json]
```

`--kind` accepts `allow` / `deny` / `ask`.

### `list-projects`

Enumerate Claude projects discovered on this machine, via the
`~/.claude/projects/` transcript registry.

```sh
claude-scope-cli list-projects [--json]
```

### `version`

Print the same diagnostic block the GUI's About dialog shows. Useful
for bug reports — paste the output into the issue.

```sh
claude-scope-cli version [--json]
```

The clap-builtin `--version` still prints just the version line.

### `move`

Move a permission rule between scopes. Writes are atomic and create a
`.bak` of the original on the first write of the session.

```sh
claude-scope-cli move "Bash(git status)" --kind allow --from project --to user [--dry-run] [--json]
```

### `history`

Read the audit log. See [Undo and history](../user-guide/undo-redo.md)
for the user-facing description; the CLI surface mirrors the History
dialog one-for-one.

```sh
claude-scope-cli history [--since <DURATION>] [--limit <N>] [--kind <KIND>]... [--json]
```

- `--since` accepts `s` / `m` / `h` / `d` / `ms` suffixes. Bare
  numbers are rejected — the parser refuses to guess units.
- `--limit` defaults to 20; `0` is unlimited.
- `--kind` is repeatable: `move` / `add` / `delete` / `change-kind` /
  `restore`. OR semantics when multiple flags are passed.
- `--json` emits the same `AuditLogPage` wire shape the GUI's
  `list_audit_records` IPC returns.

Human output: tab-separated `ULID  ts  verb  scope-arrow  rule`,
newest-first.

### `undo` / `redo`

Step backward / forward through the audit log one operation at a
time, mirroring the GUI's Undo / Redo buttons.

```sh
claude-scope-cli undo [--dry-run] [--yes] [--json]
claude-scope-cli redo [--dry-run] [--yes] [--json]
```

`undo` reverts the most recent operation by writing the affected
files back to the snapshots in the log; `redo` re-applies the most
recently undone one. `redo` exits non-zero after a *sequence break*
(a change made since the last undo) — the forward stack is then
ambiguous, same rule as the greyed-out GUI button.

Both print the planned restore and prompt `Apply? [y/N]` unless
`--yes` is passed. `--dry-run` prints the plan and writes nothing.
Each successful run appends a `restore` entry with `actor: cli`.

### `restore`

Restore every file affected by a logged entry — and the entries after
it — back to its state before that entry.

```sh
claude-scope-cli restore <ENTRY-ID> [--dry-run] [--yes] [--json]
```

`<ENTRY-ID>` is a full ULID or any unique prefix; an ambiguous prefix
prints the candidates and exits non-zero. Same `--dry-run` / `--yes`
flags as `undo`.

For `undo` / `redo` / `restore`, `--json` emits the `RestorePreview`
for a `--dry-run` and the resulting `restore` entry (as an
`AuditRecordView`, the same shape `history --json` produces) once
applied.
