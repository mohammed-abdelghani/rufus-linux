# Third-party assets and their provenance

This file documents every file bundled under [`assets/`](assets/). Sources and
licenses are stated only where they can be proven from this repository
(source-code comments, the README Acknowledgements table, or file contents).
Anything not provable is marked **provenance not documented** and listed as a
TODO for the maintainer — no license has been guessed.

## Boot code and loaders

| File | What it is | Source | License |
|---|---|---|---|
| `mbr.bin` | Rufus bootable MBR (assembled from Rufus `res/mbr/mbr.S`) | Rufus project | GPL-3.0 |
| `mbr_win7.bin` | Windows 7 MBR boot record — ms-sys `mbr_win7_0x0`, first 440 bytes | ms-sys (bundled via Rufus; per `src/engine.rs` comment) | GPL-3.0 (ms-sys boot records per README Acknowledgements) |
| `msboot/br_ntfs_0x0.bin` | NTFS partition boot record fragment (offset 0x0) | ms-sys NTFS PBR, "GPL, from the Rufus project" (per `src/engine.rs` comment) | GPL |
| `msboot/br_ntfs_0x54.bin` | NTFS partition boot record fragment (offset 0x54) | ms-sys NTFS PBR, "GPL, from the Rufus project" (per `src/engine.rs` comment) | GPL |
| `freedos/br_fat32_0x0.bin` | FreeDOS FAT32 boot record fragment (offset 0x0) | ms-sys DOS/FreeDOS boot record (per `src/engine.rs` comment) | GPL |
| `freedos/br_fat32_0x52.bin` | FreeDOS FAT32 boot record fragment (offset 0x52) | ms-sys DOS/FreeDOS boot record (per `src/engine.rs` comment) | GPL |
| `freedos/br_fat32fd_0x3f0.bin` | FreeDOS FAT32 boot record fragment (offset 0x3F0) | ms-sys DOS/FreeDOS boot record (per `src/engine.rs` comment) | GPL |
| `freedos/KERNEL.SYS` | FreeDOS kernel | FreeDOS project | GPL-3.0 |
| `freedos/COMMAND.COM` | FreeDOS command interpreter | FreeDOS project | GPL-3.0 |
| `ldlinux_v4.bss` | SYSLINUX 4.07 VBR template | Syslinux project | GPL-3.0-compatible |
| `ldlinux_v4.sys` | SYSLINUX 4.07 loader core | Syslinux project | GPL-3.0-compatible |
| `ldlinux_v6.bss` | SYSLINUX 6.04 VBR template | Syslinux project | GPL-3.0-compatible |
| `ldlinux_v6.sys` | SYSLINUX 6.04 loader core | Syslinux project | GPL-3.0-compatible |
| `mboot.c32` | SYSLINUX COM32 module | Syslinux project | GPL-3.0-compatible |
| `boot.img` | GRUB2 boot image | GRUB2 project | GPL-3.0 |
| `core.img` | GRUB2 core image | GRUB2 project | GPL-3.0 |
| `uefi-ntfs.img` | UEFI:NTFS FAT12 boot image (1 MiB), written to a trailing partition for NTFS/exFAT targets | Rufus project (per README Acknowledgements) | GPL-3.0 |

## Application assets

| File | What it is | Source | License |
|---|---|---|---|
| `autorun.ico` | Windows icon written next to `autorun.inf` by "Create extended label and icon files" — the Rufus application icon (`ExtractAppIcon` parity, per `src/engine.rs` comment) | Rufus project | GPL-3.0 |
| `screenshot-main.png` | Screenshot of the rufus-linux main window, used in the README | This project | Project asset — not separately licensed |

## Notes and open items

- `mboot.c32` is listed in the README Acknowledgements under Syslinux but is
  **not referenced by the current source code** — it may be bundled for future
  use or kept for parity. TODO(owner): confirm whether it is still needed.
- `uefi-ntfs.img` is documented in this repo as coming from the Rufus project;
  its ultimate upstream (the UEFI:NTFS project) is not documented here.
  TODO(owner): record the exact upstream source and license.
- `autorun.ico` is documented as the Rufus application icon extracted for
  parity; the exact extraction method/version is not recorded.
  TODO(owner): confirm provenance details.
- `packaging/` contains only project-authored files (desktop entry, AppStream
  metainfo, icon) under the project license — no third-party assets.
