# Nokia Valyrian Hardware Validation (macOS) - 2026-03-15

## Scope

This note records the first successful real-device macOS validation for
`unbrk` against the Nokia Valyrian board on `/dev/tty.usbserial-A5069RR4`.

Validated flows:

- Recovery only: BootROM recovery to a live RAM-resident `AN7581>` prompt
- Persistent flash: `--resume-from-uboot --flash-persistent` from the live
  prompt through `mmc erase`, both `loadx` transfers, both `mmc write`
  commands, and final `reset`

## Environment

- Host: macOS aarch64 (Apple Silicon), Darwin 24.6.0
- Serial adapter: FTDI FT232R USB UART (VID:0403 PID:6001)
- Port: `/dev/tty.usbserial-A5069RR4`
- Binary: `unbrk 0.1.0` (pre-built, `~/.cargo/bin/unbrk`)

## Artifact Source

Same firmware artifacts as the Linux validation (see
`docs/hardware-validation-2026-03-14.md`):

- `prplos-airoha-an7581-nokia_valyrian-preloader.bin` (113447 bytes)
- `prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip` (218346 bytes)

## Commands

Doctor check:

```bash
unbrk doctor \
  --port /dev/tty.usbserial-A5069RR4 \
  --preloader ../prpl/nokia/valyrian/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip ../prpl/nokia/valyrian/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip
```

Recovery only:

```bash
unbrk recover \
  --port /dev/tty.usbserial-A5069RR4 \
  --preloader ../prpl/nokia/valyrian/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip ../prpl/nokia/valyrian/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --progress plain \
  --non-interactive \
  --transcript-file /tmp/unbrk-macos-validation.log
```

Persistent flash from the live U-Boot prompt:

```bash
unbrk recover \
  --port /dev/tty.usbserial-A5069RR4 \
  --preloader ../prpl/nokia/valyrian/prplos-airoha-an7581-nokia_valyrian-preloader.bin \
  --fip ../prpl/nokia/valyrian/prplos-airoha-an7581-nokia_valyrian-bl31-uboot.fip \
  --resume-from-uboot \
  --flash-persistent \
  --non-interactive \
  --json \
  --transcript-file /tmp/unbrk-macos-flash.log
```

## Observed Recovery Behavior

Observed prompt and protocol markers matched the current implementation:

- Initial BootROM prompt matched `Press x`
- Second-stage prompt matched `Press x to load BL31 + U-Boot FIP`
- Final RAM-resident U-Boot prompt matched `AN7581>`

Recovery-only timing (from plain-text progress output):

- Initial prompt seen about 16s after session start
- Preloader transfer: 110.79 KiB over XMODEM
- FIP transfer: 213.23 KiB over XMODEM
- Total recovery time: 66 seconds

Other observed details:

- EOT quirk recovery was triggered on the FIP transfer during RAM-only
  recovery; the board advanced to the next prompt despite incomplete
  final-EOT handshake
- No EOT quirk during the subsequent flash run
- Stage 2 prompt matching worked on real hardware
- The live U-Boot prompt appeared as `AN7581>` exactly as configured

## Observed Flash Behavior

The persistent flash flow completed successfully from the live U-Boot prompt.

Observed values from the JSON event stream (2625 events total):

- `loadaddr=0x81800000`
- `mmc erase 0x0 0x800` completed successfully
- Preloader `loadx`: 113447 bytes transferred and verified
- `mmc write $loadaddr 0x4 0xde` completed successfully
- FIP `loadx`: 218346 bytes transferred and verified
- `mmc write $loadaddr 0x100 0x1ab` completed successfully
- Reset evidence matched `EcoNet System Reset`

Flash timing from the JSON event stream:

- `mmc erase`: about 0.06s
- Preloader `loadx` (CRC wait + XMODEM): about 19.3s
  - CRC readiness wait: about 8.5s
  - XMODEM transfer: about 10.8s
- Preloader `mmc write`: about 0.03s
- FIP `loadx` (CRC wait + XMODEM): about 29.5s
  - CRC readiness wait: about 8.5s
  - XMODEM transfer: about 20.8s
- FIP `mmc write`: about 0.04s
- Reset evidence after `reset`: about 0.06s

## Platform Timing Comparison (macOS vs Linux)

The macOS validation reveals a consistent ~8.5s CRC readiness wait per
`loadx` command, compared to an estimated ~2s on Linux. This is the dominant
platform-specific timing difference and adds roughly 13s to a full flash
cycle.

| Phase | Linux (2026-03-14) | macOS (2026-03-15) | Delta |
|-------|--------------------|--------------------|-------|
| Preloader `loadx` total | ~11.4s | ~19.3s | +7.9s |
| - CRC readiness wait | ~2s (est.) | ~8.5s | +6.5s |
| - XMODEM transfer | ~9.4s (est.) | ~10.8s | +1.4s |
| FIP `loadx` total | ~21.8s | ~29.5s | +7.7s |
| - CRC readiness wait | ~2s (est.) | ~8.5s | +6.5s |
| - XMODEM transfer | ~19.8s (est.) | ~20.8s | +1.0s |
| `mmc erase` | ~0.06s | ~0.06s | 0 |
| Preloader `mmc write` | ~0.03s | ~0.03s | 0 |
| FIP `mmc write` | ~0.12s | ~0.04s | -0.08s |
| Reset evidence | ~0.06s | ~0.06s | 0 |

Key observations:

- XMODEM actual transfer speed is similar across platforms (~15% slower on
  macOS), consistent with 115200 baud wire speed being the bottleneck
- The CRC readiness delay is 4x longer on macOS, likely due to macOS
  USB-to-serial driver latency or buffering differences in the FTDI kext
- `mmc` operations are identical (board-side, OS-independent)
- EOT quirk recovery triggered on macOS (RAM recovery, FIP stage) but not
  on Linux, suggesting tighter EOT timing on macOS USB serial

The default `prompt-timeout` of 30s and `command-timeout` of 30s remain
adequate for macOS. No timeout adjustments are needed.

## Additional CLI Validation

The following CLI features were also validated on macOS aarch64:

- `unbrk ports`: correctly discovers FT232R as `[plausible]`, filters PCI
  serial ports as `[ignored]`
- `unbrk doctor`: validates port access, preloader size (fits 129024 byte
  window), FIP readability; correctly rejects oversized combined image
  (349418 bytes > 129024 byte preloader window)
- `unbrk completions bash|zsh|fish`: all three generate valid output
- `unbrk man`: generates valid troff man page
- Error handling: exit code 1 for I/O errors (bad port), exit code 7 for
  bad input (missing file), exit code 2 for timeout

## Post-Flash Boot Verification

After the persistent flash, the board was confirmed to boot normally from
NAND. A subsequent `unbrk recover` with `--prompt-timeout 10` timed out
waiting for the recovery prompt (exit code 2), confirming the board no
longer enters recovery mode and boots from the freshly written firmware.

## Outcome

`unbrk` successfully validated both required hardware flows on macOS
aarch64 against the live Valyrian board:

- BootROM recovery to the RAM-resident U-Boot prompt
- Resume-from-U-Boot persistent flash of the bootloader artifacts
- Post-flash boot from NAND

This confirms cross-platform compatibility of the prompt patterns, XMODEM
behavior, U-Boot parsing, flash verification, and reset detection. The
primary platform difference is the CRC readiness latency, which is well
within existing timeout margins.
