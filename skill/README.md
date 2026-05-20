# ClaudeScope skills

Claude Code skills that ship with ClaudeScope. They live here, in the same
repository as the code they wrap, so the skill and its CLI stay in lockstep on
argument shape.

## `claude-scope`

Lets Claude Code view and move Claude Code permission rules between settings
scopes in conversation, by wrapping the `claude-scope-cli` binary. Claude always
previews a write with a dry-run and confirms before applying it.

### Prerequisite

The skill calls `claude-scope-cli`. Install it from a ClaudeScope checkout:

```sh
cargo install --path src-tauri --bin claude-scope-cli
```

That puts `claude-scope-cli` on your Cargo bin path (`~/.cargo/bin`). Make sure
that directory is on your `PATH`.

### Install the skill

Copy (or symlink) the skill directory into your Claude Code skills folder:

```sh
# Copy
cp -r skill/claude-scope ~/.claude/skills/claude-scope

# …or symlink, so it tracks this repo
ln -s "$(pwd)/skill/claude-scope" ~/.claude/skills/claude-scope
```

Claude Code discovers skills under `~/.claude/skills/` automatically — start a
new session and ask it to move a permission rule between scopes.
