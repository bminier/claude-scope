# Audit log

ClaudeScope appends one JSON Lines record per successful write to
`~/.claude/claude-scope/audit.jsonl`. The log is the foundation for
[Undo and history](../user-guide/undo-redo.md); the design document
lives at
[#19](https://github.com/bminier/claude-scope/issues/19).

## Record shape

Each line is a self-contained JSON record. Phase 1 (#122) pinned the
schema; phases 3-5 (#124 / #125 / #126) added one optional field —
`restore` — for undo/redo/restore meta-entries, leaving every older
record valid.

```jsonc
{
  "id": "01HF...",                   // ULID (Crockford base32). Lex-sort = chrono-sort.
  "kind": "move",                    // move | add | delete | change_kind | restore
  "leaf_kind": "permission_rule",    // permission_rule | permission_list | top_level_key
  "actor": "gui",                    // gui | cli | skill — who triggered it
  "project_dir": "/work/proj",       // optional — user-scope-only ops omit
  "from": {                          // optional — Add omits; every restore omits
    "scope": "project",
    "file_path": "/work/proj/.claude/settings.json",
    "top_level_key": "permissions",
    "key_before": { "allow": ["Bash(ls)", "Read(**)"] },
    "key_after":  { "allow": ["Read(**)"] }
  },
  "to": {                            // optional — Delete omits
    "scope": "user",
    "file_path": "/home/me/.claude/settings.json",
    "top_level_key": "permissions",
    "key_before": { "allow": [] },
    "key_after":  { "allow": ["Bash(ls)"] }
  },
  "path": ["permissions", "allow", 0],
  "to_kind": null,                   // set on change-kind ops
  "claude_scope_version": "0.3.0"
}
```

The **affected top-level key** (`permissions` for permission ops, the
moved key for top-level moves) is snapshotted before AND after on
each side. That's the minimum needed for an undo to invert any logged
op without re-reading history, while staying narrower than the whole
file.

## Restore entries

An undo, redo, or restore-to-point appends a record with
`kind: "restore"`. Instead of `from` / `to` it carries a `restore`
object — a restore can touch more than two files, so two fixed sides
aren't enough:

```jsonc
"restore": {
  "target_id": "01HF...",            // the entry undone / redone / restored-to
  "direction": "undo",               // undo | redo | to_point
  "files": [                         // one Side per file the restore rewrote
    {
      "scope": "project",
      "file_path": "/work/proj/.claude/settings.json",
      "top_level_key": "permissions",
      "key_before": { "allow": ["Read(**)"] },
      "key_after":  { "allow": ["Bash(ls)", "Read(**)"] }
    }
  ]
}
```

Because each `files` entry snapshots before AND after, a restore is
itself invertible — undoing an undo is just another restore. The
undo/redo cursor is never stored: it's reconstructed on demand by
replaying the `direction` of every `restore` entry over the ordered
list of ops (`audit::undo_redo_state`). A write appended after an undo
latches a *sequence break*, which withholds redo rather than
discarding the forward stack.

Snapshot-restore — writing a file back to a `key_before` the log
already captured — is how undo stays faithful. Synthesizing a reverse
move/add/delete instead would mishandle a move into a scope that
already shared the rule (the reverse move would wrongly strip the
destination's own copy); writing the captured snapshot back cannot.

## Why ULID, not timestamps

ULIDs embed a 48-bit millisecond timestamp in their first half and a
random suffix in the second. Three benefits:

- **Lex-sort = chrono-sort.** A reader can `sort` the file by `id`
  and get correct ordering without trusting wall-clock timestamps,
  which can skew across machines or under NTP jumps.
- **One field, not two.** A separate `ts` field could drift from
  `id` under clock corrections. Decoding the timestamp from the ULID
  at read time keeps the two in lockstep by construction.
- **Compact**. 26 chars of base32, fits a line cleanly.

The frontend never parses Crockford base32 directly — the
`list_audit_records` IPC returns an `AuditRecordView` with `id` and a
derived `ts_ms: u64` flattened together. The CLI mirrors the same
shape with `--json`.

## Append is one syscall

`audit::append` opens with `O_APPEND` and writes the serialized
record + `\n` in a single `write_all` call. For any sane record size
that lands as a single kernel `write()` syscall, which the OS
executes atomically against concurrent appenders. That's the
property the truncated-tail-tolerance in `audit::read_all` relies on:
a partial line is treated as one skipped entry, not as a parse
failure that aborts the read.

## Fail-open

A failure to append never fails the primary write. The user asked
ClaudeScope to move a rule; an `audit.jsonl` permission error
doesn't negate that. The append happens *after* the settings write
succeeds, so a failure here means the file is updated but not
logged — which is the safe direction for ambiguity.

Tauri emits an `audit-error` event when an append fails, surfaced as
a non-fatal warning in the GUI.

## Rotation

Once `audit.jsonl` exceeds the configurable size cap (default
10 MB), `rotate_if_needed` runs before the next append. The active
file is renamed to `audit-YYYY-MM.jsonl` (using the file's mtime, so
the stamp reflects the data inside) and the next entry lands in a
fresh `audit.jsonl`. Same-month collisions become `-2`, `-3`, ...
suffixes.

The reader **only scans the active file**. Archives are archival —
the History dialog and `claude-scope-cli history` don't surface
them. Undo / redo and restore-to-point are bounded to the active log
for the same reason; restoring across an archive boundary would be a
separate feature.

## Sandbox

When `--home` is set, `emit_audit` skips the rotation + append
sequence entirely. Scratch runs cannot pollute the real
`~/.claude/claude-scope/`. The preferences-write path has the same
guard.
