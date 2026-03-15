# unbrk

`unbrk` is a Rust CLI for automating the Airoha AN7581 UART recovery flow and,
optionally, the follow-on U-Boot flash sequence.

<p align="center">
  <img src="docs/unbrk-recover.gif" alt="unbrk recover demo">
</p>

## Support Status

- Linux is hardware-validated on Nokia Valyrian as of 2026-03-14.
- macOS (aarch64) is hardware-validated on Nokia Valyrian as of 2026-03-15.
- Windows is a portability target only until it has its own real-device
  validation evidence.

Both Linux and macOS validations were performed against the Nokia Valyrian
(Airoha AN7581) using an FTDI FT232R USB UART adapter. The macOS validation
covered RAM recovery, persistent NAND flashing (`--flash-persistent`),
`--resume-from-uboot`, `--json` output, image verification, and all CLI
subcommands (`doctor`, `ports`, `completions`, `man`).

The current Windows portability claim comes from the `test (windows-latest)`
job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml), which runs the
simulated clippy, nextest, and doc-test suite. It does not imply real-device
UART recovery has been validated on Windows yet.

## Installation

### One-liner (Linux / macOS)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ynezz/unbrk/releases/latest/download/unbrk-cli-installer.sh | sh
```

### PowerShell (Windows)

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/ynezz/unbrk/releases/latest/download/unbrk-cli-installer.ps1 | iex"
```

### Manual download

Download the archive that matches your host from the
[Releases](https://github.com/ynezz/unbrk/releases) page:

| Platform | Archive |
|---|---|
| Linux x86_64 | `unbrk-cli-x86_64-unknown-linux-gnu.tar.gz` |
| macOS x86_64 | `unbrk-cli-x86_64-apple-darwin.tar.gz` |
| macOS ARM | `unbrk-cli-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 | `unbrk-cli-x86_64-pc-windows-msvc.zip` |

### Verification

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

## Build From Source

The repository pins Rust `1.93.1` in
[`rust-toolchain.toml`](rust-toolchain.toml), so a current `rustup` install is
enough to pick up the expected toolchain automatically.

Build the release CLI locally with Cargo:

```bash
cargo build --release -p unbrk-cli
```

The compiled binary will be available at `target/release/unbrk`.

If you use [`just`](justfile), the repo also exposes the shared developer
targets:

```bash
just dist
just ci
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
- `docs/hardware-validation-2026-03-14.md` (Linux)
- `docs/hardware-validation-2026-03-15.md` (macOS)
- `docs/testing.md`
- `tests/fixtures/an7581/README.md`
