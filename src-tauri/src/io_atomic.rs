//! Atomic read/write of settings files with per-session backup.
//!
//! All write paths in the app funnel through [`atomic_write_json`]: the
//! pre-write JSON revalidation, parent-directory creation, optional `.bak`
//! handling, tempfile + sync + rename sequence all live there. Higher-level
//! callers ([`save`] for `SettingsDoc`, `preferences::save` for the user
//! preferences file) render their own bytes and hand them to the helper.
//!
//! Write strategy (enforced inside `atomic_write_json`):
//!   1. Re-parse the produced bytes as JSON. Refuse before any disk I/O if
//!      they don't parse — the caller's serializer being safe-by-construction
//!      is not enough; the invariant lives at the I/O boundary so it can't
//!      be lost in a future refactor.
//!   2. Create the parent directory if missing.
//!   3. On the first write of this process that targets a given path, copy
//!      the existing file (if any) to `<file>.bak` — opt-in per call site
//!      via the `backups` argument.
//!   4. Write the new contents to a tempfile in the same directory and
//!      `sync_all()` the file's data + metadata.
//!   5. If the caller passed an expected [`FileStamp`], re-stat the target
//!      and refuse with [`IoError::ConcurrentModification`] when the stamp
//!      differs — this app should not silently overwrite an edit a hand-edit
//!      session or another tool made between our load and our save.
//!   6. Rename the tempfile over the target (atomic on the same filesystem).
//!   7. On Unix, `sync_all()` the parent directory so the rename itself is
//!      durable across a crash. Windows skips this step — see the
//!      `atomic_write_json` doc-comment for the rationale.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::Value;

use crate::model::SettingsDoc;

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON parse error in {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("refused to write {path}: produced JSON failed revalidation: {reason}")]
    Revalidate { path: PathBuf, reason: String },
    #[error("settings root must be a JSON object in {path}")]
    NotObject { path: PathBuf },
    #[error("{path} changed on disk between load and save; reload and retry")]
    ConcurrentModification { path: PathBuf },
}

impl IoError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Snapshot captured at load time, used to detect concurrent modification
/// before a save overwrites someone else's edit. Pairs `(mtime, len)` so a
/// rewrite that lands on the same mtime tick (mtime granularity is as coarse
/// as 1s on FAT, 2s on some Windows configurations) is still distinguishable
/// when the size differs.
///
/// `Missing` represents "the file did not exist when we read it" so a save
/// expecting `Missing` correctly fails if a third party has since created
/// the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStamp {
    Missing,
    Present { mtime: SystemTime, len: u64 },
}

impl FileStamp {
    /// Stat `path` and capture a stamp for it.
    pub fn of(path: &Path) -> Result<Self, IoError> {
        match fs::metadata(path) {
            Ok(m) => {
                let mtime = m.modified().map_err(|e| IoError::io(path, e))?;
                Ok(FileStamp::Present {
                    mtime,
                    len: m.len(),
                })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FileStamp::Missing),
            Err(e) => Err(IoError::io(path, e)),
        }
    }
}

/// Tracks which files we've already backed up this process, so we only create
/// one `.bak` per session regardless of how many writes happen.
#[derive(Default)]
pub struct BackupTracker {
    seen: Mutex<HashSet<PathBuf>>,
}

impl BackupTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure `path` has been backed up this session, holding the lock for
    /// the full check-and-copy so two concurrent saves of the same file can't
    /// race past each other and each attempt a backup.
    ///
    /// If a pre-session file exists at `path` and no `.bak` is already
    /// present from an earlier run, copy it aside. On copy failure the path
    /// is *not* recorded so a later retry still gets a chance. When there's
    /// no pre-session file to preserve we still record the path, preventing
    /// a later save (after our own session created the file) from mistaking
    /// its own earlier write for a pre-session original.
    fn ensure_backed_up(&self, path: &Path) -> Result<(), IoError> {
        let mut guard = self.seen.lock().expect("backup tracker poisoned");
        if guard.contains(path) {
            return Ok(());
        }
        if path.exists() {
            let bak = bak_path(path);
            if !bak.exists() {
                fs::copy(path, &bak).map_err(|e| IoError::io(&bak, e))?;
            }
        }
        guard.insert(path.to_path_buf());
        Ok(())
    }
}

/// Load a settings file, returning `Ok(None)` when the file does not exist.
/// A missing file is a normal state (e.g. a project without a `settings.json`).
pub fn load(path: &Path) -> Result<Option<SettingsDoc>, IoError> {
    load_with_stamp(path).map(|(doc, _)| doc)
}

/// Load a settings file along with a [`FileStamp`] capturing its on-disk
/// state. Pass the stamp back into [`save`] / [`atomic_write_json`] to refuse
/// the write when the file was modified by another process between the
/// load and the save.
///
/// The stamp is taken from the same open file handle the contents are read
/// from, closing the small TOCTOU window a separate `metadata()` + `read`
/// pair would have. `FileStamp::Missing` is returned for a non-existent
/// file, distinguishing "no file when we looked" from any present-state
/// stamp.
pub fn load_with_stamp(path: &Path) -> Result<(Option<SettingsDoc>, FileStamp), IoError> {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((None, FileStamp::Missing)),
        Err(e) => return Err(IoError::io(path, e)),
    };
    let meta = file.metadata().map_err(|e| IoError::io(path, e))?;
    let stamp = FileStamp::Present {
        mtime: meta.modified().map_err(|e| IoError::io(path, e))?,
        len: meta.len(),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| IoError::io(path, e))?;
    drop(file);

    if text.trim().is_empty() {
        return Ok((Some(SettingsDoc::empty()), stamp));
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| IoError::Parse {
        path: path.to_path_buf(),
        source: e,
    })?;
    if !value.is_object() {
        return Err(IoError::NotObject {
            path: path.to_path_buf(),
        });
    }
    Ok((
        Some(SettingsDoc::from_value(value, detect_indent(&text))),
        stamp,
    ))
}

/// Atomic write with revalidation and one-time-per-session backup.
///
/// Convenience wrapper that renders a [`SettingsDoc`] and routes the bytes
/// through [`atomic_write_json`] with backups enabled. `expected` carries
/// the [`FileStamp`] the caller captured at load time so a concurrent edit
/// is detected and refused — pass `None` to skip the check (e.g. on a
/// rollback path where the app is the canonical writer).
pub fn save(
    path: &Path,
    doc: &SettingsDoc,
    backups: &BackupTracker,
    expected: Option<&FileStamp>,
) -> Result<(), IoError> {
    let rendered = doc.render();
    atomic_write_json(path, rendered.as_bytes(), Some(backups), expected)
}

/// The single chokepoint every write path in the app must use.
///
/// **Invariant:** `bytes` is re-parsed as JSON before any disk I/O. On parse
/// failure the function returns [`IoError::Revalidate`] and **no tempfile,
/// `.bak`, or destination file is created or modified**. Funnelling every
/// writer through here means a future caller — a manually-rendered config,
/// a new asset type per #11, a "fast path" that skips serde — has to
/// deliberately bypass this function to skip validation, not just forget
/// a line.
///
/// `backups` is `Some(_)` for user-data files where a one-shot recovery copy
/// is worth the surprise of an extra file on disk (e.g. `~/.claude/
/// settings.json`), and `None` for ClaudeScope-owned files that are cheap to
/// regenerate and would clutter their directory with `.bak`s (e.g. the
/// preferences file under the OS config dir).
///
/// `expected` is `Some(_)` to refuse the write when the destination has
/// changed on disk since the caller's load — paired with a [`FileStamp`]
/// captured by [`load_with_stamp`] or [`FileStamp::of`]. The check is
/// best-effort: a third-party writer that lands between the stamp recheck
/// and `tempfile::persist` (a microsecond-scale window) will still be
/// overwritten. For coordinated rollback paths where this app is the
/// canonical writer, pass `None`.
///
/// **Durability:** after the rename, on Unix this also opens the parent
/// directory and `sync_all()`s it — without that, a crash between the
/// `rename` syscall and the kernel flushing the directory entry can leave
/// the *file data* on disk while the *directory entry pointing at it* is
/// lost. Windows skips the parent-dir step: opening a directory handle for
/// flushing requires `FILE_FLAG_BACKUP_SEMANTICS`, which `std::fs::File`
/// does not expose for directory opens, so doing the equivalent would need
/// a small raw-winapi wrapper. NTFS journals rename metadata as part of
/// `MoveFileEx`, so the additional sync would mostly duplicate work the
/// filesystem already commits to.
///
/// If the parent-dir sync itself fails, the function returns
/// `IoError::Io { path: <parent> }` *after* the rename has already taken
/// effect. Treat such an error as a durability warning, not as "the write
/// did not happen": the destination has been replaced, but its directory
/// entry isn't yet guaranteed to survive a crash. This is the honest
/// failure mode — silently swallowing the fsync error would leave the docs
/// claiming durability the code can't actually deliver, and `eprintln!` is
/// invisible in Windows release builds where `windows_subsystem = "windows"`
/// discards stderr.
pub fn atomic_write_json(
    path: &Path,
    bytes: &[u8],
    backups: Option<&BackupTracker>,
    expected: Option<&FileStamp>,
) -> Result<(), IoError> {
    // Revalidate before touching disk. This is the load-bearing contract:
    // bad bytes in => no observable filesystem effect.
    serde_json::from_slice::<Value>(bytes).map_err(|e| IoError::Revalidate {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;

    let parent = path
        .parent()
        .ok_or_else(|| IoError::io(path, std::io::Error::other("path has no parent")))?;
    fs::create_dir_all(parent).map_err(|e| IoError::io(parent, e))?;

    // Back up the pre-session file on the first save this session. The
    // tracker handles concurrency, missing-file, and existing-.bak cases.
    if let Some(backups) = backups {
        backups.ensure_backed_up(path)?;
    }

    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| IoError::io(parent, e))?;
    tmp.write_all(bytes)
        .map_err(|e| IoError::io(tmp.path(), e))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|e| IoError::io(tmp.path(), e))?;

    // Stamp recheck immediately before persist — the closer to the rename,
    // the smaller the race window. Done after the tempfile is fsynced so a
    // mismatch doesn't waste the write to a tempfile we'd otherwise have
    // to clean up: NamedTempFile's Drop removes it for us when we return
    // without persisting.
    if let Some(expected) = expected {
        let current = FileStamp::of(path)?;
        if &current != expected {
            return Err(IoError::ConcurrentModification {
                path: path.to_path_buf(),
            });
        }
    }

    tmp.persist(path).map_err(|e| IoError::io(path, e.error))?;

    // After `persist`, the destination path has already been replaced. A
    // parent-directory sync failure here means the write is committed but
    // not yet crash-durable; propagate it as `IoError::Io` against the
    // parent path so the caller sees an honest failure rather than a silent
    // swallow. See the doc-comment on `atomic_write_json` for the contract
    // callers must follow when this happens.
    sync_parent_dir(parent)?;

    Ok(())
}

/// Flush the parent directory so the rename in `atomic_write_json` is
/// crash-durable, not just crash-atomic. Unix-only; see the
/// `atomic_write_json` doc-comment for why Windows is a no-op here.
#[cfg(unix)]
fn sync_parent_dir(parent: &Path) -> Result<(), IoError> {
    let dir = fs::File::open(parent).map_err(|e| IoError::io(parent, e))?;
    dir.sync_all().map_err(|e| IoError::io(parent, e))
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) -> Result<(), IoError> {
    Ok(())
}

fn bak_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

/// Detect the indentation width of the first indented line. Defaults to 2
/// when the file has no indentation. Tabs are returned as a tab character.
fn detect_indent(text: &str) -> Indent {
    for line in text.lines() {
        let mut chars = line.chars();
        match chars.next() {
            Some('\t') => return Indent::Tab,
            Some(' ') => {
                let width = 1 + chars.take_while(|c| *c == ' ').count();
                return Indent::Spaces(width);
            }
            _ => continue,
        }
    }
    Indent::Spaces(2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Spaces(usize),
    Tab,
}

impl Indent {
    pub fn as_bytes(self) -> Vec<u8> {
        match self {
            Indent::Spaces(n) => vec![b' '; n],
            Indent::Tab => vec![b'\t'],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_two_space_indent() {
        let text = "{\n  \"a\": 1\n}";
        assert_eq!(detect_indent(text), Indent::Spaces(2));
    }

    #[test]
    fn detects_four_space_indent() {
        let text = "{\n    \"a\": 1\n}";
        assert_eq!(detect_indent(text), Indent::Spaces(4));
    }

    #[test]
    fn detects_tab_indent() {
        let text = "{\n\t\"a\": 1\n}";
        assert_eq!(detect_indent(text), Indent::Tab);
    }

    #[test]
    fn load_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let got = load(&tmp.path().join("missing.json")).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn load_and_save_roundtrip_creates_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
  "permissions": {
    "allow": ["Bash(git status)"]
  }
}"#,
        )
        .unwrap();

        let backups = BackupTracker::new();
        let doc = load(&path).unwrap().unwrap();
        save(&path, &doc, &backups, None).unwrap();

        let bak = path.with_file_name("settings.json.bak");
        assert!(bak.exists(), "first save should create .bak");

        // Second save should not update the .bak.
        let bak_mtime = std::fs::metadata(&bak).unwrap().modified().unwrap();
        save(&path, &doc, &backups, None).unwrap();
        let bak_mtime2 = std::fs::metadata(&bak).unwrap().modified().unwrap();
        assert_eq!(bak_mtime, bak_mtime2);
    }

    #[test]
    fn save_preserves_existing_bak_from_previous_run() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"permissions":{"allow":["new"]}}"#).unwrap();
        let bak = path.with_file_name("settings.json.bak");
        // Pre-existing backup from a prior session.
        std::fs::write(&bak, r#"{"permissions":{"allow":["preserved"]}}"#).unwrap();

        let backups = BackupTracker::new();
        let doc = load(&path).unwrap().unwrap();
        save(&path, &doc, &backups, None).unwrap();

        let bak_contents = std::fs::read_to_string(&bak).unwrap();
        assert!(
            bak_contents.contains("preserved"),
            "pre-existing .bak must not be clobbered on first save of session, got: {bak_contents}"
        );
    }

    #[test]
    fn load_rejects_non_object_root() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "[]").unwrap();
        let err = load(&path).unwrap_err();
        assert!(matches!(err, IoError::NotObject { .. }));
    }

    /// The load-bearing contract of `atomic_write_json`: invalid bytes in
    /// must produce zero observable filesystem effect — no tempfile, no
    /// `.bak`, and the existing destination file is left exactly as it was.
    #[test]
    fn atomic_write_json_rejects_invalid_bytes_without_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let bak = path.with_file_name("settings.json.bak");
        std::fs::write(&path, r#"{"original":true}"#).unwrap();

        let backups = BackupTracker::new();
        let err = atomic_write_json(&path, b"{ not json", Some(&backups), None).unwrap_err();
        assert!(
            matches!(err, IoError::Revalidate { .. }),
            "expected Revalidate, got {err:?}"
        );

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after, r#"{"original":true}"#,
            "destination file must not be touched on revalidation failure"
        );
        assert!(
            !bak.exists(),
            ".bak must not be created on revalidation failure"
        );

        // No stray tempfiles left in the parent directory.
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != "settings.json")
            .map(|e| e.file_name())
            .collect();
        assert!(
            strays.is_empty(),
            "no extra files should remain in parent, got: {strays:?}"
        );
    }

    #[test]
    fn atomic_write_json_writes_valid_bytes_with_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"original":true}"#).unwrap();

        let backups = BackupTracker::new();
        atomic_write_json(&path, br#"{"updated":true}"#, Some(&backups), None).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"updated":true}"#
        );
        let bak = path.with_file_name("settings.json.bak");
        assert!(bak.exists(), "backup should be created on first write");
        assert_eq!(
            std::fs::read_to_string(&bak).unwrap(),
            r#"{"original":true}"#
        );
    }

    #[test]
    fn atomic_write_json_skips_backup_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"original":true}"#).unwrap();

        atomic_write_json(&path, br#"{"updated":true}"#, None, None).unwrap();

        let bak = path.with_file_name("config.json.bak");
        assert!(!bak.exists(), ".bak must not be created when backups: None");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"updated":true}"#
        );
    }

    /// Bump the file's mtime far enough that any filesystem timestamp
    /// granularity (1s on FAT, 2s on some Windows configs) reads it as
    /// distinct from the original.
    fn touch_mtime_in_the_past(path: &Path) {
        let earlier = SystemTime::now() - std::time::Duration::from_secs(10);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(earlier).unwrap();
    }

    #[test]
    fn atomic_write_json_refuses_when_stamp_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"original":true}"#).unwrap();

        // Snapshot stamp, then someone else edits the file.
        let stamp = FileStamp::of(&path).unwrap();
        std::fs::write(&path, r#"{"hand_edited":true}"#).unwrap();
        touch_mtime_in_the_past(&path); // make sure mtime is observably different

        let backups = BackupTracker::new();
        let err = atomic_write_json(
            &path,
            br#"{"our_overwrite":true}"#,
            Some(&backups),
            Some(&stamp),
        )
        .unwrap_err();
        assert!(
            matches!(err, IoError::ConcurrentModification { .. }),
            "expected ConcurrentModification, got {err:?}"
        );

        // The hand edit must survive.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"hand_edited":true}"#
        );
    }

    #[test]
    fn atomic_write_json_refuses_when_expected_missing_but_file_appeared() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // We expected the file to be missing — caller's load returned None.
        let stamp = FileStamp::Missing;
        // Then someone else created it.
        std::fs::write(&path, r#"{"created_by_other":true}"#).unwrap();

        let backups = BackupTracker::new();
        let err = atomic_write_json(
            &path,
            br#"{"our_create":true}"#,
            Some(&backups),
            Some(&stamp),
        )
        .unwrap_err();
        assert!(
            matches!(err, IoError::ConcurrentModification { .. }),
            "expected ConcurrentModification when stamp said Missing but file now present, got {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"created_by_other":true}"#,
            "third-party content must survive"
        );
    }

    #[test]
    fn atomic_write_json_succeeds_when_stamp_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"original":true}"#).unwrap();

        let stamp = FileStamp::of(&path).unwrap();
        let backups = BackupTracker::new();
        atomic_write_json(&path, br#"{"updated":true}"#, Some(&backups), Some(&stamp)).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"updated":true}"#
        );
    }

    #[test]
    fn load_with_stamp_returns_missing_for_absent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (doc, stamp) = load_with_stamp(&tmp.path().join("nope.json")).unwrap();
        assert!(doc.is_none());
        assert_eq!(stamp, FileStamp::Missing);
    }

    #[test]
    fn load_with_stamp_returns_present_for_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"a":1}"#).unwrap();

        let (doc, stamp) = load_with_stamp(&path).unwrap();
        assert!(doc.is_some());
        assert!(matches!(stamp, FileStamp::Present { .. }));
    }
}
