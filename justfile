# modulex build tasks.
# `just check` is the gate: it is what .githooks/pre-push and CI both run.

# Full validation: format, lint (zero warnings), tests.
check: fmt-check lint test

build:
    cargo build

release:
    cargo build --release

test:
    cargo test --all

lint:
    cargo clippy --all-targets --all-features -- -D warnings

fmt:
    cargo fmt

fmt-check:
    cargo fmt -- --check

# Run the CLI against the example config (dry run; no side effects).
demo:
    MODULEX_CONFIG=modulex.toml.example cargo run -p modulex-cli -- run morning --dry-run

install-hooks:
    git config core.hooksPath .githooks
    @echo "push hooks installed (.githooks/pre-push)"

clean:
    cargo clean
