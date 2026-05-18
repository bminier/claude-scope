//! `claude-scope-cli` — terminal front end for the same backend the GUI uses.
//!
//! Re-uses `claude_scope_lib` for scope discovery, atomic writes, and the
//! move-leaf primitive. The GUI's `#[tauri::command]` wrappers and this
//! binary call the same impl functions, so the two surfaces can't drift on
//! semantics.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{json, Value};

use claude_scope_lib::app_info::AppInfo;
use claude_scope_lib::commands::{
    apply_move_leaf_impl, build_loaded, diff_move_leaf_impl, MoveLeafPreview, MoveLeafRequest,
};
use claude_scope_lib::io_atomic::{self, BackupTracker};
use claude_scope_lib::model::{PathSeg, PermissionKind};
use claude_scope_lib::projects::{self, KnownProject};
use claude_scope_lib::scope::{self, Scope, ScopePaths};
use claude_scope_lib::watcher::WatchState;

#[derive(Debug, Parser)]
#[command(
    name = "claude-scope-cli",
    version,
    about = "View and move Claude Code settings between scopes from the terminal",
    long_about = None,
)]
struct Cli {
    /// Project root override. Defaults to walking up from the current directory
    /// for the nearest `.git` or `.claude/` entry.
    #[arg(long, value_name = "PATH", global = true)]
    project_dir: Option<PathBuf>,

    /// Home directory override. Redirects `user` and `user-local` scopes to a
    /// sandbox home for dogfooding without touching the real `~/.claude/`.
    #[arg(long, value_name = "PATH", global = true)]
    home_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List recognized scopes and their on-disk state.
    Scopes {
        /// Emit machine-readable JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Show one scope's raw contents, or the effective merged view.
    Show {
        /// Scope to print. Mutually exclusive with `--effective`.
        #[arg(long, value_name = "SCOPE", conflicts_with = "effective")]
        scope: Option<ScopeArg>,

        /// Print the effective merged view across all scopes.
        #[arg(long, conflicts_with = "scope")]
        effective: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// List permission rules across scopes.
    ListRules {
        /// Limit to a single scope.
        #[arg(long, value_name = "SCOPE")]
        scope: Option<ScopeArg>,

        /// Limit to a single permission kind.
        #[arg(long, value_name = "KIND")]
        kind: Option<KindArg>,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// List Claude projects discovered on this machine via the
    /// `~/.claude/projects/` transcript registry (#106).
    ListProjects {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Print the same diagnostic block the GUI's About dialog shows (#21).
    /// Useful for bug reports: paste the output into the issue. `--version`
    /// (clap-builtin) still prints just the version line.
    Version {
        /// Emit machine-readable JSON instead of the Markdown block.
        #[arg(long)]
        json: bool,
    },

    /// Move a permission rule between scopes. Writes are atomic and create a
    /// `.bak` of the original on the first write of the session.
    Move {
        /// The permission rule string to move, exactly as it appears on disk
        /// (e.g. `"Bash(git status)"`).
        rule: String,

        /// Permission kind the rule belongs to.
        #[arg(long, value_name = "KIND")]
        kind: KindArg,

        /// Source scope.
        #[arg(long, value_name = "SCOPE")]
        from: ScopeArg,

        /// Destination scope.
        #[arg(long, value_name = "SCOPE")]
        to: ScopeArg,

        /// Print the preview without writing anything.
        #[arg(long)]
        dry_run: bool,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScopeArg {
    Local,
    Project,
    #[value(name = "user-local")]
    UserLocal,
    User,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Scope {
        match s {
            ScopeArg::Local => Scope::Local,
            ScopeArg::Project => Scope::Project,
            ScopeArg::UserLocal => Scope::UserLocal,
            ScopeArg::User => Scope::User,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum KindArg {
    Allow,
    Deny,
    Ask,
}

impl KindArg {
    fn key(self) -> &'static str {
        match self {
            KindArg::Allow => "allow",
            KindArg::Deny => "deny",
            KindArg::Ask => "ask",
        }
    }

    fn as_permission_kind(self) -> PermissionKind {
        match self {
            KindArg::Allow => PermissionKind::Allow,
            KindArg::Deny => PermissionKind::Deny,
            KindArg::Ask => PermissionKind::Ask,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // `list-projects` operates on the home registry only and doesn't need a
    // resolved project root, so dispatch it before `resolve_paths` to avoid
    // a spurious walk-up when the CLI is invoked outside any repo.
    if let Command::ListProjects { json } = cli.command {
        return cmd_list_projects(cli.home_dir.as_deref(), json);
    }
    // `version` doesn't read any files either — same dispatch-early reason.
    // A user pasting `claude-scope-cli version` from `/tmp` for a bug report
    // shouldn't error on missing settings.
    if let Command::Version { json } = cli.command {
        return cmd_version(json);
    }

    let paths = resolve_paths(cli.project_dir.as_deref(), cli.home_dir.as_deref())?;
    match cli.command {
        Command::Scopes { json } => cmd_scopes(&paths, json),
        Command::Show {
            scope,
            effective,
            json,
        } => cmd_show(&paths, scope, effective, json),
        Command::ListRules { scope, kind, json } => cmd_list_rules(&paths, scope, kind, json),
        Command::ListProjects { .. } => unreachable!("handled above"),
        Command::Version { .. } => unreachable!("handled above"),
        Command::Move {
            rule,
            kind,
            from,
            to,
            dry_run,
            json,
        } => cmd_move(&paths, &rule, kind, from.into(), to.into(), dry_run, json),
    }
}

/// Resolve scope paths for the CLI.
///
/// When `--project-dir` is explicit, the CLI treats it as a literal — no
/// walk-up to a parent `.git`. The GUI's `scope::resolve_with_home` walks
/// upward to discover a repo root from the picker's selection, which is the
/// right default there but the wrong default for a CLI flag the user just
/// typed. When no override is given, fall through to the shared lib so the
/// CLI still finds the project root from anywhere inside a repo.
fn resolve_paths(
    project_dir: Option<&std::path::Path>,
    home_dir: Option<&std::path::Path>,
) -> Result<ScopePaths, Box<dyn std::error::Error>> {
    let Some(project_root) = project_dir else {
        return Ok(scope::resolve_with_home(None, home_dir)?);
    };
    let home = home_dir
        .map(std::path::Path::to_path_buf)
        .or_else(dirs::home_dir);
    Ok(ScopePaths {
        project_dir: project_root.to_path_buf(),
        local: Some(project_root.join(".claude").join("settings.local.json")),
        project: Some(project_root.join(".claude").join("settings.json")),
        user_local: home
            .as_ref()
            .map(|h| h.join(".claude").join("settings.local.json")),
        user: home.map(|h| h.join(".claude").join("settings.json")),
    })
}

fn cmd_scopes(paths: &ScopePaths, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    #[derive(Serialize)]
    struct ScopeEntry {
        scope: String,
        path: Option<String>,
        exists: bool,
        parse_error: Option<String>,
    }

    let mut entries = Vec::with_capacity(Scope::ALL.len());
    for scope in Scope::ALL {
        let path = paths.path_for(scope);
        let (exists, parse_error) = match path {
            Some(p) => match io_atomic::load(p) {
                Ok(Some(_)) => (true, None),
                Ok(None) => (false, None),
                Err(e) => (p.exists(), Some(e.to_string())),
            },
            None => (false, None),
        };
        entries.push(ScopeEntry {
            scope: scope.label().to_string(),
            path: path.map(|p| p.display().to_string()),
            exists,
            parse_error,
        });
    }

    if json {
        let out = json!({
            "project-dir": paths.project_dir.display().to_string(),
            "scopes": entries,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("project-dir: {}", paths.project_dir.display());
        for e in &entries {
            let status = if e.exists { "present" } else { "absent" };
            let path = e.path.as_deref().unwrap_or("(unresolvable)");
            println!("  {:<11} [{status}] {path}", e.scope);
            if let Some(err) = &e.parse_error {
                println!("              parse-error: {err}");
            }
        }
    }
    Ok(())
}

fn cmd_show(
    paths: &ScopePaths,
    scope: Option<ScopeArg>,
    effective: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = build_loaded(paths)?;

    if effective {
        let perms = &loaded.combined_permissions;
        if json {
            let out = json!({
                "project-dir": loaded.project_dir,
                "effective": {
                    "permissions": {
                        "allow": perms.allow,
                        "deny": perms.deny,
                        "ask": perms.ask,
                    },
                },
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("effective merged permissions ({}):", loaded.project_dir);
            print_permissions_section("allow", &perms.allow);
            print_permissions_section("deny", &perms.deny);
            print_permissions_section("ask", &perms.ask);
        }
        return Ok(());
    }

    let target = scope.ok_or_else::<Box<dyn std::error::Error>, _>(|| {
        "show requires either --scope <SCOPE> or --effective".into()
    })?;
    let target_scope: Scope = target.into();
    let view = loaded
        .scopes
        .iter()
        .find(|v| v.scope == target_scope)
        .ok_or_else::<Box<dyn std::error::Error>, _>(|| {
            format!(
                "internal error: scope `{}` missing from load",
                target_scope.label()
            )
            .into()
        })?;

    if json {
        let out = json!({
            "project-dir": loaded.project_dir,
            "scope": target_scope.label(),
            "path": view.path,
            "exists": view.exists,
            "values": view.values,
            "parse-error": view.parse_error,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let path = view.path.as_deref().unwrap_or("(unresolvable)");
        let status = if view.exists { "present" } else { "absent" };
        println!("scope: {} [{status}] {path}", target_scope.label());
        if let Some(err) = &view.parse_error {
            println!("parse-error: {err}");
            return Ok(());
        }
        if view.values.is_empty() {
            println!("(no top-level keys)");
        } else {
            let pretty = serde_json::to_string_pretty(&Value::Object(view.values.clone()))?;
            println!("{pretty}");
        }
    }
    Ok(())
}

fn print_permissions_section(kind: &str, rules: &[String]) {
    println!("  {kind}:");
    if rules.is_empty() {
        println!("    (none)");
    } else {
        for rule in rules {
            println!("    - {rule}");
        }
    }
}

fn cmd_list_rules(
    paths: &ScopePaths,
    scope_filter: Option<ScopeArg>,
    kind_filter: Option<KindArg>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    #[derive(Serialize)]
    struct RuleRow {
        scope: String,
        kind: String,
        rule: String,
    }

    let target_scope: Option<Scope> = scope_filter.map(Into::into);
    let kind_str = kind_filter.map(|k| k.key());

    let mut rows = Vec::new();
    for scope in Scope::ALL {
        if let Some(want) = target_scope {
            if scope != want {
                continue;
            }
        }
        let Some(path) = paths.path_for(scope) else {
            continue;
        };
        let Ok(Some(doc)) = io_atomic::load(path) else {
            continue;
        };
        for k in ["allow", "deny", "ask"] {
            if let Some(want_kind) = kind_str {
                if k != want_kind {
                    continue;
                }
            }
            for rule in rules_at(&doc, k) {
                rows.push(RuleRow {
                    scope: scope.label().to_string(),
                    kind: k.to_string(),
                    rule,
                });
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("(no rules)");
    } else {
        for row in &rows {
            println!("{:<11} {:<6} {}", row.scope, row.kind, row.rule);
        }
    }
    Ok(())
}

fn cmd_list_projects(
    home: Option<&std::path::Path>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let entries = projects::list_known_projects(home)?;
    if json {
        // Wrap in `KnownProjectOut` so the wire format is stable even if
        // the lib struct grows new fields. `serde_json::to_value` on
        // `KnownProject` would silently start exposing those fields.
        #[derive(Serialize)]
        struct KnownProjectOut<'a> {
            name: &'a str,
            root: String,
        }
        let out: Vec<KnownProjectOut<'_>> = entries
            .iter()
            .map(|p: &KnownProject| KnownProjectOut {
                name: &p.name,
                root: p.root.display().to_string(),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "projects": out }))?
        );
    } else if entries.is_empty() {
        println!("(no Claude projects discovered)");
    } else {
        for p in &entries {
            println!("{}\t{}", p.name, p.root.display());
        }
    }
    Ok(())
}

fn cmd_version(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // No webview in the CLI process — `AppInfo::to_markdown()` will render
    // the field as `unknown`, which is the right answer rather than a
    // misleading "0.0.0".
    let info = AppInfo::build(None);
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("{}", info.to_markdown());
    }
    Ok(())
}

fn cmd_move(
    paths: &ScopePaths,
    rule: &str,
    kind: KindArg,
    from: Scope,
    to: Scope,
    dry_run: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if from == to {
        return Err("--from and --to must differ".into());
    }
    let from_path = paths
        .path_for(from)
        .ok_or_else::<Box<dyn std::error::Error>, _>(|| {
            format!("cannot resolve path for scope `{}`", from.label()).into()
        })?;
    let from_doc =
        io_atomic::load(from_path)?.ok_or_else::<Box<dyn std::error::Error>, _>(|| {
            format!("source file {} does not exist", from_path.display()).into()
        })?;
    let idx = locate_rule(&from_doc, kind.as_permission_kind(), rule)
        .ok_or_else::<Box<dyn std::error::Error>, _>(|| {
            format!(
                "rule `{rule}` not found in {} permissions.{} ({})",
                from.label(),
                kind.key(),
                from_path.display()
            )
            .into()
        })?;

    let req = MoveLeafRequest {
        path: vec![
            PathSeg::Key("permissions".into()),
            PathSeg::Key(kind.key().into()),
            PathSeg::Index(idx),
        ],
        from,
        to,
        to_kind: None,
    };

    if dry_run {
        let preview = diff_move_leaf_impl(paths, &req)?;
        print_preview(&preview, json)?;
        return Ok(());
    }

    apply_move_leaf_impl(paths, &req, &BackupTracker::new(), &WatchState::default())?;

    if json {
        let out = json!({
            "moved": true,
            "rule": rule,
            "kind": kind.key(),
            "from": from.label(),
            "to": to.label(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "moved {} rule `{rule}` from {} to {}",
            kind.key(),
            from.label(),
            to.label()
        );
    }
    Ok(())
}

fn rules_at(doc: &claude_scope_lib::model::SettingsDoc, kind_key: &str) -> Vec<String> {
    let Some(arr) = doc.get_at_path(&[
        PathSeg::Key("permissions".into()),
        PathSeg::Key(kind_key.into()),
    ]) else {
        return Vec::new();
    };
    let Some(items) = arr.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn locate_rule(
    doc: &claude_scope_lib::model::SettingsDoc,
    kind: PermissionKind,
    rule: &str,
) -> Option<usize> {
    let key = match kind {
        PermissionKind::Allow => "allow",
        PermissionKind::Deny => "deny",
        PermissionKind::Ask => "ask",
    };
    let arr = doc.get_at_path(&[PathSeg::Key("permissions".into()), PathSeg::Key(key.into())])?;
    arr.as_array()?
        .iter()
        .position(|v| v.as_str() == Some(rule))
}

fn print_preview(preview: &MoveLeafPreview, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string_pretty(preview)?);
        return Ok(());
    }
    println!("dry-run preview:");
    println!(
        "  from: scope={} path={} will-write={}",
        preview.from.scope.label(),
        preview.from.file_path,
        preview.from.will_write
    );
    println!(
        "  to:   scope={} path={} will-write={}",
        preview.to.scope.label(),
        preview.to.file_path,
        preview.to.will_write
    );
    if let Some(note) = &preview.to.note {
        println!("  note: {note}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_scope_lib::io_atomic::Indent;
    use claude_scope_lib::model::SettingsDoc;
    use serde_json::json;
    use std::path::Path;

    fn doc_from(value: serde_json::Value) -> SettingsDoc {
        SettingsDoc::from_value(value, Indent::Spaces(2))
    }

    #[test]
    fn resolve_paths_uses_project_dir_literally_without_walk_up() {
        // The CLI must honor `--project-dir <PATH>` as-is. The GUI's
        // `scope::resolve_with_home` walks up to a parent `.git`/`.claude`,
        // which would hijack to an unrelated repo on a system where any
        // ancestor (e.g. $HOME) happens to be a git working tree. Pin the
        // CLI-literal behavior so a future refactor can't quietly route
        // back through the walk-up.
        let project = Path::new("/tmp/clitest-fixture/project");
        let home = Path::new("/tmp/clitest-fixture/home");
        let paths = resolve_paths(Some(project), Some(home)).unwrap();
        assert_eq!(paths.project_dir, project);
        assert_eq!(
            paths.local.as_deref().unwrap(),
            project.join(".claude").join("settings.local.json")
        );
        assert_eq!(
            paths.project.as_deref().unwrap(),
            project.join(".claude").join("settings.json")
        );
    }

    #[test]
    fn resolve_paths_roots_user_scopes_under_home_override() {
        // Sandbox / dogfooding parity with the GUI's `--home` override.
        // Verify user + user-local both resolve under the supplied home,
        // not the real `dirs::home_dir()`.
        let project = Path::new("/tmp/clitest-fixture/project");
        let home = Path::new("/tmp/clitest-fixture/home");
        let paths = resolve_paths(Some(project), Some(home)).unwrap();
        assert_eq!(
            paths.user.as_deref().unwrap(),
            home.join(".claude").join("settings.json")
        );
        assert_eq!(
            paths.user_local.as_deref().unwrap(),
            home.join(".claude").join("settings.local.json")
        );
    }

    #[test]
    fn rules_at_returns_rules_for_each_kind() {
        let doc = doc_from(json!({
            "permissions": {
                "allow": ["Bash(git status)", "Read(**)"],
                "deny": ["WebFetch(domain:evil.example)"],
            }
        }));
        assert_eq!(
            rules_at(&doc, "allow"),
            vec!["Bash(git status)".to_string(), "Read(**)".to_string()]
        );
        assert_eq!(
            rules_at(&doc, "deny"),
            vec!["WebFetch(domain:evil.example)".to_string()]
        );
        assert!(rules_at(&doc, "ask").is_empty());
    }

    #[test]
    fn rules_at_degrades_to_empty_when_permissions_absent() {
        // A settings file without a `permissions` key (or with a non-array
        // shape after hand-editing) must not panic — match the lib's
        // "partial corruption shouldn't break the UI" stance.
        let doc = doc_from(json!({ "theme": "dark" }));
        assert!(rules_at(&doc, "allow").is_empty());

        let weird = doc_from(json!({ "permissions": { "allow": "not-an-array" } }));
        assert!(rules_at(&weird, "allow").is_empty());
    }

    #[test]
    fn locate_rule_finds_index_and_misses_correctly() {
        let doc = doc_from(json!({
            "permissions": { "allow": ["Bash(git status)", "Read(**)"] }
        }));
        assert_eq!(
            locate_rule(&doc, PermissionKind::Allow, "Read(**)"),
            Some(1)
        );
        assert_eq!(locate_rule(&doc, PermissionKind::Allow, "Missing(*)"), None);
        // Looking under a kind that doesn't exist on disk must also miss
        // rather than panic.
        assert_eq!(
            locate_rule(&doc, PermissionKind::Deny, "Bash(git status)"),
            None
        );
    }
}
