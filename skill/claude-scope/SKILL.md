---
name: claude-scope
description: >-
  View and move Claude Code permission rules between settings scopes (User /
  User-Local / Project / Local) using the claude-scope CLI. Use when the user
  asks to move, promote, or demote a permission rule; asks which scope a rule
  lives in or what the effective merged permissions are; wants to compare
  settings.json across scopes; or asks about Claude Code permission-scope
  management. Always previews a write with a dry-run and gets explicit
  confirmation before applying it.
---

# claude-scope

`claude-scope-cli` is a terminal front end for ClaudeScope — it reads and moves
Claude Code permission rules across the settings scopes. It shares the GUI's
backend, so writes are atomic, validated, and backed up the same way.

## When to use this skill

Use it when the user wants to **inspect or change Claude Code permission
rules** — for example:

- "Move my `WebFetch(domain:example.com)` rule from project to user."
- "Which scopes have a `Bash(git push)` rule?"
- "What are my effective permissions right now?"
- "What's the difference between my user and project settings?"
- "Promote this project allow rule to the user scope."

Do **not** invoke this skill for unrelated mentions of the word "settings", for
editor/IDE config, or for application preferences that are not Claude Code
permission rules. It is specifically about Claude Code's permission scopes.

## The scope model

Claude Code merges settings from four scopes. In precedence order, highest
first:

1. `local` — `./.claude/settings.local.json` (project-local, gitignored)
2. `project` — `./.claude/settings.json` (committed with the project)
3. `user-local` — `~/.claude/settings.local.json` (machine-local override)
4. `user` — `~/.claude/settings.json` (machine-global)

A rule lives under `permissions.allow`, `permissions.deny`, or
`permissions.ask`. "Promote" usually means moving toward `user` (broader
reach); "demote" means moving toward `local` (narrower). Confirm the direction
with the user if their wording is ambiguous.

## Finding the binary

Invoke `claude-scope-cli` directly — assume it is on `PATH`. Verify once with
`claude-scope-cli version` before relying on it. If it is not found, tell the
user to install it (`cargo install --path src-tauri --bin claude-scope-cli`
from a ClaudeScope checkout, or add the built binary to `PATH`) and stop —
do not fall back to hand-editing JSON files.

The CLI resolves the project root by walking up from the current directory.
Pass `--project-dir <PATH>` to target a specific project explicitly. All
commands accept it.

## Read commands — safe, run freely

These never write. Prefer `--json` so you can parse the output reliably.

| Goal | Command |
|------|---------|
| Per-scope file state (present / absent / parse error) | `claude-scope-cli scopes --json` |
| Effective merged permissions across all scopes | `claude-scope-cli show --effective --json` |
| One scope's raw contents | `claude-scope-cli show --scope <scope> --json` |
| Flat list of every rule (scope, kind, rule) | `claude-scope-cli list-rules --json` |
| Rules in one scope or kind | `claude-scope-cli list-rules --scope <scope> --kind <kind> --json` |
| Projects discovered on this machine | `claude-scope-cli list-projects --json` |
| Recent settings changes (audit log) | `claude-scope-cli history --json` |

To answer "which scopes have rule X?", run `list-rules --json` and filter the
rows yourself — match the `rule` field exactly.

## Moving a rule — write command, follow the safety protocol

```
claude-scope-cli move "<rule>" --kind <allow|deny|ask> --from <scope> --to <scope> [--dry-run] [--json]
```

The `<rule>` must match the on-disk string **exactly** (e.g.
`"Bash(git status)"`). Look it up with `list-rules` first so you have the
precise string, its kind, and its source scope — do not guess.

**Safety protocol — every move, no exceptions:**

1. **Dry-run first.** Run the command with `--dry-run`. This previews the two
   files that would change and writes nothing.
2. **Show the user the preview.** Report which files would be written and any
   note the preview carries.
3. **Get explicit confirmation.** Wait for the user to clearly approve this
   specific move. A vague earlier "yes" to something else does not count.
4. **Only then apply.** Re-run the exact same command **without** `--dry-run`.

Never run a real `move` (without `--dry-run`) before steps 1–3. If the user
asks you to "just do it", still run the dry-run and show the preview — it is
fast and it is the user's last checkpoint before an atomic file write.

## Exit codes

The CLI exits non-zero and prints `error: …` on any refusal — rule not found in
the source scope, scope path unresolvable, source file missing, `--from` equal
to `--to`, invalid JSON. Treat a non-zero exit as a hard stop: report the
error to the user and diagnose it. Do not retry the same command, and do not
work around a refusal by editing the JSON files directly — the CLI refuses
for a reason.

## Worked example

User: "Move my `WebFetch(domain:docs.rs)` allow rule from project up to user."

1. `claude-scope-cli list-rules --kind allow --json` — confirm the exact rule
   string and that it is in `project`.
2. `claude-scope-cli move "WebFetch(domain:docs.rs)" --kind allow --from project --to user --dry-run --json`
   — preview.
3. Show the user: "This rewrites `./.claude/settings.json` (removes the rule)
   and `~/.claude/settings.json` (adds it). Apply?"
4. On a clear yes: re-run step 2's command without `--dry-run`.
5. Report the result. If the CLI exited non-zero, surface the error instead.
