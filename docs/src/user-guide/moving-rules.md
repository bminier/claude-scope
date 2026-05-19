# Moving rules

Every per-rule action goes through the same primitive: pick a rule,
pick a destination scope (or `allow` / `deny` / `ask` if you're
reclassifying), confirm the diff, write. The destination file is
written **first**, then the source file is rewritten without the rule.
If the source rewrite fails, the destination is rolled back — the rule
never ends up in two places, never goes missing. See
[Architecture → Atomic writes](../architecture/atomic-writes.md) for
the invariants.

## From the GUI

Each rule row carries move buttons for every other scope (`→ User`,
`→ Project`, etc.). Right-click the row for the full menu:

- **Move to** → submenu with scopes + a "Project…" submenu listing
  every discovered Claude project (so a rule can land directly in
  another repo's `.claude/settings.json` without switching the loaded
  project).
- **Change kind** → switch `allow` ↔ `deny` ↔ `ask` within the same scope.
- **Copy / Paste** — clipboard plumbing for rules and rule lists.
- **Delete** — drops the rule from its scope.

Every action that writes shows a **diff preview modal** first. Apply
the move with `Enter`, cancel with `Escape`. The trigger button gets
focus back when the modal closes.

## Search

Press `/` anywhere to focus the search input. Filter is
case-insensitive substring across rules in every scope. Match counts
appear per group (`allow (1/3)` = one match out of three). Escape
clears the filter.

## Tool grouping

When two or more rules share a tool prefix (`Bash(...)`,
`handoff(...)`), they fold under a synthetic `Tool ▸ (N)` node. The
threshold defaults to 2; future work tracked in
[#115](https://github.com/bminier/claude-scope/issues/115) will
surface it as a Settings option.

## Sandbox mode

Two ways to dogfood a build against a throwaway home directory rather
than your real `~/.claude/`:

```sh
# Env vars (works with `npm run tauri dev`):
CLAUDE_SCOPE_HOME=/tmp/scratch CLAUDE_SCOPE_PROJECT=/tmp/scratch/project npm run tauri dev

# CLI flags (works against a built binary):
claude-scope --home /tmp/scratch --project /tmp/scratch/project
```

When either override is active, a yellow **Sandbox mode** banner sits
under the toolbar so you can't forget you're in scratch mode. The
audit log, recent-projects LRU, and preferences write to the scratch
home too — running ClaudeScope under `--home` cannot pollute the real
config dir.

`scripts/scratch-home.py` seeds a fresh scratch dir with realistic
example settings:

```sh
python scripts/scratch-home.py            # auto temp dir
python scripts/scratch-home.py /tmp/sb    # explicit, idempotent
python scripts/scratch-home.py /tmp/sb --force   # wipe + reseed
```
