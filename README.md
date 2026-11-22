<div align="center">

# rufus-linux

**Rufus, rebuilt from scratch in Rust — for Linux.**

Create bootable USB drives from ISO images with the same dialog, the same modes
and the same knobs as [Rufus](https://github.com/pbatard/rufus) — natively on Linux,
in a single 9 MB binary. No GTK. No Qt. No Electron.

[![CI](https://github.com/mohammed-abdelghani/rufus-linux/actions/workflows/ci.yml/badge.svg)](https://github.com/mohammed-abdelghani/rufus-linux/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mohammed-abdelghani/rufus-linux?color=blue&label=release)](https://github.com/mohammed-abdelghani/rufus-linux/releases/latest)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux-black?logo=linux&logoColor=white)](#installation)
[![Language](https://img.shields.io/badge/written%20in-Rust-000?logo=rust&logoColor=white&color=orange)](#development)
[![UI languages](https://img.shields.io/badge/UI%20languages-38-blueviolet)](#localization)

<br>

<img src="https://cdn.jsdelivr.net/gh/mohammed-abdelghani/rufus-linux@main/assets/screenshot-main.png" alt="rufus-linux main window — the Rufus dialog on Linux" width="620">

<br>

`rufus-linux gui` — the Rufus dialog, pixel-for-pixel in spirit, on Linux

</div>

---

## Why

Every Linux user knows the pain: Rufus is the best USB-imaging tool ever made — and
it only runs on Windows. The Linux alternatives each cover a slice of the job, and
none of them cover the whole workflow:

- **balenaEtcher** flashes raw images — but can't lay out a proper partitioned,
  file-copied Windows/Linux install stick.
- **WoeUSB-ng** handles one thing: a subset of Windows ISOs.
- **Ventoy** takes over your drive with its own bootloader and menu — great for
  multiboot, but it's not a real installer stick.

**rufus-linux** ports the entire Rufus workflow to Linux: the write modes, the
partition layouts, the Windows To Go engine, the hidden settings, the bad-block
tester, even the 38-language interface — implemented from scratch in safe Rust
over sysfs and raw block devices, with a self-contained GUI that renders its own
Win32-style theme.

- **Rufus dialog parity** — device combo, boot selection, image options, partition
  scheme, advanced drive properties, format options, the destroy-confirmation and
  the green READY bar
- **Windows To Go on Linux** — applies a Windows edition to an NTFS volume with
  Rufus's exact GPT/MBR boot layout, BCD stores and SanPolicy 4
- **Official ISO downloads in-app** — Windows 11/10 straight from Microsoft (Fido
  parity) plus 8 major Linux distros, with SHA-256 verification
- **38-language UI** — real OpenType shaping and bidi reordering (Arabic, Hebrew,
  Thai, CJK, Indic…)
- **Battle-tested internals** — 128 unit tests, zero clippy warnings, CI on every push
- **Tiny and self-contained** — one 9 MB binary; the only GUI stack is `winit` +
  `softbuffer`

> [!IMPORTANT]
> Writing to a block device is destructive by definition. rufus-linux refuses to
> touch the device hosting your root filesystem and asks for confirmation before
> every write — but always double-check the selected device.

---

## Contents

- [Features](#features)
- [Installation](#installation)
- [Usage](#usage)
- [How it compares](#how-it-compares)
- [Localization](#localization)
- [Roadmap](#roadmap)
- [Development](#development)
- [Acknowledgements](#acknowledgements)
- [License](#license)

---

## Features

### Write modes

The same five modes as Rufus's *Boot selection* + *Image option* combos:

| Mode | What it does |
|---|---|
| **Disk or ISO image** *(default)* | MBR or GPT partition table → FAT32 → copies the ISO9660 tree (Rock Ridge names preserved) → boot files. `install.wim` > 4 GiB is split automatically via `wimlib-imagex` to satisfy FAT32's limit |
| **Raw image (DD)** | Byte-for-byte write for `.img` and non-ISO images, with on-the-fly gzip decompression for `.img.gz` |
| **Non bootable** | Wipe + partition + format only — no image required |
| **FreeDOS** | FAT32 volume with the FreeDOS boot files and the ms-sys DOS boot record — exactly like Rufus |
| **Windows To Go** | Applies a selected Windows *edition* from the WIM onto NTFS with `wimlib-imagex`, then lays out the drive exactly like Rufus: GPT = 260 MB ESP + 128 MB MSR + NTFS with `bootmgfw.efi` + retargeted BCD; MBR = `bootmgr`/`\Boot\BCD` for BIOS **and** `\EFI\Boot\bootx64.efi` for UEFI (via the trailing `uefi-ntfs` partition — Rufus's `bcdboot /f ALL` parity). Includes the Rufus-parity *Version selection* dialog and the removable-media warning (MSG_098) |

### Windows installation extras

- **Windows User Experience dialog** — before writing a Windows ISO, START opens
  Rufus's customization panel: TPM / Secure Boot / RAM check bypass (LabConfig keys
  injected into `boot.wim`), online-account bypass, data-collection opt-out, a local
  account named after the current user, regional options from the session locale,
  and *Prevent Windows To Go from accessing internal disks* (SanPolicy 4) — all
  applied through a generated `unattend.xml` in `\Windows\Panther`.
- **Rufus-style suggestions** — ISOs with a > 4 GiB `install.wim` switch to NTFS
  automatically; UEFI-only ISOs enable UEFI-only targeting.
- **UEFI:NTFS** — for NTFS/exFAT targets a trailing partition carries the embedded
  `uefi-ntfs.img` UEFI driver so firmware can read the volume.
- **Use Rufus MBR with BIOS ID** — the "AKEO" MBR vs. the Windows 7 MBR, plus the
  disk-ID masquerade (0x81–0x87) for old BIOSes, with Rufus's auto-checking rules.
- **Add fixes for old BIOSes** — the extra 0xEA compatibility partition at the end
  of the disk, and track-aligned first partitions on MBR.
- **Extended label & icon** — `autorun.inf` (UTF-16 LE) + `autorun.ico` so long
  volume labels and drive icons show up in file managers.

### ISO downloads

The split-SELECT dropdown opens Rufus 4.6's download flow:

- **Windows 11 / 10** straight from Microsoft's official software-download servers —
  the same request chain as Rufus's Fido script (release → edition → language →
  architecture, system language and x64 preselected).
- **Linux distros**: Ubuntu, Debian, Fedora, Linux Mint, Arch Linux,
  openSUSE Tumbleweed, Pop!_OS, Alpine Linux — filenames are resolved at download
  time from each distro's published checksum files, so point releases never break
  the catalog.
- Progress bar, STOP button, SHA-256 verification, auto-select on finish.

### Safety

- Confirms before every write; **STOP** button cancels a write mid-flight.
- Refuses to touch the device that hosts the running root filesystem
  (overridable with `--force`).
- Optional `--verify`: full SHA-256 comparison of every copied file.
- **Bad-blocks check** — faithful port of Rufus's `badblocks.c`: 1/2/4 passes with
  the original SLC/MLC/TLC pattern sets, aborts after 256 bad blocks.

### Under the hood

- Native **FAT32 formatter** (no `mkfs.vfat`), incl. 4Kn devices for raw writes;
  NTFS/exFAT via `mkfs.ntfs`/`mkfs.exfat`.
- **MBR + GPT** written in-process with random disk signatures.
- ISO9660 parser with **Rock Ridge (SUSP)** and **El Torito** catalog detection.
- **Native syslinux installer** — embedded `ldlinux` v6 loader + VBR template,
  patched in place after the FAT walk; GRUB2 ISOs get `core.img` blocklists
  patched instead of a raw write.
- **Image hashes** — MD5/SHA1/SHA256 computed in the background (SHA-512 via Alt-H).
- **Settings dialog** — Rufus's hidden settings ported and persisted
  (`~/.config/rufus-linux/settings.json`): extra hashes, Rufus MBR, large FAT32
  (> 32 GiB), preserve timestamps, persistent log, USB enumeration debug — each
  with its original magic key (**Alt-H**, **Alt-A**, **Alt-L**, **Alt-T**,
  **Alt-.**, **Ctrl-P**).
- **Inline log panel** (Ctrl-L) with Clear / Save / Close, session log written to
  `~/.config/rufus-linux/rufus.log`, level colors matching the original.
- **Linux persistence** — casper-rw partition size configurable for Ubuntu-family ISOs.
- Detects USB, **NVMe** (`nvme0n1`) and **MMC/SD** (`mmcblk0`) devices via sysfs,
  with correct per-type partition naming.

---

## Installation

### From a release (prebuilt)

Every release ships CI-built packages for **x86_64, i686 (32-bit x86), ARMv7 and
ARM64**, with a `SHA256SUMS.txt` covering all files:

| Package | Fits | Architectures |
|---|---|---|
| `.tar.gz` | any Linux — extract and run | x86_64, i686, armv7, aarch64 |
| `.deb` | Debian, Ubuntu, Mint, Pop!_OS… | amd64, i386, arm64, armhf |
| `.rpm` | Fedora, RHEL, openSUSE, Mageia… | x86_64, i686, aarch64, armv7hl |

```sh
# tarball
tar xzf rufus-linux-v0.1.0-x86_64.tar.gz
sudo ./rufus-linux

# deb
sudo apt install ./rufus-linux_0.1.0-5_amd64.deb

# rpm
sudo dnf install ./rufus-linux-0.1.0-5.x86_64.rpm    # or zypper in
```

The prebuilt binaries need **glibc ≥ 2.34** (Ubuntu 22.04+, Debian 12+, Fedora 36+,
Raspberry Pi OS Bookworm, Arch, openSUSE Tumbleweed…) and X11 — on Wayland
sessions they run under XWayland like most desktop apps.

At runtime, writes need root or `pkexec` (the GUI relaunches itself through
`pkexec` automatically).

Optional runtime helpers: `wimlib-imagex` (Windows `install.wim` > 4 GiB /
Windows To Go), `mkfs.ntfs` + `mkfs.exfat` (NTFS/exFAT targets), `zenity`
(file picker), `curl` (ISO downloads — present on essentially every distro).

### From source

```sh
# Debian/Ubuntu
sudo apt install build-essential pkg-config libx11-dev libx11-xcb-dev
# Fedora
sudo dnf install gcc pkgconf-devel libX11-devel libX11-xcb-devel

cargo install --locked --path .
# or: cargo build --locked --release  → target/release/rufus-linux
```

No C/C++ GUI libraries, no interpretive runtime, no vendored tree — the toolchain
plus X11 headers is all it needs.

---

## Usage

### GUI

```sh
sudo rufus-linux                # the Rufus dialog
sudo rufus-linux gui --locale ar-SA   # force a locale
```

Pick a device, pick an ISO (or the *Download* entry), pick a partition scheme, press
**START**. Advanced drive properties (file system, cluster size, volume label,
bad blocks, persistence) unfold exactly where Rufus puts them.

### CLI

```sh
sudo rufus-linux list --all                     # list devices (incl. non-USB)
sudo rufus-linux iso-info ubuntu.iso            # inspect an image
sudo rufus-linux disk-info /dev/sdb             # read-only table inspect
sudo rufus-linux write /dev/sdb ubuntu.iso --label UBUNTU
sudo rufus-linux write /dev/sdb image.img       # raw DD mode
sudo rufus-linux write /dev/sdb image.img.gz    # raw, gzip-decompressed
sudo rufus-linux write /dev/sdb win.iso --gpt --verify
sudo rufus-linux write /dev/sdb win.iso --old-bios-fixes --badblocks 1
sudo rufus-linux write /dev/sdb win.iso --fs ntfs --image-option wtg   # Windows To Go
sudo rufus-linux write /dev/sdb ubuntu.iso --persistent 1024
sudo rufus-linux write /dev/sdb --mode freedos --yes
```

<details>
<summary><strong>All write flags</strong></summary>

| Flag | Meaning |
|---|---|
| `--label <NAME>` | Volume label |
| `--gpt` | GPT partition scheme (default MBR) |
| `--efi-only` | Target UEFI systems only |
| `--fs <fat32\|ntfs\|exfat>` | File system |
| `--mode <iso\|raw\|non-bootable\|freedos>` | Write mode |
| `--image-option <install\|wtg>` | Windows: standard install or Windows To Go |
| `--persistent <MB>` | casper-rw persistence partition |
| `--verify` | SHA-256 verify every copied file |
| `--badblocks <0-4>` | Destructive bad-blocks test passes |
| `--old-bios-fixes` | BIOS-compatibility partition + track alignment |
| `--no-rufus-mbr` | Windows 7 MBR instead of the Rufus MBR |
| `--disk-id <0x80-0x87>` | BIOS disk-ID masquerade |
| `--no-autorun` | Skip `autorun.inf`/icon creation |
| `--yes` | Skip the y/N confirmation |
| `--force` | Override the system-disk refusal |

</details>

---

## How it compares

| Feature | **rufus-linux** | Rufus | balenaEtcher | WoeUSB-ng | Ventoy |
|---|:---:|:---:|:---:|:---:|:---:|
| Runs on Linux | Yes | No (Windows only) | Yes | Yes | Yes |
| Partitioned, file-copied ISO install | Yes | Yes | No (raw only) | Partial (Windows ISOs) | Partial (menu-based) |
| Raw/DD image flashing | Yes | Yes | Yes | No | No |
| Windows To Go | Yes | Yes | No | No | Partial (plugin) |
| FreeDOS stick | Yes | Yes | No | No | No |
| In-app official ISO downloads | Yes (Win + Linux) | Yes (Win) | No | No | No |
| TPM/Secure Boot bypasses | Yes | Yes | No | No | No |
| UEFI:NTFS bootloader | Yes | Yes | No | No | n/a |
| Bad-blocks test | Yes | Yes | No | No | No |
| casper-rw persistence | Yes | Yes | No | No | Yes |
| UI languages | 38 | 38 | Fewer | Fewer | Fewer |
| Footprint | single 9 MB Rust binary | single ~2 MB | Electron app | small | ~20 MB |

*Rufus itself remains the gold standard — on Windows. rufus-linux brings that
standard to Linux.*

---

## Localization

The language combo in the bottom-left lists every locale Rufus ships — 38
languages. Display names and most strings come straight from upstream `.po`
files; untranslated strings fall back to English exactly like Rufus's empty
`msgstr`. Arabic and Hebrew render with real OpenType shaping and bidi
reordering (rustybuzz + unicode-bidi, both pure Rust); Noto fonts installed on
the system cover CJK/Thai/Indic scripts. The selection applies live and persists
in `settings.json`.

---

## Roadmap

Shipped in v0.1: ISO / raw / non-bootable / FreeDOS / Windows To Go modes,
MBR + GPT, UEFI:NTFS, bad-blocks check, ISO downloads, 38-language UI, settings,
persistence, Rufus MBR + disk-ID masquerade, old-BIOS fixes, Windows User
Experience dialog, image hashes, inline log.

Planned:

- [ ] Partitioned modes on 4Kn (4096-byte-sector) devices — currently refused
  with a clear error (raw/DD works)
- [ ] GPT + NTFS/exFAT combination testing on real hardware
- [ ] VHD / WinPE modes
- [ ] Wayland-native backend (X11/XWayland only today)
- [ ] End-to-end Windows To Go boot verification in QEMU (OVMF + SeaBIOS)

---

## Development

```sh
cargo build --locked --release   # build
cargo test                       # 128 unit tests (no root needed)
cargo clippy -- -D warnings      # zero-warning policy
```

CI builds, lints and tests on every push to `main`. Tag pushes and manual runs
of the *Release* workflow build `.tar.gz`, `.deb` and `.rpm` packages for all
four architectures and attach them to the GitHub release.

---

## Acknowledgements

Heartfelt thanks to [Pete Batard](https://github.com/pbatard) for a decade of
Rufus — this project follows its architecture and reuses its attitude.

| Asset | Source | License |
|---|---|---|
| `mbr.bin`, `uefi-ntfs.img`, ms-sys boot records | [Rufus](https://github.com/pbatard/rufus) | GPL-3.0 |
| `ldlinux_v4.*`, `ldlinux_v6.*`, `mboot.c32` | [Syslinux](https://www.syslinux.org) | GPL-3.0-compatible |
| `freedos/` — KERNEL.SYS, COMMAND.COM | [FreeDOS](https://www.freedos.org) | GPL-3.0 |
| `boot.img`, `core.img` | [GRUB2](https://www.gnu.org/software/grub/) | GPL-3.0 |

Pure-Rust GUI stack: [winit](https://github.com/rust-windowing/winit),
[softbuffer](https://github.com/rust-windowing/softbuffer),
[ab_glyph](https://github.com/alexheretic/ab-glyph),
[rustybuzz](https://github.com/harfbuzz/rustybuzz).

---

## License

GPL-3.0-only — the same license as Rufus. See [LICENSE](LICENSE).

<div align="center">
<sub>If rufus-linux saved you a Windows reinstall, a star on the repo is always appreciated.</sub>
</div>
