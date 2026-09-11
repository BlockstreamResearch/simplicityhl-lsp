lockfile := "existing"

# Run the full local CI mirror: all toolchains, lint, docs, fmt check.
ci: test-all lint docs fmt-check

# Test on every toolchain this repo pins (stable, nightly, MSRV).
test-all: (test "stable") (test "nightly") (test "msrv")

# Test on a single toolchain: `just test stable`.
test toolchain:
    cargo rbmt --lock-file {{lockfile}} test --toolchain {{toolchain}}

# Workspace-wide clippy bundle rbmt runs (stricter than a bare `cargo clippy`).
lint:
    cargo rbmt --lock-file {{lockfile}} lint

# Doc build (stable) + docs.rs-style build (nightly).
docs:
    cargo rbmt --lock-file {{lockfile}} docs
    cargo rbmt --lock-file {{lockfile}} docsrs

# Check formatting without modifying files.
fmt-check:
    cargo rbmt fmt --check

# Reformat the tree.
fmt:
    cargo rbmt fmt
