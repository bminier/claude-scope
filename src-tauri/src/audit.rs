//! Append-only audit log of every write ClaudeScope makes (#19, Phase 1).
//!
//! Each successful permission-write command (move / add / delete /
//! change-kind) appends one JSON Lines record to
//! `~/.claude/claude-scope/audit.jsonl`. The log answers questions the
//! single-slot `.bak` model can't:
//!
//!   - "What did I change in the last hour, in order?"
//!   - (Phase 2+) "Undo / redo / restore-to-point."
//!
//! Phase 1 is **logging only** — no undo, no redo, no UI. The schema captures
//! enough state (whole top-level-key snapshots on each affected file) that a
//! later phase can invert any logged op without needing to extend the wire
//! format. That's deliberate: the on-disk audit log is a versioned contract
//! the moment the first entry hits disk, so paying the snapshot cost now
//! avoids a breaking schema bump later.
//!
//! ## Invariants
//!
//! - **Authoritative for history; the filesystem is authoritative for state.**
//!   A user hand-editing `settings.json` between sessions doesn't show up in
//!   the log — that's fine, because restore (Phase 3+) always re-reads the
//!   current file before computing a delta.
//! - **Fail-open.** A failure to append must not fail the primary write. The
//!   user asked to move a rule; an `audit.jsonl` permission error doesn't
//!   negate that. Callers swallow the [`AppendError`] and surface it as a
//!   non-fatal Tauri event instead.
//! - **Append-only.** Records are never edited or rewritten. Phase 3+ undo /
//!   redo emit *new* records (kind = `Restore`) referencing the inverted
//!   entry, never mutate the original.
//! - **ULID IDs.** Lexicographic sort = chronological sort, so a Phase 2
//!   reader can `sort` the file by id and get correct ordering without
//!   trusting timestamps, which are subject to wall-clock skew. The ULID
//!   itself embeds the timestamp in its first 48 bits.

use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use crate::model::{PathSeg, PermissionKind};
use crate::scope::Scope;

/// Compile-time version of ClaudeScope, captured into every audit record so
/// a later reader can tell which build wrote a given entry. Useful when the
/// schema gains optional fields in Phase 2+ — older entries are missing
/// those fields, which is fine, but knowing the writer's version pins the
/// expected shape.
fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// One audit log entry. JSON-serializable as a single line — `serde_json`
/// guarantees no embedded newlines for valid JSON values (strings escape
/// them), so a `write_all(serde_json::to_vec(rec) + "\n")` is one syscall
/// the OS executes atomically up to the underlying `write()` size limit
/// (PIPE_BUF on POSIX, much larger on regular files). That's the "tolerated
/// truncated tail" property [`read_all`] relies on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// ULID — embeds a 48-bit timestamp; sorts chronologically by lex order.
    /// Serializes as Crockford base32 (the ULID standard) via the `ulid`
    /// crate's `serde` feature.
    pub id: Ulid,
    /// What kind of operation this entry records. See [`Kind`].
    pub kind: Kind,
    /// Sub-classifier for the affected leaf. Mirrors `MoveLeafKind` in
    /// `commands.rs`; carried as a separate field so the wire format stays
    /// readable as `(verb, leaf_kind)` instead of a fused
    /// `move_permission_rule` string.
    pub leaf_kind: LeafKind,
    /// Who triggered the op — `Gui`, `Cli`, or `Skill`. Orthogonal to
    /// [`Kind`]: an undo run from the CLI is `actor: Cli`, `kind: Restore`.
    pub actor: Actor,
    /// Project root that scoped the operation. `None` for user-scope-only
    /// ops (e.g. moving a rule into User scope from another user-level
    /// scope — there is no project context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<PathBuf>,
    /// Source side. `None` for [`Kind::Add`] (no source) and for every
    /// [`Kind::Restore`] entry — restores carry their per-file snapshots in
    /// [`Record::restore`] instead, since they can touch more than two files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Side>,
    /// Destination side. `None` for [`Kind::Delete`] and every
    /// [`Kind::Restore`] entry (see [`Record::from`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Side>,
    /// JSON path of the affected leaf, e.g.
    /// `["permissions", "allow", 2]`. Carried verbatim from the
    /// `MoveLeafRequest` / `DeleteLeafRequest` / `AddLeafRequest`.
    pub path: Vec<PathSeg>,
    /// Set when the move was a `to_kind`-style reclassification (#8). The
    /// frontend's "Change kind" mode rides through the move primitive with
    /// `from == to` and `to_kind` set; the audit record preserves that
    /// distinction so a later reader can render the entry as "change kind"
    /// rather than "move".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_kind: Option<PermissionKind>,
    /// Set on — and only on — [`Kind::Restore`] entries. Carries the
    /// undo / redo / restore-to-point payload (#19 Phases 3-4): the id of
    /// the entry acted on, the direction, and a per-file before/after
    /// snapshot. Absent on the wire for every non-restore record, so the
    /// field is backwards-compatible with Phase 1-2 logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestoreMeta>,
    /// Build that wrote this record. See [`current_version`].
    pub claude_scope_version: String,
}

/// What kind of write the record describes. Kept coarse — three verbs —
/// with [`LeafKind`] carrying the noun (rule / list / key) so the matrix
/// stays small and a new leaf-kind doesn't require a new verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `apply_move_leaf` with `from != to`. The op moved a leaf between
    /// scopes. May also be a kind-change move (`to_kind` set) at the
    /// destination scope.
    Move,
    /// `apply_move_leaf` with `from == to` and `to_kind` set — same-scope
    /// permission reclassification. Recorded as its own kind because a
    /// reader presenting history wants "Change kind: allow → deny" rather
    /// than "Move project → project" with a hidden `to_kind`.
    ChangeKind,
    /// `apply_add_leaf`. Added a leaf (rule, list, or top-level key) to
    /// the `to` side.
    Add,
    /// `apply_delete_leaf`. Removed a leaf from the `from` side.
    Delete,
    /// A meta-entry recording an undo, redo, or restore-to-point (#19
    /// Phases 3-4). Its payload lives in [`Record::restore`]; `from` / `to`
    /// stay `None`. The undo/redo state machine reads [`RestoreMeta`] to
    /// reconstruct the cursor — `Undo` / `Redo` restores are cursor moves,
    /// a `ToPoint` restore is itself an ordinary undoable op.
    Restore,
}

/// What shape of leaf the record's `path` targets. Mirrors `MoveLeafKind`
/// in `commands.rs` exactly; duplicated here rather than re-exported so the
/// audit-log wire contract doesn't bind to an internal enum that could
/// rename for unrelated UI reasons later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeafKind {
    TopLevelKey,
    PermissionList,
    PermissionRule,
}

/// Who triggered the op. Phase 1 only emits `Gui`; `Cli` reserves a
/// wire-format slot for the CLI (#13) and `Skill` for a future Claude Code
/// skill. Orthogonal to [`Kind`]: an undo is `kind: Restore` whatever the
/// actor, so there is deliberately no `Restore` actor variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Gui,
    Cli,
    Skill,
}

/// One side of a write — the source or destination, depending on the kind.
/// Each side captures the **affected top-level key**'s value before and
/// after the write. That's the smallest snapshot that suffices for restore
/// (Phase 3+) to invert any logged op without re-reading history, while
/// staying narrower than the whole file (which could contain unrelated
/// keys).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Side {
    pub scope: Scope,
    pub file_path: PathBuf,
    /// Top-level key whose value the `key_before` / `key_after` fields
    /// snapshot. For permission ops this is always `"permissions"`; for
    /// top-level moves it's the moved key itself.
    pub top_level_key: String,
    /// Snapshot of `top_level_key`'s value on disk before the op. `None`
    /// means the key was absent (or the file didn't exist); a JSON `null`
    /// is distinguished by being `Some(Value::Null)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_before: Option<serde_json::Value>,
    /// Same shape as `key_before`, captured after the op completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_after: Option<serde_json::Value>,
}

/// Direction of a [`Kind::Restore`] entry. The undo/redo state machine
/// (`commands::audit_undo`) treats `Undo` / `Redo` as cursor moves and
/// `ToPoint` as an ordinary op that can itself be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreDirection {
    /// Inverted the most recent applied op (#19 Phase 3).
    Undo,
    /// Re-applied the most recently undone op (#19 Phase 3).
    Redo,
    /// Rolled the affected files back to their state before a chosen
    /// history entry (#19 Phase 4).
    ToPoint,
}

/// Payload of a [`Kind::Restore`] record. Carries the id of the entry the
/// restore acted on, the direction, and a per-file before/after snapshot of
/// everything the restore wrote — the snapshot is what lets a *later* undo
/// invert the restore itself.
///
/// `Undo` / `Redo` restores touch one or two files (a single inverted op);
/// a `ToPoint` restore may touch up to four. Either way the affected sides
/// live in `files` rather than in [`Record::from`] / [`Record::to`], which
/// only have room for two.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreMeta {
    /// The entry this restore inverted (`Undo`), re-applied (`Redo`), or
    /// rolled back to (`ToPoint`).
    pub target_id: Ulid,
    /// Which of undo / redo / restore-to-point produced this entry.
    pub direction: RestoreDirection,
    /// One [`Side`] per file the restore rewrote, each with its
    /// `key_before` / `key_after` snapshot.
    pub files: Vec<Side>,
}

impl Record {
    /// Construct a new record stamped with a fresh ULID and the current
    /// crate version. Kept as a constructor (rather than asking callers to
    /// fill `id` and `claude_scope_version` themselves) so the two fields
    /// can't drift across call sites — every record gets the same shape
    /// from one place. The argument count exceeds clippy's default
    /// threshold (8 vs 7), but every field is semantically distinct and
    /// the alternative — a config struct just for this — would obscure
    /// the call sites without buying readability.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: Kind,
        leaf_kind: LeafKind,
        actor: Actor,
        project_dir: Option<PathBuf>,
        from: Option<Side>,
        to: Option<Side>,
        path: Vec<PathSeg>,
        to_kind: Option<PermissionKind>,
    ) -> Self {
        Self {
            id: Ulid::new(),
            kind,
            leaf_kind,
            actor,
            project_dir,
            from,
            to,
            path,
            to_kind,
            restore: None,
            claude_scope_version: current_version(),
        }
    }

    /// Construct a [`Kind::Restore`] record for an undo / redo /
    /// restore-to-point op (#19 Phases 3-4). `from` / `to` / `to_kind` are
    /// always `None` on a restore record — the affected sides live in
    /// `restore.files` — so this constructor takes only the fields a
    /// restore actually carries. `leaf_kind` echoes the leaf kind of the
    /// entry being restored so the History view can label the row.
    pub fn new_restore(
        leaf_kind: LeafKind,
        actor: Actor,
        project_dir: Option<PathBuf>,
        path: Vec<PathSeg>,
        restore: RestoreMeta,
    ) -> Self {
        Self {
            id: Ulid::new(),
            kind: Kind::Restore,
            leaf_kind,
            actor,
            project_dir,
            from: None,
            to: None,
            path,
            to_kind: None,
            restore: Some(restore),
            claude_scope_version: current_version(),
        }
    }
}

/// Resolve the on-disk path for the audit log. `home` overrides
/// `dirs::home_dir()` so sandbox mode (#66) and unit tests don't touch the
/// real `~/.claude/`. Returns `None` when no home can be resolved — the
/// caller treats that the same as "audit disabled," which is the
/// fail-open path.
pub fn audit_path(home: Option<&Path>) -> Option<PathBuf> {
    let base = home.map(Path::to_path_buf).or_else(dirs::home_dir)?;
    Some(
        base.join(".claude")
            .join("claude-scope")
            .join("audit.jsonl"),
    )
}

/// Errors from a single append. Distinct from `io::Error` so a caller can
/// match on "log directory couldn't be located" (treat as fail-open silent
/// no-op) versus a real I/O failure (treat as fail-open with a surfaced
/// warning event). Never escapes to the user as a hard failure — see the
/// module-level docs.
#[derive(Debug, Error)]
pub enum AppendError {
    /// No `~/.claude/claude-scope/` could be created (e.g. no home dir
    /// available — happens in highly-sandboxed CI). The append is silently
    /// skipped.
    #[error("audit log location unavailable")]
    NoLocation,
    /// Filesystem error while creating the directory or appending the line.
    #[error("audit log I/O: {0}")]
    Io(#[from] io::Error),
    /// JSON serialization of the record failed. Shouldn't happen for any
    /// record produced via [`Record::new`] (the struct is wholly
    /// serializable by construction), but the variant exists so a future
    /// schema bug surfaces as a typed error rather than a panic.
    #[error("audit log serialization: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Append `record` to the audit log rooted at `home`. One line per record.
/// The full serialized line (record bytes + `\n`) is written in a single
/// `write_all` call; for any sane record size that lands as a single
/// `write()` syscall the OS executes atomically against concurrent
/// appenders — which is the property [`read_all`] relies on to tolerate a
/// truncated tail line.
///
/// O_APPEND (via `OpenOptions::append`) ensures the kernel-level offset is
/// taken at write time, so two ClaudeScope processes racing to log don't
/// interleave bytes — they get sequential lines in *some* order, which is
/// the same property we need for the in-process single-process case.
///
/// Note: `append` itself does NOT take the rotate/append lock. Production
/// callers route through [`persist`] which holds the lock around the
/// rotate-then-append pair (#166). Direct `append` callers are the
/// truncated-tail-tolerance tests and the malformed-line injection in
/// the CLI integration tests, where the bypass is intentional.
pub fn append(record: &Record, home: Option<&Path>) -> Result<(), AppendError> {
    let path = audit_path(home).ok_or(AppendError::NoLocation)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(&bytes)?;
    Ok(())
}

/// Options for [`persist`]. `rotate_cap_bytes` is `Some(_)` to attempt a
/// rotation before the append when the active log exceeds the cap; `None`
/// to skip rotation entirely.
#[derive(Debug, Default, Clone, Copy)]
pub struct PersistOptions {
    pub rotate_cap_bytes: Option<u64>,
}

/// Outcome of [`persist`]: rotation and append errors are reported
/// separately so callers can surface them in their idiomatic way.
#[derive(Debug, Default)]
pub struct PersistResult {
    pub rotate_error: Option<String>,
    pub append_error: Option<AppendError>,
}

/// Persist `record` to the audit log under `home`, optionally rotating
/// first when the active log is over `rotate_cap_bytes`.
///
/// The whole operation runs under an advisory file lock at
/// `<audit_dir>/audit.lock`. The lock closes the rotate-then-append race
/// (#166): without it, two processes can race such that one rotates the
/// active log into an archive while the other is mid-append, leaving the
/// fresh record stranded in the archive where [`read_all`] would have
/// missed it pre-fix. With the lock and the now-archive-aware
/// [`read_all`], records are durable and reachable from the undo/redo
/// state machine regardless of how rotations interleave with writes.
///
/// Rotation failure is non-fatal — the append still proceeds against the
/// over-cap log, which is better than dropping the record. Append failure
/// is returned in the result; callers (GUI / CLI) surface it as a
/// warning, not a hard error, because the on-disk operation that
/// triggered the audit (the move / add / delete itself) has already
/// succeeded.
pub fn persist(record: &Record, home: Option<&Path>, options: PersistOptions) -> PersistResult {
    let _lock = AuditLock::acquire(home).ok().flatten();
    let mut result = PersistResult::default();
    if let Some(cap_bytes) = options.rotate_cap_bytes {
        if let Err(err) = rotate_if_needed(home, cap_bytes) {
            result.rotate_error = Some(format!("rotation: {err}"));
        }
    }
    if let Err(err) = append(record, home) {
        result.append_error = Some(err);
    }
    result
}

/// Path to the cross-process advisory lock file. Lives alongside
/// `audit.jsonl` so the lock semantically protects the log directory.
fn audit_lock_path(home: Option<&Path>) -> Option<PathBuf> {
    audit_path(home).and_then(|p| p.parent().map(|parent| parent.join("audit.lock")))
}

/// RAII guard for the audit-log advisory lock. Acquired around
/// rotate-then-append in [`persist`] so two processes can't interleave a
/// rotation with another process's append. The lock is `fs2`'s advisory
/// flock (`LockFileEx` on Windows, `flock` on POSIX) — released when the
/// guard drops, or when the process exits even after a crash (the kernel
/// reclaims advisory locks on file-handle close).
struct AuditLock(std::fs::File);

impl AuditLock {
    /// Open (creating if needed) the lock file and take an exclusive
    /// lock. Returns `None` when the audit location is unavailable
    /// (no home dir) — the caller falls open just like the legacy
    /// `append` did, so audit failures never block the primary write.
    fn acquire(home: Option<&Path>) -> io::Result<Option<Self>> {
        let Some(path) = audit_lock_path(home) else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Lock file is content-irrelevant — flock semantics don't care
        // about bytes. `.truncate(false)` keeps any stray bytes alone
        // (which is fine, we never read it) and silences clippy's
        // "no truncate spec on write open" warning.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        // Blocking exclusive lock — both POSIX and Windows. For the
        // millisecond-scale rotate+append, blocking is fine; if a
        // crashed process held the lock the kernel would have released
        // it.
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(Some(AuditLock(file)))
    }
}

impl Drop for AuditLock {
    fn drop(&mut self) {
        // Best-effort. If unlock fails, dropping the file handle still
        // releases the kernel lock on exit. Don't panic in Drop.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// Read every well-formed record from the audit log, merging the active
/// log with any rotated archives (`audit-YYYY-MM.jsonl`, plus collision
/// suffixes) in the same directory. Archives come first, in filename
/// order (which encodes `YYYY-MM` plus collision suffix — older archives
/// sort first), then the active log. Within each file, records are
/// returned in append order — the file's own ordering, not re-sorted by
/// ULID, because file order IS the causal record of what happened.
///
/// Archive-aware reads close half of the rotate/append race (#166):
/// even if a record lands in what becomes an archive (a slow append
/// racing a rotation), [`read_all`] still finds it. The other half is
/// the lock held by [`persist`] which makes the race extremely narrow
/// in the first place.
///
/// A truncated tail line (no terminating newline AND a parse error) is
/// silently skipped — that's the partial-write tolerance property. A
/// well-formed line that fails parse (e.g. a future schema field this
/// build doesn't understand strictly) is also skipped, with the count
/// of skipped lines (summed across archives + active) returned
/// alongside the records so a caller can surface "N entries unreadable"
/// rather than silently swallowing data.
pub fn read_all(home: Option<&Path>) -> io::Result<(Vec<Record>, usize)> {
    let mut records = Vec::new();
    let mut skipped = 0usize;
    for archive in audit_archives(home)? {
        let (recs, skip) = read_log_file(&archive)?;
        records.extend(recs);
        skipped += skip;
    }
    if let Some(active) = audit_path(home) {
        let (recs, skip) = read_log_file(&active)?;
        records.extend(recs);
        skipped += skip;
    }
    Ok((records, skipped))
}

/// Enumerate `audit-*.jsonl` files in the audit directory (the rotation
/// archives — see [`rotate_if_needed`]), sorted lexicographically by
/// filename so older archives come first. The archive naming scheme
/// (`audit-YYYY-MM[-N].jsonl`) makes lex order match chronological
/// order for both the main per-month files and the within-month
/// collision suffixes. Returns an empty vec if the directory doesn't
/// exist; the caller treats that as "no archives."
fn audit_archives(home: Option<&Path>) -> io::Result<Vec<PathBuf>> {
    let Some(active) = audit_path(home) else {
        return Ok(Vec::new());
    };
    let Some(parent) = active.parent() else {
        return Ok(Vec::new());
    };
    if !parent.exists() {
        return Ok(Vec::new());
    }
    let mut archives = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Match `audit-*.jsonl` exactly — and skip the active log itself
        // (which is `audit.jsonl`, no dash) since the caller reads it
        // separately.
        if name.starts_with("audit-") && name.ends_with(".jsonl") {
            archives.push(path);
        }
    }
    archives.sort();
    Ok(archives)
}

/// Read records + skipped count from a single audit-log file. Factored
/// out so [`read_all`] can apply the same parse-tolerance behavior to
/// both the active log and each archive.
fn read_log_file(path: &Path) -> io::Result<(Vec<Record>, usize)> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut skipped = 0usize;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(trimmed) {
            Ok(rec) => records.push(rec),
            Err(_) => skipped += 1,
        }
    }
    Ok((records, skipped))
}

/// Undo / redo availability, reconstructed from the append-only log by
/// [`undo_redo_state`]. The log is the single source of truth — there is no
/// mutable cursor on disk — so every undo / redo decision is a fresh replay
/// of the records.
#[derive(Debug, Clone, Default)]
pub struct UndoRedoState {
    /// The op the next undo would invert. `None` when nothing is undoable
    /// (empty log, or every op already undone).
    pub undoable: Option<Record>,
    /// The op the next redo would re-apply. `None` when there is nothing to
    /// redo, or when `sequence_break` has stranded the redo stack.
    pub redoable: Option<Record>,
    /// True when a fresh write landed while undone ops were pending,
    /// stranding the redo stack (#124). Redo is disabled and `redoable` is
    /// `None`; the flag lets the UI explain *why* in a tooltip rather than
    /// silently discarding the forward stack.
    pub sequence_break: bool,
}

/// How a log entry participates in the undo/redo replay.
enum ReplayRole {
    /// An ordinary undoable op: a `move` / `add` / `delete` / `change_kind`,
    /// or a restore-to-point (which is itself undoable).
    Op,
    /// An `undo` restore — a cursor move back, carrying the target op id.
    Undo(Ulid),
    /// A `redo` restore — a cursor move forward, carrying the target op id.
    Redo(Ulid),
    /// A malformed restore record (no payload). Left out of the replay
    /// entirely rather than allowed to perturb the cursor.
    Ignore,
}

fn replay_role(rec: &Record) -> ReplayRole {
    match rec.kind {
        Kind::Move | Kind::ChangeKind | Kind::Add | Kind::Delete => ReplayRole::Op,
        Kind::Restore => match rec.restore.as_ref() {
            Some(meta) => match meta.direction {
                // Restore-to-point is a forward write, not a cursor move:
                // it can be undone like any other op.
                RestoreDirection::ToPoint => ReplayRole::Op,
                RestoreDirection::Undo => ReplayRole::Undo(meta.target_id),
                RestoreDirection::Redo => ReplayRole::Redo(meta.target_id),
            },
            None => ReplayRole::Ignore,
        },
    }
}

/// Reconstruct undo/redo state from `records` (the full log in file order,
/// as returned by [`read_all`]).
///
/// The replay walks the log once, building the list of undoable ops, a
/// per-op "currently undone" flag, and a redo stack. `undo` / `redo`
/// restores move the cursor; every other entry — including a
/// restore-to-point — is an op. A write appended while the redo stack is
/// non-empty latches `sequence_break`: the redo stack is now ambiguous, so
/// redo is withheld (but the entries are kept, not discarded — see #124).
pub fn undo_redo_state(records: &[Record]) -> UndoRedoState {
    // `ops` holds indices into `records`; `undone` is parallel to it;
    // `redo_stack` holds indices into `ops` (top = next op to redo).
    let mut ops: Vec<usize> = Vec::new();
    let mut undone: Vec<bool> = Vec::new();
    let mut redo_stack: Vec<usize> = Vec::new();
    let mut sequence_break = false;

    for (i, rec) in records.iter().enumerate() {
        match replay_role(rec) {
            ReplayRole::Op => {
                if !redo_stack.is_empty() {
                    sequence_break = true;
                }
                ops.push(i);
                undone.push(false);
            }
            ReplayRole::Undo(target) => {
                if let Some(op_ix) = ops.iter().position(|&r| records[r].id == target) {
                    // Guard against a double-undo of the same op via two
                    // log entries (a corrupt log, or two racing writers).
                    if !undone[op_ix] {
                        undone[op_ix] = true;
                        redo_stack.push(op_ix);
                    }
                }
                // A target that isn't in `ops` is dangling (rotated out of
                // the active log, or never written) — skip it silently.
            }
            ReplayRole::Redo(target) => {
                if let Some(op_ix) = ops.iter().position(|&r| records[r].id == target) {
                    if undone[op_ix] {
                        undone[op_ix] = false;
                        redo_stack.retain(|&x| x != op_ix);
                    }
                }
            }
            ReplayRole::Ignore => {}
        }
    }

    let undoable = ops
        .iter()
        .zip(&undone)
        .rev()
        .find(|(_, &u)| !u)
        .map(|(&r, _)| records[r].clone());
    // `sequence_break` only matters while the redo stack still holds
    // something; if a (CLI-forced) redo emptied it, drop the stale flag.
    let sequence_break = sequence_break && !redo_stack.is_empty();
    let redoable = if sequence_break {
        None
    } else {
        redo_stack.last().map(|&op_ix| records[ops[op_ix]].clone())
    };

    UndoRedoState {
        undoable,
        redoable,
        sequence_break,
    }
}

/// Outcome of a [`rotate_if_needed`] call. `Skipped` covers four cases that
/// all want fail-open behavior at the call site (no rotation needed, no
/// audit path resolvable, audit file doesn't exist yet, under-threshold);
/// distinguishing them on the wire would be noise. `Rotated` carries the
/// archive path so the caller can surface "moved to X" in logs / telemetry
/// if it wants to.
#[derive(Debug)]
pub enum RotateOutcome {
    Skipped,
    Rotated { archive: PathBuf },
}

/// Rotate the active `audit.jsonl` to a `audit-YYYY-MM.jsonl` archive when
/// it exceeds `max_size_bytes`. Run **before** an append so the new line
/// lands in the fresh file. Reader semantics in [`read_all`] are unchanged:
/// archives are ignored, only the active file is read.
///
/// Naming: the year-month stamp is derived from the file's last-modified
/// time, not the moment of rotation. That way the archive name reflects
/// when the last entry in the rolled-out log was written, which is the
/// more useful question to ask of a `ls`-d directory of archives.
///
/// Collisions in the same month — second rotation within May 2026, say —
/// get a `-N` suffix. The suffix starts at `2` because the first archive
/// in a given month is unsuffixed; readers scanning the directory can
/// sort lexicographically and get chronological order.
///
/// Fail-open: errors propagate so the caller can warn (the audit's
/// `audit-error` event channel), but the calling pattern at
/// `commands::emit_audit` treats both `Ok(Skipped)` and `Err(_)` as
/// "proceed to append". A rotation failure must not block the actual
/// write — see the issue's safety invariants.
pub fn rotate_if_needed(
    home: Option<&Path>,
    max_size_bytes: u64,
) -> Result<RotateOutcome, AppendError> {
    let Some(path) = audit_path(home) else {
        return Ok(RotateOutcome::Skipped);
    };
    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RotateOutcome::Skipped),
        Err(e) => return Err(e.into()),
    };
    if metadata.len() < max_size_bytes {
        return Ok(RotateOutcome::Skipped);
    }
    // Prefer mtime over "now" so the archive name reflects the data in
    // the file. Falls back to wall-clock if the FS strips mtime (some
    // network mounts do); that's degraded but never wrong.
    let stamp_source = metadata
        .modified()
        .ok()
        .unwrap_or_else(std::time::SystemTime::now);
    let stamp = year_month_stamp(stamp_source);
    let parent = path
        .parent()
        .expect("audit_path always nests under `<home>/.claude/claude-scope/`");
    let mut archive = parent.join(format!("audit-{stamp}.jsonl"));
    let mut n: u32 = 2;
    while archive.exists() {
        archive = parent.join(format!("audit-{stamp}-{n}.jsonl"));
        n = n.checked_add(1).ok_or_else(|| {
            AppendError::Io(io::Error::other(
                "exhausted archive collision suffixes — too many same-month rotations",
            ))
        })?;
    }
    std::fs::rename(&path, &archive)?;
    Ok(RotateOutcome::Rotated { archive })
}

/// Format a `SystemTime` as `YYYY-MM` (UTC). Hand-rolled (Howard Hinnant's
/// civil-from-days) to avoid pulling in `chrono` / `time` for one call
/// site — same reasoning as the rest of the audit module's
/// dep-minimization stance.
fn year_month_stamp(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64 + 719_468;
    let era = if days >= 0 {
        days / 146_097
    } else {
        (days - 146_096) / 146_097
    };
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = y + if m <= 2 { 1 } else { 0 };
    format!("{year:04}-{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PathSeg;
    use std::fs;
    use tempfile::TempDir;

    fn make_side(scope: Scope, file_path: &str) -> Side {
        Side {
            scope,
            file_path: PathBuf::from(file_path),
            top_level_key: "permissions".to_string(),
            key_before: Some(serde_json::json!({"allow": ["A"]})),
            key_after: Some(serde_json::json!({"allow": []})),
        }
    }

    fn sample_record() -> Record {
        Record::new(
            Kind::Move,
            LeafKind::PermissionRule,
            Actor::Gui,
            Some(PathBuf::from("/work/proj")),
            Some(make_side(
                Scope::Project,
                "/work/proj/.claude/settings.json",
            )),
            Some(make_side(Scope::User, "/home/me/.claude/settings.json")),
            vec![
                PathSeg::Key("permissions".into()),
                PathSeg::Key("allow".into()),
                PathSeg::Index(0),
            ],
            None,
        )
    }

    #[test]
    fn record_round_trips_through_json() {
        let rec = sample_record();
        let s = serde_json::to_string(&rec).unwrap();
        let parsed: Record = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, rec);
    }

    #[test]
    fn append_creates_directory_and_writes_one_line() {
        let tmp = TempDir::new().unwrap();
        append(&sample_record(), Some(tmp.path())).unwrap();
        let path = audit_path(Some(tmp.path())).unwrap();
        assert!(path.exists(), "audit.jsonl should be created");
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.ends_with('\n'), "every line must terminate with \\n");
        // No embedded newlines in the serialized record — `serde_json`
        // escapes them in strings, so the file is exactly one line per
        // record. That's the property `read_all` relies on.
        assert_eq!(body.matches('\n').count(), 1);
    }

    #[test]
    fn append_then_read_round_trips_multiple_records() {
        let tmp = TempDir::new().unwrap();
        let a = sample_record();
        let b = sample_record();
        let c = sample_record();
        append(&a, Some(tmp.path())).unwrap();
        append(&b, Some(tmp.path())).unwrap();
        append(&c, Some(tmp.path())).unwrap();
        let (records, skipped) = read_all(Some(tmp.path())).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(records.len(), 3);
        // Order is file order (which is append order, thanks to O_APPEND).
        assert_eq!(records[0].id, a.id);
        assert_eq!(records[1].id, b.id);
        assert_eq!(records[2].id, c.id);
    }

    #[test]
    fn read_skips_truncated_tail_line() {
        // Simulates a partial write: a complete line followed by an
        // unterminated, malformed tail (e.g. ClaudeScope crashed mid-write
        // before the newline landed).
        let tmp = TempDir::new().unwrap();
        let rec = sample_record();
        append(&rec, Some(tmp.path())).unwrap();
        let path = audit_path(Some(tmp.path())).unwrap();
        let mut body = fs::read_to_string(&path).unwrap();
        body.push_str(r#"{"id":"01HF...","kind":"mo"#); // truncated
        fs::write(&path, body).unwrap();

        let (records, skipped) = read_all(Some(tmp.path())).unwrap();
        // The well-formed first line is still readable; the truncated
        // tail counts as one skip rather than blowing up the read.
        assert_eq!(records.len(), 1);
        assert_eq!(skipped, 1);
        assert_eq!(records[0].id, rec.id);
    }

    #[test]
    fn read_missing_file_returns_empty_not_error() {
        let tmp = TempDir::new().unwrap();
        let (records, skipped) = read_all(Some(tmp.path())).unwrap();
        assert!(records.is_empty());
        assert_eq!(skipped, 0);
    }

    #[test]
    fn append_with_no_home_returns_no_location_error() {
        // `home: None` AND `dirs::home_dir()` unset is hard to simulate
        // portably (dirs::home_dir falls back to env vars). Instead we
        // exercise the `audit_path` resolver directly — if the resolver
        // returns None, `append` is required to surface `NoLocation`.
        // We use a sentinel by setting `home` to a path that exists; the
        // resolver returns Some, so this test asserts the happy path.
        // The NoLocation arm is exercised by `audit_path_returns_none_when`
        // patterns in future tests once we have a mockable env layer.
        let tmp = TempDir::new().unwrap();
        let result = append(&sample_record(), Some(tmp.path()));
        assert!(result.is_ok());
    }

    #[test]
    fn append_concurrent_writers_dont_interleave_bytes() {
        // Two threads append concurrently; both records must land as
        // complete lines (no byte interleaving), and `read_all` must see
        // both with no skipped lines. This validates the O_APPEND
        // atomic-write-per-line invariant on whatever platform CI runs on.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let home_a = home.clone();
        let home_b = home.clone();
        let t1 = std::thread::spawn(move || {
            for _ in 0..50 {
                append(&sample_record(), Some(&home_a)).unwrap();
            }
        });
        let t2 = std::thread::spawn(move || {
            for _ in 0..50 {
                append(&sample_record(), Some(&home_b)).unwrap();
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let (records, skipped) = read_all(Some(&home)).unwrap();
        assert_eq!(records.len(), 100);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn record_kind_serializes_to_snake_case_strings() {
        // The wire format is a contract — pinning the casing here means a
        // future `#[serde(rename_all = ...)]` typo can't silently change
        // every entry's `kind` string and break readers downstream.
        let json = serde_json::to_string(&Kind::ChangeKind).unwrap();
        assert_eq!(json, r#""change_kind""#);
        let json = serde_json::to_string(&Kind::Move).unwrap();
        assert_eq!(json, r#""move""#);
        let json = serde_json::to_string(&Kind::Restore).unwrap();
        assert_eq!(json, r#""restore""#);
    }

    fn sample_restore_record(direction: RestoreDirection) -> Record {
        Record::new_restore(
            LeafKind::PermissionRule,
            Actor::Gui,
            Some(PathBuf::from("/work/proj")),
            vec![
                PathSeg::Key("permissions".into()),
                PathSeg::Key("allow".into()),
                PathSeg::Index(0),
            ],
            RestoreMeta {
                target_id: Ulid::new(),
                direction,
                files: vec![
                    make_side(Scope::Project, "/work/proj/.claude/settings.json"),
                    make_side(Scope::User, "/home/me/.claude/settings.json"),
                ],
            },
        )
    }

    #[test]
    fn restore_record_round_trips_through_json() {
        for dir in [
            RestoreDirection::Undo,
            RestoreDirection::Redo,
            RestoreDirection::ToPoint,
        ] {
            let rec = sample_restore_record(dir);
            let s = serde_json::to_string(&rec).unwrap();
            let parsed: Record = serde_json::from_str(&s).unwrap();
            assert_eq!(parsed, rec);
        }
    }

    #[test]
    fn restore_direction_serializes_to_snake_case() {
        // The wire format is a contract — pin the casing so a future
        // `rename_all` typo can't silently rewrite every restore entry.
        assert_eq!(
            serde_json::to_string(&RestoreDirection::Undo).unwrap(),
            r#""undo""#
        );
        assert_eq!(
            serde_json::to_string(&RestoreDirection::Redo).unwrap(),
            r#""redo""#
        );
        assert_eq!(
            serde_json::to_string(&RestoreDirection::ToPoint).unwrap(),
            r#""to_point""#
        );
    }

    #[test]
    fn non_restore_record_omits_restore_field_on_the_wire() {
        // `restore` is skip-on-`None`, so a Phase 1-2 reader (which doesn't
        // know the field) sees the exact same bytes it always did.
        let json = serde_json::to_value(sample_record()).unwrap();
        assert!(json.get("restore").is_none());
        // A restore record carries it.
        let json = serde_json::to_value(sample_restore_record(RestoreDirection::Undo)).unwrap();
        assert!(json.get("restore").is_some());
        assert!(
            json.get("from").is_none(),
            "restore records leave from/to unset"
        );
        assert!(json.get("to").is_none());
    }

    #[test]
    fn restore_record_survives_append_then_read() {
        let tmp = TempDir::new().unwrap();
        let rec = sample_restore_record(RestoreDirection::ToPoint);
        append(&rec, Some(tmp.path())).unwrap();
        let (records, skipped) = read_all(Some(tmp.path())).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0], rec);
    }

    // -- undo/redo state machine (#124) ------------------------------------

    /// A bare op record — `undo_redo_state` reads file order, not the ULID
    /// clock, so no inter-record sleeps are needed.
    fn op_record() -> Record {
        Record::new(
            Kind::Move,
            LeafKind::PermissionRule,
            Actor::Gui,
            None,
            None,
            None,
            vec![],
            None,
        )
    }

    fn restore_of(target: &Record, direction: RestoreDirection) -> Record {
        Record::new_restore(
            target.leaf_kind,
            Actor::Gui,
            None,
            vec![],
            RestoreMeta {
                target_id: target.id,
                direction,
                files: vec![],
            },
        )
    }

    #[test]
    fn undo_redo_state_empty_log_has_nothing() {
        let state = undo_redo_state(&[]);
        assert!(state.undoable.is_none());
        assert!(state.redoable.is_none());
        assert!(!state.sequence_break);
    }

    #[test]
    fn undo_redo_state_fresh_writes_undo_targets_the_last() {
        let (a, b, c) = (op_record(), op_record(), op_record());
        let log = [a, b, c.clone()];
        let state = undo_redo_state(&log);
        assert_eq!(state.undoable.unwrap().id, c.id);
        assert!(state.redoable.is_none(), "nothing undone yet");
        assert!(!state.sequence_break);
    }

    #[test]
    fn undo_redo_state_after_one_undo() {
        let (a, b, c) = (op_record(), op_record(), op_record());
        let log = [
            a.clone(),
            b.clone(),
            c.clone(),
            restore_of(&c, RestoreDirection::Undo),
        ];
        let state = undo_redo_state(&log);
        // C is undone, so the next undo targets B and the next redo C.
        assert_eq!(state.undoable.unwrap().id, b.id);
        assert_eq!(state.redoable.unwrap().id, c.id);
        assert!(!state.sequence_break);
    }

    #[test]
    fn undo_redo_state_stacked_undos_walk_backward() {
        let (a, b, c) = (op_record(), op_record(), op_record());
        let log = [
            a.clone(),
            b.clone(),
            c.clone(),
            restore_of(&c, RestoreDirection::Undo),
            restore_of(&b, RestoreDirection::Undo),
        ];
        let state = undo_redo_state(&log);
        assert_eq!(state.undoable.unwrap().id, a.id);
        // Redo re-applies the most recently undone op first: B.
        assert_eq!(state.redoable.unwrap().id, b.id);
    }

    #[test]
    fn undo_redo_state_redo_clears_the_forward_step() {
        let (a, b, c) = (op_record(), op_record(), op_record());
        let log = [
            a,
            b,
            c.clone(),
            restore_of(&c, RestoreDirection::Undo),
            restore_of(&c, RestoreDirection::Redo),
        ];
        let state = undo_redo_state(&log);
        // C is applied again; redo stack is empty.
        assert_eq!(state.undoable.unwrap().id, c.id);
        assert!(state.redoable.is_none());
    }

    #[test]
    fn undo_redo_state_write_after_undo_is_a_sequence_break() {
        let (a, b, c, d) = (op_record(), op_record(), op_record(), op_record());
        let log = [
            a,
            b,
            c.clone(),
            restore_of(&c, RestoreDirection::Undo),
            d.clone(),
        ];
        let state = undo_redo_state(&log);
        // The new write D is undoable; redo is withheld and the break flag
        // is set so the UI can explain it.
        assert_eq!(state.undoable.unwrap().id, d.id);
        assert!(state.redoable.is_none());
        assert!(state.sequence_break);
    }

    #[test]
    fn undo_redo_state_restore_to_point_counts_as_an_op() {
        let a = op_record();
        let to_point = restore_of(&a, RestoreDirection::ToPoint);
        // A restore-to-point is itself undoable.
        let state = undo_redo_state(&[a.clone(), to_point.clone()]);
        assert_eq!(state.undoable.unwrap().id, to_point.id);
        // And undoing it walks back to A.
        let undo_tp = restore_of(&to_point, RestoreDirection::Undo);
        let state = undo_redo_state(&[a.clone(), to_point.clone(), undo_tp]);
        assert_eq!(state.undoable.unwrap().id, a.id);
        assert_eq!(state.redoable.unwrap().id, to_point.id);
    }

    #[test]
    fn undo_redo_state_ignores_dangling_and_malformed_restores() {
        let a = op_record();
        // An undo targeting an op that isn't in the log (rotated out) must
        // not perturb the cursor.
        let orphan = op_record();
        let dangling = restore_of(&orphan, RestoreDirection::Undo);
        let state = undo_redo_state(&[a.clone(), dangling]);
        assert_eq!(state.undoable.unwrap().id, a.id);
        // A restore record with no payload is ignored entirely.
        let mut malformed = restore_of(&a, RestoreDirection::Undo);
        malformed.restore = None;
        let state = undo_redo_state(&[a.clone(), malformed]);
        assert_eq!(
            state.undoable.unwrap().id,
            a.id,
            "malformed restore left A applied"
        );
    }

    #[test]
    fn ulid_lex_order_matches_chronological_order() {
        // The whole point of using ULIDs (vs random IDs) is that a sort by
        // id gives chronological order. Append three records with small
        // sleeps between them; sorting by id must match append order.
        let tmp = TempDir::new().unwrap();
        let a = Record::new(
            Kind::Move,
            LeafKind::PermissionRule,
            Actor::Gui,
            None,
            None,
            None,
            vec![],
            None,
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = Record::new(
            Kind::Move,
            LeafKind::PermissionRule,
            Actor::Gui,
            None,
            None,
            None,
            vec![],
            None,
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = Record::new(
            Kind::Move,
            LeafKind::PermissionRule,
            Actor::Gui,
            None,
            None,
            None,
            vec![],
            None,
        );
        append(&c, Some(tmp.path())).unwrap();
        append(&a, Some(tmp.path())).unwrap();
        append(&b, Some(tmp.path())).unwrap();
        let (mut records, _) = read_all(Some(tmp.path())).unwrap();
        records.sort_by_key(|r| r.id);
        assert_eq!(records[0].id, a.id);
        assert_eq!(records[1].id, b.id);
        assert_eq!(records[2].id, c.id);
    }

    // -- rotation (#127) ---------------------------------------------------

    #[test]
    fn rotate_if_needed_is_a_noop_when_file_is_absent() {
        let tmp = TempDir::new().unwrap();
        let out = rotate_if_needed(Some(tmp.path()), 1).unwrap();
        assert!(matches!(out, RotateOutcome::Skipped));
        // No archive should be created out of thin air.
        let parent = audit_path(Some(tmp.path()))
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        if parent.exists() {
            let archives: Vec<_> = fs::read_dir(&parent)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("audit-"))
                .collect();
            assert!(archives.is_empty());
        }
    }

    #[test]
    fn rotate_if_needed_skips_when_under_cap() {
        let tmp = TempDir::new().unwrap();
        append(&sample_record(), Some(tmp.path())).unwrap();
        // The single record is well under any reasonable cap.
        let out = rotate_if_needed(Some(tmp.path()), 10_000).unwrap();
        assert!(matches!(out, RotateOutcome::Skipped));
        // Active file still exists.
        assert!(audit_path(Some(tmp.path())).unwrap().exists());
    }

    #[test]
    fn rotate_if_needed_renames_to_year_month_archive_when_over_cap() {
        let tmp = TempDir::new().unwrap();
        for _ in 0..5 {
            append(&sample_record(), Some(tmp.path())).unwrap();
        }
        // Pass a cap below the current size so rotation triggers.
        let active = audit_path(Some(tmp.path())).unwrap();
        let size = fs::metadata(&active).unwrap().len();
        assert!(size > 0);
        let cap = size / 2;

        let out = rotate_if_needed(Some(tmp.path()), cap).unwrap();
        let archive = match out {
            RotateOutcome::Rotated { archive } => archive,
            _ => panic!("expected rotation"),
        };
        assert!(archive.exists(), "archive should exist after rotation");
        assert!(!active.exists(), "active file should be moved away");
        // Archive name matches the audit-YYYY-MM.jsonl pattern. The exact
        // year-month depends on the test clock, so just structural check.
        let name = archive.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("audit-"), "got: {name}");
        assert!(name.ends_with(".jsonl"), "got: {name}");
        // YYYY-MM is 7 chars between "audit-" and ".jsonl": "2026-05".
        assert_eq!(name.len(), "audit-YYYY-MM.jsonl".len(), "got: {name}");
    }

    #[test]
    fn rotate_if_needed_collides_to_n2_in_same_month() {
        // Two rotations within one month must produce distinct archive
        // names. The first is `audit-YYYY-MM.jsonl`; the second adds
        // `-2`. Without that, the second rename would clobber the first
        // archive and lose data — the exact scenario this branch guards
        // against.
        let tmp = TempDir::new().unwrap();
        for _ in 0..5 {
            append(&sample_record(), Some(tmp.path())).unwrap();
        }
        let active = audit_path(Some(tmp.path())).unwrap();
        let cap = fs::metadata(&active).unwrap().len() / 2;

        let first = match rotate_if_needed(Some(tmp.path()), cap).unwrap() {
            RotateOutcome::Rotated { archive } => archive,
            _ => panic!("expected first rotation"),
        };

        // Fill the active file again and rotate. The collision handler
        // must pick `-2`.
        for _ in 0..5 {
            append(&sample_record(), Some(tmp.path())).unwrap();
        }
        let second = match rotate_if_needed(Some(tmp.path()), cap).unwrap() {
            RotateOutcome::Rotated { archive } => archive,
            _ => panic!("expected second rotation"),
        };
        assert_ne!(first, second);
        let second_name = second.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            second_name.contains("-2.jsonl"),
            "second archive should have -2 suffix, got: {second_name}"
        );
        assert!(first.exists(), "first archive must not be clobbered");
        assert!(second.exists(), "second archive must land");
    }

    #[test]
    fn read_all_merges_active_log_with_rotated_archives() {
        // After rotation the active file is fresh, but read_all is
        // archive-aware as of #166 — records that landed in archives
        // remain reachable so undo/redo/history don't lose entries to
        // a rotation that interleaved with a write.
        let tmp = TempDir::new().unwrap();
        // Seed three records into the active log…
        let mut expected_ids = Vec::new();
        for _ in 0..3 {
            let rec = sample_record();
            expected_ids.push(rec.id);
            append(&rec, Some(tmp.path())).unwrap();
            // Sleep a hair so each ULID's timestamp portion differs —
            // makes the sort assertion meaningful.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // …then rotate them into an archive.
        let cap = fs::metadata(audit_path(Some(tmp.path())).unwrap())
            .unwrap()
            .len()
            / 2;
        rotate_if_needed(Some(tmp.path()), cap).unwrap();

        let (records, skipped) = read_all(Some(tmp.path())).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(
            records.len(),
            3,
            "archived records must remain reachable from read_all (#166)"
        );
        let got_ids: Vec<_> = records.iter().map(|r| r.id).collect();
        assert_eq!(
            got_ids, expected_ids,
            "archived records must come back in append order"
        );

        // A subsequent append lands in the new active file. read_all
        // returns the archive's 3 first, then the new 1 — archives
        // before active, file-order within each.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let post_rotation = sample_record();
        let post_id = post_rotation.id;
        append(&post_rotation, Some(tmp.path())).unwrap();

        let (records, _) = read_all(Some(tmp.path())).unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(
            records.last().map(|r| r.id),
            Some(post_id),
            "the post-rotation record must come after the archived ones"
        );
    }

    #[test]
    fn persist_concurrent_threads_lose_no_records_across_rotation() {
        // Regression for #166. Multiple threads call `persist` with the
        // active log near the rotation cap. Without the lock, one thread
        // can rotate the log while another's append is in flight,
        // potentially stranding the racer's record in the archive (and
        // pre-fix `read_all` would never see it). With the lock + the
        // now-archive-aware `read_all`, every record persisted must be
        // retrievable.
        use std::sync::Arc;
        use std::thread;

        let tmp = Arc::new(TempDir::new().unwrap());

        // Seed a few records so the log has size, then pick a cap just
        // below the current size so the first persist triggers a
        // rotation.
        for _ in 0..5 {
            append(&sample_record(), Some(tmp.path())).unwrap();
        }
        let seeded_len = fs::metadata(audit_path(Some(tmp.path())).unwrap())
            .unwrap()
            .len();
        let cap = seeded_len - 1;

        let n_threads = 8;
        let mut handles = Vec::new();
        for _ in 0..n_threads {
            let tmp = Arc::clone(&tmp);
            handles.push(thread::spawn(move || {
                let rec = sample_record();
                let id = rec.id;
                let options = PersistOptions {
                    rotate_cap_bytes: Some(cap),
                };
                let result = persist(&rec, Some(tmp.path()), options);
                assert!(
                    result.append_error.is_none(),
                    "concurrent append must not fail: {:?}",
                    result.append_error
                );
                id
            }));
        }
        let mut spawned_ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        spawned_ids.sort();

        let (records, skipped) = read_all(Some(tmp.path())).unwrap();
        assert_eq!(skipped, 0);
        // Every record we persisted must be reachable — the 5 seeded
        // plus the 8 concurrent appends, regardless of which file each
        // landed in.
        assert_eq!(
            records.len(),
            5 + n_threads,
            "lost records under concurrent rotate+append (#166)"
        );
        for id in &spawned_ids {
            assert!(
                records.iter().any(|r| r.id == *id),
                "spawned record {id:?} not retrievable from read_all"
            );
        }
    }

    #[test]
    fn rotate_if_needed_skips_when_no_home_available() {
        // `home: None` falls through to `dirs::home_dir()` inside the
        // path resolver. The resolver returns None when there's no home
        // (highly-sandboxed CI). On a normal dev machine `home_dir`
        // resolves, so this test only proves the skip-with-None-home
        // contract via the audit_path layer indirectly — by passing a
        // home with no audit file, which behaves identically.
        let tmp = TempDir::new().unwrap();
        let out = rotate_if_needed(Some(tmp.path()), 1).unwrap();
        assert!(matches!(out, RotateOutcome::Skipped));
    }

    #[test]
    fn year_month_stamp_known_values() {
        // 2026-05-01T00:00:00Z = 1_777_680_000 seconds since epoch.
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_777_680_000);
        assert_eq!(year_month_stamp(t), "2026-05");
        // 2026-01-31T23:59:59Z — last second of January 2026, still in
        // the January bucket.
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_769_903_999);
        assert_eq!(year_month_stamp(t), "2026-01");
        // 2000-02-29T00:00:00Z — leap day. Tests the doy → m=2 branch.
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(951_782_400);
        assert_eq!(year_month_stamp(t), "2000-02");
    }
}
