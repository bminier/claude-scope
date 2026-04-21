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

/// Save preferences atomically (tempfile + rename). Creates the parent
/// directory if it doesn't exist yet. Errors propagate so the caller can
/// surface them to the user; silent failure here would be a confusing
/// "my toggle didn't stick."
pub fn save(prefs: &Preferences) -> std::io::Result<()> {
    let path = config_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no OS config directory available",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let body = serde_json::to_vec_pretty(prefs)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Atomic write: tempfile next to the target, rename over. Rename is
    // atomic on the same filesystem, so a crash mid-write can't leave the
    // config half-written.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
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
}
