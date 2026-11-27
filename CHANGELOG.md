# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- README: independence/name note, Requirements and FAQ/Troubleshooting
  sections, project-status paragraph, and a verification caveat for
  third-party tool comparisons.
- Community and maintenance files: `CONTRIBUTING.md`, `SECURITY.md`, GitHub
  issue templates, pull request template, Dependabot configuration, and
  `ASSETS.md` documenting bundled third-party assets.

## [0.1.0] - 2026-11-22

Initial public release.

### Added

- Rufus-compatible bootable USB creator for Linux, written in Rust with a
  self-contained `winit` + `softbuffer` GUI (no GTK, Qt, or Electron).
- Write modes: Disk or ISO image (MBR/GPT, FAT32, file-copied install with
  automatic `wimlib-imagex` split for `install.wim` > 4 GiB), raw/DD image
  (including gzip-compressed images), non-bootable, FreeDOS, and Windows To
  Go with Rufus's GPT/MBR boot layouts and BCD stores.
- Windows User Experience dialog: TPM/Secure Boot/RAM check bypasses,
  online-account bypass, local account, and `unattend.xml` customization.
- Official ISO downloads for Windows 11/10 and eight Linux distributions,
  with SHA-256 verification.
- 38-language UI with OpenType shaping and bidi reordering.
- Rufus extras: Rufus MBR with BIOS ID masquerade, old-BIOS fixes,
  UEFI:NTFS bootloader, bad-blocks check, casper-rw persistence, extended
  label/icon files, image hashes, hidden settings dialog, and inline log.
- Safety features: write confirmation, system-disk refusal (`--force`
  override), mid-write STOP, and optional per-file SHA-256 verify.
- Native FAT32 formatter, in-process MBR/GPT writers, ISO9660 parser
  (Rock Ridge + El Torito), and native syslinux/GRUB2 boot patching.
- USB, NVMe, and MMC/SD device detection via sysfs.
- CI builds, lints, and tests on every push; release workflow produces
  `.tar.gz`, `.deb`, and `.rpm` packages for x86_64, i686, ARMv7, and ARM64.
- 128 unit tests; zero clippy warnings.
