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
| 3 | Undo / redo via the diff-confirm modal, `Ctrl`/`Cmd`+`Z` shortcuts | Shipped ([#124](https://github.com/bminier/claude-scope/issues/124)) |
| 4 | "Restore to before this" — multi-step rollback from a History row | Shipped ([#125](https://github.com/bminier/claude-scope/issues/125)) |
| 5 | CLI parity — `claude-scope-cli history / undo / redo / restore` | Shipped ([#126](https://github.com/bminier/claude-scope/issues/126)) |

The on-disk schema is stable as of phase 1; phases 3-5 added an
optional `restore` field for undo/redo/restore meta-entries without
breaking older log entries. See
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

A `restore` entry (the result of an undo / redo / restore-to-point)
shows as `Undo` / `Redo` / `Restore to point`, names the entry it
acted on, and lists the scopes it touched.

The bottom of the list surfaces a count of any malformed entries the
reader skipped — useful if `audit.jsonl` was hand-edited or partially
written during a crash.

## Undo and redo

The topbar's **Undo** and **Redo** buttons — and `Ctrl`/`Cmd`+`Z` /
`Ctrl`/`Cmd`+`Shift`+`Z` — step backward and forward through the log
one operation at a time. Each button's tooltip names exactly what the
next step would do (e.g. *"Undo: Move permission rule Bash(ls)
(3 min ago)"*).

Every undo and redo previews before it writes: it routes through the
same diff-confirm modal a move does, so you see the per-file
before/after and confirm. Confirming appends a new `restore` entry —
the log is append-only, so an undo is itself recorded and can be
redone.

**Sequence break.** If you make a fresh change after an undo, the
redo stack is ambiguous — Redo greys out and its tooltip explains
why, rather than silently discarding the forward history.

**External edits.** Undo writes a file back to the snapshot the log
captured. If the file was hand-edited since (so its current contents
differ from what the log expected), the confirm modal shows a warning
band — you can still proceed, but you'll be overwriting that edit.

**Disabled with a "log degraded" tooltip.** If
`~/.claude/claude-scope/audit.jsonl` has unreadable lines (corrupt
JSON, a crash mid-write, schema drift the build doesn't recognize),
both Undo and Redo are withheld until the log is repaired. The
tooltip names the count of unreadable entries. Acting against a
partial log could emit a duplicate restore entry, so ClaudeScope
refuses rather than guess. The CLI exits non-zero with the same
message. Repair option: copy `audit.jsonl` aside, drop the broken
lines, restart.

**"Log changed since the preview."** If a concurrent CLI or GUI
session writes to the audit log while the confirm modal is open,
the apply is refused with that message — re-open the History view
and retry against the fresh state. Same posture under
`claude-scope-cli undo` and `--yes`.

**"Path injection refused."** If an audit-log entry's `file_path`
points outside the legitimate scope set for its project (e.g. an
attacker hand-wrote a hostile line into `audit.jsonl`), the restore
is refused before any I/O. See the
[Security threat model](../security.md#threat-model) for context.

## Restore to before an entry

Single-step undo walks back one operation at a time. To jump back
multiple operations at once — *"take me back to before I imported
that preset"* — open **History** and click the **Restore** button on
the target row.

That reverts the targeted entry **and every entry logged after it**:
ClaudeScope walks the log forward, computes the net per-file delta,
and writes each affected file back to its state before the target.
The confirm modal lists every file that will change and how many
operations are being reverted (*"Restoring to 4 ops back"*). A
restore-to-point is itself a single log entry, so it can be undone
like any other operation.

## Rotation

Once the log exceeds the size cap (default 10 MB) it's renamed to
`audit-YYYY-MM.jsonl` and the next entry lands in a fresh file. Old
archives stay on disk indefinitely — delete them by hand if you want.
Both behaviors are configurable under **Settings → Audit log
rotation**.

## CLI

`claude-scope-cli` mirrors the History dialog and the undo / redo /
restore actions from a shell:

```sh
# Last 20 entries, newest first.
claude-scope-cli history

# Undo the most recent change — previews, then prompts.
claude-scope-cli undo

# Redo it, no prompt.
claude-scope-cli redo --yes

# Preview a restore-to-point without writing.
claude-scope-cli restore 01JABC --dry-run
```

The entry id passed to `restore` is a full ULID or any unique prefix.
`--dry-run` prints the planned restore and writes nothing; `--yes`
skips the confirm prompt; `--json` emits machine-readable output. CLI
restores log a `restore` entry with `actor: cli`, so a GUI session
sees them in its History. See [CLI reference](../reference/cli.md)
for the full subcommand surface.
