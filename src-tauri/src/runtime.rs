//! Launch-time runtime overrides for sandbox / scratch-home mode (#66).
//!
//! These overrides redirect scope discovery away from the real user home (and
//! optionally the cwd-derived project root) so the app can be exercised
//! against a throwaway directory without putting the user's actual
//! `~/.claude/` at risk.
//!
//! Resolution order, highest priority first:
//!   1. `--home <path>` / `--project <path>` CLI flags
//!   2. `CLAUDE_SCOPE_HOME` / `CLAUDE_SCOPE_PROJECT` env vars
//!   3. Real `dirs::home_dir()` and the front-end's project picker
//!
//! Both overrides survive the lifetime of the process and are exposed to the
//! front-end via `load_runtime_info` so the UI can render a persistent banner
//! under the toolbar warning the user that they're in scratch mode.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Default, Clone)]
pub struct RuntimeOverrides {
    pub home: Option<PathBuf>,
    pub project: Option<PathBuf>,
}

impl RuntimeOverrides {
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub fn project(&self) -> Option<&Path> {
        self.project.as_deref()
    }

    /// Build from process argv + env. Unknown args are ignored so Tauri / the
    /// platform launcher can still pass their own flags through. Argv-supplied
    /// values win over env-var values; env vars win over real home / cwd.
    pub fn from_env_and_args<I, S>(args: I, env: &dyn EnvSource) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut overrides = Self::default();
        let mut iter = args.into_iter().map(Into::into).peekable();
        // Skip argv[0] (the executable path). Tests still include a stub
        // first element so this drop matches production behavior. Use
        // `.next()` discard instead of unwrap so an empty argv from a
        // malformed launcher doesn't panic the app.
        let _ = iter.next();
        // Consume the next arg as the value for `flag` only when it's a real
        // value (non-empty, doesn't itself start with `--`). Without the
        // flag-shape guard, `--home --project /p` would set home="--project"
        // and silently drop the real project override; without the empty-
        // string guard, `--home=` would set home to PathBuf("") instead of
        // matching the env-var convention where empty means unset.
        fn take_value<I: Iterator<Item = String>>(
            iter: &mut std::iter::Peekable<I>,
        ) -> Option<String> {
            match iter.peek() {
                Some(v) if !v.is_empty() && !v.starts_with("--") => iter.next(),
                _ => None,
            }
        }
        while let Some(arg) = iter.next() {
            if let Some(rest) = arg.strip_prefix("--home=") {
                if !rest.is_empty() {
                    overrides.home = Some(PathBuf::from(rest));
                }
            } else if arg == "--home" {
                if let Some(value) = take_value(&mut iter) {
                    overrides.home = Some(PathBuf::from(value));
                }
            } else if let Some(rest) = arg.strip_prefix("--project=") {
                if !rest.is_empty() {
                    overrides.project = Some(PathBuf::from(rest));
                }
            } else if arg == "--project" {
                if let Some(value) = take_value(&mut iter) {
                    overrides.project = Some(PathBuf::from(value));
                }
            }
            // Unknown args fall through silently. Tauri / WebView2 / OS
            // launchers occasionally pass diagnostic flags we don't care about.
        }
        if overrides.home.is_none() {
            if let Some(v) = env.get("CLAUDE_SCOPE_HOME") {
                if !v.is_empty() {
                    overrides.home = Some(PathBuf::from(v));
                }
            }
        }
        if overrides.project.is_none() {
            if let Some(v) = env.get("CLAUDE_SCOPE_PROJECT") {
                if !v.is_empty() {
                    overrides.project = Some(PathBuf::from(v));
                }
            }
        }
        overrides
    }
}

/// Tiny indirection so the parser is unit-testable without poking
/// `std::env::var`. Production passes a `RealEnv`; tests pass a fake map.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct RealEnv;

impl EnvSource for RealEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Snapshot exposed to the front-end. Paths are stringified for IPC; both
/// fields are `None` when the user is running unsandboxed, so the sandbox
/// banner is omitted in that case.
#[derive(Debug, Default, Serialize)]
pub struct RuntimeInfo {
    pub home_override: Option<String>,
    pub project_override: Option<String>,
}

impl RuntimeInfo {
    pub fn from_overrides(overrides: &RuntimeOverrides) -> Self {
        Self {
            home_override: overrides.home().map(|p| p.display().to_string()),
            project_override: overrides.project().map(|p| p.display().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv(HashMap<String, String>);
    impl EnvSource for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }
    fn empty_env() -> FakeEnv {
        FakeEnv(HashMap::new())
    }

    #[test]
    fn parses_separate_home_and_project_flags() {
        let args = ["claude-scope", "--home", "/tmp/h", "--project", "/tmp/p"];
        let got = RuntimeOverrides::from_env_and_args(args, &empty_env());
        assert_eq!(got.home(), Some(Path::new("/tmp/h")));
        assert_eq!(got.project(), Some(Path::new("/tmp/p")));
    }

    #[test]
    fn parses_equals_form() {
        let args = ["claude-scope", "--home=/tmp/h", "--project=/tmp/p"];
        let got = RuntimeOverrides::from_env_and_args(args, &empty_env());
        assert_eq!(got.home(), Some(Path::new("/tmp/h")));
        assert_eq!(got.project(), Some(Path::new("/tmp/p")));
    }

    #[test]
    fn cli_wins_over_env() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_SCOPE_HOME".to_string(), "/env/h".to_string());
        env.insert("CLAUDE_SCOPE_PROJECT".to_string(), "/env/p".to_string());
        let args = ["claude-scope", "--home", "/cli/h"];
        let got = RuntimeOverrides::from_env_and_args(args, &FakeEnv(env));
        assert_eq!(got.home(), Some(Path::new("/cli/h")));
        // project came from env since CLI didn't override it.
        assert_eq!(got.project(), Some(Path::new("/env/p")));
    }

    #[test]
    fn ignores_unknown_args() {
        // Tauri / WebView2 occasionally inject their own flags. Ignoring them
        // (instead of erroring) keeps the launch resilient.
        let args = ["claude-scope", "--webview-arg=foo", "--home", "/h"];
        let got = RuntimeOverrides::from_env_and_args(args, &empty_env());
        assert_eq!(got.home(), Some(Path::new("/h")));
    }

    #[test]
    fn empty_env_value_is_treated_as_unset() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_SCOPE_HOME".to_string(), String::new());
        let got = RuntimeOverrides::from_env_and_args(["claude-scope"], &FakeEnv(env));
        assert!(got.home().is_none());
    }

    #[test]
    fn missing_value_for_trailing_flag_is_dropped() {
        // `--home` at end of argv with no value should leave home unset
        // rather than panic on a missing iterator next().
        let got = RuntimeOverrides::from_env_and_args(["claude-scope", "--home"], &empty_env());
        assert!(got.home().is_none());
    }

    #[test]
    fn flag_followed_by_another_flag_does_not_consume_it() {
        // Regression: `--home --project /p` previously set home="--project"
        // and silently dropped the real project flag. Now `--home` finds no
        // value (next token is flag-shaped), home stays unset, and parsing
        // continues so `--project /p` is honored.
        let args = ["claude-scope", "--home", "--project", "/p"];
        let got = RuntimeOverrides::from_env_and_args(args, &empty_env());
        assert!(got.home().is_none(), "home should not be set to --project");
        assert_eq!(got.project(), Some(Path::new("/p")));
    }

    #[test]
    fn empty_equals_form_is_treated_as_unset() {
        // Match the env-var convention: an explicitly empty value means
        // "no override" rather than "override with the empty path".
        let args = ["claude-scope", "--home=", "--project="];
        let got = RuntimeOverrides::from_env_and_args(args, &empty_env());
        assert!(got.home().is_none());
        assert!(got.project().is_none());
    }

    #[test]
    fn empty_separated_value_is_treated_as_unset() {
        // `--home ""` (empty string passed as the next arg) — same convention
        // as the equals form. Drops home rather than path-of-empty-string.
        let args = ["claude-scope", "--home", "", "--project", "/p"];
        let got = RuntimeOverrides::from_env_and_args(args, &empty_env());
        assert!(got.home().is_none());
        assert_eq!(got.project(), Some(Path::new("/p")));
    }

    #[test]
    fn runtime_info_serializes_present_overrides() {
        let overrides = RuntimeOverrides {
            home: Some(PathBuf::from("/h")),
            project: None,
        };
        let info = RuntimeInfo::from_overrides(&overrides);
        assert_eq!(info.home_override.as_deref(), Some("/h"));
        assert!(info.project_override.is_none());
    }
}
