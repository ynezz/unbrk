set shell := ["bash", "-cu"]

default:
    @just --list

setup:
    git config core.hooksPath .githooks

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo nextest run --workspace

doc-test:
    cargo test --doc --workspace

deny:
    cargo deny check

typos:
    typos

ci: fmt-check lint deny typos test doc-test

dist:
    cargo build --workspace --release
