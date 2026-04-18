//! Tauri command handlers exposed to the front-end.

use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::io_atomic::{self, BackupTracker};
use crate::model::{PermissionKind, PermissionRules, SettingsDoc};
use crate::scope::{self, Scope, ScopePaths};

/// Process-global backup tracker so we only write one `.bak` per file per
/// session, regardless of which command triggered the first write.
static BACKUPS: OnceLock<BackupTracker> = OnceLock::new();

fn backups() -> &'static BackupTracker {
    BACKUPS.get_or_init(BackupTracker::new)
}

#[derive(Debug, Serialize)]
pub struct ScopeView {
    pub scope: Scope,
    pub path: Option<String>,
    pub exists: bool,
    pub permissions: PermissionRules,
    pub other_keys: Vec<String>,
    pub parse_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LoadedScopes {
    pub project_dir: String,
    pub scopes: Vec<ScopeView>,
    pub effective_permissions: PermissionRules,
}

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub rule: String,
    pub kind: PermissionKind,
    pub from: Scope,
    pub to: Scope,
}

#[tauri::command]
pub fn load_scopes(project_dir: Option<String>) -> Result<LoadedScopes, String> {
    let start = project_dir.as_ref().map(Path::new);
    let paths = scope::resolve(start).map_err(|e| e.to_string())?;
    build_loaded(&paths).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn diff_move(req: MoveRequest, _state: State<'_, ()>) -> Result<String, String> {
    let paths = scope::resolve(None).map_err(|e| e.to_string())?;
    diff_move_impl(&paths, &req).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn apply_move(req: MoveRequest, _state: State<'_, ()>) -> Result<(), String> {
    let paths = scope::resolve(None).map_err(|e| e.to_string())?;
    apply_move_impl(&paths, &req, backups()).map_err(|e| e.to_string())
}

fn build_loaded(paths: &ScopePaths) -> Result<LoadedScopes, Box<dyn std::error::Error>> {
    let mut views = Vec::with_capacity(3);
    let mut docs: [Option<SettingsDoc>; 3] = Default::default();

    for (idx, scope) in Scope::ALL.iter().copied().enumerate() {
        let path = paths.path_for(scope);
        let (exists, perms, other, err, doc) = match path {
            Some(p) => match io_atomic::load(p) {
                Ok(Some(doc)) => (
                    true,
                    doc.permissions(),
                    doc.other_keys(),
                    None,
                    Some(doc),
                ),
                Ok(None) => (false, PermissionRules::default(), vec![], None, None),
                Err(e) => (
                    p.exists(),
                    PermissionRules::default(),
                    vec![],
                    Some(e.to_string()),
                    None,
                ),
            },
            None => (false, PermissionRules::default(), vec![], None, None),
        };
        views.push(ScopeView {
            scope,
            path: path.map(|p| p.display().to_string()),
            exists,
            permissions: perms,
            other_keys: other,
            parse_error: err,
        });
        docs[idx] = doc;
    }

    let effective = effective_permissions(&views);

    Ok(LoadedScopes {
        project_dir: paths.project_dir.display().to_string(),
        scopes: views,
        effective_permissions: effective,
    })
}

/// Build the effective permission view. For v1 we union `allow` / `deny` /
/// `ask` across the three recognized scopes and deduplicate while preserving
/// the order Local → Project → User (highest precedence first). That's a
/// faithful first approximation of "what Claude Code sees" for list-valued
/// rule sets; precedence-sensitive semantics like conflict resolution are a
/// future concern.
fn effective_permissions(views: &[ScopeView]) -> PermissionRules {
    let mut out = PermissionRules::default();
    for view in views {
        for rule in &view.permissions.allow {
            if !out.allow.iter().any(|r| r == rule) {
                out.allow.push(rule.clone());
            }
        }
        for rule in &view.permissions.deny {
            if !out.deny.iter().any(|r| r == rule) {
                out.deny.push(rule.clone());
            }
        }
        for rule in &view.permissions.ask {
            if !out.ask.iter().any(|r| r == rule) {
                out.ask.push(rule.clone());
            }
        }
    }
    out
}

fn diff_move_impl(
    paths: &ScopePaths,
    req: &MoveRequest,
) -> Result<String, Box<dyn std::error::Error>> {
    if req.from == req.to {
        return Err("source and destination scopes must differ".into());
    }
    let from_path = require_path(paths, req.from)?;
    let to_path = require_path(paths, req.to)?;

    let mut out = String::new();
    out.push_str(&format!(
        "Move {kind} rule `{rule}`\n  from {from_label}: {from}\n    to {to_label}: {to}\n",
        kind = req.kind.key(),
        rule = req.rule,
        from_label = req.from.label(),
        from = from_path.display(),
        to_label = req.to.label(),
        to = to_path.display(),
    ));

    if to_path.exists() {
        let existing = io_atomic::load(to_path)?.unwrap_or_else(SettingsDoc::empty);
        if existing.permissions().contains(req.kind, &req.rule) {
            out.push_str("(destination already has this rule; source copy will simply be removed)\n");
        }
    } else {
        out.push_str("(destination file will be created)\n");
    }
    Ok(out)
}

fn apply_move_impl(
    paths: &ScopePaths,
    req: &MoveRequest,
    backups: &BackupTracker,
) -> Result<(), Box<dyn std::error::Error>> {
    if req.from == req.to {
        return Err("source and destination scopes must differ".into());
    }
    let from_path = require_path(paths, req.from)?.to_path_buf();
    let to_path = require_path(paths, req.to)?.to_path_buf();

    // Load destination first — creating it in-memory if missing — so we can
    // fail early before mutating the source.
    let mut to_doc = io_atomic::load(&to_path)?.unwrap_or_else(SettingsDoc::empty);
    to_doc.add_rule(req.kind, &req.rule);

    let mut from_doc = match io_atomic::load(&from_path)? {
        Some(d) => d,
        None => return Err(format!("source file {} does not exist", from_path.display()).into()),
    };
    let removed = from_doc.remove_rule(req.kind, &req.rule);
    if !removed {
        return Err(format!(
            "rule `{}` not found in {} {}",
            req.rule,
            req.from.label(),
            from_path.display()
        )
        .into());
    }

    // Destination first: if the source write fails after adding to the dest,
    // the rule still exists in exactly one place (dest), which is safer than
    // losing it entirely.
    io_atomic::save(&to_path, &to_doc, backups)?;
    io_atomic::save(&from_path, &from_doc, backups)?;
    Ok(())
}

fn require_path(paths: &ScopePaths, scope: Scope) -> Result<&Path, Box<dyn std::error::Error>> {
    paths.path_for(scope).ok_or_else(|| {
        format!(
            "cannot resolve path for scope `{}` (home directory missing?)",
            scope.label()
        )
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(path: &PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn paths_in(tmp: &std::path::Path) -> ScopePaths {
        let project = tmp.join("project");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        let user_home = tmp.join("home");
        std::fs::create_dir_all(user_home.join(".claude")).unwrap();
        ScopePaths {
            project_dir: project.clone(),
            local: Some(project.join(".claude").join("settings.local.json")),
            project: Some(project.join(".claude").join("settings.json")),
            user: Some(user_home.join(".claude").join("settings.json")),
        }
    }

    #[test]
    fn move_from_project_to_user_preserves_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{
  "theme": "dark",
  "permissions": { "allow": ["Bash(git status)", "Read(**)"] }
}"#,
        );

        let backups = BackupTracker::new();
        apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(git status)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
            &backups,
        )
        .unwrap();

        let project_doc = io_atomic::load(paths.project.as_ref().unwrap()).unwrap().unwrap();
        assert_eq!(project_doc.permissions().allow, vec!["Read(**)".to_string()]);
        assert!(project_doc.top_level_keys().iter().any(|k| k == "theme"));

        let user_doc = io_atomic::load(paths.user.as_ref().unwrap()).unwrap().unwrap();
        assert_eq!(
            user_doc.permissions().allow,
            vec!["Bash(git status)".to_string()]
        );
    }

    #[test]
    fn move_creates_destination_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(
            paths.project.as_ref().unwrap(),
            r#"{ "permissions": { "deny": ["WebFetch(domain:evil.example)"] } }"#,
        );
        assert!(!paths.local.as_ref().unwrap().exists());

        let backups = BackupTracker::new();
        apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "WebFetch(domain:evil.example)".into(),
                kind: PermissionKind::Deny,
                from: Scope::Project,
                to: Scope::Local,
            },
            &backups,
        )
        .unwrap();

        let local_doc = io_atomic::load(paths.local.as_ref().unwrap()).unwrap().unwrap();
        assert_eq!(
            local_doc.permissions().deny,
            vec!["WebFetch(domain:evil.example)".to_string()]
        );
        let project_doc = io_atomic::load(paths.project.as_ref().unwrap()).unwrap().unwrap();
        assert!(project_doc.permissions().deny.is_empty());
    }

    #[test]
    fn move_same_scope_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        let err = apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "x".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::Project,
            },
            &BackupTracker::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("must differ"));
    }

    #[test]
    fn move_missing_rule_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_in(tmp.path());
        write(paths.project.as_ref().unwrap(), r#"{"permissions":{"allow":[]}}"#);
        let err = apply_move_impl(
            &paths,
            &MoveRequest {
                rule: "Bash(nope)".into(),
                kind: PermissionKind::Allow,
                from: Scope::Project,
                to: Scope::User,
            },
            &BackupTracker::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn effective_unions_across_scopes() {
        let views = vec![
            ScopeView {
                scope: Scope::Local,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git status)".into()],
                    deny: vec![],
                    ask: vec![],
                },
                other_keys: vec![],
                parse_error: None,
            },
            ScopeView {
                scope: Scope::Project,
                path: None,
                exists: true,
                permissions: PermissionRules {
                    allow: vec!["Bash(git status)".into(), "Read(**)".into()],
                    deny: vec!["WebFetch(domain:evil.example)".into()],
                    ask: vec![],
                },
                other_keys: vec![],
                parse_error: None,
            },
            ScopeView {
                scope: Scope::User,
                path: None,
                exists: false,
                permissions: PermissionRules::default(),
                other_keys: vec![],
                parse_error: None,
            },
        ];
        let eff = effective_permissions(&views);
        assert_eq!(eff.allow, vec!["Bash(git status)", "Read(**)"]);
        assert_eq!(eff.deny, vec!["WebFetch(domain:evil.example)"]);
        assert!(eff.ask.is_empty());
    }
}
