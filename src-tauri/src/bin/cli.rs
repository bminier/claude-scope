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
use ulid::Ulid;

use claude_scope_lib::app_info::AppInfo;
use claude_scope_lib::audit::{self, Record as AuditRecord};
use claude_scope_lib::commands::{
    apply_move_leaf_impl, apply_restore_plan, build_loaded, build_restore_plan, diff_move_leaf_impl,
    plan_restore_to, preview_restore_plan, restore_record, AuditLogPage, AuditRecordView,
    MoveLeafPreview, MoveLeafRequest, RestorePlan, RestorePreview,
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

    /// Show recent entries from the audit log (#19 phase 5 — history slice).
    /// Reads `~/.claude/claude-scope/audit.jsonl`, honoring `--home-dir` so a
    /// sandboxed session sees the scratch log instead of the real one. Newer
    /// entries are listed first.
    History {
        /// Filter to entries newer than this duration (`5m`, `2h`, `1d`).
        /// Suffixes: `s` seconds, `m` minutes, `h` hours, `d` days. No
        /// suffix is rejected — the parser refuses to guess units rather
        /// than silently interpreting `5` as one of "5 seconds" or
        /// "5 minutes" depending on platform convention.
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        /// Cap the number of rows. Default 20; pass `0` for unlimited.
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
        /// Filter to a single audit kind. Repeat the flag to widen the
        /// filter — clap collects multiples into a Vec.
        #[arg(long, value_name = "KIND")]
        kind: Vec<AuditKindArg>,
        /// Emit machine-readable JSON in the same `AuditLogPage` shape the
        /// GUI's `list_audit_records` IPC returns, so a Claude Code skill
        /// (#13) can deserialize CLI output and IPC payloads through one
        /// shared type.
        #[arg(long)]
        json: bool,
    },

    /// Undo the most recent change in the audit log (#19 phase 5). Reverts
    /// a move / add / delete / change-kind / restore-to-point by writing
    /// the affected files back to their pre-op snapshots, then logs a
    /// `restore` entry (actor `cli`) so a GUI session sees it in History.
    Undo {
        /// Print the planned restore as a diff and exit without writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive `Apply? [y/N]` prompt.
        #[arg(long)]
        yes: bool,
        /// Emit machine-readable JSON: the `RestorePreview` for `--dry-run`,
        /// otherwise the `restore` entry as an `AuditRecordView`.
        #[arg(long)]
        json: bool,
    },

    /// Redo the most recently undone change (#19 phase 5). Refuses with a
    /// non-zero exit when a change was made after the last undo (the
    /// forward stack is then ambiguous — same rule as the GUI).
    Redo {
        /// Print the planned restore as a diff and exit without writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive `Apply? [y/N]` prompt.
        #[arg(long)]
        yes: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Restore every file affected by an audit entry — and the entries
    /// after it — back to its state before that entry (#19 phase 5).
    Restore {
        /// Audit entry id: a full ULID or any unique prefix (ULIDs
        /// lex-sort, so prefix matching is cheap and unambiguous).
        id: String,
        /// Print the planned restore as a diff and exit without writing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive `Apply? [y/N]` prompt.
        #[arg(long)]
        yes: bool,
        /// Emit machine-readable JSON.
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

/// CLI counterpart to `audit::Kind`. Mirrored locally rather than re-using
/// the lib enum so the wire format of `--kind <value>` stays a CLI concern
/// (kebab-case as clap renders ValueEnum), independent of the JSON
/// snake_case the audit module already pins.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum AuditKindArg {
    Move,
    Add,
    Delete,
    #[value(name = "change-kind")]
    ChangeKind,
    Restore,
}

impl AuditKindArg {
    fn matches(self, k: audit::Kind) -> bool {
        matches!(
            (self, k),
            (AuditKindArg::Move, audit::Kind::Move)
                | (AuditKindArg::Add, audit::Kind::Add)
                | (AuditKindArg::Delete, audit::Kind::Delete)
                | (AuditKindArg::ChangeKind, audit::Kind::ChangeKind)
                | (AuditKindArg::Restore, audit::Kind::Restore)
        )
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
    // `history` reads only the audit log, which lives under the home
    // directory. No project root needed — running it from outside a repo
    // (e.g. `~`) is a perfectly valid "what did I change everywhere?"
    // workflow.
    if let Command::History {
        since,
        limit,
        kind,
        json,
    } = &cli.command
    {
        return cmd_history(
            cli.home_dir.as_deref(),
            since.as_deref(),
            *limit,
            kind,
            *json,
        );
    }
    // `undo` / `redo` / `restore` operate purely off the audit log and the
    // absolute file paths recorded in it — like `history`, they need no
    // project root, so dispatch before `resolve_paths`.
    if let Command::Undo {
        dry_run,
        yes,
        json,
    } = cli.command
    {
        return cmd_undo(cli.home_dir.as_deref(), dry_run, yes, json);
    }
    if let Command::Redo {
        dry_run,
        yes,
        json,
    } = cli.command
    {
        return cmd_redo(cli.home_dir.as_deref(), dry_run, yes, json);
    }
    if let Command::Restore {
        id,
        dry_run,
        yes,
        json,
    } = &cli.command
    {
        return cmd_restore(cli.home_dir.as_deref(), id, *dry_run, *yes, *json);
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
        Command::History { .. } => unreachable!("handled above"),
        Command::Undo { .. } => unreachable!("handled above"),
        Command::Redo { .. } => unreachable!("handled above"),
        Command::Restore { .. } => unreachable!("handled above"),
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
    // CLI doesn't surface the Move-to filter (#111) — `list-projects` is
    // meant as a discovery / scripting hook, where any silent filter would
    // surprise the caller. Pass the no-op default so the output matches
    // what was on disk.
    let entries = projects::list_known_projects(home, &projects::ProjectsFilter::default())?;
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

    apply_move_leaf_impl(
        paths,
        &req,
        Some(&BackupTracker::new()),
        &WatchState::default(),
    )?;

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

/// Read the audit log for a CLI restore command, surfacing the
/// unreadable-line count the same way `history` does. `--home-dir`
/// redirects it identically.
fn read_audit_log(
    home: Option<&std::path::Path>,
) -> Result<Vec<AuditRecord>, Box<dyn std::error::Error>> {
    let (records, skipped) = audit::read_all(home)?;
    if skipped > 0 {
        eprintln!(
            "note: {skipped} unreadable {} skipped in the log",
            if skipped == 1 { "entry" } else { "entries" }
        );
    }
    Ok(records)
}

fn restore_action_label(direction: audit::RestoreDirection) -> &'static str {
    match direction {
        audit::RestoreDirection::Undo => "undo",
        audit::RestoreDirection::Redo => "redo",
        audit::RestoreDirection::ToPoint => "restore-to-point",
    }
}

/// Print a restore preview as a human-readable block: the entry being
/// acted on, the spanned-entry count for a restore-to-point, and one line
/// per affected file with its write status.
fn print_restore_preview(preview: &RestorePreview) {
    println!(
        "{}: {} {}",
        restore_action_label(preview.direction),
        format_verb(&preview.target.record),
        extract_rule_summary(&preview.target.record).unwrap_or_default(),
    );
    if matches!(preview.direction, audit::RestoreDirection::ToPoint) {
        println!(
            "  reverting {} logged entr{}",
            preview.ops_spanned,
            if preview.ops_spanned == 1 { "y" } else { "ies" }
        );
    }
    for side in &preview.sides {
        let status = if !side.will_write {
            "no change"
        } else if side.state_mismatch {
            "write — overwrites an external edit"
        } else {
            "write"
        };
        println!(
            "  {:<11} {}  [{status}]",
            side.scope.label(),
            side.file_path
        );
    }
}

/// Prompt `Apply? [y/N]` on the terminal; returns true only for an explicit
/// yes. Any other input (including EOF on a closed stdin) is a "no".
fn confirm_prompt() -> Result<bool, Box<dyn std::error::Error>> {
    use std::io::Write;
    print!("Apply? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Find the single audit entry whose id begins with `prefix` — a full ULID
/// or any unique prefix. ULIDs render as uppercase Crockford base32 and
/// lex-sort, so prefix matching is a cheap `starts_with`.
fn resolve_entry_id(
    records: &[AuditRecord],
    prefix: &str,
) -> Result<Ulid, Box<dyn std::error::Error>> {
    let needle = prefix.trim().to_uppercase();
    if needle.is_empty() {
        return Err("empty audit entry id".into());
    }
    let matches: Vec<&AuditRecord> = records
        .iter()
        .filter(|r| r.id.to_string().starts_with(&needle))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("no audit entry matches id `{prefix}`").into()),
        [one] => Ok(one.id),
        many => {
            let ids: Vec<String> = many.iter().map(|r| r.id.to_string()).collect();
            Err(format!(
                "id `{prefix}` is ambiguous — matches {} entries:\n  {}",
                many.len(),
                ids.join("\n  ")
            )
            .into())
        }
    }
}

/// Shared driver for `undo` / `redo` / `restore`: preview, optionally
/// confirm, apply, and log the resulting `restore` entry (actor `cli`).
fn run_cli_restore(
    home: Option<&std::path::Path>,
    plan: &RestorePlan,
    target: &AuditRecord,
    dry_run: bool,
    yes: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let preview = preview_restore_plan(plan, target)?;
    if dry_run {
        if json {
            println!("{}", serde_json::to_string_pretty(&preview)?);
        } else {
            print_restore_preview(&preview);
            println!("(dry run — nothing written)");
        }
        return Ok(());
    }
    if !yes {
        if !json {
            print_restore_preview(&preview);
        }
        if !confirm_prompt()? {
            // A declined confirm is not an error — mirror the GUI's Cancel.
            if !json {
                println!("aborted");
            }
            return Ok(());
        }
    }
    let files = apply_restore_plan(plan, Some(&BackupTracker::new()), &WatchState::default())?;
    let record = restore_record(plan, audit::Actor::Cli, files);
    // Fail-open: the restore already took effect on disk, so a log-append
    // failure is a warning, not a command failure (matches the GUI).
    if let Err(err) = audit::append(&record, home) {
        eprintln!("warning: restore applied but audit-log append failed: {err}");
    }
    if json {
        let view = AuditRecordView {
            ts_ms: record.id.timestamp_ms(),
            record,
        };
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        println!(
            "{} applied — {} file(s) restored",
            restore_action_label(preview.direction),
            preview.sides.len()
        );
    }
    Ok(())
}

fn cmd_undo(
    home: Option<&std::path::Path>,
    dry_run: bool,
    yes: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let records = read_audit_log(home)?;
    let target = audit::undo_redo_state(&records)
        .undoable
        .ok_or("nothing to undo")?;
    let plan = build_restore_plan(&target, audit::RestoreDirection::Undo)?;
    run_cli_restore(home, &plan, &target, dry_run, yes, json)
}

fn cmd_redo(
    home: Option<&std::path::Path>,
    dry_run: bool,
    yes: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let records = read_audit_log(home)?;
    let state = audit::undo_redo_state(&records);
    if state.sequence_break {
        return Err("redo is unavailable: a change was made after the last undo".into());
    }
    let target = state.redoable.ok_or("nothing to redo")?;
    let plan = build_restore_plan(&target, audit::RestoreDirection::Redo)?;
    run_cli_restore(home, &plan, &target, dry_run, yes, json)
}

fn cmd_restore(
    home: Option<&std::path::Path>,
    id: &str,
    dry_run: bool,
    yes: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let records = read_audit_log(home)?;
    let target_id = resolve_entry_id(&records, id)?;
    let plan = plan_restore_to(&records, target_id)?;
    let target = records
        .iter()
        .find(|r| r.id == target_id)
        .expect("plan_restore_to verified the id is in the log")
        .clone();
    run_cli_restore(home, &plan, &target, dry_run, yes, json)
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

fn cmd_history(
    home: Option<&std::path::Path>,
    since: Option<&str>,
    limit: usize,
    kind_filters: &[AuditKindArg],
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (records, skipped) = audit::read_all(home)?;
    let cutoff_ms = match since {
        Some(s) => Some(cutoff_ms_for_since(s)?),
        None => None,
    };
    let filtered = filter_records(records, kind_filters, cutoff_ms, limit);

    if json {
        // Match the GUI's `AuditLogPage` wire shape verbatim so a Claude
        // Code skill (#13) can deserialize CLI output and IPC payloads
        // through a single type. Building the views inline avoids a public
        // helper in `commands.rs` whose only consumer is this binary.
        let views: Vec<AuditRecordView> = filtered
            .into_iter()
            .map(|r| {
                let ts_ms = r.id.timestamp_ms();
                AuditRecordView { record: r, ts_ms }
            })
            .collect();
        let page = AuditLogPage {
            records: views,
            skipped,
        };
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }

    if filtered.is_empty() {
        println!("(no audit entries)");
    } else {
        for rec in &filtered {
            println!("{}", format_history_line(rec));
        }
    }
    if skipped > 0 {
        eprintln!(
            "note: {skipped} unreadable {} skipped in the log",
            if skipped == 1 { "entry" } else { "entries" }
        );
    }
    Ok(())
}

/// Parse a human-friendly duration suffix (`5m`, `2h`, `1d`, `30s`) into
/// milliseconds. Rejects bare numbers so the parser never has to guess
/// whether `5` means seconds or minutes.
fn parse_duration_ms(s: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty --since value".into());
    }
    let (num_part, mult_ms): (&str, u64) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, 1)
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, 1_000)
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 60_000)
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 3_600_000)
    } else if let Some(rest) = s.strip_suffix('d') {
        (rest, 86_400_000)
    } else {
        return Err(format!("unrecognized --since unit in `{s}` — use s/m/h/d").into());
    };
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("invalid --since count in `{s}`"))?;
    n.checked_mul(mult_ms)
        .ok_or_else(|| format!("--since `{s}` overflows").into())
}

/// Compute the floor ts_ms cutoff for a `--since DURATION` value:
/// `now - duration`. Saturates at zero if the duration overflows the
/// wall clock backwards, so a 9999-year filter still returns "everything"
/// rather than wrapping.
fn cutoff_ms_for_since(s: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let dur_ms = parse_duration_ms(s)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(now_ms.saturating_sub(dur_ms))
}

/// Apply the CLI's filters and ordering to a fresh `read_all` result.
/// Kept as a pure function over the inputs so the test suite can drive it
/// without reaching for a real audit log.
fn filter_records(
    mut records: Vec<AuditRecord>,
    kinds: &[AuditKindArg],
    cutoff_ms: Option<u64>,
    limit: usize,
) -> Vec<AuditRecord> {
    if !kinds.is_empty() {
        records.retain(|r| kinds.iter().any(|k| k.matches(r.kind)));
    }
    if let Some(cutoff) = cutoff_ms {
        records.retain(|r| r.id.timestamp_ms() >= cutoff);
    }
    // Newest-first: the user reading "history" almost always wants the
    // most recent op at the top. The reader returns file order which is
    // append order which is chronological; flip here.
    records.reverse();
    if limit > 0 && records.len() > limit {
        records.truncate(limit);
    }
    records
}

/// Render one audit record as a single human-readable line. Columns:
///   `<ULID base32>  <ts ISO Z>  <kind>  <leaf>  <scope arrow>  <rule>`
///
/// Tab-separated rather than padded columns: a downstream `awk` /
/// `cut` pipeline beats fragile alignment when rules contain spaces.
/// The History UI computes the rule string the same way (diff of
/// before/after snapshots); duplicating the logic here rather than
/// extracting to a shared helper keeps the lib's wire surface
/// uncluttered while phases 3-5 are in flux.
fn format_history_line(rec: &AuditRecord) -> String {
    let ts = format_iso_utc(rec.id.timestamp_ms());
    let verb = format_verb(rec);
    let arrow = format_scope_arrow(rec);
    let rule = extract_rule_summary(rec).unwrap_or_default();
    format!("{}\t{}\t{}\t{}\t{}", rec.id, ts, verb, arrow, rule)
        .trim_end()
        .to_string()
}

fn format_verb(rec: &AuditRecord) -> String {
    if matches!(rec.kind, audit::Kind::ChangeKind) {
        return match rec.to_kind {
            Some(k) => format!("change-kind→{}", permission_kind_str(k)),
            None => "change-kind".to_string(),
        };
    }
    if matches!(rec.kind, audit::Kind::Restore) {
        // A restore meta-entry: name the direction. The payload is in
        // `rec.restore`; a record missing it is malformed but shouldn't
        // panic the history printer.
        return match rec.restore.as_ref().map(|r| r.direction) {
            Some(audit::RestoreDirection::Undo) => "undo".to_string(),
            Some(audit::RestoreDirection::Redo) => "redo".to_string(),
            Some(audit::RestoreDirection::ToPoint) => "restore-to-point".to_string(),
            None => "restore".to_string(),
        };
    }
    let noun = match rec.leaf_kind {
        audit::LeafKind::PermissionRule => "rule",
        audit::LeafKind::PermissionList => "list",
        audit::LeafKind::TopLevelKey => "key",
    };
    let verb = match rec.kind {
        audit::Kind::Move => "move",
        audit::Kind::Add => "add",
        audit::Kind::Delete => "delete",
        audit::Kind::ChangeKind | audit::Kind::Restore => unreachable!("handled above"),
    };
    format!("{verb}-{noun}")
}

fn permission_kind_str(k: PermissionKind) -> &'static str {
    match k {
        PermissionKind::Allow => "allow",
        PermissionKind::Deny => "deny",
        PermissionKind::Ask => "ask",
    }
}

fn format_scope_arrow(rec: &AuditRecord) -> String {
    let from = rec.from.as_ref().map(|s| s.scope.label());
    let to = rec.to.as_ref().map(|s| s.scope.label());
    match (from, to) {
        (Some(f), Some(t)) if f != t => format!("{f}→{t}"),
        (Some(f), _) => f.to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => "-".to_string(),
    }
}

/// Recover the rule string the op acted on by diffing the appropriate
/// snapshot side. Mirrors the History UI's extractor — see #123 for the
/// equivalent in TypeScript.
fn extract_rule_summary(rec: &AuditRecord) -> Option<String> {
    if matches!(rec.leaf_kind, audit::LeafKind::TopLevelKey) {
        if let Some(PathSeg::Key(k)) = rec.path.first() {
            return Some(k.clone());
        }
        return None;
    }
    if matches!(rec.leaf_kind, audit::LeafKind::PermissionList) {
        if let Some(PathSeg::Key(k)) = rec.path.get(1) {
            return Some(format!("permissions.{k}"));
        }
        return None;
    }
    // PermissionRule: diff the snapshots on the side that gained or
    // lost the rule. Add gains on `to`; move / delete / change-kind lose
    // on `from`.
    let kind_key = match rec.path.get(1) {
        Some(PathSeg::Key(k)) => k.as_str(),
        _ => return None,
    };
    match rec.kind {
        audit::Kind::Add => {
            let side = rec.to.as_ref()?;
            let after = rules_for_kind(&side.key_after, kind_key);
            let before = rules_for_kind(&side.key_before, kind_key);
            first_unique_in(&after, &before)
        }
        _ => {
            let side = rec.from.as_ref()?;
            let before = rules_for_kind(&side.key_before, kind_key);
            let after = rules_for_kind(&side.key_after, kind_key);
            first_unique_in(&before, &after)
        }
    }
}

fn rules_for_kind(snapshot: &Option<serde_json::Value>, kind_key: &str) -> Vec<String> {
    let Some(Value::Object(obj)) = snapshot.as_ref() else {
        return Vec::new();
    };
    let Some(Value::Array(arr)) = obj.get(kind_key) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn first_unique_in(a: &[String], b: &[String]) -> Option<String> {
    let set: std::collections::HashSet<&String> = b.iter().collect();
    a.iter().find(|v| !set.contains(*v)).cloned()
}

/// Format a millis-since-epoch as an ISO-8601 UTC string. Hand-rolled
/// rather than pulling in `chrono` / `time` for a single use site — the
/// audit module avoids those deps for the same reason, and the format
/// here matches the History UI's `<time datetime=…>` attribute.
fn format_iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = (ms % 1000) as u32;
    // Days since 1970-01-01.
    let mut days = (secs / 86_400) as i64;
    let secs_today = secs % 86_400;
    let hour = (secs_today / 3600) as u32;
    let min = ((secs_today % 3600) / 60) as u32;
    let sec = (secs_today % 60) as u32;
    // Civil-from-days (Howard Hinnant's algorithm). Standard, branch-free
    // beyond the leap-year arithmetic — exact for any positive day count
    // far past any realistic ULID timestamp.
    days += 719_468;
    let era = if days >= 0 {
        days / 146_097
    } else {
        (days - 146_096) / 146_097
    };
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = y + if m <= 2 { 1 } else { 0 };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
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

    // -- history subcommand (#126) -----------------------------------------

    use claude_scope_lib::audit::{
        self as audit_lib, Actor, Kind, LeafKind, Record, RestoreDirection, RestoreMeta, Side,
    };

    fn make_audit_record(kind: Kind, leaf_kind: LeafKind) -> Record {
        Record::new(kind, leaf_kind, Actor::Gui, None, None, None, vec![], None)
    }

    #[test]
    fn parse_duration_handles_each_unit() {
        assert_eq!(parse_duration_ms("500ms").unwrap(), 500);
        assert_eq!(parse_duration_ms("5s").unwrap(), 5_000);
        assert_eq!(parse_duration_ms("2m").unwrap(), 120_000);
        assert_eq!(parse_duration_ms("3h").unwrap(), 10_800_000);
        assert_eq!(parse_duration_ms("1d").unwrap(), 86_400_000);
    }

    #[test]
    fn parse_duration_rejects_bare_number_and_garbage() {
        // No suffix: ambiguous between seconds and minutes. Rejecting is
        // safer than silently picking a unit.
        assert!(parse_duration_ms("5").is_err());
        assert!(parse_duration_ms("").is_err());
        assert!(parse_duration_ms("forever").is_err());
        // "ms" suffix is real; "ks" / "y" are not, and the parser should
        // refuse rather than guess.
        assert!(parse_duration_ms("3y").is_err());
    }

    #[test]
    fn filter_records_applies_kind_filter() {
        let records = vec![
            make_audit_record(Kind::Move, LeafKind::PermissionRule),
            make_audit_record(Kind::Add, LeafKind::PermissionRule),
            make_audit_record(Kind::Delete, LeafKind::PermissionRule),
        ];
        let only_adds = filter_records(records.clone(), &[AuditKindArg::Add], None, 0);
        assert_eq!(only_adds.len(), 1);
        assert!(matches!(only_adds[0].kind, Kind::Add));

        // Multiple --kind flags widen the filter (OR semantics).
        let adds_and_deletes =
            filter_records(records, &[AuditKindArg::Add, AuditKindArg::Delete], None, 0);
        assert_eq!(adds_and_deletes.len(), 2);
    }

    #[test]
    fn filter_records_applies_limit_and_orders_newest_first() {
        // Three records with strictly-increasing ULIDs (sleep is the only
        // portable way to guarantee ULID monotonicity across `Ulid::new()`
        // calls without reaching into the crate's internals).
        let a = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = make_audit_record(Kind::Move, LeafKind::PermissionRule);

        let filtered = filter_records(vec![a.clone(), b.clone(), c.clone()], &[], None, 2);
        assert_eq!(filtered.len(), 2);
        // Newest-first ordering: c (newest) then b.
        assert_eq!(filtered[0].id, c.id);
        assert_eq!(filtered[1].id, b.id);
    }

    #[test]
    fn filter_records_limit_zero_means_unlimited() {
        let records: Vec<Record> = (0..30)
            .map(|_| make_audit_record(Kind::Move, LeafKind::PermissionRule))
            .collect();
        let filtered = filter_records(records, &[], None, 0);
        assert_eq!(filtered.len(), 30);
    }

    #[test]
    fn filter_records_applies_since_cutoff() {
        // Record older than the cutoff drops; newer record survives.
        let old = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let new = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        let cutoff = new.id.timestamp_ms();
        let filtered = filter_records(vec![old, new.clone()], &[], Some(cutoff), 0);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, new.id);
    }

    fn perms_with_allow(rules: &[&str]) -> serde_json::Value {
        json!({
            "allow": rules.iter().map(|r| json!(r)).collect::<Vec<_>>(),
        })
    }

    fn side_with(scope: Scope, before: serde_json::Value, after: serde_json::Value) -> Side {
        Side {
            scope,
            file_path: PathBuf::from(format!("/fake/{}/.claude/settings.json", scope.label())),
            top_level_key: "permissions".to_string(),
            key_before: Some(before),
            key_after: Some(after),
        }
    }

    #[test]
    fn extract_rule_summary_recovers_moved_rule_from_source_diff() {
        let mut rec = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        rec.path = vec![
            PathSeg::Key("permissions".into()),
            PathSeg::Key("allow".into()),
            PathSeg::Index(0),
        ];
        rec.from = Some(side_with(
            Scope::Project,
            perms_with_allow(&["Bash(ls)", "Read(*)"]),
            perms_with_allow(&["Read(*)"]),
        ));
        rec.to = Some(side_with(
            Scope::User,
            perms_with_allow(&[]),
            perms_with_allow(&["Bash(ls)"]),
        ));
        assert_eq!(extract_rule_summary(&rec).as_deref(), Some("Bash(ls)"));
    }

    #[test]
    fn extract_rule_summary_recovers_added_rule_from_destination_diff() {
        let mut rec = make_audit_record(Kind::Add, LeafKind::PermissionRule);
        rec.path = vec![
            PathSeg::Key("permissions".into()),
            PathSeg::Key("deny".into()),
            PathSeg::Index(0),
        ];
        rec.to = Some(Side {
            scope: Scope::User,
            file_path: PathBuf::from("/fake/user.json"),
            top_level_key: "permissions".to_string(),
            key_before: Some(json!({"deny": []})),
            key_after: Some(json!({"deny": ["WebFetch(domain:evil.example)"]})),
        });
        assert_eq!(
            extract_rule_summary(&rec).as_deref(),
            Some("WebFetch(domain:evil.example)"),
        );
    }

    #[test]
    fn extract_rule_summary_uses_path_for_top_level_key_ops() {
        let mut rec = make_audit_record(Kind::Move, LeafKind::TopLevelKey);
        rec.path = vec![PathSeg::Key("env".into())];
        assert_eq!(extract_rule_summary(&rec).as_deref(), Some("env"));
    }

    #[test]
    fn extract_rule_summary_returns_none_when_snapshots_are_ambiguous() {
        // No `from` side and the kind isn't Add → can't recover the rule.
        // Better to elide than fabricate.
        let mut rec = make_audit_record(Kind::Delete, LeafKind::PermissionRule);
        rec.path = vec![
            PathSeg::Key("permissions".into()),
            PathSeg::Key("allow".into()),
            PathSeg::Index(0),
        ];
        rec.from = None;
        assert_eq!(extract_rule_summary(&rec), None);
    }

    #[test]
    fn format_verb_renders_change_kind_with_destination() {
        let mut rec = make_audit_record(Kind::ChangeKind, LeafKind::PermissionRule);
        rec.to_kind = Some(PermissionKind::Deny);
        assert_eq!(format_verb(&rec), "change-kind→deny");
    }

    #[test]
    fn format_scope_arrow_collapses_single_side_ops() {
        let mut rec = make_audit_record(Kind::Add, LeafKind::PermissionRule);
        rec.to = Some(Side {
            scope: Scope::User,
            file_path: PathBuf::from("/fake/user.json"),
            top_level_key: "permissions".to_string(),
            key_before: None,
            key_after: None,
        });
        assert_eq!(format_scope_arrow(&rec), "user");
    }

    #[test]
    fn format_iso_utc_known_value() {
        // 2026-05-28T00:00:00Z = 1_779_926_400_000 ms since epoch.
        // Cross-checked via the same civil-from-days algorithm
        // (Hinnant's) the formatter implements, plus an offline check
        // against `date -u -r 1779926400` → "Thu May 28 00:00:00 UTC 2026".
        assert_eq!(
            format_iso_utc(1_779_926_400_000),
            "2026-05-28T00:00:00.000Z"
        );
        // Millis show up zero-padded — guards against a future refactor
        // dropping the `:03` width specifier.
        assert_eq!(
            format_iso_utc(1_779_926_400_007),
            "2026-05-28T00:00:00.007Z"
        );
        // Sub-second wraparound: 999 ms doesn't bleed into seconds.
        assert_eq!(
            format_iso_utc(1_779_926_400_999),
            "2026-05-28T00:00:00.999Z"
        );
    }

    #[test]
    fn format_history_line_includes_all_columns() {
        let mut rec = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        rec.path = vec![
            PathSeg::Key("permissions".into()),
            PathSeg::Key("allow".into()),
            PathSeg::Index(0),
        ];
        rec.from = Some(side_with(
            Scope::Project,
            perms_with_allow(&["Bash(ls)"]),
            perms_with_allow(&[]),
        ));
        rec.to = Some(side_with(
            Scope::User,
            perms_with_allow(&[]),
            perms_with_allow(&["Bash(ls)"]),
        ));
        let line = format_history_line(&rec);
        // Tab-separated; carries the ULID, ISO timestamp, verb, arrow,
        // and recovered rule string.
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 5);
        assert_eq!(cols[0].len(), 26); // ULID base32 width.
        assert!(cols[1].ends_with('Z'));
        assert_eq!(cols[2], "move-rule");
        assert_eq!(cols[3], "project→user");
        assert_eq!(cols[4], "Bash(ls)");
    }

    /// Integration: write three audit records to a tempdir-overridden
    /// home, run cmd_history's filter+order logic, verify JSON shape
    /// matches the GUI's `AuditLogPage` wire format.
    #[test]
    fn history_json_shape_matches_audit_log_page() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let a = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = make_audit_record(Kind::Add, LeafKind::PermissionRule);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = make_audit_record(Kind::Delete, LeafKind::PermissionRule);
        audit_lib::append(&a, Some(home)).unwrap();
        audit_lib::append(&b, Some(home)).unwrap();
        audit_lib::append(&c, Some(home)).unwrap();

        let (records, skipped) = audit_lib::read_all(Some(home)).unwrap();
        assert_eq!(skipped, 0);
        let filtered = filter_records(records, &[], None, 0);
        let views: Vec<AuditRecordView> = filtered
            .into_iter()
            .map(|r| {
                let ts_ms = r.id.timestamp_ms();
                AuditRecordView { record: r, ts_ms }
            })
            .collect();
        let page = AuditLogPage {
            records: views,
            skipped,
        };
        let json: serde_json::Value = serde_json::to_value(&page).unwrap();
        // `AuditLogPage` shape: top-level `records` array + `skipped` count.
        // Each record carries `ts_ms` flattened alongside the audit fields
        // (no nested `record` key).
        assert!(json.get("records").and_then(|v| v.as_array()).is_some());
        assert_eq!(json.get("skipped").and_then(|v| v.as_u64()), Some(0));
        let first = &json["records"][0];
        assert!(first.get("ts_ms").is_some());
        assert!(first.get("kind").is_some());
        assert!(first.get("record").is_none());
        // Newest-first: filter_records reversed file order.
        assert_eq!(json["records"].as_array().unwrap().len(), 3);
        // Cross-check that the GUI's IPC would produce the same shape —
        // both consume `audit::Record` via `AuditRecordView`, so this
        // assertion is a forward-compat anchor.
    }

    // -- undo / redo / restore subcommands (#126) --------------------------

    /// Write a settings file, creating parent dirs.
    fn write_file(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A logged move of `Bash(ls)` from `project` to `user`, with
    /// before/after snapshots — the shape `cmd_undo` reads.
    fn logged_move(project: &Path, user: &Path) -> Record {
        let mut rec = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        rec.path = vec![
            PathSeg::Key("permissions".into()),
            PathSeg::Key("allow".into()),
            PathSeg::Index(0),
        ];
        rec.from = Some(Side {
            scope: Scope::Project,
            file_path: project.to_path_buf(),
            top_level_key: "permissions".into(),
            key_before: Some(json!({"allow": ["Bash(ls)"]})),
            key_after: Some(json!({"allow": []})),
        });
        rec.to = Some(Side {
            scope: Scope::User,
            file_path: user.to_path_buf(),
            top_level_key: "permissions".into(),
            key_before: Some(json!({"allow": []})),
            key_after: Some(json!({"allow": ["Bash(ls)"]})),
        });
        rec
    }

    #[test]
    fn cli_undo_reverts_the_last_logged_move_and_logs_a_cli_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let project = tmp.path().join("proj/.claude/settings.json");
        let user = home.join(".claude/settings.json");
        // Files at their post-move state: the rule has moved Project → User.
        write_file(&project, r#"{"permissions":{"allow":[]}}"#);
        write_file(&user, r#"{"permissions":{"allow":["Bash(ls)"]}}"#);
        audit_lib::append(&logged_move(&project, &user), Some(home)).unwrap();

        // `--yes` skips the prompt; not a dry run.
        cmd_undo(Some(home), false, true, false).unwrap();

        // Both files are back to the pre-move state.
        assert_eq!(
            rules_at(&io_atomic::load(&project).unwrap().unwrap(), "allow"),
            vec!["Bash(ls)".to_string()],
        );
        assert!(rules_at(&io_atomic::load(&user).unwrap().unwrap(), "allow").is_empty());
        // A `restore` entry was logged with actor `cli`, so a GUI session
        // sees the CLI-driven undo in its History.
        let (records, _) = audit_lib::read_all(Some(home)).unwrap();
        assert_eq!(records.len(), 2);
        assert!(matches!(records[1].kind, Kind::Restore));
        assert!(matches!(records[1].actor, Actor::Cli));
    }

    #[test]
    fn cli_undo_dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let project = tmp.path().join("proj/.claude/settings.json");
        let user = home.join(".claude/settings.json");
        write_file(&project, r#"{"permissions":{"allow":[]}}"#);
        write_file(&user, r#"{"permissions":{"allow":["Bash(ls)"]}}"#);
        audit_lib::append(&logged_move(&project, &user), Some(home)).unwrap();

        cmd_undo(Some(home), true, true, false).unwrap();

        // Files untouched, no restore entry appended.
        assert!(rules_at(&io_atomic::load(&project).unwrap().unwrap(), "allow").is_empty());
        let (records, _) = audit_lib::read_all(Some(home)).unwrap();
        assert_eq!(records.len(), 1, "a dry run must not append a restore entry");
    }

    #[test]
    fn cli_undo_errors_on_an_empty_log() {
        let tmp = tempfile::tempdir().unwrap();
        let err = cmd_undo(Some(tmp.path()), false, true, false).unwrap_err();
        assert!(err.to_string().contains("nothing to undo"));
    }

    #[test]
    fn cli_redo_errors_after_a_sequence_break() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let project = tmp.path().join("proj/.claude/settings.json");
        let user = home.join(".claude/settings.json");
        write_file(&project, r#"{"permissions":{"allow":[]}}"#);
        write_file(&user, r#"{"permissions":{"allow":["Bash(ls)"]}}"#);
        // move → undo(move) → another move = a sequence break.
        let m1 = logged_move(&project, &user);
        let undo = Record::new_restore(
            LeafKind::PermissionRule,
            Actor::Gui,
            None,
            vec![],
            RestoreMeta {
                target_id: m1.id,
                direction: RestoreDirection::Undo,
                files: vec![],
            },
        );
        let m2 = logged_move(&project, &user);
        for rec in [&m1, &undo, &m2] {
            audit_lib::append(rec, Some(home)).unwrap();
        }
        let err = cmd_redo(Some(home), false, true, false).unwrap_err();
        assert!(err.to_string().contains("redo is unavailable"));
    }

    #[test]
    fn resolve_entry_id_matches_a_full_ulid_and_rejects_misses() {
        let a = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        let records = vec![a.clone()];
        // Full id resolves, case-insensitively.
        assert_eq!(resolve_entry_id(&records, &a.id.to_string()).unwrap(), a.id);
        assert_eq!(
            resolve_entry_id(&records, &a.id.to_string().to_lowercase()).unwrap(),
            a.id,
        );
        // No 2026-era ULID begins with `Z`, so this matches nothing.
        assert!(resolve_entry_id(&records, "ZZZZZZ")
            .unwrap_err()
            .to_string()
            .contains("no audit entry"));
        // Whitespace-only is rejected up front.
        assert!(resolve_entry_id(&records, "  ")
            .unwrap_err()
            .to_string()
            .contains("empty"));
    }

    #[test]
    fn resolve_entry_id_reports_an_ambiguous_prefix() {
        let a = make_audit_record(Kind::Move, LeafKind::PermissionRule);
        let b = make_audit_record(Kind::Add, LeafKind::PermissionRule);
        let records = vec![a.clone(), b];
        // The leading base32 char encodes the top 5 bits of the 48-bit ms
        // timestamp — identical for any two ULIDs minted this century — so a
        // one-char prefix matches both records.
        let one_char = &a.id.to_string()[..1];
        let err = resolve_entry_id(&records, one_char).unwrap_err();
        assert!(err.to_string().contains("ambiguous"));
    }
}
