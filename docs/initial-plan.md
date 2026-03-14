# unbrk Initial Plan

Date: 2026-03-14
Status: Draft approved for implementation planning

## Summary

`unbrk` will be a Rust-first CLI that targets Linux, macOS, and Windows while
automating the documented UART recovery flow for Airoha AN7581 first.
Linux should be treated as the first hardware-validated host, with macOS and
Windows treated as portability targets until they have seen real-device runs.

The design should leave room for later Airoha board support, but v1 should be
explicitly grounded in the AN7581 protocol that has already been documented
and observed.

The first release will focus on getting a board from power-off through a full
recovery to a RAM-resident U-Boot prompt, and then through an explicitly
requested persistent bootloader reinstall, by:

1. Opening the serial console.
2. Detecting recovery prompts.
3. Sending `x` at the right time.
4. Transferring the preloader over XMODEM.
5. Detecting the second-stage prompt.
6. Transferring the BL31 + U-Boot FIP over XMODEM.
7. Waiting for a live RAM-resident U-Boot prompt.
8. Optionally running the documented AN7581 U-Boot flash sequence over the
   same UART session when the operator explicitly asks for persistent flashing.
9. Observing post-flash reset activity, or handing off to an interactive
   console or machine-readable status stream at an explicit stop point.

The existing Python helper demonstrates that the full AN7581 workflow is
already practical as one continuous UART session. The plan should therefore
keep the persistent flash phase in the same command, but it should require
explicit user intent before erasing flash instead of treating destructive
rewrites as the default `recover` behavior.

This automates the UART conversation after the board is placed into recovery
mode; physical power and reset sequencing remain the job of an operator or an
external test harness.

## Inputs And Constraints

This plan is based on:

- `docs/an7581-uart-recovery-protocol.md`
- `docs/an7581-end-to-end.log`
- `tools/an7581_uart_recovery.py`
- local board XMODEM recovery notes
- local U-Boot XMODEM recovery notes
- local `mtk_uartboot` as a style and scope reference, not as a protocol match

Key protocol facts from the recovery docs:

- The recovery flow is prompt-driven over UART.
- The entry condition is board-specific: on AN7581 the operator starts with
  the board powered off, then holds the middle reset button during power-on to
  enter BootROM recovery mode.
- The documented UART settings are `115200` baud, `8N1`, with no flow control.
- The boot path expects the user to press `x` at specific prompts.
- The first prompt contains `Press x`.
- The second prompt is `Press x to load BL31 + U-Boot FIP`.
- Each stage advances when the host sends one literal ASCII `x` byte without a
  trailing newline.
- File transfer uses standard XMODEM with CRC.
- Each transfer starts only after XMODEM-CRC readiness appears as repeated `C`
  bytes.
- The first upload is a preloader binary.
- The second upload is a BL31 + U-Boot FIP image.
- The second-stage prompt contains the text of the first prompt, so matching
  has to be state-scoped rather than one global `Press x` search.
- The board can move to the next prompt before the sender observes a clean
  final XMODEM `EOT` acknowledgement, so console progress is more authoritative
  than a pristine sender-side return value.
- After the second upload, the console emits substantial boot noise before
  reaching a usable `AN7581>` U-Boot prompt.
- The helper's end-to-end flow continues from that prompt by reading
  `loadaddr`, running `mmc erase 0 0x800`, sending the preloader and FIP with
  `loadx`, writing them to `0x4/0xfc` and `0x100/0x700`, then issuing `reset`.
- Each `loadx` is verified via U-Boot's `filesize` variable, which on this
  board is emitted as hexadecimal without a `0x` prefix.
- The UART stream does not, by itself, prove the board's power or button state.

This means `unbrk` should be designed around a serial-console state machine
plus XMODEM transport, not around the Mediatek BootROM protocol used by
`mtk_uartboot`.

## Approved Product Decisions

- Language: Rust for v1.
- v1 scope: automate Airoha AN7581 recovery from BootROM entry to a usable
  RAM-resident U-Boot prompt by default, and support the documented persistent
  U-Boot flash sequence as an explicit opt-in phase.
- Release scope: GitHub Releases, checksums, provenance/attestations, and a
  crates.io / `cargo install` path once the public CLI contract is ready.
- Deferred scope: Homebrew, Scoop, Winget, and generalized multi-board flash
  commands beyond the initial AN7581 flow.

## Goals

- Provide one CLI with shared semantics for the initial target board on Linux,
  macOS, and Windows, and only call a host OS "reliable" after it has passed
  the planned validation bar on real hardware.
- Support both human-driven use and agent-driven automation.
- Make the recovery flow reproducible and observable.
- Use a modern release workflow with semantic versioning and changelog
  automation.
- Keep the codebase easy for agents to extend safely.

## Non-Goals For v1

- Support for additional Airoha boards or SoCs before the initial target works
  well.
- Generalizing the AN7581 flash layout into a board-agnostic abstraction
  before the initial target works well.
- GUI tooling.
- Backward compatibility with unstable internal APIs.
- Native OS package-manager distribution on day one.

## Proposed Repository Shape

Use a small Cargo workspace from the start:

```text
unbrk/
  AGENTS.md
  CHANGELOG.md
  CONTRIBUTING.md
  Cargo.toml
  rust-toolchain.toml
  deny.toml
  justfile
  .editorconfig
  .github/
    workflows/
  crates/
    unbrk-core/
    unbrk-cli/
  xtask/
  docs/
    initial-plan.md
    an7581-uart-recovery-protocol.md
    testing.md
  tests/
    fixtures/
      an7581/
```

## Architecture

### 1. Core library

`unbrk-core` should own:

- Serial transport abstraction.
- Target-profile data for prompt patterns, serial defaults, flash offsets,
  block sizes, block counts, and stage ordering.
- Prompt detection and recovery state machine.
- XMODEM send logic.
- U-Boot command execution and prompt-synchronized command parsing.
- Flash-plan execution for the documented AN7581 `mmc erase`, `loadx`,
  `mmc write`, `reset` sequence.
- Recovery transcript capture and parsing, preserving raw bytes.
- Structured event model shared by human and agent output modes.
- Error taxonomy that the CLI maps to documented exit codes.

Design rule:

- Keep UART and protocol logic testable without real hardware by separating
  transport, state, and rendering.

### 2. CLI binary

`unbrk-cli` should own:

- Argument parsing.
- TTY detection.
- Human-friendly progress rendering.
- Agent-oriented JSON event output.
- Exit-code mapping for documented failure classes.
- Interactive console handoff after recovery succeeds at the RAM-resident
  U-Boot prompt when persistent flashing is not requested.

### 3. Repo automation

`xtask` should own:

- Completion generation.
- manpage generation.
- release sanity checks.
- optional fixture generation from captured recovery transcripts.

## CLI Shape

Initial commands:

- `unbrk recover`
- `unbrk ports`
- `unbrk completions`
- `unbrk man`
- `unbrk doctor`

Initial `recover` inputs:

- `--port`
- `--baud 115200`
- `--preloader <path>`
- `--fip <path>`
- `--prompt-timeout`
- `--packet-timeout`
- `--xmodem-block-retry`
- `--xmodem-eot-retry`
- `--command-timeout`
- `--reset-timeout`
- `--log-file <path>` (structured logs and events)
- `--transcript-file <path>` (raw UART bytes)
- `--uboot-prompt <regex>`
- `--flash-persistent`
- `--resume-from-uboot` (expert-only)
- expert-only block-layout overrides for the documented AN7581 flash layout
- `--progress auto|plain|fancy|off`
- `--non-interactive`
- `--json`
- `--no-console` (skip interactive console handoff)

Suggested defaults:

- Auto-select a port only in interactive mode, and only when exactly one
  plausible candidate remains after filtering obviously irrelevant devices.
- Use fancy progress only when stdout is a TTY.
- Disable ANSI and spinners automatically for non-TTY or `--json`.
- Save a raw-byte transcript file when recovery fails.
- In `--non-interactive` mode, require an explicit `--port`; heuristic port
  selection is too risky for an automation path that can erase flash.
- Default to stopping at the RAM-resident U-Boot prompt and require
  `--flash-persistent` before running the erase/write/reset sequence.

## Human UX

When run by a human in a TTY, the tool should feel deliberate rather than
minimal:

- Phase-based progress display that reflects the chosen path:
  `Waiting for recovery mode -> Sending x for preloader -> Uploading preloader
  -> Waiting for stage 2 prompt -> Sending x for FIP -> Uploading FIP ->
  Waiting for live U-Boot prompt`, followed only when `--flash-persistent` is
  set by `Erasing flash -> Loading preloader into RAM -> Writing preloader ->
  Loading FIP into RAM -> Writing FIP -> Resetting target`.
- Spinner or progress bar with bytes sent, throughput, and elapsed time.
- Clear status banners for prompt transitions.
- Compact recovery summary at the end.
- Helpful remediation hints on timeout or unexpected console output.
- Do not hand off on the first `U-Boot` banner alone; wait for a live prompt and
  tolerate ANSI boot-menu noise on the way there.
- Only offer interactive console handoff when stdout is a TTY and neither
  `--json` nor `--no-console` is active.
- Report whether the run stopped at temporary U-Boot or completed the
  explicitly requested persistent flash-and-reset sequence.

Likely implementation:

- `clap` for CLI parsing.
- `indicatif` for TTY-aware progress.
- `tracing` plus structured event emission for logs.

## Agent UX

The agent mode should be deterministic and parseable:

- `--json` emits newline-delimited JSON events.
- The stream should declare its schema version in the opening event so agents
  can reject incompatible output explicitly.
- `--json` implies `--no-console`, and the CLI should reject conflicting flag
  combinations rather than mixing NDJSON with a live console stream.
- `--non-interactive` forbids ambiguous prompts, refuses heuristic port
  selection, and fails fast.
- `--progress` is reserved for human-oriented rendering; machine-readable output
  should use `--json`.
- Stable event kinds such as `port_opened`, `prompt_seen`, `input_sent`,
  `crc_ready`, `xmodem_started`, `xmodem_progress`, `xmodem_completed`,
  `uboot_prompt_seen`, `uboot_command_started`, `uboot_command_completed`,
  `image_verified`, `reset_seen`, `handoff_ready`, `failure`.
- Stable exit codes for common failure classes:
  serial open failure, timeout, prompt mismatch, XMODEM failure, U-Boot command
  failure, verification mismatch, bad input, user-abort.
- A sender-library XMODEM failure should only become a terminal recovery
  failure if the expected next prompt or active U-Boot prompt never appears.

This makes the CLI suitable for other agents, scripts, CI fixtures, and future
higher-level orchestration.

## Recovery State Machine For v1

Planned happy-path flow:

1. Open serial port at 115200.
2. Wait for the initial recovery prompt while an operator or external harness
   power-cycles the board into recovery mode.
3. Detect `Press x` prompt.
4. Send one literal `x` byte with no newline.
5. Detect XMODEM readiness from raw RX bytes, including repeated `C`
   characters.
6. Upload preloader over XMODEM.
7. Continue reading console output.
8. Detect the second prompt requesting BL31 + U-Boot FIP.
9. Send one literal `x` byte with no newline.
10. Detect XMODEM readiness again from raw RX bytes.
11. Upload the FIP image over XMODEM.
12. Wait for the RAM-resident U-Boot prompt, allowing for boot-menu output and
    ANSI escape noise before the prompt becomes visible.
13. If `--flash-persistent` is not set, attach the user to the live console
    only in interactive mode when `--no-console` is not set; otherwise report
    success at the temporary U-Boot prompt and exit cleanly.
14. If `--flash-persistent` is set, confirm that U-Boot is interactive and
    read `loadaddr`.
15. Before any erase or write, confirm that each configured flash range is
    large enough for its host image using the profile's MMC block size, and
    fail closed if either image would overrun its allotted block count.
16. Run `mmc erase 0 0x800` and require explicit success from the console.
17. Run `loadx $loadaddr 115200` for the preloader, wait for raw-byte XMODEM
    readiness, and send the preloader.
18. Read `printenv filesize`, parse it as hexadecimal, and compare it with the
    host preloader size.
19. Run `mmc write $loadaddr 0x4 0xfc` and require explicit success.
20. Run `loadx $loadaddr 115200` for the FIP, wait for raw-byte XMODEM
    readiness, and send the FIP.
21. Read `printenv filesize`, parse it as hexadecimal, and compare it with the
    host FIP size.
22. Run `mmc write $loadaddr 0x100 0x700` and require explicit success.
23. Run `reset`.
24. Treat explicit reset activity such as `resetting ...` followed by
    `EcoNet System Reset` as evidence that the `reset` command executed, but
    do not treat a bare `Press x` prompt or generic `U-Boot` banner as proof
    that the newly written persistent boot path is healthy.

Important implementation note:

- Prompt matching should be pattern-based and tolerant of minor formatting
  changes, line endings, and timing jitter.
- Each wait should consume only bytes observed after the preceding transition
  so stale prompt text cannot satisfy a later state.
- The first-stage matcher must not be allowed to consume the longer second
  prompt out of sequence.
- XMODEM readiness detection should inspect raw bytes, not only decoded text,
  because the observed UART stream mixes control bytes with the repeated `C`
  characters.
- If the console advances to the next prompt or to U-Boot before the sender
  sees a clean final `EOT` acknowledgement, treat that as forward progress with
  a warning instead of an unconditional transfer failure.
- During the U-Boot flash phase, command success must come from parsing the
  console output (`blocks erased: OK`, `blocks written: OK`, `Total Size`,
  `filesize=`), not from assuming that command echo implies success.
- The flash plan should validate image-size-to-block-count fit before issuing
  `mmc erase` or `mmc write`, rather than assuming the documented ranges are
  always large enough for whatever images the operator supplied.
- The AN7581 flash phase should parse `filesize` as hexadecimal even when
  U-Boot omits the `0x` prefix.
- The implementation should distinguish `flash sequence completed`,
  `reset observed`, and `persistent boot verified`; only the last of those can
  justify a claim that the board successfully booted from the rewritten flash.
- `--resume-from-uboot` should skip only the BootROM/XMODEM recovery phase and
  resume at the first interactive U-Boot prompt, then either hand off there or
  enter the flash plan if `--flash-persistent` is set. This mode should be
  treated as expert-only because the operator has to know the current U-Boot
  context is safe for the requested next step.

## Testing Strategy

### Fast tests

- Unit tests for prompt parsing and state transitions.
- Unit tests for timeout behavior and retry boundaries.
- Unit tests for event emission and exit-code mapping.
- Unit tests that confirm stage-local matching so the second prompt cannot be
  mistaken for the first.
- XMODEM tests using fixtures and mocked transports.
- Convert `docs/an7581-end-to-end.log` into fixtures immediately, then add
  multiple fresh captures across repeated recoveries before treating any prompt
  matcher as settled.

### Integration tests without hardware

- Simulated serial peer using a transport fake or PTY-backed harness.
- Golden transcript tests for:
  - happy path
  - resume-from-uboot happy path
  - missing first prompt
  - missing second prompt
  - XMODEM cancel/failure
  - sender reports a failed final `EOT` handshake but the console advances
  - noisy console output
  - long DRAM / boot chatter between the two recovery stages
  - ANSI-heavy boot-menu output before the `AN7581>` prompt
  - repeated `C` readiness mixed with non-printable control bytes
  - `mmc erase` failure
  - image larger than its configured flash allocation
  - `filesize` mismatch after `loadx`
  - `mmc write` failure
  - missing reset evidence after `reset`
  - partial prompt fragmentation across reads

### Cross-platform CI expectations

- Linux: full unit and integration suite, plus the first real-device
  validation runs.
- macOS: full unit suite and selected integration tests to prove portability;
  do not describe hardware recovery as validated there until real-device runs
  have succeeded on macOS.
- Windows: compile, unit tests, and any transport-agnostic integration tests
  to prove portability; do not describe hardware recovery as validated there
  until real-device runs have succeeded on Windows.

### Later hardening

- Hardware-in-the-loop tests on a self-hosted runner once the device fixture is
  practical.
- Re-capture transcripts from additional recovery attempts and bootloader
  revisions before declaring prompt heuristics stable.

## CI And Quality Gates

Use GitHub Actions with pinned actions, least-privilege permissions, and
workflow concurrency cancellation.

Planned workflows:

### `ci.yml`

On pull requests and pushes to the main branch:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo nextest run --workspace`
- `cargo test --doc --workspace`
- `cargo deny check`
- `typos`
- build matrix for Linux, macOS, Windows

### `release-pr.yml`

Use `release-plz` to:

- open/update release PRs
- bump versions according to conventional commits
- update `CHANGELOG.md`
- create Git tags and GitHub Releases after merge

### `dist.yml`

Build release artifacts for:

- `x86_64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Optional later targets:

- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`

Release artifacts should include:

- archives
- SHA-256 checksums
- provenance/attestations
- optional shell and PowerShell installer scripts if `cargo-dist` fits cleanly

## Release Management

Recommended stack:

- `release-plz` for semantic versioning, release PRs, and changelog automation
- `cargo-dist` for cross-platform release artifacts
- GitHub Releases for binary distribution
- crates.io trusted publishing once the public CLI contract is ready for
  `cargo install`

Commit and PR conventions:

- Use Conventional Commits.
- Require human-readable PR titles that map cleanly to changelog entries.
- Keep release notes concise and user-facing.

Versioning policy:

- `0.x` until the recovery flow and CLI contract settle.
- Minor bumps for user-visible features.
- Patch bumps for fixes and workflow-only corrections that do not change public
  behavior.
- Major `1.0` only after the recovery flow is proven stable and the machine
  interface is intentionally versioned.

## Security And Supply Chain

- Pin GitHub Actions by full SHA where practical.
- Use minimal workflow permissions.
- Enable Dependabot for GitHub Actions and Cargo dependencies.
- Use `cargo deny` for advisories and license policy.
- Generate attestations for release artifacts.
- Prefer crates.io trusted publishing over long-lived publish tokens.

## Documentation Plan

Ship with:

- `README.md` with quick start and supported workflow
- `docs/an7581-uart-recovery-protocol.md` for the current board's observed
  prompts, transfer order, and documented quirks
- `docs/hardware-validation-2026-03-14.md` for real-device Linux evidence,
  failure signatures, and operator recovery guidance
- `docs/testing.md` for simulated transport and hardware test instructions
- `tests/fixtures/an7581/` for transcript-derived serial fixtures
- `CONTRIBUTING.md` for local dev workflow
- `AGENTS.md` with repo-specific instructions for coding agents

The README should include:

- a human-driven full end-to-end recovery example
- a machine-driven `--json --non-interactive` example
- an example that stops at temporary U-Boot for debugging

Current status on 2026-03-14:

- `README.md` and `docs/testing.md` are now the operator-facing entry points
- `docs/hardware-validation-2026-03-14.md` holds the current Linux
  real-device evidence and the failure-mode note
- transcript-derived fixtures under `tests/fixtures/an7581/` include both the
  original happy-path captures and real-hardware prompt-variation slices

## Agent-Oriented Project Conventions

To make the repo easier for coding agents to work in:

- Keep protocol logic in a library crate, not in `main.rs`.
- Provide `just` targets for every common task:
  `just fmt`, `just lint`, `just test`, `just ci`, `just dist`
- Keep fixture files small and named by scenario.
- Check in a root `AGENTS.md` with coding, testing, and release expectations.
- Prefer explicit types and stable error enums over ad hoc strings.
- Prefer transcript fixtures over prose-only protocol descriptions.
- Keep CLI JSON output explicitly versioned from its first release and document
  the schema once exposed.

## Milestones

### Milestone 0: Repo bootstrap

Deliverables:

- Cargo workspace
- basic docs
- CI skeleton
- lint/test/release tooling config

Exit criteria:

- clean CI on Linux, macOS, Windows
- release tooling config checked in, but public publishing automation still
  deferred

### Milestone 1: Recovery core

Deliverables:

- serial abstraction
- prompt parser
- state machine
- XMODEM send path
- U-Boot command runner
- AN7581 flash-plan executor
- transcript logging
- transcript-backed fixtures derived from the existing log plus multiple fresh
  real recovery captures

Exit criteria:

- happy-path end-to-end recovery works against simulated fixtures
- prompt assumptions have been checked against the existing end-to-end log and
  multiple fresh captures from repeated recoveries
- clear failures for timeout, prompt mismatch, U-Boot command failure, and size
  verification mismatch
- fixture coverage includes the observed no-final-ACK XMODEM quirk and ANSI
  noise before the U-Boot prompt

### Milestone 2: Usable CLI

Deliverables:

- `recover` command
- TTY-aware progress UI
- JSON event mode
- explicit persistent-flash mode and resume-from-U-Boot mode
- console handoff

Exit criteria:

- human end-to-end flow is usable without reading source
- agent mode is deterministic and documented

### Milestone 3: Hardware validation

Deliverables:

- tested against the initial target board on the first hardware-validated host
- AN7581 protocol notes and fixtures updated from real transcripts
- advertised host-OS support level documented explicitly
- recovery operator guidance refined

Exit criteria:

- successful repeated recoveries on target hardware from Linux
- macOS and Windows remain portability targets unless and until they each have
  their own successful real-device recovery evidence
- known failure modes documented

Current status on 2026-03-14:

- Linux real-device recovery and persistent flash validation are done
- fresh clean and interrupted transcripts are captured under
  `artifacts/hardware-transcripts/2026-03-14/`
- transcript-derived prompt and CRC fixtures have been updated from those runs
- macOS and Windows still remain portability targets pending their own
  validation passes

### Milestone 4: Release hardening

Deliverables:

- `release-plz` automation
- `cargo-dist` artifacts
- checksums
- attestations
- first public prerelease

Exit criteria:

- tagged release produces installable artifacts for all primary platforms
- `cargo install` is enabled only if the CLI contract has been declared ready
  for public installation

## Implementation Order

Recommended order of work:

1. Create workspace and repo policy files.
2. Implement transport abstraction and transcript capture.
3. Convert the existing end-to-end log into transcript fixtures and capture
   multiple fresh serial transcripts to lock down prompt matching.
4. Implement prompt-driven state machine.
5. Implement XMODEM upload support.
6. Implement the U-Boot command runner, `loadx` verification, and AN7581
   flash-plan executor.
7. Add simulated integration tests anchored to captured transcripts.
8. Validate the first end-to-end happy path on target hardware.
9. Add human progress renderer and JSON event mode.
10. Add CI quality gates.
11. Add release automation and distribution.

## Risks And Mitigations

- Prompt text may differ across bootloader revisions.
  Mitigation: pattern-based matching plus transcript capture.

- XMODEM behavior may vary by sender/receiver timing.
  Mitigation: configurable timeouts, bounded retries, detailed transfer logs.

- Serial behavior differs across platforms.
  Mitigation: keep transport abstract, test on all target OSes in CI, and avoid
  platform-specific assumptions in CLI logic.

- The AN7581 flash offsets or block counts could be wrong for another image
  layout or board revision.
  Mitigation: keep the flash plan explicitly profile-backed, expose expert
  overrides, validate image fit against the configured block size and block
  counts before writing, and treat generalization to other boards as deferred
  work.

- U-Boot environment parsing can be brittle if `loadaddr` or `filesize` output
  format changes.
  Mitigation: parse the current output conservatively, cover it with transcript
  fixtures, and fail closed on ambiguous command results.

- Prose protocol notes may miss spacing, fragmentation, or timing details that
  matter to prompt detection.
  Mitigation: capture real transcripts early and derive fixtures from them
  before hardening parser behavior.

- A single captured recovery can bias matcher design toward one bootloader
  revision or timing profile.
  Mitigation: use the existing log to bootstrap tests, then gather additional
  captures and keep prompt regexes conservative until variation is understood.

- Release automation can become overbuilt for a small project.
  Mitigation: keep `release-plz` and `cargo-dist` usage narrow and resist adding
  package-manager publishers before v1 is stable.

## Deferred Decisions

These should be revisited after Milestone 2:

- crate licensing
- whether to publish the core crate separately
- whether to support additional Airoha SoCs in the same CLI
- whether the stable UX should remain one `recover` command with
  `--flash-persistent` or later split into separate `recover` and `flash`
  commands
- whether to add Homebrew, Scoop, and Winget packaging
- whether to add hardware-in-the-loop GitHub runners

## First Follow-Up Tasks

After this plan is accepted, the first implementation tasks should be:

1. bootstrap the Rust workspace and repo policy files
2. add a transport abstraction and transcript logger
3. turn the existing end-to-end log into fixtures and capture fresh serial
   transcripts to lock down prompt matching
4. implement the prompt-driven state machine
5. implement XMODEM upload support
6. build the U-Boot command runner and AN7581 flash-plan executor directly
   from the proven helper sequence
