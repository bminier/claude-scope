# Security

## Reporting a vulnerability

Please **do not open a public GitHub issue** for security problems.

Report them via
[GitHub Private Vulnerability Reporting](https://github.com/bminier/claude-scope/security/advisories/new).
It's the fastest channel and keeps the details private until there's
a fix ready to announce. You don't need any special access — anyone
with a GitHub account can file one.

What to include:

- A clear description of the issue and its impact.
- Steps to reproduce (the smallest version that still demonstrates
  the problem).
- The affected version or commit.
- Any suggested fix or mitigation, if you have one.

Acknowledgement: within a few business days, with a sense of the
expected timeline from there.

## Supported versions

ClaudeScope is pre-1.0. Only the most recent tagged release gets
security fixes — and until `v0.1.0` actually ships, that means the
latest commit on the `dev` branch. Older tags (once they exist) are
archived as-is.

## What CI catches

These checks run on every push to `dev` and every PR against it (see
[`.github/workflows/ci.yml`](https://github.com/bminier/claude-scope/blob/dev/.github/workflows/ci.yml)):

- **`cargo-audit`** — gates on advisories in the
  [RustSec advisory DB](https://rustsec.org) for any Rust dep in
  `src-tauri/Cargo.lock`.
- **`npm audit --package-lock-only --audit-level=high`** — gates on
  high+ severity vulnerabilities in the npm tree, reading
  `package-lock.json` directly (no `node_modules` install, no
  lifecycle scripts).
- **`detect-private-key`** pre-commit hook — rejects commits
  containing common private-key headers.
- **Dependabot** — opens weekly grouped PRs for Cargo, npm, and
  GitHub Actions minor/patch bumps so known-patched vulns don't
  linger.

Anything `cargo-audit` or `npm audit` surfaces after a clean merge is
a signal to ship a patch PR. The advisory DBs update continuously,
so a dep that was clean yesterday can light up the `security` job
tomorrow with no code change on our end.

## Privacy

- ClaudeScope is a **local-only** tool. It reads and writes files on
  the user's machine; no network requests in normal operation.
- The audit log (`~/.claude/claude-scope/audit.jsonl`) is as sensitive
  as the settings files themselves — it contains full rule text and
  project paths. Don't ship it anywhere. The on-disk format is
  documented at [Architecture → Audit log](./architecture/audit-log.md).
- The docs site (this one) has **no analytics**. No tracking
  pixels, no third-party scripts.

## Threat model

ClaudeScope is a single-user desktop tool. The user running the app
is the only legitimate operator. Within that frame, two threat
surfaces are explicitly defended:

**Hostile audit-log entries.** An attacker who can append a line to
`~/.claude/claude-scope/audit.jsonl` (cloud-sync collision from a
compromised peer, a previously-compromised tool with user-scope
write, a malicious package postinstall, a shared workstation) should
not be able to escalate that capability. The audit log drives
`apply_restore_plan` to write to files at paths recorded in the log
itself — without defenses, one well-formed line could direct
ClaudeScope to write attacker-controlled JSON to any path the user
can write to. The
[Architecture → Audit log § Security invariants](./architecture/audit-log.md#security-invariants)
section documents the three gates (path allowlist, degraded-log
refusal, tail-ID stalecheck) that enforce containment. The path
allowlist is the load-bearing control: every `Side.file_path` is
validated against the four legitimate `ScopePaths` for its
`project_dir` before any write happens.

**Hand-edited settings files.** Users (or other tools) may edit
`settings.json` outside ClaudeScope at any time. The atomic-write
path captures a `FileStamp` at load time and refuses the save when
the stamp differs at persist time — the user's external edit
survives. See
[Architecture → Atomic writes](./architecture/atomic-writes.md) for
the contract.

Out of scope:

- Multi-user systems with adversarial co-users. The threat model
  assumes a single trusted user; on shared workstations, OS-level
  ACLs are the right layer.
- Resource-exhaustion attacks. A user who can write large amounts
  of data into `audit.jsonl` can fill the disk; that's a property
  of any append-only log, not a ClaudeScope-specific issue.
- Tampering with the bundled binary. Binary integrity is the
  installer / OS package manager's responsibility.
