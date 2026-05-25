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
    config_dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let project = tmp.path().join("project");
        let home = tmp.path().join("home");
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        Self {
            _tmp: tmp,
            project,
            home,
            config_dir,
        }
    }

    /// Write the CLI's `config.json` (the file `preferences::load()` reads).
    /// The integration tests pass `CLAUDE_SCOPE_CONFIG_DIR` to redirect
    /// `preferences::config_path()` here, so each test gets isolated prefs.
    fn write_pref(&self, json: &str) {
        std::fs::write(self.config_dir.join("config.json"), json).unwrap();
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

    /// Seed a fake `~/.claude/projects/<encoded>/<session>.jsonl` referencing
    /// `cwd_root`. The created root has a `.claude/` directory so it passes
    /// the loadability filter in `projects::list_known_projects`.
    fn seed_known_project(&self, encoded: &str, name: &str) -> PathBuf {
        let cwd = self._tmp.path().join(name);
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        let dir = self.home.join(".claude").join("projects").join(encoded);
        std::fs::create_dir_all(&dir).unwrap();
        let cwd_str = cwd.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(
            dir.join("0001-session.jsonl"),
            format!(r#"{{"sessionId":"s","cwd":"{cwd_str}"}}"#),
        )
        .unwrap();
        cwd
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .env("CLAUDE_SCOPE_CONFIG_DIR", &self.config_dir)
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

#[test]
fn list_projects_reports_empty_when_registry_missing() {
    let sb = Sandbox::new();
    // No `~/.claude/projects/` exists in the sandbox home.
    let out = sb.run(&["list-projects"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("(no Claude projects discovered)"));
}

#[test]
fn list_projects_lists_seeded_projects_sorted() {
    let sb = Sandbox::new();
    sb.seed_known_project("D--gamma", "gamma");
    sb.seed_known_project("D--alpha", "alpha");
    sb.seed_known_project("D--Beta", "Beta");

    let out = sb.run(&["list-projects"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let s = stdout(&out);
    // Each line: "<name>\t<root>". Pull names in output order and assert
    // alphabetical (case-insensitive).
    let names: Vec<&str> = s
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split('\t').next())
        .collect();
    assert_eq!(names, vec!["alpha", "Beta", "gamma"]);
}

#[test]
fn list_projects_json_shape_is_stable() {
    let sb = Sandbox::new();
    sb.seed_known_project("D--alpha", "alpha");

    let out = sb.run(&["list-projects", "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("non-json stdout");
    let arr = v["projects"].as_array().expect("projects array missing");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "alpha");
    assert!(arr[0]["root"].as_str().unwrap().ends_with("alpha"));
}

#[test]
fn list_projects_hides_stale_roots_with_no_dot_claude() {
    let sb = Sandbox::new();
    // Seed a transcript that points at a path with no `.claude/` —
    // simulating a project that was deleted or never had Claude state.
    let bogus = sb._tmp.path().join("stale-root");
    std::fs::create_dir_all(&bogus).unwrap();
    let dir = sb.home.join(".claude").join("projects").join("D--stale");
    std::fs::create_dir_all(&dir).unwrap();
    let cwd_str = bogus.to_string_lossy().replace('\\', "\\\\");
    std::fs::write(dir.join("0001.jsonl"), format!(r#"{{"cwd":"{cwd_str}"}}"#)).unwrap();
    // And a real one alongside so the test asserts filtering, not just
    // emptiness.
    sb.seed_known_project("D--real", "real");

    let out = sb.run(&["list-projects"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let s = stdout(&out);
    assert!(s.contains("real"), "expected loadable project in output");
    assert!(
        !s.contains("stale-root"),
        "stale root should be filtered out: {s}"
    );
}

#[test]
fn version_subcommand_prints_diagnostic_block() {
    // No sandbox needed — `version` doesn't touch any files, so it runs
    // outside the Sandbox's `--project-dir` / `--home-dir` plumbing.
    let out = Command::new(BIN)
        .arg("version")
        .output()
        .expect("failed to execute claude-scope-cli");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let s = stdout(&out);
    // Every line of `AppInfo::to_markdown` should appear in stdout.
    assert!(s.contains("ClaudeScope:"));
    assert!(s.contains("Tauri:"));
    assert!(s.contains("Rust (MSRV):"));
    assert!(s.contains("OS:"));
    // CLI has no webview → renders as the explicit `unknown` sentinel.
    assert!(s.contains("WebView: unknown"));
}

#[test]
fn version_subcommand_json_shape_is_stable() {
    let out = Command::new(BIN)
        .args(["version", "--json"])
        .output()
        .expect("failed to execute claude-scope-cli");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("non-json stdout");
    // Every required key appears. Assert presence (not exact values) so the
    // test survives release-please bumps and platform differences.
    for key in [
        "version",
        "git_sha",
        "tauri_version",
        "webview_version",
        "rust_version",
        "os",
        "arch",
    ] {
        assert!(v.get(key).is_some(), "missing key `{key}` in: {v}");
    }
    // CLI process: `webview_version` must be null, not a fabricated string.
    assert!(v["webview_version"].is_null());
}

// ---------------------------------------------------------------------------
// v0.6 release-gate fixes: audit emission, backup pref, restore log recheck
// ---------------------------------------------------------------------------

/// #164 — `claude-scope-cli move` must append an audit record. Pre-fix the
/// command wrote settings files but never called `audit::append`, so the
/// move was invisible to History / Undo / Redo.
#[test]
fn cli_move_appends_audit_record() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(git status)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":[]}}"#);

    let mv = sb.run(&[
        "move",
        "Bash(git status)",
        "--kind",
        "allow",
        "--from",
        "project",
        "--to",
        "user",
    ]);
    assert!(mv.status.success(), "stderr: {}", stderr(&mv));

    let hist = sb.run(&["history", "--json"]);
    assert!(hist.status.success(), "stderr: {}", stderr(&hist));
    let page: serde_json::Value = serde_json::from_str(&stdout(&hist)).expect("non-json history");
    let records = page["records"]
        .as_array()
        .expect("history JSON must have a records array");
    assert_eq!(
        records.len(),
        1,
        "expected exactly one audit entry from the CLI move, got: {records:?}"
    );
    // AuditRecordView flattens Record into the parent object via
    // #[serde(flatten)], so `kind` / `actor` live at the top level
    // alongside `ts_ms` — not nested under `record`.
    let rec = &records[0];
    assert_eq!(rec["kind"], "move", "audit kind should be move");
    assert_eq!(
        rec["actor"], "cli",
        "audit actor should be cli for a CLI-initiated move"
    );
}

/// #168 — CLI write paths must honor `Preferences::backup_on_write`. With
/// the pref set to `false`, no `.bak` should be created.
#[test]
fn cli_move_respects_backup_on_write_false_pref() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(git status)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":[]}}"#);
    sb.write_pref(r#"{"backup_on_write":false}"#);

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

    // The move itself must still succeed — pref only affects .bak creation.
    let project_allow = read_allow(&sb.project.join(".claude").join("settings.json"));
    assert!(project_allow.is_empty(), "rule should have moved out");

    // No .bak alongside either touched file.
    assert!(
        !sb.project
            .join(".claude")
            .join("settings.json.bak")
            .exists(),
        "project settings.json.bak must not exist when backup_on_write=false"
    );
    assert!(
        !sb.home.join(".claude").join("settings.json.bak").exists(),
        "user settings.json.bak must not exist when backup_on_write=false"
    );
}

/// #165 — CLI restore must re-read the audit log after the confirm prompt
/// and refuse to apply if another session appended between preview and
/// apply. Uses a stdin pipe with a brief sleep so the child has time to
/// build the preview + print the prompt; then we inject a concurrent
/// append before sending `y`.
#[test]
fn cli_undo_aborts_when_audit_log_changes_during_confirm_prompt() {
    use std::io::Write as _;
    use std::process::Stdio;
    use std::time::Duration;

    use claude_scope_lib::audit;

    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(git status)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":[]}}"#);

    // Seed the audit log with one CLI move so there's something to undo.
    let mv = sb.run(&[
        "move",
        "Bash(git status)",
        "--kind",
        "allow",
        "--from",
        "project",
        "--to",
        "user",
    ]);
    assert!(mv.status.success(), "seed move failed: {}", stderr(&mv));

    // Snapshot the project file so we can confirm the aborted undo did NOT
    // overwrite it.
    let project_after_move = sb.project_settings();

    // Spawn `undo` with stdin piped — we'll send "y\n" later but only
    // AFTER injecting a concurrent append.
    let mut child = Command::new(BIN)
        .env("CLAUDE_SCOPE_CONFIG_DIR", &sb.config_dir)
        .args([
            "--project-dir",
            sb.project.to_str().unwrap(),
            "--home-dir",
            sb.home.to_str().unwrap(),
        ])
        .arg("undo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claude-scope-cli undo");

    // The child reads the audit log, prints the preview, then prompts. By
    // the time we wake up, the log read is done (expected_tail_id captured)
    // and the child is blocked on stdin. 400ms is generous on a CI-ish
    // machine; bump if this turns flaky.
    std::thread::sleep(Duration::from_millis(400));

    // Inject a fresh audit record via the lib — this is the "concurrent
    // GUI/CLI session" the issue's failure scenario describes. The child's
    // post-prompt re-read must notice this and refuse to apply.
    let bogus = audit::Record::new(
        audit::Kind::Add,
        audit::LeafKind::PermissionRule,
        audit::Actor::Cli,
        None,
        None,
        Some(audit::Side {
            scope: claude_scope_lib::scope::Scope::Project,
            file_path: sb.project.join(".claude").join("settings.json"),
            top_level_key: "permissions".to_string(),
            key_before: Some(serde_json::json!({"allow": []})),
            key_after: Some(serde_json::json!({"allow": ["Sleep(injected)"]})),
        }),
        vec![],
        None,
    );
    audit::append(&bogus, Some(sb.home.as_path())).expect("inject bogus audit append");

    // Now confirm the prompt. The child reads "y", proceeds to its
    // post-prompt re-read, sees the tail shifted, and aborts.
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(b"y\n")
        .expect("write y to child stdin");

    let out = child
        .wait_with_output()
        .expect("wait for claude-scope-cli undo");
    assert!(
        !out.status.success(),
        "expected non-zero exit on stale-log apply, got success. stderr: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("log changed"),
        "expected staleness error on stderr, got: {err}"
    );

    // The aborted undo must not have touched the on-disk settings.
    assert_eq!(
        sb.project_settings(),
        project_after_move,
        "project settings must be unchanged after a refused undo"
    );
}

/// #170 — CLI undo / redo / restore must refuse against a log with
/// unreadable lines. A corrupt restore line could leave the state
/// machine pointing at an op that's already undone; running undo
/// against that would emit a duplicate restore entry. Refuse with a
/// clear message and non-zero status instead.
#[test]
fn cli_undo_refuses_when_audit_log_has_unreadable_lines() {
    let sb = Sandbox::new();
    sb.write_project(r#"{"permissions":{"allow":["Bash(git status)"]}}"#);
    sb.write_user(r#"{"permissions":{"allow":[]}}"#);

    // Seed one valid CLI move so there's something the state machine
    // would have considered undoable.
    let mv = sb.run(&[
        "move",
        "Bash(git status)",
        "--kind",
        "allow",
        "--from",
        "project",
        "--to",
        "user",
    ]);
    assert!(mv.status.success(), "seed move failed: {}", stderr(&mv));

    // Append a malformed line to the audit log. `audit::read_all` will
    // count it as `skipped`. The CLI's `read_audit_log` should refuse.
    let audit_path = sb
        .home
        .join(".claude")
        .join("claude-scope")
        .join("audit.jsonl");
    let mut existing = std::fs::read_to_string(&audit_path).expect("read audit log");
    existing.push_str("{ not even close to valid json }\n");
    std::fs::write(&audit_path, existing).expect("rewrite audit log with corrupt line");

    let out = sb.run(&["undo", "--yes"]);
    assert!(
        !out.status.success(),
        "expected non-zero exit on degraded log, got success. stderr: {}",
        stderr(&out)
    );
    let err = stderr(&out);
    assert!(
        err.contains("unreadable") && err.contains("partial log"),
        "expected staleness error on stderr, got: {err}"
    );
}
