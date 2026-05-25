//! Detect redundant permission rules across the loaded scopes (#17).
//!
//! Two redundancy shapes are reported:
//!
//!   - **Duplicate** — the exact same rule string appears in more than one
//!     `(scope, kind, index)` position within the same `PermissionKind`.
//!     Trivial; any of the copies can be removed.
//!   - **Subsumed** — a narrower rule pattern is fully covered by a broader
//!     rule of the same kind (`Bash(git status)` ⊂ `Bash(git *)`). The
//!     narrower copy adds nothing because the broader rule already
//!     grants / denies it.
//!
//! Both cases are scoped to one [`PermissionKind`] at a time. Cross-kind
//! shadowing (`allow Bash(git *)` vs `deny Bash(git push)`) is a different
//! UX surface — the user might *want* a carve-out — and is left to the
//! kind-conflict warning (#156) for v0.7.
//!
//! ## Subsumption philosophy
//!
//! Conservative-by-design. The detector reports **only** the cases it can
//! decide with certainty by string matching; everything else falls through
//! as "not redundant." Claude Code's real argument grammars per tool aren't
//! publicly documented in full, so a wrong "remove this, it's covered"
//! claim would actively erase user intent — far worse than a false
//! negative.
//!
//! The recognized subsumption shapes are:
//!
//!   - **Bare tool name** covers `Tool(<anything>)`. `Bash` (equivalent to
//!     `Bash(*)`) covers `Bash(git status)` and every other `Bash(...)`.
//!   - **Tool with universal arg** `Tool(*)` covers `Tool(<anything>)`.
//!   - **Tool with glob suffix** `Tool(prefix*)` covers
//!     `Tool(prefixXYZ)` where the broader rule's literal prefix is a
//!     byte-prefix of the narrower rule's full arg. The `*` is treated as
//!     "any tail" — Claude Code's matcher is shell-glob-ish but we don't
//!     assume per-character semantics.
//!   - **WebFetch domain wildcards** — `WebFetch(domain:*)` covers any
//!     `WebFetch(domain:<host>)`. `WebFetch(domain:*.suffix)` covers any
//!     `WebFetch(domain:<sub>.suffix)`.
//!   - **MCP server wildcards** — `mcp__<server>__*` covers
//!     `mcp__<server>__<tool>` for the same server.
//!
//! Everything else returns `false`. Notably: nested glob patterns
//! (`Bash(git **)`), path globs with intermediate wildcards
//! (`Read(/foo/*/bar)`), and any rule whose argument the lint rejects
//! all fall through unmatched.

use serde::Serialize;

use crate::commands::ScopeView;
use crate::model::PermissionKind;
use crate::scope::Scope;

/// One detected redundancy: a `redundant` rule that's fully covered by
/// `covered_by`. The frontend renders a ⚠ badge on every chip whose
/// `(scope, kind, index)` matches `redundant`, with a popover that names
/// the covering rule by string + scope.
///
/// Position fields (`scope` / `kind` / `index`) carry enough context to
/// dispatch an `apply_delete_leaf` IPC against the redundant rule
/// directly — wiring that delete path into a "Remove redundant" action
/// is a follow-up.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Redundancy {
    pub redundant: RuleLoc,
    pub covered_by: RuleLoc,
    pub kind: RedundancyKind,
}

/// Position of one rule inside a `LoadedScopes` view. `index` is the
/// position within `permissions.<kind>` at the redundant rule's scope.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuleLoc {
    pub rule: String,
    pub scope: Scope,
    pub kind: PermissionKind,
    pub index: usize,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedundancyKind {
    /// Exact string duplicate.
    Duplicate,
    /// Pattern subsumption: `redundant`'s pattern is a strict subset of
    /// `covered_by`'s pattern.
    Subsumed,
}

/// Walk every (scope, kind, rule) triple across `views` and report the
/// minimal set of redundancies a user could safely remove.
///
/// Detection direction: for each rule, find the **broadest** other rule
/// of the same kind that covers it. A rule covered by itself (its own
/// position in the iteration) doesn't count.
///
/// The returned list is deduplicated on `redundant.{scope, kind, index}`
/// so each redundant chip surfaces at most one badge — the badge picks
/// the broadest coverer to display.
pub fn detect_redundancies(views: &[ScopeView]) -> Vec<Redundancy> {
    // Flatten to a Vec of (loc, view-index) so the cross-pair scan can
    // skip-self by index. `views` is in `Scope::ALL` (precedence) order,
    // so a deterministic scan inherits that order — handy for tests
    // that pin the chosen coverer.
    let mut rules: Vec<RuleLoc> = Vec::new();
    for view in views {
        let perms = crate::commands::permissions_from_values(&view.values);
        for (kind, list) in [
            (PermissionKind::Allow, &perms.allow),
            (PermissionKind::Deny, &perms.deny),
            (PermissionKind::Ask, &perms.ask),
        ] {
            for (idx, rule) in list.iter().enumerate() {
                rules.push(RuleLoc {
                    rule: rule.clone(),
                    scope: view.scope,
                    kind,
                    index: idx,
                });
            }
        }
    }

    let mut out: Vec<Redundancy> = Vec::new();
    for (i, candidate) in rules.iter().enumerate() {
        // Find the broadest other rule of the same kind that covers
        // `candidate`. "Broadest" is determined by `rule_breadth`: a
        // bare tool name > `Tool(*)` > `Tool(prefix*)` > exact. Ties
        // resolve by scan order (which matches `Scope::ALL`
        // precedence) so the chosen coverer is deterministic.
        let mut best: Option<(&RuleLoc, RedundancyKind, u32)> = None;
        for (j, other) in rules.iter().enumerate() {
            if i == j {
                continue;
            }
            if other.kind != candidate.kind {
                continue;
            }
            let Some(kind_of_redundancy) = subsumes(&other.rule, &candidate.rule) else {
                continue;
            };
            let breadth = rule_breadth(&other.rule);
            let take = match best {
                None => true,
                Some((_, _, b)) => breadth > b,
            };
            if take {
                best = Some((other, kind_of_redundancy, breadth));
            }
        }
        if let Some((other, kind, _)) = best {
            // For an exact duplicate, only emit the badge on ONE of the
            // two copies — otherwise both copies show "this is redundant
            // because of the other one" which is circular. Keep the
            // later-scanned copy as the redundant one; the earlier one
            // (which appears first in precedence + index order) is the
            // canonical keeper.
            if kind == RedundancyKind::Duplicate {
                let other_idx = rules.iter().position(|r| r == other).unwrap_or(usize::MAX);
                if other_idx < i {
                    out.push(Redundancy {
                        redundant: candidate.clone(),
                        covered_by: other.clone(),
                        kind,
                    });
                }
            } else {
                out.push(Redundancy {
                    redundant: candidate.clone(),
                    covered_by: other.clone(),
                    kind,
                });
            }
        }
    }
    out
}

/// Returns `Some(kind)` iff `broader` is recognized to cover `narrower`
/// for the same `PermissionKind`. Returns `None` for any case the
/// detector can't decide with certainty.
///
/// The kind axis is the caller's responsibility — this function compares
/// rule *strings* only.
fn subsumes(broader: &str, narrower: &str) -> Option<RedundancyKind> {
    // Reject obviously unparseable inputs up front so the identity branch
    // can't fire on garbage strings (e.g. `subsumes("", "")` had been
    // falsely reporting an empty pair as a duplicate).
    if parse_rule(broader).is_none() || parse_rule(narrower).is_none() {
        return None;
    }
    if broader == narrower {
        return Some(RedundancyKind::Duplicate);
    }
    if covers_strictly(broader, narrower) {
        return Some(RedundancyKind::Subsumed);
    }
    None
}

/// True iff `broader`'s pattern covers `narrower`'s pattern AND they are
/// not byte-equal. Identity is handled separately by [`subsumes`] so the
/// duplicate / subsumed distinction stays in the caller.
fn covers_strictly(broader: &str, narrower: &str) -> bool {
    let Some(b) = parse_rule(broader) else {
        return false;
    };
    let Some(n) = parse_rule(narrower) else {
        return false;
    };

    // MCP: server-level wildcard covers every tool under that server.
    if let (ParsedRule::Mcp(b_server, b_tool), ParsedRule::Mcp(n_server, _)) = (&b, &n) {
        if b_server == n_server && b_tool == "*" {
            return true;
        }
    }

    // Tool name must match for non-MCP. Bare tool name is treated as
    // `Tool(*)` for coverage purposes.
    let (broader_tool, broader_args) = match b {
        ParsedRule::BareTool(name) => (name, "*".to_string()),
        ParsedRule::Tool(name, args) => (name, args),
        ParsedRule::Mcp(_, _) => return false,
    };
    let (narrower_tool, narrower_args) = match n {
        ParsedRule::BareTool(name) => (name, "*".to_string()),
        ParsedRule::Tool(name, args) => (name, args),
        ParsedRule::Mcp(_, _) => return false,
    };
    if broader_tool != narrower_tool {
        return false;
    }

    args_cover(&broader_tool, &broader_args, &narrower_args)
}

/// Tool-specific argument-pattern coverage. Conservative by design —
/// `false` is always a safe answer.
fn args_cover(tool: &str, broader: &str, narrower: &str) -> bool {
    if broader == narrower {
        return false; // identity handled upstream
    }
    // Universal arg: `Tool(*)` covers any non-empty arg.
    if broader == "*" {
        return !narrower.is_empty();
    }
    // WebFetch domain wildcards: handle `domain:*` and `domain:*.suffix`.
    if tool == "WebFetch" {
        if let (Some(b_host), Some(n_host)) = (
            broader.strip_prefix("domain:"),
            narrower.strip_prefix("domain:"),
        ) {
            return domain_covers(b_host, n_host);
        }
        return false;
    }
    // Generic glob suffix: `prefix*` covers `prefix<anything>` (where
    // `<anything>` is non-empty and the literal prefix is a byte-prefix
    // of the narrower arg). Reserved for the case where the broader rule
    // ends in exactly one `*` and the narrower has no `*` at all — the
    // moment either side gets fancier (intermediate `*`, `**`, character
    // classes) we bail.
    if let Some(prefix) = broader.strip_suffix('*') {
        if !prefix.contains('*') && !narrower.contains('*') && narrower.starts_with(prefix) {
            // Require the narrower arg to actually extend the prefix —
            // otherwise `Bash(*)` would be claimed to cover itself, and
            // empty-extension cases collapse into the duplicate branch.
            return narrower.len() > prefix.len();
        }
    }
    false
}

/// Domain-glob coverage for WebFetch. `*` covers anything; `*.suffix`
/// covers `<sub>.suffix` (single label or further nested). Conservative:
/// no support for mid-string wildcards.
fn domain_covers(broader: &str, narrower: &str) -> bool {
    if broader == "*" {
        return !narrower.is_empty();
    }
    if let Some(suffix) = broader.strip_prefix("*.") {
        if narrower.contains('*') {
            return false;
        }
        // `*.github.com` covers `api.github.com` (one label prefix) and
        // `nested.api.github.com` (multi-label) — match the wildcard's
        // shell-style semantics.
        if let Some(stripped) = narrower.strip_suffix(suffix) {
            return stripped.ends_with('.') && stripped.len() > 1;
        }
    }
    false
}

/// Rank a rule's "breadth" — how many other rules it could plausibly
/// cover. Used to pick the broadest coverer when multiple rules cover
/// the same candidate. Higher number = broader.
///
/// The ranking is heuristic but stable enough that tests can pin a
/// specific coverer choice. Exact values don't matter; only their
/// relative order does.
fn rule_breadth(rule: &str) -> u32 {
    let Some(parsed) = parse_rule(rule) else {
        return 0;
    };
    match parsed {
        // Bare tool name and `Tool(*)` are equivalent — both rank as
        // "universal". MCP server-level wildcard sits at the same tier
        // since it covers all tools under the server.
        ParsedRule::BareTool(_) => 100,
        ParsedRule::Tool(_, args) if args == "*" => 100,
        ParsedRule::Mcp(_, tool) if tool == "*" => 100,
        // Tool with a glob suffix: more specific than universal but
        // broader than an exact rule. Shorter literal prefix = broader.
        ParsedRule::Tool(_, args)
            if args.ends_with('*') && !args[..args.len() - 1].contains('*') =>
        {
            50 + (32_u32.saturating_sub(args.len() as u32))
        }
        // WebFetch domain wildcards: same tier as glob suffix, scored
        // by how much of the domain is wild.
        ParsedRule::Tool(tool, args) if tool == "WebFetch" && args.starts_with("domain:*") => 60,
        _ => 10,
    }
}

#[derive(Debug, Clone)]
enum ParsedRule {
    /// Bare tool name (no parens). `Bash` is equivalent to `Bash(*)`.
    BareTool(String),
    /// Tool with arguments: `Tool(<args>)`.
    Tool(String, String),
    /// MCP rule: `mcp__<server>__<tool>` or `mcp__<server>__*`.
    Mcp(String, String),
}

fn parse_rule(raw: &str) -> Option<ParsedRule> {
    let rule = raw.trim();
    if rule.is_empty() {
        return None;
    }
    if let Some(rest) = rule.strip_prefix("mcp__") {
        let parts: Vec<&str> = rest.split("__").collect();
        if parts.len() < 2 {
            return None;
        }
        let server = parts[0].to_string();
        let tool = parts[parts.len() - 1].to_string();
        if server.is_empty() || tool.is_empty() {
            return None;
        }
        return Some(ParsedRule::Mcp(server, tool));
    }
    if let Some(paren_start) = rule.find('(') {
        if !rule.ends_with(')') {
            return None;
        }
        let name = rule[..paren_start].to_string();
        let args = rule[paren_start + 1..rule.len() - 1].to_string();
        if name.is_empty() {
            return None;
        }
        return Some(ParsedRule::Tool(name, args));
    }
    // Bare tool name — alphanumeric only (the lint accepts an explicit
    // small set, but for coverage purposes any identifier is fine).
    if rule.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Some(ParsedRule::BareTool(rule.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ScopeView;

    fn view(scope: Scope, allow: &[&str], deny: &[&str], ask: &[&str]) -> ScopeView {
        let mut values = serde_json::Map::new();
        if !allow.is_empty() || !deny.is_empty() || !ask.is_empty() {
            values.insert(
                "permissions".into(),
                serde_json::json!({
                    "allow": allow,
                    "deny": deny,
                    "ask": ask,
                }),
            );
        }
        ScopeView {
            scope,
            path: None,
            exists: true,
            values,
            parse_error: None,
        }
    }

    // ---- subsumes() -----------------------------------------------------

    #[test]
    fn subsumes_identity_is_duplicate() {
        assert_eq!(
            subsumes("Bash(git status)", "Bash(git status)"),
            Some(RedundancyKind::Duplicate)
        );
    }

    #[test]
    fn subsumes_universal_arg_covers_any_arg_for_same_tool() {
        assert_eq!(
            subsumes("Bash(*)", "Bash(git status)"),
            Some(RedundancyKind::Subsumed)
        );
        assert_eq!(
            subsumes("Bash(*)", "Bash(echo hi)"),
            Some(RedundancyKind::Subsumed)
        );
    }

    #[test]
    fn subsumes_bare_tool_equivalent_to_universal() {
        // `Bash` is shorthand for `Bash(*)` per the lint module.
        assert_eq!(
            subsumes("Bash", "Bash(git status)"),
            Some(RedundancyKind::Subsumed)
        );
    }

    #[test]
    fn subsumes_glob_suffix_covers_prefixed_args() {
        assert_eq!(
            subsumes("Bash(git *)", "Bash(git status)"),
            Some(RedundancyKind::Subsumed)
        );
        assert_eq!(
            subsumes("Read(/home/u/foo/*)", "Read(/home/u/foo/bar.txt)"),
            Some(RedundancyKind::Subsumed)
        );
    }

    #[test]
    fn subsumes_glob_suffix_does_not_cover_unrelated_prefix() {
        // Different prefix — the broader rule does not in fact cover.
        assert_eq!(subsumes("Bash(git *)", "Bash(rm -rf /)"), None);
    }

    #[test]
    fn subsumes_does_not_claim_when_narrower_also_has_glob() {
        // `Bash(git *)` vs `Bash(git st*)` — both have wildcards;
        // deciding whether the latter's match set is a subset of the
        // former's requires real glob semantics we don't have. Bail.
        assert_eq!(subsumes("Bash(git *)", "Bash(git st*)"), None);
    }

    #[test]
    fn subsumes_does_not_claim_across_different_tools() {
        assert_eq!(subsumes("Bash(*)", "Read(/etc/passwd)"), None);
    }

    #[test]
    fn subsumes_webfetch_universal_domain_covers_specific_host() {
        assert_eq!(
            subsumes("WebFetch(domain:*)", "WebFetch(domain:api.github.com)"),
            Some(RedundancyKind::Subsumed)
        );
    }

    #[test]
    fn subsumes_webfetch_wildcard_suffix_covers_subdomain() {
        assert_eq!(
            subsumes(
                "WebFetch(domain:*.github.com)",
                "WebFetch(domain:api.github.com)"
            ),
            Some(RedundancyKind::Subsumed)
        );
        // Multi-level subdomains also count: `*.github.com` covers
        // `nested.api.github.com` because shell-glob semantics make
        // `*` greedy across `.`.
        assert_eq!(
            subsumes(
                "WebFetch(domain:*.github.com)",
                "WebFetch(domain:nested.api.github.com)"
            ),
            Some(RedundancyKind::Subsumed)
        );
    }

    #[test]
    fn subsumes_webfetch_wildcard_does_not_cover_unrelated_domain() {
        assert_eq!(
            subsumes(
                "WebFetch(domain:*.github.com)",
                "WebFetch(domain:gitlab.com)"
            ),
            None
        );
    }

    #[test]
    fn subsumes_mcp_server_wildcard_covers_specific_tool() {
        assert_eq!(
            subsumes("mcp__github__*", "mcp__github__list_issues"),
            Some(RedundancyKind::Subsumed)
        );
    }

    #[test]
    fn subsumes_mcp_server_wildcard_does_not_cover_different_server() {
        assert_eq!(subsumes("mcp__github__*", "mcp__gitlab__list_issues"), None);
    }

    #[test]
    fn subsumes_bails_on_unparseable_inputs() {
        assert_eq!(subsumes("not a rule", "Bash(rm)"), None);
        assert_eq!(subsumes("Bash(rm", "Bash(rm)"), None);
        assert_eq!(subsumes("", ""), None);
    }

    // ---- detect_redundancies() ------------------------------------------

    #[test]
    fn detects_exact_duplicate_within_one_scope_and_kind() {
        // Hand-edited duplicate in one file. Emits ONE redundancy
        // (later copy is the redundant one; earlier is the keeper) —
        // not two cross-referencing each other.
        let views = vec![view(Scope::Local, &["Bash(rm)", "Bash(rm)"], &[], &[])];
        let red = detect_redundancies(&views);
        assert_eq!(red.len(), 1);
        assert_eq!(red[0].redundant.index, 1);
        assert_eq!(red[0].covered_by.index, 0);
        assert_eq!(red[0].kind, RedundancyKind::Duplicate);
    }

    #[test]
    fn detects_exact_duplicate_across_scopes() {
        // `Bash(rm)` in both User and Project — same kind. Scan order
        // is callsite-supplied (`view` calls go in test order), so
        // whichever scope appears first is the keeper. The detector's
        // job is to emit exactly one redundancy in either case.
        let views = vec![
            view(Scope::Project, &["Bash(rm)"], &[], &[]),
            view(Scope::User, &["Bash(rm)"], &[], &[]),
        ];
        let red = detect_redundancies(&views);
        assert_eq!(red.len(), 1);
        // First-scanned scope (Project here) is the keeper.
        assert_eq!(red[0].covered_by.scope, Scope::Project);
        assert_eq!(red[0].redundant.scope, Scope::User);
    }

    #[test]
    fn detects_subsumption_within_one_scope() {
        // `Bash(git *)` covers `Bash(git status)`.
        let views = vec![view(
            Scope::Local,
            &["Bash(git *)", "Bash(git status)"],
            &[],
            &[],
        )];
        let red = detect_redundancies(&views);
        assert_eq!(red.len(), 1);
        assert_eq!(red[0].redundant.rule, "Bash(git status)");
        assert_eq!(red[0].covered_by.rule, "Bash(git *)");
        assert_eq!(red[0].kind, RedundancyKind::Subsumed);
    }

    #[test]
    fn detects_subsumption_across_scopes() {
        // User has the broad rule; Project has the specific one. The
        // Project specific is redundant.
        let views = vec![
            view(Scope::Project, &["Bash(git status)"], &[], &[]),
            view(Scope::User, &["Bash(git *)"], &[], &[]),
        ];
        let red = detect_redundancies(&views);
        assert_eq!(red.len(), 1);
        assert_eq!(red[0].redundant.scope, Scope::Project);
        assert_eq!(red[0].covered_by.scope, Scope::User);
    }

    #[test]
    fn does_not_detect_subsumption_across_kinds() {
        // Same rule string, but allow vs deny. That's the
        // kind-conflict warning (#156), not a redundancy.
        let views = vec![view(
            Scope::Project,
            &["Bash(git *)"],
            &["Bash(git push)"],
            &[],
        )];
        let red = detect_redundancies(&views);
        assert!(
            red.is_empty(),
            "cross-kind shadowing is out of scope: {red:?}"
        );
    }

    #[test]
    fn picks_the_broadest_coverer_when_multiple_apply() {
        // `Bash(git status)` is covered by both `Bash(*)` (universal)
        // and `Bash(git *)` (prefix). The broader rule (`Bash(*)`)
        // should be reported as the coverer.
        let views = vec![view(
            Scope::Local,
            &["Bash(git status)", "Bash(git *)", "Bash(*)"],
            &[],
            &[],
        )];
        let red = detect_redundancies(&views);
        // Three rules; two should be flagged as redundant (everything
        // except `Bash(*)`).
        let redundant_rules: Vec<&str> = red.iter().map(|r| r.redundant.rule.as_str()).collect();
        assert!(redundant_rules.contains(&"Bash(git status)"));
        assert!(redundant_rules.contains(&"Bash(git *)"));
        // The flagged `Bash(git status)` should point at the broadest
        // coverer, not the intermediate `Bash(git *)`.
        let git_status_red = red
            .iter()
            .find(|r| r.redundant.rule == "Bash(git status)")
            .unwrap();
        assert_eq!(git_status_red.covered_by.rule, "Bash(*)");
    }

    #[test]
    fn returns_empty_when_no_redundancies() {
        let views = vec![view(
            Scope::Local,
            &[
                "Bash(git status)",
                "Read(**)",
                "WebFetch(domain:github.com)",
            ],
            &[],
            &[],
        )];
        assert!(detect_redundancies(&views).is_empty());
    }

    #[test]
    fn detects_mcp_server_wildcard_subsumption() {
        let views = vec![view(
            Scope::Local,
            &["mcp__github__*", "mcp__github__list_issues"],
            &[],
            &[],
        )];
        let red = detect_redundancies(&views);
        assert_eq!(red.len(), 1);
        assert_eq!(red[0].redundant.rule, "mcp__github__list_issues");
        assert_eq!(red[0].covered_by.rule, "mcp__github__*");
    }

    #[test]
    fn webfetch_subdomain_wildcard_subsumption_end_to_end() {
        let views = vec![view(
            Scope::Local,
            &[
                "WebFetch(domain:*.github.com)",
                "WebFetch(domain:api.github.com)",
            ],
            &[],
            &[],
        )];
        let red = detect_redundancies(&views);
        assert_eq!(red.len(), 1);
        assert_eq!(red[0].redundant.rule, "WebFetch(domain:api.github.com)");
    }

    #[test]
    fn three_copies_in_two_scopes_emit_one_keeper_two_redundants() {
        // Hand-edited duplicate in Project + a copy in User. The
        // first-scanned copy (Project's first slot) becomes the
        // keeper; the other two are redundant. Three rules → two
        // redundancies, regardless of which is the keeper.
        let views = vec![
            view(Scope::Project, &["Bash(rm)", "Bash(rm)"], &[], &[]),
            view(Scope::User, &["Bash(rm)"], &[], &[]),
        ];
        let red = detect_redundancies(&views);
        assert_eq!(red.len(), 2);
        // All redundants point at the same keeper.
        let keepers: std::collections::HashSet<_> = red
            .iter()
            .map(|r| (r.covered_by.scope, r.covered_by.index))
            .collect();
        assert_eq!(
            keepers.len(),
            1,
            "all redundants should share one keeper: {red:?}"
        );
    }
}
