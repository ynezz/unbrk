# Airoha AN7581 UART Recovery Protocol

## Summary

The Airoha AN7581 recovery path is a two-stage UART conversation driven by
plain-text prompts and XMODEM transfers. It is not the Mediatek BootROM packet
protocol used by `mtk_uartboot`.

The recovery sequence is:

1. Boot the board into BootROM recovery mode by holding the reset button during
   power-on.
2. Wait for a UART prompt containing `Press x`.
3. Send `x`.
4. Wait for XMODEM-CRC readiness, observed as repeated `C` bytes.
5. Send the preloader image over XMODEM.
6. Wait for the second-stage prompt `Press x to load BL31 + U-Boot FIP`.
7. Send `x`.
8. Wait for XMODEM-CRC readiness again.
9. Send the BL31 + U-Boot FIP image over XMODEM.
10. Expect a temporary U-Boot session running from RAM.

After that, permanent flashing happens with normal U-Boot commands.

## Entry Conditions

- Start from the board powered off.
- Hold the reset button while powering on.
- On Airoha AN7581, the reset button is the middle button.
- The local board note says this asserts the `GPIO0` strap so BootROM enters
  serial recovery mode.

## Serial Parameters

- Port: `/dev/ttyUSB0` in the current lab setup
- Adapter: FTDI FT232R USB UART
- Baud: `115200`
- Framing: `8N1`
- Flow control: none

## Images

The image pair that matches the documented flow is:

- Preloader:
  `/var/home/ynezz/dev/tftp/prplos-airoha-an7581-an7581-preloader.bin`
- FIP:
  `/var/home/ynezz/dev/tftp/prplos-airoha-an7581-an7581-bl31-uboot.fip`

Observed checksums:

- `preloader.bin`:
  `6c3b2339d036340396730a13adfe35c0d2a4dddedeffb6f9965a24e0c7908808`
- `bl31-uboot.fip`:
  `54e8e701ba8ef2cc61d97bf9521c54014bbc57992f1a74a1954566417f707363`

There is also a combined image,
`prplos-airoha-an7581-an7581-preloader-bl31-uboot.img`, but the local
board recovery note documents the two-transfer flow above, so that is the
protocol `unbrk` should target first.

## Wire-Level Behavior

The protocol visible on UART is simple:

- Human-readable boot and recovery prompts are emitted as ASCII text.
- The host advances both stages by sending a single ASCII `x`.
- Each file transfer starts only after the target emits XMODEM-CRC readiness
  bytes (`C`).
- File upload uses standard XMODEM with CRC, not raw binary streaming.
- The first payload is the preloader.
- The second payload is the BL31 + U-Boot FIP image.

For automation purposes, the state machine is:

1. `wait_for_initial_prompt`
2. `send_x_for_preloader`
3. `wait_for_xmodem_crc_preloader`
4. `send_preloader`
5. `wait_for_fip_prompt`
6. `send_x_for_fip`
7. `wait_for_xmodem_crc_fip`
8. `send_fip`
9. `wait_for_uboot_prompt`

## Host-Side Transfer Options

The existing local notes document three working host-side approaches:

### `sx`

`screen` and `picocom` notes both use `sx` from the `lrzsz` package as the
XMODEM sender.

Typical direct send shape:

```bash
sx /var/home/ynezz/dev/tftp/prplos-airoha-an7581-an7581-preloader.bin \
  < /dev/ttyUSB0 > /dev/ttyUSB0
```

Then repeat with:

```bash
sx /var/home/ynezz/dev/tftp/prplos-airoha-an7581-an7581-bl31-uboot.fip \
  < /dev/ttyUSB0 > /dev/ttyUSB0
```

### `picocom`

The board note uses:

```bash
picocom -b 115200 --send-cmd "sx %f" /dev/ttyUSB0
```

Then:

1. Wait for the target prompt.
2. Press `x`.
3. Press `Ctrl+A`, then `Ctrl+S`.
4. Provide the file path.

### Python

For this workspace, `pyserial` was already available and the `xmodem` Python
package was installed locally. A safe one-shot install command is:

```bash
python3 -m pip install --user pyserial xmodem
```

That is sufficient to implement a scripted sender for `unbrk` or for ad hoc
recovery helpers without depending on `lrzsz`.

This repo now includes a helper script that automates the same two-stage flow:

```bash
python3 tools/an7581_uart_recovery.py
```

Useful options:

- `--port /dev/ttyUSB0`
- `--preloader /path/to/preloader.bin`
- `--fip /path/to/bl31-uboot.fip`
- `--resume-from-uboot`
- `--stop-at-uboot`
- `--transcript-file /tmp/an7581-recovery.log`

The helper waits for `Press x`, sends `x`, waits for repeated `C`, sends the
preloader over XMODEM, then repeats the same pattern for the FIP. It tolerates
the observed target quirk where the board can return to the next prompt before
acknowledging the final XMODEM `EOT`.

By default the helper continues from the temporary `AN7581>` prompt and performs
the permanent flash sequence over the same UART link:

```text
mmc erase 0 0x800
loadx $loadaddr 115200
mmc write $loadaddr 0x4 0xfc
loadx $loadaddr 115200
mmc write $loadaddr 0x100 0x700
reset
```

It verifies each `loadx` by reading U-Boot's `filesize` environment variable.
On this board, `filesize` is printed as hex without a `0x` prefix, so the
helper treats it as hexadecimal unconditionally. Use `--stop-at-uboot` if you
only want the RAM-resident recovery stage, or `--resume-from-uboot` if the
board is already sitting at a live `AN7581>` prompt and only the flash phase
should run.

## Evidence From Existing Local Notes

Board-specific note:

- `/var/home/ynezz/dev/prpl/nokia/an7581/docs/board-xmodem-recovery.txt`

Host-side XMODEM usage note:

- `/var/home/ynezz/dev/prpl/nokia/an7581/docs/u-boot-xmodem-recovery.txt`

Post-recovery flash note:

- `/var/home/ynezz/dev/prpl/nokia/an7581/docs/u-boot-upgrade.txt`

Existing transfer history:

- `/var/home/ynezz/minicom.log`

The minicom log shows repeated attempts that send exactly:

- `prplos-airoha-an7581-an7581-preloader.bin`
- `prplos-airoha-an7581-an7581-bl31-uboot.fip`

which matches the board note's two-stage protocol.

## Post-Recovery Flashing

Once temporary U-Boot is running, the local upgrade note uses:

```text
mmc erase 0 0x800

tftpboot prplos-airoha-an7581-an7581-preloader.bin
mmc write $loadaddr 0x4 0xfc

tftpboot prplos-airoha-an7581-an7581-bl31-uboot.fip
mmc write $loadaddr 0x100 0x700
```

The helper in this repo uses `loadx` instead of `tftpboot`, but it writes the
same offsets and block counts shown above.

That flashing phase is separate from the UART recovery protocol itself. The
recovery protocol's job ends once RAM-resident U-Boot is available.

## Implications For `unbrk`

`unbrk recover` should be built around prompt detection plus XMODEM transport:

- detect text prompts rather than binary handshakes
- send literal `x` at two specific transitions
- treat repeated `C` as XMODEM readiness
- transfer two distinct files in order
- declare success only after a usable U-Boot prompt appears

## Live Validation Status

At the time of writing, `/dev/ttyUSB0` was present and usable, but the board was
not emitting fresh recovery output, so this document is based on:

- `docs/initial-plan.md`
- the board-specific AN7581 recovery notes
- the existing minicom transfer log

To validate the exact live prompt text again, the board needs to be rebooted
back into recovery mode.
