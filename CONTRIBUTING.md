# Contributing

`unbrk` is a Rust-first CLI for automating Nokia Valyrian UART recovery. The
repository is still in early bootstrap, so changes should keep the codebase
small, explicit, and easy to verify.

## Development Workflow

Use the shared `justfile` targets so humans, CI, and agents run the same
commands:

- `just fmt`: format the workspace with `cargo fmt --all`
- `just lint`: run `cargo clippy --workspace --all-targets -- -D warnings`
- `just test`: run `cargo nextest run --workspace`
- `just doc-test`: run `cargo test --doc --workspace`
- `just deny`: run `cargo deny check`
- `just ci`: run the full local quality gate sequence
- `just dist`: build release artifacts with `cargo build --workspace --release`

If `just`, `cargo-nextest`, or `cargo-deny` are missing locally, install them
before sending a change for review.

## Commit And PR Conventions

- Use Conventional Commits for all commit subjects.
- Keep changes scoped to one logical unit of work when possible.
- Reference the relevant Beads issue ID in commits and PR descriptions.
- Include validation notes in the PR description for every non-trivial change.
- Update docs and fixtures in the same PR as the code they describe.

## Pull Requests

Every pull request should include:

- a short problem statement
- the reason the change is needed
- the user-visible or operator-visible impact
- the commands you ran locally
- any follow-up work that remains intentionally out of scope

Keep PR titles human-readable so they map cleanly to future changelog entries.

## Fixtures And Logs

Transcript-derived fixtures belong under `tests/fixtures/valyrian/` once that
test layout exists. When adding or updating fixtures:

- preserve the original raw transcript source when possible
- document how the fixture was captured
- keep prompt patterns conservative when a capture shows variation
- update any docs that depend on the captured behavior

Do not hand-edit binary fixtures unless the change is intentional and
documented.

## Integration Tests

Hardware validation and transport-agnostic integration tests are separate
concerns:

- use simulated transports and transcript fixtures for repeatable tests
- reserve real-device runs for explicitly documented validation work
- capture transcripts from hardware runs when behavior changes

Until the dedicated testing docs land, use `docs/initial-plan.md`,
`docs/valyrian-uart-recovery-protocol.md`, and `tools/valyrian_uart_recovery.py`
as the current references for expected behavior.
