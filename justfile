set shell := ["bash", "-cu"]
set windows-shell := ["cmd.exe", "/c"]

default:
    @just --list

# Install JS dependencies
install:
    npm install

# Run the Tauri app in dev mode
dev:
    npm run tauri dev

# Run only the Vite dev server (no Tauri shell)
web:
    npm run dev

# Production build of the Tauri app
build:
    npm run tauri build

# Lint JS/TS with Biome
lint:
    npm run lint

# Format JS/TS with Biome
fmt:
    npm run format

# Rust: format, clippy, test
rs-fmt:
    cargo fmt --manifest-path src-tauri/Cargo.toml

rs-lint:
    cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings

rs-test:
    cargo test --manifest-path src-tauri/Cargo.toml

# JS/TS tests (runner not yet configured)
test:
    @echo "No JS test runner configured yet" && exit 1

# Run all checks (JS lint + Rust fmt/clippy/test)
check: lint rs-lint rs-test
    cargo fmt --manifest-path src-tauri/Cargo.toml -- --check

# Clean build artifacts
clean:
    rm -rf dist
    cargo clean --manifest-path src-tauri/Cargo.toml
