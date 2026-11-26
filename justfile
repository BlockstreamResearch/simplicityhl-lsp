lockfile := "existing"

# Run the local CI checks: tests, lint, docs, fmt check.
ci: test-all lint docs fmt-check

# Install the toolchains this repo pins (stable, nightly, MSRV).
toolchains:
    cargo rbmt toolchains

# Test on stable and MSRV, as CI does.
test-all: (test "stable") (test "msrv")

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
