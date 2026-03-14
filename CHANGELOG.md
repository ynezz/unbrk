# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/ynezz/unbrk/releases/tag/v0.2.0) - 2026-03-14

### Added

- fixture-backed end-to-end flash coverage
- releasing workflow documentation
- Linux hardware validation notes

### Changed

- narrow prompt-based XMODEM recovery to only tolerate EOT completion failures
- reject overflowing flash layout overrides at plan build time
- keep serial operation context in CLI error chains
- split flash fixture replay helpers for better test reuse

### Fixed

- release-plz workflow restored to canonical single-file design with
  fetch-tags: true for git_only version detection

## [0.1.0](https://github.com/ynezz/unbrk/releases/tag/v0.1.0) - 2026-03-14

### Other

- expose compiled prompt matchers for prompt reuse
- split XMODEM block and EOT retry flags
- drop unused stderr tty tracking
- drop unused cli and core dependencies
- respect target erase offsets in CLI flash plans
- harden CLI event logging and failure reporting
- add recover runtime override coverage
- wire recover into core recovery and flash flows
- harden unbrk-akl.1 CLI parser coverage
- implement unbrk-akl.1 CLI entrypoint scaffolding
- bootstrap cargo workspace and repo policy
