# Nokia Valyrian Hardware Validation - 2026-03-14

## Scope

This note records the first successful real-device Linux validation for
`unbrk` against the Nokia Valyrian board on `/dev/ttyS4`.

Validated flows:

- Recovery only: BootROM recovery to a live RAM-resident `AN7581>` prompt
- Persistent flash: `--resume-from-uboot --flash-persistent` from the live
  prompt through `mmc erase`, both `loadx` transfers, both `mmc write`
  commands, and final `reset`

## Artifact Source

GitLab job:

- <https://gitlab.com/prpl-foundation/prplos/prplos/-/jobs/13491641115>

Downloaded directly with `glab api` from job artifact paths:

- `bin/targets/airoha/an7581/prplos-airoha-an7581-nokia_valyrian-preloader.bin`
- `bin/targets/airoha/an7581/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip`

Verified against the job's published `sha256sums`:

- `preloader.bin`: `6c3b2339d036340396730a13adfe35c0d2a4dddedeffb6f9965a24e0c7908808`
- `bl31-uboot.fip`: `f1d988d89f5894fccc045183b93f4a29c6764c009b35b4d79c746990938fb561`

## Commands

Recovery only:

```bash
cargo run -p unbrk-cli -- recover \
  --port /dev/ttyS4 \
  --preloader /tmp/unbrk-artifacts/job-13491641115/valyrian/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip /tmp/unbrk-artifacts/job-13491641115/valyrian/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --prompt-timeout 120 \
  --non-interactive \
  --json \
  --log-file /tmp/unbrk-runs/20260314T144229Z/recovery-json-events.jsonl \
  --transcript-file /tmp/unbrk-runs/20260314T144229Z/recovery-json-transcript.bin
```

Persistent flash from the live U-Boot prompt:

```bash
cargo run -p unbrk-cli -- recover \
  --port /dev/ttyS4 \
  --preloader /tmp/unbrk-artifacts/job-13491641115/valyrian/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip /tmp/unbrk-artifacts/job-13491641115/valyrian/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --resume-from-uboot \
  --flash-persistent \
  --command-timeout 60 \
  --reset-timeout 60 \
  --non-interactive \
  --json \
  --log-file /tmp/unbrk-runs/20260314T144401Z/flash-json-events.jsonl \
  --transcript-file /tmp/unbrk-runs/20260314T144401Z/flash-transcript.bin
```

## Observed Recovery Behavior

Observed prompt and protocol markers matched the current implementation:

- Initial BootROM prompt matched `Press x`
- Second-stage prompt matched `Press x to load BL31 + U-Boot FIP`
- Final RAM-resident U-Boot prompt matched `AN7581>`

Recovery-only timing from the emitted event stream:

- Initial prompt seen about 20.2s after session start
- Preloader transfer: about 12.2s
- Delay from preloader completion to second prompt: about 1.8s
- FIP transfer: about 24.5s
- Delay from FIP completion to `AN7581>`: about 4.2s

Other observed details:

- No EOT-quirk recovery was needed on either transfer
- Stage 2 prompt matching worked on real hardware without stale stage 1 reuse
- The live U-Boot prompt appeared as `AN7581>` exactly as configured

The first attempted run was invalidated by repeated manual restarts during the
active transfer window. A controlled single restart produced the successful
recovery above.

## Observed Flash Behavior

The persistent flash flow completed successfully from the live U-Boot prompt.

Observed values from the transcript:

- `loadaddr=0x81800000`
- `mmc erase 0x0 0x800` reported `2048 blocks erased: OK`
- Preloader `loadx` reported `Total Size = 0x0001bb27 = 113447 Bytes`
- `printenv filesize` reported `filesize=1bb27`
- `mmc write $loadaddr 0x4 0xfc` reported `252 blocks written: OK`
- FIP `loadx` reported `Total Size = 0x000354ea = 218346 Bytes`
- `printenv filesize` reported `filesize=354ea`
- `mmc write $loadaddr 0x100 0x700` reported `1792 blocks written: OK`
- Reset evidence matched `EcoNet System Reset`

Flash timing from the emitted event stream:

- `mmc erase`: about 0.06s
- Preloader `loadx`: about 11.4s
- Preloader `mmc write`: about 0.03s
- FIP `loadx`: about 21.8s
- FIP `mmc write`: about 0.12s
- Reset evidence after `reset`: about 0.06s

## Known Failure Modes

Observed or capture-backed failure signatures from the same setup:

- Repeated manual restarts after `recover` has already started can invalidate
  the run. The first attempted session failed this way before the successful
  controlled single-restart run.
- An interrupted recovery can show `Press x`, then `CCC`, then later `U-Boot`
  output without ever reaching `AN7581>`. The stored interrupted transcript in
  `run-04-interrupted-manual/` shows exactly that shape and should be treated
  as incomplete recovery, not success.
- The board can echo the input byte before CRC readiness appears. `xCCC` is
  normal and must not be interpreted as corruption.
- The second-stage prompt can be preceded by boot chatter such as
  `NOTICE:  3-3-3`, and the final `AN7581>` prompt can be preceded by ANSI
  escape bytes. Those bytes are part of normal console noise on this target.
- Sender-side transfer completion is less authoritative than console
  progression. If the console advances to the expected next prompt or to
  `AN7581>`, that forward progress matters more than a pristine final-EOT
  handshake.
- Additional terminal failure classes remain test-backed even though they were
  not observed in the successful hardware runs above: `filesize` verification
  mismatch after `loadx`, `mmc erase` failure, `mmc write` failure, and
  missing reset evidence after `reset`.

## Operator Recovery Guidance

- Start `unbrk recover` first and let it wait on `/dev/ttyS4`.
- Perform exactly one controlled reset after the command is already waiting.
- Do not press reset again once `Press x` has appeared or while a transfer is
  in progress.
- If no prompt appears before `prompt-timeout`, stop and re-check recovery-mode
  timing, serial-port selection, and cabling before trying again.
- If the run reaches a live `AN7581>` prompt, leave the board alone until the
  requested path completes. For `--flash-persistent`, wait for explicit reset
  evidence instead of assuming a bare `U-Boot` banner proves success.
- If `loadx` verification, `mmc erase`, `mmc write`, or reset evidence fails,
  stop immediately and keep the JSON event log plus raw transcript for review
  before attempting another destructive run.
- Keep both the JSON event log and raw transcript for any failed run so prompt
  timing and transfer state can be compared against the stored captures.

## Outcome

`unbrk` successfully validated both required Linux hardware flows against the
live Valyrian board on `/dev/ttyS4`:

- BootROM recovery to the RAM-resident U-Boot prompt
- Resume-from-U-Boot persistent flash of the bootloader artifacts

This confirms the current prompt patterns, XMODEM behavior, U-Boot parsing,
flash verification, and reset detection on real hardware for this board.

## Stored Transcript Captures

Fresh transcript captures from the same hardware setup are now stored in the
repository under `artifacts/hardware-transcripts/2026-03-14/`:

- `run-01-clean/`: complete successful recovery to `AN7581>`
- `run-02-clean/`: second complete successful recovery to `AN7581>`
- `run-04-interrupted-manual/`: intentionally interrupted partial capture

Observed markers in the interrupted capture:

- `Press x` at byte offset 1
- `CCC` at byte offset 11
- `U-Boot` later in the stream at byte offset 121428

The interrupted capture timed out before JSON event flushing completed, so it
contains only the raw UART transcript. The two clean runs include both
`events.jsonl` and `transcript.bin`.
