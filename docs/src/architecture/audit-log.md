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
  "claude_scope_version": "0.6.0"
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
a non-fatal warning in the GUI; the CLI prints the same information
to stderr.

## Rotation and concurrency (#166)

Two ClaudeScope sessions can target the same audit log — a GUI plus a
CLI invocation, two CLI invocations in different shells, or a Claude
Code skill driving the CLI alongside an open GUI. The rotation +
append sequence has to behave correctly under that interleaving.

**Cross-process lock.** Every rotate-then-append pair runs inside
`audit::persist`, which holds an advisory exclusive lock on
`<audit_dir>/audit.lock` for the duration. `fs2` provides the
cross-platform primitive (`flock` on POSIX, `LockFileEx` on
Windows). Two processes both arriving with the active log near the
cap serialize on the lock: the first rotates and appends to a fresh
active file; the second waits, sees the active file is fresh
(under cap, no rotation needed), and appends to the same fresh
file.

**Archive-aware reads.** Even with the lock, `audit.jsonl` may have
been rotated into `audit-YYYY-MM.jsonl` between two appends — that's
the normal-case behavior under rotation, not a bug. `audit::read_all`
therefore enumerates every `audit-*.jsonl` archive in the directory,
reads them in filename order (which is chronological — the naming
encodes `YYYY-MM` plus collision suffix), then reads the active log,
and returns the concatenated record stream. Undo / redo / history
all see every persisted record regardless of which file it lives in.

**File naming.** The active log is `audit.jsonl`. The first
rotation lands at `audit-YYYY-MM.jsonl` using the file's mtime so
the stamp reflects the data inside. Same-month collisions become
`audit-YYYY-MM-2.jsonl`, `audit-YYYY-MM-3.jsonl`, … — the lex order
of those filenames also matches chronological order, so the
archive enumeration above stays sorted without timestamp parsing.

**Cap.** Default 10 MB, configurable under Settings → Audit log
rotation. The cap is on the **active** file's size; archives can
grow unbounded across history.

**Restore across archives.** Phase 4 (`plan_restore_to`) builds its
window from `audit::read_all`, which now spans archives. Restoring
to a point that lives in an archive works the same way as restoring
to a point in the active log — same `record_sides` walk, same per-
file snapshot revert.

## Security invariants

The audit log is read by every undo / redo / restore call, and its
contents drive direct writes to the user's settings files. A hostile
line appended to `audit.jsonl` (cloud-sync collision, malicious
postinstall, compromised tool with user-scope write) must not be able
to escalate into arbitrary file-write. Three gates enforce that.

**Path allowlist (#183).** Every audit-log boundary —
`undo_redo_target`, `restore_to_point_preview`,
`apply_restore_to_point`, and the CLI's `cmd_undo` / `cmd_redo` /
`cmd_restore` — calls `validate_audit_records` immediately after
reading records and before building any `RestorePlan`. Each
`Side.file_path` must equal one of the four resolved `ScopePaths`
slots (`local`, `project`, `user_local`, `user`) for that record's
own `project_dir`, against the active home override. Canonicalization
absorbs platform-specific path forms (`/var → /private/var` on macOS,
`\\?\C:\…` extended-length paths on Windows). The basename must be
`settings.json` or `settings.local.json`. Any path outside the
allowlist is refused with a clear `path injection refused` message;
no writes happen.

**Degraded-log refusal (#170).** `audit::read_all` reports a count
of unreadable lines (corrupt JSON, schema drift the reader can't
parse, truncated tails). Every undo / redo / restore path gates on
that count: the GUI's `require_clean_audit_log` and the CLI's
`read_audit_log` both refuse with a non-zero exit / blocked topbar
button when `skipped > 0`. A partial log could leave the undo/redo
state machine pointing at an op that was already undone, and
confirming would emit a duplicate `restore` entry.

**Tail-ID stalecheck (#165, #171).** Every restore re-reads the log
immediately before applying and compares the trailing record's ULID
against the value captured at preview time. A mismatch means the log
changed under the user — refuse rather than apply a plan against a
log the user didn't see. This catches concurrent GUI/CLI appends in
the prompt window AND the (smaller) window between the initial read
and `--yes` apply.

## Sandbox

When `--home` is set, `emit_audit` skips the rotation + append
sequence entirely. Scratch runs cannot pollute the real
`~/.claude/claude-scope/`. The preferences-write path has the same
guard.
