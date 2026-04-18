//! Atomic read/write of settings files with per-session backup.
//!
//! Write strategy:
//!   1. Serialize the updated JSON value.
//!   2. Re-parse it to guarantee the output is valid JSON. Refuse otherwise.
//!   3. On the first write of this process that targets a given path, copy
//!      the existing file (if any) to `<file>.bak`.
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
pub fn save(path: &Path, doc: &SettingsDoc, backups: &BackupTracker) -> Result<(), IoError> {
    let rendered = doc.render();
    // Revalidate before touching disk.
    serde_json::from_str::<Value>(&rendered).map_err(|e| IoError::Revalidate {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;

    let parent = path
        .parent()
        .ok_or_else(|| IoError::io(path, std::io::Error::other("path has no parent")))?;
    fs::create_dir_all(parent).map_err(|e| IoError::io(parent, e))?;

    // Back up the pre-session file on the first save this session. The
    // tracker handles concurrency, missing-file, and existing-.bak cases.
    backups.ensure_backed_up(path)?;

    let mut tmp =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| IoError::io(parent, e))?;
    tmp.write_all(rendered.as_bytes())
        .map_err(|e| IoError::io(tmp.path(), e))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|e| IoError::io(tmp.path(), e))?;
    tmp.persist(path)
        .map_err(|e| IoError::io(path, e.error))?;

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
}
