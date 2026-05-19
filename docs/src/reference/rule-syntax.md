# Rule syntax

Claude Code's permission grammar isn't publicly documented in full,
but a few common shapes are well-established. ClaudeScope recognizes
these and surfaces a subtle ⚠ lint badge on rules that don't match any
of them. **The badge is best-effort, not authoritative** — a flagged
rule may still be valid; the popover names the heuristic that tripped
so you can judge.

## Shapes ClaudeScope knows

| Shape | Example | Notes |
|---|---|---|
| `Bash(<pattern>)` | `Bash(git status)` | Shell command. Patterns may include `*` wildcards (`Bash(git *)`). |
| `Read(<glob>)` | `Read(**)` | File read. Standard glob syntax. |
| `WebFetch(domain:<host>)` | `WebFetch(domain:docs.claude.com)` | URL fetch restricted to a domain. |
| `mcp__<server>__<tool>` | `mcp__filesystem__read_file` | MCP server tool invocation. |
| `<Tool>(<args>)` | `Edit(*.ts)` / `Write(README.md)` | Generic tool-with-arguments shape. |

Rules outside these shapes (no parentheses, unrecognized tool names,
malformed args) are still allowed on disk — Claude Code may know
shapes ClaudeScope doesn't. The lint just flags them so you can
double-check.

## Kinds

Every rule lives under one of three lists at `permissions.<kind>`:

- **`allow`** — permit the action without prompting.
- **`deny`** — refuse the action.
- **`ask`** — prompt before allowing.

ClaudeScope shows each kind in its own column within a scope and lets
you reclassify a rule in-place (right-click → **Change kind** → pick
the destination kind). The same primitive powers cross-scope kind
changes — moving `Bash(rm *)` from project-allow to user-deny is one
operation.

## Examples by tool

Most realistic settings files have rules clustered around a few
tools. Two or more rules sharing a `Tool(...)` prefix fold under a
synthetic `Tool ▸ (N)` group in the per-scope tree view:

```jsonc
{
  "permissions": {
    "allow": [
      "Bash(git status)",
      "Bash(git diff)",
      "Bash(npm test)",
      "Read(**)",
      "WebFetch(domain:docs.claude.com)"
    ]
  }
}
```

In the UI:

```
permissions
├── allow
│   ├── Bash ▸ (3)
│   │   ├── Bash(git status)
│   │   ├── Bash(git diff)
│   │   └── Bash(npm test)
│   ├── Read(**)
│   └── WebFetch(domain:docs.claude.com)
```

The grouping threshold defaults to 2; tracked in
[#115](https://github.com/bminier/claude-scope/issues/115) for a
Settings-dialog control.

## Useful docs

- [Claude Code permissions](https://docs.claude.com/en/docs/claude-code/iam#permission-rules) — upstream reference for the permission system.
- [Settings keys](./settings-keys.md) — per-key help map (planned, [#9](https://github.com/bminier/claude-scope/issues/9)).
