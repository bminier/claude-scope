#!/usr/bin/env python3
"""Seed a throwaway home + project tree for sandbox-mode dogfooding (#66).

Usage:
    python scripts/scratch-home.py [<dest>]

`<dest>` defaults to a temp dir if omitted. Creates four `.claude/settings*`
files — one per recognized scope — populated with realistic-looking
permission rules and `env` / `hooks` blocks so every UI feature has data to
exercise.

Layout produced:

    <dest>/
        .claude/settings.json           # user
        .claude/settings.local.json     # user-local
        project/
            .git/                       # marks the project root for scope discovery
            .claude/settings.json       # project
            .claude/settings.local.json # local

Then prints the recommended launch command for ClaudeScope:

    claude-scope --home <dest> --project <dest>/project

The script is idempotent — re-running it overwrites the seeded files but
preserves anything else under `<dest>` (your hand-edits, .bak trail from
the last run, etc.). Pass `--force` to wipe `<dest>/.claude` and
`<dest>/project/.claude` before reseeding.

Why Python: Brian's global preference is portable scripts so the same file
runs on Windows / macOS / Linux without dragging shell quirks into the repo.
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
import tempfile
from pathlib import Path

# Default seed contents. Picked to cover every visible UI surface:
#   - allow / deny / ask permission rules across all four scopes
#   - duplicate rules that show up in multiple scopes (combined-tooltip test)
#   - top-level `env` (DeepMerge) so move-key flow has data
#   - top-level `hooks` (Replace) so the override-only note appears
#   - top-level `theme` (Replace) for the simple scalar case
#   - one rule whose shape will trip the lint badge so you can see the warn UI


USER_SETTINGS = {
    "permissions": {
        "allow": [
            "Bash(git status)",
            "Bash(git log)",
            "Read(**)",
        ],
        "deny": [
            "WebFetch(domain:evil.example)",
        ],
        "ask": [],
    },
    "theme": "dark",
    "env": {
        "PATH": "/usr/local/bin:/usr/bin",
        "EDITOR": "vim",
    },
}

USER_LOCAL_SETTINGS = {
    "permissions": {
        "allow": [
            "Bash(npm test)",
            "Bash(git status)",
        ],
        "deny": [],
        "ask": [],
    },
}

PROJECT_SETTINGS = {
    "permissions": {
        "allow": [
            "Bash(cargo test)",
            "Bash(cargo build)",
            "WebFetch(domain:docs.rs)",
            "Read(./src/**)",
        ],
        "deny": [
            "Bash(rm -rf *)",
        ],
        "ask": [
            "Bash(git push *)",
        ],
    },
    "hooks": {
        "PreToolUse": [
            {"matcher": "Bash", "command": "echo project hook"}
        ]
    },
    "env": {
        "RUST_BACKTRACE": "1",
    },
}

LOCAL_SETTINGS = {
    "permissions": {
        "allow": [
            "Bash(cargo run)",
            "lookslikegarbage",  # exercises the lint badge
        ],
        "deny": [],
        "ask": [],
    },
    "env": {
        "DEV_OVERRIDE": "1",
    },
}


def write_json(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(payload, indent=2) + "\n",
        encoding="utf-8",
    )


def seed(dest: Path, force: bool) -> None:
    user_claude = dest / ".claude"
    project_dir = dest / "project"
    project_claude = project_dir / ".claude"

    if force:
        for d in (user_claude, project_claude):
            if d.exists():
                shutil.rmtree(d)

    write_json(user_claude / "settings.json", USER_SETTINGS)
    write_json(user_claude / "settings.local.json", USER_LOCAL_SETTINGS)
    write_json(project_claude / "settings.json", PROJECT_SETTINGS)
    write_json(project_claude / "settings.local.json", LOCAL_SETTINGS)

    # find_project_root prefers a `.git` ancestor over a `.claude` ancestor,
    # so we plant a stub `.git` directory in the project dir to make sure the
    # scope walker locks onto `<dest>/project` and not some ancestor that
    # happens to have its own `.claude/`.
    git_marker = project_dir / ".git"
    git_marker.mkdir(parents=True, exist_ok=True)
    # An empty .git directory satisfies the existence check; a regular file
    # named HEAD avoids confusing other tools that scan for git repos.
    (git_marker / "HEAD").write_text("ref: refs/heads/main\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", maxsplit=1)[0])
    parser.add_argument(
        "dest",
        nargs="?",
        default=None,
        help="Destination directory (default: a fresh temp dir).",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Wipe existing .claude/ trees under dest before reseeding.",
    )
    args = parser.parse_args()

    if args.dest:
        dest = Path(args.dest).expanduser().resolve()
        dest.mkdir(parents=True, exist_ok=True)
    else:
        dest = Path(tempfile.mkdtemp(prefix="claude-scope-scratch-"))

    seed(dest, force=args.force)

    project = dest / "project"
    print(f"Seeded sandbox at: {dest}")
    print()
    # Env vars are the recommended path for `npm run tauri dev` because the
    # npm → tauri-cli → cargo chain mangles `--` separators and tauri-cli
    # forwards CLI args as cargo flags rather than binary flags. Env vars
    # bypass that chain entirely. CLI flags work fine against a built binary.
    print("Launch ClaudeScope against the scratch tree with:")
    print()
    if sys.platform == "win32":
        print("    # Dev build (PowerShell):")
        print(f'    $env:CLAUDE_SCOPE_HOME = "{dest}"')
        print(f'    $env:CLAUDE_SCOPE_PROJECT = "{project}"')
        print("    npm run tauri dev")
        print()
        # cmd.exe: wrap the whole NAME=VALUE in double quotes so spaces in
        # the dest path don't get split into separate tokens. The `set
        # "VAR=value"` form is the canonical cmd-safe spelling — the outer
        # quotes are stripped by `set` and never end up in the variable.
        print("    # Dev build (cmd.exe):")
        print(f'    set "CLAUDE_SCOPE_HOME={dest}"')
        print(f'    set "CLAUDE_SCOPE_PROJECT={project}"')
        print("    npm run tauri dev")
    else:
        print("    # Dev build:")
        print(
            f"    CLAUDE_SCOPE_HOME='{dest}' "
            f"CLAUDE_SCOPE_PROJECT='{project}' npm run tauri dev"
        )
    print()
    # Quote the path arguments for the same reason — a built binary call
    # `claude-scope --home C:\path with spaces\X` would otherwise split.
    print("    # Built binary (CLI flags):")
    if sys.platform == "win32":
        print(f'    claude-scope --home "{dest}" --project "{project}"')
    else:
        print(f"    claude-scope --home '{dest}' --project '{project}'")
    return 0


if __name__ == "__main__":
    sys.exit(main())
