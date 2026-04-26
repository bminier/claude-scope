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
//!   4. Write the new contents to a tempfile in the same directory.
//!   5. Rename the tempfile over the target (atomic on the same filesystem).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
}

impl IoError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
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
    match fs::read_to_string(path) {
        Ok(text) => {
            if text.trim().is_empty() {
                return Ok(Some(SettingsDoc::empty()));
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
            Ok(Some(SettingsDoc::from_value(value, detect_indent(&text))))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IoError::io(path, e)),
    }
}

/// Atomic write with revalidation and one-time-per-session backup.
///
/// Convenience wrapper that renders a [`SettingsDoc`] and routes the bytes
/// through [`atomic_write_json`] with backups enabled.
pub fn save(path: &Path, doc: &SettingsDoc, backups: &BackupTracker) -> Result<(), IoError> {
    let rendered = doc.render();
    atomic_write_json(path, rendered.as_bytes(), Some(backups))
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
pub fn atomic_write_json(
    path: &Path,
    bytes: &[u8],
    backups: Option<&BackupTracker>,
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
    tmp.persist(path).map_err(|e| IoError::io(path, e.error))?;

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
        save(&path, &doc, &backups).unwrap();

        let bak = path.with_file_name("settings.json.bak");
        assert!(bak.exists(), "first save should create .bak");

        // Second save should not update the .bak.
        let bak_mtime = std::fs::metadata(&bak).unwrap().modified().unwrap();
        save(&path, &doc, &backups).unwrap();
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
        save(&path, &doc, &backups).unwrap();

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
        let err = atomic_write_json(&path, b"{ not json", Some(&backups)).unwrap_err();
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
        atomic_write_json(&path, br#"{"updated":true}"#, Some(&backups)).unwrap();

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

        atomic_write_json(&path, br#"{"updated":true}"#, None).unwrap();

        let bak = path.with_file_name("config.json.bak");
        assert!(!bak.exists(), ".bak must not be created when backups: None");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"updated":true}"#
        );
    }
}
