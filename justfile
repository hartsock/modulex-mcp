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

# Tier-3 live-contract tests (#36): verify mock fixtures against REAL
# tools on THIS host. Opt-in and host-dependent by design — never part of
# `just check` or PR CI. Tools absent on the host skip with a notice.
live-test:
    MODULEX_LIVE_TESTS=1 cargo test -p modulex-core --test live_contract -- --nocapture

# Local coverage with an HTML report (opens target/llvm-cov/html/index.html).
# Excludes binary/pyo3 entrypoints (integration-tested, not unit-tested) so
# the number reflects the library logic the tests actually cover.
cov:
    cargo llvm-cov --all-features --workspace \
        --ignore-filename-regex '(main\.rs|modulex-py/src/lib\.rs)' --html
    @echo "report: target/llvm-cov/html/index.html"

# CI-mode coverage gate: same scope as `cov`, fails under the floor. Mirrors
# the `coverage` job in .github/workflows/ci.yml — keep the floor in sync.
cov-ci:
    cargo llvm-cov --all-features --workspace \
        --ignore-filename-regex '(main\.rs|modulex-py/src/lib\.rs)' \
        --fail-under-lines 80

# Run the CLI against the example config (dry run; no side effects).
demo:
    MODULEX_CONFIG=modulex.toml.example cargo run -p modulex-cli -- run morning --dry-run

install-hooks:
    git config core.hooksPath .githooks
    @echo "push hooks installed (.githooks/pre-push)"

clean:
    cargo clean
