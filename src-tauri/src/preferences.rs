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

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use crate::scope::Scope;

/// Shape of the persisted config file. Every field carries a `#[serde(default)]`
/// so unknown or missing keys degrade gracefully to sensible defaults — the
/// schema can evolve without forcing a migration on every launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the OS `prefers-color-scheme` value at runtime.
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// Scope columns the user wants visible. Defaults to all four. The
    /// deserialize hook normalizes any incoming list (dedup, canonical
    /// ordering, fall back to the default when empty) so a hand-edited or
    /// older buggy config file can't push the app into a state the UI
    /// can't recover from.
    #[serde(
        default = "default_visible_scopes",
        deserialize_with = "deserialize_visible_scopes"
    )]
    pub visible_scopes: Vec<Scope>,
    /// Color theme override. `Auto` defers to the OS at the JS layer;
    /// `Light`/`Dark` pin the palette regardless of OS preference.
    #[serde(default, deserialize_with = "deserialize_theme")]
    pub theme: Theme,
    /// Whether to drop a `.bak` next to a settings file on the first write
    /// per session (#88). Default `true` — silently dropping the safety net
    /// for existing users would be a worse default than minor clutter for
    /// the users who opt out. When `false`, command handlers pass `None`
    /// into `io_atomic::save` and the existing no-backup arm runs.
    #[serde(default = "default_backup_on_write")]
    pub backup_on_write: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            visible_scopes: default_visible_scopes(),
            theme: Theme::default(),
            backup_on_write: default_backup_on_write(),
        }
    }
}

fn default_visible_scopes() -> Vec<Scope> {
    Scope::ALL.to_vec()
}

fn default_backup_on_write() -> bool {
    true
}

/// Normalize a `visible_scopes` list: dedupe, reorder to match `Scope::ALL`,
/// and fall back to the default when nothing remains. Splitting this out of
/// the serde hook keeps it usable from in-process code paths too if a
/// future caller constructs `Preferences` by hand.
fn normalize_visible_scopes(input: Vec<Scope>) -> Vec<Scope> {
    let set: HashSet<Scope> = input.into_iter().collect();
    let ordered: Vec<Scope> = Scope::ALL
        .iter()
        .copied()
        .filter(|s| set.contains(s))
        .collect();
    if ordered.is_empty() {
        default_visible_scopes()
    } else {
        ordered
    }
}

fn deserialize_visible_scopes<'de, D>(deserializer: D) -> Result<Vec<Scope>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<Scope>::deserialize(deserializer)?;
    Ok(normalize_visible_scopes(raw))
}

/// Deserialize a `Theme` value, falling back to `Auto` for any unrecognized
/// string. Without this, an unknown variant (e.g. from a hand-edited config or
/// a future version adding a new theme) would cause `serde_json::from_slice`
/// to fail and `load()` to silently reset *all* preferences via `unwrap_or_default`.
fn deserialize_theme<'de, D>(deserializer: D) -> Result<Theme, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer).unwrap_or_default();
    Ok(match s.as_str() {
        "light" => Theme::Light,
        "dark" => Theme::Dark,
        _ => Theme::Auto,
    })
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
    crate::io_atomic::atomic_write_json(&path, &body, None, None).map_err(|e| {
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
    fn empty_visible_scopes_falls_back_to_default() {
        // A hand-edited or stale config that explicitly persists an empty
        // list shouldn't strand the app in an empty-grid state.
        let prefs: Preferences = serde_json::from_str(r#"{"visible_scopes":[]}"#).unwrap();
        assert_eq!(prefs.visible_scopes, default_visible_scopes());
    }

    #[test]
    fn duplicate_visible_scopes_are_deduped() {
        let prefs: Preferences =
            serde_json::from_str(r#"{"visible_scopes":["project","project","user"]}"#).unwrap();
        assert_eq!(prefs.visible_scopes, vec![Scope::Project, Scope::User]);
    }

    #[test]
    fn visible_scopes_load_in_canonical_order() {
        // Input order is reversed; the loader should reorder to Scope::ALL.
        let prefs: Preferences =
            serde_json::from_str(r#"{"visible_scopes":["user","user_local","project","local"]}"#)
                .unwrap();
        assert_eq!(prefs.visible_scopes, Scope::ALL.to_vec());
    }

    #[test]
    fn round_trips_through_json() {
        let prefs = Preferences {
            visible_scopes: vec![Scope::Local, Scope::Project],
            theme: Theme::Light,
            backup_on_write: false,
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let parsed: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, prefs);
    }

    #[test]
    fn theme_defaults_to_auto() {
        let prefs = Preferences::default();
        assert_eq!(prefs.theme, Theme::Auto);
    }

    #[test]
    fn deserializes_with_missing_theme_field() {
        // Older configs predate the theme field; missing key must collapse
        // to the default rather than fail the whole load.
        let prefs: Preferences = serde_json::from_str(r#"{"visible_scopes":["project"]}"#).unwrap();
        assert_eq!(prefs.theme, Theme::Auto);
    }

    #[test]
    fn theme_round_trips_through_json_for_each_variant() {
        for theme in [Theme::Auto, Theme::Light, Theme::Dark] {
            let prefs = Preferences {
                visible_scopes: default_visible_scopes(),
                theme,
                backup_on_write: true,
            };
            let json = serde_json::to_string(&prefs).unwrap();
            let parsed: Preferences = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.theme, theme);
        }
    }

    #[test]
    fn unknown_theme_value_falls_back_to_auto_without_losing_other_fields() {
        // A hand-edited config or a future schema with an unrecognized theme
        // variant must degrade to Auto rather than failing the whole parse and
        // resetting visible_scopes (and any other fields) via unwrap_or_default.
        let prefs: Preferences =
            serde_json::from_str(r#"{"visible_scopes":["project"],"theme":"sepia"}"#).unwrap();
        assert_eq!(prefs.theme, Theme::Auto);
        assert_eq!(prefs.visible_scopes, vec![Scope::Project]);
    }

    #[test]
    fn theme_serializes_lowercase() {
        let prefs = Preferences {
            visible_scopes: default_visible_scopes(),
            theme: Theme::Dark,
            backup_on_write: true,
        };
        let json = serde_json::to_string(&prefs).unwrap();
        // The JS side reads this string verbatim — pinning the casing
        // here keeps the IPC contract from drifting silently.
        assert!(json.contains(r#""theme":"dark""#));
    }

    #[test]
    fn backup_on_write_defaults_to_true() {
        // Default constructor (the load-path fallback) must keep the
        // existing safety behavior so a fresh install or unreadable config
        // doesn't silently regress to no-backup writes.
        let prefs = Preferences::default();
        assert!(prefs.backup_on_write);
    }

    #[test]
    fn backup_on_write_missing_field_defaults_to_true() {
        // Older configs predate the field; missing key must collapse to
        // the safe default, not deserialize as `false` (the bool default).
        let prefs: Preferences = serde_json::from_str(r#"{"visible_scopes":["project"]}"#).unwrap();
        assert!(prefs.backup_on_write);
    }

    #[test]
    fn backup_on_write_explicit_false_survives_round_trip() {
        let prefs: Preferences = serde_json::from_str(r#"{"backup_on_write":false}"#).unwrap();
        assert!(!prefs.backup_on_write);
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(json.contains(r#""backup_on_write":false"#));
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
            theme: Theme::Auto,
            backup_on_write: true,
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
            theme: Theme::Dark,
            backup_on_write: false,
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
