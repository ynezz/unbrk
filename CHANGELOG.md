# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/ynezz/unbrk/releases/tag/v0.1.0) - 2026-03-14

### Added

- XMODEM-based serial flash and recovery for Airoha IoT devices
- fixture-backed end-to-end flash and recovery test coverage
- Linux hardware validation with real transcript fixtures
- serial port auto-detection scaffolding
- total size verification for flash transfers
- interactive console handoff after recovery
- human-readable progress output with fancy spinner and banner
- releasing workflow with cargo-dist cross-platform binaries
- shell and PowerShell installer scripts
- SHA-256 checksums and GitHub artifact attestations

### Other

- expose compiled prompt matchers for prompt reuse
- split XMODEM block and EOT retry flags
- reject overflowing flash layout overrides at plan build time
- keep serial operation context in CLI error chains
