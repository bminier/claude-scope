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
//!    `note_self_write` right after each successful save, and the watcher
//!    ignores any debounced batch that arrives within `SELF_WRITE_GRACE_MS`
//!    of the most recent self-write. Without this, every move would trigger
//!    a phantom external-change reload.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify_debouncer_mini::{
    new_debouncer, notify::RecursiveMode, DebounceEventResult, Debouncer,
};
use tauri::{AppHandle, Emitter};

use crate::scope::ScopePaths;

const DEBOUNCE_MS: u64 = 200;
const SELF_WRITE_GRACE_MS: u64 = 500;

/// Tauri-managed state. `WatchState::default()` is a no-op watcher; call
/// `install` to wire up actual paths.
pub struct WatchState {
    /// Holds the active debouncer; dropping it stops the watch threads.
    debouncer: Mutex<Option<Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>>>,
    /// Shared with the watcher callback so it can suppress echoes of our own
    /// writes. Stored separately from `debouncer` so the callback doesn't
    /// have to reach into the debouncer's mutex while it's being replaced.
    last_self_write: Arc<Mutex<Instant>>,
}

impl Default for WatchState {
    fn default() -> Self {
        Self {
            debouncer: Mutex::new(None),
            // Push it far enough back that the very first watcher event after
            // install is never mistaken for a self-write echo.
            last_self_write: Arc::new(Mutex::new(Instant::now() - Duration::from_secs(60))),
        }
    }
}

impl WatchState {
    /// Record that we just wrote to disk ourselves. Call this *after* each
    /// successful `io_atomic::save` from a write command (e.g. `apply_move`).
    /// Calling it after (not before) means a save that fails doesn't silence
    /// a real subsequent external edit — and the 200ms debouncer gives us
    /// plenty of slack between the actual file write and the notify callback.
    ///
    /// A poisoned mutex is recovered with `into_inner`: self-write
    /// suppression is important enough that silently skipping the update
    /// would defeat the whole point — poisoning just means a previous lock
    /// holder panicked, and overwriting the timestamp is safe either way.
    pub fn note_self_write(&self) {
        let mut t = self
            .last_self_write
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *t = Instant::now();
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
            let mut guard = self
                .debouncer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.take();
            return Ok(());
        }

        let last_self_write = Arc::clone(&self.last_self_write);
        let app_for_cb = app.clone();
        let match_paths_for_cb = plan.match_paths;
        let mut debouncer = new_debouncer(
            Duration::from_millis(DEBOUNCE_MS),
            move |res: DebounceEventResult| {
                let events = match res {
                    Ok(evs) => evs,
                    Err(_) => return, // Best-effort: swallow notify errors.
                };
                // Exact-path filter. match_paths is precomputed from the
                // resolved scope paths (plus any expected-but-missing
                // `.claude/` dirs), so unrelated `settings.json` files
                // elsewhere in the tree can't fire a spurious reload.
                let interesting = events
                    .iter()
                    .any(|ev| match_paths_for_cb.contains(&ev.path));
                if !interesting {
                    return;
                }
                if recent_self_write(&last_self_write) {
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
        let mut guard = self
            .debouncer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        paths.user.as_ref(),
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect();

    let mut plan = WatchPlan::default();
    for scope_path in &scope_paths {
        plan.match_paths.insert(scope_path.clone());
        let Some(parent) = scope_path.parent() else { continue };
        if parent.exists() {
            // Common case: `.claude/` is already there, watch it
            // non-recursively and wait for settings*.json events.
            plan.roots.insert(parent.to_path_buf());
            continue;
        }
        // Fallback: `.claude/` doesn't exist yet. Watch its parent
        // non-recursively and match on the creation of `.claude/`. When
        // that fires, the scopes-changed event triggers a reload on the
        // front-end, which re-invokes install() with the now-existing
        // `.claude/` as the watch root.
        if let Some(grandparent) = parent.parent() {
            if grandparent.exists() {
                plan.roots.insert(grandparent.to_path_buf());
                plan.match_paths.insert(parent.to_path_buf());
            }
        }
    }
    plan
}

fn recent_self_write(slot: &Arc<Mutex<Instant>>) -> bool {
    let Ok(guard) = slot.lock() else { return false };
    Instant::now().duration_since(*guard) < Duration::from_millis(SELF_WRITE_GRACE_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_paths_from(local: Option<PathBuf>, project: Option<PathBuf>, user: Option<PathBuf>) -> ScopePaths {
        ScopePaths {
            project_dir: PathBuf::from("/tmp/unused"),
            local,
            project,
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
        assert!(plan.roots.contains(&claude_dir), "should watch .claude/ when it exists");
        assert!(plan.match_paths.contains(&settings), "scope path is always in match set");
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
    fn plan_empty_when_no_ancestor_exists() {
        // A fabricated path that has no existing ancestor on the system.
        let ghost = PathBuf::from("/nonexistent-claudescope-root/x/.claude/settings.json");
        let plan = compute_watch_plan(&scope_paths_from(None, Some(ghost.clone()), None));
        assert!(plan.roots.is_empty(), "no existing ancestor => no watch roots");
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
    fn recent_self_write_true_within_grace_window() {
        let slot = Arc::new(Mutex::new(Instant::now()));
        assert!(recent_self_write(&slot));
    }

    #[test]
    fn recent_self_write_false_past_grace_window() {
        let slot = Arc::new(Mutex::new(
            Instant::now() - Duration::from_millis(SELF_WRITE_GRACE_MS + 50),
        ));
        assert!(!recent_self_write(&slot));
    }

    #[test]
    fn note_self_write_recovers_from_poisoned_mutex() {
        // Poison the inner mutex by panicking inside a lock guard on another
        // thread, then confirm note_self_write still writes a fresh value.
        let state = WatchState::default();
        let arc = Arc::clone(&state.last_self_write);
        let _ = std::thread::spawn(move || {
            let _guard = arc.lock().unwrap();
            panic!("intentional poisoning for test");
        })
        .join();
        assert!(state.last_self_write.is_poisoned());

        state.note_self_write();

        let ts = state
            .last_self_write
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(Instant::now().duration_since(*ts) < Duration::from_millis(200));
    }
}
