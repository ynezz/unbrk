# Valyrian Transcript Fixtures

These fixtures were extracted directly from
`docs/valyrian-end-to-end.log`, which is our current ground-truth capture for
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

Known limitation:

- This log does not contain the outbound XMODEM payload bytes themselves. It
  preserves the observed CRC-ready bursts and the post-transfer console
  summaries, which is enough to bootstrap prompt and orchestration tests, but
  later hardware captures should add payload-level fixtures if we need to
  replay exact XMODEM traffic.
