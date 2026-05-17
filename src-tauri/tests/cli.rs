//! Integration tests for the `claude-scope-cli` binary.
//!
//! Drives the compiled binary via `env!("CARGO_BIN_EXE_*")` against a
//! sandbox tempdir tree. Pinning the CLI surface this way means a future
//! "let's quietly rename a subcommand or flag" refactor fails CI loudly
//! instead of breaking shell scripts in the wild.
//!
//! Every test passes both `--project-dir` and `--home-dir` so the binary
//! never touches the developer's real `~/.claude/`. Working from the
//! tempdir's project directory also guarantees no stray walk-up to an
//! ancestor `.git` (the explicit `--project-dir` is honored literally,
//! covered separately in the bin's unit tests).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_claude-scope-cli");

struct Sandbox {
    _tmp: TempDir,
    project: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let project = tmp.path().join("project");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        Self {
            _tmp: tmp,
            project,
            home,
        }
    }

    fn write_project(&self, body: &str) {
        std::fs::write(self.project.join(".claude").join("settings.json"), body).unwrap();
    }

    fn write_user(&self, body: &str) {
        std::fs::write(self.home.join(".claude").join("settings.json"), body).unwrap();
    }

    fn project_settings(&self) -> String {
        std::fs::read_to_string(self.project.join(".claude").join("settings.json")).unwrap()
    }

    fn user_settings(&self) -> String {
        std::fs::read_to_string(self.home.join(".claude").join("settings.json")).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args([
                "--project-dir",
                self.project.to_str().unwrap(),
                "--home-dir",
                self.home.to_str().unwrap(),
            ])
            .args(args)
            .output()
            .expect("failed to execute claude-scope-cli")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("non-utf8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("non-utf8 stderr")
}

fn read_allow(path: &Path) -> Vec<String> {
    let body = std::fs::read_to_string(path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["permissions"]["allow"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn scopes_lists_all_four_recognized_scopes() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(ls)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":[]}}"#);

    let out = sb.run(&["scopes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let s = stdout(&out);
    for label in ["local", "project", "user-local", "user"] {
        assert!(s.contains(label), "scopes output missing `{label}`: {s}");
    }
    // Tempdir project is present (we created the file), tempdir user is
    // present (we created the file). Local + user-local stay absent.
    assert!(s.contains("project     [present]"));
    assert!(s.contains("user        [present]"));
    assert!(s.contains("local       [absent]"));
    assert!(s.contains("user-local  [absent]"));
}

#[test]
fn show_effective_merges_permissions_across_scopes() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(git status)","Read(**)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":["Bash(ls)"]}}"#);

    let out = sb.run(&["show", "--effective"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let s = stdout(&out);
    // Effective view should surface rules from both files.
    assert!(s.contains("Bash(git status)"));
    assert!(s.contains("Read(**)"));
    assert!(s.contains("Bash(ls)"));
}

#[test]
fn list_rules_filters_by_scope_and_kind() {
    let sb = Sandbox::new();
    sb.write_project(
        r#"{"permissions":{"allow":["Bash(git status)"],"deny":["WebFetch(domain:evil.example)"]}}"#,
    );
    sb.write_user(r#"{"permissions":{"allow":["Bash(ls)"]}}"#);

    let scoped = sb.run(&["list-rules", "--scope", "project", "--kind", "allow"]);
    assert!(scoped.status.success(), "stderr: {}", stderr(&scoped));
    let body = stdout(&scoped);
    assert!(body.contains("Bash(git status)"));
    // Other-scope and other-kind rows must be filtered out.
    assert!(!body.contains("Bash(ls)"));
    assert!(!body.contains("WebFetch(domain:evil.example)"));
}

#[test]
fn move_dry_run_previews_without_writing() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(git status)","Read(**)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":[]}}"#);

    let project_before = sb.project_settings();
    let user_before = sb.user_settings();

    let out = sb.run(&[
        "move",
        "Bash(git status)",
        "--kind",
        "allow",
        "--from",
        "project",
        "--to",
        "user",
        "--dry-run",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let body = stdout(&out);
    assert!(body.contains("dry-run preview:"));
    assert!(body.contains("will-write=true"));

    // The on-disk files must be byte-for-byte unchanged.
    assert_eq!(sb.project_settings(), project_before);
    assert_eq!(sb.user_settings(), user_before);
}

#[test]
fn move_atomically_relocates_rule_and_creates_backups() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(git status)","Read(**)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":["Bash(ls)"]}}"#);

    let out = sb.run(&[
        "move",
        "Bash(git status)",
        "--kind",
        "allow",
        "--from",
        "project",
        "--to",
        "user",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    let project_allow = read_allow(&sb.project.join(".claude").join("settings.json"));
    let user_allow = read_allow(&sb.home.join(".claude").join("settings.json"));
    assert_eq!(project_allow, vec!["Read(**)".to_string()]);
    assert!(user_allow.contains(&"Bash(git status)".to_string()));
    assert!(user_allow.contains(&"Bash(ls)".to_string()));

    // First write of the session must drop `.bak` snapshots of the
    // pre-write state alongside both files.
    assert!(sb
        .project
        .join(".claude")
        .join("settings.json.bak")
        .exists());
    assert!(sb.home.join(".claude").join("settings.json.bak").exists());
}

#[test]
fn move_rejects_rule_missing_from_source_with_nonzero_exit() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Read(**)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":[]}}"#);

    let out = sb.run(&[
        "move",
        "DoesNotExist",
        "--kind",
        "allow",
        "--from",
        "project",
        "--to",
        "user",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not found"));
    // The source file must not be touched on the rejected path.
    assert!(sb.project_settings().contains("Read(**)"));
}

#[test]
fn move_rejects_same_source_and_destination_scope() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(ls)"]}}"#);

    let out = sb.run(&[
        "move", "Bash(ls)", "--kind", "allow", "--from", "project", "--to", "project",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("must differ"));
}

#[test]
fn show_without_scope_or_effective_errors_out() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":[]}}"#);

    let out = sb.run(&["show"]);
    assert!(!out.status.success());
    let msg = stderr(&out);
    assert!(
        msg.contains("--scope") || msg.contains("--effective"),
        "expected the error to point at the missing flag pair, got: {msg}"
    );
}

#[test]
fn version_flag_prints_crate_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    // `claude-scope-cli <version>`. Avoid pinning the exact version so the
    // test survives release-please bumps; just check the binary name and a
    // version-shaped trailer.
    assert!(s.starts_with("claude-scope-cli "));
    assert!(s.split_whitespace().nth(1).is_some());
}
