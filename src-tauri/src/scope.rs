//! Scope discovery.
//!
//! Claude Code recognizes these scopes (highest precedence first):
//!   1. Managed    — enterprise, out of scope for this tool
//!   2. Local      — <project>/.claude/settings.local.json
//!   3. Project    — <project>/.claude/settings.json
//!   4. UserLocal  — ~/.claude/settings.local.json
//!   5. User       — ~/.claude/settings.json
//!
//! `~/.claude/settings.local.json` was originally excluded, but Claude Code
//! is observed to create and use it when the user's home directory is itself
//! inside a git repo, so it's treated as a real scope on par with the others.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Local,
    Project,
    UserLocal,
    User,
}

impl Scope {
    pub const ALL: [Scope; 4] = [Scope::Local, Scope::Project, Scope::UserLocal, Scope::User];

    pub fn label(self) -> &'static str {
        match self {
            Scope::Local => "local",
            Scope::Project => "project",
            Scope::UserLocal => "user-local",
            Scope::User => "user",
        }
    }
}

/// Resolved on-disk paths for each recognized scope. A path is `None` only when
/// it cannot be determined (e.g. no home directory). The file at the path may
/// or may not exist.
#[derive(Debug, Clone)]
pub struct ScopePaths {
    pub project_dir: PathBuf,
    pub local: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub user_local: Option<PathBuf>,
    pub user: Option<PathBuf>,
}

impl ScopePaths {
    pub fn path_for(&self, scope: Scope) -> Option<&Path> {
        match scope {
            Scope::Local => self.local.as_deref(),
            Scope::Project => self.project.as_deref(),
            Scope::UserLocal => self.user_local.as_deref(),
            Scope::User => self.user.as_deref(),
        }
    }
}

/// Walks up from `start` (or the current working directory if `None`) to find
/// the project root. Prefers the nearest ancestor containing a `.git` entry
/// (directory or file, marking the VCS boundary), falling back to the nearest
/// ancestor containing `.claude/`, and finally to `start` itself. A `.git`
/// entry takes precedence so that running from a sub-crate like `src-tauri/`
/// still resolves to the repo root, even if that sub-crate happens to contain
/// its own stray `.claude/`.
///
/// The real `$HOME` is **never** treated as a project root, regardless of
/// whether it contains `.git` or `.claude/` — `~/.claude` is by definition
/// the User scope's location, and a versioned-dotfiles `.git` at home would
/// otherwise hijack the walk-up. `extra_home` is an additional path to skip
/// on top of the real home — used by sandbox mode (#66) so a scratch `~`
/// override is also exempt from project discovery, even when the start path
/// lives inside it.
pub fn find_project_root(
    start: Option<&Path>,
    extra_home: Option<&Path>,
) -> std::io::Result<PathBuf> {
    // Normalize to an absolute path so the walk-up terminates at the real
    // filesystem root. A relative `start` would otherwise `.pop()` down to
    // an empty relative path, and joining `.git` onto that would probe
    // relative to the process CWD — masking a missing ancestor as a match.
    let start = match start {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => std::env::current_dir()?.join(p),
        None => std::env::current_dir()?,
    };

    let real_home = dirs::home_dir();
    let is_home = |p: &Path| {
        real_home.as_deref().is_some_and(|h| p == h) || extra_home.is_some_and(|h| p == h)
    };

    // .git may be a directory (normal repo) or a file (worktree / submodule),
    // so check existence rather than is_dir(). try_exists() distinguishes
    // "not found" from real I/O errors (e.g., permission denied) — the former
    // continues the walk, the latter propagates instead of silently falling
    // through to .claude/start.
    let mut cursor = start.clone();
    loop {
        if !is_home(&cursor) && cursor.join(".git").try_exists()? {
            return Ok(cursor);
        }
        if !cursor.pop() {
            break;
        }
    }

    let mut cursor = start.clone();
    loop {
        if !is_home(&cursor) {
            // Mirror the `.git` pass: ENOENT keeps walking, genuine I/O errors
            // propagate. We need metadata() (not try_exists()) so we can also
            // reject a `.claude` that's a regular file.
            match cursor.join(".claude").metadata() {
                Ok(md) if md.is_dir() => return Ok(cursor),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if !cursor.pop() {
            // Reached filesystem root without finding .git or .claude; fall
            // back to the starting directory so the UI can still render empty
            // scopes.
            return Ok(start);
        }
    }
}

/// Resolve the file paths for every recognized scope given a starting
/// directory. The starting directory becomes the project dir after
/// [`find_project_root`] walks upward looking for a `.git` entry (preferred)
/// and then a `.claude/` directory.
///
/// When `home_override` is `Some`, that path replaces what `dirs::home_dir()`
/// would return, so the `User` and `UserLocal` scopes look at
/// `<override>/.claude/settings*.json` instead of the real user home. Used by
/// sandbox / scratch-home mode (#66) so the app can be exercised without
/// putting the user's actual Claude Code settings at risk.
pub fn resolve_with_home(
    start: Option<&Path>,
    home_override: Option<&Path>,
) -> std::io::Result<ScopePaths> {
    // Walk-up always skips real $HOME implicitly; only pass the sandbox
    // override here, and only when it's distinct from real $HOME.
    let project_dir = find_project_root(start, home_override)?;
    let home = home_override.map(Path::to_path_buf).or_else(dirs::home_dir);
    let local = Some(project_dir.join(".claude").join("settings.local.json"));
    let project = Some(project_dir.join(".claude").join("settings.json"));
    let user_local = home
        .as_ref()
        .map(|h| h.join(".claude").join("settings.local.json"));
    let user = home.map(|h| h.join(".claude").join("settings.json"));
    Ok(ScopePaths {
        project_dir,
        local,
        project,
        user_local,
        user,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // Serialize tests that mutate the process CWD so they don't race the
    // other tests (which read CWD indirectly via find_project_root's
    // normalization branch). Poisoning is benign — we only restore CWD.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    struct CwdGuard(PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[test]
    fn finds_project_root_when_dot_claude_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        // Canonicalize both sides so the test isn't flaky on macOS, where the
        // tempdir lives under /var which resolves to /private/var via a
        // filesystem-level symlink.
        let canon = |p: PathBuf| p.canonicalize().unwrap_or(p);
        let got = canon(find_project_root(Some(&nested), None).unwrap());
        let want = canon(tmp.path().to_path_buf());
        assert_eq!(got, want);
    }

    #[test]
    fn prefers_git_root_over_nested_dot_claude() {
        // Mirrors the real-world case where `tauri dev` starts in
        // `src-tauri/` and that directory has picked up its own stray
        // `.claude/`. The repo root (marked by `.git/`) should still win.
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("src-tauri");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::create_dir_all(sub.join(".claude")).unwrap();
        let canon = |p: PathBuf| p.canonicalize().unwrap_or(p);
        let got = canon(find_project_root(Some(&sub), None).unwrap());
        let want = canon(tmp.path().to_path_buf());
        assert_eq!(got, want);
    }

    #[test]
    fn prefers_git_file_root_over_nested_dot_claude() {
        // Worktrees and submodules represent `.git` as a file rather than a
        // directory. Keep this case covered so the `.git` existence check
        // stays regression-tested.
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("src-tauri");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: ../.git/modules/repo\n").unwrap();
        std::fs::create_dir_all(sub.join(".claude")).unwrap();
        let canon = |p: PathBuf| p.canonicalize().unwrap_or(p);
        let got = canon(find_project_root(Some(&sub), None).unwrap());
        let want = canon(tmp.path().to_path_buf());
        assert_eq!(got, want);
    }

    #[test]
    fn falls_back_to_start_when_no_dot_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let got = find_project_root(Some(&nested), None).unwrap();
        assert_eq!(got, nested);
    }

    #[test]
    fn walks_past_home_with_dot_claude() {
        // Regression: when the user's home contains `.claude/` (the User
        // scope), the walk-up must NOT lock onto home as the project root.
        // Build a fake home that sits as an ancestor of the start path,
        // plant a `.claude/` inside it, and verify find_project_root walks
        // past it. Mirrors Brian's real Windows setup where $HOME has
        // .claude/ and tempdirs live under $HOME.
        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path().join("home");
        let nested = fake_home.join("work").join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(fake_home.join(".claude")).unwrap();

        let got = find_project_root(Some(&nested), Some(&fake_home)).unwrap();
        // No .git or .claude anywhere except inside home (which is skipped),
        // so the walker falls back to the start path.
        assert_eq!(got, nested);
    }

    #[test]
    fn walks_past_home_with_dot_git() {
        // Same guarantee for `.git` at home: a versioned dotfiles repo at
        // $HOME must not hijack project discovery.
        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path().join("home");
        let nested = fake_home.join("work").join("a");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(fake_home.join(".git")).unwrap();

        let got = find_project_root(Some(&nested), Some(&fake_home)).unwrap();
        assert_eq!(got, nested);
    }

    #[test]
    fn normalizes_relative_start_against_cwd() {
        // Regression: without normalization, passing a relative `start` lets
        // `cursor.pop()` bottom out at an empty relative path, and the next
        // `.git` probe resolves against the process CWD — so a `.git` that
        // happens to sit in CWD would hijack the walk-up even though it
        // isn't an ancestor of `start`. Here CWD=tmp has a `.git`, but the
        // true ancestor chain of `tmp/work` runs up into tmp too, so the
        // correct answer is tmp either way — the discriminating detail is
        // that without the fix the returned path is an empty PathBuf (the
        // bug), whereas the fix returns an absolute tmp path.
        let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = CwdGuard(std::env::current_dir().unwrap());

        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let got = find_project_root(Some(Path::new("work")), None).unwrap();
        assert!(
            got.is_absolute(),
            "expected absolute path, got relative: {got:?}"
        );
        let canon = |p: PathBuf| p.canonicalize().unwrap_or(p);
        assert_eq!(canon(got), canon(tmp.path().to_path_buf()));
    }

    #[test]
    fn resolve_builds_local_and_project_paths() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        let paths = resolve_with_home(Some(tmp.path()), None).unwrap();
        assert_eq!(
            paths.local.as_deref().unwrap().file_name().unwrap(),
            "settings.local.json"
        );
        assert_eq!(
            paths.project.as_deref().unwrap().file_name().unwrap(),
            "settings.json"
        );
    }

    #[test]
    fn resolve_builds_user_local_path_alongside_user() {
        // Regression for #23: ~/.claude/settings.local.json needs to be a
        // first-class scope, not hidden under the shared home-dir path
        // computation.
        let tmp = tempfile::tempdir().unwrap();
        let paths = resolve_with_home(Some(tmp.path()), None).unwrap();
        if let (Some(ul), Some(u)) = (paths.user_local.as_deref(), paths.user.as_deref()) {
            assert_eq!(ul.file_name().unwrap(), "settings.local.json");
            assert_eq!(u.file_name().unwrap(), "settings.json");
            assert_eq!(ul.parent(), u.parent());
        } else {
            // dirs::home_dir() returned None (no $HOME in the test env);
            // then both must be None together.
            assert!(paths.user_local.is_none() && paths.user.is_none());
        }
    }

    #[test]
    fn resolve_with_home_redirects_user_scopes() {
        // Sandbox mode (#66): when home_override is set, User and UserLocal
        // must root at that path — not the real $HOME and not None — so a
        // dogfood session can't accidentally clobber the user's real
        // ~/.claude/.
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let fake_home = tmp.path().join("scratch-home");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::create_dir_all(fake_home.join(".claude")).unwrap();
        let paths = resolve_with_home(Some(&project), Some(&fake_home)).unwrap();
        assert_eq!(
            paths.user.as_deref().unwrap(),
            fake_home.join(".claude").join("settings.json"),
        );
        assert_eq!(
            paths.user_local.as_deref().unwrap(),
            fake_home.join(".claude").join("settings.local.json"),
        );
        // Project + Local must still root at the project_dir, not the home
        // override — promotion target columns should stay distinct.
        assert!(paths
            .project
            .as_deref()
            .unwrap()
            .starts_with(&paths.project_dir));
        assert!(paths
            .local
            .as_deref()
            .unwrap()
            .starts_with(&paths.project_dir));
    }

    #[test]
    fn scope_all_includes_user_local_between_project_and_user() {
        // Precedence: highest first. UserLocal sits above User so that the
        // combined-permissions union iterates in the right order.
        assert_eq!(
            Scope::ALL,
            [Scope::Local, Scope::Project, Scope::UserLocal, Scope::User]
        );
    }
}
