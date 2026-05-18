//! Build- and runtime-time diagnostic block surfaced by the About dialog
//! (#21) and `claude-scope-cli version --verbose`.
//!
//! Most fields are `&'static str`s baked in at compile time, so constructing
//! the struct is essentially free and we don't cache. The webview version
//! is the one runtime field — pulled from `tauri::webview_version()` on the
//! GUI side and absent on the CLI side, since there's no webview there.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    /// `CARGO_PKG_VERSION` — release-please-managed, matches the GitHub
    /// release tag.
    pub version: &'static str,
    /// Short SHA captured by `build.rs`. `None` when the build didn't run
    /// from a git working tree (release tarballs, vendored builds).
    pub git_sha: Option<&'static str>,
    /// `tauri::VERSION` — the Tauri crate version this binary linked
    /// against, useful when triaging webview-shaped bugs.
    pub tauri_version: &'static str,
    /// Result of `tauri::webview_version()` at the time the command ran.
    /// `None` outside a running Tauri context (e.g. the CLI) or when the
    /// platform can't introspect its webview.
    pub webview_version: Option<String>,
    /// MSRV pinned in `Cargo.toml` via `rust-version`. Diagnostic only —
    /// not necessarily the rustc that built this binary. Capturing the
    /// actual rustc version would mean a `build.rs` branch for diminishing
    /// returns; the MSRV is what's committed and what reviewers care about.
    pub rust_version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
}

impl AppInfo {
    /// Build an `AppInfo` with `webview_version` supplied by the caller, so
    /// the GUI command passes `tauri::webview_version()` and the CLI passes
    /// `None`. Decoupling the runtime field from the struct constructor
    /// also means the unit test doesn't need a live Tauri context.
    pub fn build(webview_version: Option<String>) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            git_sha: option_env!("CLAUDE_SCOPE_GIT_SHA"),
            tauri_version: tauri::VERSION,
            webview_version,
            rust_version: env!("CARGO_PKG_RUST_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }

    /// Markdown rendering for the "Copy diagnostics" button and the CLI's
    /// human-readable output. Centralized so the two surfaces can't drift
    /// in formatting — a paste from either ends up identical in the bug
    /// report. Optional fields are emitted as `unknown` so the block has a
    /// stable shape (a reviewer scanning a paste shouldn't have to guess
    /// whether a missing line means "absent" or "the bot rendered it
    /// wrong").
    pub fn to_markdown(&self) -> String {
        let sha = self.git_sha.unwrap_or("unknown");
        let webview = self.webview_version.as_deref().unwrap_or("unknown");
        format!(
            "- ClaudeScope: {version} ({sha})\n\
             - Tauri: {tauri}, WebView: {webview}\n\
             - Rust (MSRV): {rust}\n\
             - OS: {os} {arch}",
            version = self.version,
            sha = sha,
            tauri = self.tauri_version,
            webview = webview,
            rust = self.rust_version,
            os = self.os,
            arch = self.arch,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_matches_compile_time_constants() {
        let info = AppInfo::build(Some("121.0.6167.184".into()));
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.tauri_version, tauri::VERSION);
        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert_eq!(info.webview_version.as_deref(), Some("121.0.6167.184"));
    }

    #[test]
    fn markdown_includes_every_field_with_known_values() {
        let info = AppInfo {
            version: "9.9.9",
            git_sha: Some("abc123def456"),
            tauri_version: "2.0.0",
            webview_version: Some("121.0.6167.184".into()),
            rust_version: "1.88",
            os: "windows",
            arch: "x86_64",
        };
        let md = info.to_markdown();
        assert!(md.contains("ClaudeScope: 9.9.9 (abc123def456)"));
        assert!(md.contains("Tauri: 2.0.0, WebView: 121.0.6167.184"));
        assert!(md.contains("Rust (MSRV): 1.88"));
        assert!(md.contains("OS: windows x86_64"));
    }

    #[test]
    fn markdown_substitutes_unknown_for_missing_optional_fields() {
        let info = AppInfo {
            version: "0.1.0",
            git_sha: None,
            tauri_version: "2.0.0",
            webview_version: None,
            rust_version: "1.88",
            os: "linux",
            arch: "aarch64",
        };
        let md = info.to_markdown();
        // A bug-report paste should still be a complete diagnostic block —
        // omitting lines silently would make the absence ambiguous with a
        // copy-paste truncation.
        assert!(md.contains("(unknown)"));
        assert!(md.contains("WebView: unknown"));
    }
}
