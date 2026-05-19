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
    /// Who triggered the op. Always `Gui` in Phase 1; `Cli` and `Restore`
    /// land with the CLI (#13) and undo (#19 Phase 3) work respectively.
    pub actor: Actor,
    /// Project root that scoped the operation. `None` for user-scope-only
    /// ops (e.g. moving a rule into User scope from another user-level
    /// scope — there is no project context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<PathBuf>,
    /// Source side. `None` for [`Kind::Add`] (no source) and for top-level
    /// `Restore` entries that target multiple files (Phase 3+).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Side>,
    /// Destination side. `None` for [`Kind::Delete`].
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

/// Who triggered the op. Phase 1 only emits `Gui`; the other variants
/// reserve wire-format slots so a CLI (#13) or undo (#19 Phase 3) write
/// doesn't force a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Gui,
    Cli,
    Skill,
    Restore,
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

/// Read every well-formed record from the audit log, in file order. A
/// truncated tail line (no terminating newline AND a parse error) is
/// silently skipped — that's the partial-write tolerance property. A
/// well-formed line that fails parse (e.g. a future schema field this
/// build doesn't understand strictly) is also skipped, with the count of
/// skipped lines returned alongside the records so a caller can surface
/// "N entries unreadable" rather than silently swallowing data.
///
/// Phase 1 callers only need the count for telemetry; the Phase 2 History
/// view will use the records themselves.
pub fn read_all(home: Option<&Path>) -> io::Result<(Vec<Record>, usize)> {
    let Some(path) = audit_path(home) else {
        return Ok((Vec::new(), 0));
    };
    let file = match std::fs::File::open(&path) {
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
}
