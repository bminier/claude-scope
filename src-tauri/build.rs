use std::path::Path;
use std::process::Command;

fn main() {
    capture_git_sha();
    tauri_build::build()
}

/// Capture the current commit's short SHA into `CLAUDE_SCOPE_GIT_SHA` for
/// `option_env!` to read at compile time. Surfaces in the About dialog and
/// `claude-scope-cli version --verbose` so bug reports can pin the exact
/// build. Silently does nothing when the source tree isn't a git repo
/// (release tarballs, vendored builds), `git` isn't on PATH, or
/// `git rev-parse` exits non-zero. In any of those cases `option_env!`
/// yields `None` and the About dialog just omits the SHA — no
/// diagnostic-time hard dependency on git.
fn capture_git_sha() {
    // `.git` can be a directory (normal repo) or a file (worktree pointing
    // at the main repo's `.git/worktrees/<name>/`). Either is acceptable;
    // bail early when neither exists so a release tarball build doesn't
    // shell out to git at all.
    let git_marker = Path::new("..").join(".git");
    if !git_marker.exists() {
        return;
    }

    let output = match Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };

    let sha = match String::from_utf8(output.stdout) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return,
    };
    if sha.is_empty() {
        return;
    }

    println!("cargo:rustc-env=CLAUDE_SCOPE_GIT_SHA={sha}");
    // Rerun when HEAD moves so a fresh commit doesn't ship with a stale
    // baked-in SHA. `.git/HEAD` covers branch checkouts; the active ref's
    // file covers commits on the current branch. Both paths are best-
    // effort: missing entries are fine, cargo just falls back to the
    // default rerun heuristics.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    if let Ok(head) = std::fs::read_to_string("../.git/HEAD") {
        if let Some(reference) = head
            .strip_prefix("ref: ")
            .and_then(|s| s.split_whitespace().next())
        {
            println!("cargo:rerun-if-changed=../.git/{reference}");
        }
    }
}
