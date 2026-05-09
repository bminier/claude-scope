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

/// One segment of a JSON path. The frontend ships paths as mixed arrays of
/// strings and numbers (`["permissions", "allow", 2]`), so an untagged enum
/// over `String` / `usize` keeps the wire format identical to JSON Pointer
/// shape without forcing every caller to escape numeric indices.
///
/// `Key` is tried first by serde, which is the right preference: object keys
/// are strings on the wire even when they happen to look numeric, and
/// `serde_json` only feeds the `usize` arm an actual JSON number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathSeg {
    Key(String),
    Index(usize),
}

impl PathSeg {
    /// Borrow the key segment if this is one. Used by the diff/apply flows
    /// to pull the affected top-level key off a validated path; index
    /// segments don't appear at index 0 of any movable shape.
    pub fn as_key(&self) -> Option<&str> {
        match self {
            PathSeg::Key(s) => Some(s.as_str()),
            PathSeg::Index(_) => None,
        }
    }
}

/// Shape of a path the move-leaf primitive accepts. `validate_movable_path`
/// classifies every incoming request into one of these so the diff/apply
/// impls can dispatch on a small, exhaustive enum instead of scrutinizing the
/// path slice in three places. The set deliberately mirrors today's two-IPC
/// capabilities (whole top-level key, whole permission kind array, single
/// permission rule) — broader sub-path moves into other keys (`env.PATH`,
/// `hooks.PreToolUse[0]`, etc.) are explicitly rejected for v1; they're a
/// natural follow-up but out of scope for issue #67.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MovablePath<'a> {
    /// Whole top-level key. Mirrors the old key-move flow.
    TopLevelKey(&'a str),
    /// Whole `permissions.<kind>` array (allow / deny / ask). Array-union into
    /// the destination's matching array.
    PermissionList(PermissionKind),
    /// Single rule entry under `permissions.<kind>`. Array-union into the
    /// destination's matching array (push if not already present).
    PermissionRule(PermissionKind, usize),
}

/// Classify a wire-level path into a `MovablePath`, or reject it with a
/// descriptive error. Centralizing the rules here keeps the diff path, apply
/// path, and any future caller in lockstep — a path that's invalid at
/// validation time is impossible at the dispatch site.
pub fn validate_movable_path(path: &[PathSeg]) -> Result<MovablePath<'_>, String> {
    match path {
        [PathSeg::Key(k)] if k == "permissions" => Err(
            "the permissions key is not movable as a whole; move a specific rule list \
             (allow / deny / ask) or a single rule"
                .into(),
        ),
        [PathSeg::Key(k)] => Ok(MovablePath::TopLevelKey(k.as_str())),
        [PathSeg::Key(perm), PathSeg::Key(kind)] if perm == "permissions" => {
            permission_kind_from_str(kind).map(MovablePath::PermissionList)
        }
        [PathSeg::Key(perm), PathSeg::Key(kind), PathSeg::Index(i)] if perm == "permissions" => {
            permission_kind_from_str(kind).map(|k| MovablePath::PermissionRule(k, *i))
        }
        [] => Err("path is empty; move requires a target".into()),
        _ => Err(format!(
            "path is not movable in this version: {}",
            describe_path(path)
        )),
    }
}

fn permission_kind_from_str(s: &str) -> Result<PermissionKind, String> {
    match s {
        "allow" => Ok(PermissionKind::Allow),
        "deny" => Ok(PermissionKind::Deny),
        "ask" => Ok(PermissionKind::Ask),
        other => Err(format!(
            "unknown permission kind `{other}`: expected allow / deny / ask"
        )),
    }
}

/// Render a path slice in JSON-Pointer-ish form for error messages. Uses dot
/// separators for keys and bracketed indices for array entries (e.g.
/// `permissions.allow[2]`) — readable in error toasts without needing the
/// caller to construct the string themselves.
pub fn describe_path(path: &[PathSeg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            PathSeg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            PathSeg::Index(i) => {
                out.push_str(&format!("[{i}]"));
            }
        }
    }
    if out.is_empty() {
        out.push_str("(root)");
    }
    out
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

    /// Top-level non-permission entries (key + value) in on-disk order.
    /// The UI consumes `all_entries` instead since the migration in #67
    /// rendered `permissions` inline with the other top-level keys; this
    /// accessor is kept under `#[cfg(test)]` so unit tests can still make
    /// "permissions vs everything else" assertions compactly.
    #[cfg(test)]
    pub fn other_entries(&self) -> Map<String, Value> {
        let Some(obj) = self.root.as_object() else {
            return Map::new();
        };
        obj.iter()
            .filter(|(k, _)| k.as_str() != "permissions")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Every top-level key + value in on-disk order, including `permissions`.
    /// Drives the unified scope-column tree on the frontend.
    pub fn all_entries(&self) -> Map<String, Value> {
        let Some(obj) = self.root.as_object() else {
            return Map::new();
        };
        obj.clone()
    }

    /// Read the permissions block as a `PermissionRules` snapshot. Missing or
    /// malformed shapes produce empty lists rather than failing; a partially
    /// corrupt settings file shouldn't stop the whole UI from loading.
    /// Production code reaches permissions through the unified `values`
    /// map (see `commands::permissions_from_values`); this accessor is
    /// kept for test convenience.
    #[cfg(test)]
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

    /// Read a single top-level key (any type). Tests and the move-leaf
    /// diff/apply use it for one-shot reads; production callers that need
    /// path-based access go through `get_at_path`.
    pub fn get_top_level(&self, key: &str) -> Option<&Value> {
        self.root.get(key)
    }

    /// Merge `value` into the top-level `key` using the documented Claude
    /// Code semantics for that key (see [`key_policy`]).
    ///
    /// If the key isn't present yet, the value is inserted unchanged. If the
    /// key already exists, the policy decides what happens:
    ///
    /// - [`KeyPolicy::Replace`] / [`KeyPolicy::ReplaceUnknown`]: the
    ///   destination is overwritten.
    /// - [`KeyPolicy::DeepMerge`]: object trees are merged recursively, with
    ///   source values winning on scalar/non-object conflicts.
    /// - [`KeyPolicy::ArrayUnion`]: arrays are concatenated with duplicates
    ///   removed; mismatched shapes fall back to overwrite.
    /// - [`KeyPolicy::Sandbox`]: object tree walked per [`SANDBOX_SCHEMA`];
    ///   leaves under the schema's array-union paths union, every other
    ///   leaf replaces. Mismatched shapes fall back to overwrite.
    pub fn merge_top_level(&mut self, key: &str, value: Value) {
        let policy = key_policy(key);
        let obj = ensure_object(&mut self.root);
        match policy {
            KeyPolicy::Replace | KeyPolicy::ReplaceUnknown => {
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
            KeyPolicy::Sandbox => match obj.get_mut(key) {
                Some(existing) => sandbox_merge_in_place(existing, value),
                None => {
                    obj.insert(key.to_string(), value);
                }
            },
        }
    }

    /// Read the JSON value at `path` if every segment resolves. Object keys
    /// are matched verbatim; array indices must be in-bounds. Returns `None`
    /// when any segment misses, which the move flow distinguishes from
    /// `Some(Value::Null)` (a real null value at that path).
    pub fn get_at_path(&self, path: &[PathSeg]) -> Option<&Value> {
        let mut cur = &self.root;
        for seg in path {
            cur = match (cur, seg) {
                (Value::Object(obj), PathSeg::Key(k)) => obj.get(k)?,
                (Value::Array(arr), PathSeg::Index(i)) => arr.get(*i)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// Remove the value at `path`. Mirrors `get_at_path`'s navigation but
    /// removes the trailing segment instead of returning the value: keys are
    /// removed from their parent object, array indices are spliced out (which
    /// shifts later indices). Returns true iff a removal happened — a missing
    /// path is a no-op `false` so callers can detect a stale request.
    pub fn remove_at_path(&mut self, path: &[PathSeg]) -> bool {
        let Some((last, parents)) = path.split_last() else {
            return false;
        };
        let mut cur = &mut self.root;
        for seg in parents {
            cur = match (cur, seg) {
                (Value::Object(obj), PathSeg::Key(k)) => match obj.get_mut(k) {
                    Some(v) => v,
                    None => return false,
                },
                (Value::Array(arr), PathSeg::Index(i)) => match arr.get_mut(*i) {
                    Some(v) => v,
                    None => return false,
                },
                _ => return false,
            };
        }
        match (cur, last) {
            (Value::Object(obj), PathSeg::Key(k)) => obj.remove(k).is_some(),
            (Value::Array(arr), PathSeg::Index(i)) if *i < arr.len() => {
                arr.remove(*i);
                true
            }
            _ => false,
        }
    }

    /// Merge `value` into the destination using the move-leaf semantics
    /// classified by `validate_movable_path`. The caller is expected to have
    /// already validated `path`; passing an invalid path here is a bug, so
    /// this returns an error rather than silently dropping the write.
    ///
    /// - `MovablePath::TopLevelKey(k)` defers to `merge_top_level` so the
    ///   existing per-key policy table (Replace / DeepMerge / ArrayUnion /
    ///   Sandbox / ReplaceUnknown) governs the merge.
    /// - `MovablePath::PermissionList(kind)` array-unions the incoming list
    ///   into `permissions.<kind>`. Non-array sources fall back to overwrite,
    ///   matching `array_union_in_place`'s shape-mismatch contract.
    /// - `MovablePath::PermissionRule(kind, _)` ignores the source index (the
    ///   destination's array order is independent) and pushes the rule string
    ///   into `permissions.<kind>` if not already present.
    pub fn merge_at_path(&mut self, path: &[PathSeg], value: Value) -> Result<(), String> {
        let movable = validate_movable_path(path)?;
        match movable {
            MovablePath::TopLevelKey(k) => {
                self.merge_top_level(k, value);
                Ok(())
            }
            MovablePath::PermissionList(kind) => {
                // Reject up front when the source value isn't an array.
                // `array_union_in_place` would otherwise fall back to an
                // overwrite (its documented shape-mismatch path), which for
                // a permission move means propagating a malformed
                // `permissions.<kind>` (a string, scalar, object) into the
                // destination scope.
                if !value.is_array() {
                    return Err(format!(
                        "permission list at `permissions.{}` must be a JSON array, got {}",
                        kind.key(),
                        json_type_name(&value)
                    ));
                }
                // Refuse to move when any entry isn't a string. The
                // renderer, combined panel, count helpers, and confirm
                // modal all treat permission lists as string-rules-only
                // (non-strings are filtered silently) — unioning a number
                // or object would write data the UI never shows on the
                // dest. Filtering them out on the wire would silently
                // drop the user's data without telling them; the
                // explicit error gives them a chance to fix the file
                // before retrying. Defense in depth: the frontend
                // suppresses the affordance too, but a hand-crafted IPC
                // would otherwise corrupt the destination.
                let arr = value.as_array().expect("value.is_array()");
                if let Some((idx, bad)) = arr.iter().enumerate().find(|(_, v)| !v.is_string()) {
                    return Err(format!(
                        "permission list at `permissions.{}` contains a non-string entry \
                         at index {} ({}); ClaudeScope only treats string entries as rules \
                         — fix the file before moving the list",
                        kind.key(),
                        idx,
                        json_type_name(bad)
                    ));
                }
                let arr_slot = ensure_permission_list(&mut self.root, kind);
                array_union_in_place(arr_slot, value);
                Ok(())
            }
            MovablePath::PermissionRule(kind, _) => {
                let Some(rule) = value.as_str() else {
                    return Err(format!(
                        "permission rule must be a JSON string, got {}",
                        json_type_name(&value)
                    ));
                };
                let arr_value = ensure_permission_list(&mut self.root, kind);
                let arr = arr_value
                    .as_array_mut()
                    .expect("ensure_permission_list returns array");
                if !arr.iter().any(|v| v.as_str() == Some(rule)) {
                    arr.push(Value::String(rule.to_string()));
                }
                Ok(())
            }
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

/// Resolve `root.permissions.<kind>` to a mutable array slot, creating any
/// missing intermediates. If `permissions` exists but is the wrong shape
/// (e.g. a hand-edited file where `permissions` ended up as a string), the
/// non-object value is replaced with a fresh map — matching the conservative
/// "rather create than fail" stance of `add_rule`. Returns the array slot as
/// a `&mut Value` so callers can hand it straight to `array_union_in_place`.
fn ensure_permission_list(root: &mut Value, kind: PermissionKind) -> &mut Value {
    let obj = ensure_object(root);
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
    list_entry
}

/// Human-readable name for a JSON value's runtime shape, used in error
/// messages. Lifted to a helper so the move-leaf API can stay terse.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// How a top-level settings key should be combined when its value is moved
/// from one scope into another that already has the same key. Modelled on
/// how Claude Code itself reads each key across scopes, so that a move in
/// ClaudeScope produces an effective config consistent with the documented
/// semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyPolicy {
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
    /// `sandbox` — object whose nested fields have mixed semantics: some
    /// arrays under `filesystem.*`/`network.*`/`excludedCommands` are
    /// concatenated and deduplicated, every other leaf is override-only.
    /// Walked per [`SANDBOX_SCHEMA`].
    Sandbox,
    /// Key not in our known-keys table. Conservative replace-with-warning so
    /// new Claude Code keys still work but the user is told we don't have a
    /// documented policy for them.
    ReplaceUnknown,
}

/// Documented merge semantics for a top-level Claude Code settings key.
///
/// `OVERRIDE_ONLY_KEYS` covers the override-only keys from the official
/// Claude Code settings reference plus a few legacy/project-supported keys
/// (e.g. `theme`) that this crate's contract advertises in
/// `CLAUDE.md`. Anything missing falls through to `ReplaceUnknown`, which
/// shows a "no documented policy" warning rather than silently picking
/// behavior. Caller note: `permissions` is handled by the per-rule move
/// flow, not the key-move flow, and is rejected upstream by
/// `validate_move_key`.
pub(crate) fn key_policy(key: &str) -> KeyPolicy {
    match key {
        "env" => KeyPolicy::DeepMerge,
        "allowedHttpHookUrls" | "httpHookAllowedEnvVars" => KeyPolicy::ArrayUnion,
        "sandbox" => KeyPolicy::Sandbox,
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
    "enabledPlugins",
    "extraKnownMarketplaces",
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
    // `theme` isn't in the current Claude Code settings reference, but this
    // project's CLAUDE.md and tests advertise it as a supported top-level
    // key, so users who still carry it in settings.json see Replace rather
    // than the unknown-key warning.
    "theme",
    "tui",
    "useAutoModeDuringPlan",
    "viewMode",
    "voice",
    "voiceEnabled",
    "worktree",
    "wslInheritsWindowsSettings",
];

/// Recursive object merge: nested objects merge key-by-key, source wins on
/// any non-object collision. Used for [`KeyPolicy::DeepMerge`] keys. If
/// either side isn't an object the source replaces the destination outright;
/// the move flow's preview note inspects the runtime shapes (see
/// `policy_preview_note` in `commands.rs`) so the user is warned ahead of
/// time when this fallback applies.
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
/// an existing entry. Used for [`KeyPolicy::ArrayUnion`]. If either side
/// isn't an array the source replaces the destination outright; the move
/// flow's preview note inspects the runtime shapes (see `policy_preview_note`
/// in `commands.rs`) so the user is warned ahead of time when this fallback
/// applies.
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

/// Dotted paths inside the `sandbox` object whose values are arrays that
/// Claude Code documents as concatenated-and-deduplicated across scopes.
/// Anything else under `sandbox` (scalars, unknown subkeys) is treated as
/// override-only — the source value replaces whatever was at the same path
/// in the destination. New entries should match the docs at
/// <https://code.claude.com/docs/en/settings>.
pub(crate) const SANDBOX_SCHEMA: &[&str] = &[
    "excludedCommands",
    "filesystem.allowRead",
    "filesystem.allowWrite",
    "filesystem.denyRead",
    "filesystem.denyWrite",
    "network.allowMachLookup",
    "network.allowUnixSockets",
    "network.allowedDomains",
    "network.deniedDomains",
];

/// Merge a `sandbox` object using the documented per-subkey semantics in
/// [`SANDBOX_SCHEMA`]. Used for [`KeyPolicy::Sandbox`].
///
/// At each key, the walker dispatches in this order:
/// - If both sides are objects, recurse into them. Object subtrees always
///   recurse regardless of whether their dotted path appears in the schema —
///   the schema describes leaves, not branches.
/// - Else if the dotted path matches an entry in [`SANDBOX_SCHEMA`], the
///   leaf goes through [`array_union_in_place`].
/// - Else the destination's leaf is replaced by the source's value.
/// - Subkeys present only on the source are inserted as-is.
///
/// If either side at the top level isn't an object — e.g. a hand-edited
/// file where `sandbox` ended up as a string — the source replaces the
/// destination outright. The move flow's preview note inspects the runtime
/// shapes (see `policy_preview_note` in `commands.rs`) so the user is
/// warned ahead of time when that fallback applies.
fn sandbox_merge_in_place(dest: &mut Value, src: Value) {
    if !dest.is_object() || !src.is_object() {
        *dest = src;
        return;
    }
    structured_merge_in_place(dest, src, SANDBOX_SCHEMA, "");
}

/// Recursive helper for [`sandbox_merge_in_place`]: walk the source object
/// key-by-key against `dest`, building up a dotted path so each leaf can
/// be matched against `array_union_paths`. Nested objects on both sides
/// recurse; everywhere else, the array-union schema decides between
/// `array_union_in_place` and a straight overwrite.
fn structured_merge_in_place(
    dest: &mut Value,
    src: Value,
    array_union_paths: &[&str],
    base_path: &str,
) {
    let (Value::Object(d), Value::Object(s)) = (dest, src) else {
        return;
    };
    for (k, v) in s {
        let path = if base_path.is_empty() {
            k.clone()
        } else {
            format!("{base_path}.{k}")
        };
        match d.get_mut(&k) {
            Some(existing) if existing.is_object() && v.is_object() => {
                structured_merge_in_place(existing, v, array_union_paths, &path);
            }
            Some(existing) if array_union_paths.contains(&path.as_str()) => {
                array_union_in_place(existing, v);
            }
            Some(existing) => {
                *existing = v;
            }
            None => {
                d.insert(k, v);
            }
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
    fn project_supported_legacy_keys_resolve_to_replace_not_unknown() {
        // theme isn't in the current Claude Code settings reference, but
        // CLAUDE.md and the project tests advertise it as supported, so it
        // must land on Replace rather than the ReplaceUnknown warning.
        assert_eq!(key_policy("theme"), KeyPolicy::Replace);
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
    fn sandbox_uses_structured_merge_policy() {
        assert_eq!(key_policy("sandbox"), KeyPolicy::Sandbox);
    }

    #[test]
    fn sandbox_unions_filesystem_allow_write_arrays() {
        // The acceptance criterion for #84: array fields under
        // sandbox.filesystem.* concatenate and deduplicate across scopes
        // rather than the source replacing the destination wholesale.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": {"filesystem": {"allowWrite": ["/dest", "/shared"]}}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "sandbox",
            serde_json::json!({"filesystem": {"allowWrite": ["/shared", "/source"]}}),
        );
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({
                "filesystem": {"allowWrite": ["/dest", "/shared", "/source"]}
            })
        );
    }

    #[test]
    fn sandbox_replaces_scalar_enabled() {
        // Scalar fields (and any subkey not in SANDBOX_SCHEMA) are
        // override-only — the source's value replaces whatever was there.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": {"enabled": false, "failIfUnavailable": false}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level("sandbox", serde_json::json!({"enabled": true}));
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({"enabled": true, "failIfUnavailable": false})
        );
    }

    #[test]
    fn sandbox_combines_array_union_and_scalar_replace() {
        // Mixed move: array leaves union, scalar leaves replace, untouched
        // destination keys survive. Network array (allowedDomains) and
        // filesystem array (denyRead) both go through the schema; the
        // scalar `enabled` flips; the dest-only `httpProxyPort` survives.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({
                "sandbox": {
                    "enabled": false,
                    "filesystem": {"denyRead": ["~/.aws/credentials"]},
                    "network": {
                        "allowedDomains": ["github.com"],
                        "httpProxyPort": 8080
                    }
                }
            }),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "sandbox",
            serde_json::json!({
                "enabled": true,
                "filesystem": {"denyRead": ["~/.ssh/id_rsa"]},
                "network": {"allowedDomains": ["*.npmjs.org"]}
            }),
        );
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({
                "enabled": true,
                "filesystem": {
                    "denyRead": ["~/.aws/credentials", "~/.ssh/id_rsa"]
                },
                "network": {
                    "allowedDomains": ["github.com", "*.npmjs.org"],
                    "httpProxyPort": 8080
                }
            })
        );
    }

    #[test]
    fn sandbox_inserts_source_only_subtrees() {
        // Subkeys present only on the source should be added to the
        // destination wholesale.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": {"enabled": true}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "sandbox",
            serde_json::json!({"network": {"allowedDomains": ["github.com"]}}),
        );
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({
                "enabled": true,
                "network": {"allowedDomains": ["github.com"]}
            })
        );
    }

    #[test]
    fn sandbox_replaces_arrays_not_in_schema() {
        // Only the dotted paths listed in SANDBOX_SCHEMA union — an
        // arbitrary array under filesystem.* (or anywhere else) that isn't
        // in the schema must replace, not union, so the preview note
        // doesn't overpromise. Regression test for PR review on #84.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": {"filesystem": {"unknownArrayField": ["dest"]}}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "sandbox",
            serde_json::json!({"filesystem": {"unknownArrayField": ["src"]}}),
        );
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({"filesystem": {"unknownArrayField": ["src"]}})
        );
    }

    #[test]
    fn sandbox_recurses_into_nested_objects_outside_schema() {
        // Object subtrees always recurse when both sides are objects, even
        // when the path isn't in SANDBOX_SCHEMA (the schema describes
        // leaves, not branches). A scalar inside the unknown subtree should
        // still replace at the leaf, but sibling keys must survive.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": {"futureFeature": {"a": 1, "b": 2}}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level("sandbox", serde_json::json!({"futureFeature": {"b": 99}}));
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({"futureFeature": {"a": 1, "b": 99}})
        );
    }

    #[test]
    fn sandbox_array_union_leaves_replace_on_leaf_shape_mismatch() {
        // A documented array-union path (filesystem.allowWrite) where one
        // side isn't an array — e.g. hand-edited file that left a string
        // there — falls back to leaf replacement via array_union_in_place's
        // shape-mismatch path. The preview note now documents this caveat.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": {"filesystem": {"allowWrite": "broken-string"}}}),
            Indent::Spaces(2),
        );
        doc.merge_top_level(
            "sandbox",
            serde_json::json!({"filesystem": {"allowWrite": ["/src"]}}),
        );
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({"filesystem": {"allowWrite": ["/src"]}})
        );
    }

    #[test]
    fn sandbox_falls_back_to_replace_on_shape_mismatch() {
        // Hand-edited file where sandbox ended up as a string. The
        // structured walker can't recurse into a non-object; source replaces.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"sandbox": "broken-string"}),
            Indent::Spaces(2),
        );
        doc.merge_top_level("sandbox", serde_json::json!({"enabled": true}));
        assert_eq!(
            *doc.get_top_level("sandbox").unwrap(),
            serde_json::json!({"enabled": true})
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
    fn deep_merge_falls_back_to_replace_on_shape_mismatch() {
        // If the destination's existing value isn't an object (corrupted or
        // hand-edited file), deep-merge has nothing to recurse into. Source
        // replaces — the diff preview note flags this so the user isn't
        // surprised.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"env": "not-an-object"}),
            Indent::Spaces(2),
        );
        doc.merge_top_level("env", serde_json::json!({"A": "1"}));
        assert_eq!(
            *doc.get_top_level("env").unwrap(),
            serde_json::json!({"A": "1"})
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

    fn key(s: &str) -> PathSeg {
        PathSeg::Key(s.to_string())
    }

    fn idx(i: usize) -> PathSeg {
        PathSeg::Index(i)
    }

    #[test]
    fn validate_movable_path_classifies_top_level_key() {
        assert_eq!(
            validate_movable_path(&[key("env")]).unwrap(),
            MovablePath::TopLevelKey("env")
        );
    }

    #[test]
    fn validate_movable_path_classifies_permission_list() {
        assert_eq!(
            validate_movable_path(&[key("permissions"), key("allow")]).unwrap(),
            MovablePath::PermissionList(PermissionKind::Allow)
        );
        assert_eq!(
            validate_movable_path(&[key("permissions"), key("deny")]).unwrap(),
            MovablePath::PermissionList(PermissionKind::Deny)
        );
    }

    #[test]
    fn validate_movable_path_classifies_permission_rule() {
        assert_eq!(
            validate_movable_path(&[key("permissions"), key("ask"), idx(3)]).unwrap(),
            MovablePath::PermissionRule(PermissionKind::Ask, 3)
        );
    }

    #[test]
    fn validate_movable_path_rejects_bare_permissions_key() {
        // Whole-block permissions moves bypass the per-rule policy and would
        // surprise users — reject so the UI never offers the gesture.
        assert!(validate_movable_path(&[key("permissions")]).is_err());
    }

    #[test]
    fn validate_movable_path_rejects_unknown_permission_kind() {
        assert!(validate_movable_path(&[key("permissions"), key("maybe")]).is_err());
    }

    #[test]
    fn validate_movable_path_rejects_intermediate_subkeys() {
        // v1 only carries today's two-IPC capabilities forward; sub-path moves
        // into other keys (`env.PATH`, `hooks.PreToolUse[0]`) are explicitly
        // out of scope until a follow-up issue.
        assert!(validate_movable_path(&[key("env"), key("PATH")]).is_err());
        assert!(validate_movable_path(&[key("hooks"), key("PreToolUse"), idx(0)]).is_err());
    }

    #[test]
    fn validate_movable_path_rejects_empty() {
        assert!(validate_movable_path(&[]).is_err());
    }

    #[test]
    fn describe_path_renders_dot_and_brackets() {
        assert_eq!(
            describe_path(&[key("permissions"), key("allow"), idx(2)]),
            "permissions.allow[2]"
        );
        assert_eq!(describe_path(&[key("env")]), "env");
        assert_eq!(describe_path(&[]), "(root)");
    }

    #[test]
    fn get_at_path_navigates_object_and_array() {
        let doc = SettingsDoc::from_value(
            serde_json::json!({
                "permissions": {"allow": ["Bash(git status)", "Read(**)"]},
                "env": {"PATH": "/bin"}
            }),
            Indent::Spaces(2),
        );
        assert_eq!(
            doc.get_at_path(&[key("permissions"), key("allow"), idx(1)])
                .unwrap(),
            &serde_json::json!("Read(**)")
        );
        assert_eq!(
            doc.get_at_path(&[key("env"), key("PATH")]).unwrap(),
            &serde_json::json!("/bin")
        );
    }

    #[test]
    fn get_at_path_returns_none_on_miss() {
        let doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["x"]}}),
            Indent::Spaces(2),
        );
        // Index out of bounds.
        assert!(doc
            .get_at_path(&[key("permissions"), key("allow"), idx(7)])
            .is_none());
        // Wrong segment kind for the runtime shape.
        assert!(doc.get_at_path(&[key("permissions"), idx(0)]).is_none());
        // Missing key.
        assert!(doc.get_at_path(&[key("nope")]).is_none());
    }

    #[test]
    fn get_at_path_distinguishes_null_value_from_absence() {
        // Real `null` round-trips as `Some(Value::Null)`; absence is `None`.
        // The move flow keys off this distinction (skip_serializing_if).
        let doc = SettingsDoc::from_value(serde_json::json!({"theme": null}), Indent::Spaces(2));
        assert_eq!(doc.get_at_path(&[key("theme")]), Some(&Value::Null));
        assert_eq!(doc.get_at_path(&[key("missing")]), None);
    }

    #[test]
    fn remove_at_path_removes_top_level_key() {
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"theme": "dark", "env": {"A": "1"}}),
            Indent::Spaces(2),
        );
        assert!(doc.remove_at_path(&[key("theme")]));
        assert!(doc.get_top_level("theme").is_none());
        assert!(doc.get_top_level("env").is_some());
    }

    #[test]
    fn remove_at_path_splices_array_element_and_shifts() {
        // Removing an element shifts later indices left — caller responsibility
        // to re-fetch by value, not by stale index.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["a", "b", "c"]}}),
            Indent::Spaces(2),
        );
        assert!(doc.remove_at_path(&[key("permissions"), key("allow"), idx(1)]));
        assert_eq!(
            doc.get_at_path(&[key("permissions"), key("allow")])
                .unwrap(),
            &serde_json::json!(["a", "c"])
        );
    }

    #[test]
    fn remove_at_path_returns_false_on_miss() {
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["x"]}}),
            Indent::Spaces(2),
        );
        assert!(!doc.remove_at_path(&[key("permissions"), key("allow"), idx(7)]));
        assert!(!doc.remove_at_path(&[key("permissions"), key("deny"), idx(0)]));
        assert!(!doc.remove_at_path(&[key("nope")]));
        assert!(!doc.remove_at_path(&[]));
    }

    #[test]
    fn merge_at_path_top_level_key_dispatches_to_existing_policy() {
        // env's KeyPolicy::DeepMerge must apply through merge_at_path so the
        // unified primitive doesn't change semantics for keys that already had
        // a policy under merge_top_level.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"env": {"PATH": "/old", "HOME": "/h"}}),
            Indent::Spaces(2),
        );
        doc.merge_at_path(&[key("env")], serde_json::json!({"PATH": "/new", "X": "1"}))
            .unwrap();
        assert_eq!(
            *doc.get_top_level("env").unwrap(),
            serde_json::json!({"PATH": "/new", "HOME": "/h", "X": "1"})
        );
    }

    #[test]
    fn merge_at_path_permission_list_unions_array() {
        // Whole-list move: array-union into the destination's matching kind,
        // matching the rule-move semantics rather than a wholesale replace.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["a", "b"]}}),
            Indent::Spaces(2),
        );
        doc.merge_at_path(
            &[key("permissions"), key("allow")],
            serde_json::json!(["b", "c"]),
        )
        .unwrap();
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["a", "b", "c"]})
        );
    }

    #[test]
    fn merge_at_path_permission_rule_pushes_when_absent() {
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["a"]}}),
            Indent::Spaces(2),
        );
        doc.merge_at_path(
            &[key("permissions"), key("allow"), idx(0)],
            serde_json::json!("b"),
        )
        .unwrap();
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["a", "b"]})
        );
    }

    #[test]
    fn merge_at_path_permission_rule_is_idempotent() {
        // Existing rule already in the destination is a no-op merge; the
        // source-removal half of the move flow handles the cleanup.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["a"]}}),
            Indent::Spaces(2),
        );
        doc.merge_at_path(
            &[key("permissions"), key("allow"), idx(0)],
            serde_json::json!("a"),
        )
        .unwrap();
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["a"]})
        );
    }

    #[test]
    fn merge_at_path_permission_rule_creates_missing_kind_array() {
        // Destination has no `deny` array yet — the helper should create it
        // and insert, mirroring `add_rule`'s self-bootstrapping behavior.
        let mut doc = SettingsDoc::empty();
        doc.merge_at_path(
            &[key("permissions"), key("deny"), idx(0)],
            serde_json::json!("WebFetch(domain:evil.example)"),
        )
        .unwrap();
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"deny": ["WebFetch(domain:evil.example)"]})
        );
    }

    #[test]
    fn merge_at_path_rejects_non_string_permission_rule() {
        let mut doc = SettingsDoc::empty();
        let err = doc
            .merge_at_path(
                &[key("permissions"), key("allow"), idx(0)],
                serde_json::json!(42),
            )
            .unwrap_err();
        assert!(err.contains("must be a JSON string"));
    }

    #[test]
    fn merge_at_path_rejects_non_array_permission_list() {
        // Hand-edited file where `permissions.allow` ended up as a string
        // (or any non-array). Without this guard, `array_union_in_place`
        // would fall back to overwrite and propagate the malformed shape
        // into the destination scope. The frontend already suppresses the
        // affordance, but the IPC must refuse a hand-crafted request too.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["a"]}}),
            Indent::Spaces(2),
        );
        let err = doc
            .merge_at_path(
                &[key("permissions"), key("allow")],
                serde_json::json!("not-an-array"),
            )
            .unwrap_err();
        assert!(err.contains("must be a JSON array"));
        // Destination unchanged.
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["a"]})
        );
    }

    #[test]
    fn merge_at_path_rejects_permission_list_with_non_string_entry() {
        // Source array contains a number — the rest of the codebase treats
        // permission lists as string-rules-only (renderer + combined panel
        // + count helpers all filter), so a permission-list move must
        // refuse rather than silently union an entry the UI never shows on
        // the dest. The error names the bad index so the user can find it.
        let mut doc = SettingsDoc::from_value(
            serde_json::json!({"permissions": {"allow": ["a"]}}),
            Indent::Spaces(2),
        );
        let err = doc
            .merge_at_path(
                &[key("permissions"), key("allow")],
                serde_json::json!(["valid", 42, "another"]),
            )
            .unwrap_err();
        assert!(err.contains("non-string entry"));
        assert!(err.contains("index 1"));
        // Destination unchanged.
        assert_eq!(
            *doc.get_top_level("permissions").unwrap(),
            serde_json::json!({"allow": ["a"]})
        );
    }

    #[test]
    fn merge_at_path_rejects_invalid_path() {
        let mut doc = SettingsDoc::empty();
        // Bare permissions key — same rejection as validate_movable_path.
        assert!(doc
            .merge_at_path(&[key("permissions")], serde_json::json!({}))
            .is_err());
        // Sub-key under env not yet supported.
        assert!(doc
            .merge_at_path(&[key("env"), key("PATH")], serde_json::json!("/bin"))
            .is_err());
    }

    #[test]
    fn path_seg_serde_roundtrip_matches_wire_format() {
        // Frontend ships paths as JSON arrays of strings + numbers. The
        // untagged enum has to preserve that shape exactly so the IPC wire
        // format never needs an escape hatch.
        let path: Vec<PathSeg> =
            serde_json::from_value(serde_json::json!(["permissions", "allow", 2])).unwrap();
        assert_eq!(path, vec![key("permissions"), key("allow"), idx(2)]);

        let back = serde_json::to_value(&path).unwrap();
        assert_eq!(back, serde_json::json!(["permissions", "allow", 2]));
    }
}
