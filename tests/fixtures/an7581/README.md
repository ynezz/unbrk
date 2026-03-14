# AN7581 Transcript Fixtures

These fixtures were extracted directly from
`docs/an7581-end-to-end.log`, which is our current ground-truth capture for
the documented happy path.

The split keeps stage boundaries explicit so later parser and state-machine
tests can load only the portion they need:

- `full-session.bin`: complete raw transcript capture.
- `happy-path-stage1-prompt.bin`: initial `Press x` prompt.
- `happy-path-stage1-crc-readiness.bin`: first XMODEM CRC-ready burst.
- `happy-path-interstage-chatter.bin`: DRAM and boot chatter between the two
  BootROM stages.
- `happy-path-stage2-prompt.bin`: second `Press x to load BL31 + U-Boot FIP`
  prompt.
- `happy-path-stage2-crc-readiness.bin`: second XMODEM CRC-ready burst.
- `happy-path-uboot-boot-noise.bin`: post-FIP boot noise, including ANSI menu
  control sequences.
- `happy-path-uboot-prompts.bin`: first visible `AN7581>` prompts after boot.
- `flash-preloader-sequence.bin`: U-Boot preloader flash sequence from
  `printenv loadaddr` through the preloader write.
- `flash-fip-sequence.bin`: U-Boot FIP flash sequence from `loadx` through the
  FIP write.
- `reset-evidence.bin`: reset command plus the observed reset evidence.

Additional real-hardware slices captured on 2026-03-14 live under the same
directory:

- `real-stage1-leading-garbage.bin`: stage-1 prompt preceded by stale bytes
  from a real UART capture.
- `real-stage2-notice-and-prompt.bin`: stage-2 prompt preceded by observed
  boot notice chatter.
- `real-uboot-ansi-prompt.bin`: final `AN7581>` prompt with ANSI cursor
  control bytes still present.
- `real-preloader-echoed-x-crc.bin`: echoed `x` followed by the real `CCC`
  readiness burst.

Known limitation:

- This log does not contain the outbound XMODEM payload bytes themselves. It
  preserves the observed CRC-ready bursts and the post-transfer console
  summaries, which is enough to bootstrap prompt and orchestration tests, but
  later hardware captures should add payload-level fixtures if we need to
  replay exact XMODEM traffic.
