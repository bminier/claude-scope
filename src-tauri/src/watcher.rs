//! Filesystem watcher for the three recognized scope files.
//!
//! On every successful `load_scopes` call we (re)install a debounced watcher
//! over the parent directories of Local, Project, and User settings paths.
//! When any of those directories sees a change to a file named `settings.json`
//! or `settings.local.json`, the watcher fires a single Tauri `scopes-changed`
//! event so the front-end can refresh.
//!
//! Two correctness concerns the design has to solve:
//!
//! 1. We watch directories, not files, because the file may not exist when
//!    we start watching (e.g. a project with no `.claude/settings.json` yet).
//!    Watching the parent lets us pick up `Create` events when the file
//!    later appears.
//!
//! 2. We must not echo our own writes. `apply_move` calls `note_self_write`
//!    immediately before each save; the watcher ignores any debounced batch
//!    that arrives within `SELF_WRITE_GRACE_MS` of the most recent self-write.
//!    Without this, every move would trigger a phantom external-change reload.

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

/// File names we care about. Anything else in the watched directories is
/// ignored (e.g. `.bak` files we created ourselves, sibling project files).
const WATCHED_FILE_NAMES: &[&str] = &["settings.json", "settings.local.json"];

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
    /// Record that we just wrote to disk ourselves. Call this immediately
    /// before each `io_atomic::save` from a write command (e.g. `apply_move`).
    pub fn note_self_write(&self) {
        if let Ok(mut t) = self.last_self_write.lock() {
            *t = Instant::now();
        }
    }

    /// Tear down any existing watcher and install a fresh one over the
    /// three resolved scope files. Idempotent: calling it twice with the
    /// same paths is fine. The new debouncer is built and wired up *before*
    /// the old one is dropped — if construction or any `watch()` call
    /// fails, the previous watcher keeps working.
    pub fn install(&self, app: AppHandle, paths: &ScopePaths) -> Result<(), String> {
        // Collect (watch_root, scope_path) pairs. We watch the nearest
        // existing ancestor of each scope path (not the parent blindly) so a
        // workspace without `.claude/` yet still gets wired up — the user
        // creating `.claude/settings.json` later will fire through the
        // ancestor's recursive watch. Events are filtered by matching the
        // exact scope path in the callback so unrelated `settings.json`
        // files elsewhere in the tree can't trigger a reload.
        let scope_paths: Vec<PathBuf> = [
            paths.local.as_ref(),
            paths.project.as_ref(),
            paths.user.as_ref(),
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect();

        let mut roots: HashSet<PathBuf> = HashSet::new();
        for p in &scope_paths {
            if let Some(root) = nearest_existing_ancestor(p) {
                roots.insert(root);
            }
        }

        if roots.is_empty() {
            // Nothing to watch (no home dir, no project). Drop the old
            // watcher — there's explicitly nothing to replace it with.
            if let Ok(mut guard) = self.debouncer.lock() {
                guard.take();
            }
            return Ok(());
        }

        let last_self_write = Arc::clone(&self.last_self_write);
        let app_for_cb = app.clone();
        let watched_paths: Vec<PathBuf> = scope_paths.clone();
        let mut debouncer = new_debouncer(
            Duration::from_millis(DEBOUNCE_MS),
            move |res: DebounceEventResult| {
                let events = match res {
                    Ok(evs) => evs,
                    Err(_) => return, // Best-effort: swallow notify errors.
                };
                let interesting = events.iter().any(|ev| {
                    // Match on either the exact scope path (covers renames
                    // landing on that path) or the filename convention
                    // (covers create/remove that notify reports as the
                    // parent dir on some backends). Belt-and-suspenders.
                    watched_paths.iter().any(|w| w == &ev.path)
                        || ev
                            .path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| {
                                WATCHED_FILE_NAMES.iter().any(|w| *w == n)
                                    && ev
                                        .path
                                        .parent()
                                        .and_then(|p| p.file_name())
                                        .and_then(|n| n.to_str())
                                        == Some(".claude")
                            })
                            .unwrap_or(false)
                });
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
                .watch(dir, RecursiveMode::Recursive)
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

/// Walk up from `path`'s parent until we find a directory that exists on
/// disk. Returns `None` if we hit the filesystem root without finding one
/// (effectively impossible on sane systems).
fn nearest_existing_ancestor(path: &std::path::Path) -> Option<PathBuf> {
    let mut cursor = path.parent()?.to_path_buf();
    loop {
        if cursor.exists() {
            return Some(cursor);
        }
        if !cursor.pop() {
            return None;
        }
    }
}

fn recent_self_write(slot: &Arc<Mutex<Instant>>) -> bool {
    let Ok(guard) = slot.lock() else { return false };
    Instant::now().duration_since(*guard) < Duration::from_millis(SELF_WRITE_GRACE_MS)
}
