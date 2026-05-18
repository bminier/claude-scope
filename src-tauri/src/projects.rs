//! Discovery of Claude Code projects on this machine.
//!
//! Claude Code maintains a per-project transcript registry at
//! `~/.claude/projects/<encoded-cwd>/<session>.jsonl`. The encoded directory
//! name collapses drive letters and path separators to `-`, so it's lossy
//! and can't be reversed deterministically. The transcripts themselves
//! record the original absolute path in a `cwd` JSON field, so we use the
//! filesystem as a registry but ignore its keys: we open the most recent
//! `.jsonl` in each subdirectory and extract `cwd` directly. That makes the
//! list authoritative on Windows (where decoding `D--bminier-claude-scope`
//! is ambiguous) without inventing a new on-disk registry.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Bounded scan over each transcript looking for the first `cwd`. The first
/// few records are session-metadata wrappers (`type: "last-prompt"`,
/// `"custom-title"`, `"agent-name"`, `"permission-mode"`, `"bridge-session"`)
/// that don't carry `cwd`; the field appears once the first real
/// prompt/response record lands. Cap the scan so a corrupt or truncated
/// transcript can't make discovery O(file size).
const CWD_SCAN_LINE_CAP: usize = 50;

#[derive(Debug, Clone, Serialize)]
pub struct KnownProject {
    /// Basename of `root`. Stable display label for the Move-to submenu.
    pub name: String,
    /// Absolute path to the project root, as recorded in the transcript's
    /// `cwd` field. Always an existing directory with a `.claude/`
    /// subdirectory at the time of discovery.
    pub root: PathBuf,
}

/// Minimal shape we need from each transcript line. `serde` ignores the
/// dozens of other fields each record carries, so a single struct works for
/// both session-metadata and prompt/response records — only the ones with a
/// `cwd` value contribute.
#[derive(Debug, Deserialize)]
struct TranscriptLine {
    cwd: Option<String>,
}

/// Enumerate every Claude project visible to this machine. `home` overrides
/// `dirs::home_dir()` for sandboxed tests (and the runtime `--home`
/// override). Missing `~/.claude/projects/` is not an error — fresh
/// installs simply return an empty list.
pub fn list_known_projects(home: Option<&Path>) -> io::Result<Vec<KnownProject>> {
    let Some(projects_dir) = projects_dir(home) else {
        return Ok(Vec::new());
    };

    let entries = match fs::read_dir(&projects_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut found: Vec<KnownProject> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(cwd) = latest_cwd_in(&entry.path())? else {
            continue;
        };
        if !is_loadable_project(&cwd) {
            continue;
        }
        let name = display_name_for(&cwd);
        found.push(KnownProject { name, root: cwd });
    }

    dedupe_and_sort(&mut found);
    Ok(found)
}

fn projects_dir(home: Option<&Path>) -> Option<PathBuf> {
    let base = home.map(Path::to_path_buf).or_else(dirs::home_dir)?;
    Some(base.join(".claude").join("projects"))
}

/// Pick the newest transcript file in `dir` and extract its first `cwd`.
/// Newest-wins because a project the user has come back to most recently
/// has the truest current path: an old transcript could reference a path
/// from before a directory rename.
fn latest_cwd_in(dir: &Path) -> io::Result<Option<PathBuf>> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir)?.flatten() {
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        // mtime fallback to UNIX_EPOCH keeps ordering stable on filesystems
        // that fail to report a modified time (rare, but seen on some
        // Windows network shares); the comparison still yields *some* total
        // order so the loop terminates with a definite winner.
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match &newest {
            Some((best, _)) if *best >= modified => {}
            _ => newest = Some((modified, path)),
        }
    }

    let Some((_, path)) = newest else {
        return Ok(None);
    };
    Ok(extract_first_cwd(&path)?.map(PathBuf::from))
}

fn extract_first_cwd(path: &Path) -> io::Result<Option<String>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(CWD_SCAN_LINE_CAP) {
        let line = match line {
            Ok(s) => s,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<TranscriptLine>(&line) else {
            continue;
        };
        if let Some(cwd) = parsed.cwd {
            if !cwd.is_empty() {
                return Ok(Some(cwd));
            }
        }
    }
    Ok(None)
}

/// A project is "loadable" by ClaudeScope when its `.claude/` directory
/// still exists. Without it there's nothing for the Move-to submenu to
/// target, so silently hide the entry rather than offer a destination that
/// would immediately fail in `scope::resolve`.
fn is_loadable_project(root: &Path) -> bool {
    root.join(".claude").is_dir()
}

fn display_name_for(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| root.display().to_string())
}

/// De-dupe by canonicalized path, then sort alphabetically (case-insensitive
/// on the display name) for a stable Move-to ordering. Canonicalization
/// catches the same project showing up under two encoded directory names
/// (e.g. after a path-case change on Windows that left a stale transcript
/// alongside the new one).
fn dedupe_and_sort(projects: &mut Vec<KnownProject>) {
    projects.sort_by_key(|p| canonical_key(&p.root));
    projects.dedup_by(|a, b| canonical_key(&a.root) == canonical_key(&b.root));
    projects.sort_by_key(|p| p.name.to_lowercase());
}

fn canonical_key(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;

    fn write_jsonl(path: &Path, lines: &[&str]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, lines.join("\n")).unwrap();
    }

    fn seed_project(home: &Path, encoded: &str, cwd: &Path) {
        let dir = home.join(".claude").join("projects").join(encoded);
        // Emit a few session-metadata records before the cwd-bearing record
        // so the test exercises the bounded scan past the prelude. Mirrors
        // the shape Claude Code writes in real transcripts.
        let cwd_str = cwd.to_string_lossy().replace('\\', "\\\\");
        let lines = [
            r#"{"type":"last-prompt","leafUuid":"u","sessionId":"s"}"#.to_string(),
            r#"{"type":"custom-title","customTitle":"t","sessionId":"s"}"#.to_string(),
            format!(r#"{{"sessionId":"s","cwd":"{cwd_str}"}}"#),
        ];
        write_jsonl(
            &dir.join("0001-session.jsonl"),
            &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        );
    }

    /// Build a project root with a `.claude/` so it survives the loadability
    /// filter. Returns the path so the test can compare against it.
    fn make_loadable_project(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        fs::create_dir_all(root.join(".claude")).unwrap();
        root
    }

    #[test]
    fn returns_empty_when_projects_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // No `~/.claude/projects/` at all.
        let got = list_known_projects(Some(tmp.path())).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn extracts_cwd_from_transcript_prelude() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = make_loadable_project(tmp.path(), "alpha");
        seed_project(&home, "D--alpha", &project);

        let got = list_known_projects(Some(&home)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "alpha");
        assert_eq!(canonical_key(&got[0].root), canonical_key(&project));
    }

    #[test]
    fn hides_projects_whose_directory_no_longer_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        // Transcript references a path that does NOT exist on disk.
        let bogus = tmp.path().join("deleted-project");
        seed_project(&home, "D--gone", &bogus);

        let got = list_known_projects(Some(&home)).unwrap();
        assert!(got.is_empty(), "expected stale project to be filtered out");
    }

    #[test]
    fn hides_projects_with_no_dot_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        // Real directory, but no `.claude/` — not a Claude project as far as
        // ClaudeScope is concerned.
        let project = tmp.path().join("not-a-claude-project");
        fs::create_dir_all(&project).unwrap();
        seed_project(&home, "D--bare", &project);

        let got = list_known_projects(Some(&home)).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn skips_transcripts_with_no_cwd_record() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let dir = home.join(".claude").join("projects").join("D--no-cwd");
        // Session-metadata only — no record carries a `cwd` field.
        write_jsonl(
            &dir.join("0001.jsonl"),
            &[
                r#"{"type":"last-prompt","leafUuid":"u","sessionId":"s"}"#,
                r#"{"type":"custom-title","customTitle":"t","sessionId":"s"}"#,
            ],
        );

        let got = list_known_projects(Some(&home)).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn sorts_alphabetically_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let alpha = make_loadable_project(tmp.path(), "alpha");
        let beta = make_loadable_project(tmp.path(), "Beta");
        let gamma = make_loadable_project(tmp.path(), "gamma");
        seed_project(&home, "p-gamma", &gamma);
        seed_project(&home, "p-alpha", &alpha);
        seed_project(&home, "p-Beta", &beta);

        let got = list_known_projects(Some(&home)).unwrap();
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Beta", "gamma"]);
    }

    #[test]
    fn dedupes_two_encoded_dirs_pointing_at_same_root() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = make_loadable_project(tmp.path(), "shared");
        // Same `cwd` written under two different encoded dirs — simulates a
        // path-case change on Windows that leaves a stale transcript dir
        // alongside the new one.
        seed_project(&home, "D--SHARED", &project);
        seed_project(&home, "D--shared", &project);

        let got = list_known_projects(Some(&home)).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn ignores_non_jsonl_files_and_nested_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = make_loadable_project(tmp.path(), "mixed");
        let dir = home.join(".claude").join("projects").join("D--mixed");

        // Real transcript with cwd.
        let cwd_str = project.to_string_lossy().replace('\\', "\\\\");
        write_jsonl(
            &dir.join("session.jsonl"),
            &[&format!(r#"{{"sessionId":"s","cwd":"{cwd_str}"}}"#)],
        );
        // Extension-less file: should be skipped, not parsed.
        fs::write(dir.join("scratch"), "not jsonl").unwrap();
        // A nested directory shaped like a uuid — real Claude Code does
        // this. Must not be opened as a transcript.
        fs::create_dir_all(dir.join("1fad75bd-3add-4aba-85d6-9ac4b8369d34")).unwrap();

        let got = list_known_projects(Some(&home)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "mixed");
    }

    #[test]
    fn newest_transcript_wins_when_cwd_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let old_root = make_loadable_project(tmp.path(), "old-name");
        let new_root = make_loadable_project(tmp.path(), "new-name");
        let dir = home.join(".claude").join("projects").join("D--renamed");

        // Older transcript points at the old path...
        let old_str = old_root.to_string_lossy().replace('\\', "\\\\");
        write_jsonl(
            &dir.join("0001-old.jsonl"),
            &[&format!(r#"{{"cwd":"{old_str}"}}"#)],
        );
        // ...newer one at the new path. Force a distinct mtime so the test
        // is deterministic on filesystems with coarse-grained mtime
        // resolution (e.g. FAT32 has 2s granularity).
        let new_str = new_root.to_string_lossy().replace('\\', "\\\\");
        let newer = dir.join("0002-new.jsonl");
        write_jsonl(&newer, &[&format!(r#"{{"cwd":"{new_str}"}}"#)]);
        // Force a distinct mtime so the test is deterministic on
        // filesystems with coarse mtime resolution (FAT32 has 2s
        // granularity). `File::set_modified` is stable since 1.75 and
        // ClaudeScope's MSRV is 1.88.
        let later = SystemTime::now() + std::time::Duration::from_secs(60);
        let f = fs::OpenOptions::new().write(true).open(&newer).unwrap();
        f.set_modified(later).unwrap();

        let got = list_known_projects(Some(&home)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "new-name");
    }
}
