# unbrk

`unbrk` is a Rust CLI for automating the Airoha AN7581 UART recovery flow and,
optionally, the follow-on U-Boot flash sequence.

## Support Status

- Linux is hardware-validated on Nokia Valyrian as of 2026-03-14.
- macOS and Windows are portability targets only until they each have their
  own real-device validation evidence.

The current macOS and Windows portability claims come from the
`test (macos-latest)` and `test (windows-latest)` jobs in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml), which run the simulated
clippy, nextest, and doc-test suite. They do not imply real-device UART
recovery has been validated on either host yet.

## Installation

`unbrk` is distributed through GitHub Releases, not crates.io.

- Download the archive that matches your host from the
  [Releases](https://github.com/ynezz/unbrk/releases) page.
- Linux and macOS releases ship as `.tar.gz` archives.
- Windows releases ship as `.zip` archives.
- Shell and PowerShell installer scripts are attached to each tagged release.

`cargo install` is intentionally not supported yet. The CLI contract is still
settling, so public installation stays tied to signed release artifacts instead
of crates.io.

Before using a release artifact, verify both integrity and provenance:

```bash
sha256sum -c unbrk-vX.Y.Z.sha256
gh attestation verify ./unbrk-cli-x86_64-unknown-linux-gnu.tar.gz \
  -R ynezz/unbrk
```

## Quick Start

Human-driven recovery to the temporary RAM-resident `AN7581>` prompt, with
live progress output and an interactive console handoff at the end:

```bash
unbrk recover \
  --port /dev/ttyS4 \
  --preloader /path/to/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip /path/to/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --prompt-timeout 120
```

Machine-driven recovery that stops at U-Boot and emits newline-delimited JSON
events instead of a live console:

```bash
unbrk recover \
  --port /dev/ttyS4 \
  --preloader /path/to/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip /path/to/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --prompt-timeout 120 \
  --non-interactive \
  --json
```

Persistent flash from an already-live U-Boot prompt:

```bash
unbrk recover \
  --port /dev/ttyS4 \
  --preloader /path/to/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip /path/to/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --resume-from-uboot \
  --flash-persistent \
  --non-interactive \
  --json
```

## Operator Guidance

- Start `recover` first, then perform exactly one controlled reset while it is
  waiting for the initial prompt.
- Do not press reset again after `Press x`, after `CCC`, or during either
  XMODEM transfer. Repeated restarts can invalidate the run.
- Treat `xCCC` as normal. The echoed `x` is the board reflecting the byte you
  sent, and `CCC` is XMODEM-CRC readiness.
- Treat `NOTICE:  3-3-3` before the second prompt and ANSI bytes before
  `AN7581>` as normal boot chatter, not as protocol failure.
- If `prompt-timeout` expires before any prompt appears, restart from a clean
  power-off state and re-check the port, cabling, and recovery-mode timing.

## Exit Codes

`unbrk recover` uses stable exit codes:

- `0`: success
- `1`: I/O error
- `2`: timeout
- `3`: protocol error
- `4`: XMODEM failure
- `5`: U-Boot command failure
- `6`: verification mismatch
- `7`: bad input
- `8`: user abort

## JSON Event Stream

`--json` emits newline-delimited JSON events. The opening `session_started`
event declares `schema_version`, which is currently `1`, so automation can
reject incompatible streams explicitly.

The stable event kinds are:

- `session_started`
- `port_opened`
- `prompt_seen`
- `input_sent`
- `crc_ready`
- `xmodem_started`
- `xmodem_progress`
- `xmodem_completed`
- `uboot_prompt_seen`
- `uboot_command_started`
- `uboot_command_completed`
- `image_verified`
- `reset_seen`
- `handoff_ready`
- `failure`

## Reference Docs

- `docs/an7581-uart-recovery-protocol.md`
- `docs/hardware-validation-2026-03-14.md`
- `docs/testing.md`
- `tests/fixtures/an7581/README.md`
