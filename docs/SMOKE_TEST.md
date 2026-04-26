# Smoke test checklist

A short manual pass to run before merging anything that touches the UI.
This is the stand-in for automated UI tests (see #52); when those land it
becomes the redundant fallback. Should fit in **~5 minutes** for a focused
reviewer.

## Setup

1. Fresh checkout of the branch under review, dependencies installed:
   ```sh
   npm install
   ```
2. Point at a throwaway `.claude/` to avoid mutating your real config.
   Pick any temp directory you don't mind deleting; the examples use
   `/tmp/cs-smoke` on macOS / Linux and `$env:TEMP\cs-smoke` on Windows.

   **macOS / Linux (bash):**
   ```sh
   mkdir -p /tmp/cs-smoke/.claude
   echo '{"permissions":{"allow":["Bash(ls *)","Bash(git status)"]}}' \
     > /tmp/cs-smoke/.claude/settings.json
   echo '{"permissions":{"allow":["WebFetch(domain:example.com)"]}}' \
     > /tmp/cs-smoke/.claude/settings.local.json
   ```

   **Windows (PowerShell):**
   ```powershell
   # -Encoding utf8 is critical: PowerShell 5.1 (Windows 11 default)
   # writes UTF-16 LE w/ BOM otherwise, which Rust's serde_json
   # (via fs::read_to_string) cannot parse.
   $proj = Join-Path $env:TEMP 'cs-smoke'
   New-Item -ItemType Directory -Force "$proj\.claude" | Out-Null
   '{"permissions":{"allow":["Bash(ls *)","Bash(git status)"]}}' `
     | Set-Content -Encoding utf8 "$proj\.claude\settings.json"
   '{"permissions":{"allow":["WebFetch(domain:example.com)"]}}' `
     | Set-Content -Encoding utf8 "$proj\.claude\settings.local.json"
   ```
3. Launch the dev build:
   ```sh
   npm run tauri dev
   ```
4. Click **Open project…** → pick the throwaway dir from step 2.

> ⚠ **User-scope safety.** Moves targeting the **User** or
> **User-Local** columns write to your real `~/.claude/` — the throwaway
> project only isolates the Project / Local scopes. Either back those
> files up first (e.g. `cp ~/.claude/settings.json{,.smoke.bak}`), or
> after launch open **Settings** and hide the User + User-Local columns
> so you can only move between Project ↔ Local. The app's own one-shot
> `.bak` covers the *first* mutation per file per session, but not
> subsequent ones in the same run.

> Reminder: a real `~/.claude/settings.json` will be read regardless. If
> you want the User column visibly populated, drop a couple of throwaway
> entries there; otherwise expect User / User-Local to show
> `(file not present)`.

## Golden path

- [ ] **All four scope columns render.** User / User-Local / Project / Local
      appear left-to-right; the "Combined permissions" panel shows the union
      across scopes on top, with the precedence-aware-evaluation caveat in
      its subtitle.
- [ ] **Click-to-move a permission rule.** From any populated scope, click a
      `→ <Scope>` button on a rule → diff modal opens with before/after for
      both sides → click **Apply** → modal closes, rule disappears from
      source column and appears in destination column, both files on disk
      reflect the change.
- [ ] **Cancel a move.** Trigger another move, hit **Esc** (or click
      Cancel). Modal closes; no file changes; focus returns to the original
      `→` button.
- [ ] **Move a top-level "Other settings" key.** If a populated scope has
      an `env` / `hooks` / `theme` entry, use the per-key `→ <Scope>`
      button. Same diff/apply flow; the key moves and JSON round-trips
      cleanly.
- [ ] **Toggle scope visibility.** Click **Settings** to open the dialog
      → uncheck a scope. Column disappears immediately. Re-check it;
      column returns. Close and re-open the dialog; checkbox state is
      preserved.
- [ ] **Search filter.** Press `/` anywhere → search input takes focus.
      Type a substring → group headers switch to `allow (m/n)` /
      `deny (m/n)` / `ask (m/n)` and only matching rules render. Press
      **Esc** in the input → filter clears.
- [ ] **Lint-warning chip.** Find (or temporarily add) a malformed rule
      like `Bash()` (empty args) or `WebFetch(example.com)` (missing
      `domain:` prefix). The ⚠ chip appears. Tab to it — focus ring
      visible. Activate it (Enter or click) → popover opens with the
      reason. Press **Esc** → popover closes, focus returns to the chip.

## Edge cases

- [ ] **Empty scope.** Delete the Local file, then click **Reload**.
      The Local column header shows `(file not present)`.
      ```sh
      # macOS / Linux
      rm /tmp/cs-smoke/.claude/settings.local.json
      ```
      ```powershell
      # Windows
      Remove-Item "$env:TEMP\cs-smoke\.claude\settings.local.json"
      ```
- [ ] **Parse-error scope.** Overwrite the Project file with invalid
      JSON, then click **Reload**. The affected column header shows
      `Parse error: <message>` instead of rule rows; other columns stay
      functional.
      ```sh
      # macOS / Linux
      printf '{ invalid json' > /tmp/cs-smoke/.claude/settings.json
      ```
      ```powershell
      # Windows — -Encoding utf8 keeps the parse failure about JSON, not BOM.
      Set-Content -Encoding utf8 -Value '{ invalid json' `
        "$env:TEMP\cs-smoke\.claude\settings.json"
      ```
- [ ] **Busy-state idempotency.** Trigger a move and rapidly click a
      second `→` button before the modal opens. Only one diff modal
      appears; the second click is a no-op.
- [ ] **External-edit auto-reload.** With the app open, edit one of the
      JSON files in another editor (e.g. add a rule) and save. Within
      ~1 s the watcher fires and the UI re-renders to match disk — no
      manual Reload needed.
- [ ] **Last-column guard.** In the Settings dialog, uncheck scopes one by
      one. The final remaining checkbox refuses to uncheck (the grid would
      otherwise render empty with no recovery from inside the dialog).

## Side-effect verification

After exercising the moves above, in the throwaway `.claude/` dir:

- [ ] **Backup exists.** Each settings file that was modified during the
      session has a sibling `<file>.bak` containing the **pre-session**
      contents. (Subsequent writes do **not** overwrite the `.bak` — it's
      one-shot per file per session.)
- [ ] **JSON round-trip is valid.** `python -m json.tool < settings.json`
      (and `.local.json`) exits 0.
- [ ] **Mtime matches the move time.** `stat` (or PowerShell's
      `Get-Item`) on the modified file shows a recent modification time.
- [ ] **No leftover tempfiles.** No `*.tmp*` / `.settings.json.swp`-style
      siblings — the atomic write should rename the temp into place and
      leave nothing behind on success.

## When you're done

Tear down the throwaway dir, and (if you backed up `~/.claude/`)
restore it:

```sh
# macOS / Linux
rm -rf /tmp/cs-smoke
mv ~/.claude/settings.json.smoke.bak ~/.claude/settings.json   # if backed up
```

```powershell
# Windows
Remove-Item -Recurse -Force (Join-Path $env:TEMP 'cs-smoke')
```

If anything in this list misbehaved, link the broken step in the PR
description rather than merging through it. If the checklist itself
feels stale (e.g. a feature shipped that isn't covered), edit this doc
in the same PR — it's meant to track reality, not the original spec.
