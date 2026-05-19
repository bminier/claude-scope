# Atomic writes

Every settings-file write in ClaudeScope funnels through a single
`atomic_write_json` chokepoint in `src-tauri/src/io_atomic.rs`. The
sequence:

1. **Serialize** the in-memory `SettingsDoc` to bytes.
2. **Re-parse** the produced bytes via `serde_json::from_slice` — if
   they don't round-trip, the write is refused before anything hits
   disk. Bad bytes never escape.
3. **Tempfile** in the same directory as the target.
4. **Stamp recheck** — a `FileStamp` (mtime + length) captured at load
   time is compared to the target's current state. If the file
   changed on disk between load and save, the write is refused with a
   "reload and retry" error rather than overwriting the external
   edit. The recheck has a microsecond-scale TOCTOU window between
   the comparison and the `persist` — a writer that lands inside
   that window will still be overwritten.
5. **Rename** the tempfile over the target. The rename is
   filesystem-atomic.
6. On Unix, the parent directory is also `fsync`'d so the rename is
   crash-durable. On Windows, the parent-dir flush is skipped —
   `std::fs` cannot open a directory handle for flushing without a
   small raw-winapi wrapper, and NTFS journals rename metadata in
   `MoveFileEx` already.

## Backups

On the first write of a given file per session, the original is
copied to `<file>.bak`. Existing `.bak` files from previous runs are
preserved, not clobbered. This is a one-shot backup attempt, not a
best-effort write-through: **if backup creation is required and the
copy fails, the error is surfaced and the write is not attempted**.

The behavior is opt-out under **Settings → Backups**
([#88](https://github.com/bminier/claude-scope/issues/88)). Users
whose `.claude/` is version-controlled and don't want extra files can
turn it off — at which point the audit log
([Undo and history](../user-guide/undo-redo.md)) becomes the only
safety net.

## Moves are two-file transactions

A scope-to-scope move writes the **destination first**, then removes
the rule from the source. If the source write fails, the destination
write is rolled back so the rule ends up in exactly one scope rather
than being lost *or* duplicated. If the rollback itself fails the
user sees an explicit error rather than a silent partial state.

The rollback path can't reuse `apply_move_leaf_impl` — it has to
write the pre-move destination directly, with no `.bak` (we already
created one), and with no stamp check (we are the canonical writer of
the destination at that point). This is why the apply impl threads
`backups` and `stamp` parameters down explicitly rather than reading
them from a static.

## What's atomic vs. what's not

**Atomic:**

- The bytes hitting any single file. Either the new contents land
  intact or the old contents are preserved — never a partial write.
- The rename of tempfile-over-target on a single filesystem.
- The bytes-validate-as-JSON gate. The re-parse step runs before any
  filesystem operation; broken JSON never reaches disk.

**Not atomic:**

- The pair of writes that constitute a cross-scope move. They are
  two filesystem operations; if the process is killed between them,
  the rollback can't run. The destination's `.bak` is the recovery
  path in that case.
- The audit log append relative to the settings write. The audit
  entry lands *after* the settings write succeeds, so a process kill
  in that narrow window means a write happened but isn't logged. The
  log being authoritative-for-history-but-not-state
  ([Architecture → Overview](./overview.md)) absorbs that gap.
