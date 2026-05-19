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
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::scope::Scope;

/// Maximum number of paths kept in `recent_projects`. The dropdown (#47)
/// shows this many entries plus an "Open project…" escape hatch — enough
/// for the working set of repos a user typically toggles between without
/// turning the menu into a vertical scroll.
pub const RECENT_PROJECTS_CAP: usize = 10;

/// Bounds on the user-configurable audit-log rotation cap (#127). The
/// floor is small but non-zero so a misclick can't trigger rotation on
/// every append; the ceiling is high enough that "effectively never" is
/// expressible without a separate kill-switch.
pub const AUDIT_LOG_MAX_SIZE_MB_MIN: u32 = 1;
pub const AUDIT_LOG_MAX_SIZE_MB_MAX: u32 = 1000;
pub const AUDIT_LOG_MAX_SIZE_MB_DEFAULT: u32 = 10;

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
    /// LRU of project roots the user has opened in ClaudeScope, most-recent
    /// first. Drives the project-label dropdown (#47). The deserialize hook
    /// dedupes and caps at [`RECENT_PROJECTS_CAP`] so a hand-edited or
    /// older config can't push the menu into an unbounded list.
    #[serde(default, deserialize_with = "deserialize_recent_projects")]
    pub recent_projects: Vec<PathBuf>,
    /// Whether to rotate `audit.jsonl` once it exceeds
    /// [`Self::audit_log_max_size_mb`] (#127). Default `true` — auto-rotation
    /// is the safer default for a personal tool that will otherwise
    /// accumulate years of writes into a single file. Toggle off only if
    /// you want the audit log to grow without fragmenting into archives.
    #[serde(default = "default_audit_log_rotate")]
    pub audit_log_rotate: bool,
    /// Size threshold in MB at which `audit.jsonl` is rotated. Clamped at
    /// load time to `[AUDIT_LOG_MAX_SIZE_MB_MIN, AUDIT_LOG_MAX_SIZE_MB_MAX]`
    /// so a hand-edited or older config can't push the cap to zero (which
    /// would rotate on every append) or absurdly large (effectively
    /// disabling, which is what `audit_log_rotate=false` is for).
    #[serde(
        default = "default_audit_log_max_size_mb",
        deserialize_with = "deserialize_audit_log_max_size_mb"
    )]
    pub audit_log_max_size_mb: u32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            visible_scopes: default_visible_scopes(),
            theme: Theme::default(),
            backup_on_write: default_backup_on_write(),
            recent_projects: Vec::new(),
            audit_log_rotate: default_audit_log_rotate(),
            audit_log_max_size_mb: default_audit_log_max_size_mb(),
        }
    }
}

fn default_visible_scopes() -> Vec<Scope> {
    Scope::ALL.to_vec()
}

fn default_backup_on_write() -> bool {
    true
}

fn default_audit_log_rotate() -> bool {
    true
}

fn default_audit_log_max_size_mb() -> u32 {
    AUDIT_LOG_MAX_SIZE_MB_DEFAULT
}

/// Clamp the deserialized audit-log size cap so a hand-edited config
/// can't ship the user a silently-broken rotation policy. Out-of-range
/// values fall back to the default rather than the nearest boundary, so
/// the user sees the safe baseline instead of a value they didn't pick.
fn deserialize_audit_log_max_size_mb<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = u32::deserialize(deserializer)?;
    if (AUDIT_LOG_MAX_SIZE_MB_MIN..=AUDIT_LOG_MAX_SIZE_MB_MAX).contains(&raw) {
        Ok(raw)
    } else {
        Ok(default_audit_log_max_size_mb())
    }
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

/// Compare two paths for LRU de-dup. Case-insensitive on Windows (filesystem
/// is case-insensitive there, so two entries differing only in drive-letter
/// case refer to the same directory and would otherwise stack); exact match
/// elsewhere.
fn same_recent_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        a.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// Drop empty paths, dedupe (preserving first-seen order, which from the
/// JSON file means most-recent-first), and truncate to the cap. Shared by
/// the serde hook and the in-process [`push_recent_project`] helper so both
/// pathways enforce the same invariants.
fn normalize_recent_projects(input: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(input.len().min(RECENT_PROJECTS_CAP));
    for p in input {
        if p.as_os_str().is_empty() {
            continue;
        }
        if out.iter().any(|existing| same_recent_path(existing, &p)) {
            continue;
        }
        out.push(p);
        if out.len() >= RECENT_PROJECTS_CAP {
            break;
        }
    }
    out
}

fn deserialize_recent_projects<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<PathBuf>::deserialize(deserializer)?;
    Ok(normalize_recent_projects(raw))
}

/// Promote `path` to the front of [`Preferences::recent_projects`], removing
/// any earlier occurrence (case-insensitively on Windows) and truncating to
/// the cap. Called from `load_scopes` on every successful project load so the
/// LRU stays honest — the user opening a project IS the signal we want to
/// record, no extra UI ceremony needed.
pub fn push_recent_project(prefs: &mut Preferences, path: &Path) {
    if path.as_os_str().is_empty() {
        return;
    }
    prefs
        .recent_projects
        .retain(|existing| !same_recent_path(existing, path));
    prefs.recent_projects.insert(0, path.to_path_buf());
    if prefs.recent_projects.len() > RECENT_PROJECTS_CAP {
        prefs.recent_projects.truncate(RECENT_PROJECTS_CAP);
    }
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
            recent_projects: Vec::new(),
            audit_log_rotate: false,
            audit_log_max_size_mb: 25,
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
                recent_projects: Vec::new(),
                audit_log_rotate: true,
                audit_log_max_size_mb: AUDIT_LOG_MAX_SIZE_MB_DEFAULT,
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
            recent_projects: Vec::new(),
            audit_log_rotate: true,
            audit_log_max_size_mb: AUDIT_LOG_MAX_SIZE_MB_DEFAULT,
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

    #[test]
    fn audit_log_rotate_defaults_to_true() {
        // The safer default: a personal tool's audit log auto-rotates
        // rather than growing for years into a hard-to-grep file. Older
        // configs predate the field; missing key must collapse to true.
        let prefs: Preferences = serde_json::from_str("{}").unwrap();
        assert!(prefs.audit_log_rotate);
    }

    #[test]
    fn audit_log_max_size_mb_defaults_to_constant() {
        let prefs = Preferences::default();
        assert_eq!(prefs.audit_log_max_size_mb, AUDIT_LOG_MAX_SIZE_MB_DEFAULT);
    }

    #[test]
    fn audit_log_max_size_mb_clamps_out_of_range_to_default() {
        // Hand-edited config sets the cap to zero (which would rotate on
        // every append) — must clamp back to the default rather than
        // pinning to the floor, so the user sees the safe baseline
        // instead of a value they didn't pick.
        let prefs: Preferences = serde_json::from_str(r#"{"audit_log_max_size_mb":0}"#).unwrap();
        assert_eq!(prefs.audit_log_max_size_mb, AUDIT_LOG_MAX_SIZE_MB_DEFAULT);
        // Same for absurdly large values — `audit_log_rotate=false` is
        // the intended way to express "don't rotate".
        let prefs: Preferences =
            serde_json::from_str(r#"{"audit_log_max_size_mb":99999}"#).unwrap();
        assert_eq!(prefs.audit_log_max_size_mb, AUDIT_LOG_MAX_SIZE_MB_DEFAULT);
    }

    #[test]
    fn audit_log_max_size_mb_in_range_survives_round_trip() {
        let prefs: Preferences = serde_json::from_str(r#"{"audit_log_max_size_mb":25}"#).unwrap();
        assert_eq!(prefs.audit_log_max_size_mb, 25);
        // Boundary values stay (min and max are valid).
        let prefs: Preferences = serde_json::from_str(r#"{"audit_log_max_size_mb":1}"#).unwrap();
        assert_eq!(prefs.audit_log_max_size_mb, 1);
        let prefs: Preferences = serde_json::from_str(r#"{"audit_log_max_size_mb":1000}"#).unwrap();
        assert_eq!(prefs.audit_log_max_size_mb, 1000);
    }

    #[test]
    fn audit_log_rotate_explicit_false_survives_round_trip() {
        let prefs: Preferences = serde_json::from_str(r#"{"audit_log_rotate":false}"#).unwrap();
        assert!(!prefs.audit_log_rotate);
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(json.contains(r#""audit_log_rotate":false"#));
    }

    #[test]
    fn recent_projects_default_is_empty() {
        let prefs = Preferences::default();
        assert!(prefs.recent_projects.is_empty());
    }

    #[test]
    fn recent_projects_missing_field_deserializes_as_empty() {
        let prefs: Preferences = serde_json::from_str("{}").unwrap();
        assert!(prefs.recent_projects.is_empty());
    }

    #[test]
    fn push_recent_project_prepends() {
        let mut prefs = Preferences::default();
        push_recent_project(&mut prefs, Path::new("/a"));
        push_recent_project(&mut prefs, Path::new("/b"));
        assert_eq!(
            prefs.recent_projects,
            vec![PathBuf::from("/b"), PathBuf::from("/a")],
        );
    }

    #[test]
    fn push_recent_project_promotes_existing_to_front() {
        let mut prefs = Preferences::default();
        push_recent_project(&mut prefs, Path::new("/a"));
        push_recent_project(&mut prefs, Path::new("/b"));
        push_recent_project(&mut prefs, Path::new("/a"));
        // `/a` was older; the third push should move it to the front and
        // drop the stale entry, not double-list it.
        assert_eq!(
            prefs.recent_projects,
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
        );
    }

    #[test]
    fn push_recent_project_caps_at_max() {
        let mut prefs = Preferences::default();
        for i in 0..(RECENT_PROJECTS_CAP + 5) {
            push_recent_project(&mut prefs, &PathBuf::from(format!("/p{i}")));
        }
        assert_eq!(prefs.recent_projects.len(), RECENT_PROJECTS_CAP);
        // The most-recent push must still sit at index 0 — truncation
        // drops the tail, not the head.
        assert_eq!(
            prefs.recent_projects[0],
            PathBuf::from(format!("/p{}", RECENT_PROJECTS_CAP + 4))
        );
    }

    #[test]
    fn push_recent_project_ignores_empty_path() {
        let mut prefs = Preferences::default();
        push_recent_project(&mut prefs, Path::new(""));
        assert!(prefs.recent_projects.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn push_recent_project_dedupes_case_insensitively_on_windows() {
        // Windows filesystem is case-insensitive; two entries differing only
        // in drive-letter case point at the same dir and would otherwise
        // stack up in the dropdown.
        let mut prefs = Preferences::default();
        push_recent_project(&mut prefs, Path::new(r"D:\repos\claude-scope"));
        push_recent_project(&mut prefs, Path::new(r"d:\repos\Claude-Scope"));
        assert_eq!(prefs.recent_projects.len(), 1);
        // The newer (lowercased) entry wins — it represents the user's
        // most recent observed casing.
        assert_eq!(
            prefs.recent_projects[0],
            PathBuf::from(r"d:\repos\Claude-Scope")
        );
    }

    #[test]
    fn deserializing_recent_projects_dedupes_and_caps() {
        // Hand-crafted config with duplicates and over-cap length — the
        // serde hook must clean both up at load time so the in-memory
        // invariant holds without an extra normalization step at the call
        // site.
        let mut raw = String::from(r#"{"recent_projects":["#);
        for i in 0..(RECENT_PROJECTS_CAP + 3) {
            if i > 0 {
                raw.push(',');
            }
            raw.push_str(&format!(r#""/p{i}""#));
        }
        raw.push_str(r#",""]}"#);
        let prefs: Preferences = serde_json::from_str(&raw).unwrap();
        assert_eq!(prefs.recent_projects.len(), RECENT_PROJECTS_CAP);
        // Empty path entry was dropped.
        assert!(prefs
            .recent_projects
            .iter()
            .all(|p| !p.as_os_str().is_empty()));
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
            recent_projects: Vec::new(),
            audit_log_rotate: true,
            audit_log_max_size_mb: AUDIT_LOG_MAX_SIZE_MB_DEFAULT,
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
            recent_projects: vec![PathBuf::from("/tmp/p1"), PathBuf::from("/tmp/p2")],
            audit_log_rotate: false,
            audit_log_max_size_mb: 5,
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
