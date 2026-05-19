# Undo and history

ClaudeScope keeps an append-only **audit log** of every successful
write — moves, adds, deletes, kind changes. The log answers questions
the single-slot `.bak` model can't:

- *"What did I change in the last hour, in order?"*
- *"Undo the last change."* (Not the whole session — just the last one.)
- *"Take me back to before I imported that preset."*

## What ships today

| Phase | Feature | Status |
|---|---|---|
| 1 | JSONL audit log at `~/.claude/claude-scope/audit.jsonl` | Shipped ([#19](https://github.com/bminier/claude-scope/issues/19) phase 1) |
| 2 | Read-only **History** dialog (topbar button) listing entries newest-first | Shipped (#19 phase 2) |
| 3 | `Ctrl+Z` / `Cmd+Z` undo via the diff-confirm modal | Tracked in [#124](https://github.com/bminier/claude-scope/issues/124) |
| 4 | "Restore to before this" — multi-step rollback from a History row | Tracked in [#125](https://github.com/bminier/claude-scope/issues/125) |
| 5 | CLI parity (`claude-scope-cli history` shipped; `undo / redo / restore` pending) | Tracked in [#126](https://github.com/bminier/claude-scope/issues/126) |

The on-disk schema is stable as of phase 1, so phases 3-5 don't break
existing log entries. See
[Architecture → Audit log](../architecture/audit-log.md) for the
record format.

## Using the History dialog

Click **History** in the topbar. Entries appear newest-first, with:

- A color-tinted **verb** (`Move permission rule` /
  `Add permission rule` / `Delete permission rule` /
  `Change kind → deny`).
- An ISO timestamp (locale-formatted text, ISO `datetime=…` for
  screen readers).
- The scope arrow (`Project → User` for cross-scope moves; single
  scope for adds/deletes).
- The affected rule string, recovered by diffing the before/after
  snapshots in the record.

The bottom of the list surfaces a count of any malformed entries the
reader skipped — useful if `audit.jsonl` was hand-edited or partially
written during a crash.

## Rotation

Once the log exceeds the size cap (default 10 MB) it's renamed to
`audit-YYYY-MM.jsonl` and the next entry lands in a fresh file. Old
archives stay on disk indefinitely — delete them by hand if you want.
Both behaviors are configurable under **Settings → Audit log
rotation**.

## CLI

`claude-scope-cli history` mirrors the GUI dialog from a shell:

```sh
# Last 20 entries, newest first.
claude-scope-cli history

# Just deletes in the last hour, JSON.
claude-scope-cli history --since 1h --kind delete --json

# Everything in the active log (no cap).
claude-scope-cli history --limit 0
```

`--json` emits the same `AuditLogPage` wire format the GUI uses, so a
[Claude Code skill (#13)](https://github.com/bminier/claude-scope/issues/13)
can consume CLI output and IPC payloads through one shared type once
that lands.
