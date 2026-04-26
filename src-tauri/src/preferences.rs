//! User preferences, persisted to a ClaudeScope-owned config file.
//!
//! Storage lives under the OS config directory (e.g. `~/.config/claude-scope/
//! config.json` on Linux, `%APPDATA%/claude-scope/config.json` on Windows).
//! This is deliberately separate from Claude Code's own `~/.claude/`
//! settings tree — those are read/written by the main app flow and we don't
//! want ClaudeScope's UI state polluting them.
//!
//! Missing file or parse errors collapse to `Preferences::default()`: a
//! user-visible surface (the settings modal) should still open on a broken
//! or first-run config, not error out.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::scope::Scope;

/// Shape of the persisted config file. Every field carries a `#[serde(default)]`
/// so unknown or missing keys degrade gracefully to sensible defaults — the
/// schema can evolve without forcing a migration on every launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// Scope columns the user wants visible. Defaults to all four. An empty
    /// list is tolerated but renders an empty grid — the UI guards against
    /// that separately by keeping at least one checkbox visually ticked.
    #[serde(default = "default_visible_scopes")]
    pub visible_scopes: Vec<Scope>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            visible_scopes: default_visible_scopes(),
        }
    }
}

fn default_visible_scopes() -> Vec<Scope> {
    Scope::ALL.to_vec()
}

/// Resolve the on-disk path for the config file. `None` when the OS couldn't
/// give us a config directory (no `$HOME` set, say) — we treat that the same
/// as "no config yet" and fall back to defaults.
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("claude-scope").join("config.json"))
}

/// Load preferences from disk. Absent file, unreadable file, and invalid JSON
/// all fall back to `Preferences::default()` — the UI should always open
/// with something sensible.
pub fn load() -> Preferences {
    let Some(path) = config_path() else {
        return Preferences::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Preferences::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Save preferences atomically. Routes through `io_atomic::atomic_write_json`
/// so the pre-write JSON revalidation invariant is enforced at the same
/// chokepoint the settings writer uses — even though `serde_json::to_vec_pretty`
/// against a typed `Preferences` is safe-by-construction today, a future
/// manual render or schema-evolution shortcut can't quietly skip validation
/// without bypassing the helper on purpose.
///
/// `backups: None` is intentional: the preferences file lives in the OS
/// config directory and is cheap to regenerate; a stray `.bak` next to it
/// would be more surprise than safety.
pub fn save(prefs: &Preferences) -> std::io::Result<()> {
    let path = config_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no OS config directory available",
        )
    })?;
    let body = serde_json::to_vec_pretty(prefs)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::io_atomic::atomic_write_json(&path, &body, None).map_err(|e| {
        let kind = match &e {
            crate::io_atomic::IoError::Io { source, .. } => source.kind(),
            crate::io_atomic::IoError::Revalidate { .. } => std::io::ErrorKind::InvalidData,
            _ => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_all_scopes_visible() {
        let prefs = Preferences::default();
        assert_eq!(prefs.visible_scopes.len(), Scope::ALL.len());
        for scope in Scope::ALL {
            assert!(prefs.visible_scopes.contains(&scope));
        }
    }

    #[test]
    fn deserializes_with_missing_field_as_default() {
        // Empty object: no `visible_scopes` key. Defaults should fill in.
        let prefs: Preferences = serde_json::from_str("{}").unwrap();
        assert_eq!(prefs, Preferences::default());
    }

    #[test]
    fn deserializes_with_explicit_subset() {
        let prefs: Preferences =
            serde_json::from_str(r#"{"visible_scopes":["project","user"]}"#).unwrap();
        assert_eq!(prefs.visible_scopes, vec![Scope::Project, Scope::User]);
    }

    #[test]
    fn round_trips_through_json() {
        let prefs = Preferences {
            visible_scopes: vec![Scope::Local, Scope::Project],
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let parsed: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, prefs);
    }

    /// Exercise the save path against a real filesystem so the
    /// `NamedTempFile::persist()` replace succeeds both on first write and
    /// when a previous config file already exists — that second case is
    /// the Windows regression that plain `fs::rename` would fail on.
    #[test]
    fn save_path_overwrites_existing_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("claude-scope").join("config.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        let first = Preferences {
            visible_scopes: vec![Scope::Local],
        };
        let body = serde_json::to_vec_pretty(&first).unwrap();
        let parent = target.parent().unwrap();
        let mut tmpfile = tempfile::NamedTempFile::new_in(parent).unwrap();
        std::io::Write::write_all(&mut tmpfile, &body).unwrap();
        tmpfile.as_file_mut().sync_all().unwrap();
        tmpfile.persist(&target).unwrap();
        assert!(target.exists());

        // Second write overwrites the first — this is where `fs::rename`
        // alone would fail on Windows.
        let second = Preferences {
            visible_scopes: vec![Scope::Project, Scope::User],
        };
        let body2 = serde_json::to_vec_pretty(&second).unwrap();
        let mut tmpfile2 = tempfile::NamedTempFile::new_in(parent).unwrap();
        std::io::Write::write_all(&mut tmpfile2, &body2).unwrap();
        tmpfile2.as_file_mut().sync_all().unwrap();
        tmpfile2.persist(&target).unwrap();

        let round_trip: Preferences =
            serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(round_trip, second);
    }
}
