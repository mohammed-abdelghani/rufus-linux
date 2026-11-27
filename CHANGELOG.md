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
- CI: cargo build cache (`Swatinem/rust-cache`), a non-blocking `cargo audit`
  job, and a `cargo deny` license check with an allow-list of the licenses
  present in the tree.

### Changed

- Release binaries are stripped and built with thin LTO
  (`[profile.release]` `strip = true`, `lto = "thin"`), shrinking the binary
  by about 25%.
- The tree is rustfmt-formatted and `cargo fmt --check` is enforced in CI;
  the formatting commit is excluded from `git blame`.
- sha1, sha2 and md-5 upgraded to the digest 0.11 series: the image-hash
  dialog now hex-encodes digest output explicitly and the copy-verify path
  hashes with an explicit read loop (digest 0.11 hashers no longer
  implement `std::io::Write`). Digest results are unchanged — the
  known-vector tests still pass.

None of the entries above changes observable runtime behavior: packaging
size, source formatting, developer docs and CI are the only visible changes,
and the digest upgrade produces byte-identical hash results (verified by the
known-vector tests).

### Fixed

- The HDIO_GETGEO ioctl buffer (`HdGeometry` in `disk.rs`) was smaller than
  the kernel's `struct hd_geometry` on 64-bit Linux, so a successful ioctl
  wrote 4 bytes past the buffer; the Rust mirror now matches the kernel
  layout (16 bytes on LP64), verified by a layout test.

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
