#!/usr/bin/env python3

from __future__ import annotations

import argparse
from contextlib import nullcontext
import math
import re
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO

try:
    import serial
except ImportError as exc:  # pragma: no cover - import guard
    raise SystemExit(
        "Missing dependency: pyserial\n"
        "Install it with:\n"
        "  python3 -m pip install --user pyserial xmodem"
    ) from exc

try:
    from xmodem import XMODEM
except ImportError as exc:  # pragma: no cover - import guard
    raise SystemExit(
        "Missing dependency: xmodem\n"
        "Install it with:\n"
        "  python3 -m pip install --user pyserial xmodem"
    ) from exc


DEFAULT_PRELOADER = (
    Path.home() / "dev/tftp/prplos-airoha-an7581-an7581-preloader.bin"
)
DEFAULT_FIP = (
    Path.home() / "dev/tftp/prplos-airoha-an7581-an7581-bl31-uboot.fip"
)
DEFAULT_ERASE_BLOCK_COUNT = 0x800
DEFAULT_PRELOADER_START_BLOCK = 0x4
DEFAULT_PRELOADER_BLOCK_COUNT = 0xFC
DEFAULT_FIP_START_BLOCK = 0x100
DEFAULT_FIP_BLOCK_COUNT = 0x700
TOTAL_SIZE_RE = re.compile(
    r"Total Size\s*=\s*0x([0-9a-fA-F]+)\s*=\s*([0-9]+)\s*Bytes"
)


class RecoveryError(RuntimeError):
    """Raised when the board does not follow the expected recovery flow."""


@dataclass(slots=True)
class TransferResult:
    ok: bool
    expected_packets: int
    file_size: int


class SerialConsole:
    def __init__(
        self,
        port: str,
        baudrate: int,
        echo: bool,
        transcript: BinaryIO | None,
    ) -> None:
        self.port = port
        self.echo = echo
        self.transcript = transcript
        self._buffer = bytearray()
        self._serial = serial.Serial(
            port=port,
            baudrate=baudrate,
            bytesize=serial.EIGHTBITS,
            parity=serial.PARITY_NONE,
            stopbits=serial.STOPBITS_ONE,
            timeout=0.2,
            xonxoff=False,
            rtscts=False,
            dsrdtr=False,
        )

    def close(self) -> None:
        self._serial.close()

    def mark(self) -> int:
        return len(self._buffer)

    def text_since(self, start: int) -> str:
        return self._buffer[start:].decode("utf-8", errors="replace")

    def _record_rx(self, data: bytes) -> None:
        if not data:
            return
        self._buffer.extend(data)
        if self.echo:
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
        if self.transcript is not None:
            self.transcript.write(data)
            self.transcript.flush()

    def _read(self, size: int = 4096) -> bytes:
        data = self._serial.read(size)
        self._record_rx(data)
        return data

    def send(self, data: bytes) -> None:
        self._serial.write(data)
        self._serial.flush()

    def send_line(self, line: str) -> None:
        self.send(line.encode("ascii") + b"\n")

    def wait_for_text(self, label: str, pattern: re.Pattern[str], start: int, timeout: float) -> str:
        deadline = time.monotonic() + timeout
        while True:
            decoded = self._buffer[start:].decode("utf-8", errors="replace")
            match = pattern.search(decoded)
            if match:
                return match.group(0)
            if time.monotonic() >= deadline:
                tail = decoded[-200:] or "<no console output>"
                raise RecoveryError(
                    f"Timed out waiting for {label}. Last console text:\n{tail}"
                )
            if not self._read():
                time.sleep(0.05)

    def wait_for_crc_readiness(self, start: int, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while True:
            if b"CCC" in self._buffer[start:]:
                return
            if time.monotonic() >= deadline:
                tail = self._buffer[start:][-200:].decode("utf-8", errors="replace")
                raise RecoveryError(
                    "Timed out waiting for XMODEM CRC readiness ('C'). "
                    f"Last console text:\n{tail or '<no console output>'}"
                )
            if not self._read():
                time.sleep(0.05)

    def send_xmodem(
        self,
        image_name: str,
        image_path: Path,
        timeout: float,
        retry: int,
    ) -> TransferResult:
        packet_size = 128
        file_size = image_path.stat().st_size
        expected_packets = math.ceil(file_size / packet_size)
        last_report_at = 0.0
        progress_printed = False

        def getc(size: int, timeout_seconds: float = 1.0) -> bytes | None:
            previous_timeout = self._serial.timeout
            self._serial.timeout = timeout_seconds
            try:
                data = self._serial.read(size)
            finally:
                self._serial.timeout = previous_timeout
            self._record_rx(data)
            return data or None

        def putc(data: bytes, timeout_seconds: float = 1.0) -> int:
            del timeout_seconds
            written = self._serial.write(data)
            self._serial.flush()
            return written

        def callback(total_packets: int, success_count: int, error_count: int) -> None:
            del total_packets
            nonlocal last_report_at, progress_printed
            now = time.monotonic()
            should_print = (
                error_count > 0
                or success_count >= expected_packets
                or now - last_report_at >= 0.5
            )
            if not should_print:
                return
            percent = min(100.0, success_count * 100.0 / expected_packets)
            print(
                f"\r{image_name}: {percent:5.1f}% "
                f"({success_count}/{expected_packets} packets, errors={error_count})",
                end="",
                file=sys.stderr,
                flush=True,
            )
            progress_printed = True
            last_report_at = now

        modem = XMODEM(getc, putc, mode="xmodem")
        with image_path.open("rb") as stream:
            ok = modem.send(
                stream,
                retry=retry,
                timeout=timeout,
                quiet=True,
                callback=callback,
            )

        if progress_printed:
            print(file=sys.stderr)

        return TransferResult(
            ok=ok,
            expected_packets=expected_packets,
            file_size=file_size,
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Drive the Airoha AN7581 UART recovery flow over /dev/ttyUSB0 "
            "using prompt detection plus XMODEM."
        )
    )
    parser.add_argument("--port", default="/dev/ttyUSB0", help="Serial device")
    parser.add_argument(
        "--baud",
        type=int,
        default=115200,
        help="UART baud rate (default: 115200)",
    )
    parser.add_argument(
        "--preloader",
        type=Path,
        default=DEFAULT_PRELOADER,
        help=f"Preloader image path (default: {DEFAULT_PRELOADER})",
    )
    parser.add_argument(
        "--fip",
        type=Path,
        default=DEFAULT_FIP,
        help=f"BL31 + U-Boot FIP path (default: {DEFAULT_FIP})",
    )
    parser.add_argument(
        "--prompt-timeout",
        type=float,
        default=30.0,
        help="Seconds to wait for prompts and CRC readiness (default: 30)",
    )
    parser.add_argument(
        "--packet-timeout",
        type=float,
        default=10.0,
        help="Seconds to wait for each XMODEM packet response (default: 10)",
    )
    parser.add_argument(
        "--xmodem-retry",
        type=int,
        default=16,
        help="Retry count passed to the xmodem sender (default: 16)",
    )
    parser.add_argument(
        "--uboot-prompt",
        default=r"AN7581>",
        help="Regex used to detect the RAM-resident U-Boot prompt",
    )
    parser.add_argument(
        "--resume-from-uboot",
        action="store_true",
        help="Skip the BootROM recovery phase and start from an existing U-Boot prompt",
    )
    parser.add_argument(
        "--stop-at-uboot",
        action="store_true",
        help="Stop after reaching the RAM-resident U-Boot prompt",
    )
    parser.add_argument(
        "--command-timeout",
        type=float,
        default=30.0,
        help="Seconds to wait for U-Boot commands to finish (default: 30)",
    )
    parser.add_argument(
        "--reset-timeout",
        type=float,
        default=20.0,
        help="Seconds to wait for post-flash reset output (default: 20)",
    )
    parser.add_argument(
        "--erase-block-count",
        type=parse_u_boot_int,
        default=DEFAULT_ERASE_BLOCK_COUNT,
        help=(
            "MMC block count for the bootloader erase step "
            f"(default: {hex(DEFAULT_ERASE_BLOCK_COUNT)})"
        ),
    )
    parser.add_argument(
        "--preloader-start-block",
        type=parse_u_boot_int,
        default=DEFAULT_PRELOADER_START_BLOCK,
        help=(
            "MMC start block for the preloader write "
            f"(default: {hex(DEFAULT_PRELOADER_START_BLOCK)})"
        ),
    )
    parser.add_argument(
        "--preloader-block-count",
        type=parse_u_boot_int,
        default=DEFAULT_PRELOADER_BLOCK_COUNT,
        help=(
            "MMC block count for the preloader write "
            f"(default: {hex(DEFAULT_PRELOADER_BLOCK_COUNT)})"
        ),
    )
    parser.add_argument(
        "--fip-start-block",
        type=parse_u_boot_int,
        default=DEFAULT_FIP_START_BLOCK,
        help=(
            "MMC start block for the FIP write "
            f"(default: {hex(DEFAULT_FIP_START_BLOCK)})"
        ),
    )
    parser.add_argument(
        "--fip-block-count",
        type=parse_u_boot_int,
        default=DEFAULT_FIP_BLOCK_COUNT,
        help=(
            "MMC block count for the FIP write "
            f"(default: {hex(DEFAULT_FIP_BLOCK_COUNT)})"
        ),
    )
    parser.add_argument(
        "--transcript-file",
        type=Path,
        help="Optional path for raw RX transcript capture",
    )
    parser.add_argument(
        "--no-echo",
        action="store_true",
        help="Do not mirror serial RX bytes to stdout",
    )
    return parser.parse_args()


def parse_u_boot_int(value: str) -> int:
    return int(value, 0)


def ensure_file(path: Path, label: str) -> None:
    if not path.is_file():
        raise SystemExit(f"{label} does not exist: {path}")


def info(message: str) -> None:
    print(message, file=sys.stderr, flush=True)


def tail_text(text: str, limit: int = 400) -> str:
    return text[-limit:] if text else "<no console output>"


def parse_u_boot_hex(value: str) -> int:
    if value.lower().startswith("0x"):
        return int(value, 0)
    return int(value, 16)


def run_uboot_command(
    console: SerialConsole,
    prompt: re.Pattern[str],
    command: str,
    timeout: float,
) -> str:
    info(f"U-Boot> {command}")
    start = console.mark()
    console.send_line(command)
    console.wait_for_text(
        f"the U-Boot prompt after `{command}`",
        prompt,
        start=start,
        timeout=timeout,
    )
    return console.text_since(start)


def require_output(
    output: str,
    pattern: re.Pattern[str],
    label: str,
) -> re.Match[str]:
    match = pattern.search(output)
    if match is None:
        raise RecoveryError(
            f"{label} did not report success. Last console text:\n"
            f"{tail_text(output)}"
        )
    return match


def verify_filesize(
    console: SerialConsole,
    prompt: re.Pattern[str],
    timeout: float,
    image_path: Path,
) -> None:
    output = run_uboot_command(console, prompt, "printenv filesize", timeout)
    match = require_output(
        output,
        re.compile(r"filesize=([0-9a-fA-Fx]+)"),
        "U-Boot filesize",
    )
    actual_size = parse_u_boot_hex(match.group(1))
    expected_size = image_path.stat().st_size
    if actual_size != expected_size:
        raise RecoveryError(
            f"U-Boot reported filesize {hex(actual_size)} for {image_path.name}, "
            f"but the host file is {hex(expected_size)}."
        )


def transfer_via_loadx(
    console: SerialConsole,
    prompt: re.Pattern[str],
    image_name: str,
    image_path: Path,
    baud: int,
    command_timeout: float,
    packet_timeout: float,
    retry: int,
) -> None:
    info(f"U-Boot> loadx $loadaddr {baud}")
    start = console.mark()
    console.send_line(f"loadx $loadaddr {baud}")
    console.wait_for_crc_readiness(
        start=start,
        timeout=command_timeout,
    )

    info(f"Sending {image_name} via XMODEM: {image_path}")
    result = console.send_xmodem(
        image_name=image_name,
        image_path=image_path,
        timeout=packet_timeout,
        retry=retry,
    )
    if not result.ok:
        info(
            f"{image_name} transfer did not end with a clean ACK. "
            "Checking for the U-Boot prompt because this target can move on "
            "before ACKing the final EOT."
        )

    console.wait_for_text(
        f"the U-Boot prompt after loading {image_name}",
        prompt,
        start=start,
        timeout=command_timeout,
    )
    output = console.text_since(start)
    size_match = TOTAL_SIZE_RE.search(output)
    if size_match is not None:
        loaded_size = int(size_match.group(2))
        expected_size = image_path.stat().st_size
        if loaded_size != expected_size:
            raise RecoveryError(
                f"loadx reported {loaded_size} bytes for {image_path.name}, "
                f"but the host file is {expected_size} bytes."
            )

    verify_filesize(console, prompt, command_timeout, image_path)


def ensure_uboot_prompt(
    console: SerialConsole,
    prompt: re.Pattern[str],
    timeout: float,
) -> None:
    info("Waiting for an active U-Boot prompt.")
    start = console.mark()
    console.send(b"\r\n")
    console.wait_for_text(
        "an active U-Boot prompt",
        prompt,
        start=start,
        timeout=timeout,
    )


def flash_from_uboot(
    console: SerialConsole,
    args: argparse.Namespace,
    prompt: re.Pattern[str],
) -> None:
    ensure_uboot_prompt(console, prompt, args.command_timeout)

    loadaddr_output = run_uboot_command(
        console,
        prompt,
        "printenv loadaddr",
        args.command_timeout,
    )
    loadaddr_match = require_output(
        loadaddr_output,
        re.compile(r"loadaddr=([0-9a-fA-Fx]+)"),
        "U-Boot loadaddr",
    )
    info(f"Using loadaddr {loadaddr_match.group(1)} for loadx/mmc writes.")

    erase_output = run_uboot_command(
        console,
        prompt,
        f"mmc erase 0 {hex(args.erase_block_count)}",
        args.command_timeout,
    )
    require_output(
        erase_output,
        re.compile(r"blocks erased:\s+OK", re.IGNORECASE),
        "MMC erase",
    )

    transfer_via_loadx(
        console,
        prompt,
        image_name="preloader",
        image_path=args.preloader,
        baud=args.baud,
        command_timeout=args.command_timeout,
        packet_timeout=args.packet_timeout,
        retry=args.xmodem_retry,
    )
    preloader_write_output = run_uboot_command(
        console,
        prompt,
        (
            "mmc write $loadaddr "
            f"{hex(args.preloader_start_block)} "
            f"{hex(args.preloader_block_count)}"
        ),
        args.command_timeout,
    )
    require_output(
        preloader_write_output,
        re.compile(r"blocks written:\s+OK", re.IGNORECASE),
        "preloader MMC write",
    )

    transfer_via_loadx(
        console,
        prompt,
        image_name="fip",
        image_path=args.fip,
        baud=args.baud,
        command_timeout=args.command_timeout,
        packet_timeout=args.packet_timeout,
        retry=args.xmodem_retry,
    )
    fip_write_output = run_uboot_command(
        console,
        prompt,
        (
            "mmc write $loadaddr "
            f"{hex(args.fip_start_block)} "
            f"{hex(args.fip_block_count)}"
        ),
        args.command_timeout,
    )
    require_output(
        fip_write_output,
        re.compile(r"blocks written:\s+OK", re.IGNORECASE),
        "FIP MMC write",
    )

    info("U-Boot> reset")
    reset_mark = console.mark()
    console.send_line("reset")
    console.wait_for_text(
        "post-flash reset output",
        re.compile(r"EcoNet System Reset|Press x|U-Boot"),
        start=reset_mark,
        timeout=args.reset_timeout,
    )


def reach_ram_uboot(
    console: SerialConsole,
    args: argparse.Namespace,
    initial_prompt: re.Pattern[str],
    second_prompt: re.Pattern[str],
    uboot_prompt: re.Pattern[str],
) -> None:
    info(
        "Board should be powered on into recovery mode with the "
        "middle reset button held. Waiting for 'Press x'..."
    )
    initial_mark = console.mark()
    console.wait_for_text(
        "the initial recovery prompt",
        initial_prompt,
        start=initial_mark,
        timeout=args.prompt_timeout,
    )

    info("Initial prompt detected. Sending 'x' for preloader stage.")
    console.send(b"x")

    crc_mark = console.mark()
    console.wait_for_crc_readiness(
        start=crc_mark,
        timeout=args.prompt_timeout,
    )

    info(f"Sending preloader via XMODEM: {args.preloader}")
    preloader_mark = console.mark()
    preloader_result = console.send_xmodem(
        image_name="preloader",
        image_path=args.preloader,
        timeout=args.packet_timeout,
        retry=args.xmodem_retry,
    )

    if not preloader_result.ok:
        info(
            "Preloader transfer did not end with a clean ACK. "
            "Checking for the next prompt because this target can "
            "jump ahead before acknowledging the final EOT."
        )

    console.wait_for_text(
        "the BL31 + U-Boot FIP prompt",
        second_prompt,
        start=preloader_mark,
        timeout=args.prompt_timeout,
    )

    info("Stage 2 prompt detected. Sending 'x' for FIP stage.")
    console.send(b"x")

    crc_mark = console.mark()
    console.wait_for_crc_readiness(
        start=crc_mark,
        timeout=args.prompt_timeout,
    )

    info(f"Sending BL31 + U-Boot FIP via XMODEM: {args.fip}")
    fip_mark = console.mark()
    fip_result = console.send_xmodem(
        image_name="fip",
        image_path=args.fip,
        timeout=args.packet_timeout,
        retry=args.xmodem_retry,
    )

    if not fip_result.ok:
        info(
            "FIP transfer did not end with a clean ACK. "
            "Checking for a U-Boot prompt because this target can "
            "return to the console before ACKing the final EOT."
        )

    console.wait_for_text(
        "the RAM-resident U-Boot prompt",
        uboot_prompt,
        start=fip_mark,
        timeout=args.prompt_timeout,
    )


def main() -> int:
    args = parse_args()
    ensure_file(args.preloader, "Preloader image")
    ensure_file(args.fip, "FIP image")

    transcript_handle: BinaryIO | None = None
    if args.transcript_file is not None:
        transcript_handle = args.transcript_file.open("wb")

    initial_prompt = re.compile(r"Press x")
    second_prompt = re.compile(r"Press x to load BL31 \+ U-Boot FIP")
    uboot_prompt = re.compile(args.uboot_prompt)

    try:
        with (
            transcript_handle
            if transcript_handle is not None
            else nullcontext()
        ) as transcript:
            console = SerialConsole(
                port=args.port,
                baudrate=args.baud,
                echo=not args.no_echo,
                transcript=transcript,
            )
            try:
                if args.resume_from_uboot:
                    info("Skipping BootROM recovery phase and resuming from U-Boot.")
                else:
                    reach_ram_uboot(
                        console,
                        args,
                        initial_prompt,
                        second_prompt,
                        uboot_prompt,
                    )
                    info("Recovery transfer complete. U-Boot prompt detected.")

                if args.stop_at_uboot:
                    info("Stopping at the RAM-resident U-Boot prompt as requested.")
                    return 0

                flash_from_uboot(console, args, uboot_prompt)
                info("End-to-end recovery complete.")
                return 0
            finally:
                console.close()
    except RecoveryError as exc:
        info(str(exc))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
