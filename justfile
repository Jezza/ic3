# CI-equivalent checks. Run before every `jj describe`.
check: fmt clippy test doc

fmt:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
