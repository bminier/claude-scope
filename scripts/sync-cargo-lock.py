#!/usr/bin/env python3
"""Sync the local-package version in src-tauri/Cargo.lock with src-tauri/Cargo.toml.

release-please's TOML updater bumps `[package].version` in `Cargo.toml`, but
`Cargo.lock` carries an independent copy of that version under the
`[[package]] name = "claude-scope"` entry. CI runs `cargo --locked`, so a
stale lockfile fails the release-please PR's CI run.

This script does a surgical regex update of just that one version line. We
deliberately avoid invoking cargo: a full `cargo update` could touch the
dependency tree, which would conflate a release version bump with a
dependency churn we don't want bundled into the release-please PR.

Side effect: a CRLF Cargo.lock (e.g. a Windows clone with
`core.autocrlf=true`) gets normalized to LF on write, matching the
repo's LF-only convention. CI's checkout is already LF, so this is a
no-op there.

Usage:
    python scripts/sync-cargo-lock.py

Run from the repo root. Exits non-zero if the [package].version can't be
parsed from Cargo.toml or if Cargo.lock has zero or multiple matching
[[package]] entries (either case is unexpected and a silent skip would mask
a real problem).
"""
from __future__ import annotations

import sys

if sys.version_info < (3, 11):
    sys.exit(
        "sync-cargo-lock requires Python 3.11+ (uses stdlib tomllib); "
        f"got {sys.version_info.major}.{sys.version_info.minor}. "
        "CI pins Python 3.12 (.github/workflows/ci.yml, release-please.yml)."
    )

import re
import tomllib
from pathlib import Path

CARGO_TOML = Path("src-tauri/Cargo.toml")
CARGO_LOCK = Path("src-tauri/Cargo.lock")
PACKAGE_NAME = "claude-scope"


def read_target_version() -> str:
    with CARGO_TOML.open("rb") as f:
        data = tomllib.load(f)
    try:
        return data["package"]["version"]
    except KeyError:
        sys.exit(f"could not find [package].version in {CARGO_TOML}")


def update_lock(version: str) -> bool:
    # Read/write in binary so platform newline translation can't sneak CRLF
    # into the lockfile when this script runs on Windows as a manual fallback.
    # The repo is LF-only and pre-commit's mixed-line-ending hook would fix
    # it, but better not to dirty the file in the first place.
    raw = CARGO_LOCK.read_bytes()
    # A Windows clone with core.autocrlf=true can put CRLF in Cargo.lock,
    # which defeats the LF-literal regex below. The repo is LF-only, so
    # normalize on read and let the byte-mode write below restore that.
    text = raw.decode("utf-8").replace("\r\n", "\n")
    pattern = re.compile(
        r'(\[\[package\]\]\nname\s*=\s*"' + re.escape(PACKAGE_NAME) + r'"\nversion\s*=\s*")[^"]*(")'
    )
    matches = list(pattern.finditer(text))
    if len(matches) == 0:
        sys.exit(f"no [[package]] entry for {PACKAGE_NAME!r} in {CARGO_LOCK}")
    if len(matches) > 1:
        sys.exit(
            f"unexpected: {len(matches)} [[package]] entries for {PACKAGE_NAME!r} in {CARGO_LOCK}"
        )
    new_text = pattern.sub(rf"\g<1>{version}\g<2>", text)
    if new_text == text:
        return False
    CARGO_LOCK.write_bytes(new_text.encode("utf-8"))
    return True


def main() -> None:
    version = read_target_version()
    changed = update_lock(version)
    if changed:
        print(f"{CARGO_LOCK}: synced {PACKAGE_NAME} -> {version}")
    else:
        print(f"{CARGO_LOCK}: already at {version}")


if __name__ == "__main__":
    main()
