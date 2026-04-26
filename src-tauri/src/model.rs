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

    /// Merge `value` into the top-level `key` with type-aware semantics:
    ///
    /// - If both the existing and new values are objects, their keys are
    ///   merged recursively (new values win on conflict for scalars, arrays
    ///   accumulate with dedup, nested objects recurse).
    /// - If both are arrays, the new items are appended to the existing
    ///   array, skipping any that compare equal to an existing element.
    /// - In any other combination (scalar, or mismatched shapes), the new
    ///   value replaces whatever was there.
    ///
    /// If `key` didn't exist before, the new value is inserted as-is.
    ///
    /// This is a generic, *shape-based* policy: it does not know what each
    /// key actually means to Claude Code. Some keys may be replacement-only,
    /// some order-sensitive, some additive in ways that don't match
    /// "append + dedup". For keys where shape-based merge does not match the
    /// real semantics, this can silently change meaning. Tracked by
    /// <https://github.com/bminier/claude-scope/issues/33>.
    pub fn merge_top_level(&mut self, key: &str, value: Value) {
        let obj = ensure_object(&mut self.root);
        if let Some(existing) = obj.get_mut(key) {
            merge_value_in_place(existing, value);
        } else {
            obj.insert(key.to_string(), value);
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

/// Merge `src` into `dest` in place with the shape-aware rules documented
/// on `SettingsDoc::merge_top_level`. This is the recursive worker: object
/// nodes merge key-by-key, arrays accumulate with dedup, everything else
/// overwrites.
fn merge_value_in_place(dest: &mut Value, src: Value) {
    match (dest, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                if let Some(existing) = d.get_mut(&k) {
                    merge_value_in_place(existing, v);
                } else {
                    d.insert(k, v);
                }
            }
        }
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
    fn merge_top_level_object_merges_keys_new_wins() {
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
    fn merge_top_level_array_appends_and_dedupes() {
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"hooks": ["PreToolUse", "PostToolUse"]}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "hooks",
            serde_json::json!(["PostToolUse", "UserPromptSubmit"]),
        );
        let hooks = doc.get_top_level("hooks").unwrap();
        assert_eq!(
            *hooks,
            serde_json::json!(["PreToolUse", "PostToolUse", "UserPromptSubmit"])
        );
    }

    #[test]
    fn merge_top_level_scalar_overwrites() {
        let mut doc =
            SettingsDoc::from_value(serde_json::json!({"theme": "dark"}), Indent::Spaces(2));
        doc.merge_top_level("theme", serde_json::json!("light"));
        assert_eq!(
            doc.get_top_level("theme").unwrap(),
            &serde_json::json!("light")
        );
    }

    #[test]
    fn merge_top_level_inserts_when_missing() {
        let mut doc = SettingsDoc::empty();
        doc.merge_top_level("env", serde_json::json!({"A": "1"}));
        assert_eq!(doc.get_top_level("env").unwrap()["A"], "1");
    }

    #[test]
    fn merge_top_level_mismatched_shapes_overwrite() {
        // Existing is a string, new value is an object — shape mismatch, so
        // the new value replaces the old one rather than trying to coerce.
        let mut doc =
            SettingsDoc::from_value(serde_json::json!({"x": "scalar"}), Indent::Spaces(2));
        doc.merge_top_level("x", serde_json::json!({"nested": true}));
        assert_eq!(doc.get_top_level("x").unwrap()["nested"], true);
    }

    #[test]
    fn merge_recurses_into_nested_objects() {
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
