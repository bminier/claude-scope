//! In-memory settings model.
//!
//! The underlying JSON is kept as a `serde_json::Value` (with preserved key
//! order thanks to the `preserve_order` feature of serde_json) so we can
//! round-trip non-permissions keys untouched. Helpers on top of it know how
//! to enumerate and move permission rules.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::io_atomic::Indent;

/// One of the three permission list kinds Claude Code knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionKind {
    Allow,
    Deny,
    Ask,
}

impl PermissionKind {
    pub const ALL: [PermissionKind; 3] = [
        PermissionKind::Allow,
        PermissionKind::Deny,
        PermissionKind::Ask,
    ];

    pub fn key(self) -> &'static str {
        match self {
            PermissionKind::Allow => "allow",
            PermissionKind::Deny => "deny",
            PermissionKind::Ask => "ask",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
}

impl PermissionRules {
    pub fn get(&self, kind: PermissionKind) -> &[String] {
        match kind {
            PermissionKind::Allow => &self.allow,
            PermissionKind::Deny => &self.deny,
            PermissionKind::Ask => &self.ask,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingsDoc {
    root: Value,
    indent: Indent,
}

impl SettingsDoc {
    pub fn empty() -> Self {
        Self {
            root: Value::Object(Map::new()),
            indent: Indent::Spaces(2),
        }
    }

    pub fn from_value(root: Value, indent: Indent) -> Self {
        Self { root, indent }
    }

    /// Top-level non-permission entries (key + value) in the order they were
    /// written on disk. Used by the UI tree-view so it can show what's in
    /// `env`, `hooks`, `theme`, and any future keys — not just their names.
    pub fn other_entries(&self) -> Map<String, Value> {
        let Some(obj) = self.root.as_object() else {
            return Map::new();
        };
        obj.iter()
            .filter(|(k, _)| k.as_str() != "permissions")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Read the permissions block as a `PermissionRules` snapshot. Missing or
    /// malformed shapes produce empty lists rather than failing; a partially
    /// corrupt settings file shouldn't stop the whole UI from loading.
    pub fn permissions(&self) -> PermissionRules {
        let mut out = PermissionRules::default();
        let Some(perms) = self.root.get("permissions").and_then(Value::as_object) else {
            return out;
        };
        for kind in PermissionKind::ALL {
            if let Some(Value::Array(items)) = perms.get(kind.key()) {
                let strs = items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                match kind {
                    PermissionKind::Allow => out.allow = strs,
                    PermissionKind::Deny => out.deny = strs,
                    PermissionKind::Ask => out.ask = strs,
                }
            }
        }
        out
    }

    /// Add a rule to the given list if it isn't already present. Ensures
    /// `permissions.<kind>` exists as an array. Returns true if the document
    /// was actually mutated (i.e. the rule wasn't already present).
    pub fn add_rule(&mut self, kind: PermissionKind, rule: &str) -> bool {
        let obj = ensure_object(&mut self.root);
        let perms_entry = obj
            .entry("permissions".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !perms_entry.is_object() {
            *perms_entry = Value::Object(Map::new());
        }
        let perms = perms_entry.as_object_mut().expect("permissions is object");
        let list_entry = perms
            .entry(kind.key().to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !list_entry.is_array() {
            *list_entry = Value::Array(Vec::new());
        }
        let arr = list_entry.as_array_mut().expect("rule list is array");
        if arr.iter().any(|v| v.as_str() == Some(rule)) {
            return false;
        }
        arr.push(Value::String(rule.to_string()));
        true
    }

    /// Remove every occurrence of `rule` from `permissions.<kind>`. Returns
    /// true if at least one entry was removed.
    pub fn remove_rule(&mut self, kind: PermissionKind, rule: &str) -> bool {
        let Some(perms) = self
            .root
            .get_mut("permissions")
            .and_then(Value::as_object_mut)
        else {
            return false;
        };
        let Some(Value::Array(arr)) = perms.get_mut(kind.key()) else {
            return false;
        };
        let before = arr.len();
        arr.retain(|v| v.as_str() != Some(rule));
        before != arr.len()
    }

    /// Read a single top-level key (any type). Used by the key-move flow to
    /// inspect the raw JSON value before applying a merge.
    pub fn get_top_level(&self, key: &str) -> Option<&Value> {
        self.root.get(key)
    }

    /// Remove a top-level key. Returns true if the key was present.
    pub fn remove_top_level(&mut self, key: &str) -> bool {
        self.root
            .as_object_mut()
            .and_then(|o| o.remove(key))
            .is_some()
    }

    /// Merge `value` into the top-level `key` using the documented Claude
    /// Code semantics for that key (see [`key_policy`]).
    ///
    /// If the key isn't present yet, the value is inserted unchanged. If the
    /// key already exists, the policy decides what happens:
    ///
    /// - [`KeyPolicy::Replace`] / [`KeyPolicy::ReplaceComplex`] /
    ///   [`KeyPolicy::ReplaceUnknown`]: the destination is overwritten.
    /// - [`KeyPolicy::DeepMerge`]: object trees are merged recursively, with
    ///   source values winning on scalar/non-object conflicts.
    /// - [`KeyPolicy::ArrayUnion`]: arrays are concatenated with duplicates
    ///   removed; mismatched shapes fall back to overwrite.
    pub fn merge_top_level(&mut self, key: &str, value: Value) {
        let policy = key_policy(key);
        let obj = ensure_object(&mut self.root);
        match policy {
            KeyPolicy::Replace | KeyPolicy::ReplaceComplex | KeyPolicy::ReplaceUnknown => {
                obj.insert(key.to_string(), value);
            }
            KeyPolicy::DeepMerge => match obj.get_mut(key) {
                Some(existing) => deep_merge_in_place(existing, value),
                None => {
                    obj.insert(key.to_string(), value);
                }
            },
            KeyPolicy::ArrayUnion => match obj.get_mut(key) {
                Some(existing) => array_union_in_place(existing, value),
                None => {
                    obj.insert(key.to_string(), value);
                }
            },
        }
    }

    /// Render to JSON using the detected indentation. We intentionally avoid
    /// serde_json's pretty printer configuration because it doesn't support
    /// tab indents; hand-rolling keeps our options open.
    pub fn render(&self) -> String {
        let mut buf = Vec::with_capacity(256);
        let formatter = IndentFormatter::new(self.indent);
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        use serde::Serialize as _;
        self.root
            .serialize(&mut ser)
            .expect("serde_json serialize to Vec is infallible");
        buf.push(b'\n');
        String::from_utf8(buf).expect("serde_json emits valid utf-8")
    }
}

fn ensure_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("just made it an object")
}

/// How a top-level settings key should be combined when its value is moved
/// from one scope into another that already has the same key. Modelled on
/// how Claude Code itself reads each key across scopes, so that a move in
/// ClaudeScope produces an effective config consistent with the documented
/// semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPolicy {
    /// Override-only key — Claude Code uses the highest-precedence scope's
    /// value verbatim. Moving overwrites the destination's existing value.
    Replace,
    /// `env` — nested object, deep-merged across scopes by Claude Code, with
    /// the higher-precedence scope winning on conflicts. Source wins here
    /// because the move's intent is "make the destination carry this value".
    DeepMerge,
    /// Array-valued key whose entries are concatenated and deduplicated
    /// across scopes (e.g. `allowedHttpHookUrls`).
    ArrayUnion,
    /// Known key with complex per-subkey merge semantics that ClaudeScope
    /// doesn't model yet (currently `sandbox`). Replace-with-warning so the
    /// user sees the diff and can decide; structured merge is a follow-up.
    ReplaceComplex,
    /// Key not in our known-keys table. Conservative replace-with-warning so
    /// new Claude Code keys still work but the user is told we don't have a
    /// documented policy for them.
    ReplaceUnknown,
}

/// Documented merge semantics for a top-level Claude Code settings key.
///
/// Sourced from the official Claude Code settings reference. The lists of
/// override-only keys are exhaustive as of the current docs so that real
/// keys land on `Replace` rather than the `ReplaceUnknown` warning fallback.
/// Caller note: `permissions` is handled by the per-rule move flow, not the
/// key-move flow, and is rejected upstream by `validate_move_key`.
pub fn key_policy(key: &str) -> KeyPolicy {
    match key {
        "env" => KeyPolicy::DeepMerge,
        "allowedHttpHookUrls" | "httpHookAllowedEnvVars" => KeyPolicy::ArrayUnion,
        "sandbox" => KeyPolicy::ReplaceComplex,
        k if OVERRIDE_ONLY_KEYS.contains(&k) => KeyPolicy::Replace,
        _ => KeyPolicy::ReplaceUnknown,
    }
}

/// Top-level keys documented as override-only (highest-precedence scope
/// wins; values are not merged across scopes). Kept as a sorted slice so
/// future additions stay easy to scan and `contains` is fine at this size.
const OVERRIDE_ONLY_KEYS: &[&str] = &[
    "agent",
    "allowManagedHooksOnly",
    "allowManagedMcpServersOnly",
    "allowManagedPermissionRulesOnly",
    "allowedChannelPlugins",
    "allowedMcpServers",
    "alwaysThinkingEnabled",
    "apiKeyHelper",
    "attribution",
    "autoMemoryDirectory",
    "autoMode",
    "autoScrollEnabled",
    "autoUpdatesChannel",
    "availableModels",
    "awaySummaryEnabled",
    "awsAuthRefresh",
    "awsCredentialExport",
    "blockedMarketplaces",
    "channelsEnabled",
    "cleanupPeriodDays",
    "companyAnnouncements",
    "defaultShell",
    "deniedMcpServers",
    "disableAllHooks",
    "disableAutoMode",
    "disableDeepLinkRegistration",
    "disableSkillShellExecution",
    "disabledMcpjsonServers",
    "editorMode",
    "effortLevel",
    "enableAllProjectMcpServers",
    "enabledMcpjsonServers",
    "fastModePerSessionOptIn",
    "feedbackSurveyRate",
    "fileSuggestion",
    "forceLoginMethod",
    "forceLoginOrgUUID",
    "forceRemoteSettingsRefresh",
    "hooks",
    "includeCoAuthoredBy",
    "includeGitInstructions",
    "language",
    "minimumVersion",
    "model",
    "modelOverrides",
    "otelHeadersHelper",
    "outputStyle",
    "plansDirectory",
    "pluginTrustMessage",
    "prefersReducedMotion",
    "prUrlTemplate",
    "respectGitignore",
    "showClearContextOnPlanAccept",
    "showThinkingSummaries",
    "showTurnDuration",
    "skipWebFetchPreflight",
    "spinnerTipsEnabled",
    "spinnerTipsOverride",
    "spinnerVerbs",
    "sshConfigs",
    "statusLine",
    "strictKnownMarketplaces",
    "teammateMode",
    "terminalProgressBarEnabled",
    "tui",
    "useAutoModeDuringPlan",
    "viewMode",
    "voice",
    "voiceEnabled",
    "worktree",
    "wslInheritsWindowsSettings",
];

/// Recursive object merge: nested objects merge key-by-key, source wins on
/// any non-object collision. Used for [`KeyPolicy::DeepMerge`] keys.
fn deep_merge_in_place(dest: &mut Value, src: Value) {
    match (dest, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(&k) {
                    Some(existing) => deep_merge_in_place(existing, v),
                    None => {
                        d.insert(k, v);
                    }
                }
            }
        }
        (dest_slot, src) => {
            *dest_slot = src;
        }
    }
}

/// Append items from `src` onto `dest`, skipping any that compare equal to
/// an existing entry. If either side isn't an array, the source replaces
/// the destination — `merge_top_level`'s preview note tells the user when
/// this falls back to overwrite behavior. Used for [`KeyPolicy::ArrayUnion`].
fn array_union_in_place(dest: &mut Value, src: Value) {
    match (dest, src) {
        (Value::Array(d), Value::Array(s)) => {
            for item in s {
                if !d.iter().any(|v| v == &item) {
                    d.push(item);
                }
            }
        }
        (dest_slot, src) => {
            *dest_slot = src;
        }
    }
}

struct IndentFormatter {
    indent: Vec<u8>,
    current: usize,
    has_value: Vec<bool>,
}

impl IndentFormatter {
    fn new(indent: Indent) -> Self {
        Self {
            indent: indent.as_bytes(),
            current: 0,
            has_value: Vec::new(),
        }
    }

    fn write_indent<W: ?Sized + std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        for _ in 0..self.current {
            writer.write_all(&self.indent)?;
        }
        Ok(())
    }
}

impl serde_json::ser::Formatter for IndentFormatter {
    fn begin_array<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.current += 1;
        self.has_value.push(false);
        writer.write_all(b"[")
    }
    fn end_array<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.current -= 1;
        let had = self.has_value.pop().unwrap_or(false);
        if had {
            writer.write_all(b"\n")?;
            self.write_indent(writer)?;
        }
        writer.write_all(b"]")
    }
    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if !first {
            writer.write_all(b",")?;
        }
        writer.write_all(b"\n")?;
        self.write_indent(writer)
    }
    fn end_array_value<W: ?Sized + std::io::Write>(&mut self, _w: &mut W) -> std::io::Result<()> {
        if let Some(flag) = self.has_value.last_mut() {
            *flag = true;
        }
        Ok(())
    }
    fn begin_object<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.current += 1;
        self.has_value.push(false);
        writer.write_all(b"{")
    }
    fn end_object<W: ?Sized + std::io::Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        self.current -= 1;
        let had = self.has_value.pop().unwrap_or(false);
        if had {
            writer.write_all(b"\n")?;
            self.write_indent(writer)?;
        }
        writer.write_all(b"}")
    }
    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if !first {
            writer.write_all(b",")?;
        }
        writer.write_all(b"\n")?;
        self.write_indent(writer)
    }
    fn begin_object_value<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        writer.write_all(b": ")
    }
    fn end_object_value<W: ?Sized + std::io::Write>(&mut self, _w: &mut W) -> std::io::Result<()> {
        if let Some(flag) = self.has_value.last_mut() {
            *flag = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_permissions_when_missing() {
        let doc = SettingsDoc::empty();
        let p = doc.permissions();
        assert!(p.allow.is_empty());
        assert!(p.deny.is_empty());
        assert!(p.ask.is_empty());
    }

    #[test]
    fn reads_allow_and_deny() {
        let doc = SettingsDoc::from_value(
            serde_json::json!({
                "permissions": {
                    "allow": ["Bash(git status)", "Read(**)"],
                    "deny": ["WebFetch(domain:evil.example)"]
                }
            }),
            Indent::Spaces(2),
        );
        let p = doc.permissions();
        assert_eq!(p.allow, vec!["Bash(git status)", "Read(**)"]);
        assert_eq!(p.deny, vec!["WebFetch(domain:evil.example)"]);
        assert!(p.ask.is_empty());
    }

    #[test]
    fn add_then_remove_roundtrip() {
        let mut doc = SettingsDoc::empty();
        doc.add_rule(PermissionKind::Allow, "Bash(git status)");
        assert!(doc
            .permissions()
            .allow
            .contains(&"Bash(git status)".to_string()));
        assert!(doc.remove_rule(PermissionKind::Allow, "Bash(git status)"));
        assert!(doc.permissions().allow.is_empty());
    }

    #[test]
    fn add_rule_is_idempotent_and_reports_mutation() {
        let mut doc = SettingsDoc::empty();
        assert!(doc.add_rule(PermissionKind::Deny, "Bash(rm -rf /)"));
        assert!(!doc.add_rule(PermissionKind::Deny, "Bash(rm -rf /)"));
        assert_eq!(doc.permissions().deny.len(), 1);
    }

    #[test]
    fn render_preserves_key_order_and_indent() {
        let doc = SettingsDoc::from_value(
            serde_json::from_str(r#"{"theme":"dark","permissions":{"allow":["a"],"deny":[]}}"#)
                .unwrap(),
            Indent::Spaces(4),
        );
        let out = doc.render();
        // Theme key should come before permissions (preserve_order).
        let theme_at = out.find("\"theme\"").unwrap();
        let perms_at = out.find("\"permissions\"").unwrap();
        assert!(theme_at < perms_at);
        // Indent width should be 4.
        assert!(out.contains("\n    \"theme\""));
    }

    #[test]
    fn render_with_tab_indent() {
        let doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["x"]}}),
            Indent::Tab,
        );
        let out = doc.render();
        assert!(out.contains("\n\t\"permissions\""));
    }

    #[test]
    fn render_empty_object_stays_compact() {
        let doc = SettingsDoc::empty();
        let out = doc.render();
        assert_eq!(out.trim_end(), "{}");
    }

    #[test]
    fn remove_rule_on_missing_is_noop() {
        let mut doc = SettingsDoc::empty();
        assert!(!doc.remove_rule(PermissionKind::Allow, "nope"));
    }

    #[test]
    fn env_uses_deep_merge_with_source_winning() {
        assert_eq!(key_policy("env"), KeyPolicy::DeepMerge);
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"env": {"PATH": "/old", "HOME": "/home/a"}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level("env", serde_json::json!({"PATH": "/new", "API_KEY": "abc"}));
        let env = doc.get_top_level("env").unwrap();
        assert_eq!(env["PATH"], "/new");
        assert_eq!(env["HOME"], "/home/a");
        assert_eq!(env["API_KEY"], "abc");
    }

    #[test]
    fn env_deep_merge_recurses_into_nested_objects() {
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"env": {"group": {"A": "1", "B": "2"}}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level("env", serde_json::json!({"group": {"B": "new", "C": "3"}}));
        let group = &doc.get_top_level("env").unwrap()["group"];
        assert_eq!(group["A"], "1");
        assert_eq!(group["B"], "new");
        assert_eq!(group["C"], "3");
    }

    #[test]
    fn allowed_http_hook_urls_uses_array_union() {
        assert_eq!(key_policy("allowedHttpHookUrls"), KeyPolicy::ArrayUnion);
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"allowedHttpHookUrls": ["https://a.example", "https://b.example"]}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "allowedHttpHookUrls",
            serde_json::json!(["https://b.example", "https://c.example"]),
        );
        assert_eq!(
            *doc.get_top_level("allowedHttpHookUrls").unwrap(),
            serde_json::json!([
                "https://a.example",
                "https://b.example",
                "https://c.example"
            ])
        );
    }

    #[test]
    fn http_hook_allowed_env_vars_uses_array_union() {
        assert_eq!(key_policy("httpHookAllowedEnvVars"), KeyPolicy::ArrayUnion);
    }

    #[test]
    fn hooks_is_override_only_replace_not_deep_merge() {
        // Regression: pre-#33, the generic shape-based merge would deep-merge
        // hooks objects across scopes. Claude Code itself reads hooks
        // override-only, so a move must replace, not merge.
        assert_eq!(key_policy("hooks"), KeyPolicy::Replace);
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"hooks": {"PreToolUse": [{"command": "old"}]}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "hooks",
            serde_json::json!({"PostToolUse": [{"command": "new"}]}),
        );
        assert_eq!(
            *doc.get_top_level("hooks").unwrap(),
            serde_json::json!({"PostToolUse": [{"command": "new"}]})
        );
    }

    #[test]
    fn scalar_keys_replace() {
        assert_eq!(key_policy("model"), KeyPolicy::Replace);
        let mut doc =
            SettingsDoc::from_value(serde_json::json!({"theme": "dark"}), Indent::Spaces(2));
        doc.merge_top_level("theme", serde_json::json!("light"));
        assert_eq!(
            doc.get_top_level("theme").unwrap(),
            &serde_json::json!("light")
        );
    }

    #[test]
    fn missing_destination_inserts_value_unchanged() {
        let mut doc = SettingsDoc::empty();
        doc.merge_top_level("env", serde_json::json!({"A": "1"}));
        assert_eq!(doc.get_top_level("env").unwrap()["A"], "1");
        let mut doc = SettingsDoc::empty();
        doc.merge_top_level("hooks", serde_json::json!({"PreToolUse": []}));
        assert_eq!(
            *doc.get_top_level("hooks").unwrap(),
            serde_json::json!({"PreToolUse": []})
        );
    }

    #[test]
    fn sandbox_uses_replace_complex_pending_structured_merge() {
        // sandbox has per-subkey merge semantics (some arrays union, some
        // scalars override) that ClaudeScope doesn't model yet. Until then
        // the destination is replaced wholesale; the user sees this in the
        // preview note. See the follow-up issue tracked in commands.rs.
        assert_eq!(key_policy("sandbox"), KeyPolicy::ReplaceComplex);
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": {"enabled": false, "filesystem": {"allowWrite": ["/old"]}}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "sandbox",
            serde_json::json!({"enabled": true, "filesystem": {"allowWrite": ["/new"]}}),
        );
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({"enabled": true, "filesystem": {"allowWrite": ["/new"]}})
        );
    }

    #[test]
    fn unknown_keys_replace_with_warning_policy() {
        assert_eq!(key_policy("notARealKey"), KeyPolicy::ReplaceUnknown);
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"notARealKey": {"existing": true}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level("notARealKey", serde_json::json!({"new": 1}));
        assert_eq!(
            *doc.get_top_level("notARealKey").unwrap(),
            serde_json::json!({"new": 1})
        );
    }

    #[test]
    fn array_union_falls_back_to_replace_on_shape_mismatch() {
        // If the destination's existing value isn't an array (corrupted or
        // hand-edited file), array-union has nothing to append to. Source
        // replaces — the diff preview shows the user what happened.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"allowedHttpHookUrls": "not-an-array"}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "allowedHttpHookUrls",
            serde_json::json!(["https://a.example"]),
        );
        assert_eq!(
            *doc.get_top_level("allowedHttpHookUrls").unwrap(),
            serde_json::json!(["https://a.example"])
        );
    }

    #[test]
    fn remove_top_level_reports_presence() {
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"env": {"A": "1"}, "theme": "dark"}),
            Indent::Spaces(2),
        );
        assert!(doc.remove_top_level("theme"));
        assert!(!doc.remove_top_level("theme"));
        assert!(doc.get_top_level("theme").is_none());
        assert!(doc.get_top_level("env").is_some());
    }
}
