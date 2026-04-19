# Security Policy

## Reporting a vulnerability

Please **do not open a public GitHub issue** for security problems.

Report them via [GitHub Private Vulnerability Reporting](https://github.com/bminier/claude-scope/security/advisories/new). It's the fastest channel and keeps the details private until there's a fix ready to announce. You don't need any special access — anyone with a GitHub account can file one.

What to include:

- A clear description of the issue and its impact
- Steps to reproduce (the smallest version that still demonstrates the problem)
- The affected version or commit
- Any suggested fix or mitigation, if you have one

I'll try to acknowledge reports within a few business days and give you a sense of the expected timeline from there.

## Supported versions

ClaudeScope is pre-1.0. Only the most recent tagged release gets security fixes — and until `v0.1.0` actually ships, that means the latest commit on the `dev` branch. Older tags (once they exist) are archived as-is.

## What the CI catches

These checks run on every push to `dev` and every PR against it (see [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)):

- **`cargo-audit`** — gates on advisories in the [RustSec advisory DB](https://rustsec.org) for any Rust dep in `src-tauri/Cargo.lock`
- **`npm audit --package-lock-only --audit-level=high`** — gates on high+ severity vulnerabilities in the npm tree, reading `package-lock.json` directly (no `node_modules` install, no lifecycle scripts)
- **`detect-private-key`** pre-commit hook — rejects commits containing common private-key headers
- **Dependabot** — opens weekly grouped PRs for Cargo, npm, and GitHub Actions minor/patch bumps so known-patched vulns don't linger

Anything `cargo-audit` or `npm audit` surfaces after a clean merge is a signal to ship a patch PR. The advisory DBs update continuously, so a dep that was clean yesterday can light up the `security` job tomorrow with no code change on our end.
