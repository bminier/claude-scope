/**
 * Shape-only lint for Claude Code permission rule strings.
 *
 * This lives on top of an imprecise target — Claude Code's actual parser
 * isn't publicly documented in full (case sensitivity, whitespace handling,
 * per-tool argument grammars). We therefore stick to shape-matching against
 * the forms that do appear in official examples:
 *
 *   - `Bash(...)` / `Read(...)` / `Edit(...)` / `Write(...)` / `Agent(...)`
 *   - `WebFetch(domain:<host>)`
 *   - `mcp__<server>__<tool>` (or `mcp__<server>__*`)
 *   - Bare tool name (e.g. just `Bash`) — equivalent to `Bash(*)`
 *
 * What the lint does flag, deliberately:
 *   - Non-canonical casing (`bash(...)`) — docs always capitalize, so this
 *     is almost certainly a typo even if the runtime turns out to tolerate
 *     it. Falling into the "Unknown tool" branch is the right signal.
 *   - Whitespace between the tool name and the `(` (`Bash (...)`). Same
 *     reasoning: no example permits this, treating it as suspect is safer
 *     than silently accepting it.
 *
 * What the lint intentionally does not check:
 *   - The internal grammar of each tool's argument (Bash globs, Read path
 *     syntax, WebFetch domain shape beyond the `domain:` prefix). That's
 *     where the docs run out.
 *   - Load-time behavior — we don't know whether Claude Code drops,
 *     warns, or errors on malformed rules.
 *
 * No rule is ever rejected outright; we return a warning with a reason and
 * the UI shows a subtle indicator so users can fix typos before promoting
 * a half-broken rule to a wider scope. Moves are never blocked.
 *
 * If Anthropic publishes a formal schema later, we can tighten this toward
 * a strict validator.
 */

export interface LintResult {
  ok: boolean;
  reason?: string;
}

const BARE_TOOL_NAMES = new Set([
  "Bash",
  "Read",
  "Edit",
  "Write",
  "WebFetch",
  "Agent",
]);

const TOOLS_WITH_ARGS = BARE_TOOL_NAMES;

export function lintRule(raw: string): LintResult {
  const rule = raw.trim();
  if (rule === "") {
    return { ok: false, reason: "Rule is empty." };
  }

  // MCP tool rules: `mcp__<server>__<tool>` or `mcp__<server>__*`.
  if (rule.startsWith("mcp__")) {
    // An MCP rule is an identifier chain; parens or whitespace in there are
    // almost always the user mixing it up with the `Bash(...)` / `Read(...)`
    // shape (e.g. `mcp__github__list_issues()`), so flag those explicitly
    // before the segment check passes them through.
    if (/[\s()]/.test(rule)) {
      return {
        ok: false,
        reason: "MCP rule shouldn't contain parentheses or whitespace; use `mcp__<server>__<tool>`.",
      };
    }
    const parts = rule.split("__");
    // Expect at least: ["mcp", "<server>", "<tool-or-*>"]; more segments are
    // tolerated because tool names can contain underscores too (the split is
    // on the double-underscore separator; single underscores inside names are
    // fine, and real tool names rarely if ever contain `__`, so we don't
    // over-reach here).
    const server = parts[1];
    const tool = parts[parts.length - 1];
    if (parts.length < 3 || !server || !tool) {
      return {
        ok: false,
        reason: "MCP rule should look like `mcp__<server>__<tool>` (or `mcp__<server>__*`).",
      };
    }
    return { ok: true };
  }

  // Bare tool name (equivalent to `Tool(*)`).
  if (BARE_TOOL_NAMES.has(rule)) {
    return { ok: true };
  }

  // Tool-with-args: `Name(...)`.
  const parenStart = rule.indexOf("(");
  if (parenStart === -1) {
    return {
      ok: false,
      reason:
        "Unknown rule shape. Expected e.g. `Bash(git status)`, `Read(**)`, or `mcp__server__tool`.",
    };
  }
  if (!rule.endsWith(")")) {
    return { ok: false, reason: "Missing closing `)`." };
  }

  const name = rule.slice(0, parenStart);
  const args = rule.slice(parenStart + 1, rule.length - 1);

  if (!TOOLS_WITH_ARGS.has(name)) {
    return {
      ok: false,
      reason:
        `Unknown tool \`${name}\`. Expected one of: Bash, Read, Edit, Write, WebFetch, Agent, or mcp__…`,
    };
  }

  if (args.trim() === "") {
    return { ok: false, reason: `\`${name}(...)\` has empty arguments.` };
  }

  // WebFetch expects a `domain:` prefix per the docs. A bare URL, scheme, or
  // path is almost certainly user error.
  if (name === "WebFetch" && !args.trim().startsWith("domain:")) {
    return {
      ok: false,
      reason:
        "WebFetch requires a `domain:` prefix, e.g. `WebFetch(domain:example.com)`.",
    };
  }

  return { ok: true };
}
