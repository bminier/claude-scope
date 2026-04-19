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
    pub fn note_self_write(&self) {
        if let Ok(mut t) = self.last_self_write.lock() {
            *t = Instant::now();
        }
    }

    /// Tear down any existing watcher and install a fresh one. Idempotent:
    /// calling it twice with the same paths is fine. The new debouncer is
    /// built and wired up *before* the old one is dropped — if construction
    /// or any `watch()` call fails, the previous watcher keeps working.
    pub fn install(&self, app: AppHandle, paths: &ScopePaths) -> Result<(), String> {
        let scope_paths: Vec<PathBuf> = [
            paths.local.as_ref(),
            paths.project.as_ref(),
            paths.user.as_ref(),
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect();

        // Decide what to watch per scope path + which exact paths the filter
        // should treat as "interesting" when they appear in events.
        let mut roots: HashSet<PathBuf> = HashSet::new();
        let mut match_paths: HashSet<PathBuf> = HashSet::new();
        for scope_path in &scope_paths {
            match_paths.insert(scope_path.clone());
            let Some(parent) = scope_path.parent() else { continue };
            if parent.exists() {
                // Common case: `.claude/` is already there, watch it
                // non-recursively and wait for settings*.json events.
                roots.insert(parent.to_path_buf());
                continue;
            }
            // Fallback: `.claude/` doesn't exist yet. Watch its parent
            // non-recursively and match on the creation of `.claude/`.
            // When that fires, the scopes-changed event triggers a reload
            // on the front-end, which re-invokes install() with the
            // now-existing `.claude/` as the watch root.
            if let Some(grandparent) = parent.parent() {
                if grandparent.exists() {
                    roots.insert(grandparent.to_path_buf());
                    match_paths.insert(parent.to_path_buf());
                }
            }
        }

        if roots.is_empty() {
            // Nothing sensible to watch (no home dir, no project dir). Drop
            // the old watcher — there's explicitly nothing to replace it
            // with.
            if let Ok(mut guard) = self.debouncer.lock() {
                guard.take();
            }
            return Ok(());
        }

        let last_self_write = Arc::clone(&self.last_self_write);
        let app_for_cb = app.clone();
        let match_paths_for_cb = match_paths;
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

        for dir in &roots {
            debouncer
                .watcher()
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(|e| format!("failed to watch {}: {}", dir.display(), e))?;
        }

        // Swap-then-drop: the new debouncer is fully constructed and watching
        // at this point, so replacing the slot is atomic. The old value is
        // dropped when the guard goes out of scope, stopping its threads.
        if let Ok(mut guard) = self.debouncer.lock() {
            *guard = Some(debouncer);
        }
        Ok(())
    }
}

fn recent_self_write(slot: &Arc<Mutex<Instant>>) -> bool {
    let Ok(guard) = slot.lock() else { return false };
    Instant::now().duration_since(*guard) < Duration::from_millis(SELF_WRITE_GRACE_MS)
}
