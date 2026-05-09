//! Tauri command handlers exposed to the front-end.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::io_atomic::{self, BackupTracker};
use crate::model::{
    describe_path, key_policy, validate_movable_path, KeyPolicy, MovablePath, PathSeg,
    PermissionKind, PermissionRules, SettingsDoc,
};
use crate::preferences::{self, Preferences};
use crate::runtime::{RuntimeInfo, RuntimeOverrides};
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
    pub combined_permissions: PermissionRules,
    /// Parallel to `combined_permissions`: for each rule in
    /// `allow`/`deny`/`ask`, the list of scopes that contribute that rule, in
    /// precedence order (highest first). Drives the front-end's
    /// scope-origin tooltip on combined rule rows.
    pub combined_origins: PermissionRuleOrigins,
}

/// Per-rule provenance for the combined permissions view: for each rule in
/// the parallel `PermissionRules` array (same kind, same index), the scopes
/// that contributed it, in precedence order.
#[derive(Debug, Default, Serialize)]
pub struct PermissionRuleOrigins {
    pub allow: Vec<Vec<Scope>>,
    pub deny: Vec<Vec<Scope>>,
    pub ask: Vec<Vec<Scope>>,
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

/// Path-based move request. Replaces the per-rule `MoveRequest` and per-key
/// `MoveKeyRequest` flows with a single primitive addressable by JSON path.
/// `validate_movable_path` (in `model`) classifies each request into one of:
/// whole top-level key, whole `permissions.<kind>` array, or a single rule
/// under `permissions.<kind>`. Other intermediate sub-paths are explicitly
/// rejected for v1; broader sub-key moves (e.g. `env.PATH`) are a follow-up
/// to issue #67, not in scope for this primitive.
#[derive(Debug, Deserialize)]
pub struct MoveLeafRequest {
    pub path: Vec<PathSeg>,
    pub from: Scope,
    pub to: Scope,
}

/// What kind of movable path a `MoveLeafPreview` describes. Surfaced on the
/// preview so the frontend's diff modal can pick the right rendering — it
/// would otherwise need to re-classify the path itself, which would mean a
/// second source of truth for the rules in `validate_movable_path`.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveLeafKind {
    TopLevelKey,
    PermissionList,
    PermissionRule,
}

impl MoveLeafKind {
    fn from_movable(m: &MovablePath<'_>) -> Self {
        match m {
            MovablePath::TopLevelKey(_) => MoveLeafKind::TopLevelKey,
            MovablePath::PermissionList(_) => MoveLeafKind::PermissionList,
            MovablePath::PermissionRule(_, _) => MoveLeafKind::PermissionRule,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MoveLeafPreview {
    pub path: Vec<PathSeg>,
    pub kind: MoveLeafKind,
    pub from: MoveLeafSide,
    pub to: MoveLeafSide,
}

/// One side of a `MoveLeafPreview`. The `key_before` / `key_after` pair is the
/// **whole top-level key's value** (for a permission-rule move at
/// `permissions.allow[2]`, that's the entire `permissions` object), so the
/// frontend can drill into the path itself when rendering and avoid having
/// the wire format duplicate parent/leaf pairs. Skip-on-`None` distinguishes
/// "key absent" from "key explicitly set to JSON null", same as
/// `MoveKeySide`.
#[derive(Debug, Serialize)]
pub struct MoveLeafSide {
    pub scope: Scope,
    pub file_path: String,
    pub file_path_exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_before: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_after: Option<serde_json::Value>,
    pub will_write: bool,
    pub note: Option<String>,
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

/// Resolve scope paths honoring the launch-time overrides. The `home`
/// override is always applied so user / user-local lookups stay redirected for
/// the whole session. The `project` override only kicks in when the
/// front-end didn't supply its own `project_dir` (i.e. on first load before
/// the user has picked a project) — once the user picks something explicitly
/// we want that to win.
fn resolve_with_overrides(
    project_dir: Option<&str>,
    overrides: &RuntimeOverrides,
) -> std::io::Result<ScopePaths> {
    let project_path: Option<PathBuf> = match project_dir {
        Some(s) => Some(PathBuf::from(s)),
        None => overrides.project().map(Path::to_path_buf),
    };
    scope::resolve_with_home(project_path.as_deref(), overrides.home())
}

#[tauri::command]
pub fn load_scopes(
    project_dir: Option<String>,
    app: AppHandle,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<LoadedScopes, String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
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
pub fn diff_move(
    req: MoveRequest,
    project_dir: Option<String>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<MovePreview, String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    diff_move_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_move(
    req: MoveRequest,
    project_dir: Option<String>,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    apply_move_impl(&paths, &req, backups(), &watch).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn diff_move_key(
    req: MoveKeyRequest,
    project_dir: Option<String>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<MoveKeyPreview, String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    diff_move_key_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_move_key(
    req: MoveKeyRequest,
    project_dir: Option<String>,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    apply_move_key_impl(&paths, &req, backups(), &watch).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn diff_move_leaf(
    req: MoveLeafRequest,
    project_dir: Option<String>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<MoveLeafPreview, String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    diff_move_leaf_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_move_leaf(
    req: MoveLeafRequest,
    project_dir: Option<String>,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    apply_move_leaf_impl(&paths, &req, backups(), &watch).map_err(|e| e.to_string())
}

/// Snapshot of the launch-time overrides — the front-end uses this to
/// render a persistent sandbox banner under the toolbar so the user can't
/// forget they're in scratch mode.
#[tauri::command]
pub fn load_runtime_info(overrides: State<'_, RuntimeOverrides>) -> RuntimeInfo {
    RuntimeInfo::from_overrides(&overrides)
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

    let (combined, origins) = combined_permissions(&views);

    Ok(LoadedScopes {
        project_dir: paths.project_dir.display().to_string(),
        scopes: views,
        combined_permissions: combined,
        combined_origins: origins,
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

/// Build the combined permission view: union `allow` / `deny` / `ask`
/// across the recognized scopes and deduplicate while preserving the order
/// Local → Project → UserLocal → User (highest precedence first).
///
/// This is intentionally *not* a precedence-aware effective evaluation.
/// When a rule appears in both an `allow` and a `deny` list across scopes,
/// both copies appear in their respective lists and no resolution happens.
/// Naming it "combined" rather than "effective" keeps the UI honest about
/// what the panel shows; modelling Claude Code's full conflict semantics is
/// out of scope for v1 and would require grammar Claude Code does not
/// publicly document.
fn combined_permissions(views: &[ScopeView]) -> (PermissionRules, PermissionRuleOrigins) {
    let mut rules = PermissionRules::default();
    let mut origins = PermissionRuleOrigins::default();
    // `views` is built from `Scope::ALL`, which iterates in precedence order
    // (highest first), so an `origins` Vec built by appending in iteration
    // order naturally lists contributing scopes highest-first too — matching
    // the tooltip's documented order.
    for view in views {
        accumulate(
            &view.permissions.allow,
            view.scope,
            &mut rules.allow,
            &mut origins.allow,
        );
        accumulate(
            &view.permissions.deny,
            view.scope,
            &mut rules.deny,
            &mut origins.deny,
        );
        accumulate(
            &view.permissions.ask,
            view.scope,
            &mut rules.ask,
            &mut origins.ask,
        );
    }
    (rules, origins)
}

fn accumulate(
    incoming: &[String],
    scope: Scope,
    out_rules: &mut Vec<String>,
    out_origins: &mut Vec<Vec<Scope>>,
) {
    for rule in incoming {
        if let Some(existing) = out_rules.iter().position(|r| r == rule) {
            // Guard against the same scope file listing the rule twice
            // (legal JSON, possible after hand-editing): without this check
            // the tooltip would render "Local, Local, User" instead of
            // "Local, User". `contains` is O(scopes) and scopes maxes at 4.
            if !out_origins[existing].contains(&scope) {
                out_origins[existing].push(scope);
            }
        } else {
            out_rules.push(rule.clone());
            out_origins.push(vec![scope]);
        }
    }
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
    } else {
        to_before
            .as_ref()
            .map(|before| policy_preview_note(&req.key, before, &src_value))
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
    // Derive the existence flag from the load result rather than a separate
    // `exists()` call: a third party that creates the file between our
    // `exists()` and our `load_with_stamp` would otherwise leave us with
    // `to_existed_before = false` plus a present stamp, and on rollback
    // we'd `remove_file` a file we never created.
    let (to_doc_loaded, to_stamp) = io_atomic::load_with_stamp(&to_path)?;
    let to_existed_before = to_doc_loaded.is_some();
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

/// The top-level key affected by a movable path. Validation guarantees the
/// first segment is always a `Key` for the three movable shapes, so this is
/// infallible after `validate_movable_path` succeeds. Pulled out so the
/// diff/apply flows can stay readable and the invariant is documented in
/// exactly one place.
fn path_top_level_key(path: &[PathSeg]) -> &str {
    path.first()
        .and_then(PathSeg::as_key)
        .expect("validate_movable_path guarantees path[0] is a key")
}

fn diff_move_leaf_impl(
    paths: &ScopePaths,
    req: &MoveLeafRequest,
) -> Result<MoveLeafPreview, Box<dyn std::error::Error>> {
    if req.from == req.to {
        return Err("source and destination scopes must differ".into());
    }
    let movable = validate_movable_path(&req.path)?;
    let from_path = require_path(paths, req.from)?;
    let to_path = require_path(paths, req.to)?;
    let affected_key = path_top_level_key(&req.path);

    let from_doc = match io_atomic::load(from_path)? {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", from_path.display()).into()),
    };
    let src_value =
        from_doc
            .get_at_path(&req.path)
            .cloned()
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "path `{}` not found in {} {}",
                    describe_path(&req.path),
                    req.from.label(),
                    from_path.display()
                )
                .into()
            })?;

    // Source `key_before` / `key_after` describe the *top-level key* the move
    // is acting through, not the leaf. The frontend already knows the leaf
    // path from the request and drills in itself, so the wire format avoids
    // duplicating parent + child copies of the same JSON.
    let from_key_before = from_doc.get_top_level(affected_key).cloned();
    let mut from_after_doc = from_doc.clone();
    from_after_doc.remove_at_path(&req.path);
    let from_key_after = from_after_doc.get_top_level(affected_key).cloned();

    let to_path_exists = to_path.exists();
    let to_doc_loaded = io_atomic::load(to_path)?.unwrap_or_else(SettingsDoc::empty);
    let to_key_before = to_doc_loaded.get_top_level(affected_key).cloned();
    let mut to_doc = to_doc_loaded.clone();
    to_doc.merge_at_path(&req.path, src_value.clone())?;
    let to_key_after = to_doc.get_top_level(affected_key).cloned();

    let dest_unchanged = to_key_before == to_key_after;
    let to_note = leaf_preview_note(
        &movable,
        dest_unchanged,
        to_path_exists,
        to_key_before.as_ref(),
        &src_value,
    );

    Ok(MoveLeafPreview {
        path: req.path.clone(),
        kind: MoveLeafKind::from_movable(&movable),
        from: MoveLeafSide {
            scope: req.from,
            file_path: from_path.display().to_string(),
            file_path_exists: from_path.exists(),
            key_before: from_key_before,
            key_after: from_key_after,
            will_write: true,
            note: None,
        },
        to: MoveLeafSide {
            scope: req.to,
            file_path: to_path.display().to_string(),
            file_path_exists: to_path_exists,
            key_before: to_key_before,
            key_after: to_key_after,
            will_write: !dest_unchanged,
            note: to_note,
        },
    })
}

fn apply_move_leaf_impl(
    paths: &ScopePaths,
    req: &MoveLeafRequest,
    backups: &BackupTracker,
    watch: &WatchState,
) -> Result<(), Box<dyn std::error::Error>> {
    if req.from == req.to {
        return Err("source and destination scopes must differ".into());
    }
    let _movable = validate_movable_path(&req.path)?;
    let from_path = require_path(paths, req.from)?.to_path_buf();
    let to_path = require_path(paths, req.to)?.to_path_buf();
    let affected_key = path_top_level_key(&req.path);

    let (from_doc_loaded, from_stamp) = io_atomic::load_with_stamp(&from_path)?;
    let mut from_doc = match from_doc_loaded {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", from_path.display()).into()),
    };
    let src_value =
        from_doc
            .get_at_path(&req.path)
            .cloned()
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "path `{}` not found in {} {}",
                    describe_path(&req.path),
                    req.from.label(),
                    from_path.display()
                )
                .into()
            })?;

    // Snapshot whether the destination file existed on disk before we
    // touched it, so a later rollback can tell "restore the old contents"
    // apart from "we created this file, so removing it is the rollback."
    // Same rationale as `apply_move_key_impl` — derive existence from the
    // load result rather than a separate `exists()` call to dodge a TOCTOU
    // window where a third party creates the file mid-flight.
    let (to_doc_loaded, to_stamp) = io_atomic::load_with_stamp(&to_path)?;
    let to_existed_before = to_doc_loaded.is_some();
    let to_doc_before = to_doc_loaded.unwrap_or_else(SettingsDoc::empty);
    let to_key_before = to_doc_before.get_top_level(affected_key).cloned();
    let mut to_doc = to_doc_before.clone();
    to_doc.merge_at_path(&req.path, src_value.clone())?;
    let to_key_after = to_doc.get_top_level(affected_key).cloned();
    let dest_mutated = to_key_before != to_key_after;

    let removed = from_doc.remove_at_path(&req.path);
    if !removed {
        // get_at_path saw the value but remove_at_path didn't — should be
        // unreachable given the same path on the same `from_doc`, but error
        // loudly so a future regression in `remove_at_path` can't silently
        // duplicate the rule across both scopes.
        return Err(format!(
            "internal error: path `{}` resolved on read but failed to remove",
            describe_path(&req.path)
        )
        .into());
    }

    // Destination first, then source — matching `apply_move_impl` /
    // `apply_move_key_impl` so the rollback shape is identical. Stamp checks
    // protect against an external writer landing between our load and our
    // save; rollback writes pass `None` because we're the canonical writer of
    // the destination at that point.
    if dest_mutated {
        io_atomic::save(&to_path, &to_doc, backups, Some(&to_stamp))?;
        watch.note_self_write(&to_path);
    }
    if let Err(source_err) = io_atomic::save(&from_path, &from_doc, backups, Some(&from_stamp)) {
        if dest_mutated {
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

/// Human-readable note shown on the destination side of a `MoveLeafPreview`.
/// Mirrors `policy_preview_note` for top-level key moves and adds notes for
/// the two permission shapes; centralized so the diff path keeps producing
/// the same wording the user would see if the apply path had to fall back.
fn leaf_preview_note(
    movable: &MovablePath<'_>,
    dest_unchanged: bool,
    to_path_exists: bool,
    to_key_before: Option<&serde_json::Value>,
    src_value: &serde_json::Value,
) -> Option<String> {
    if dest_unchanged {
        return Some(
            "Destination already contains this value; source copy will simply be removed."
                .to_string(),
        );
    }
    if !to_path_exists {
        return Some("Destination file will be created.".to_string());
    }
    match movable {
        MovablePath::TopLevelKey(k) => to_key_before.map(|before| policy_preview_note(k, before, src_value)),
        MovablePath::PermissionList(_) => Some(
            "Permission rules are array-unioned: source entries are appended to the destination and deduplicated."
                .to_string(),
        ),
        MovablePath::PermissionRule(_, _) => Some(
            "Single permission rule will be added to the destination's list."
                .to_string(),
        ),
    }
}

/// Human-readable note explaining how the destination's existing value will
/// be combined with the incoming source value, given the documented merge
/// policy for `key`. Shown in the diff preview so the user can confirm
/// before applying the move.
///
/// `to_before` and `src_value` are inspected so that `DeepMerge` /
/// `ArrayUnion` keys whose runtime shapes don't match the policy (e.g. a
/// hand-edited file where `allowedHttpHookUrls` ended up as a string) get
/// an accurate "will be replaced instead" message rather than the nominal
/// merge description. The model layer's own fallback to overwrite on shape
/// mismatch then matches what the preview promised.
fn policy_preview_note(
    key: &str,
    to_before: &serde_json::Value,
    src_value: &serde_json::Value,
) -> String {
    match key_policy(key) {
        KeyPolicy::Replace => "Override-only key: the destination's previous value will be replaced. The original is saved to a .bak alongside the file.".to_string(),
        KeyPolicy::DeepMerge => {
            if to_before.is_object() && src_value.is_object() {
                "Deep-merged key: source values override destination values on conflict; other destination keys are preserved.".to_string()
            } else {
                "Deep-merged key, but the destination or source value is not a JSON object — the destination's value will be replaced instead. The original is saved to a .bak alongside the file.".to_string()
            }
        }
        KeyPolicy::ArrayUnion => {
            if to_before.is_array() && src_value.is_array() {
                "Array-union key: source items are appended to the destination and deduplicated.".to_string()
            } else {
                "Array-union key, but the destination or source value is not a JSON array — the destination's value will be replaced instead. The original is saved to a .bak alongside the file.".to_string()
            }
        }
        KeyPolicy::Sandbox => {
            if to_before.is_object() && src_value.is_object() {
                "Sandbox structured merge: a fixed set of documented array paths (e.g. `filesystem.allowWrite`, `network.allowedDomains`, `excludedCommands`) are concatenated and deduplicated when both sides are arrays (otherwise that leaf is replaced); every other field — including arrays not in that schema — is overwritten by the source.".to_string()
            } else {
                "Sandbox structured merge, but the destination or source value is not a JSON object — the destination's value will be replaced instead. The original is saved to a .bak alongside the file.".to_string()
            }
        }
        KeyPolicy::ReplaceUnknown => "Unknown key — ClaudeScope has no documented merge policy for it. The destination's value will be replaced. The original is saved to a .bak alongside the file. Review the diff carefully.".to_string(),
    }
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
    fn combined_unions_across_scopes() {
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
        let (combined, _origins) = combined_permissions(&views);
        assert_eq!(combined.allow, vec!["Bash(git status)", "Read(**)"]);
        assert_eq!(combined.deny, vec!["WebFetch(domain:evil.example)"]);
        assert!(combined.ask.is_empty());
    }

    /// When the same rule appears in `allow` at one scope and `deny` at
    /// another, the combined view surfaces both copies in their respective
    /// lists rather than resolving the conflict. This is the documented
    /// behavior of `combined_permissions`: it is a union, not a
    /// precedence-aware evaluation. Locking it down so a future "let's
    /// silently make this smarter" change has to delete this test.
    #[test]
    fn combined_surfaces_allow_deny_overlap_without_resolving() {
        let views = vec![
            ScopeView {
                scope: Scope::Local,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git push)".into()],
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
                    allow: vec![],
                    deny: vec!["Bash(git push)".into()],
                    ask: vec![],
                },
                other_values: serde_json::Map::new(),
                parse_error: None,
            },
        ];
        let (combined, _origins) = combined_permissions(&views);
        assert_eq!(combined.allow, vec!["Bash(git push)"]);
        assert_eq!(combined.deny, vec!["Bash(git push)"]);
        assert!(combined.ask.is_empty());
    }

    #[test]
    fn combined_origins_list_contributing_scopes_in_precedence_order() {
        // A rule that appears in three scopes should surface all three in
        // origins, ordered highest-precedence first (Local before
        // UserLocal before User). A rule unique to one scope lists only
        // that scope. Locks down the contract the front-end tooltip relies
        // on.
        let views = vec![
            ScopeView {
                scope: Scope::Local,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git status)".into(), "Read(**)".into()],
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
                permissions: PermissionRules::default(),
                other_values: serde_json::Map::new(),
                parse_error: None,
            },
            ScopeView {
                scope: Scope::UserLocal,
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
                scope: Scope::User,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git status)".into(), "Bash(ls)".into()],
                    deny: vec![],
                    ask: vec![],
                },
                other_values: serde_json::Map::new(),
                parse_error: None,
            },
        ];
        let (combined, origins) = combined_permissions(&views);
        assert_eq!(
            combined.allow,
            vec!["Bash(git status)", "Read(**)", "Bash(ls)"]
        );
        // Same rule across Local + UserLocal + User, in precedence order.
        assert_eq!(
            origins.allow[0],
            vec![Scope::Local, Scope::UserLocal, Scope::User]
        );
        // Rule only in Local.
        assert_eq!(origins.allow[1], vec![Scope::Local]);
        // Rule only in User.
        assert_eq!(origins.allow[2], vec![Scope::User]);
        assert!(origins.deny.is_empty());
        assert!(origins.ask.is_empty());
    }

    #[test]
    fn combined_origins_dedupe_repeated_rule_within_one_scope() {
        // A single settings file can legally contain the same rule string
        // more than once after hand-editing. The combined rule list
        // already de-dupes (via the cross-scope position check), but the
        // origins list must also collapse same-scope repeats so the
        // tooltip doesn't render "Local, Local, User".
        let views = vec![
            ScopeView {
                scope: Scope::Local,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git status)".into(), "Bash(git status)".into()],
                    deny: vec![],
                    ask: vec![],
                },
                other_values: serde_json::Map::new(),
                parse_error: None,
            },
            ScopeView {
                scope: Scope::User,
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
        ];
        let (combined, origins) = combined_permissions(&views);
        assert_eq!(combined.allow, vec!["Bash(git status)"]);
        assert_eq!(origins.allow[0], vec![Scope::Local, Scope::User]);
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
    fn move_key_dedupes_array_union_keys() {
        // allowedHttpHookUrls is a documented array-union key: entries from
        // every scope are concatenated and deduplicated. Moving from project
        // into user with overlap should yield the destination's entries
        // followed by the source's, with the duplicate dropped.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"allowedHttpHookUrls": ["https://a.example", "https://b.example"]}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"allowedHttpHookUrls": ["https://b.example", "https://c.example"]}"#,
        );
        apply_move_key_impl(
            &paths,
            &MoveKeyRequest {
                key: "allowedHttpHookUrls".to_string(),
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
            user_doc.get_top_level("allowedHttpHookUrls").unwrap(),
            &serde_json::json!([
                "https://b.example",
                "https://c.example",
                "https://a.example"
            ])
        );
    }

    #[test]
    fn preview_note_for_replace_keys_calls_out_override_only() {
        let note = policy_preview_note(
            "hooks",
            &serde_json::json!({"PreToolUse": []}),
            &serde_json::json!({"PostToolUse": []}),
        );
        assert!(
            note.contains("Override-only"),
            "expected override-only language for hooks, got: {note}"
        );
        assert!(note.contains(".bak"));
    }

    #[test]
    fn preview_note_for_deep_merge_uses_merge_language_when_shapes_match() {
        let note = policy_preview_note(
            "env",
            &serde_json::json!({"A": "1"}),
            &serde_json::json!({"B": "2"}),
        );
        assert!(note.contains("Deep-merged"));
        assert!(note.contains("source values override"));
    }

    #[test]
    fn preview_note_for_deep_merge_warns_on_shape_mismatch() {
        // Hand-edited file where env ended up as a string. The nominal
        // policy is DeepMerge, but the model layer falls back to replace
        // and the preview must say so.
        let note = policy_preview_note(
            "env",
            &serde_json::json!("oops-not-an-object"),
            &serde_json::json!({"A": "1"}),
        );
        assert!(
            note.contains("not a JSON object"),
            "expected shape-mismatch warning, got: {note}"
        );
        assert!(note.contains("replaced instead"));
    }

    #[test]
    fn preview_note_for_array_union_uses_union_language_when_shapes_match() {
        let note = policy_preview_note(
            "allowedHttpHookUrls",
            &serde_json::json!(["https://a.example"]),
            &serde_json::json!(["https://b.example"]),
        );
        assert!(note.contains("Array-union"));
        assert!(note.contains("appended"));
    }

    #[test]
    fn preview_note_for_array_union_warns_on_shape_mismatch() {
        let note = policy_preview_note(
            "allowedHttpHookUrls",
            &serde_json::json!("not-an-array"),
            &serde_json::json!(["https://b.example"]),
        );
        assert!(
            note.contains("not a JSON array"),
            "expected shape-mismatch warning, got: {note}"
        );
        assert!(note.contains("replaced instead"));
    }

    #[test]
    fn preview_note_for_sandbox_describes_structured_merge() {
        let note = policy_preview_note(
            "sandbox",
            &serde_json::json!({"enabled": false}),
            &serde_json::json!({"enabled": true}),
        );
        assert!(
            note.contains("Sandbox structured merge"),
            "expected structured-merge language, got: {note}"
        );
        assert!(note.contains("concatenated and deduplicated"));
        assert!(note.contains("overwritten"));
    }

    #[test]
    fn preview_note_for_sandbox_warns_on_shape_mismatch() {
        let note = policy_preview_note(
            "sandbox",
            &serde_json::json!("not-an-object"),
            &serde_json::json!({"enabled": true}),
        );
        assert!(
            note.contains("not a JSON object"),
            "expected shape-mismatch warning, got: {note}"
        );
        assert!(note.contains("replaced instead"));
    }

    #[test]
    fn preview_note_for_unknown_key_warns_user() {
        let note = policy_preview_note(
            "notARealKey",
            &serde_json::json!({"a": 1}),
            &serde_json::json!({"b": 2}),
        );
        assert!(note.contains("Unknown key"));
        assert!(note.contains("replaced"));
    }

    #[test]
    fn move_key_replaces_override_only_hooks() {
        // hooks is override-only per Claude Code's docs: the highest-precedence
        // scope wins, scopes are not merged. Moving hooks from project into
        // user must overwrite the destination's existing hooks block, not
        // merge into it.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"hooks": {"PreToolUse": [{"command": "from-project"}]}}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"hooks": {"PostToolUse": [{"command": "from-user"}]}}"#,
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
            &serde_json::json!({"PreToolUse": [{"command": "from-project"}]})
        );
    }

    #[test]
    fn move_key_applies_structured_sandbox_merge() {
        // End-to-end check that the structured sandbox merge survives
        // serialization through apply_move_key_impl: array leaves under
        // filesystem.* and network.* union, scalar leaves replace,
        // dest-only leaves survive.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{
  "sandbox": {
    "enabled": true,
    "filesystem": {"allowWrite": ["/proj"]},
    "network": {"allowedDomains": ["*.npmjs.org"]}
  }
}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{
  "sandbox": {
    "enabled": false,
    "filesystem": {"allowWrite": ["/user"]},
    "network": {"allowedDomains": ["github.com"], "httpProxyPort": 8080}
  }
}"#,
        );
        apply_move_key_impl(
            &paths,
            &MoveKeyRequest {
                key: "sandbox".to_string(),
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
            user_doc.get_top_level("sandbox").unwrap(),
            &serde_json::json!({
                "enabled": true,
                "filesystem": {"allowWrite": ["/user", "/proj"]},
                "network": {
                    "allowedDomains": ["github.com", "*.npmjs.org"],
                    "httpProxyPort": 8080
                }
            })
        );
    }

    // -- move_leaf parity tests --------------------------------------------
    //
    // The path-based primitive replaces today's per-rule + per-key flows. The
    // tests below verify it produces the same end state as the legacy
    // commands for the canonical cases users hit today, plus the new
    // permission-list shape that wasn't expressible before. Existing per-rule
    // and per-key tests (above) stay until the legacy IPCs are retired.

    fn key(s: &str) -> PathSeg {
        PathSeg::Key(s.to_string())
    }

    fn idx(i: usize) -> PathSeg {
        PathSeg::Index(i)
    }

    #[test]
    fn move_leaf_rule_matches_apply_move_for_canonical_rule_move() {
        // Same input as `move_from_project_to_user_preserves_other_keys` but
        // through the unified primitive — end state must be identical so the
        // frontend cutover has a true behavioral regression test, not just a
        // shape sanity check.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{
  "theme": "dark",
  "permissions": { "allow": ["Bash(git status)", "Read(**)"] }
}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
            },
            &BackupTracker::new(),
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
    fn move_leaf_rule_already_present_in_dest_still_removes_source() {
        // Moving a rule the destination already has is the no-op-write case
        // — destination is unchanged, but the source must still lose its
        // copy. Matches `apply_move_impl`'s contract for the same shape.
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

        apply_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
            },
            &BackupTracker::new(),
            &WatchState::default(),
        )
        .unwrap();

        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(project_doc.permissions().allow.is_empty());
        let user_doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            user_doc.permissions().allow,
            vec!["Bash(git status)".to_string()]
        );
    }

    #[test]
    fn move_leaf_top_level_key_matches_move_key_for_env() {
        // env's KeyPolicy::DeepMerge must continue to apply through the
        // unified primitive — same destination state as `apply_move_key_impl`
        // for the same input.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"env":{"PATH":"/proj","API_KEY":"xyz"}}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"env":{"PATH":"/old","HOME":"/h"}}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("env")],
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
        // Source values win on conflict (PATH); destination-only keys (HOME)
        // are preserved; source-only keys (API_KEY) are inserted.
        assert_eq!(
            user_doc.get_top_level("env").unwrap(),
            &serde_json::json!({"PATH": "/proj", "HOME": "/h", "API_KEY": "xyz"})
        );
        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        // Source key was removed wholesale.
        assert!(project_doc.get_top_level("env").is_none());
    }

    #[test]
    fn move_leaf_permission_list_unions_into_destination() {
        // The new shape that wasn't expressible under the old IPCs: move the
        // entire `permissions.allow` array. Array-union semantics, source
        // ends up with an empty allow list (the array was removed, but the
        // permissions key itself stays since deny / ask might still be there
        // — though here they aren't).
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["a","b"],"deny":["d"]}}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["b","c"]}}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow")],
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
            *user_doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["b", "c", "a"]})
        );
        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        // The source's allow array was removed; deny survives untouched.
        assert_eq!(
            *project_doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"deny": ["d"]})
        );
    }

    #[test]
    fn diff_move_leaf_classifies_kind_and_describes_top_level_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        );

        let preview = diff_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
            },
        )
        .unwrap();
        assert!(matches!(preview.kind, MoveLeafKind::PermissionRule));
        // from.key_before / from.key_after describe the *whole permissions
        // object* on the source; the leaf was removed.
        assert_eq!(
            preview.from.key_before.unwrap(),
            serde_json::json!({"allow": ["Bash(git status)"]})
        );
        assert_eq!(
            preview.from.key_after.unwrap(),
            serde_json::json!({"allow": []})
        );
        // Destination didn't have permissions at all yet — key_before is None
        // (skipped on the wire), key_after contains the merged result.
        assert!(preview.to.key_before.is_none());
        assert_eq!(
            preview.to.key_after.unwrap(),
            serde_json::json!({"allow": ["Bash(git status)"]})
        );
        assert!(preview.to.will_write);
    }

    #[test]
    fn move_leaf_rejects_bare_permissions_path() {
        // Defense in depth — even if a buggy frontend skipped the leaf
        // affordance and asked to move `permissions` whole, the backend
        // refuses with the validation error from `validate_movable_path`.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["x"]}}"#,
        );
        let err = apply_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions")],
                from: Scope::Project,
                to: Scope::User,
            },
            &BackupTracker::new(),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("permissions key is not movable"));
    }

    #[test]
    fn move_leaf_rejects_same_scope_source_and_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{"theme":"dark"}"#);
        let err = apply_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("theme")],
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
    fn move_leaf_rejects_when_source_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{"permissions":{}}"#);
        let err = apply_move_leaf_impl(
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
            },
            &BackupTracker::new(),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
