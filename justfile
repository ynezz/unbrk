set shell := ["bash", "-cu"]

default:
    @just --list

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

ci: fmt-check lint test doc-test deny

dist:
    cargo build --workspace --release
