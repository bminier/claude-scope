# Scopes

Claude Code reads settings from JSON files at four scopes. The scopes are
laid out in **precedence order, highest first** — the highest-precedence
file that defines a key wins.

| # | Scope | Path | Tracked? |
|---|---|---|---|
| 1 | Managed | enterprise-deployed | Out of scope for ClaudeScope. |
| 2 | Local | `./.claude/settings.local.json` | Project-local, gitignored. |
| 3 | Project | `./.claude/settings.json` | Committed with the project. |
| 4 | User-Local | `~/.claude/settings.local.json` | Machine-local override. |
| 5 | User | `~/.claude/settings.json` | Machine-global. |

ClaudeScope **omits Managed** (#1) entirely — it's an enterprise concern
that lives outside the personal-tool workflow this project targets.

> **`~/.claude/settings.local.json`** was originally excluded too, but
> Claude Code is observed to create and use it when the user's home
> directory is itself inside a git repo. ClaudeScope treats it as a
> first-class scope on par with the other three.

## Column layout

The four scopes render as columns, **broadest-on-the-left to
narrowest-on-the-right**:

```
┌─────────┬─────────────┬──────────┬───────┐
│  User   │ User-Local  │ Project  │ Local │
└─────────┴─────────────┴──────────┴───────┘
   ←──── broader / shared      narrower ───→
```

That's the opposite of precedence order. ClaudeScope's column order is
about *audience* — User-scope rules affect every project, Local-scope
rules affect one. The **Effective settings** panel is the trailing column
in the same row, summarizing what applies in the active directory.
Default-collapsed with inline counts; expand to see the full union
across scopes. It's **not** a full precedence-aware evaluation of what
Claude Code resolves at runtime — the subtitle in the app says so.

## Per-scope file presence

A scope's column shows as **absent** when its file doesn't exist on
disk. ClaudeScope never auto-creates a scope file just to fill the
column; it creates the file on first write to that scope, and only
then. A move into an empty scope produces a fresh `settings.json` (or
`settings.local.json`) with just the moved rule.

The User and User-Local scopes resolve relative to the OS home
directory (`%USERPROFILE%` on Windows, `$HOME` elsewhere); Project and
Local resolve from the currently-loaded project root. See
[Architecture → Overview](../architecture/overview.md) for how scope
discovery walks up from the project picker's selection.

## Switching projects

Two ways to switch the loaded project:

- **Open project…** — file picker; default to `~` on first open.
- **Recent projects dropdown** — click the project label in the topbar
  for a menu of recently-opened roots (capped at 10, persists across
  sessions). See [Moving rules](./moving-rules.md) for the move flows
  that this enables across projects.
