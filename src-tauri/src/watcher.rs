//! Filesystem watcher for the three recognized scope files.
//!
//! On every successful `load_scopes` call we (re)install a debounced watcher
//! over a small set of directories that bracket the Local, Project, and User
//! scope files. When any watched path matches one of our scope paths — or the
//! `.claude/` directory we're waiting to see created — the watcher fires a
//! single Tauri `scopes-changed` event so the front-end can refresh.
//!
//! Correctness concerns:
//!
//! 1. **We watch directories, not files.** A file may not exist yet when we
//!    start watching. Watching the parent dir (non-recursively) lets us pick
//!    up `Create` events when the file appears. If the *parent* doesn't
//!    exist either (e.g. no `.claude/` in this project yet), we walk one hop
//!    further up and treat a later creation of the missing `.claude/`
//!    directory as our cue to re-install.
//!
//! 2. **We use non-recursive watches.** Recursive watches on a full project
//!    tree — or worse, the home directory — can explode into thousands of
//!    per-directory watches on inotify-based platforms. Non-recursive +
//!    one-hop-at-a-time keeps the watch set tight (at most ~3 dirs).
//!
//! 3. **We must not echo our own writes.** `apply_move` calls
//!    `note_self_write(path)` right after each successful save. Suppression
//!    is *path-scoped* and *consumed on first match*: the watcher ignores
//!    the next interesting batch whose matched paths are all self-writes
//!    we recorded recently, and removes those map entries as it does so.
//!    This means a self-write to one scope file cannot suppress a
//!    legitimate external edit to a different scope file, and a stale
//!    entry can't suppress more than one batch.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult, Debouncer};
use tauri::{AppHandle, Emitter};

use crate::scope::ScopePaths;

const DEBOUNCE_MS: u64 = 200;
const SELF_WRITE_GRACE_MS: u64 = 500;
/// Minimum gap between `watcher-error` emissions. A broken watcher would
/// otherwise fire one per debounced batch and flood the devtools console.
const ERROR_EMIT_COOLDOWN_MS: u64 = 5_000;

/// Cheap pre-filter applied to every incoming event before we do anything
/// more expensive: unless the filename looks interesting, there's no way
/// the event can match one of our scope paths or the `.claude/` dir we're
/// waiting to see created, so we short-circuit before any canonicalize()
/// syscall. Especially important in the "missing .claude/" fallback
/// where the watch root is the project dir and unrelated source-file
/// events can be frequent.
const FILENAME_ALLOWLIST: &[&str] = &["settings.json", "settings.local.json", ".claude"];

/// Tauri-managed state. `WatchState::default()` is a no-op watcher; call
/// `install` to wire up actual paths.
pub struct WatchState {
    /// Holds the active debouncer; dropping it stops the watch threads.
    debouncer: Mutex<Option<Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>>>,
    /// Per-path record of recent self-writes. Shared with the watcher
    /// callback so it can suppress echoes of our own writes — but only for
    /// the specific paths we wrote to, not globally. Each entry is consumed
    /// the first time a matching event batch arrives, so a stale entry
    /// (e.g. the OS coalesced our event with someone else's) can suppress
    /// at most one batch.
    self_writes: Arc<Mutex<HashMap<PathBuf, Instant>>>,
}

impl Default for WatchState {
    fn default() -> Self {
        Self {
            debouncer: Mutex::new(None),
            self_writes: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl WatchState {
    /// Record that we just wrote to `path` ourselves. Call this *after* each
    /// successful `io_atomic::save` from a write command (e.g. `apply_move`).
    /// Calling it after (not before) means a save that fails doesn't silence
    /// a real subsequent external edit — and the 200ms debouncer gives us
    /// plenty of slack between the actual file write and the notify callback.
    ///
    /// Both raw and canonical spellings are stored when they differ, so the
    /// callback can match regardless of which form notify reports.
    ///
    /// A poisoned mutex is recovered with `into_inner`: self-write
    /// suppression is important enough that silently skipping the update
    /// would defeat the whole point — poisoning just means a previous lock
    /// holder panicked, and overwriting the timestamp is safe either way.
    pub fn note_self_write(&self, path: &Path) {
        let mut map = self.self_writes.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        prune_expired(&mut map, now);
        map.insert(path.to_path_buf(), now);
        if let Some(canon) = canonicalize_with_fallback(path) {
            if canon != *path {
                map.insert(canon, now);
            }
        }
    }

    /// Tear down any existing watcher and install a fresh one. Idempotent:
    /// calling it twice with the same paths is fine. The new debouncer is
    /// built and wired up *before* the old one is dropped — if construction
    /// or any `watch()` call fails, the previous watcher keeps working.
    pub fn install(&self, app: AppHandle, paths: &ScopePaths) -> Result<(), String> {
        let plan = compute_watch_plan(paths);

        if plan.roots.is_empty() {
            // Nothing sensible to watch (no home dir, no project dir). Drop
            // the old watcher — there's explicitly nothing to replace it
            // with. Recover from a poisoned mutex so one bad panic in the
            // callback thread doesn't permanently disable install/uninstall.
            let mut guard = self.debouncer.lock().unwrap_or_else(|e| e.into_inner());
            guard.take();
            return Ok(());
        }

        // Store both raw and canonicalized forms of each match path. On
        // platforms with symlinked parents (macOS `/var` -> `/private/var`)
        // notify can report either spelling, so a straight PathBuf equality
        // check against a set of just-raw paths would miss legit events.
        // canonicalize_with_fallback() canonicalizes the nearest existing
        // ancestor and re-appends the remaining components, so paths whose
        // leaves don't exist yet (e.g. the `.claude/` we're waiting on)
        // still get a canonical form precomputed.
        let match_paths_for_cb: HashSet<PathBuf> = plan
            .match_paths
            .iter()
            .flat_map(|p| {
                let mut forms = vec![p.clone()];
                if let Some(canon) = canonicalize_with_fallback(p) {
                    if canon != *p {
                        forms.push(canon);
                    }
                }
                forms
            })
            .collect();

        let self_writes = Arc::clone(&self.self_writes);
        let app_for_cb = app.clone();
        // Cooldown guard against flooding the frontend with `watcher-error`
        // notifications if notify enters a persistent error state. The
        // debouncer itself caps event batches; this caps batched errors.
        let last_error_emit: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let mut debouncer = new_debouncer(
            Duration::from_millis(DEBOUNCE_MS),
            move |res: DebounceEventResult| {
                let events = match res {
                    Ok(evs) => evs,
                    Err(err) => {
                        emit_watcher_error_rate_limited(&app_for_cb, &last_error_emit, &err);
                        return;
                    }
                };
                // Collect the deduped set of interesting paths in this
                // batch. match_paths_for_cb is precomputed from the
                // resolved scope paths (plus any expected-but-missing
                // `.claude/` dirs) in both raw and canonical forms, so
                // unrelated `settings.json` files elsewhere in the tree
                // can't fire a spurious reload.
                let mut matched: HashSet<PathBuf> = HashSet::new();
                for ev in &events {
                    // Cheap filename pre-filter — rejects the overwhelming
                    // majority of events (editor swaps, build outputs,
                    // etc.) without touching the filesystem.
                    let name_ok = ev
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| FILENAME_ALLOWLIST.contains(&n));
                    if !name_ok {
                        continue;
                    }
                    if match_paths_for_cb.contains(&ev.path) {
                        matched.insert(ev.path.clone());
                        continue;
                    }
                    // Last-resort: canonicalize the incoming event path
                    // and check again. Handles the inverse of the
                    // install-time canonicalization (raw-in-match-set,
                    // canonical event) and any other late-resolving
                    // symlink cases. The filename pre-filter above
                    // guarantees we only pay the syscall for plausibly-
                    // relevant paths.
                    if let Ok(canon) = std::fs::canonicalize(&ev.path) {
                        if match_paths_for_cb.contains(&canon) {
                            matched.insert(canon);
                        }
                    }
                }
                if matched.is_empty() {
                    return;
                }
                if !classify_external_change(&matched, &self_writes, Instant::now()) {
                    return;
                }
                let _ = app_for_cb.emit("scopes-changed", ());
            },
        )
        .map_err(|e| format!("failed to start file watcher: {e}"))?;

        for dir in &plan.roots {
            debouncer
                .watcher()
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(|e| format!("failed to watch {}: {}", dir.display(), e))?;
        }

        // Swap-then-drop: the new debouncer is fully constructed and watching
        // at this point, so replacing the slot is atomic. The old value is
        // dropped when the guard goes out of scope, stopping its threads.
        let mut guard = self.debouncer.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(debouncer);
        Ok(())
    }
}

/// The two derived inputs to the watcher: which directories to watch and
/// which exact paths the event filter should treat as interesting.
#[derive(Debug, Default, PartialEq, Eq)]
struct WatchPlan {
    roots: HashSet<PathBuf>,
    match_paths: HashSet<PathBuf>,
}

/// Decide what to watch and what to match on, based only on filesystem
/// existence checks. Extracted from `install` so the logic is unit-testable
/// without a real Tauri app + debouncer.
fn compute_watch_plan(paths: &ScopePaths) -> WatchPlan {
    let scope_paths: Vec<PathBuf> = [
        paths.local.as_ref(),
        paths.project.as_ref(),
        paths.user_local.as_ref(),
        paths.user.as_ref(),
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect();

    let mut plan = WatchPlan::default();
    for scope_path in &scope_paths {
        plan.match_paths.insert(scope_path.clone());
        let Some(parent) = scope_path.parent() else {
            continue;
        };
        // `is_dir()` (not `exists()`) — a stray regular file named `.claude`
        // would satisfy `exists()` but can't be watched as a directory, so
        // we fall through to the grandparent watch instead and wait for it
        // to be replaced with an actual directory.
        if parent.is_dir() {
            // Common case: `.claude/` is already there, watch it
            // non-recursively and wait for settings*.json events.
            plan.roots.insert(parent.to_path_buf());
            continue;
        }
        // Fallback: `.claude/` doesn't exist yet (or isn't a directory).
        // Watch its parent non-recursively and match on the creation of
        // `.claude/`. When that fires, the scopes-changed event triggers
        // a reload on the front-end, which re-invokes install() with the
        // now-existing `.claude/` as the watch root.
        if let Some(grandparent) = parent.parent() {
            if grandparent.is_dir() {
                plan.roots.insert(grandparent.to_path_buf());
                plan.match_paths.insert(parent.to_path_buf());
            }
        }
    }
    plan
}

fn emit_watcher_error_rate_limited(
    app: &AppHandle,
    cooldown: &Arc<Mutex<Option<Instant>>>,
    error: &notify_debouncer_mini::notify::Error,
) {
    let mut guard = cooldown.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    if let Some(last) = *guard {
        if now.duration_since(last) < Duration::from_millis(ERROR_EMIT_COOLDOWN_MS) {
            return;
        }
    }
    *guard = Some(now);
    let _ = app.emit("watcher-error", error.to_string());
}

/// Canonicalize `p` when possible, preserving any non-existent trailing
/// components. Walks up to find the nearest existing ancestor, canonicalizes
/// that, then re-appends the bits we stripped. Lets us precompute canonical
/// forms for paths whose leaves (e.g. a not-yet-created `.claude/` or
/// `settings.json`) don't exist yet, which matters on platforms where
/// notify reports canonical spellings for later creation events.
fn canonicalize_with_fallback(p: &std::path::Path) -> Option<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = p.to_path_buf();
    loop {
        if cursor.exists() {
            break;
        }
        let name = cursor.file_name().map(ToOwned::to_owned)?;
        tail.push(name);
        if !cursor.pop() {
            return None;
        }
    }
    let mut canon = std::fs::canonicalize(&cursor).ok()?;
    for name in tail.iter().rev() {
        canon.push(name);
    }
    Some(canon)
}

/// Decide whether a debounced batch should fire `scopes-changed`.
///
/// Each path in `matched` is checked against the per-path self-write log:
/// - If the entry is fresh (within `SELF_WRITE_GRACE_MS`), we treat that
///   path as a self-echo and *consume* the entry, so the next batch for
///   the same path won't be suppressed.
/// - If the entry is missing or expired, the path counts as an external
///   change and the function returns `true`.
///
/// Returns `true` iff at least one matched path is not a current self-write.
///
/// Poisoned mutex is recovered with `into_inner` — see `note_self_write`
/// for the rationale; the same logic applies here.
fn classify_external_change(
    matched: &HashSet<PathBuf>,
    self_writes: &Arc<Mutex<HashMap<PathBuf, Instant>>>,
    now: Instant,
) -> bool {
    let mut map = self_writes.lock().unwrap_or_else(|e| e.into_inner());
    prune_expired(&mut map, now);
    let mut external = false;
    for path in matched {
        match map.get(path) {
            Some(&ts) if now.duration_since(ts) < Duration::from_millis(SELF_WRITE_GRACE_MS) => {
                map.remove(path);
            }
            _ => external = true,
        }
    }
    external
}

/// Drop self-write entries older than the grace window. Called on every
/// insert and lookup so the map can't grow unbounded if note_self_write is
/// invoked in a tight loop (e.g. many rule moves) or if events never
/// arrive for some recorded write.
fn prune_expired(map: &mut HashMap<PathBuf, Instant>, now: Instant) {
    let grace = Duration::from_millis(SELF_WRITE_GRACE_MS);
    map.retain(|_, ts| now.duration_since(*ts) < grace);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_paths_from(
        local: Option<PathBuf>,
        project: Option<PathBuf>,
        user: Option<PathBuf>,
    ) -> ScopePaths {
        ScopePaths {
            project_dir: PathBuf::from("/tmp/unused"),
            local,
            project,
            user_local: None,
            user,
        }
    }

    #[test]
    fn plan_watches_existing_parent_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = claude_dir.join("settings.json");

        let plan = compute_watch_plan(&scope_paths_from(None, Some(settings.clone()), None));
        assert!(
            plan.roots.contains(&claude_dir),
            "should watch .claude/ when it exists"
        );
        assert!(
            plan.match_paths.contains(&settings),
            "scope path is always in match set"
        );
        // Parent of .claude/ is NOT a root in this case — we don't need it.
        assert!(!plan.roots.contains(tmp.path()));
    }

    #[test]
    fn plan_falls_back_to_grandparent_when_claude_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        // Intentionally do NOT create .claude/.
        let settings = claude_dir.join("settings.json");

        let plan = compute_watch_plan(&scope_paths_from(None, Some(settings.clone()), None));
        assert!(
            plan.roots.contains(tmp.path()),
            "should watch project dir when .claude/ doesn't exist yet"
        );
        assert!(
            !plan.roots.contains(&claude_dir),
            "shouldn't try to watch the non-existent .claude/"
        );
        // Filter needs to accept both the eventual settings.json AND the
        // creation of the .claude/ dir, so reinstall can be re-triggered.
        assert!(plan.match_paths.contains(&settings));
        assert!(plan.match_paths.contains(&claude_dir));
    }

    #[test]
    fn plan_falls_back_when_claude_is_a_file_not_a_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a stray regular file named `.claude` — .exists() says
        // true, but we can't watch it as a directory. The plan should
        // treat this like the "missing" case and fall back to the
        // grandparent, waiting for `.claude` to become an actual dir.
        let claude_path = tmp.path().join(".claude");
        std::fs::write(&claude_path, "not a directory").unwrap();
        let settings = claude_path.join("settings.json");

        let plan = compute_watch_plan(&scope_paths_from(None, Some(settings), None));
        assert!(
            plan.roots.contains(tmp.path()),
            "should fall back to grandparent when .claude is a file",
        );
        assert!(
            !plan.roots.contains(&claude_path),
            "must not try to watch a non-directory as a directory",
        );
    }

    #[test]
    fn plan_includes_user_local_alongside_user() {
        // Regression for #23: `~/.claude/settings.local.json` is a real
        // scope, so the watcher plan has to see its path as both a watch
        // root (when the dir exists) and a match path, mirroring how the
        // project/user scopes are handled.
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let user = claude_dir.join("settings.json");
        let user_local = claude_dir.join("settings.local.json");

        let paths = ScopePaths {
            project_dir: PathBuf::from("/tmp/unused"),
            local: None,
            project: None,
            user_local: Some(user_local.clone()),
            user: Some(user.clone()),
        };
        let plan = compute_watch_plan(&paths);
        assert!(
            plan.roots.contains(&claude_dir),
            "user_local shares .claude/ with user; one watch root covers both"
        );
        assert!(
            plan.match_paths.contains(&user_local),
            "user_local scope path must be in the match set"
        );
        assert!(
            plan.match_paths.contains(&user),
            "user scope path must stay in the match set",
        );
    }

    #[test]
    fn plan_empty_when_no_ancestor_exists() {
        // A fabricated path that has no existing ancestor on the system.
        let ghost = PathBuf::from("/nonexistent-claudescope-root/x/.claude/settings.json");
        let plan = compute_watch_plan(&scope_paths_from(None, Some(ghost.clone()), None));
        assert!(
            plan.roots.is_empty(),
            "no existing ancestor => no watch roots"
        );
        assert!(
            plan.match_paths.contains(&ghost),
            "match set still contains the scope path for future re-installs",
        );
    }

    #[test]
    fn plan_dedupes_shared_claude_dir() {
        // Local and project paths share the same `.claude/` dir; only one
        // watch root should result.
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = claude_dir.join("settings.json");
        let local = claude_dir.join("settings.local.json");

        let plan = compute_watch_plan(&scope_paths_from(
            Some(local.clone()),
            Some(settings.clone()),
            None,
        ));
        assert_eq!(plan.roots.len(), 1);
        assert!(plan.match_paths.contains(&settings));
        assert!(plan.match_paths.contains(&local));
    }

    #[test]
    fn canonicalize_with_fallback_resolves_ancestor_for_missing_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().canonicalize().unwrap();
        // Ancestor exists, but `.claude/settings.json` does not.
        let missing = tmp.path().join(".claude").join("settings.json");

        let canonical = canonicalize_with_fallback(&missing).expect("has existing ancestor");
        let expected = real.join(".claude").join("settings.json");
        assert_eq!(canonical, expected);
    }

    fn make_self_writes() -> Arc<Mutex<HashMap<PathBuf, Instant>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn classify_treats_unrecorded_path_as_external() {
        // No self-writes recorded — any matched path is external.
        let writes = make_self_writes();
        let mut matched = HashSet::new();
        matched.insert(PathBuf::from("/x/.claude/settings.json"));
        assert!(classify_external_change(&matched, &writes, Instant::now()));
    }

    #[test]
    fn classify_suppresses_a_self_write_to_the_same_path() {
        let writes = make_self_writes();
        let path = PathBuf::from("/x/.claude/settings.json");
        let now = Instant::now();
        writes.lock().unwrap().insert(path.clone(), now);

        let mut matched = HashSet::new();
        matched.insert(path.clone());
        assert!(!classify_external_change(&matched, &writes, now));
        // Entry consumed — a second matching batch isn't suppressed.
        assert!(!writes.lock().unwrap().contains_key(&path));
    }

    #[test]
    fn classify_does_not_suppress_external_change_to_different_path() {
        // Regression for #31: a self-write to one scope file must not
        // silence a legitimate external edit to a different scope file
        // arriving in the same batch (or any later batch).
        let writes = make_self_writes();
        let local = PathBuf::from("/x/.claude/settings.local.json");
        let project = PathBuf::from("/x/.claude/settings.json");
        let now = Instant::now();
        writes.lock().unwrap().insert(local.clone(), now);

        let mut matched = HashSet::new();
        matched.insert(project.clone());
        assert!(
            classify_external_change(&matched, &writes, now),
            "external edit to project must fire even with a fresh self-write to local"
        );
        // The unrelated self-write entry is left intact for its own future event.
        assert!(writes.lock().unwrap().contains_key(&local));
    }

    #[test]
    fn classify_emits_when_batch_mixes_self_write_and_external_change() {
        let writes = make_self_writes();
        let local = PathBuf::from("/x/.claude/settings.local.json");
        let project = PathBuf::from("/x/.claude/settings.json");
        let now = Instant::now();
        writes.lock().unwrap().insert(local.clone(), now);

        let mut matched = HashSet::new();
        matched.insert(local.clone());
        matched.insert(project.clone());
        assert!(classify_external_change(&matched, &writes, now));
        // The matching self-write entry is consumed; the unrelated path
        // wasn't recorded, so nothing else changes.
        assert!(!writes.lock().unwrap().contains_key(&local));
    }

    #[test]
    fn classify_does_not_suppress_after_grace_window_elapses() {
        let writes = make_self_writes();
        let path = PathBuf::from("/x/.claude/settings.json");
        let now = Instant::now();
        writes.lock().unwrap().insert(
            path.clone(),
            now - Duration::from_millis(SELF_WRITE_GRACE_MS + 50),
        );

        let mut matched = HashSet::new();
        matched.insert(path.clone());
        assert!(classify_external_change(&matched, &writes, now));
    }

    #[test]
    fn classify_consumes_self_write_so_followup_batch_is_not_suppressed() {
        // Self-write -> first batch suppressed, entry consumed.
        // External edit to the same path immediately after -> NOT suppressed,
        // even though it lands inside the legacy global grace window.
        let writes = make_self_writes();
        let path = PathBuf::from("/x/.claude/settings.json");
        let now = Instant::now();
        writes.lock().unwrap().insert(path.clone(), now);

        let mut matched = HashSet::new();
        matched.insert(path.clone());
        assert!(!classify_external_change(&matched, &writes, now));

        // Second batch arrives shortly after, well within the old global window.
        let later = now + Duration::from_millis(50);
        assert!(classify_external_change(&matched, &writes, later));
    }

    #[test]
    fn note_self_write_records_path_and_canonical_form() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = claude_dir.join("settings.json");
        std::fs::write(&settings, "{}").unwrap();

        let state = WatchState::default();
        state.note_self_write(&settings);

        let map = state.self_writes.lock().unwrap();
        assert!(
            map.contains_key(&settings),
            "raw spelling must be present so callbacks matching the raw form hit"
        );
        // On Windows + macOS the tempdir path often has a non-canonical
        // ancestor (e.g. `C:\Users\…\AppData\Local\Temp` vs an 8.3 form
        // or `/var` vs `/private/var`). Whenever it does, the canonical
        // form must also be recorded.
        if let Ok(canon) = std::fs::canonicalize(&settings) {
            if canon != settings {
                assert!(
                    map.contains_key(&canon),
                    "canonical spelling must also be present"
                );
            }
        }
    }

    #[test]
    fn note_self_write_recovers_from_poisoned_mutex() {
        // Poison the inner mutex by panicking inside a lock guard on another
        // thread, then confirm note_self_write still writes a fresh value.
        let state = WatchState::default();
        let arc = Arc::clone(&state.self_writes);
        let _ = std::thread::spawn(move || {
            let _guard = arc.lock().unwrap();
            panic!("intentional poisoning for test");
        })
        .join();
        assert!(state.self_writes.is_poisoned());

        let path = PathBuf::from("/x/.claude/settings.json");
        let before = Instant::now();
        state.note_self_write(&path);
        let after = Instant::now();

        let map = state.self_writes.lock().unwrap_or_else(|e| e.into_inner());
        let ts = map.get(&path).expect("path was recorded");
        assert!(*ts >= before);
        assert!(*ts <= after);
    }

    #[test]
    fn prune_expired_drops_old_entries_only() {
        let mut map = HashMap::new();
        let now = Instant::now();
        let fresh = PathBuf::from("/x/.claude/settings.json");
        let stale = PathBuf::from("/x/.claude/settings.local.json");
        map.insert(fresh.clone(), now);
        map.insert(
            stale.clone(),
            now - Duration::from_millis(SELF_WRITE_GRACE_MS + 100),
        );

        prune_expired(&mut map, now);

        assert!(map.contains_key(&fresh));
        assert!(!map.contains_key(&stale));
    }
}
