set shell := ["bash", "-cu"]
set windows-shell := ["cmd.exe", "/c"]

# Recipe convention: bminier/home #92 / claude-scope #160.
# `run` is intentionally omitted — `dev` is the canonical run for a Tauri app.

default:
    @just --list

# Install all dependencies.
setup:
    npm install

# Run the Tauri app in dev mode (HMR).
dev:
    npm run tauri dev

# Vite dev server only (no Tauri shell) — handy for pure-UI work.
web:
    npm run dev

# Production build of the Tauri app.
build:
    npm run tauri build

# --- lint / fmt / test ---------------------------------------------------

# Read-only JS/TS static analysis: biome (lint) + tsc --noEmit
# (typecheck). Both must pass for `just check` to clear. tsc catches
# strict-type regressions that biome's syntactic lint doesn't — e.g.
# missing fields on `PropsOverrides`, return-type drift, unhandled
# null branches. Vite's build runs tsc too, but kicking it in via the
# lint pipeline closes the gap where vitest (which doesn't strict-
# typecheck) passes locally and CI's `npm run build` fails late.
js-lint:
    npm run lint
    npm run typecheck

rs-lint:
    cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
    cargo fmt --manifest-path src-tauri/Cargo.toml -- --check

lint: js-lint rs-lint

js-fmt:
    npm run format

rs-fmt:
    cargo fmt --manifest-path src-tauri/Cargo.toml

fmt: js-fmt rs-fmt
alias format := fmt

js-test:
    npm run test:unit

rs-test:
    cargo test --manifest-path src-tauri/Cargo.toml

test: js-test rs-test

check: lint test

# --- misc ----------------------------------------------------------------

docs:
    mdbook build docs

clean:
    rm -rf dist docs/book
    cargo clean --manifest-path src-tauri/Cargo.toml
