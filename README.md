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

## Quick Start

Recovery to the temporary RAM-resident `AN7581>` prompt:

```bash
cargo run -p unbrk-cli -- recover \
  --port /dev/ttyS4 \
  --preloader /path/to/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip /path/to/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --prompt-timeout 120 \
  --non-interactive \
  --json
```

Persistent flash from an already-live U-Boot prompt:

```bash
cargo run -p unbrk-cli -- recover \
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

## Reference Docs

- `docs/an7581-uart-recovery-protocol.md`
- `docs/hardware-validation-2026-03-14.md`
- `docs/testing.md`
- `tests/fixtures/an7581/README.md`
