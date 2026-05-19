# ClaudeScope

Desktop GUI for promoting [Claude Code](https://docs.claude.com/en/docs/claude-code)
permission rules between scopes — without hand-editing JSON.

Claude Code reads settings from JSON files at four scopes (User, User-Local,
Project, Local). Moving a rule from one to another by hand means editing two
files, preserving key order, and not breaking the JSON. ClaudeScope shows
every rule across the four scopes side-by-side and moves them with a click.

## What's here

- **[User guide](./user-guide/scopes.md)** — the scope model, how moves
  work, and how history / undo (#19) lets you walk operations back.
- **[Reference](./reference/rule-syntax.md)** — permission-rule grammar
  ClaudeScope understands, the `claude-scope-cli` subcommand surface, and
  the per-key help shown by tooltips.
- **[Architecture](./architecture/overview.md)** — how the Rust backend
  and TypeScript front end fit together; the atomic-write invariants;
  the on-disk audit log.
- **[Contributing](./contributing.md)** — local dev setup, the test
  suites, and the release workflow.
- **[Security](./security.md)** — how to report a vulnerability and what
  CI catches.

## Install

Pre-built installers for Windows, macOS, and Linux are attached to each
[GitHub Release](https://github.com/bminier/claude-scope/releases).
First launch on Windows or macOS shows an unsigned-binary warning;
signed builds are planned but not yet available — see
[Contributing → Known limitation: unsigned builds](./contributing.md#known-limitation-unsigned-builds).

To run from source, see [Contributing → Develop](./contributing.md#develop).
