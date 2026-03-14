# Testing

## Local Quality Gates

Use the shared `justfile` when possible:

```bash
just ci
```

The mandatory workspace checks remain:

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## Transcript And Fixture Coverage

Targeted transcript-derived checks:

```bash
cargo test -p unbrk-core --test recovery_fixture_harness -- --nocapture
cargo test -p unbrk-core real_ -- --nocapture
```

Those tests cover:

- happy-path recovery and flash transcripts
- prompt matching against real hardware variations
- CRC readiness detection against echoed-input captures
- reset evidence parsing and flash verification paths

## Hardware Validation Procedure

Use this flow for real-device Linux validation on Nokia Valyrian:

1. Prepare the board powered off and connect the serial adapter.
2. Start `unbrk recover` with explicit `--port`, `--preloader`, and `--fip`.
3. Wait until the command is blocked on the initial prompt.
4. Perform exactly one controlled reset to enter recovery mode.
5. Leave the board alone until the command succeeds or times out.
6. Save both `--log-file` and `--transcript-file` outputs for the run.

Recovery-only example:

```bash
cargo run -p unbrk-cli -- recover \
  --port /dev/ttyS4 \
  --preloader /path/to/preloader.bin \
  --fip /path/to/bl31-uboot.fip \
  --prompt-timeout 120 \
  --log-file /tmp/unbrk/recovery-events.jsonl \
  --transcript-file /tmp/unbrk/recovery-transcript.bin \
  --non-interactive \
  --json
```

Persistent-flash example from a live prompt:

```bash
cargo run -p unbrk-cli -- recover \
  --port /dev/ttyS4 \
  --preloader /path/to/preloader.bin \
  --fip /path/to/bl31-uboot.fip \
  --resume-from-uboot \
  --flash-persistent \
  --command-timeout 60 \
  --reset-timeout 60 \
  --log-file /tmp/unbrk/flash-events.jsonl \
  --transcript-file /tmp/unbrk/flash-transcript.bin \
  --non-interactive \
  --json
```

## Hardware Failure Triage

- No `Press x` before `prompt-timeout`: the board did not enter recovery in
  time, the wrong serial port is open, or the UART link is unhealthy.
- `xCCC` appears: expected, not a failure.
- Boot chatter appears before the second prompt or before `AN7581>`: expected,
  not a failure by itself.
- A run reaches `U-Boot` text but never reaches `AN7581>`: treat it as an
  interrupted or incomplete recovery and restart from a clean power cycle.
- `filesize` or `Total Size` does not match the host image after `loadx`: stop
  before any write, verify the artifact pair, and keep the log/transcript.
- `mmc erase` or `mmc write` does not report `OK`: treat it as a terminal
  flash failure and do not assume the device state is safe.
- `reset` completes without later reset evidence such as `EcoNet System Reset`:
  treat the flash sequence as incomplete and keep the captured outputs.
- Repeated manual resets after the command has started: treat the whole run as
  invalid and restart cleanly.

The March 14, 2026 validation evidence and stored transcript paths live in
`docs/hardware-validation-2026-03-14.md`.
