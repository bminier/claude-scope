# Settings keys

A reference for every recognized top-level key in
`.claude/settings.json` (and the `.local.json` overrides), with the
same one-sentence explanations the in-app hover tooltips surface.

The full per-key help map lives in
[`src/ui.ts`](https://github.com/bminier/claude-scope/blob/dev/src/ui.ts)
today; tracked in
[#9](https://github.com/bminier/claude-scope/issues/9) for canonical
content with deep links to Claude Code's documentation. Until that's
exported as structured data, this page is a stub — see the in-app
tooltips for the current help text.

## Common keys

- **`permissions`** — `allow` / `deny` / `ask` rule lists. The
  primary surface ClaudeScope manages. See
  [Rule syntax](./rule-syntax.md).
- **`env`** — environment variables Claude Code injects into each
  agent invocation.
- **`hooks`** — pre / post tool-call hooks. Powerful and easy to
  misuse; double-check before moving these between scopes.
- **`theme`** — UI theme override (`auto` / `light` / `dark`).
- **`model`** — default model selector.
- **`apiKeyHelper`** — script invoked to source the API key on
  demand.
- **`statusLine`** — custom status-line configuration.
- **`enableAllProjectMcpServers`** — escape hatch for
  per-project MCP plumbing.

For the authoritative list, see
[Claude Code's settings documentation](https://docs.claude.com/en/docs/claude-code/settings).
