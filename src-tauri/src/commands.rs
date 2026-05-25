//! Tauri command handlers exposed to the front-end.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use ulid::Ulid;

use crate::app_info::AppInfo;
use crate::audit;
use crate::io_atomic::{self, BackupTracker, FileStamp};
use crate::model::{
    describe_path, key_policy, validate_movable_path, KeyPolicy, MovablePath, PathSeg,
    PermissionKind, PermissionRules, SettingsDoc,
};
use crate::preferences::{self, Preferences};
use crate::projects::{self, KnownProject, ProjectsFilter};
use crate::runtime::{RuntimeInfo, RuntimeOverrides};
use crate::scope::{self, Scope, ScopePaths};
use crate::watcher::WatchState;

/// Process-global backup tracker so we only write one `.bak` per file per
/// session, regardless of which command triggered the first write.
static BACKUPS: OnceLock<BackupTracker> = OnceLock::new();

fn backups() -> &'static BackupTracker {
    BACKUPS.get_or_init(BackupTracker::new)
}

/// Resolve which backup-tracker (if any) a write command should pass into
/// the impl. Reads `Preferences::backup_on_write` on every call so a
/// toggle in the Settings dialog takes effect on the very next write
/// without needing a restart or in-memory cache invalidation. Read costs
/// one small JSON parse; negligible against the rest of the write path.
/// Defaults to `Some(backups())` when the preferences file is missing or
/// malformed — the load helper itself collapses to defaults in that
/// case (#88).
fn backups_for_session() -> Option<&'static BackupTracker> {
    if preferences::load().backup_on_write {
        Some(backups())
    } else {
        None
    }
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
    /// Pairs (or larger groups) of scopes whose resolved file paths point at
    /// the same file on disk — the common case is launching ClaudeScope
    /// from `$HOME` itself, which makes Project and User collapse onto the
    /// same `~/.claude/settings.json`. Surfacing this as a warning banner
    /// stops the user from believing "move from User to Project" is doing
    /// anything other than overwriting the file with its own contents. See
    /// #153.
    pub path_collisions: Vec<PathCollision>,
    /// Rules that appear in multiple scopes under *different* kinds — e.g.
    /// `Bash(git *)` is `allow` in User scope but `deny` in Project. The
    /// effective union picks one by precedence; the UI surfaces the
    /// disagreement so the user can spot a likely typo or stale rule
    /// without scanning every column. See #156.
    pub kind_conflicts: Vec<KindConflict>,
    /// Permission rules that are either exact duplicates or fully
    /// covered by a broader rule of the same kind (#17). The UI renders
    /// a ⚠ badge on every redundant chip with a popover naming the
    /// covering rule. Cross-kind shadowing (allow vs deny) is out of
    /// scope here — that's the kind-conflict warning's territory.
    pub redundancies: Vec<crate::redundancy::Redundancy>,
}

/// Two-or-more scopes whose resolved file paths point at the same on-disk
/// file. Surfaced by [`detect_path_collisions`] and rendered as a top-of-app
/// warning banner. See #153.
#[derive(Debug, Clone, Serialize)]
pub struct PathCollision {
    /// Scopes that share the path, in [`Scope::ALL`] order so the rendered
    /// banner reads in the same broad→narrow order as the scope columns.
    pub scopes: Vec<Scope>,
    /// The shared file path, as resolved by [`scope::resolve_with_home`].
    pub path: String,
}

/// One rule string that appears in 2+ scopes with disagreeing kinds. See
/// #156.
#[derive(Debug, Clone, Serialize)]
pub struct KindConflict {
    pub rule: String,
    /// Every (scope, kind) pairing of this rule across all four scopes, in
    /// precedence order (highest first — matches `combined_origins`). The
    /// frontend picks the highest-precedence entry as the "winner" for
    /// effective-evaluation rendering.
    pub occurrences: Vec<KindOccurrence>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct KindOccurrence {
    pub scope: Scope,
    pub kind: PermissionKind,
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
    /// Change-kind variant (#8): when set, the rule at `path` (which must
    /// classify as `MovablePath::PermissionRule`) is added to
    /// `permissions.<to_kind>` on the destination side instead of the same
    /// kind. Enables in-place allow ↔ deny ↔ ask reclassification
    /// (`from == to`) and cross-scope reclassification (`from != to`) through
    /// the same primitive. Absent on the wire ⇒ behaves identically to a
    /// pre-#8 move request, so the field is backwards-compatible.
    #[serde(default)]
    pub to_kind: Option<PermissionKind>,
}

/// Path-based delete request (#8). Removes the leaf at `path` from `scope`
/// using the same atomic-write + `.bak` plumbing as a move. The path is
/// validated through `validate_movable_path`, so only the three movable
/// shapes (top-level key, whole permission list, single permission rule) are
/// acceptable — same surface as the move primitive.
#[derive(Debug, Deserialize)]
pub struct DeleteLeafRequest {
    pub path: Vec<PathSeg>,
    pub from: Scope,
}

/// Path-based add request (#8). Inserts `value` at `path` in `scope` using
/// the existing `merge_at_path` semantics (array-union for permission rules,
/// per-key policy for top-level keys). Powers paste; v1 only exposes the
/// permission-rule shape on the frontend, but the backend accepts any
/// movable shape so future paste/import flows can reuse the primitive.
#[derive(Debug, Deserialize)]
pub struct AddLeafRequest {
    pub path: Vec<PathSeg>,
    pub to: Scope,
    pub value: serde_json::Value,
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
    /// Set when `to_kind` was supplied (#8 change-kind). The frontend uses
    /// this to label the diff modal "Change kind" instead of "Move", and to
    /// collapse the bilateral display into a single side when `from == to`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_kind: Option<PermissionKind>,
}

/// Diff preview for a delete-leaf request. Shape mirrors `MoveLeafPreview`'s
/// `from` side — there is no destination, so the wire format omits one
/// instead of carrying a duplicated null.
#[derive(Debug, Serialize)]
pub struct DeleteLeafPreview {
    pub path: Vec<PathSeg>,
    pub kind: MoveLeafKind,
    pub from: MoveLeafSide,
}

/// Diff preview for an add-leaf request. Shape mirrors `MoveLeafPreview`'s
/// `to` side — there is no source.
#[derive(Debug, Serialize)]
pub struct AddLeafPreview {
    pub path: Vec<PathSeg>,
    pub kind: MoveLeafKind,
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

/// Resolve a `(paths_from, paths_to)` pair for a move-leaf command (#179).
/// Each side honors its own optional `project_dir_*` override; an absent
/// side falls back to the single-project `project_dir` argument, which in
/// turn falls back to the runtime override or cwd walk-up. When the two
/// project dirs end up equal — the usual single-project case — only one
/// resolution is done and the resulting `ScopePaths` is cloned, so this
/// helper is zero-extra-work on the hot path.
fn resolve_move_paths(
    project_dir: Option<&str>,
    project_dir_from: Option<&str>,
    project_dir_to: Option<&str>,
    overrides: &RuntimeOverrides,
) -> std::io::Result<(ScopePaths, ScopePaths)> {
    let from_arg = project_dir_from.or(project_dir);
    let to_arg = project_dir_to.or(project_dir);
    if from_arg == to_arg {
        let paths = resolve_with_overrides(from_arg, overrides)?;
        return Ok((paths.clone(), paths));
    }
    let paths_from = resolve_with_overrides(from_arg, overrides)?;
    let paths_to = resolve_with_overrides(to_arg, overrides)?;
    Ok((paths_from, paths_to))
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
    // Promote this project to the front of the recent-projects LRU (#47).
    // Best-effort: a failure to persist preferences must not fail the load
    // itself — the user's stated request was "open this project", and the
    // dropdown's freshness is the secondary concern. Sandbox sessions (#66)
    // skip the write so a scratch run can't pollute the real config.
    if overrides.home().is_none() {
        let mut prefs = preferences::load();
        preferences::push_recent_project(&mut prefs, &paths.project_dir);
        let _ = preferences::save(&prefs);
    }
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

/// Diff-preview a move-leaf op (#8). Per-side `project_dir_from` /
/// `project_dir_to` overrides mirror `apply_move_leaf` for symmetry —
/// either absent falls back to the single-project `project_dir` (#179).
#[tauri::command]
pub fn diff_move_leaf(
    req: MoveLeafRequest,
    project_dir: Option<String>,
    project_dir_from: Option<String>,
    project_dir_to: Option<String>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<MoveLeafPreview, String> {
    let (paths_from, paths_to) = resolve_move_paths(
        project_dir.as_deref(),
        project_dir_from.as_deref(),
        project_dir_to.as_deref(),
        &overrides,
    )
    .map_err(|e| e.to_string())?;
    diff_move_leaf_impl(&paths_from, &paths_to, &req).map_err(|e| e.to_string())
}

/// Snapshot the value of a single top-level key on `path` if the file
/// exists, returning `None` for "file absent" or "key absent" alike (the
/// audit log uses `None` for both — Phase 2+ restore re-reads current
/// state before computing any delta, so distinguishing the two at log time
/// would just bloat the schema). Errors are also collapsed to `None`: an
/// I/O hiccup pre-apply shouldn't poison the post-apply audit entry.
pub fn snapshot_top_level_key(path: &Path, key: &str) -> Option<serde_json::Value> {
    let (doc, _stamp) = io_atomic::load_with_stamp(path).ok()?;
    doc.and_then(|d| d.get_top_level(key).cloned())
}

/// Audit-side values captured during an apply_*_leaf_impl invocation,
/// from the same `load_with_stamp` reads the impl uses for its writes.
///
/// Returning these from the impl (rather than having the command boundary
/// do a separate `snapshot_top_level_key`) closes the race window a
/// third-party writer could exploit by landing between the command's
/// pre-snapshot and the impl's own load — pre-fix the audit's `key_before`
/// could reflect a state the impl never actually overwrote, so a later
/// undo would silently revert past the third-party edit. See #169.
///
/// Move populates both `from_*` and `to_*`. Same-scope change-kind also
/// fills both, with identical values (one file, recorded as two sides).
/// Delete fills only `from_*`. Add fills only `to_*`.
#[derive(Debug, Default)]
pub struct LeafApplyOutcome {
    pub from_before: Option<serde_json::Value>,
    pub from_after: Option<serde_json::Value>,
    pub to_before: Option<serde_json::Value>,
    pub to_after: Option<serde_json::Value>,
}

/// Classify a movable leaf path for the audit log. Path shapes mirror
/// `validate_movable_path` — re-derived from the path itself rather than
/// running validate again, because by the time the audit-recording code
/// runs the apply impl has already validated. Keeping this classifier
/// path-driven also means the audit module doesn't pull in `MovablePath`,
/// which is a frontend-facing wire enum.
pub fn audit_leaf_kind(path: &[PathSeg]) -> audit::LeafKind {
    if path.len() == 1 {
        audit::LeafKind::TopLevelKey
    } else if path.len() == 2
        && matches!(path.first().and_then(PathSeg::as_key), Some("permissions"))
    {
        audit::LeafKind::PermissionList
    } else {
        audit::LeafKind::PermissionRule
    }
}

/// Build one [`audit::Side`] from the pre-resolved pieces. Returns `None`
/// when the scope has no file path on this machine (e.g. user scope
/// disabled on a sandboxed home), so the audit record skips the field
/// rather than emitting `"file_path":""` placeholder garbage.
pub fn audit_side(
    scope: Scope,
    file_path: Option<&Path>,
    key: &str,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
) -> Option<audit::Side> {
    let file_path = file_path?.to_path_buf();
    Some(audit::Side {
        scope,
        file_path,
        top_level_key: key.to_string(),
        key_before: before,
        key_after: after,
    })
}

/// Outcome of [`persist_audit_record`]. Rotation and append errors are
/// reported separately so the GUI can surface them as distinct Tauri
/// events and the CLI can print distinct warnings.
#[derive(Debug, Default)]
pub struct AuditPersistResult {
    /// Set when log rotation failed. Non-fatal — the append still
    /// proceeds against the over-cap log; the user just keeps writing
    /// to a log that's larger than the configured cap until rotation
    /// can land.
    pub rotate_error: Option<String>,
    /// Set when the append failed. Fatal-ish: the record is *not* in
    /// the log; history/undo/redo won't see this op. Callers surface
    /// it as a warning rather than a hard error because the primary
    /// write already succeeded.
    pub append_error: Option<String>,
}

impl AuditPersistResult {
    pub fn is_clean(&self) -> bool {
        self.rotate_error.is_none() && self.append_error.is_none()
    }
}

/// Rotate (if configured) and append `record` to the audit log at the
/// location implied by `home` (None = production, Some = sandbox / test).
///
/// Wraps [`audit::persist`], which holds a cross-process advisory lock
/// around the rotate+append pair so two ClaudeScope sessions can't
/// interleave a rotation with another session's append (#166). Reads the
/// rotation preference (`audit_log_rotate` / `audit_log_max_size_mb`)
/// fresh on every call so a toggle in Settings takes effect on the next
/// write.
///
/// Both steps are best-effort: rotation failure is logged and execution
/// proceeds to append; append failure is reported in the returned struct
/// so the caller can surface it in their idiomatic way (the GUI emits a
/// Tauri event; the CLI prints to stderr). Shared by GUI and CLI write
/// paths so the two surfaces can't drift on rotation behavior — see
/// #164 and #168.
pub fn persist_audit_record(record: &audit::Record, home: Option<&Path>) -> AuditPersistResult {
    let prefs = preferences::load();
    let options = audit::PersistOptions {
        rotate_cap_bytes: if prefs.audit_log_rotate {
            Some(u64::from(prefs.audit_log_max_size_mb) * 1_000_000)
        } else {
            None
        },
    };
    let result = audit::persist(record, home, options);
    AuditPersistResult {
        rotate_error: result.rotate_error,
        append_error: result.append_error.map(|e| e.to_string()),
    }
}

/// Append one entry to the audit log on a successful GUI write. Fail-open:
/// any error from rotation or append surfaces as a Tauri event but never
/// as a hard failure to the caller — the primary write already succeeded,
/// and the user's stated intent (move / add / delete this rule) took
/// effect. Sandbox sessions (`--home` override active) skip the write
/// entirely so scratch-mode runs don't pollute the real
/// `~/.claude/claude-scope/`.
fn emit_audit(app: &AppHandle, overrides: &RuntimeOverrides, record: audit::Record) {
    if overrides.home().is_some() {
        return;
    }
    let outcome = persist_audit_record(&record, None);
    if let Some(err) = outcome.rotate_error {
        let _ = app.emit("audit-error", err);
    }
    if let Some(err) = outcome.append_error {
        let _ = app.emit("audit-error", err);
    }
}

/// Apply a move-leaf op (#8). When `project_dir_from` / `project_dir_to`
/// are set, the matching side's [`ScopePaths`] resolves under that root
/// instead of the single-project `project_dir` — used by the Move-to
/// submenu's cross-project branch (#111 / #181), where a move from the
/// current project's Local into OtherProject's Local sets the two
/// overrides to the two project roots. Either field absent (`None`) falls
/// back to `project_dir`, so existing single-project callers keep their
/// IPC shape unchanged (#179).
#[tauri::command]
pub fn apply_move_leaf(
    req: MoveLeafRequest,
    project_dir: Option<String>,
    project_dir_from: Option<String>,
    project_dir_to: Option<String>,
    app: AppHandle,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    let (paths_from, paths_to) = resolve_move_paths(
        project_dir.as_deref(),
        project_dir_from.as_deref(),
        project_dir_to.as_deref(),
        &overrides,
    )
    .map_err(|e| e.to_string())?;

    // Validate the path shape before any helper that assumes "path[0] is a
    // key" — path_top_level_key panics on a malformed payload, and a panic
    // across the Tauri FFI boundary becomes an opaque error instead of the
    // typed validation message that lives one frame deeper (#174).
    validate_movable_path(&req.path).map_err(|e| e.to_string())?;
    let key = path_top_level_key(&req.path).to_string();
    let from_path = paths_from.path_for(req.from).map(Path::to_path_buf);
    let to_path = paths_to.path_for(req.to).map(Path::to_path_buf);

    // The impl returns the before/after values it captured from its own
    // load_with_stamp — closing the race window a separate pre-snapshot
    // at this boundary would leave open against a third-party writer
    // (#169).
    let outcome = apply_move_leaf_impl(&paths_from, &paths_to, &req, backups_for_session(), &watch)
        .map_err(|e| e.to_string())?;

    // Same-scope move with `to_kind` set is the "change kind" flow (#8);
    // recorded as its own audit kind so a History reader (#19 Phase 2)
    // can label it correctly instead of presenting a confusing
    // "Move project → project" line.
    let kind = if req.from == req.to && req.to_kind.is_some() {
        audit::Kind::ChangeKind
    } else {
        audit::Kind::Move
    };
    // Audit metadata splits on cross-project: `project_dir` holds the
    // source root (or the only root for single-project ops);
    // `project_dir_to` holds the destination root only when it actually
    // differs from the source — single-project moves leave the field at
    // its default `None`, keeping wire compatibility with pre-#179
    // records.
    let audit_project_dir_from = project_dir_from
        .as_ref()
        .or(project_dir.as_ref())
        .map(PathBuf::from);
    let audit_project_dir_to_raw = project_dir_to
        .as_ref()
        .or(project_dir.as_ref())
        .map(PathBuf::from);
    let audit_project_dir_to = match (&audit_project_dir_from, &audit_project_dir_to_raw) {
        (Some(f), Some(t)) if f == t => None,
        (_, t) => t.clone(),
    };
    let record = audit::Record::new(
        kind,
        audit_leaf_kind(&req.path),
        audit::Actor::Gui,
        audit_project_dir_from,
        audit_side(
            req.from,
            from_path.as_deref(),
            &key,
            outcome.from_before,
            outcome.from_after,
        ),
        audit_side(
            req.to,
            to_path.as_deref(),
            &key,
            outcome.to_before,
            outcome.to_after,
        ),
        req.path.clone(),
        req.to_kind,
    )
    .with_project_dir_to(audit_project_dir_to);
    emit_audit(&app, &overrides, record);
    Ok(())
}

#[tauri::command]
pub fn diff_delete_leaf(
    req: DeleteLeafRequest,
    project_dir: Option<String>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<DeleteLeafPreview, String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    diff_delete_leaf_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_delete_leaf(
    req: DeleteLeafRequest,
    project_dir: Option<String>,
    app: AppHandle,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;

    // See #174 — validate before any helper that assumes path[0] is a key.
    validate_movable_path(&req.path).map_err(|e| e.to_string())?;
    let key = path_top_level_key(&req.path).to_string();
    let from_path = paths.path_for(req.from).map(Path::to_path_buf);

    let outcome = apply_delete_leaf_impl(&paths, &req, backups_for_session(), &watch)
        .map_err(|e| e.to_string())?;

    let record = audit::Record::new(
        audit::Kind::Delete,
        audit_leaf_kind(&req.path),
        audit::Actor::Gui,
        project_dir.as_ref().map(PathBuf::from),
        audit_side(
            req.from,
            from_path.as_deref(),
            &key,
            outcome.from_before,
            outcome.from_after,
        ),
        None,
        req.path.clone(),
        None,
    );
    emit_audit(&app, &overrides, record);
    Ok(())
}

#[tauri::command]
pub fn diff_add_leaf(
    req: AddLeafRequest,
    project_dir: Option<String>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<AddLeafPreview, String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;
    diff_add_leaf_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_add_leaf(
    req: AddLeafRequest,
    project_dir: Option<String>,
    app: AppHandle,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    let paths =
        resolve_with_overrides(project_dir.as_deref(), &overrides).map_err(|e| e.to_string())?;

    // See #174 — validate before any helper that assumes path[0] is a key.
    validate_movable_path(&req.path).map_err(|e| e.to_string())?;
    let key = path_top_level_key(&req.path).to_string();
    let to_path = paths.path_for(req.to).map(Path::to_path_buf);

    let outcome = apply_add_leaf_impl(&paths, &req, backups_for_session(), &watch)
        .map_err(|e| e.to_string())?;

    // Idempotent add: the impl returns the same before/after, meaning
    // nothing was written. Skip the audit append so the History view
    // doesn't show a phantom Add row that an Undo would treat as the
    // next undoable op while doing nothing on disk (#172).
    if outcome.to_before == outcome.to_after {
        return Ok(());
    }

    let record = audit::Record::new(
        audit::Kind::Add,
        audit_leaf_kind(&req.path),
        audit::Actor::Gui,
        project_dir.as_ref().map(PathBuf::from),
        None,
        audit_side(
            req.to,
            to_path.as_deref(),
            &key,
            outcome.to_before,
            outcome.to_after,
        ),
        req.path.clone(),
        None,
    );
    emit_audit(&app, &overrides, record);
    Ok(())
}

/// Snapshot of the launch-time overrides — the front-end uses this to
/// render a persistent sandbox banner under the toolbar so the user can't
/// forget they're in scratch mode.
#[tauri::command]
pub fn load_runtime_info(overrides: State<'_, RuntimeOverrides>) -> RuntimeInfo {
    RuntimeInfo::from_overrides(&overrides)
}

/// Hardcoded recency window for the Move-to submenu filter (#111). 90 days
/// is a generous cutoff — the user's "I came back to this project last
/// quarter" case still makes the list, but a one-off exploration from
/// six months ago drops out. Lives in `commands.rs` rather than
/// `projects.rs` so the pure discovery function stays policy-free and
/// the default policy is one obvious place to revisit.
const DEFAULT_PROJECT_RECENCY_DAYS: u32 = 90;

/// Hardcoded minimum session count for the Move-to submenu filter
/// (#111). `2` drops projects that have exactly one transcript — usually
/// a one-shot "let me ask Claude something" exploration — without
/// affecting any project the user has actually returned to.
const DEFAULT_PROJECT_MIN_SESSIONS: u32 = 2;

/// Build the effective filter for `list_known_projects`. Any field the
/// frontend left unset (or set to an empty keyword) falls back to the
/// hardcoded default for that dimension, so the silent baseline applies
/// to every call but the user's typed keyword still composes on top.
fn project_filter_with_defaults(req: Option<ProjectsFilter>) -> ProjectsFilter {
    let req = req.unwrap_or_default();
    ProjectsFilter {
        recency_days: req.recency_days.or(Some(DEFAULT_PROJECT_RECENCY_DAYS)),
        min_sessions: req.min_sessions.or(Some(DEFAULT_PROJECT_MIN_SESSIONS)),
        // Normalize an empty keyword to `None` — the projects-layer
        // filter already treats `Some("")` as a no-op, but normalizing
        // here keeps the wire / logs cleaner and gives the frontend a
        // single shape to send ("" when no input).
        keyword: req.keyword.filter(|k| !k.is_empty()),
    }
}

/// Enumerate every Claude project on this machine for the Move-to submenu
/// (#106). Routes through `RuntimeOverrides::home` so sandbox mode (#66)
/// reads from the scratch home instead of the real `~/.claude/projects/`.
///
/// `filter` (#111) narrows the list. Defaults apply per-field — the
/// frontend can leave any dimension unset and the silent baseline
/// (`DEFAULT_PROJECT_RECENCY_DAYS` / `DEFAULT_PROJECT_MIN_SESSIONS`)
/// takes over, while a typed keyword still composes on top.
#[tauri::command]
pub fn list_known_projects(
    overrides: State<'_, RuntimeOverrides>,
    filter: Option<ProjectsFilter>,
) -> Result<Vec<KnownProject>, String> {
    let effective = project_filter_with_defaults(filter);
    projects::list_known_projects(overrides.home(), &effective).map_err(|e| e.to_string())
}

/// One row in the History view's audit-log list (#19 phase 2). Wraps an
/// [`audit::Record`] with a derived `ts_ms` field so the frontend doesn't
/// need to ship its own ULID parser to render timestamps. The 48-bit
/// timestamp is embedded in the first half of the ULID; pulling it out
/// here keeps that decode in one place.
#[derive(Debug, Serialize)]
pub struct AuditRecordView {
    #[serde(flatten)]
    pub record: audit::Record,
    /// Unix millis-since-epoch, decoded from the record's ULID.
    pub ts_ms: u64,
}

/// Payload for `list_audit_records`: the records plus a count of any
/// malformed lines the reader skipped. `skipped > 0` is non-fatal — the
/// History view surfaces it as a small footer warning so the user knows
/// some entries are unreadable, but the rest of the log still renders.
#[derive(Debug, Serialize)]
pub struct AuditLogPage {
    pub records: Vec<AuditRecordView>,
    pub skipped: usize,
}

/// Read the audit log for the History view (#19 phase 2). Routes through
/// `RuntimeOverrides::home` so sandbox sessions (#66) and unit tests read
/// from their scratch home rather than the real `~/.claude/`. A missing
/// file returns an empty page — fresh installs and users who haven't
/// made any writes yet shouldn't see an error here.
///
/// The records are returned in file order (append order, which matches
/// chronological order thanks to ULIDs). The frontend reverses for
/// display.
#[tauri::command]
pub fn list_audit_records(overrides: State<'_, RuntimeOverrides>) -> Result<AuditLogPage, String> {
    let (records, skipped) = audit::read_all(overrides.home()).map_err(|e| e.to_string())?;
    let records = records.into_iter().map(record_view).collect();
    Ok(AuditLogPage { records, skipped })
}

/// Undo / redo availability for the topbar buttons (#19 Phase 3). `undo` /
/// `redo` carry the audit entry the next click would act on so the frontend
/// can render a descriptive tooltip with the same helpers the History view
/// uses; `sequence_break` lets it explain a disabled redo button.
#[derive(Debug, Serialize)]
pub struct UndoRedoStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo: Option<AuditRecordView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redo: Option<AuditRecordView>,
    pub sequence_break: bool,
    /// Count of unreadable audit-log lines (`read_all` skip count). When
    /// non-zero, the undo/redo state machine is missing some records —
    /// a corrupt `restore` line could make us think an op is still
    /// undoable when it isn't. The topbar surfaces this and refuses
    /// undo/redo until the log is repaired. See #170.
    pub skipped: usize,
}

/// Report what the next undo / redo would target (#19 Phase 3). Routes
/// through `RuntimeOverrides::home` like the other audit commands, so a
/// sandbox session reads its scratch log. A sandbox log is always empty
/// (writes there are never logged), so undo / redo are simply unavailable
/// in scratch mode — `.bak` remains the recovery path there.
#[tauri::command]
pub fn audit_undo_status(overrides: State<'_, RuntimeOverrides>) -> Result<UndoRedoStatus, String> {
    let (records, skipped) = audit::read_all(overrides.home()).map_err(|e| e.to_string())?;
    let state = audit::undo_redo_state(&records);
    // When the log has malformed lines, withhold undo/redo: a corrupt
    // restore record could leave the state machine pointing at an op
    // that was already undone, and confirming would emit a duplicate
    // restore entry. The topbar surfaces `skipped > 0` as a warning
    // and disables the buttons. See #170.
    let degraded = skipped > 0;
    Ok(UndoRedoStatus {
        undo: if degraded {
            None
        } else {
            state.undoable.map(record_view)
        },
        redo: if degraded {
            None
        } else {
            state.redoable.map(record_view)
        },
        sequence_break: state.sequence_break,
        skipped,
    })
}

/// Resolve the audit entry the next undo or redo would act on, or a
/// descriptive error. Shared by the preview and apply commands so they
/// can't drift on which entry is the target.
fn undo_redo_target(
    direction: audit::RestoreDirection,
    overrides: &RuntimeOverrides,
) -> Result<audit::Record, Box<dyn std::error::Error>> {
    let records = require_clean_audit_log(overrides)?;
    // Audit log boundary check — refuse any record whose Side points
    // at a path outside the legitimate scope-file allowlist for its
    // project_dir (#183). A hostile log line can otherwise drive
    // `apply_restore_plan` to write attacker JSON to any user-writable
    // path.
    validate_audit_records(&records, overrides.home())?;
    let state = audit::undo_redo_state(&records);
    let target = match direction {
        audit::RestoreDirection::Undo => state.undoable,
        audit::RestoreDirection::Redo => {
            if state.sequence_break {
                return Err("redo is unavailable: a change was made after the last undo".into());
            }
            state.redoable
        }
        audit::RestoreDirection::ToPoint => {
            return Err("restore-to-point is not an undo/redo target".into())
        }
    };
    target.ok_or_else(|| -> Box<dyn std::error::Error> {
        match direction {
            audit::RestoreDirection::Undo => "nothing to undo".into(),
            _ => "nothing to redo".into(),
        }
    })
}

/// Read the audit log, refusing if any records were skipped. Shared
/// gate for every undo/redo/restore-to-point GUI command path: an IPC
/// caller that bypassed the topbar's pre-check shouldn't be able to
/// drive the state machine against a partial log either (#170 extended
/// per codex's second-pass finding). The CLI has its own equivalent in
/// `read_audit_log`.
fn require_clean_audit_log(
    overrides: &RuntimeOverrides,
) -> Result<Vec<audit::Record>, Box<dyn std::error::Error>> {
    let (records, skipped) = audit::read_all(overrides.home())?;
    if skipped > 0 {
        return Err(format!(
            "{skipped} audit-log {} unreadable — refusing to compute \
             undo/redo against a partial log. Repair {} (or copy aside \
             and reset) before retrying.",
            if skipped == 1 {
                "entry is"
            } else {
                "entries are"
            },
            if skipped == 1 {
                "the entry"
            } else {
                "those entries"
            }
        )
        .into());
    }
    Ok(records)
}

fn undo_redo_preview(
    direction: audit::RestoreDirection,
    overrides: &RuntimeOverrides,
) -> Result<RestorePreview, Box<dyn std::error::Error>> {
    let target = undo_redo_target(direction, overrides)?;
    let plan = build_restore_plan(&target, direction)?;
    preview_restore_plan(&plan, &target)
}

/// Diff preview for the next undo (#19 Phase 3) — same modal the move /
/// delete / add flows confirm through.
#[tauri::command]
pub fn audit_undo_preview(
    overrides: State<'_, RuntimeOverrides>,
) -> Result<RestorePreview, String> {
    undo_redo_preview(audit::RestoreDirection::Undo, &overrides).map_err(|e| e.to_string())
}

/// Diff preview for the next redo (#19 Phase 3).
#[tauri::command]
pub fn audit_redo_preview(
    overrides: State<'_, RuntimeOverrides>,
) -> Result<RestorePreview, String> {
    undo_redo_preview(audit::RestoreDirection::Redo, &overrides).map_err(|e| e.to_string())
}

fn apply_undo_redo(
    expected_id: &str,
    direction: audit::RestoreDirection,
    app: &AppHandle,
    watch: &WatchState,
    overrides: &RuntimeOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let target = undo_redo_target(direction, overrides)?;
    // The frontend confirmed a preview built from a specific entry; if the
    // log moved under us (a concurrent CLI write, an external rotation)
    // refuse rather than silently acting on a different op.
    if target.id.to_string() != expected_id {
        return Err("the audit log changed since the preview — reload and try again".into());
    }
    let plan = build_restore_plan(&target, direction)?;
    let files = apply_restore_plan(&plan, backups_for_session(), watch)?;
    emit_audit(
        app,
        overrides,
        restore_record(&plan, audit::Actor::Gui, files),
    );
    Ok(())
}

/// Apply the next undo (#19 Phase 3). `expected_id` is the id of the entry
/// the confirmed preview was built from — see [`apply_undo_redo`].
#[tauri::command]
pub fn audit_apply_undo(
    expected_id: String,
    app: AppHandle,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    apply_undo_redo(
        &expected_id,
        audit::RestoreDirection::Undo,
        &app,
        &watch,
        &overrides,
    )
    .map_err(|e| e.to_string())
}

/// Apply the next redo (#19 Phase 3).
#[tauri::command]
pub fn audit_apply_redo(
    expected_id: String,
    app: AppHandle,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    apply_undo_redo(
        &expected_id,
        audit::RestoreDirection::Redo,
        &app,
        &watch,
        &overrides,
    )
    .map_err(|e| e.to_string())
}

/// Parse a ULID string from the frontend, mapping a decode failure to a
/// readable error rather than the `ulid` crate's terse one.
fn parse_audit_id(s: &str) -> Result<Ulid, Box<dyn std::error::Error>> {
    Ulid::from_string(s).map_err(|e| format!("invalid audit entry id `{s}`: {e}").into())
}

fn restore_to_point_preview(
    target_id: &str,
    overrides: &RuntimeOverrides,
) -> Result<RestorePreview, Box<dyn std::error::Error>> {
    let id = parse_audit_id(target_id)?;
    let records = require_clean_audit_log(overrides)?;
    // Validate the entire window before building the plan (#183). Each
    // record in `[target..]` could reference a different project_dir;
    // the validator checks each against its own allowlist.
    let target_idx = records
        .iter()
        .position(|r| r.id == id)
        .ok_or("target audit entry not found in the log")?;
    validate_audit_records(&records[target_idx..], overrides.home())?;
    let plan = plan_restore_to(&records, id)?;
    let target = records
        .iter()
        .find(|r| r.id == id)
        .expect("plan_restore_to already verified the id is in the log");
    let mut preview = preview_restore_plan(&plan, target)?;
    // Capture the trailing ULID so the apply can detect a concurrent
    // append that left `ops_spanned` coincidentally identical (#171).
    preview.tail_id = records.last().map(|r| r.id);
    Ok(preview)
}

/// Diff preview for a restore-to-point (#19 Phase 4) — rolls the affected
/// files back to their state before `target_id` was written.
#[tauri::command]
pub fn audit_restore_to_point_preview(
    target_id: String,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<RestorePreview, String> {
    restore_to_point_preview(&target_id, &overrides).map_err(|e| e.to_string())
}

fn apply_restore_to_point(
    target_id: &str,
    expected_ops_spanned: usize,
    expected_tail_id: Option<Ulid>,
    app: &AppHandle,
    watch: &WatchState,
    overrides: &RuntimeOverrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let id = parse_audit_id(target_id)?;
    let records = require_clean_audit_log(overrides)?;
    // Allowlist gate on the window — see `restore_to_point_preview`.
    // Mirrored at apply time so a malicious record appended between
    // preview and apply also fails the security check (#183).
    let target_idx_for_validate = records
        .iter()
        .position(|r| r.id == id)
        .ok_or("target audit entry not found in the log")?;
    validate_audit_records(&records[target_idx_for_validate..], overrides.home())?;
    // Identity check first: a concurrent rotate+append can leave the log
    // with a same-length-but-different-records window (codex #166 +
    // #171). Compare the trailing ULID against the preview's snapshot.
    // None on either side means "no tail recorded" — treat the absence
    // as a non-mismatch only when both are absent.
    let current_tail = records.last().map(|r| r.id);
    if current_tail != expected_tail_id {
        return Err("the audit log changed since the preview — reload and try again".into());
    }
    let plan = plan_restore_to(&records, id)?;
    // Spanned-count check is now redundant with the tail check above —
    // but keep it as defense in depth in case `expected_tail_id` is
    // None for any future reason (callers that haven't been updated).
    if plan.ops_spanned != expected_ops_spanned {
        return Err("the audit log changed since the preview — reload and try again".into());
    }
    let files = apply_restore_plan(&plan, backups_for_session(), watch)?;
    emit_audit(
        app,
        overrides,
        restore_record(&plan, audit::Actor::Gui, files),
    );
    Ok(())
}

/// Apply a restore-to-point (#19 Phase 4). `expected_ops_spanned` and
/// `expected_tail_id` are the span and trailing-record ULID that the
/// confirmed preview reported — see [`apply_restore_to_point`].
#[tauri::command]
pub fn audit_apply_restore_to_point(
    target_id: String,
    expected_ops_spanned: usize,
    expected_tail_id: Option<Ulid>,
    app: AppHandle,
    watch: State<'_, WatchState>,
    overrides: State<'_, RuntimeOverrides>,
) -> Result<(), String> {
    apply_restore_to_point(
        &target_id,
        expected_ops_spanned,
        expected_tail_id,
        &app,
        &watch,
        &overrides,
    )
    .map_err(|e| e.to_string())
}

/// Build- and runtime-time diagnostic block for the About dialog (#21).
/// `webview_version` is queried at command time — it's the one field that
/// can vary across processes (a system WebView2 update mid-session) and
/// the only one that needs a live Tauri context.
#[tauri::command]
pub fn get_app_info() -> AppInfo {
    AppInfo::build(tauri::webview_version().ok())
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

pub fn build_loaded(paths: &ScopePaths) -> Result<LoadedScopes, Box<dyn std::error::Error>> {
    let mut views = Vec::with_capacity(Scope::ALL.len());

    for scope in Scope::ALL {
        views.push(load_scope_view(scope, paths.path_for(scope)));
    }

    let (combined, origins) = combined_permissions(&views);
    let path_collisions = detect_path_collisions(paths);
    let kind_conflicts = detect_kind_conflicts(&views);
    let redundancies = crate::redundancy::detect_redundancies(&views);

    Ok(LoadedScopes {
        project_dir: paths.project_dir.display().to_string(),
        scopes: views,
        combined_permissions: combined,
        combined_origins: origins,
        path_collisions,
        kind_conflicts,
        redundancies,
    })
}

/// Detect scopes whose resolved file paths point at the same on-disk file
/// (#153). The common case is launching ClaudeScope with the project root
/// equal to `$HOME` — Project and User then both resolve to
/// `~/.claude/settings.json`, and so do Local and UserLocal. The two
/// columns are still rendered, but every move between them is a no-op
/// against the same file; surfacing the collision lets the user notice
/// before relying on the columns as if they were independent.
///
/// Comparison is byte-equal on the `PathBuf`s `resolve_with_home` produced.
/// That's enough for the only collision shape this catches: project_dir
/// landing on home, which generates the same lexical path string from both
/// sides of the resolver. Canonicalize-if-exists would defend against an
/// exotic case (one scope's symlink resolving differently from another's
/// non-symlink view), but that case isn't observed in the wild and the
/// existing `dirs::home_dir()` / `find_project_root` pipeline never emits
/// such a pair anyway.
pub fn detect_path_collisions(paths: &ScopePaths) -> Vec<PathCollision> {
    let mut groups: std::collections::BTreeMap<PathBuf, Vec<Scope>> =
        std::collections::BTreeMap::new();
    for scope in Scope::ALL {
        if let Some(p) = paths.path_for(scope) {
            groups.entry(p.to_path_buf()).or_default().push(scope);
        }
    }
    let mut out = Vec::new();
    for (path, mut scopes) in groups {
        if scopes.len() < 2 {
            continue;
        }
        // Sort by Scope::ALL order so the banner reads broad→narrow,
        // matching the column layout. BTreeMap gave us path-key sort, not
        // scope sort, so this is the meaningful ordering step.
        scopes.sort_by_key(|s| Scope::ALL.iter().position(|x| x == s).unwrap_or(usize::MAX));
        out.push(PathCollision {
            scopes,
            path: path.display().to_string(),
        });
    }
    out
}

/// Detect rules that appear in 2+ scopes with disagreeing kinds (#156). A
/// rule string is in conflict iff it lands in at least one `allow` list AND
/// at least one `deny` or `ask` list across the four scopes (any pair of
/// different kinds suffices; the case shape is two `allow`s and one `deny`
/// of the same rule, etc.).
///
/// Returns one [`KindConflict`] per conflicting rule, with occurrences
/// listed in [`Scope::ALL`] precedence order (highest first) so the
/// frontend can render "X wins by precedence" against the head entry
/// without re-sorting. A rule that appears multiple times in the same
/// (scope, kind) bucket — possible from a hand-edited duplicate —
/// collapses to a single occurrence; the duplicate-detection issue (#17)
/// is the right surface for "you've got two copies of the same rule
/// here", not this one.
fn detect_kind_conflicts(views: &[ScopeView]) -> Vec<KindConflict> {
    // Preserve first-seen rule order so the rendered list stays stable
    // across loads. `views` is in `Scope::ALL` (precedence) order, so the
    // first scope to mention a rule sets its slot here, and subsequent
    // scopes append to that slot.
    let mut order: Vec<String> = Vec::new();
    let mut by_rule: std::collections::HashMap<String, Vec<KindOccurrence>> =
        std::collections::HashMap::new();
    for view in views {
        let perms = permissions_from_values(&view.values);
        for (kind, rules) in [
            (PermissionKind::Allow, &perms.allow),
            (PermissionKind::Deny, &perms.deny),
            (PermissionKind::Ask, &perms.ask),
        ] {
            for rule in rules {
                let entry = by_rule.entry(rule.clone()).or_insert_with(|| {
                    order.push(rule.clone());
                    Vec::new()
                });
                // Collapse duplicates within the same (scope, kind) bucket;
                // see fn-level rationale.
                if !entry
                    .iter()
                    .any(|o| o.scope == view.scope && o.kind == kind)
                {
                    entry.push(KindOccurrence {
                        scope: view.scope,
                        kind,
                    });
                }
            }
        }
    }
    let mut out = Vec::new();
    for rule in order {
        let occurrences = by_rule.remove(&rule).expect("seeded above");
        let distinct_kinds: std::collections::HashSet<PermissionKind> =
            occurrences.iter().map(|o| o.kind).collect();
        if distinct_kinds.len() >= 2 {
            out.push(KindConflict { rule, occurrences });
        }
    }
    out
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
pub fn permissions_from_values(
    values: &serde_json::Map<String, serde_json::Value>,
) -> PermissionRules {
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
pub fn path_top_level_key(path: &[PathSeg]) -> &str {
    path.first()
        .and_then(PathSeg::as_key)
        .expect("validate_movable_path guarantees path[0] is a key")
}

/// Remove the source side of a move, dispatched by `MovablePath` so the
/// semantics match the legacy IPCs the move-leaf primitive replaced.
///
/// For `PermissionRule`, every occurrence of the rule string is removed
/// from `permissions.<kind>` (not just the indexed entry). The legacy
/// `apply_move_impl` did this via `SettingsDoc::remove_rule`, and dropping
/// it would silently leave duplicates behind in hand-edited files: the
/// destination's array-union dedupes, so the user would see "moved" while
/// a stray copy survives in the source. For `PermissionList` and
/// `TopLevelKey`, an index-based `remove_at_path` is correct (whole array
/// or whole key). Returns true iff the document was mutated; the apply
/// path treats `false` as an internal-error guard since `get_at_path`
/// already succeeded just above.
fn remove_movable_source(
    doc: &mut SettingsDoc,
    movable: &MovablePath<'_>,
    src_value: &serde_json::Value,
    path: &[PathSeg],
) -> bool {
    match movable {
        MovablePath::PermissionRule(kind, _) => match src_value.as_str() {
            // `validate_movable_path` only classifies a path as
            // `PermissionRule` when its index segment is in-range syntactically;
            // the actual leaf shape is checked here at use time so a
            // hand-edited array with a non-string at the index still has the
            // index removed via the slower `remove_at_path` fallback.
            Some(rule) => remove_all_rule_occurrences(doc, *kind, rule),
            None => doc.remove_at_path(path),
        },
        MovablePath::PermissionList(_) | MovablePath::TopLevelKey(_) => doc.remove_at_path(path),
    }
}

/// Remove every occurrence of `rule` from `permissions.<kind>`. Resurrects
/// the legacy `SettingsDoc::remove_rule` semantics specifically for the
/// move-leaf source side; see `remove_movable_source` for why a hand-edited
/// duplicate would otherwise strand. Returns true iff at least one entry
/// was removed.
///
/// Implementation: locate the next matching index, remove it, repeat. The
/// re-locate per iteration is fine since rule arrays are tiny in practice
/// (tens of entries, not thousands), and using the existing
/// `remove_at_path` keeps the mutation API surface on `SettingsDoc`
/// minimal.
fn remove_all_rule_occurrences(doc: &mut SettingsDoc, kind: PermissionKind, rule: &str) -> bool {
    let mut removed = false;
    while let Some(idx) = find_rule_index(doc, kind, rule) {
        let did_remove = doc.remove_at_path(&[
            PathSeg::Key("permissions".into()),
            PathSeg::Key(kind.key().into()),
            PathSeg::Index(idx),
        ]);
        if !did_remove {
            // Defensive: `find_rule_index` saw the value at `idx`, so the
            // remove ought to succeed. Bail out of the loop rather than
            // spin forever if some future refactor breaks that invariant.
            break;
        }
        removed = true;
    }
    removed
}

/// Locate the first index in `permissions.<kind>` that matches `rule`, or
/// `None` if no entry matches. Used by `remove_all_rule_occurrences` to
/// drive its index-by-index removal loop without introducing a second
/// public mutation API on `SettingsDoc`.
fn find_rule_index(doc: &SettingsDoc, kind: PermissionKind, rule: &str) -> Option<usize> {
    let arr = doc.get_at_path(&[
        PathSeg::Key("permissions".into()),
        PathSeg::Key(kind.key().into()),
    ])?;
    arr.as_array()?
        .iter()
        .position(|v| v.as_str() == Some(rule))
}

/// Compute the destination path for a move-leaf request. Identical to the
/// source path unless `to_kind` is set (#8 change-kind), in which case the
/// permission-kind segment is swapped so the merge step lands the rule under
/// `permissions.<to_kind>` instead of `permissions.<from_kind>`. The index
/// segment is preserved because `merge_at_path` ignores it for
/// `PermissionRule` (the destination array order is independent), and
/// preserving it keeps the path classifying as `PermissionRule` in
/// `validate_movable_path`.
fn dest_path_for(req: &MoveLeafRequest) -> Vec<PathSeg> {
    match req.to_kind {
        None => req.path.clone(),
        Some(new_kind) => {
            let mut p = req.path.clone();
            if let Some(seg) = p.get_mut(1) {
                *seg = PathSeg::Key(new_kind.key().to_string());
            }
            p
        }
    }
}

/// Validate a move-leaf request before any disk work. Centralizes the
/// scope-equality and to_kind compatibility rules so the diff and apply
/// paths can't drift. `to_kind` is only valid on permission-rule paths;
/// scope equality is only allowed when `to_kind` is set and changes the
/// effective kind.
fn validate_move_request(
    req: &MoveLeafRequest,
    movable: &MovablePath<'_>,
    paths_from: &ScopePaths,
    paths_to: &ScopePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let from_path = paths_from.path_for(req.from);
    let to_path = paths_to.path_for(req.to);

    // Path-collision check (#153 + #179). Two move shapes collapse to a
    // file-overwrites-itself no-op:
    //   - Cross-scope-name same-file: project + user resolve to the same
    //     `~/.claude/settings.json` when the project root is $HOME (#153).
    //   - Same-scope-name same-project: the legacy "move local to local"
    //     within one project, where validate has always rejected because
    //     there's no meaningful destination distinct from the source.
    //
    // The two cases collapse into one check: if the resolved source and
    // destination paths match, reject. Same-scope change-kind
    // (`req.from == req.to` with `to_kind` set) skips this gate — the op
    // mutates the file's permissions object meaningfully even though the
    // source and destination files are by definition identical.
    let is_same_scope_change_kind = req.from == req.to && req.to_kind.is_some();
    if !is_same_scope_change_kind {
        if let (Some(fp), Some(tp)) = (from_path, to_path) {
            if fp == tp {
                return Err(format!(
                    "{} and {} both resolve to {} — a move between them would \
                     just overwrite the file with itself. {}",
                    req.from.label(),
                    req.to.label(),
                    fp.display(),
                    if req.from == req.to {
                        "Pick a different destination scope or use a different \
                         project for the destination side."
                    } else {
                        "Open a different project root to give the scopes \
                         distinct files."
                    },
                )
                .into());
            }
        }
    }
    match (req.to_kind, movable) {
        (Some(new_kind), MovablePath::PermissionRule(current_kind, _)) => {
            if req.from == req.to && *current_kind == new_kind {
                return Err(format!(
                    "rule already in `permissions.{}` of {}; nothing to change",
                    new_kind.key(),
                    req.from.label()
                )
                .into());
            }
            Ok(())
        }
        (Some(_), _) => Err(
            "to_kind only applies to a single permission rule path (permissions.<kind>[i])".into(),
        ),
        (None, _) => Ok(()),
    }
}

pub fn diff_move_leaf_impl(
    paths_from: &ScopePaths,
    paths_to: &ScopePaths,
    req: &MoveLeafRequest,
) -> Result<MoveLeafPreview, Box<dyn std::error::Error>> {
    let movable = validate_movable_path(&req.path)?;
    validate_move_request(req, &movable, paths_from, paths_to)?;

    // Mirror `apply_move_leaf_impl`'s dispatch: the single-file branch
    // fires when source and destination resolve to the same on-disk
    // file, not just when `req.from == req.to`. Same rationale (#179):
    // cross-project `Local → Local` is a two-file move, not a
    // change-kind.
    let same_file = match (paths_from.path_for(req.from), paths_to.path_for(req.to)) {
        (Some(fp), Some(tp)) => fp == tp,
        _ => false,
    };
    if same_file {
        return diff_change_kind_same_scope(paths_from, req, &movable);
    }

    let from_path = require_path(paths_from, req.from)?;
    let to_path = require_path(paths_to, req.to)?;
    let affected_key = path_top_level_key(&req.path);
    let dest_path = dest_path_for(req);

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
    // duplicating parent + child copies of the same JSON. Removal goes
    // through `remove_movable_source` so the preview reflects the same
    // legacy-parity semantics the apply path uses (notably: every copy of
    // a duplicated rule string disappears, not just the indexed one).
    let from_key_before = from_doc.get_top_level(affected_key).cloned();
    let mut from_after_doc = from_doc.clone();
    if !remove_movable_source(&mut from_after_doc, &movable, &src_value, &req.path) {
        // get_at_path saw the value but the removal helper didn't take it.
        // Same invariant the apply path enforces (a few lines below in
        // `apply_move_leaf_impl`); surfacing the same error here keeps the
        // diff and apply paths from drifting on what counts as a valid
        // move request.
        return Err(format!(
            "internal error: path `{}` resolved on read but failed to remove",
            describe_path(&req.path)
        )
        .into());
    }
    let from_key_after = from_after_doc.get_top_level(affected_key).cloned();

    // Derive existence from the load result rather than a separate
    // `to_path.exists()` call to avoid a TOCTOU window where a third party
    // creates / deletes the destination between the two checks. Mirrors
    // `apply_move_leaf_impl`'s `to_existed_before` shape so the preview
    // and apply paths agree on what "destination file will be created"
    // means.
    let to_doc_loaded = io_atomic::load(to_path)?;
    let to_path_exists = to_doc_loaded.is_some();
    let to_doc_before = to_doc_loaded.unwrap_or_else(SettingsDoc::empty);
    let to_key_before = to_doc_before.get_top_level(affected_key).cloned();
    let mut to_doc = to_doc_before.clone();
    to_doc.merge_at_path(&dest_path, src_value.clone())?;
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
        to_kind: req.to_kind,
    })
}

/// Diff a same-scope change-kind move (#8): the rule is reclassified within
/// one settings file. Both `from` and `to` sides reference the same file
/// with the same key_before / key_after — the frontend collapses the
/// bilateral display to a single side. Only `from.will_write` is true since
/// the apply path issues a single write.
fn diff_change_kind_same_scope(
    paths: &ScopePaths,
    req: &MoveLeafRequest,
    movable: &MovablePath<'_>,
) -> Result<MoveLeafPreview, Box<dyn std::error::Error>> {
    let path = require_path(paths, req.from)?;
    let affected_key = path_top_level_key(&req.path);
    let dest_path = dest_path_for(req);

    let doc = match io_atomic::load(path)? {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", path.display()).into()),
    };
    let src_value =
        doc.get_at_path(&req.path)
            .cloned()
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "path `{}` not found in {} {}",
                    describe_path(&req.path),
                    req.from.label(),
                    path.display()
                )
                .into()
            })?;

    let key_before = doc.get_top_level(affected_key).cloned();
    let mut after_doc = doc.clone();
    // Add to destination kind first, then remove from source kind. Order
    // matters because `remove_movable_source` for a `PermissionRule` strips
    // every occurrence of the rule string from the source kind's array;
    // adding first leaves no race on the destination kind's array.
    after_doc.merge_at_path(&dest_path, src_value.clone())?;
    if !remove_movable_source(&mut after_doc, movable, &src_value, &req.path) {
        return Err(format!(
            "internal error: path `{}` resolved on read but failed to remove",
            describe_path(&req.path)
        )
        .into());
    }
    let key_after = after_doc.get_top_level(affected_key).cloned();

    let file_path = path.display().to_string();
    let file_path_exists = path.exists();
    let note = Some(format!(
        "Reclassified within {} — written in place.",
        req.from.label()
    ));
    Ok(MoveLeafPreview {
        path: req.path.clone(),
        kind: MoveLeafKind::from_movable(movable),
        from: MoveLeafSide {
            scope: req.from,
            file_path: file_path.clone(),
            file_path_exists,
            key_before: key_before.clone(),
            key_after: key_after.clone(),
            will_write: true,
            note: note.clone(),
        },
        to: MoveLeafSide {
            scope: req.to,
            file_path,
            file_path_exists,
            key_before,
            key_after,
            will_write: false,
            note,
        },
        to_kind: req.to_kind,
    })
}

pub fn apply_move_leaf_impl(
    paths_from: &ScopePaths,
    paths_to: &ScopePaths,
    req: &MoveLeafRequest,
    backups: Option<&BackupTracker>,
    watch: &WatchState,
) -> Result<LeafApplyOutcome, Box<dyn std::error::Error>> {
    let movable = validate_movable_path(&req.path)?;
    validate_move_request(req, &movable, paths_from, paths_to)?;

    // Take the single-file change-kind branch only when source and
    // destination resolve to the same on-disk file. Pre-#179, that was
    // equivalent to `req.from == req.to`; with the cross-project split
    // (#179), `Local → Local` between two different projects is a real
    // two-file move and needs the cross-scope branch below. Use the
    // resolved paths as the discriminator rather than the scope enums
    // so the dispatch stays correct under any future `ScopePaths`
    // variation.
    let same_file = match (paths_from.path_for(req.from), paths_to.path_for(req.to)) {
        (Some(fp), Some(tp)) => fp == tp,
        _ => false,
    };
    if same_file {
        // Same-scope change-kind: one file, one write. Same-file
        // implies one project, so `paths_from` is the only meaningful
        // side.
        return apply_change_kind_same_scope(paths_from, req, &movable, backups, watch);
    }

    let from_path = require_path(paths_from, req.from)?.to_path_buf();
    let to_path = require_path(paths_to, req.to)?.to_path_buf();
    let affected_key = path_top_level_key(&req.path);
    let dest_path = dest_path_for(req);

    let (from_doc_loaded, from_stamp) = io_atomic::load_with_stamp(&from_path)?;
    let mut from_doc = match from_doc_loaded {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", from_path.display()).into()),
    };
    // Capture the source's pre-mutation top-level-key value for the
    // audit record. From this load — not a separate snapshot at the
    // command boundary — so a third-party writer that lands between
    // the command and this point can't leave the audit log claiming a
    // value the impl never overwrote (#169).
    let from_key_before = from_doc.get_top_level(affected_key).cloned();
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
    to_doc.merge_at_path(&dest_path, src_value.clone())?;
    let to_key_after = to_doc.get_top_level(affected_key).cloned();
    let dest_mutated = to_key_before != to_key_after;

    // Source-side removal goes through `remove_movable_source` so a
    // PermissionRule move drops every copy of the rule string (matching
    // the legacy `remove_rule` behavior), not just the indexed entry —
    // otherwise hand-edited duplicates would survive in the source while
    // the destination's array-union dedupes, leaving a stale copy behind.
    let removed = remove_movable_source(&mut from_doc, &movable, &src_value, &req.path);
    if !removed {
        // get_at_path saw the value but the helper didn't take it — should
        // be unreachable for any path that just resolved, but error loudly
        // so a future regression can't silently duplicate the rule across
        // both scopes.
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
    let from_key_after = from_doc.get_top_level(affected_key).cloned();
    Ok(LeafApplyOutcome {
        from_before: from_key_before,
        from_after: from_key_after,
        to_before: to_key_before,
        to_after: to_key_after,
    })
}

/// Apply a same-scope change-kind move (#8): one file, one save. Avoids the
/// cross-scope rollback dance entirely — both add and remove apply to the
/// same in-memory doc before we hit disk.
fn apply_change_kind_same_scope(
    paths: &ScopePaths,
    req: &MoveLeafRequest,
    movable: &MovablePath<'_>,
    backups: Option<&BackupTracker>,
    watch: &WatchState,
) -> Result<LeafApplyOutcome, Box<dyn std::error::Error>> {
    let path = require_path(paths, req.from)?.to_path_buf();
    let affected_key = path_top_level_key(&req.path);
    let dest_path = dest_path_for(req);

    let (loaded, stamp) = io_atomic::load_with_stamp(&path)?;
    let mut doc = match loaded {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", path.display()).into()),
    };
    // Same-scope change-kind: one file, but the audit record carries two
    // sides (`from` + `to`) referencing it, both pre/post the in-memory
    // mutation. Capture before/after here from the impl's own load so
    // the audit values reflect what was actually overwritten (#169).
    let key_before = doc.get_top_level(affected_key).cloned();
    let src_value =
        doc.get_at_path(&req.path)
            .cloned()
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "path `{}` not found in {} {}",
                    describe_path(&req.path),
                    req.from.label(),
                    path.display()
                )
                .into()
            })?;

    // Add to destination kind first, then strip every occurrence of the
    // rule string from the source kind. Order matters: adding first means
    // the destination kind's array always grows by exactly one (or zero, if
    // the rule was already there), regardless of how many copies of the
    // string lived in the source kind's array.
    doc.merge_at_path(&dest_path, src_value.clone())?;
    if !remove_movable_source(&mut doc, movable, &src_value, &req.path) {
        return Err(format!(
            "internal error: path `{}` resolved on read but failed to remove",
            describe_path(&req.path)
        )
        .into());
    }
    let key_after = doc.get_top_level(affected_key).cloned();

    io_atomic::save(&path, &doc, backups, Some(&stamp))?;
    watch.note_self_write(&path);
    Ok(LeafApplyOutcome {
        from_before: key_before.clone(),
        from_after: key_after.clone(),
        to_before: key_before,
        to_after: key_after,
    })
}

fn diff_delete_leaf_impl(
    paths: &ScopePaths,
    req: &DeleteLeafRequest,
) -> Result<DeleteLeafPreview, Box<dyn std::error::Error>> {
    let movable = validate_movable_path(&req.path)?;
    let path = require_path(paths, req.from)?;
    let affected_key = path_top_level_key(&req.path);

    let doc = match io_atomic::load(path)? {
        Some(d) => d,
        None => return Err(format!("file {} does not exist", path.display()).into()),
    };
    let src_value =
        doc.get_at_path(&req.path)
            .cloned()
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "path `{}` not found in {} {}",
                    describe_path(&req.path),
                    req.from.label(),
                    path.display()
                )
                .into()
            })?;

    let key_before = doc.get_top_level(affected_key).cloned();
    let mut after_doc = doc.clone();
    if !remove_movable_source(&mut after_doc, &movable, &src_value, &req.path) {
        return Err(format!(
            "internal error: path `{}` resolved on read but failed to remove",
            describe_path(&req.path)
        )
        .into());
    }
    let key_after = after_doc.get_top_level(affected_key).cloned();

    let note = Some(match movable {
        MovablePath::PermissionRule(_, _) => format!(
            "Permission rule will be removed from {}. The original is saved to a .bak alongside the file.",
            req.from.label()
        ),
        MovablePath::PermissionList(kind) => format!(
            "Entire `permissions.{}` list will be removed from {}. The original is saved to a .bak alongside the file.",
            kind.key(),
            req.from.label()
        ),
        MovablePath::TopLevelKey(k) => format!(
            "Top-level key `{}` will be removed from {}. The original is saved to a .bak alongside the file.",
            k,
            req.from.label()
        ),
    });

    Ok(DeleteLeafPreview {
        path: req.path.clone(),
        kind: MoveLeafKind::from_movable(&movable),
        from: MoveLeafSide {
            scope: req.from,
            file_path: path.display().to_string(),
            file_path_exists: path.exists(),
            key_before,
            key_after,
            will_write: true,
            note,
        },
    })
}

fn apply_delete_leaf_impl(
    paths: &ScopePaths,
    req: &DeleteLeafRequest,
    backups: Option<&BackupTracker>,
    watch: &WatchState,
) -> Result<LeafApplyOutcome, Box<dyn std::error::Error>> {
    let movable = validate_movable_path(&req.path)?;
    let path = require_path(paths, req.from)?.to_path_buf();
    let affected_key = path_top_level_key(&req.path);

    let (loaded, stamp) = io_atomic::load_with_stamp(&path)?;
    let mut doc = match loaded {
        Some(d) => d,
        None => return Err(format!("file {} does not exist", path.display()).into()),
    };
    let key_before = doc.get_top_level(affected_key).cloned();
    let src_value =
        doc.get_at_path(&req.path)
            .cloned()
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                format!(
                    "path `{}` not found in {} {}",
                    describe_path(&req.path),
                    req.from.label(),
                    path.display()
                )
                .into()
            })?;
    if !remove_movable_source(&mut doc, &movable, &src_value, &req.path) {
        return Err(format!(
            "internal error: path `{}` resolved on read but failed to remove",
            describe_path(&req.path)
        )
        .into());
    }
    let key_after = doc.get_top_level(affected_key).cloned();
    io_atomic::save(&path, &doc, backups, Some(&stamp))?;
    watch.note_self_write(&path);
    Ok(LeafApplyOutcome {
        from_before: key_before,
        from_after: key_after,
        to_before: None,
        to_after: None,
    })
}

fn diff_add_leaf_impl(
    paths: &ScopePaths,
    req: &AddLeafRequest,
) -> Result<AddLeafPreview, Box<dyn std::error::Error>> {
    let movable = validate_movable_path(&req.path)?;
    let path = require_path(paths, req.to)?;
    let affected_key = path_top_level_key(&req.path);

    let to_doc_loaded = io_atomic::load(path)?;
    let to_path_exists = to_doc_loaded.is_some();
    let to_doc_before = to_doc_loaded.unwrap_or_else(SettingsDoc::empty);
    let key_before = to_doc_before.get_top_level(affected_key).cloned();
    let mut to_doc = to_doc_before.clone();
    to_doc.merge_at_path(&req.path, req.value.clone())?;
    let key_after = to_doc.get_top_level(affected_key).cloned();

    let dest_unchanged = key_before == key_after;
    let note = leaf_preview_note(
        &movable,
        dest_unchanged,
        to_path_exists,
        key_before.as_ref(),
        &req.value,
    );

    Ok(AddLeafPreview {
        path: req.path.clone(),
        kind: MoveLeafKind::from_movable(&movable),
        to: MoveLeafSide {
            scope: req.to,
            file_path: path.display().to_string(),
            file_path_exists: to_path_exists,
            key_before,
            key_after,
            will_write: !dest_unchanged,
            note,
        },
    })
}

fn apply_add_leaf_impl(
    paths: &ScopePaths,
    req: &AddLeafRequest,
    backups: Option<&BackupTracker>,
    watch: &WatchState,
) -> Result<LeafApplyOutcome, Box<dyn std::error::Error>> {
    let _movable = validate_movable_path(&req.path)?;
    let path = require_path(paths, req.to)?.to_path_buf();
    let affected_key = path_top_level_key(&req.path);

    let (loaded, stamp) = io_atomic::load_with_stamp(&path)?;
    let to_doc_before = loaded.unwrap_or_else(SettingsDoc::empty);
    let key_before = to_doc_before.get_top_level(affected_key).cloned();
    let mut doc = to_doc_before;
    doc.merge_at_path(&req.path, req.value.clone())?;
    let key_after = doc.get_top_level(affected_key).cloned();
    let outcome = LeafApplyOutcome {
        from_before: None,
        from_after: None,
        to_before: key_before.clone(),
        to_after: key_after.clone(),
    };
    if key_before == key_after {
        // Idempotent add — no write needed. Surface success quietly so the
        // frontend's load() runs without the watcher seeing a stale event.
        // The outcome still carries the (equal) before/after so the
        // command can decide whether to skip the audit append (#172).
        return Ok(outcome);
    }
    io_atomic::save(&path, &doc, backups, Some(&stamp))?;
    watch.note_self_write(&path);
    Ok(outcome)
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

// -- audit-log restore: undo / redo / restore-to-point (#19 Phases 3-4) -----

/// One file a restore will rewrite. Internal to the restore planner; the
/// wire-facing shape is [`RestoreSidePreview`].
#[derive(Debug, Clone)]
struct RestoreTarget {
    scope: Scope,
    file_path: PathBuf,
    /// Top-level key the restore replaces (or removes).
    top_level_key: String,
    /// Value the key should hold after the restore. `None` removes the key.
    target_value: Option<serde_json::Value>,
    /// Value the audit log expected the key to currently hold. Drives the
    /// state-mismatch warning only — it never gates the write.
    expected_current: Option<serde_json::Value>,
}

/// A computed, ready-to-apply restoration. Built from an audit record by
/// [`build_restore_plan`] (undo / redo) or [`plan_restore_to`] (Phase 4),
/// then handed to [`apply_restore_plan`] / [`preview_restore_plan`].
#[derive(Debug, Clone)]
pub struct RestorePlan {
    direction: audit::RestoreDirection,
    /// The entry being undone / redone / restored-to.
    target_id: Ulid,
    /// Leaf kind + path + project, copied onto the resulting restore record
    /// so the History view can label the row.
    leaf_kind: audit::LeafKind,
    project_dir: Option<PathBuf>,
    path: Vec<PathSeg>,
    /// How many logged entries this plan reverts: 1 for an undo / redo, the
    /// count of spanned entries for a restore-to-point.
    ops_spanned: usize,
    targets: Vec<RestoreTarget>,
}

/// Per-file diff in a [`RestorePreview`]. `key_current` / `key_target`
/// follow `MoveLeafSide`'s skip-on-`None` convention: an absent field means
/// "key unset", distinct from a literal JSON `null`.
#[derive(Debug, Serialize)]
pub struct RestoreSidePreview {
    pub scope: Scope,
    pub file_path: String,
    pub file_path_exists: bool,
    pub top_level_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_current: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_target: Option<serde_json::Value>,
    pub will_write: bool,
    /// True when the on-disk value differs from what the audit log
    /// expected — i.e. the file was hand-edited since the logged op. The
    /// restore still proceeds on confirm; the flag drives a warning band.
    pub state_mismatch: bool,
}

/// Diff preview for an undo / redo / restore-to-point, rendered by the
/// frontend's restore-confirm modal.
#[derive(Debug, Serialize)]
pub struct RestorePreview {
    pub direction: audit::RestoreDirection,
    /// The audit entry being acted on, so the modal can describe it with
    /// the same helpers the History view uses.
    pub target: AuditRecordView,
    pub sides: Vec<RestoreSidePreview>,
    /// ULID of the last record in the audit log at preview time. The
    /// apply path passes this back in `expected_tail_id` so it can refuse
    /// to apply against a log that has since changed — closes the gap
    /// `ops_spanned` alone leaves open (a same-length window can hold
    /// different records after a concurrent rotate+append). See #171.
    /// `None` only when the log was empty at preview time (impossible for
    /// any restore that has a target, but kept Optional for future-proofing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_id: Option<Ulid>,
    /// How many ops this restore reverts: 1 for undo / redo, the count of
    /// spanned entries for a restore-to-point (#125).
    pub ops_spanned: usize,
}

/// The sides an audit record snapshots: `from` + `to` for an ordinary op,
/// or `restore.files` for a `Kind::Restore` entry (which leaves `from` /
/// `to` unset because a restore can touch more than two files).
fn record_sides(rec: &audit::Record) -> Vec<&audit::Side> {
    if let Some(meta) = &rec.restore {
        return meta.files.iter().collect();
    }
    let mut sides = Vec::new();
    if let Some(f) = &rec.from {
        sides.push(f);
    }
    if let Some(t) = &rec.to {
        sides.push(t);
    }
    sides
}

/// Wrap an [`audit::Record`] as an [`AuditRecordView`] (record + decoded
/// timestamp) — the shape both the History view and the restore modal read.
fn record_view(rec: audit::Record) -> AuditRecordView {
    let ts_ms = rec.id.timestamp_ms();
    AuditRecordView { record: rec, ts_ms }
}

/// Validate every record in `records` against the legitimate scope-file
/// allowlist (#183). Returns an error on the first record whose Side
/// points outside the resolved [`ScopePaths`] for that record's own
/// `project_dir`. Used at every audit-log boundary — GUI commands and
/// CLI subcommands call this immediately after loading records and
/// before building any [`RestorePlan`], so a hostile audit-log line
/// can never reach `apply_restore_plan` with an attacker-controlled
/// `file_path`.
pub fn validate_audit_records(
    records: &[audit::Record],
    home_dir: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in records {
        // Cross-project records (#179) carry two project roots: the
        // source's lives in `project_dir`, the destination's in
        // `project_dir_to`. Each side's `file_path` is validated against
        // *both* allowlists — passing under either is acceptable.
        // Single-project records leave `project_dir_to` at `None`, in
        // which case the from + to allowlists collapse to the same set
        // and the check stays identical to pre-#179 behavior.
        let project_dirs: [Option<&Path>; 2] = [
            entry.project_dir.as_deref(),
            entry.project_dir_to.as_deref(),
        ];
        for s in record_sides(entry) {
            validate_audit_path_against_any(
                &s.file_path,
                &s.top_level_key,
                &project_dirs,
                home_dir,
            )?;
        }
    }
    Ok(())
}

/// `validate_audit_path` wrapper that accepts multiple candidate
/// `project_dir`s and passes the path through if any of them allows it.
/// The first allow wins; only when all fail do we surface the error
/// (using the first non-`None` project_dir for the message context so
/// the legacy single-project case keeps its existing diagnostic).
fn validate_audit_path_against_any(
    target_path: &Path,
    top_level_key: &str,
    project_dirs: &[Option<&Path>],
    home_dir: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut first_err: Option<Box<dyn std::error::Error>> = None;
    for pd in project_dirs {
        match validate_audit_path(target_path, top_level_key, *pd, home_dir) {
            Ok(()) => return Ok(()),
            Err(e) if first_err.is_none() => first_err = Some(e),
            Err(_) => {}
        }
    }
    Err(first_err
        .unwrap_or_else(|| -> Box<dyn std::error::Error> { "no project_dir candidates".into() }))
}

/// Verify that an audit-log-derived `file_path` belongs to the legitimate
/// scope-file allowlist for `record_project_dir` under `home_dir`. Returns
/// an error if the path is outside the resolved [`ScopePaths`] (local,
/// project, user_local, user) for that project, or if the basename isn't
/// a settings file.
///
/// Closes the path-injection primitive in #183: without this check, a
/// hostile audit-log line carrying an arbitrary `file_path` would drive
/// [`apply_restore_plan`] to write attacker-chosen JSON to any path the
/// user can write to (including, on Windows, the Startup folder, and on
/// Unix files like `~/.config/Code/User/settings.json` whose contents
/// trigger code execution on the next app launch).
///
/// Both sides are canonicalized before comparison so a stamp-style path
/// (`/private/var/...` on macOS, `\\?\C:\...` on Windows) doesn't slip
/// past byte-equal checks. A canonicalize failure (target file doesn't
/// exist yet — legitimate for create-on-restore) falls back to lexical
/// equality, which is safe: the audit log stores the same path string
/// the resolver produces, so they match without canonicalization too.
fn validate_audit_path(
    target_path: &Path,
    _target_top_level_key: &str,
    record_project_dir: Option<&Path>,
    home_dir: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let basename = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if !matches!(basename, "settings.json" | "settings.local.json") {
        return Err(format!(
            "audit-log path injection refused: file_path `{}` does not look \
             like a Claude Code settings file (basename `{}`).",
            target_path.display(),
            basename
        )
        .into());
    }
    let paths = scope::resolve_with_home(record_project_dir, home_dir)?;
    let allowed: [Option<&Path>; 4] = [
        paths.local.as_deref(),
        paths.project.as_deref(),
        paths.user_local.as_deref(),
        paths.user.as_deref(),
    ];
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let target_canon = canon(target_path);
    if !allowed
        .iter()
        .filter_map(|opt| *opt)
        .any(|allowed_path| canon(allowed_path) == target_canon || allowed_path == target_path)
    {
        return Err(format!(
            "audit-log path injection refused: file_path `{}` is not a \
             legitimate scope file for project_dir `{}`. \
             Refusing to restore to a path outside the resolved scope set.",
            target_path.display(),
            record_project_dir
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<user-only>".to_string()),
        )
        .into());
    }
    // Top-level key allowlist is intentionally left for follow-up — the
    // load-bearing security control is the path allowlist above. A
    // hostile `top_level_key` on a *legitimate* settings.json file
    // expands attacker control to the user's own Claude config keys,
    // which is a much narrower blast radius than the original
    // arbitrary-file-write primitive.
    Ok(())
}

/// Build the plan to undo or redo a single op record.
///
/// Undo writes each affected file's key back to the snapshot `key_before`
/// and expects the file to currently hold `key_after`; redo is the mirror.
/// Snapshot-restore — rather than synthesizing a reverse move/add/delete
/// request — is faithful by construction (it writes back a value the log
/// already captured) and handles rules, lists, and top-level keys
/// uniformly.
pub fn build_restore_plan(
    rec: &audit::Record,
    direction: audit::RestoreDirection,
) -> Result<RestorePlan, Box<dyn std::error::Error>> {
    let undo = match direction {
        audit::RestoreDirection::Undo => true,
        audit::RestoreDirection::Redo => false,
        audit::RestoreDirection::ToPoint => {
            return Err("restore-to-point plans are built by plan_restore_to".into())
        }
    };
    let sides = record_sides(rec);
    if sides.is_empty() {
        return Err("audit entry carries no file snapshots — cannot restore".into());
    }
    let mut targets: Vec<RestoreTarget> = Vec::new();
    for s in sides {
        // Dedupe on (file_path, top_level_key) — a same-scope change-kind
        // records its one file under one top-level key as both `from` and
        // `to`, which should collapse; but two sides on the same file
        // touching *different* top-level keys are independent targets and
        // must each survive. Keying on file_path alone (the pre-fix shape)
        // silently dropped the second key — see #167.
        if targets
            .iter()
            .any(|t| t.file_path == s.file_path && t.top_level_key == s.top_level_key)
        {
            continue;
        }
        let (target_value, expected_current) = if undo {
            (s.key_before.clone(), s.key_after.clone())
        } else {
            (s.key_after.clone(), s.key_before.clone())
        };
        targets.push(RestoreTarget {
            scope: s.scope,
            file_path: s.file_path.clone(),
            top_level_key: s.top_level_key.clone(),
            target_value,
            expected_current,
        });
    }
    Ok(RestorePlan {
        direction,
        target_id: rec.id,
        leaf_kind: rec.leaf_kind,
        project_dir: rec.project_dir.clone(),
        path: rec.path.clone(),
        ops_spanned: 1,
        targets,
    })
}

/// Build the plan for a restore-to-point (#125): roll the affected files
/// back to the state they were in immediately before `target_id` was
/// written, reverting that entry and every entry logged after it.
///
/// The forward walk from the target to HEAD aggregates a per-file delta:
/// the value to write back is the *first* `key_before` seen for that file
/// in the window (its state when the window began — i.e. before the target),
/// and the expected-current value is the *last* `key_after` seen (what the
/// log believes the file holds now). `restore` entries inside the window
/// count too — their `restore.files` snapshots are walked like any other.
pub fn plan_restore_to(
    records: &[audit::Record],
    target_id: Ulid,
) -> Result<RestorePlan, Box<dyn std::error::Error>> {
    let target_idx = records
        .iter()
        .position(|r| r.id == target_id)
        .ok_or("target audit entry not found in the log")?;
    let target = &records[target_idx];
    if target.kind == audit::Kind::Restore {
        return Err(
            "that entry is itself a restore — pick an original move / add / delete to restore before"
                .into(),
        );
    }
    let window = &records[target_idx..];
    // Preserve first-seen order so the plan (and the modal) list targets
    // in a stable, log-driven order rather than hash order.
    //
    // Key on (file_path, top_level_key): a window can touch two different
    // top-level keys in the same settings.json (e.g. an `env` change
    // followed by a `permissions` change), and each (file, key) pair is
    // an independent restore target. Keying on file_path alone silently
    // dropped the second key — see #163.
    type RestoreKey = (PathBuf, String);
    let mut order: Vec<RestoreKey> = Vec::new();
    let mut by_key: std::collections::HashMap<RestoreKey, RestoreTarget> =
        std::collections::HashMap::new();
    for entry in window {
        for s in record_sides(entry) {
            let map_key: RestoreKey = (s.file_path.clone(), s.top_level_key.clone());
            match by_key.get_mut(&map_key) {
                // Already seen: keep the first `key_before` (window-start
                // state for this key) and advance `expected_current` to
                // this later `key_after`.
                Some(existing) => existing.expected_current = s.key_after.clone(),
                None => {
                    order.push(map_key.clone());
                    by_key.insert(
                        map_key,
                        RestoreTarget {
                            scope: s.scope,
                            file_path: s.file_path.clone(),
                            top_level_key: s.top_level_key.clone(),
                            target_value: s.key_before.clone(),
                            expected_current: s.key_after.clone(),
                        },
                    );
                }
            }
        }
    }
    let targets: Vec<RestoreTarget> = order
        .into_iter()
        .map(|k| by_key.remove(&k).expect("key inserted above"))
        .collect();
    if targets.is_empty() {
        return Err("nothing to restore — the spanned entries touched no files".into());
    }
    Ok(RestorePlan {
        direction: audit::RestoreDirection::ToPoint,
        target_id,
        leaf_kind: target.leaf_kind,
        project_dir: target.project_dir.clone(),
        path: target.path.clone(),
        ops_spanned: window.len(),
        targets,
    })
}

/// Read a single top-level key's current on-disk value, plus whether the
/// file exists. Errors (unreadable / malformed file) propagate — a restore
/// preview that can't read its target should surface that, not paper over
/// it.
fn current_top_level(
    file_path: &Path,
    key: &str,
) -> Result<(Option<serde_json::Value>, bool), Box<dyn std::error::Error>> {
    let loaded = io_atomic::load(file_path)?;
    let exists = loaded.is_some();
    let current = loaded.and_then(|d| d.get_top_level(key).cloned());
    Ok((current, exists))
}

/// Order-insensitive deep equality for `serde_json::Value`.
///
/// `serde_json` is built with `preserve_order` (Cargo.toml:27), so
/// `Value::Object` is backed by `IndexMap` and `Value::eq` compares object
/// keys in **insertion order**. That makes a file whose top-level key has
/// the same KV pairs in a different order than the audit snapshot
/// byte-unequal under `==`, even though no data changed. For the restore
/// preview/apply equality checks (`will_write`, `state_mismatch`,
/// phase-2 no-change skip) that means a user's deliberate key
/// reordering reads as destructive: the preview flags `state_mismatch`,
/// the apply doesn't skip, and `io_atomic::save` overwrites the
/// reordering with the audit snapshot's order — silently losing the
/// user's intent (#176).
///
/// The fix treats object key order as **presentation**, not data:
///
/// - Objects: same length, every key on one side has a semantically
///   equal value on the other side. Insertion order ignored.
/// - Arrays: order-sensitive (a reordered `permissions.allow` list is
///   distinct from the original — Claude Code treats the list as a set
///   but ClaudeScope preserves index ordering, so reorderings remain
///   first-class).
/// - Scalars: identical to `==`.
fn values_semantically_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Number(x), Number(y)) => x == y,
        (String(x), String(y)) => x == y,
        (Array(x), Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(a, b)| values_semantically_equal(a, b))
        }
        (Object(x), Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| values_semantically_equal(v, w)))
        }
        _ => false,
    }
}

/// `Option<Value>` wrapper of [`values_semantically_equal`]. Both `None`
/// is equal; mixed is not; both `Some` recurses.
fn opt_values_semantically_equal(
    a: &Option<serde_json::Value>,
    b: &Option<serde_json::Value>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => values_semantically_equal(x, y),
        _ => false,
    }
}

/// Compute the diff preview for a restore plan without writing anything.
/// `target` is the audit entry being acted on.
pub fn preview_restore_plan(
    plan: &RestorePlan,
    target: &audit::Record,
) -> Result<RestorePreview, Box<dyn std::error::Error>> {
    let mut sides = Vec::with_capacity(plan.targets.len());
    for t in &plan.targets {
        let (current, exists) = current_top_level(&t.file_path, &t.top_level_key)?;
        sides.push(RestoreSidePreview {
            scope: t.scope,
            file_path: t.file_path.display().to_string(),
            file_path_exists: exists,
            top_level_key: t.top_level_key.clone(),
            will_write: !opt_values_semantically_equal(&current, &t.target_value),
            state_mismatch: !opt_values_semantically_equal(&current, &t.expected_current),
            key_current: current,
            key_target: t.target_value.clone(),
        });
    }
    Ok(RestorePreview {
        direction: plan.direction,
        target: record_view(target.clone()),
        sides,
        ops_spanned: plan.ops_spanned,
        // Filled in by higher-level callers that have the log handy
        // (`restore_to_point_preview`). `preview_restore_plan` itself only
        // sees the plan + target, not the full log.
        tail_id: None,
    })
}

/// Apply a restore plan: rewrite every target file's affected top-level key
/// atomically, rolling the whole batch back if any single write fails.
///
/// Returns one [`audit::Side`] per target — `key_before` is the value the
/// file actually held at write time, `key_after` the value written — so the
/// caller can record a `Kind::Restore` entry that a later undo can itself
/// invert.
pub fn apply_restore_plan(
    plan: &RestorePlan,
    backups: Option<&BackupTracker>,
    watch: &WatchState,
) -> Result<Vec<audit::Side>, Box<dyn std::error::Error>> {
    /// One unique file's combined write. Multiple plan targets that
    /// share `file_path` (different `top_level_key`s) coalesce into
    /// the same `FileWrite` — its `new_doc` accumulates every key's
    /// restored value, then a single save persists the combined
    /// result. Without this coalescing, two targets on the same file
    /// each issued their own save against the original stamp; the
    /// second save would either hit `ConcurrentModification` (because
    /// the first save changed the file) or, on coarse-mtime filesystems,
    /// overwrite the first key's restoration with a stale doc. See
    /// codex 4th-pass [P1].
    struct FileWrite {
        path: PathBuf,
        existed: bool,
        original: SettingsDoc,
        stamp: FileStamp,
        new_doc: SettingsDoc,
    }

    // Phase 1 — load + accumulate per-file mutations. The first time we
    // see a file_path, load it; every subsequent target on the same
    // file just mutates the in-progress new_doc. Preserves first-seen
    // file order so phase 2 writes in plan order.
    let mut writes: Vec<FileWrite> = Vec::new();
    let mut file_idx: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for t in &plan.targets {
        let idx = match file_idx.get(&t.file_path) {
            Some(&i) => i,
            None => {
                let (loaded, stamp) = io_atomic::load_with_stamp(&t.file_path)?;
                let existed = loaded.is_some();
                let original = loaded.unwrap_or_else(SettingsDoc::empty);
                let new_doc = original.clone();
                let i = writes.len();
                file_idx.insert(t.file_path.clone(), i);
                writes.push(FileWrite {
                    path: t.file_path.clone(),
                    existed,
                    original,
                    stamp,
                    new_doc,
                });
                i
            }
        };
        let fw = &mut writes[idx];
        match &t.target_value {
            Some(v) => fw.new_doc.set_top_level(&t.top_level_key, v.clone()),
            None => {
                fw.new_doc
                    .remove_at_path(&[PathSeg::Key(t.top_level_key.clone())]);
            }
        }
    }

    // Phase 2 — write each unique file once. Skip a file whose
    // accumulated new_doc matches its original on every key the plan
    // touched (no key actually moved — e.g. every per-key target was
    // a no-op against current disk).
    let mut written: Vec<usize> = Vec::new();
    for (i, fw) in writes.iter().enumerate() {
        let no_change = plan.targets.iter().all(|t| {
            if t.file_path != fw.path {
                return true;
            }
            let before = fw.original.get_top_level(&t.top_level_key).cloned();
            // Order-insensitive equality (#176) — see
            // `opt_values_semantically_equal`. Skips a write when the
            // file already holds the target's KV pairs in *any* order,
            // so a deliberate user reordering isn't overwritten with the
            // audit snapshot's order on undo.
            opt_values_semantically_equal(&before, &t.target_value)
        });
        if no_change {
            continue;
        }
        if let Err(write_err) = io_atomic::save(&fw.path, &fw.new_doc, backups, Some(&fw.stamp)) {
            for &w in written.iter().rev() {
                let pw = &writes[w];
                let rollback: Result<(), Box<dyn std::error::Error>> = if pw.existed {
                    io_atomic::save(&pw.path, &pw.original, backups, None).map_err(Into::into)
                } else {
                    std::fs::remove_file(&pw.path).map_err(Into::into)
                };
                if let Err(rollback_err) = rollback {
                    return Err(format!(
                        "restore write of {} failed: {write_err}; \
                         rolling back {} also failed: {rollback_err}",
                        fw.path.display(),
                        pw.path.display()
                    )
                    .into());
                }
                watch.note_self_write(&pw.path);
            }
            return Err(format!(
                "restore write of {} failed — all earlier files rolled back: {write_err}",
                fw.path.display()
            )
            .into());
        }
        watch.note_self_write(&fw.path);
        written.push(i);
    }

    // Phase 3 — produce one audit Side per plan target, with per-key
    // before/after values pulled from the matching FileWrite. The
    // returned Sides line up 1:1 with `plan.targets`, even when two
    // targets share a file_path — each Side reports its own
    // top-level-key transition.
    Ok(plan
        .targets
        .iter()
        .map(|t| {
            let idx = file_idx
                .get(&t.file_path)
                .expect("every target was inserted in phase 1");
            let fw = &writes[*idx];
            let key_before = fw.original.get_top_level(&t.top_level_key).cloned();
            let key_after = fw.new_doc.get_top_level(&t.top_level_key).cloned();
            audit::Side {
                scope: t.scope,
                file_path: t.file_path.clone(),
                top_level_key: t.top_level_key.clone(),
                key_before,
                key_after,
            }
        })
        .collect())
}

/// Build the `Kind::Restore` audit record describing a completed restore.
pub fn restore_record(
    plan: &RestorePlan,
    actor: audit::Actor,
    files: Vec<audit::Side>,
) -> audit::Record {
    audit::Record::new_restore(
        plan.leaf_kind,
        actor,
        plan.project_dir.clone(),
        plan.path.clone(),
        audit::RestoreMeta {
            target_id: plan.target_id,
            direction: plan.direction,
            files,
        },
    )
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
    fn detect_path_collisions_groups_scopes_sharing_one_file() {
        // #153 — Launching ClaudeScope with project_dir == $HOME makes
        // User and Project resolve to the same `~/.claude/settings.json`,
        // and Local/UserLocal to the same `settings.local.json`. The
        // detector groups those collisions with the scopes sorted in
        // Scope::ALL (broad→narrow) order so the rendered banner reads
        // in the same order the columns are laid out.
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let shared_settings = claude.join("settings.json");
        let shared_local = claude.join("settings.local.json");
        let paths = ScopePaths {
            project_dir: tmp.path().to_path_buf(),
            local: Some(shared_local.clone()),
            project: Some(shared_settings.clone()),
            user_local: Some(shared_local.clone()),
            user: Some(shared_settings.clone()),
        };

        let collisions = detect_path_collisions(&paths);
        assert_eq!(collisions.len(), 2, "two distinct shared files");

        // Scope::ALL ordering is [Local, Project, UserLocal, User], so
        // the broader (User, UserLocal) pair lists first in each group.
        let local_collision = collisions
            .iter()
            .find(|c| c.path == shared_local.display().to_string())
            .expect("local collision present");
        assert_eq!(local_collision.scopes, vec![Scope::Local, Scope::UserLocal]);
        let project_collision = collisions
            .iter()
            .find(|c| c.path == shared_settings.display().to_string())
            .expect("project collision present");
        assert_eq!(project_collision.scopes, vec![Scope::Project, Scope::User]);
    }

    #[test]
    fn detect_path_collisions_returns_empty_when_paths_are_distinct() {
        // Sanity: the common case (project_dir != home) produces zero
        // collisions even though Local/Project share a parent and
        // UserLocal/User share theirs.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        assert!(detect_path_collisions(&paths).is_empty());
    }

    #[test]
    fn detect_kind_conflicts_surfaces_allow_vs_deny_across_scopes() {
        // #156 — `Bash(git push)` is allowed in User but denied in
        // Project. The conflict surfaces with both occurrences in
        // precedence order (highest first = Project before User), so
        // the frontend can label "Project's deny wins by precedence".
        let views = vec![
            make_view(Scope::Local, true, &[], &[], &[]),
            make_view(Scope::Project, true, &[], &["Bash(git push)"], &[]),
            make_view(Scope::UserLocal, true, &[], &[], &[]),
            make_view(Scope::User, true, &["Bash(git push)"], &[], &[]),
        ];
        let conflicts = detect_kind_conflicts(&views);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].rule, "Bash(git push)");
        // Scope::ALL is [Local, Project, UserLocal, User] (precedence,
        // highest first); the conflict's occurrences inherit that.
        assert_eq!(
            conflicts[0]
                .occurrences
                .iter()
                .map(|o| (o.scope, o.kind))
                .collect::<Vec<_>>(),
            vec![
                (Scope::Project, PermissionKind::Deny),
                (Scope::User, PermissionKind::Allow),
            ]
        );
    }

    #[test]
    fn detect_kind_conflicts_ignores_matching_kinds_in_multiple_scopes() {
        // The same rule in `allow` across two scopes is *not* a conflict;
        // it just means both scopes agree. Without filtering on distinct
        // kinds, the detector would emit a spurious entry every time a
        // user-scope allow was reaffirmed at project scope.
        let views = vec![
            make_view(Scope::Local, true, &["Bash(git status)"], &[], &[]),
            make_view(Scope::User, true, &["Bash(git status)"], &[], &[]),
        ];
        assert!(detect_kind_conflicts(&views).is_empty());
    }

    #[test]
    fn detect_kind_conflicts_collapses_duplicates_in_one_scope_kind() {
        // A hand-edited duplicate in the same (scope, kind) bucket
        // shouldn't produce a phantom conflict against itself, and
        // shouldn't appear twice in `occurrences`. Duplicate-detection
        // is #17's surface, not this one.
        let views = vec![make_view(
            Scope::Local,
            true,
            &["Bash(rm)", "Bash(rm)"],
            &["Bash(rm)"],
            &[],
        )];
        let conflicts = detect_kind_conflicts(&views);
        assert_eq!(conflicts.len(), 1);
        // Two unique (scope, kind) pairs, not three.
        assert_eq!(conflicts[0].occurrences.len(), 2);
    }

    #[test]
    fn apply_move_leaf_impl_moves_local_rule_across_project_boundaries() {
        // #179 — A rule in Project A's Local scope can be moved into
        // Project B's Local scope when the impl is called with the two
        // sides' [`ScopePaths`] resolved independently. Pre-#179 the
        // backend resolved both sides from one project root, so this
        // path errored with "source not found" (codex 6th-pass [P1] in
        // PR #178). Pins the headline cross-project win.
        let tmp = tempfile::tempdir().unwrap();
        let project_a = tmp.path().join("a");
        let project_b = tmp.path().join("b");
        std::fs::create_dir_all(project_a.join(".claude")).unwrap();
        std::fs::create_dir_all(project_b.join(".claude")).unwrap();
        let user_home = tmp.path().join("home");
        std::fs::create_dir_all(user_home.join(".claude")).unwrap();
        let paths_from = ScopePaths {
            project_dir: project_a.clone(),
            local: Some(project_a.join(".claude").join("settings.local.json")),
            project: Some(project_a.join(".claude").join("settings.json")),
            user_local: Some(user_home.join(".claude").join("settings.local.json")),
            user: Some(user_home.join(".claude").join("settings.json")),
        };
        let paths_to = ScopePaths {
            project_dir: project_b.clone(),
            local: Some(project_b.join(".claude").join("settings.local.json")),
            project: Some(project_b.join(".claude").join("settings.json")),
            user_local: Some(user_home.join(".claude").join("settings.local.json")),
            user: Some(user_home.join(".claude").join("settings.json")),
        };
        let from_file = paths_from.local.clone().unwrap();
        let to_file = paths_to.local.clone().unwrap();
        write(&from_file, r#"{"permissions":{"allow":["Bash(rm)"]}}"#);

        let outcome = apply_move_leaf_impl(
            &paths_from,
            &paths_to,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Local,
                to: Scope::Local,
                to_kind: None,
            },
            None,
            &WatchState::default(),
        )
        .unwrap();

        // Source file in project A no longer holds the rule.
        let from_doc = io_atomic::load(&from_file).unwrap().unwrap();
        assert!(from_doc.permissions().allow.is_empty());
        // Destination file in project B holds it.
        let to_doc = io_atomic::load(&to_file).unwrap().unwrap();
        assert_eq!(to_doc.permissions().allow, vec!["Bash(rm)".to_string()]);
        // Outcome's `from`/`to` snapshots track each project's file
        // separately — load-with-stamp captured each before/after from
        // its own side, so a cross-project undo can later restore each.
        assert_eq!(
            outcome.from_before,
            Some(serde_json::json!({"allow": ["Bash(rm)"]}))
        );
        assert_eq!(outcome.from_after, Some(serde_json::json!({"allow": []})));
        assert!(
            outcome.to_before.is_none(),
            "destination file did not exist"
        );
        assert_eq!(
            outcome.to_after,
            Some(serde_json::json!({"allow": ["Bash(rm)"]}))
        );
    }

    #[test]
    fn apply_move_leaf_impl_moves_project_rule_across_project_boundaries() {
        // Sibling to the local→local test for the project→project case:
        // a rule in Project A's `Project` scope (committed) moves into
        // Project B's `Project` scope. Same physical scope name on both
        // sides — pre-#179 the resolver would have looked for the
        // source under the destination's root and failed.
        let tmp = tempfile::tempdir().unwrap();
        let project_a = tmp.path().join("a");
        let project_b = tmp.path().join("b");
        std::fs::create_dir_all(project_a.join(".claude")).unwrap();
        std::fs::create_dir_all(project_b.join(".claude")).unwrap();
        let paths_from = ScopePaths {
            project_dir: project_a.clone(),
            local: Some(project_a.join(".claude").join("settings.local.json")),
            project: Some(project_a.join(".claude").join("settings.json")),
            user_local: None,
            user: None,
        };
        let paths_to = ScopePaths {
            project_dir: project_b.clone(),
            local: Some(project_b.join(".claude").join("settings.local.json")),
            project: Some(project_b.join(".claude").join("settings.json")),
            user_local: None,
            user: None,
        };
        let from_file = paths_from.project.clone().unwrap();
        let to_file = paths_to.project.clone().unwrap();
        write(&from_file, r#"{"permissions":{"allow":["Read(**)"]}}"#);

        apply_move_leaf_impl(
            &paths_from,
            &paths_to,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::Project,
                to_kind: None,
            },
            None,
            &WatchState::default(),
        )
        .unwrap();

        assert!(io_atomic::load(&from_file)
            .unwrap()
            .unwrap()
            .permissions()
            .allow
            .is_empty());
        assert_eq!(
            io_atomic::load(&to_file)
                .unwrap()
                .unwrap()
                .permissions()
                .allow,
            vec!["Read(**)".to_string()]
        );
    }

    #[test]
    fn resolve_move_paths_falls_back_to_project_dir_when_per_side_unset() {
        // Backwards-compat: the IPC must accept the pre-#179 shape (just
        // `project_dir`) and treat both sides as that root. Without this
        // fallback, every existing same-project caller would break.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        let overrides = RuntimeOverrides::default();
        let (paths_from, paths_to) =
            resolve_move_paths(Some(project.to_str().unwrap()), None, None, &overrides).unwrap();
        assert_eq!(paths_from.project_dir, paths_to.project_dir);
        assert_eq!(paths_from.project_dir, project);
    }

    #[test]
    fn resolve_move_paths_honors_per_side_overrides_when_set() {
        // The new IPC shape: each side gets its own root. The fallback to
        // `project_dir` only fires when a side is `None`, so passing both
        // overrides drives the genuinely two-root resolution path.
        let tmp = tempfile::tempdir().unwrap();
        let project_a = tmp.path().join("a");
        let project_b = tmp.path().join("b");
        std::fs::create_dir_all(project_a.join(".claude")).unwrap();
        std::fs::create_dir_all(project_b.join(".claude")).unwrap();
        let overrides = RuntimeOverrides::default();
        let (paths_from, paths_to) = resolve_move_paths(
            None,
            Some(project_a.to_str().unwrap()),
            Some(project_b.to_str().unwrap()),
            &overrides,
        )
        .unwrap();
        assert_eq!(paths_from.project_dir, project_a);
        assert_eq!(paths_to.project_dir, project_b);
        // User-scope paths in both sides resolve under the same `$HOME`
        // (overrides.home is None, so both sides use the real home dir or
        // None on a no-home environment). The two ScopePaths still
        // diverge on the local/project scopes — that's the point.
        assert_ne!(paths_from.local, paths_to.local);
    }

    #[test]
    fn validate_move_request_refuses_cross_scope_move_into_a_colliding_pair() {
        // #153 — Even with the path-collision banner up, the user could
        // still try to drag a rule from User to Project when both
        // resolve to the same file. Such a move would either be a no-op
        // or corrupt the file (rewrite-with-self while the source
        // and destination IPCs race for the same FileStamp). Refuse at
        // validation rather than asking the apply layer to deal with
        // it: a typed error keeps the modal close path clean and gives
        // the UI a stable string to surface.
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        let shared = claude.join("settings.json");
        let paths = ScopePaths {
            project_dir: tmp.path().to_path_buf(),
            local: Some(claude.join("settings.local.json")),
            project: Some(shared.clone()),
            user_local: None,
            user: Some(shared.clone()),
        };
        std::fs::write(&shared, r#"{"permissions":{"allow":["X"]}}"#).unwrap();
        let req = MoveLeafRequest {
            path: vec![key("permissions"), key("allow"), idx(0)],
            from: Scope::Project,
            to: Scope::User,
            to_kind: None,
        };
        let err =
            apply_move_leaf_impl(&paths, &paths, &req, None, &WatchState::default()).unwrap_err();
        let msg = err.to_string();
        // Post-#179 the collision message folds the cross-scope and
        // same-scope same-file cases into one wording — "overwrite the
        // file with itself" is the substring shared by both branches.
        assert!(
            msg.contains("overwrite the file with itself"),
            "expected collision-message error, got: {msg}"
        );
    }

    #[test]
    fn detect_kind_conflicts_handles_three_way_disagreement() {
        // Allow / deny / ask across three scopes — all three should
        // appear in `occurrences`, still in precedence order.
        let views = vec![
            make_view(Scope::Local, true, &["X"], &[], &[]),
            make_view(Scope::Project, true, &[], &["X"], &[]),
            make_view(Scope::UserLocal, true, &[], &[], &[]),
            make_view(Scope::User, true, &[], &[], &["X"]),
        ];
        let conflicts = detect_kind_conflicts(&views);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0]
                .occurrences
                .iter()
                .map(|o| (o.scope, o.kind))
                .collect::<Vec<_>>(),
            vec![
                (Scope::Local, PermissionKind::Allow),
                (Scope::Project, PermissionKind::Deny),
                (Scope::User, PermissionKind::Ask),
            ]
        );
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
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
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
    fn apply_move_leaf_with_none_backups_writes_without_creating_bak() {
        // #88: when the user opts out of `.bak` files via Preferences, the
        // command handler passes `None` into the impl. Verify both that the
        // write still lands AND that no sibling `.bak` is created — the
        // latter is the whole point of the opt-out.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        );
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":[]}}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            None,
            &WatchState::default(),
        )
        .unwrap();

        // Move landed on both sides.
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

        // No `.bak` for either file. Probing both because the move writes to
        // *both* the source and destination scopes, and a regression on
        // either arm would silently re-introduce the clutter the pref opts
        // out of.
        let project_bak = paths.project.as_ref().unwrap().with_extension("json.bak");
        let user_bak = paths.user.as_ref().unwrap().with_extension("json.bak");
        assert!(!project_bak.exists(), "expected no .bak at {project_bak:?}");
        assert!(!user_bak.exists(), "expected no .bak at {user_bak:?}");
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
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
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
    fn move_leaf_permission_rule_strips_all_copies_of_duplicated_string() {
        // Hand-edited settings.json can list the same rule string twice in
        // `permissions.allow`. The legacy `apply_move_impl` removed *all*
        // matching strings from the source (via `arr.retain(...)`); the
        // path-based primitive now matches that semantics through
        // `remove_movable_source`. Without this fix, removing index 0
        // would leave the duplicate at index 1 behind in the source while
        // the destination's array-union dedupes — the user would think
        // the rule moved while a stale copy lived on.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)","Bash(git status)","Read(**)"]}}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let project_doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            project_doc.permissions().allow,
            vec!["Read(**)".to_string()],
            "both Bash(git status) copies must be stripped from the source"
        );
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
            &paths,
            &MoveLeafRequest {
                path: vec![key("env")],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
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
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow")],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
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
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
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
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions")],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
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
            &paths,
            &MoveLeafRequest {
                path: vec![key("theme")],
                from: Scope::Project,
                to: Scope::Project,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap_err();
        // Post-#179: the "same scope" rejection was folded into the
        // path-collision message, since same-scope same-project is just
        // a special case of "source and destination resolve to the
        // same file."
        assert!(err.to_string().contains("overwrite the file with itself"));
    }

    #[test]
    fn move_leaf_rejects_when_source_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{"permissions":{}}"#);
        let err = apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
                to_kind: None,
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // -- change-kind tests (#8) --------------------------------------------

    #[test]
    fn change_kind_same_scope_moves_rule_between_kinds_in_one_file() {
        // The headline same-scope flow: a rule under `permissions.allow` is
        // reclassified to `permissions.deny` within the same settings file.
        // Single load, single write — the cross-scope rollback path doesn't
        // need to be involved.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(git status)","Read(**)"]}}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::Project,
                to_kind: Some(PermissionKind::Deny),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["Read(**)"], "deny": ["Bash(git status)"]})
        );
    }

    #[test]
    fn change_kind_same_scope_strips_all_duplicates_of_rule_string() {
        // Mirrors `move_leaf_permission_rule_strips_all_copies_of_duplicated_string`
        // but for the same-scope change-kind path. Hand-edited duplicates in
        // the source kind must all disappear; the destination kind ends up
        // with a single copy thanks to the merge step's idempotency.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(rm)","Bash(rm)","Read(**)"]}}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::Project,
                to_kind: Some(PermissionKind::Deny),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["Read(**)"], "deny": ["Bash(rm)"]})
        );
    }

    #[test]
    fn change_kind_cross_scope_lands_under_new_kind_at_destination() {
        // Cross-scope change-kind: rule moves out of Project's allow list
        // and into User's deny list in one shot. Source must lose the rule;
        // destination must receive it under the new kind, not the old one.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(rm)"]}}"#,
        );

        apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::User,
                to_kind: Some(PermissionKind::Deny),
            },
            Some(&BackupTracker::new()),
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
        assert!(user_doc.permissions().allow.is_empty());
        assert_eq!(user_doc.permissions().deny, vec!["Bash(rm)".to_string()]);
    }

    #[test]
    fn change_kind_same_scope_same_kind_errors_with_nothing_to_change() {
        // A request that asks to change a rule to the kind it already has
        // is a no-op the user almost certainly didn't mean — error rather
        // than silently succeed so the UI can surface "this rule is already
        // an Allow rule" instead of pretending work happened.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(rm)"]}}"#,
        );
        let err = apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::Project,
                to_kind: Some(PermissionKind::Allow),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing to change"));
    }

    #[test]
    fn change_kind_rejected_on_non_rule_paths() {
        // to_kind only makes sense for a single permission rule. Whole-list
        // and top-level-key paths must reject the request rather than
        // silently misbehave.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"theme":"dark","permissions":{"allow":["Bash(rm)"]}}"#,
        );
        let err_list = apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow")],
                from: Scope::Project,
                to: Scope::User,
                to_kind: Some(PermissionKind::Deny),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err_list.to_string().contains("to_kind only applies"));

        let err_key = apply_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("theme")],
                from: Scope::Project,
                to: Scope::User,
                to_kind: Some(PermissionKind::Allow),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err_key.to_string().contains("to_kind only applies"));
    }

    #[test]
    fn diff_change_kind_same_scope_collapses_to_one_file() {
        // The preview for a same-scope change-kind request must reference the
        // same file on both sides with identical key_before / key_after, and
        // only the from side carries `will_write = true` (the apply path
        // issues exactly one save). The frontend uses these signals to
        // collapse the bilateral diff into a single side.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(rm)"]}}"#,
        );

        let preview = diff_move_leaf_impl(
            &paths,
            &paths,
            &MoveLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
                to: Scope::Project,
                to_kind: Some(PermissionKind::Deny),
            },
        )
        .unwrap();
        assert_eq!(preview.from.file_path, preview.to.file_path);
        assert_eq!(preview.from.key_before, preview.to.key_before);
        assert_eq!(preview.from.key_after, preview.to.key_after);
        assert!(preview.from.will_write);
        assert!(!preview.to.will_write);
        assert_eq!(preview.to_kind, Some(PermissionKind::Deny));
        assert_eq!(
            preview.from.key_after.unwrap(),
            serde_json::json!({"allow": [], "deny": ["Bash(rm)"]})
        );
    }

    // -- audit restore: undo / redo (#124) ---------------------------------

    fn perms(allow: &[&str]) -> serde_json::Value {
        serde_json::json!({ "allow": allow })
    }

    fn side_at(
        scope: Scope,
        path: &Path,
        before: serde_json::Value,
        after: serde_json::Value,
    ) -> audit::Side {
        audit::Side {
            scope,
            file_path: path.to_path_buf(),
            top_level_key: "permissions".to_string(),
            key_before: Some(before),
            key_after: Some(after),
        }
    }

    fn move_record(from: audit::Side, to: audit::Side) -> audit::Record {
        audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            Some(from),
            Some(to),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        )
    }

    #[test]
    fn apply_move_leaf_impl_returns_audit_outcome_matching_disk_state() {
        // Regression for #169. The impl must return before/after values
        // captured from its own load_with_stamp — not from a separate
        // snapshot at the command boundary. The test cuts out the
        // command boundary entirely and verifies the impl's outcome
        // reflects the actual transition.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        let user = paths.user.clone().unwrap();
        write(&project, r#"{"permissions":{"allow":["X","Y"]}}"#);
        write(&user, r#"{"permissions":{"allow":["Z"]}}"#);

        let req = MoveLeafRequest {
            path: vec![key("permissions"), key("allow"), idx(0)],
            from: Scope::Project,
            to: Scope::User,
            to_kind: None,
        };
        let outcome =
            apply_move_leaf_impl(&paths, &paths, &req, None, &WatchState::default()).unwrap();

        // from_before reflects the file BEFORE the impl's mutation.
        assert_eq!(
            outcome.from_before,
            Some(serde_json::json!({"allow": ["X", "Y"]}))
        );
        // from_after reflects the file AFTER the impl's mutation.
        assert_eq!(
            outcome.from_after,
            Some(serde_json::json!({"allow": ["Y"]}))
        );
        assert_eq!(outcome.to_before, Some(serde_json::json!({"allow": ["Z"]})));
        assert_eq!(
            outcome.to_after,
            Some(serde_json::json!({"allow": ["Z", "X"]}))
        );
    }

    #[test]
    fn apply_delete_leaf_impl_returns_audit_outcome() {
        // Regression for #169 — same shape, delete leaf.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        write(&project, r#"{"permissions":{"allow":["X","Y"]}}"#);

        let req = DeleteLeafRequest {
            path: vec![key("permissions"), key("allow"), idx(0)],
            from: Scope::Project,
        };
        let outcome = apply_delete_leaf_impl(&paths, &req, None, &WatchState::default()).unwrap();

        assert_eq!(
            outcome.from_before,
            Some(serde_json::json!({"allow": ["X", "Y"]}))
        );
        assert_eq!(
            outcome.from_after,
            Some(serde_json::json!({"allow": ["Y"]}))
        );
        // Delete has no `to` side.
        assert!(outcome.to_before.is_none());
        assert!(outcome.to_after.is_none());
    }

    #[test]
    fn apply_add_leaf_impl_returns_audit_outcome() {
        // Regression for #169 — same shape, add leaf.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let user = paths.user.clone().unwrap();
        write(&user, r#"{"permissions":{"allow":["Z"]}}"#);

        let req = AddLeafRequest {
            path: vec![key("permissions"), key("allow"), idx(0)],
            to: Scope::User,
            value: serde_json::Value::String("NEW".to_string()),
        };
        let outcome = apply_add_leaf_impl(&paths, &req, None, &WatchState::default()).unwrap();

        // Add has no `from` side.
        assert!(outcome.from_before.is_none());
        assert!(outcome.from_after.is_none());
        assert_eq!(outcome.to_before, Some(serde_json::json!({"allow": ["Z"]})));
        // merge_at_path appends; the new rule lands after the existing one.
        assert_eq!(
            outcome.to_after,
            Some(serde_json::json!({"allow": ["Z", "NEW"]}))
        );
    }

    #[test]
    fn apply_leaf_impls_reject_malformed_paths_without_panicking() {
        // #174 — Before the fix, the three Tauri command wrappers called
        // `path_top_level_key(&req.path)` (which `.expect()`s) before any
        // validation. A malformed IPC payload — empty path, or a path whose
        // first segment is an Index rather than a Key — would panic across
        // the FFI boundary instead of returning a typed validation error.
        // The wrappers now run `validate_movable_path` first; the test pins
        // the impl behavior that the wrappers now defer to.
        //
        // Two malformed shapes per impl:
        //   - `path = []` exercises the empty-path branch of validation.
        //   - `path = [Index(0)]` exercises the "first segment is not a key"
        //     branch (which `validate_movable_path`'s wildcard arm rejects).
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        write(&project, r#"{"permissions":{"allow":["X"]}}"#);

        let bad_paths: Vec<Vec<PathSeg>> = vec![vec![], vec![idx(0)]];

        for bad in &bad_paths {
            // Move
            let err = apply_move_leaf_impl(
                &paths,
                &paths,
                &MoveLeafRequest {
                    path: bad.clone(),
                    from: Scope::Project,
                    to: Scope::User,
                    to_kind: None,
                },
                None,
                &WatchState::default(),
            )
            .unwrap_err();
            assert!(
                !err.to_string().is_empty(),
                "move expected typed error for {bad:?}, got empty"
            );

            // Delete
            let err = apply_delete_leaf_impl(
                &paths,
                &DeleteLeafRequest {
                    path: bad.clone(),
                    from: Scope::Project,
                },
                None,
                &WatchState::default(),
            )
            .unwrap_err();
            assert!(
                !err.to_string().is_empty(),
                "delete expected typed error for {bad:?}, got empty"
            );

            // Add
            let err = apply_add_leaf_impl(
                &paths,
                &AddLeafRequest {
                    path: bad.clone(),
                    to: Scope::Project,
                    value: serde_json::Value::String("X".into()),
                },
                None,
                &WatchState::default(),
            )
            .unwrap_err();
            assert!(
                !err.to_string().is_empty(),
                "add expected typed error for {bad:?}, got empty"
            );
        }
    }

    #[test]
    fn apply_change_kind_same_scope_returns_audit_outcome() {
        // Regression for #182. The same-scope change-kind path populates all
        // four LeafApplyOutcome fields from a single load (one file is both
        // `from` and `to`), so a refactor that swapped `key_before` /
        // `key_after` on any of the four assignments would leave on-disk
        // state correct but feed wrong values into the audit log — a later
        // undo would then restore the post-change state. The two existing
        // tests assert on disk only; this one reads back the outcome.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        write(
            &project,
            r#"{"permissions":{"allow":["Bash(git status)"]}}"#,
        );

        let req = MoveLeafRequest {
            path: vec![key("permissions"), key("allow"), idx(0)],
            from: Scope::Project,
            to: Scope::Project,
            to_kind: Some(PermissionKind::Deny),
        };
        let outcome =
            apply_move_leaf_impl(&paths, &paths, &req, None, &WatchState::default()).unwrap();

        // Same-scope: from_* and to_* describe the same file, before/after
        // the in-memory mutation. The two pre-write captures should be
        // equal, the two post-write captures should be equal, and the
        // pair should differ from each other.
        let before = serde_json::json!({"allow": ["Bash(git status)"]});
        let after = serde_json::json!({"allow": [], "deny": ["Bash(git status)"]});
        assert_eq!(outcome.from_before, Some(before.clone()));
        assert_eq!(outcome.to_before, Some(before));
        assert_eq!(outcome.from_after, Some(after.clone()));
        assert_eq!(outcome.to_after, Some(after));
        assert_ne!(outcome.from_before, outcome.from_after);
    }

    #[test]
    fn apply_add_leaf_impl_idempotent_returns_equal_before_after() {
        // Regression for #172 (companion to #169). When the rule is
        // already present at the destination, the impl returns an
        // outcome with `to_before == to_after` and the command can
        // skip the audit append rather than emit a phantom entry.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let user = paths.user.clone().unwrap();
        write(&user, r#"{"permissions":{"allow":["NEW"]}}"#);

        let req = AddLeafRequest {
            path: vec![key("permissions"), key("allow"), idx(0)],
            to: Scope::User,
            value: serde_json::Value::String("NEW".to_string()),
        };
        let outcome = apply_add_leaf_impl(&paths, &req, None, &WatchState::default()).unwrap();

        // Same value before and after — caller (`apply_add_leaf`) skips
        // emit_audit on this signal.
        assert_eq!(outcome.to_before, outcome.to_after);
    }

    #[test]
    fn undo_of_a_move_restores_both_files_to_pre_move_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        let user = paths.user.clone().unwrap();
        // Disk reflects the post-move state: rule A moved Project → User.
        write(&project, r#"{"permissions":{"allow":["B"]}}"#);
        write(&user, r#"{"permissions":{"allow":["C","A"]}}"#);
        let rec = move_record(
            side_at(Scope::Project, &project, perms(&["A", "B"]), perms(&["B"])),
            side_at(Scope::User, &user, perms(&["C"]), perms(&["C", "A"])),
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        let sides = apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        assert_eq!(
            io_atomic::load(&project)
                .unwrap()
                .unwrap()
                .permissions()
                .allow,
            vec!["A".to_string(), "B".to_string()],
        );
        assert_eq!(
            io_atomic::load(&user).unwrap().unwrap().permissions().allow,
            vec!["C".to_string()],
        );
        // The returned sides snapshot what the undo wrote, so the resulting
        // restore record is itself invertible.
        assert_eq!(sides.len(), 2);
        assert_eq!(sides[0].key_before, Some(perms(&["B"])));
        assert_eq!(sides[0].key_after, Some(perms(&["A", "B"])));
    }

    #[test]
    fn redo_of_a_move_re_applies_the_post_move_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        let user = paths.user.clone().unwrap();
        // Disk reflects the pre-move state (an undo has already happened).
        write(&project, r#"{"permissions":{"allow":["A","B"]}}"#);
        write(&user, r#"{"permissions":{"allow":["C"]}}"#);
        let rec = move_record(
            side_at(Scope::Project, &project, perms(&["A", "B"]), perms(&["B"])),
            side_at(Scope::User, &user, perms(&["C"]), perms(&["C", "A"])),
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Redo).unwrap();
        apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        assert_eq!(
            io_atomic::load(&project)
                .unwrap()
                .unwrap()
                .permissions()
                .allow,
            vec!["B".to_string()],
        );
        assert_eq!(
            io_atomic::load(&user).unwrap().unwrap().permissions().allow,
            vec!["C".to_string(), "A".to_string()],
        );
    }

    #[test]
    fn undo_then_redo_round_trips_to_the_original_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        let user = paths.user.clone().unwrap();
        write(&project, r#"{"permissions":{"allow":["B"]}}"#);
        write(&user, r#"{"permissions":{"allow":["C","A"]}}"#);
        let rec = move_record(
            side_at(Scope::Project, &project, perms(&["A", "B"]), perms(&["B"])),
            side_at(Scope::User, &user, perms(&["C"]), perms(&["C", "A"])),
        );
        let undo = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        apply_restore_plan(&undo, None, &WatchState::default()).unwrap();
        let redo = build_restore_plan(&rec, audit::RestoreDirection::Redo).unwrap();
        apply_restore_plan(&redo, None, &WatchState::default()).unwrap();
        // Back to the post-move state we started from.
        assert_eq!(
            io_atomic::load(&user).unwrap().unwrap().permissions().allow,
            vec!["C".to_string(), "A".to_string()],
        );
    }

    #[test]
    fn build_restore_plan_collapses_change_kind_to_one_target() {
        // A same-scope change-kind records its single file as both `from`
        // and `to`; the plan must write that file once, not twice.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("settings.json");
        let side = side_at(
            Scope::Project,
            &project,
            serde_json::json!({"allow": ["A"], "deny": []}),
            serde_json::json!({"allow": [], "deny": ["A"]}),
        );
        let rec = audit::Record::new(
            audit::Kind::ChangeKind,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            Some(side.clone()),
            Some(side),
            vec![key("permissions"), key("allow"), idx(0)],
            Some(PermissionKind::Deny),
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        assert_eq!(plan.targets.len(), 1);
    }

    #[test]
    fn restore_preview_flags_a_hand_edited_file_as_state_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        let user = paths.user.clone().unwrap();
        // The record's `key_after` for Project is {"allow":["B"]}, but the
        // file on disk says something else — an external hand-edit.
        write(&project, r#"{"permissions":{"allow":["B","HAND_EDITED"]}}"#);
        write(&user, r#"{"permissions":{"allow":["C","A"]}}"#);
        let rec = move_record(
            side_at(Scope::Project, &project, perms(&["A", "B"]), perms(&["B"])),
            side_at(Scope::User, &user, perms(&["C"]), perms(&["C", "A"])),
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        let preview = preview_restore_plan(&plan, &rec).unwrap();
        let project_side = preview
            .sides
            .iter()
            .find(|s| s.scope == Scope::Project)
            .unwrap();
        assert!(
            project_side.state_mismatch,
            "the hand-edited project file should flag a mismatch"
        );
        let user_side = preview
            .sides
            .iter()
            .find(|s| s.scope == Scope::User)
            .unwrap();
        assert!(
            !user_side.state_mismatch,
            "the untouched user file is not a mismatch"
        );
    }

    #[test]
    fn values_semantically_equal_ignores_object_key_order_recursively() {
        // #176 — Under preserve_order, serde_json::Value::eq is order-
        // sensitive. The semantic helper used by the restore equality
        // checks must treat object key order as presentation and not
        // data, while leaving arrays order-sensitive (a reordered
        // permissions.allow list is still a different document).
        let a = serde_json::json!({
            "permissions": {"allow": ["A"], "deny": ["B"]},
            "env": {"X": "1", "Y": "2"},
        });
        let b = serde_json::json!({
            "env": {"Y": "2", "X": "1"},
            "permissions": {"deny": ["B"], "allow": ["A"]},
        });
        assert!(values_semantically_equal(&a, &b));
        // Array reordering remains unequal.
        let c = serde_json::json!({"permissions": {"allow": ["A", "B"]}});
        let d = serde_json::json!({"permissions": {"allow": ["B", "A"]}});
        assert!(!values_semantically_equal(&c, &d));
        // Different keys aren't equal even with same length.
        let e = serde_json::json!({"foo": 1});
        let f = serde_json::json!({"bar": 1});
        assert!(!values_semantically_equal(&e, &f));
        // Option wrapper rules.
        assert!(opt_values_semantically_equal(&None, &None));
        assert!(!opt_values_semantically_equal(&None, &Some(a.clone())));
        assert!(opt_values_semantically_equal(&Some(a), &Some(b)));
    }

    #[test]
    fn restore_preview_does_not_flag_state_mismatch_on_pure_key_reordering() {
        // #176 — A user who reordered the keys inside permissions in
        // their project settings has not changed the document
        // semantically. After an op landed, undoing it expects
        // current == key_after; with preserve_order, a hand-reorder of
        // the post-op file would byte-differ from key_after and trip
        // state_mismatch. The semantic-equality helper must suppress
        // that false positive. Pinning with a real preview run rather
        // than just the helper unit-test so a future refactor that
        // changes how the equality is wired in (e.g. only state_mismatch
        // but not will_write) gets caught.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        // The on-disk file holds the same KV pairs as `key_after` below
        // but with keys in a different insertion order — same data,
        // different presentation.
        write(
            &project,
            r#"{"permissions":{"deny":["X"],"allow":["A","B"]}}"#,
        );
        let side = side_at(
            Scope::Project,
            &project,
            serde_json::json!({"allow": ["A"], "deny": ["X"]}),
            serde_json::json!({"allow": ["A", "B"], "deny": ["X"]}),
        );
        let rec = audit::Record::new(
            audit::Kind::Add,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            None,
            Some(side),
            vec![key("permissions"), key("allow"), idx(1)],
            None,
        );
        // Undo: target_value = key_before, expected_current = key_after.
        // Disk matches key_after semantically (modulo key order), so
        // state_mismatch should be false.
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        let preview = preview_restore_plan(&plan, &rec).unwrap();
        let project_side = preview
            .sides
            .iter()
            .find(|s| s.scope == Scope::Project)
            .unwrap();
        assert!(
            !project_side.state_mismatch,
            "pure key reordering of the post-op file must not register as state_mismatch"
        );
    }

    #[test]
    fn apply_restore_plan_skips_write_when_disk_matches_target_in_different_key_order() {
        // #176 — The apply path's phase-2 no-change skip must also be
        // order-insensitive. Without the fix, the file on disk holds
        // the target KV pairs in a different order than the audit
        // snapshot; phase 2's `before == t.target_value` evaluates
        // false on insertion-order grounds, and io_atomic::save then
        // overwrites the user's reordering with the snapshot's order
        // — silent data loss. With the fix, the apply skips the write.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        // Pre-state mtime captured before apply; if no write happens
        // it should be byte-identical after the call.
        let pre_disk = r#"{"permissions":{"deny":["X"],"allow":["A","B"]}}"#;
        write(&project, pre_disk);
        let side = side_at(
            Scope::Project,
            &project,
            serde_json::json!({"allow": ["A"], "deny": ["X"]}),
            serde_json::json!({"allow": ["A", "B"], "deny": ["X"]}),
        );
        let rec = audit::Record::new(
            audit::Kind::Add,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            None,
            Some(side),
            vec![key("permissions"), key("allow"), idx(1)],
            None,
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Redo).unwrap();
        apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        // The file should still hold the user's deny-first key order;
        // no save should have rewritten it.
        let after = std::fs::read_to_string(&project).unwrap();
        assert!(
            after.contains(r#""deny":["X"],"allow":["A","B"]"#),
            "file was rewritten — user's key reordering was lost: {after}"
        );
    }

    #[test]
    fn undo_of_an_add_removes_the_added_rule() {
        // An add record carries only a `to` side; its undo removes the rule.
        let tmp = tempfile::tempdir().unwrap();
        let user = paths_in(tmp.path()).user.clone().unwrap();
        write(&user, r#"{"permissions":{"allow":["C","A"]}}"#);
        let rec = audit::Record::new(
            audit::Kind::Add,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            None,
            Some(side_at(
                Scope::User,
                &user,
                perms(&["C"]),
                perms(&["C", "A"]),
            )),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        assert_eq!(
            io_atomic::load(&user).unwrap().unwrap().permissions().allow,
            vec!["C".to_string()],
        );
    }

    // -- restore-to-point (#125) -------------------------------------------

    #[test]
    fn restore_to_point_reverts_the_target_and_everything_after() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        let user = paths.user.clone().unwrap();
        // Disk reflects the state after two moves: A then B, Project → User.
        write(&project, r#"{"permissions":{"allow":["C"]}}"#);
        write(&user, r#"{"permissions":{"allow":["A","B"]}}"#);
        let m1 = move_record(
            side_at(
                Scope::Project,
                &project,
                perms(&["A", "B", "C"]),
                perms(&["B", "C"]),
            ),
            side_at(Scope::User, &user, perms(&[]), perms(&["A"])),
        );
        let m2 = move_record(
            side_at(Scope::Project, &project, perms(&["B", "C"]), perms(&["C"])),
            side_at(Scope::User, &user, perms(&["A"]), perms(&["A", "B"])),
        );
        let records = vec![m1.clone(), m2];
        let plan = plan_restore_to(&records, m1.id).unwrap();
        assert_eq!(plan.ops_spanned, 2, "two entries spanned");
        apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        // Both files are back to the pre-m1 state.
        assert_eq!(
            io_atomic::load(&project)
                .unwrap()
                .unwrap()
                .permissions()
                .allow,
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
        );
        assert!(io_atomic::load(&user)
            .unwrap()
            .unwrap()
            .permissions()
            .allow
            .is_empty());
    }

    #[test]
    fn restore_to_point_on_the_last_entry_reverts_just_that_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let project = paths.project.clone().unwrap();
        let user = paths.user.clone().unwrap();
        write(&project, r#"{"permissions":{"allow":[]}}"#);
        write(&user, r#"{"permissions":{"allow":["A"]}}"#);
        let m1 = move_record(
            side_at(Scope::Project, &project, perms(&["A"]), perms(&[])),
            side_at(Scope::User, &user, perms(&[]), perms(&["A"])),
        );
        let plan = plan_restore_to(std::slice::from_ref(&m1), m1.id).unwrap();
        assert_eq!(plan.ops_spanned, 1);
        apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        assert_eq!(
            io_atomic::load(&project)
                .unwrap()
                .unwrap()
                .permissions()
                .allow,
            vec!["A".to_string()],
        );
        assert!(io_atomic::load(&user)
            .unwrap()
            .unwrap()
            .permissions()
            .allow
            .is_empty());
    }

    #[test]
    fn restore_to_point_rejects_a_restore_entry_as_target() {
        // Restoring to before an undo is confusing — the user should target
        // an original write instead.
        let restore = audit::Record::new_restore(
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            vec![],
            audit::RestoreMeta {
                target_id: Ulid::new(),
                direction: audit::RestoreDirection::Undo,
                files: vec![],
            },
        );
        let err = plan_restore_to(std::slice::from_ref(&restore), restore.id).unwrap_err();
        assert!(err.to_string().contains("itself a restore"));
    }

    #[test]
    fn restore_to_point_rejects_an_unknown_target() {
        let err = plan_restore_to(&[], Ulid::new()).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    /// Helper for the multi-key dedupe regression tests below: build a Side
    /// with a caller-supplied `top_level_key` so we can synthesize records
    /// that touch two keys in one file (the case #163 / #167 exposed).
    fn side_at_key(
        scope: Scope,
        path: &Path,
        top_level_key: &str,
        before: serde_json::Value,
        after: serde_json::Value,
    ) -> audit::Side {
        audit::Side {
            scope,
            file_path: path.to_path_buf(),
            top_level_key: top_level_key.to_string(),
            key_before: Some(before),
            key_after: Some(after),
        }
    }

    #[test]
    fn apply_restore_plan_coalesces_two_targets_in_one_file() {
        // Regression for codex 4th-pass [P1]. Two restore targets on
        // the same file (different top-level keys) used to load the
        // file twice from the same stamp and save twice; the second
        // save tripped ConcurrentModification (or, worse, overwrote
        // the first key's restore on coarse-mtime filesystems). The
        // fix loads once per file and accumulates per-key mutations
        // into a single new_doc + a single save.
        let tmp = tempfile::tempdir().unwrap();
        let project = paths_in(tmp.path()).project.clone().unwrap();
        // Current on-disk state — both keys present in their
        // post-change form.
        write(
            &project,
            r#"{"env":{"X":"1","Y":"2"},"permissions":{"allow":["A"]}}"#,
        );

        let plan = RestorePlan {
            direction: audit::RestoreDirection::ToPoint,
            target_id: Ulid::new(),
            leaf_kind: audit::LeafKind::PermissionRule,
            project_dir: None,
            path: vec![],
            ops_spanned: 2,
            targets: vec![
                RestoreTarget {
                    scope: Scope::Project,
                    file_path: project.clone(),
                    top_level_key: "env".to_string(),
                    target_value: Some(serde_json::json!({"X": "1"})),
                    expected_current: Some(serde_json::json!({"X": "1", "Y": "2"})),
                },
                RestoreTarget {
                    scope: Scope::Project,
                    file_path: project.clone(),
                    top_level_key: "permissions".to_string(),
                    target_value: Some(serde_json::json!({"allow": []})),
                    expected_current: Some(serde_json::json!({"allow": ["A"]})),
                },
            ],
        };

        let sides = apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        // Both Sides returned, in plan order, each carrying its own
        // key's before/after.
        assert_eq!(sides.len(), 2);
        assert_eq!(sides[0].top_level_key, "env");
        assert_eq!(sides[1].top_level_key, "permissions");

        // The file holds BOTH restored keys, not just one of them.
        let on_disk = std::fs::read_to_string(&project).unwrap();
        let v: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(v["env"], serde_json::json!({"X": "1"}));
        assert_eq!(v["permissions"], serde_json::json!({"allow": []}));
    }

    #[test]
    fn validate_audit_records_refuses_path_outside_scope_allowlist() {
        // Regression for #183 (security). A hostile audit-log line
        // carrying a `file_path` outside the resolved ScopePaths for
        // its `project_dir` must be rejected before any restore plan
        // is built. Otherwise the apply path would write attacker-
        // controlled JSON to an attacker-chosen file.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());

        // Hostile record claims its project is the test's project, but
        // its Side's file_path points at a totally different file.
        let hostile = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            Some(paths.project_dir.clone()),
            None,
            Some(audit::Side {
                scope: Scope::User,
                file_path: tmp.path().join("malicious-target.json"),
                top_level_key: "permissions".to_string(),
                key_before: Some(serde_json::json!({})),
                key_after: Some(serde_json::json!({"allow": ["pwn"]})),
            }),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );

        let home = tmp.path().join("home");
        let err = validate_audit_records(std::slice::from_ref(&hostile), Some(&home))
            .expect_err("must refuse path outside scope allowlist");
        let msg = err.to_string();
        assert!(
            msg.contains("path injection refused"),
            "expected path-injection refusal, got: {msg}"
        );
    }

    #[test]
    fn validate_audit_records_refuses_non_settings_basename() {
        // Defense-in-depth: even a path the resolver might somehow
        // produce must end in `settings.json` or `settings.local.json`.
        // Catches hostile records that try to use the home dir
        // structure to claim a non-settings basename.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());

        let hostile = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            Some(paths.project_dir.clone()),
            None,
            Some(audit::Side {
                scope: Scope::Project,
                file_path: paths.project_dir.join(".claude").join("config.json"), // wrong basename
                top_level_key: "permissions".to_string(),
                key_before: None,
                key_after: Some(serde_json::json!({"allow": ["x"]})),
            }),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );

        let err = validate_audit_records(std::slice::from_ref(&hostile), None)
            .expect_err("must refuse non-settings basename");
        let msg = err.to_string();
        assert!(
            msg.contains("does not look like a Claude Code settings file"),
            "expected basename refusal, got: {msg}"
        );
    }

    #[test]
    fn validate_audit_records_accepts_legitimate_record() {
        // Sanity: a record produced for a legitimate scope file under
        // the resolved project must pass.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());

        let legit = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            Some(paths.project_dir.clone()),
            Some(audit::Side {
                scope: Scope::Project,
                file_path: paths.project.clone().unwrap(),
                top_level_key: "permissions".to_string(),
                key_before: Some(serde_json::json!({"allow": ["A"]})),
                key_after: Some(serde_json::json!({"allow": []})),
            }),
            Some(audit::Side {
                scope: Scope::User,
                file_path: paths.user.clone().unwrap(),
                top_level_key: "permissions".to_string(),
                key_before: Some(serde_json::json!({"allow": []})),
                key_after: Some(serde_json::json!({"allow": ["A"]})),
            }),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );

        // home_dir matches what paths_in used for user_home.
        let home = tmp.path().join("home");
        validate_audit_records(std::slice::from_ref(&legit), Some(&home))
            .expect("legitimate record must pass");
    }

    #[test]
    fn validate_audit_records_accepts_cross_project_move_with_both_project_dirs() {
        // #179 — A Move record whose from-side lives in Project A and
        // to-side in Project B must validate. Each side's file_path is
        // checked against the allowlist of its OWN project_dir (the
        // record's `project_dir` for from, `project_dir_to` for to).
        // Without the per-side dispatch the to-side's path would be
        // rejected as "outside Project A's allowlist."
        let tmp = tempfile::tempdir().unwrap();
        let project_a = tmp.path().join("a");
        let project_b = tmp.path().join("b");
        std::fs::create_dir_all(project_a.join(".claude")).unwrap();
        std::fs::create_dir_all(project_b.join(".claude")).unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();

        let from_path = project_a.join(".claude").join("settings.local.json");
        let to_path = project_b.join(".claude").join("settings.local.json");
        std::fs::write(&from_path, r#"{"permissions":{"allow":[]}}"#).unwrap();
        std::fs::write(&to_path, r#"{"permissions":{"allow":["X"]}}"#).unwrap();

        let record = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            Some(project_a.clone()),
            Some(audit::Side {
                scope: Scope::Local,
                file_path: from_path,
                top_level_key: "permissions".to_string(),
                key_before: Some(serde_json::json!({"allow": ["X"]})),
                key_after: Some(serde_json::json!({"allow": []})),
            }),
            Some(audit::Side {
                scope: Scope::Local,
                file_path: to_path,
                top_level_key: "permissions".to_string(),
                key_before: Some(serde_json::json!({"allow": []})),
                key_after: Some(serde_json::json!({"allow": ["X"]})),
            }),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        )
        .with_project_dir_to(Some(project_b));

        validate_audit_records(std::slice::from_ref(&record), Some(&home))
            .expect("cross-project record must pass with both project_dirs");
    }

    #[test]
    fn validate_audit_records_refuses_cross_project_path_outside_both_allowlists() {
        // Defense: tagging a record with `project_dir_to` doesn't open
        // a wider injection surface. A file_path that's outside BOTH
        // project allowlists still gets rejected.
        let tmp = tempfile::tempdir().unwrap();
        let project_a = tmp.path().join("a");
        let project_b = tmp.path().join("b");
        std::fs::create_dir_all(project_a.join(".claude")).unwrap();
        std::fs::create_dir_all(project_b.join(".claude")).unwrap();
        let home = tmp.path().join("home");

        let hostile_path = tmp.path().join("malicious.json");
        let record = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            Some(project_a),
            None,
            Some(audit::Side {
                scope: Scope::Local,
                file_path: hostile_path,
                top_level_key: "permissions".to_string(),
                key_before: None,
                key_after: Some(serde_json::json!({"allow": ["pwn"]})),
            }),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        )
        .with_project_dir_to(Some(project_b));

        let err = validate_audit_records(std::slice::from_ref(&record), Some(&home))
            .expect_err("must refuse path outside both project allowlists");
        assert!(err.to_string().contains("path injection refused"));
    }

    #[test]
    fn require_clean_audit_log_refuses_when_skipped_nonzero() {
        // Regression for codex 2nd-pass [P1]: every undo/redo/restore
        // GUI command must refuse a partial log, not just the topbar
        // pre-check via `audit_undo_status`. Write a malformed line
        // into the audit log and assert the gate rejects it.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let overrides = RuntimeOverrides {
            home: Some(home.to_path_buf()),
            project: None,
        };

        // Write the audit dir and drop a malformed line.
        let audit_dir = home.join(".claude").join("claude-scope");
        std::fs::create_dir_all(&audit_dir).unwrap();
        std::fs::write(
            audit_dir.join("audit.jsonl"),
            b"{ this is not a valid audit record }\n",
        )
        .unwrap();

        let err = require_clean_audit_log(&overrides).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unreadable") && msg.contains("partial log"),
            "expected staleness/partial-log message, got: {msg}"
        );
    }

    #[test]
    fn build_restore_plan_keeps_two_keys_in_one_file_as_distinct_targets() {
        // Regression for #167. A single record whose `from` and `to` sides
        // reference the same settings.json under different top-level keys
        // must produce two restore targets — the pre-fix dedupe keyed on
        // file_path alone collapsed the second one silently.
        let tmp = tempfile::tempdir().unwrap();
        let project = paths_in(tmp.path()).project.clone().unwrap();
        let rec = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            Some(side_at_key(
                Scope::Project,
                &project,
                "permissions",
                perms(&["A"]),
                perms(&[]),
            )),
            Some(side_at_key(
                Scope::Project,
                &project,
                "env",
                serde_json::json!({"FOO": "1"}),
                serde_json::json!({"FOO": "1", "BAR": "2"}),
            )),
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        assert_eq!(
            plan.targets.len(),
            2,
            "both top-level keys should survive the dedupe"
        );
        let keys: Vec<&str> = plan
            .targets
            .iter()
            .map(|t| t.top_level_key.as_str())
            .collect();
        assert!(keys.contains(&"permissions"));
        assert!(keys.contains(&"env"));
    }

    #[test]
    fn build_restore_plan_still_collapses_same_file_same_key() {
        // The pre-fix dedupe existed for a real reason: a same-scope
        // change-kind records its one (file, key) tuple as both `from` and
        // `to`. Keep that case collapsed.
        let tmp = tempfile::tempdir().unwrap();
        let project = paths_in(tmp.path()).project.clone().unwrap();
        let rec = audit::Record::new(
            audit::Kind::ChangeKind,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            Some(side_at(
                Scope::Project,
                &project,
                serde_json::json!({"allow": ["A"], "deny": []}),
                serde_json::json!({"allow": [], "deny": ["A"]}),
            )),
            Some(side_at(
                Scope::Project,
                &project,
                serde_json::json!({"allow": ["A"], "deny": []}),
                serde_json::json!({"allow": [], "deny": ["A"]}),
            )),
            vec![key("permissions"), key("allow"), idx(0)],
            Some(PermissionKind::Deny),
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        assert_eq!(plan.targets.len(), 1, "same (file, key) must collapse");
    }

    #[test]
    fn plan_restore_to_keeps_two_keys_in_one_file_as_distinct_targets() {
        // Regression for #163. A restore window that touches the same
        // settings.json under two different top-level keys must produce
        // two targets — the pre-fix HashMap<PathBuf, _> dropped the
        // second key's `top_level_key`/`target_value` on the floor.
        let tmp = tempfile::tempdir().unwrap();
        let project = paths_in(tmp.path()).project.clone().unwrap();
        // Two consecutive ops on the same file under different keys.
        let r_env = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::TopLevelKey,
            audit::Actor::Gui,
            None,
            Some(side_at_key(
                Scope::Project,
                &project,
                "env",
                serde_json::json!({"FOO": "1"}),
                serde_json::json!({"FOO": "1", "BAR": "2"}),
            )),
            None,
            vec![key("env")],
            None,
        );
        let r_perms = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            Some(side_at_key(
                Scope::Project,
                &project,
                "permissions",
                perms(&[]),
                perms(&["A"]),
            )),
            None,
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );
        let plan = plan_restore_to(&[r_env.clone(), r_perms], r_env.id).unwrap();
        assert_eq!(
            plan.targets.len(),
            2,
            "both top-level keys should survive the window dedupe"
        );
        let keys: Vec<&str> = plan
            .targets
            .iter()
            .map(|t| t.top_level_key.as_str())
            .collect();
        assert!(keys.contains(&"permissions"));
        assert!(keys.contains(&"env"));
        // `target_value` per target is the per-key window-start state
        // (the first `key_before` seen for that key).
        let env_target = plan
            .targets
            .iter()
            .find(|t| t.top_level_key == "env")
            .unwrap();
        assert_eq!(
            env_target.target_value,
            Some(serde_json::json!({"FOO": "1"}))
        );
    }

    #[test]
    fn restore_to_point_preview_captures_trailing_ulid_for_apply_recheck() {
        // Regression for #171. The apply path compares the trailing
        // record's ULID against this `tail_id` to detect a concurrent
        // rotate+append that left `ops_spanned` coincidentally identical
        // but the window's contents different. The preview must populate
        // `tail_id` from the same read that fed `plan_restore_to`.
        let tmp = tempfile::tempdir().unwrap();
        let project = paths_in(tmp.path()).project.clone().unwrap();
        write(&project, r#"{"permissions":{"allow":["A"]}}"#);
        let target = move_record(
            side_at(Scope::Project, &project, perms(&["A"]), perms(&[])),
            side_at(Scope::User, &project, perms(&[]), perms(&["A"])),
        );
        let records = vec![target.clone()];
        let plan = plan_restore_to(&records, target.id).unwrap();
        let mut preview = preview_restore_plan(&plan, &target).unwrap();
        // `preview_restore_plan` alone doesn't see the log; the
        // higher-level `restore_to_point_preview` populates `tail_id`.
        assert!(
            preview.tail_id.is_none(),
            "preview_restore_plan should not set tail_id"
        );
        preview.tail_id = records.last().map(|r| r.id);
        // After populating, the tail should equal the target (only one
        // entry in the log).
        assert_eq!(preview.tail_id, Some(target.id));
    }

    #[test]
    fn tail_ulid_changes_when_audit_log_grows() {
        // Demonstrates the detection mechanism the #171 fix relies on.
        // If a concurrent process appends to the audit log between
        // preview and apply, the trailing ULID changes — even if the
        // window's `ops_spanned` stays coincidentally identical.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let a = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            None,
            None,
            vec![],
            None,
        );
        audit::append(&a, Some(home)).unwrap();
        // Capture trailing ULID — what the preview would record.
        let (snapshot, _) = audit::read_all(Some(home)).unwrap();
        let expected_tail = snapshot.last().map(|r| r.id);

        // Concurrent writer appends.
        let b = audit::Record::new(
            audit::Kind::Add,
            audit::LeafKind::PermissionRule,
            audit::Actor::Cli,
            None,
            None,
            None,
            vec![],
            None,
        );
        audit::append(&b, Some(home)).unwrap();

        // Apply-side re-read: tail has shifted.
        let (after, _) = audit::read_all(Some(home)).unwrap();
        let current_tail = after.last().map(|r| r.id);
        assert_ne!(
            current_tail, expected_tail,
            "trailing ULID must change after a concurrent append — this is the signal apply_restore_to_point uses to detect staleness (#171)"
        );
    }

    #[test]
    fn a_restore_to_point_entry_is_itself_undoable() {
        // A restore-to-point lands a `Kind::Restore` record; undoing it
        // walks `restore.files` and puts the file back.
        let tmp = tempfile::tempdir().unwrap();
        let project = paths_in(tmp.path()).project.clone().unwrap();
        write(&project, r#"{"permissions":{"allow":["NEW"]}}"#);
        let rec = audit::Record::new_restore(
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            vec![key("permissions"), key("allow"), idx(0)],
            audit::RestoreMeta {
                target_id: Ulid::new(),
                direction: audit::RestoreDirection::ToPoint,
                files: vec![side_at(
                    Scope::Project,
                    &project,
                    perms(&["OLD"]),
                    perms(&["NEW"]),
                )],
            },
        );
        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        apply_restore_plan(&plan, None, &WatchState::default()).unwrap();
        assert_eq!(
            io_atomic::load(&project)
                .unwrap()
                .unwrap()
                .permissions()
                .allow,
            vec!["OLD".to_string()],
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_restore_plan_rolls_back_an_earlier_file_when_a_later_one_fails() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let good_dir = tmp.path().join("good");
        let bad_dir = tmp.path().join("bad");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::create_dir_all(&bad_dir).unwrap();
        let good = good_dir.join("settings.json");
        let bad = bad_dir.join("settings.json");
        write(&good, r#"{"permissions":{"allow":["POST"]}}"#);
        write(&bad, r#"{"permissions":{"allow":["POST"]}}"#);
        // An undo plan targets `good` then `bad` (record_sides order).
        let rec = move_record(
            side_at(Scope::Project, &good, perms(&["PRE"]), perms(&["POST"])),
            side_at(Scope::User, &bad, perms(&["PRE"]), perms(&["POST"])),
        );
        // Lock the bad file's directory so its atomic write can't create a
        // tempfile — phase 2 fails on the second target.
        let mut ro = std::fs::metadata(&bad_dir).unwrap().permissions();
        ro.set_mode(0o555);
        std::fs::set_permissions(&bad_dir, ro).unwrap();

        let plan = build_restore_plan(&rec, audit::RestoreDirection::Undo).unwrap();
        let result = apply_restore_plan(&plan, None, &WatchState::default());

        // Re-grant write so TempDir cleanup and the load below can proceed.
        let mut rw = std::fs::metadata(&bad_dir).unwrap().permissions();
        rw.set_mode(0o755);
        std::fs::set_permissions(&bad_dir, rw).unwrap();

        assert!(result.is_err(), "the locked file should fail the batch");
        // `good` was written, then rolled back to its pre-restore content.
        assert_eq!(
            io_atomic::load(&good).unwrap().unwrap().permissions().allow,
            vec!["POST".to_string()],
            "the earlier file must be rolled back, not left half-restored",
        );
    }

    // -- delete-leaf tests (#8) --------------------------------------------

    #[test]
    fn delete_leaf_removes_permission_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(rm)","Read(**)"]}}"#,
        );

        apply_delete_leaf_impl(
            &paths,
            &DeleteLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(doc.permissions().allow, vec!["Read(**)".to_string()]);
    }

    #[test]
    fn delete_leaf_strips_all_duplicates_of_rule_string() {
        // Same legacy-parity contract as the move flow: a delete from a
        // hand-edited array with duplicate rule strings must strip every
        // copy, not just the indexed one.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(rm)","Bash(rm)","Read(**)"]}}"#,
        );

        apply_delete_leaf_impl(
            &paths,
            &DeleteLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(doc.permissions().allow, vec!["Read(**)".to_string()]);
    }

    #[test]
    fn delete_leaf_removes_top_level_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"theme":"dark","env":{"PATH":"/x"}}"#,
        );

        apply_delete_leaf_impl(
            &paths,
            &DeleteLeafRequest {
                path: vec![key("theme")],
                from: Scope::Project,
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.project.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(doc.get_top_level("theme").is_none());
        assert!(doc.get_top_level("env").is_some());
    }

    #[test]
    fn delete_leaf_errors_when_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{"permissions":{}}"#);
        let err = apply_delete_leaf_impl(
            &paths,
            &DeleteLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn diff_delete_leaf_returns_before_after_and_destructive_note() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(rm)","Read(**)"]}}"#,
        );

        let preview = diff_delete_leaf_impl(
            &paths,
            &DeleteLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                from: Scope::Project,
            },
        )
        .unwrap();
        assert!(matches!(preview.kind, MoveLeafKind::PermissionRule));
        assert_eq!(
            preview.from.key_before.unwrap(),
            serde_json::json!({"allow": ["Bash(rm)", "Read(**)"]})
        );
        assert_eq!(
            preview.from.key_after.unwrap(),
            serde_json::json!({"allow": ["Read(**)"]})
        );
        assert!(preview.from.will_write);
        let note = preview.from.note.unwrap();
        assert!(note.contains("removed"));
        assert!(note.contains(".bak"));
    }

    // -- add-leaf tests (#8) -----------------------------------------------

    #[test]
    fn add_leaf_pushes_permission_rule_into_destination_array() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Read(**)"]}}"#,
        );

        apply_add_leaf_impl(
            &paths,
            &AddLeafRequest {
                path: vec![key("permissions"), key("deny"), idx(0)],
                to: Scope::User,
                value: serde_json::json!("WebFetch(domain:evil.example)"),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({
                "allow": ["Read(**)"],
                "deny": ["WebFetch(domain:evil.example)"]
            })
        );
    }

    #[test]
    fn add_leaf_creates_destination_file_when_missing() {
        // Pasting into a scope that has no settings file yet must create
        // the file via the same `merge_at_path` + atomic-write path the
        // move flow uses for first-write destinations.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());

        apply_add_leaf_impl(
            &paths,
            &AddLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                to: Scope::User,
                value: serde_json::json!("Bash(ls)"),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(doc.permissions().allow, vec!["Bash(ls)".to_string()]);
    }

    #[test]
    fn add_leaf_idempotent_when_rule_already_present() {
        // Pasting a rule that's already in the destination is a no-op write
        // — the apply path returns Ok without touching the file. The check
        // is the destination's file size (or, here, that the load round-trip
        // returns the same shape). No error is raised because paste UX
        // shouldn't punish the user for a harmless duplicate.
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(ls)"]}}"#,
        );

        apply_add_leaf_impl(
            &paths,
            &AddLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                to: Scope::User,
                value: serde_json::json!("Bash(ls)"),
            },
            Some(&BackupTracker::new()),
            &WatchState::default(),
        )
        .unwrap();

        let doc = io_atomic::load(paths.user.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(doc.permissions().allow, vec!["Bash(ls)".to_string()]);
    }

    #[test]
    fn diff_add_leaf_returns_before_after_and_will_write() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Read(**)"]}}"#,
        );

        let preview = diff_add_leaf_impl(
            &paths,
            &AddLeafRequest {
                path: vec![key("permissions"), key("deny"), idx(0)],
                to: Scope::User,
                value: serde_json::json!("WebFetch(domain:evil.example)"),
            },
        )
        .unwrap();
        assert!(matches!(preview.kind, MoveLeafKind::PermissionRule));
        assert_eq!(
            preview.to.key_before.unwrap(),
            serde_json::json!({"allow": ["Read(**)"]})
        );
        assert_eq!(
            preview.to.key_after.unwrap(),
            serde_json::json!({
                "allow": ["Read(**)"],
                "deny": ["WebFetch(domain:evil.example)"]
            })
        );
        assert!(preview.to.will_write);
    }

    #[test]
    fn diff_add_leaf_marks_idempotent_paste_as_no_write() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.user.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(ls)"]}}"#,
        );

        let preview = diff_add_leaf_impl(
            &paths,
            &AddLeafRequest {
                path: vec![key("permissions"), key("allow"), idx(0)],
                to: Scope::User,
                value: serde_json::json!("Bash(ls)"),
            },
        )
        .unwrap();
        assert!(!preview.to.will_write);
    }

    // -- audit helpers (#19 Phase 1) ---------------------------------------

    #[test]
    fn audit_leaf_kind_classifies_paths_by_shape() {
        // Single-segment path is always a top-level key move (e.g. moving
        // the whole `env` block between scopes).
        assert_eq!(audit_leaf_kind(&[key("env")]), audit::LeafKind::TopLevelKey);
        // Two-segment under `permissions` is a whole-list move.
        assert_eq!(
            audit_leaf_kind(&[key("permissions"), key("allow")]),
            audit::LeafKind::PermissionList
        );
        // Three-segment under `permissions` is a single rule.
        assert_eq!(
            audit_leaf_kind(&[key("permissions"), key("allow"), idx(2)]),
            audit::LeafKind::PermissionRule
        );
    }

    #[test]
    fn snapshot_top_level_key_reads_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, r#"{"permissions":{"allow":["Bash(ls)"]}}"#).unwrap();
        let snap = snapshot_top_level_key(&path, "permissions").unwrap();
        assert_eq!(snap, serde_json::json!({"allow": ["Bash(ls)"]}));
    }

    #[test]
    fn snapshot_top_level_key_returns_none_for_missing_file_or_key() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.json");
        // File absent — None, not an error.
        assert!(snapshot_top_level_key(&missing, "permissions").is_none());

        let present = tmp.path().join("settings.json");
        std::fs::write(&present, r#"{"theme":"dark"}"#).unwrap();
        // File present but no `permissions` key — also None. The audit
        // schema uses `None` for both "absent file" and "absent key"
        // alike, so this collapsing matches the wire contract.
        assert!(snapshot_top_level_key(&present, "permissions").is_none());
    }

    #[test]
    fn audit_side_returns_none_for_scope_with_no_path() {
        // Some scopes have no file path on disk (e.g. user scope
        // disabled). The Side builder must skip the field — emitting a
        // Side with `file_path: ""` would be wire-format garbage.
        let side = audit_side(
            Scope::User,
            None,
            "permissions",
            None,
            Some(serde_json::json!({"allow": []})),
        );
        assert!(side.is_none());
    }

    /// End-to-end: run a real apply_move_leaf_impl, then synthesize the
    /// same audit record the Tauri command would write, append it through
    /// the audit module, and read it back. Validates the wire contract
    /// against a realistic operation rather than only exercising helpers
    /// in isolation.
    #[test]
    fn audit_record_for_apply_move_round_trips_through_log() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{"permissions":{"allow":["Bash(ls)"]}}"#,
        );
        write(paths.user.as_ref().unwrap(), r#"{}"#);

        let req = MoveLeafRequest {
            path: vec![key("permissions"), key("allow"), idx(0)],
            from: Scope::Project,
            to: Scope::User,
            to_kind: None,
        };

        // Mirror what the Tauri command does: pre-snapshot, apply,
        // post-snapshot, build record, append.
        let key_name = path_top_level_key(&req.path).to_string();
        let from_before = snapshot_top_level_key(paths.project.as_ref().unwrap(), &key_name);
        let to_before = snapshot_top_level_key(paths.user.as_ref().unwrap(), &key_name);

        apply_move_leaf_impl(&paths, &paths, &req, None, &WatchState::default()).unwrap();

        let from_after = snapshot_top_level_key(paths.project.as_ref().unwrap(), &key_name);
        let to_after = snapshot_top_level_key(paths.user.as_ref().unwrap(), &key_name);

        let rec = audit::Record::new(
            audit::Kind::Move,
            audit_leaf_kind(&req.path),
            audit::Actor::Gui,
            Some(paths.project_dir.clone()),
            audit_side(
                req.from,
                paths.path_for(req.from),
                &key_name,
                from_before,
                from_after,
            ),
            audit_side(
                req.to,
                paths.path_for(req.to),
                &key_name,
                to_before,
                to_after,
            ),
            req.path.clone(),
            req.to_kind,
        );

        // Sandbox the audit append into the test's tempdir — passing
        // `Some(tmp.path())` redirects audit_path away from the real
        // ~/.claude/.
        audit::append(&rec, Some(tmp.path())).unwrap();
        let (records, skipped) = audit::read_all(Some(tmp.path())).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(records.len(), 1);
        let read = &records[0];
        assert_eq!(read.kind, audit::Kind::Move);
        assert_eq!(read.leaf_kind, audit::LeafKind::PermissionRule);
        // The before snapshot must capture the rule we moved; the after
        // snapshot must show it gone from the source. This is the
        // restore-feasibility property — Phase 3+ needs both sides.
        let from_side = read.from.as_ref().unwrap();
        assert_eq!(
            from_side.key_before,
            Some(serde_json::json!({"allow": ["Bash(ls)"]}))
        );
        assert!(
            from_side
                .key_after
                .as_ref()
                .and_then(|v| v.get("allow"))
                .and_then(|a| a.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(false),
            "after snapshot should show the rule removed from source: got {:?}",
            from_side.key_after
        );
        let to_side = read.to.as_ref().unwrap();
        assert!(
            to_side
                .key_after
                .as_ref()
                .and_then(|v| v.get("allow"))
                .and_then(|a| a.as_array())
                .map(|a| a.iter().any(|x| x == "Bash(ls)"))
                .unwrap_or(false),
            "after snapshot should show the rule landed on destination: got {:?}",
            to_side.key_after
        );
    }

    // -- list_audit_records / AuditRecordView (#19 phase 2) ----------------

    /// Build the same `AuditLogPage` shape the Tauri command emits, but
    /// reaching past the `State<RuntimeOverrides>` wrapper that's awkward
    /// to construct in a unit test. Verifies the ts_ms derivation is
    /// stable across the ULID → wire round trip — that's the property
    /// the History UI binds its timestamp rendering to.
    #[test]
    fn audit_record_view_decodes_ts_ms_from_ulid() {
        let tmp = tempfile::tempdir().unwrap();
        // Append two records spaced apart so ts_ms differences are
        // observable. The audit module's own tests cover empty / truncated
        // edge cases; this one only validates the view layer.
        let rec1 = audit::Record::new(
            audit::Kind::Move,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            None,
            None,
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );
        std::thread::sleep(std::time::Duration::from_millis(3));
        let rec2 = audit::Record::new(
            audit::Kind::Add,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            None,
            None,
            vec![key("permissions"), key("deny"), idx(0)],
            None,
        );
        audit::append(&rec1, Some(tmp.path())).unwrap();
        audit::append(&rec2, Some(tmp.path())).unwrap();

        let (records, skipped) = audit::read_all(Some(tmp.path())).unwrap();
        assert_eq!(skipped, 0);
        let views: Vec<AuditRecordView> = records
            .into_iter()
            .map(|r| {
                let ts_ms = r.id.timestamp_ms();
                AuditRecordView { record: r, ts_ms }
            })
            .collect();

        // The derived ts_ms matches the ULID's embedded timestamp — that's
        // the contract the History UI binds its rendering to.
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].ts_ms, rec1.id.timestamp_ms());
        assert_eq!(views[1].ts_ms, rec2.id.timestamp_ms());
        // And the second record's ts_ms is strictly greater — the
        // 3ms sleep above guarantees ULID monotonicity.
        assert!(views[1].ts_ms > views[0].ts_ms);
    }

    #[test]
    fn audit_record_view_serializes_flattened_with_ts_ms() {
        // Pin the wire format: `ts_ms` appears at the top level alongside
        // the record's own fields (flatten), not nested. The History UI's
        // TS interface mirrors this shape; a serde regression here would
        // break the frontend silently.
        let rec = audit::Record::new(
            audit::Kind::Delete,
            audit::LeafKind::PermissionRule,
            audit::Actor::Gui,
            None,
            None,
            None,
            vec![key("permissions"), key("allow"), idx(0)],
            None,
        );
        let ts_ms = rec.id.timestamp_ms();
        let view = AuditRecordView { record: rec, ts_ms };
        let json: serde_json::Value = serde_json::to_value(&view).unwrap();
        assert_eq!(json.get("kind").and_then(|v| v.as_str()), Some("delete"));
        assert_eq!(json.get("ts_ms").and_then(|v| v.as_u64()), Some(ts_ms));
        // `record` is flattened — there should NOT be a nested `record` key.
        assert!(json.get("record").is_none());
    }
}
