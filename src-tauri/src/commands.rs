//! Tauri command handlers exposed to the front-end.

use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::io_atomic::{self, BackupTracker};
use crate::model::{PermissionKind, PermissionRules, SettingsDoc};
use crate::preferences::{self, Preferences};
use crate::scope::{self, Scope, ScopePaths};
use crate::watcher::WatchState;

/// Process-global backup tracker so we only write one `.bak` per file per
/// session, regardless of which command triggered the first write.
static BACKUPS: OnceLock<BackupTracker> = OnceLock::new();

fn backups() -> &'static BackupTracker {
    BACKUPS.get_or_init(BackupTracker::new)
}

#[derive(Debug, Serialize)]
pub struct ScopeView {
    pub scope: Scope,
    pub path: Option<String>,
    pub exists: bool,
    pub permissions: PermissionRules,
    /// Non-permission top-level keys with their raw JSON values, in the
    /// order they appear on disk. Drives the UI tree view for `env`,
    /// `hooks`, `theme`, etc.
    pub other_values: serde_json::Map<String, serde_json::Value>,
    pub parse_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LoadedScopes {
    pub project_dir: String,
    pub scopes: Vec<ScopeView>,
    pub effective_permissions: PermissionRules,
}

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub rule: String,
    pub kind: PermissionKind,
    pub from: Scope,
    pub to: Scope,
}

/// Structured preview of a pending move, built so the front-end can render a
/// real before/after diff instead of a plain-text confirmation dialog.
#[derive(Debug, Serialize)]
pub struct MovePreview {
    pub rule: String,
    pub kind: PermissionKind,
    pub from: MoveSide,
    pub to: MoveSide,
}

#[derive(Debug, Serialize)]
pub struct MoveSide {
    pub scope: Scope,
    pub path: String,
    pub path_exists: bool,
    pub rules_before: Vec<String>,
    pub rules_after: Vec<String>,
    /// True when `apply_move` will actually write this side's file. False on
    /// the destination when the rule is already present (no-op), and always
    /// true on the source (we need to remove the rule).
    pub will_write: bool,
    /// Optional human-readable note (e.g. "destination file will be created"
    /// or "already present; source copy will simply be removed").
    pub note: Option<String>,
}

/// Moves a top-level non-permission key (`env`, `hooks`, `theme`, …) between
/// scopes. Companion to `MoveRequest`, which handles individual permission
/// rules; the key-move flow uses its own type because the payload is a raw
/// JSON value rather than a rule string, and the merge semantics differ by
/// value shape (see `SettingsDoc::merge_top_level`).
#[derive(Debug, Deserialize)]
pub struct MoveKeyRequest {
    pub key: String,
    pub from: Scope,
    pub to: Scope,
}

#[derive(Debug, Serialize)]
pub struct MoveKeyPreview {
    pub key: String,
    pub from: MoveKeySide,
    pub to: MoveKeySide,
}

#[derive(Debug, Serialize)]
pub struct MoveKeySide {
    pub scope: Scope,
    pub path: String,
    pub path_exists: bool,
    /// The raw JSON value stored at this key before the move. Omitted from
    /// the serialized payload when the key isn't present — distinguishing
    /// absence from a key explicitly set to JSON `null`, which is a valid
    /// value and would otherwise collide at the wire level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_before: Option<serde_json::Value>,
    /// The JSON value this side will have after the move is applied. `None`
    /// on the source (the key is removed); same absence-vs-null skip as
    /// `value_before`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_after: Option<serde_json::Value>,
    pub will_write: bool,
    pub note: Option<String>,
}

#[tauri::command]
pub fn load_scopes(
    project_dir: Option<String>,
    app: AppHandle,
    watch: State<'_, WatchState>,
) -> Result<LoadedScopes, String> {
    let start = project_dir.as_ref().map(Path::new);
    let paths = scope::resolve(start).map_err(|e| e.to_string())?;
    let loaded = build_loaded(&paths).map_err(|e| e.to_string())?;
    // (Re)install the watcher every time we load. This handles both first
    // load and project-switch with no extra command surface area for the
    // front-end to keep in sync. Watcher errors are non-fatal — auto-reload
    // is a nice-to-have, the load itself succeeded. Surface the failure as
    // a Tauri event instead of eprintln!() so it's observable from the
    // front-end: on Windows release builds we set windows_subsystem =
    // "windows", which discards stderr entirely.
    if let Err(err) = watch.install(app.clone(), &paths) {
        let _ = app.emit("watcher-error", err);
    }
    Ok(loaded)
}

#[tauri::command]
pub fn diff_move(req: MoveRequest, project_dir: Option<String>) -> Result<MovePreview, String> {
    let start = project_dir.as_ref().map(Path::new);
    let paths = scope::resolve(start).map_err(|e| e.to_string())?;
    diff_move_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_move(
    req: MoveRequest,
    project_dir: Option<String>,
    watch: State<'_, WatchState>,
) -> Result<(), String> {
    let start = project_dir.as_ref().map(Path::new);
    let paths = scope::resolve(start).map_err(|e| e.to_string())?;
    apply_move_impl(&paths, &req, backups(), &watch).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn diff_move_key(
    req: MoveKeyRequest,
    project_dir: Option<String>,
) -> Result<MoveKeyPreview, String> {
    let start = project_dir.as_ref().map(Path::new);
    let paths = scope::resolve(start).map_err(|e| e.to_string())?;
    diff_move_key_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_move_key(
    req: MoveKeyRequest,
    project_dir: Option<String>,
    watch: State<'_, WatchState>,
) -> Result<(), String> {
    let start = project_dir.as_ref().map(Path::new);
    let paths = scope::resolve(start).map_err(|e| e.to_string())?;
    apply_move_key_impl(&paths, &req, backups(), &watch).map_err(|e| e.to_string())
}

/// Read user preferences. A missing / malformed config file falls back to
/// defaults — see `preferences::load`.
#[tauri::command]
pub fn load_preferences() -> Preferences {
    preferences::load()
}

/// Persist user preferences to the OS config dir. Unlike the permission
/// save path, this has no `.bak` trail — the preferences file is cheap to
/// regenerate if something goes wrong, and keeping backup files out of the
/// user's config dir is the less surprising default.
#[tauri::command]
pub fn save_preferences(prefs: Preferences) -> Result<(), String> {
    preferences::save(&prefs).map_err(|e| e.to_string())
}

fn build_loaded(paths: &ScopePaths) -> Result<LoadedScopes, Box<dyn std::error::Error>> {
    let mut views = Vec::with_capacity(Scope::ALL.len());

    for scope in Scope::ALL {
        views.push(load_scope_view(scope, paths.path_for(scope)));
    }

    let effective = effective_permissions(&views);

    Ok(LoadedScopes {
        project_dir: paths.project_dir.display().to_string(),
        scopes: views,
        effective_permissions: effective,
    })
}

fn load_scope_view(scope: Scope, path: Option<&Path>) -> ScopeView {
    let mut view = ScopeView {
        scope,
        path: path.map(|p| p.display().to_string()),
        exists: false,
        permissions: PermissionRules::default(),
        other_values: serde_json::Map::new(),
        parse_error: None,
    };
    let Some(p) = path else {
        return view;
    };
    match io_atomic::load(p) {
        Ok(Some(doc)) => {
            view.exists = true;
            view.permissions = doc.permissions();
            view.other_values = doc.other_entries();
        }
        Ok(None) => {}
        Err(e) => {
            view.exists = p.exists();
            view.parse_error = Some(e.to_string());
        }
    }
    view
}

/// Build the effective permission view. For v1 we union `allow` / `deny` /
/// `ask` across the recognized scopes and deduplicate while preserving the
/// order Local → Project → UserLocal → User (highest precedence first).
/// That's a faithful first approximation of "what Claude Code sees" for
/// list-valued rule sets; precedence-sensitive semantics like conflict
/// resolution are a future concern.
fn effective_permissions(views: &[ScopeView]) -> PermissionRules {
    let mut out = PermissionRules::default();
    for view in views {
        for rule in &view.permissions.allow {
            if !out.allow.iter().any(|r| r == rule) {
                out.allow.push(rule.clone());
            }
        }
        for rule in &view.permissions.deny {
            if !out.deny.iter().any(|r| r == rule) {
                out.deny.push(rule.clone());
            }
        }
        for rule in &view.permissions.ask {
            if !out.ask.iter().any(|r| r == rule) {
                out.ask.push(rule.clone());
            }
        }
    }
    out
}

fn diff_move_impl(
    paths: &ScopePaths,
    req: &MoveRequest,
) -> Result<MovePreview, Box<dyn std::error::Error>> {
    if req.from == req.to {
        return Err("source and destination scopes must differ".into());
    }
    let from_path = require_path(paths, req.from)?;
    let to_path = require_path(paths, req.to)?;

    // Source side: file must exist and the rule must currently be there.
    // Match apply_move_impl's error wording so the preview path doesn't
    // produce a different message than the one the user would see if they
    // somehow skipped the preview.
    let from_doc = match io_atomic::load(from_path)? {
        Some(d) => d,
        None => {
            return Err(format!("source file {} does not exist", from_path.display()).into());
        }
    };
    let from_before = from_doc.permissions().get(req.kind).to_vec();
    if !from_before.iter().any(|r| r == &req.rule) {
        return Err(format!(
            "rule `{}` not found in {} {}",
            req.rule,
            req.from.label(),
            from_path.display()
        )
        .into());
    }
    let from_after: Vec<String> = from_before
        .iter()
        .filter(|r| r.as_str() != req.rule)
        .cloned()
        .collect();

    // Destination side: may or may not already have it.
    let to_path_exists = to_path.exists();
    let to_doc = io_atomic::load(to_path)?.unwrap_or_else(SettingsDoc::empty);
    let to_before = to_doc.permissions().get(req.kind).to_vec();
    let already_present = to_before.iter().any(|r| r == &req.rule);
    let to_after: Vec<String> = if already_present {
        to_before.clone()
    } else {
        let mut v = to_before.clone();
        v.push(req.rule.clone());
        v
    };

    let to_note = if already_present {
        Some("Already present; source copy will simply be removed.".to_string())
    } else if !to_path_exists {
        Some("Destination file will be created.".to_string())
    } else {
        None
    };

    Ok(MovePreview {
        rule: req.rule.clone(),
        kind: req.kind,
        from: MoveSide {
            scope: req.from,
            path: from_path.display().to_string(),
            path_exists: from_path.exists(),
            rules_before: from_before,
            rules_after: from_after,
            will_write: true,
            note: None,
        },
        to: MoveSide {
            scope: req.to,
            path: to_path.display().to_string(),
            path_exists: to_path_exists,
            rules_before: to_before,
            rules_after: to_after,
            will_write: !already_present,
            note: to_note,
        },
    })
}

fn apply_move_impl(
    paths: &ScopePaths,
    req: &MoveRequest,
    backups: &BackupTracker,
    watch: &WatchState,
) -> Result<(), Box<dyn std::error::Error>> {
    if req.from == req.to {
        return Err("source and destination scopes must differ".into());
    }
    let from_path = require_path(paths, req.from)?.to_path_buf();
    let to_path = require_path(paths, req.to)?.to_path_buf();

    let (to_doc_loaded, to_stamp) = io_atomic::load_with_stamp(&to_path)?;
    let mut to_doc = to_doc_loaded.unwrap_or_else(SettingsDoc::empty);
    let dest_mutated = to_doc.add_rule(req.kind, &req.rule);

    let (from_doc_loaded, from_stamp) = io_atomic::load_with_stamp(&from_path)?;
    let mut from_doc = match from_doc_loaded {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", from_path.display()).into()),
    };
    let removed = from_doc.remove_rule(req.kind, &req.rule);
    if !removed {
        return Err(format!(
            "rule `{}` not found in {} {}",
            req.rule,
            req.from.label(),
            from_path.display()
        )
        .into());
    }

    // Destination first, then source. If the destination already had the rule
    // there's nothing to write there; skipping the save also avoids creating
    // a spurious `.bak` for a file we aren't actually changing.
    //
    // `note_self_write` is called *after* each successful save, not before:
    // a failed save means no filesystem event will arrive, so suppressing a
    // would-be-legitimate external change would leave the UI stale for no
    // reason. Suppression is path-scoped, so we pass the exact path each
    // save touched.
    //
    // Each save passes the stamp captured at load time. A third party that
    // edited the file between our load and our save aborts with
    // `ConcurrentModification` rather than getting silently overwritten.
    if dest_mutated {
        io_atomic::save(&to_path, &to_doc, backups, Some(&to_stamp))?;
        watch.note_self_write(&to_path);
    }
    // If the source write fails after the destination was updated, roll back
    // the destination so the rule doesn't end up duplicated in both scopes.
    if let Err(source_err) = io_atomic::save(&from_path, &from_doc, backups, Some(&from_stamp)) {
        if dest_mutated {
            to_doc.remove_rule(req.kind, &req.rule);
            // Rollback skips the stamp check: we are the canonical writer of
            // the destination at this point, and the stamp we'd want is the
            // one our own successful write just produced.
            if let Err(rollback_err) = io_atomic::save(&to_path, &to_doc, backups, None) {
                return Err(format!(
                    "source save failed: {source_err}; destination rollback also failed: {rollback_err}"
                )
                .into());
            }
            watch.note_self_write(&to_path);
        }
        return Err(
            format!("source save failed and destination was rolled back: {source_err}").into(),
        );
    }
    watch.note_self_write(&from_path);
    Ok(())
}

fn diff_move_key_impl(
    paths: &ScopePaths,
    req: &MoveKeyRequest,
) -> Result<MoveKeyPreview, Box<dyn std::error::Error>> {
    validate_move_key(req)?;
    let from_path = require_path(paths, req.from)?;
    let to_path = require_path(paths, req.to)?;

    let from_doc = match io_atomic::load(from_path)? {
        Some(d) => d,
        None => {
            return Err(format!("source file {} does not exist", from_path.display()).into());
        }
    };
    let src_value = from_doc.get_top_level(&req.key).cloned().ok_or_else(
        || -> Box<dyn std::error::Error> {
            format!(
                "key `{}` not found in {} {}",
                req.key,
                req.from.label(),
                from_path.display()
            )
            .into()
        },
    )?;

    let to_path_exists = to_path.exists();
    let mut to_doc = io_atomic::load(to_path)?.unwrap_or_else(SettingsDoc::empty);
    let to_before = to_doc.get_top_level(&req.key).cloned();
    to_doc.merge_top_level(&req.key, src_value.clone());
    let to_after = to_doc.get_top_level(&req.key).cloned();

    let dest_unchanged = to_before == to_after;
    let to_note = if dest_unchanged {
        Some(
            "Destination already contains this value; source copy will simply be removed."
                .to_string(),
        )
    } else if !to_path_exists {
        Some("Destination file will be created.".to_string())
    } else if to_before.is_some() {
        Some("Destination already has this key; values will be merged.".to_string())
    } else {
        None
    };

    Ok(MoveKeyPreview {
        key: req.key.clone(),
        from: MoveKeySide {
            scope: req.from,
            path: from_path.display().to_string(),
            path_exists: from_path.exists(),
            value_before: Some(src_value),
            value_after: None,
            will_write: true,
            note: None,
        },
        to: MoveKeySide {
            scope: req.to,
            path: to_path.display().to_string(),
            path_exists: to_path_exists,
            value_before: to_before,
            value_after: to_after,
            will_write: !dest_unchanged,
            note: to_note,
        },
    })
}

fn apply_move_key_impl(
    paths: &ScopePaths,
    req: &MoveKeyRequest,
    backups: &BackupTracker,
    watch: &WatchState,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_move_key(req)?;
    let from_path = require_path(paths, req.from)?.to_path_buf();
    let to_path = require_path(paths, req.to)?.to_path_buf();

    let (from_doc_loaded, from_stamp) = io_atomic::load_with_stamp(&from_path)?;
    let mut from_doc = match from_doc_loaded {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", from_path.display()).into()),
    };
    let src_value = from_doc.get_top_level(&req.key).cloned().ok_or_else(
        || -> Box<dyn std::error::Error> {
            format!(
                "key `{}` not found in {} {}",
                req.key,
                req.from.label(),
                from_path.display()
            )
            .into()
        },
    )?;

    // Snapshot whether the destination file existed on disk before we
    // touched it, so a later rollback can tell "restore the old contents"
    // apart from "we created this file, so removing it is the rollback."
    let to_existed_before = to_path.exists();
    let (to_doc_loaded, to_stamp) = io_atomic::load_with_stamp(&to_path)?;
    let to_doc_before = to_doc_loaded.unwrap_or_else(SettingsDoc::empty);
    let to_before = to_doc_before.get_top_level(&req.key).cloned();
    let mut to_doc = to_doc_before.clone();
    to_doc.merge_top_level(&req.key, src_value.clone());
    let to_after = to_doc.get_top_level(&req.key).cloned();
    let dest_mutated = to_before != to_after;

    from_doc.remove_top_level(&req.key);

    // Destination first, then source — same ordering + rollback shape as
    // apply_move_impl. Skipping the destination save when nothing changed
    // avoids writing a .bak for a file we aren't actually touching.
    //
    // Stamp checks: each save passes the stamp captured at load. Rollback
    // saves pass `None` because we are the canonical writer at that point
    // (see apply_move_impl for the same reasoning).
    if dest_mutated {
        io_atomic::save(&to_path, &to_doc, backups, Some(&to_stamp))?;
        watch.note_self_write(&to_path);
    }
    if let Err(source_err) = io_atomic::save(&from_path, &from_doc, backups, Some(&from_stamp)) {
        if dest_mutated {
            // Roll back the destination. If the file already existed before
            // we wrote to it, restore the pre-merge snapshot. If the save
            // newly created the file, delete it outright — re-saving the
            // empty snapshot would leave a stray `{}` file behind.
            let rollback_result: Result<(), Box<dyn std::error::Error>> = if to_existed_before {
                io_atomic::save(&to_path, &to_doc_before, backups, None).map_err(Into::into)
            } else {
                std::fs::remove_file(&to_path).map_err(Into::into)
            };
            if let Err(rollback_err) = rollback_result {
                return Err(format!(
                    "source save failed: {source_err}; destination rollback also failed: {rollback_err}"
                )
                .into());
            }
            watch.note_self_write(&to_path);
        }
        return Err(
            format!("source save failed and destination was rolled back: {source_err}").into(),
        );
    }
    watch.note_self_write(&from_path);
    Ok(())
}

fn validate_move_key(req: &MoveKeyRequest) -> Result<(), Box<dyn std::error::Error>> {
    if req.from == req.to {
        return Err("source and destination scopes must differ".into());
    }
    if req.key == "permissions" {
        return Err(
            "use the permission move action for individual permission rules; \
             the permissions key is not movable as a whole"
                .into(),
        );
    }
    Ok(())
}

fn require_path(paths: &ScopePaths, scope: Scope) -> Result<&Path, Box<dyn std::error::Error>> {
    paths.path_for(scope).ok_or_else(|| {
        format!(
            "cannot resolve path for scope `{}` (home directory missing?)",
            scope.label()
        )
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(path: &PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn paths_in(tmp: &std::path::Path) -> ScopePaths {
        let project = tmp.join("project");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        let user_home = tmp.join("home");
        std::fs::create_dir_all(user_home.join(".claude")).unwrap();
        ScopePaths {
            project_dir: project.clone(),
            local: Some(project.join(".claude").join("settings.local.json")),
            project: Some(project.join(".claude").join("settings.json")),
            user_local: Some(user_home.join(".claude").join("settings.local.json")),
            user: Some(user_home.join(".claude").join("settings.json")),
        }
    }

    #[test]
    fn move_from_project_to_user_preserves_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{
  "theme": "dark",
  "permissions": { "allow": ["Bash(git status)", "Read(**)"] }
}"#,
        );

        let backups = BackupTracker::new();
        apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
            &backups,
            &WatchState::default(),
        )
        .unwrap();

        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            project_doc.permissions().allow,
            vec!["Read(**)".to_string()]
        );
        assert!(project_doc.other_entries().contains_key("theme"));

        let user_doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            user_doc.permissions().allow,
            vec!["Bash(git status)".to_string()]
        );
    }

    #[test]
    fn diff_move_builds_structured_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)","Read(**)"]}}"#,
        );
        let preview = diff_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap();
        assert_eq!(preview.rule, "Bash(git status)");
        assert_eq!(preview.from.rules_before.len(), 2);
        assert_eq!(preview.from.rules_after, vec!["Read(**)".to_string()]);
        assert!(preview.from.will_write);
        // User file doesn't exist yet.
        assert!(!preview.to.path_exists);
        assert!(preview.to.will_write);
        assert_eq!(preview.to.rules_after, vec!["Bash(git status)".to_string()]);
        assert!(preview.to.note.as_deref() == Some("Destination file will be created."));
    }

    #[test]
    fn diff_move_flags_already_present_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        );
        let preview = diff_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap();
        assert!(
            !preview.to.will_write,
            "dest doesn't need a write when rule already there"
        );
        assert_eq!(preview.to.rules_before, preview.to.rules_after);
        assert!(preview
            .to
            .note
            .as_deref()
            .unwrap()
            .contains("Already present"));
    }

    #[test]
    fn diff_move_errors_when_source_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        // Source file intentionally never written.
        let err = diff_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn diff_move_errors_when_source_lacks_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":[]}}"#,
        );
        let err = diff_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(nope)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn move_creates_destination_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{ "permissions": { "deny": ["WebFetch(domain:evil.example)"] } }"#,
        );
        assert!(!paths.local.as_ref().unwrap().exists());

        let backups = BackupTracker::new();
        apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "WebFetch(domain:evil.example)".into(),
                kind: PermissionKind::Deny,
                from: Scope::Project,
                to: Scope::Local,
            },
            &backups,
            &WatchState::default(),
        )
        .unwrap();

        let local_doc = io_atomic::load(paths.local.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            local_doc.permissions().deny,
            vec!["WebFetch(domain:evil.example)".to_string()]
        );
        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(project_doc.permissions().deny.is_empty());
    }

    #[test]
    fn diff_and_move_round_trip_through_user_local() {
        // Regression for #23: UserLocal participates end-to-end — both as a
        // destination that needs creating from scratch and as a source for a
        // subsequent promotion. Covers diff_move_impl preview shape plus
        // apply_move_impl backup + creation semantics for the new scope.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)","Read(**)"]}}"#,
        );
        // user_local file does not exist yet — first move should create it.
        assert!(!paths.user_local.as_ref().unwrap().exists());

        // Preview: User → UserLocal.
        let preview = diff_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::User,
                to: Scope::UserLocal,
            },
        )
        .unwrap();
        assert_eq!(preview.from.scope, Scope::User);
        assert_eq!(preview.to.scope, Scope::UserLocal);
        assert!(!preview.to.path_exists);
        assert!(preview.to.will_write);
        assert_eq!(preview.to.rules_after, vec!["Bash(git status)".to_string()]);

        let backups = BackupTracker::new();
        apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::User,
                to: Scope::UserLocal,
            },
            &backups,
            &WatchState::default(),
        )
        .unwrap();

        let user_doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(user_doc.permissions().allow, vec!["Read(**)".to_string()]);

        let user_local_doc = io_atomic::load(paths.user_local.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            user_local_doc.permissions().allow,
            vec!["Bash(git status)".to_string()]
        );

        // Now promote it outward: UserLocal → Project.
        apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::UserLocal,
                to: Scope::Project,
            },
            &backups,
            &WatchState::default(),
        )
        .unwrap();

        let user_local_doc = io_atomic::load(paths.user_local.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(user_local_doc.permissions().allow.is_empty());

        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            project_doc.permissions().allow,
            vec!["Bash(git status)".to_string()]
        );
    }

    #[test]
    fn move_same_scope_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let err = apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "x".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::Project,
            },
            &BackupTracker::new(),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must differ"));
    }

    #[test]
    fn move_when_destination_already_has_rule_skips_dest_write() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        );
        let user_bak = paths
            .user
            .as_ref()
            .unwrap()
            .with_file_name("settings.json.bak");
        assert!(!user_bak.exists());

        let backups = BackupTracker::new();
        apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
            &backups,
            &WatchState::default(),
        )
        .unwrap();

        // Source still loses the rule.
        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(project_doc.permissions().allow.is_empty());
        // Destination was not rewritten, so no .bak should have been created.
        assert!(
            !user_bak.exists(),
            "destination .bak should not be created when rule was already present"
        );
    }

    #[test]
    fn move_missing_rule_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":[]}}"#,
        );
        let err = apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(nope)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
            &BackupTracker::new(),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn effective_unions_across_scopes() {
        let views = vec![
            ScopeView {
                scope: Scope::Local,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git status)".into()],
                    deny: vec![],
                    ask: vec![],
                },
                other_values: serde_json::Map::new(),
                parse_error: None,
            },
            ScopeView {
                scope: Scope::Project,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git status)".into(), "Read(**)".into()],
                    deny: vec!["WebFetch(domain:evil.example)".into()],
                    ask: vec![],
                },
                other_values: serde_json::Map::new(),
                parse_error: None,
            },
            ScopeView {
                scope: Scope::User,
                path: None,
                exists: false,
                permissions: PermissionRules::default(),
                other_values: serde_json::Map::new(),
                parse_error: None,
            },
        ];
        let eff = effective_permissions(&views);
        assert_eq!(eff.allow, vec!["Bash(git status)", "Read(**)"]);
        assert_eq!(eff.deny, vec!["WebFetch(domain:evil.example)"]);
        assert!(eff.ask.is_empty());
    }

    #[test]
    fn move_key_merges_env_object_into_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"env": {"PATH": "/src", "HOME": "/home/a"}}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"env": {"PATH": "/dst", "SHELL": "/bin/zsh"}}"#,
        );

        let req = MoveKeyRequest {
            key: "env".to_string(),
            from: Scope::Project,
            to: Scope::User,
        };
        apply_move_key_impl(&paths, &req, &BackupTracker::new(), &WatchState::default()).unwrap();

        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            project_doc.get_top_level("env").is_none(),
            "source env removed"
        );

        let user_doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let env = user_doc.get_top_level("env").unwrap();
        // New keys from source win on conflict; destination-only keys kept.
        assert_eq!(env["PATH"], "/src");
        assert_eq!(env["HOME"], "/home/a");
        assert_eq!(env["SHELL"], "/bin/zsh");
    }

    #[test]
    fn move_key_creates_destination_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{"theme": "dark"}"#);
        assert!(!paths.user.as_ref().unwrap().exists());

        let req = MoveKeyRequest {
            key: "theme".to_string(),
            from: Scope::Project,
            to: Scope::User,
        };
        apply_move_key_impl(&paths, &req, &BackupTracker::new(), &WatchState::default()).unwrap();

        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(project_doc.get_top_level("theme").is_none());
        let user_doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            user_doc.get_top_level("theme").unwrap(),
            &serde_json::json!("dark")
        );
    }

    #[test]
    fn diff_move_key_flags_merge_note_when_destination_has_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{"env": {"A": "1"}}"#);
        write(paths.user.as_ref().unwrap(), r#"{"env": {"B": "2"}}"#);

        let preview = diff_move_key_impl(
            &paths,
            &MoveKeyRequest {
                key: "env".to_string(),
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap();

        assert_eq!(preview.key, "env");
        assert!(preview.from.will_write);
        assert!(preview.to.will_write);
        assert!(preview
            .to
            .note
            .as_deref()
            .is_some_and(|n| n.contains("merged")));
        assert_eq!(
            preview.to.value_after.as_ref().unwrap(),
            &serde_json::json!({"B": "2", "A": "1"})
        );
    }

    #[test]
    fn move_key_refuses_permissions_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let err = diff_move_key_impl(
            &paths,
            &MoveKeyRequest {
                key: "permissions".to_string(),
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("permissions key is not movable"));
    }

    #[test]
    fn move_key_same_scope_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let err = apply_move_key_impl(
            &paths,
            &MoveKeyRequest {
                key: "env".to_string(),
                from: Scope::Project,
                to: Scope::Project,
            },
            &BackupTracker::new(),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must differ"));
    }

    #[test]
    fn move_key_errors_when_source_lacks_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{}"#);
        let err = diff_move_key_impl(
            &paths,
            &MoveKeyRequest {
                key: "theme".to_string(),
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn move_key_dedupes_hook_arrays() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"hooks": ["PreToolUse", "PostToolUse"]}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"hooks": ["PostToolUse", "UserPromptSubmit"]}"#,
        );
        apply_move_key_impl(
            &paths,
            &MoveKeyRequest {
                key: "hooks".to_string(),
                from: Scope::Project,
                to: Scope::User,
            },
            &BackupTracker::new(),
            &WatchState::default(),
        )
        .unwrap();
        let user_doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            user_doc.get_top_level("hooks").unwrap(),
            &serde_json::json!(["PostToolUse", "UserPromptSubmit", "PreToolUse"])
        );
    }
}
