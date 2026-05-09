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
    /// Every top-level key on disk in source order, including `permissions`.
    /// Drives the unified tree view in the UI: `env`, `hooks`, `theme`, and
    /// `permissions` all render through the same renderer with leaf-level
    /// move affordances at backend-marked movable paths
    /// (`validate_movable_path`).
    pub values: serde_json::Map<String, serde_json::Value>,
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

/// Path-based move request. The single primitive backing every move on the
/// frontend, addressable by JSON path. `validate_movable_path` (in `model`)
/// classifies each request into one of: whole top-level key, whole
/// `permissions.<kind>` array, or a single rule under `permissions.<kind>`.
/// Other intermediate sub-paths are explicitly rejected for v1; broader
/// sub-key moves (e.g. `env.PATH`) are a follow-up to issue #67.
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
        values: serde_json::Map::new(),
        parse_error: None,
    };
    let Some(p) = path else {
        return view;
    };
    match io_atomic::load(p) {
        Ok(Some(doc)) => {
            view.exists = true;
            view.values = doc.all_entries();
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
        let perms = permissions_from_values(&view.values);
        accumulate(
            &perms.allow,
            view.scope,
            &mut rules.allow,
            &mut origins.allow,
        );
        accumulate(&perms.deny, view.scope, &mut rules.deny, &mut origins.deny);
        accumulate(&perms.ask, view.scope, &mut rules.ask, &mut origins.ask);
    }
    (rules, origins)
}

/// Pull a `PermissionRules` snapshot out of a `ScopeView::values` map.
/// Mirrors `SettingsDoc::permissions` for the case where the caller already
/// has the deserialized top-level value-map in hand instead of a SettingsDoc
/// — used by `combined_permissions` so the union walk doesn't need a second
/// disk load. Missing or malformed shapes degrade to empty lists rather
/// than panic, matching the "partial corruption shouldn't break the UI"
/// stance everywhere else.
fn permissions_from_values(values: &serde_json::Map<String, serde_json::Value>) -> PermissionRules {
    let mut out = PermissionRules::default();
    let Some(perms) = values
        .get("permissions")
        .and_then(serde_json::Value::as_object)
    else {
        return out;
    };
    for kind in PermissionKind::ALL {
        if let Some(serde_json::Value::Array(items)) = perms.get(kind.key()) {
            let strs = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            match kind {
                PermissionKind::Allow => out.allow = strs,
                PermissionKind::Deny => out.deny = strs,
                PermissionKind::Ask => out.ask = strs,
            }
        }
    }
    out
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

    /// Build a synthetic `ScopeView` for the combined-panel tests below,
    /// going through the new `values` map shape — `permissions_from_values`
    /// pulls the rules back out, mirroring the load_scopes path. Triplets
    /// of empty lists are emitted as a missing `permissions` entry; the
    /// extractor degrades gracefully there (returns empty
    /// `PermissionRules`), and "no rules" matching "no key" makes the
    /// fixture closest to the realistic on-disk shape for a settings file
    /// without any permission rules.
    fn make_view(
        scope: Scope,
        exists: bool,
        allow: &[&str],
        deny: &[&str],
        ask: &[&str],
    ) -> ScopeView {
        let mut values = serde_json::Map::new();
        if !allow.is_empty() || !deny.is_empty() || !ask.is_empty() {
            values.insert(
                "permissions".into(),
                serde_json::json!({
                    "allow": allow,
                    "deny": deny,
                    "ask": ask,
                }),
            );
        }
        ScopeView {
            scope,
            path: None,
            exists,
            values,
            parse_error: None,
        }
    }

    #[test]
    fn combined_unions_across_scopes() {
        let views = vec![
            make_view(Scope::Local, true, &["Bash(git status)"], &[], &[]),
            make_view(
                Scope::Project,
                true,
                &["Bash(git status)", "Read(**)"],
                &["WebFetch(domain:evil.example)"],
                &[],
            ),
            make_view(Scope::User, false, &[], &[], &[]),
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
            make_view(Scope::Local, true, &["Bash(git push)"], &[], &[]),
            make_view(Scope::Project, true, &[], &["Bash(git push)"], &[]),
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
            make_view(
                Scope::Local,
                true,
                &["Bash(git status)", "Read(**)"],
                &[],
                &[],
            ),
            make_view(Scope::Project, true, &[], &[], &[]),
            make_view(Scope::UserLocal, true, &["Bash(git status)"], &[], &[]),
            make_view(
                Scope::User,
                true,
                &["Bash(git status)", "Bash(ls)"],
                &[],
                &[],
            ),
        ];
        let (combined, origins) = combined_permissions(&views);
        assert_eq!(
            combined.allow,
            vec!["Bash(git status)", "Read(**)", "Bash(ls)"]
        );
        assert_eq!(
            origins.allow[0],
            vec![Scope::Local, Scope::UserLocal, Scope::User]
        );
        assert_eq!(origins.allow[1], vec![Scope::Local]);
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
            make_view(
                Scope::Local,
                true,
                &["Bash(git status)", "Bash(git status)"],
                &[],
                &[],
            ),
            make_view(Scope::User, true, &["Bash(git status)"], &[], &[]),
        ];
        let (combined, origins) = combined_permissions(&views);
        assert_eq!(combined.allow, vec!["Bash(git status)"]);
        assert_eq!(origins.allow[0], vec![Scope::Local, Scope::User]);
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
        // entire `permissions.allow` array. Array-union semantics on the
        // destination; on the source `remove_at_path` deletes the `allow`
        // key from the `permissions` object entirely (it doesn't leave an
        // empty array behind), and the `permissions` key itself stays put
        // because `deny` / `ask` may still be there — verified below by
        // the surviving `deny: ["d"]`.
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
