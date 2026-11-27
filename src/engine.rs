//! Write engine: orchestrates wipe -> partition -> format -> mount -> copy -> boot.

use anyhow::{bail, Context, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::bcd::{self, BcdDevice};
use crate::bootpatch;
use crate::device::Device;
use crate::disk::Disk;
use crate::fat32;
use crate::gpt::GptPartSpec;
use crate::iso9660::{self, IsoImage};
use crate::mbr::{self, FsType, Partition};

/// Rufus bootable MBR (assembled from res/mbr/mbr.S).
const MBR_BOOTCODE: &[u8] = include_bytes!("../assets/mbr.bin");
/// Windows 7 MBR (ms-sys mbr_win7_0x0, first 440 bytes): written when the
/// "Use Rufus MBR with BIOS ID" box is unchecked (Rufus format.c parity).
const MBR_WIN7: &[u8] = include_bytes!("../assets/mbr_win7.bin");
/// Rufus application icon written next to autorun.inf by "Create extended
/// label and icon files" (Rufus icon.c ExtractAppIcon parity).
const AUTORUN_ICON: &[u8] = include_bytes!("../assets/autorun.ico");
/// ms-sys NTFS partition boot record (GPL, from the Rufus project): boot
/// code at 0x0..0x0B and 0x54.. onward, over the BPB written by mkfs.ntfs.
const NTFS_BR_0X0: &[u8] = include_bytes!("../assets/msboot/br_ntfs_0x0.bin");
const NTFS_BR_0X54: &[u8] = include_bytes!("../assets/msboot/br_ntfs_0x54.bin");
/// syslinux 6.04 VBR template (first sector of ldlinux) + loader core.
pub(crate) const LDLINUX_V6_BSS: &[u8] = include_bytes!("../assets/ldlinux_v6.bss");
pub(crate) const LDLINUX_V6_SYS: &[u8] = include_bytes!("../assets/ldlinux_v6.sys");
// the 4.07 pair is kept for a future "old ISO" fallback (and exercised by the
// bootpatch tests); the installer always uses v6 today
#[allow(dead_code)]
pub(crate) const LDLINUX_V4_BSS: &[u8] = include_bytes!("../assets/ldlinux_v4.bss");
#[allow(dead_code)]
pub(crate) const LDLINUX_V4_SYS: &[u8] = include_bytes!("../assets/ldlinux_v4.sys");
/// UEFI:NTFS FAT12 boot image (1 MiB) — written raw to a small trailing partition.
const UEFI_NTFS_IMG: &[u8] = include_bytes!("../assets/uefi-ntfs.img");
pub(crate) const GRUB2_BOOT: &[u8] = include_bytes!("../assets/boot.img");
pub(crate) const GRUB2_CORE: &[u8] = include_bytes!("../assets/core.img");

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageKind {
    Iso,
    Raw, // .img/.bin/.gz/dd-able image -> raw write
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PartitionScheme {
    Mbr,
    Gpt,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FsKind {
    Fat32,
    Ntfs,
    Exfat,
}
impl FsKind {
    pub fn name(&self) -> &'static str {
        match self {
            FsKind::Fat32 => "FAT32",
            FsKind::Ntfs => "NTFS",
            FsKind::Exfat => "exFAT",
        }
    }
}

/// Rufus "Check device for bad blocks": a destructive write test ported from
/// the original badblocks.c (e2fsprogs-derived). Every pass writes a test
/// pattern across the whole device and reads it back, so all data is
/// overwritten — exactly like the original Rufus check.
///
/// The variants mirror the entries of Rufus's "Number of passes" combobox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadBlocksTest {
    #[default]
    Disabled,
    /// "1 pass" — pattern 0x55
    OnePass,
    /// "2 passes" — patterns 0x55, 0xaa
    TwoPasses,
    /// "4 passes for SLC NAND"
    Slc,
    /// "4 passes for MLC NAND"
    Mlc,
    /// "4 passes for TLC NAND"
    Tlc,
}

// Constants from the original badblocks.h / rufus.h.
/// BADBLOCK_BLOCK_SIZE: the test operates on 512 KiB blocks.
pub const BB_BLOCK_SIZE: u64 = 512 * 1024;
/// BB_BLOCKS_AT_ONCE: blocks written/read per I/O (32 MiB chunks).
pub const BB_BLOCKS_AT_ONCE: u64 = 64;
/// BB_BAD_BLOCKS_THRESHOLD: abort the test after this many bad blocks.
pub const BB_THRESHOLD: usize = 256;

/// Per-pass write patterns, straight from Rufus's rufus.h
/// (BADBLOCK_PATTERN_ONE_PASS / TWO_PASSES / SLC / MLC / TLC).
const BB_PATTERNS: [&[u32]; 5] = [
    &[0x55],
    &[0xaa],
    &[0x00, 0xff, 0x55, 0xaa],
    &[0x00, 0xff, 0x33, 0xcc],
    &[0x00, 0xff, 0x1c71c7, 0xe38e38],
];

/// Entry labels of Rufus's "Number of passes" combobox.
pub const BB_PASSES_NAMES: [&str; 5] = [
    "1 pass",
    "2 passes",
    "4 passes for SLC NAND",
    "4 passes for MLC NAND",
    "4 passes for TLC NAND",
];

impl BadBlocksTest {
    /// From the index of Rufus's passes combobox (format.c:
    /// `BadBlocks(..., (sel >= 2) ? 4 : sel + 1, sel, ...)`).
    pub fn from_sel(sel: usize) -> Self {
        match sel {
            0 => BadBlocksTest::OnePass,
            1 => BadBlocksTest::TwoPasses,
            2 => BadBlocksTest::Slc,
            3 => BadBlocksTest::Mlc,
            _ => BadBlocksTest::Tlc,
        }
    }

    /// Number of write+verify passes (sel >= 2 selects the 4-pass NAND sets).
    pub fn passes(&self) -> usize {
        match self {
            BadBlocksTest::Disabled => 0,
            BadBlocksTest::OnePass => 1,
            BadBlocksTest::TwoPasses => 2,
            BadBlocksTest::Slc | BadBlocksTest::Mlc | BadBlocksTest::Tlc => 4,
        }
    }

    fn patterns(&self) -> &'static [&'static [u32]] {
        match self {
            BadBlocksTest::Disabled => &[],
            BadBlocksTest::OnePass => &BB_PATTERNS[0..1],
            BadBlocksTest::TwoPasses => &BB_PATTERNS[0..2],
            BadBlocksTest::Slc => &BB_PATTERNS[2..3],
            BadBlocksTest::Mlc => &BB_PATTERNS[3..4],
            BadBlocksTest::Tlc => &BB_PATTERNS[4..5],
        }
    }
}

/// Clamp a volume label to what a FAT32/FAT16 boot sector can carry (Rufus
/// parity): at most 11 bytes, uppercase, and only the characters FAT
/// directory entries allow — everything else (UTF-8 bytes included) becomes
/// '_', like Rufus's invalid-label replacement. NTFS/exFAT labels keep their
/// Unicode; only the FAT paths run through this.
pub fn sanitize_fat_label(label: &str) -> String {
    const FAT_ALLOWED: &str = " !#$%&'()-.@^_`{}~";
    let mut out = String::with_capacity(11);
    for c in label.chars() {
        if out.len() >= 11 {
            break;
        }
        let c = c.to_ascii_uppercase();
        let ok = c.is_ascii() && (c.is_ascii_alphanumeric() || FAT_ALLOWED.contains(c));
        if ok {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out.trim_end().to_string()
}

#[derive(Debug, Clone)]
pub struct Options {
    pub fs: FsKind,
    pub scheme: PartitionScheme,
    pub fs_label: String,
    pub quick: bool,    // skip bad-block scan
    pub verify: bool,   // verify after write (sha256 compare pass)
    pub efi_only: bool, // don't bother with BIOS boot
    /// casper-rw ext4 persistence partition size in MiB (0 = disabled)
    pub persistence_mib: u64,
    /// which pipeline to run (Rufus boot-selection modes)
    pub write_mode: WriteMode,
    /// "Add fixes for old BIOSes (extra partition, align, etc.)": creates the
    /// Rufus "BIOS Compatibility" extra partition (one track at the end of
    /// the disk) and, on MBR, starts the first partition on a track boundary
    /// instead of 1 MiB
    pub old_bios_fixes: bool,
    /// Rufus "Windows User Experience" customizations for Windows ISOs
    pub wue: Option<WueOptions>,
    /// allow writing to a device that hosts the root filesystem (DANGEROUS)
    pub allow_system_disk: bool,
    /// "Check device for bad blocks" — destructive pattern write test (Rufus)
    pub bb_test: BadBlocksTest,
    /// Windows To Go: 1-based index of the install.wim edition to apply
    /// (0 = first image, Rufus defaults to 1 when there is no dialog)
    pub wim_index: u32,
    /// "Use Rufus MBR" hidden setting (Rufus !DisableRufusMBR, Alt-A): false
    /// writes a plain MBR partition table without the Rufus boot code
    pub rufus_mbr: bool,
    /// Rufus IDC_DISK_ID masquerade: the bootable-flag byte of the first
    /// partition (0x80 = default; 0x81-0x87 masquerade the stick as disk
    /// 2-8 for old BIOSes). Only applies with the Rufus MBR on MBR+BIOS.
    pub disk_id: u8,
    /// Rufus "Create extended label and icon files" (IDC_EXTENDED_LABEL):
    /// write autorun.inf (UTF-16LE label) + autorun.ico on the volume so
    /// file managers show labels longer than 11/32 chars and an icon
    pub autorun: bool,
    /// "Preserve timestamps" hidden setting (Rufus PreserveTimestamps,
    /// Alt-T): copy the ISO directory-record dates to the written files
    pub preserve_timestamps: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fs: FsKind::Fat32,
            scheme: PartitionScheme::Mbr,
            fs_label: "RUFUS".into(),
            quick: true,
            verify: false,
            efi_only: false,
            persistence_mib: 0,
            write_mode: WriteMode::DiskImage,
            old_bios_fixes: false,
            wue: None,
            allow_system_disk: false,
            bb_test: BadBlocksTest::Disabled,
            wim_index: 0,
            rufus_mbr: true,
            disk_id: 0x80,
            autorun: true,
            preserve_timestamps: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub message: String,
}

/// Partition the boot installers write to.
struct BootTarget<'a> {
    part_dev: &'a str,
    part_start_lba: u64,
    sector_size: u64,
}

/// Which write pipeline to run (Rufus boot-selection modes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WriteMode {
    /// write the ISO/image contents (default)
    DiskImage,
    /// wipe + partition + format only ("Non bootable")
    NonBootable,
    /// FAT32 + FreeDOS boot files ("FreeDOS")
    FreeDos,
    /// apply install.wim onto the drive ("Windows To Go" image option)
    WindowsToGo,
}

/// Rufus "Windows User Experience" customization flags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WueOptions {
    /// Remove requirement for 4GB+ RAM, Secure Boot and TPM 2.0
    pub bypass_tpm: bool,
    /// Remove requirement for an online Microsoft account
    pub no_online_account: bool,
    /// Disable data collection (skip privacy questions)
    pub disable_data_collection: bool,
    /// Set a local account using the same name as this user's
    pub local_account: bool,
    /// Set regional options using the same values as this user's
    pub regional_same_as_user: bool,
    /// Rufus MSG_332 "Prevent Windows To Go from accessing internal disks"
    /// (UNATTEND_OFFLINE_INTERNAL_DRIVES): SanPolicy = 4 ("Offline Internal")
    /// in the offlineServicing pass; Rufus applies it to the image with DISM,
    /// we ship it in the same unattend.xml Windows picks up at first boot
    pub offline_internal_drives: bool,
}

/// Build the Rufus-style unattend.xml for the selected customizations.
/// Rufus drops this at sources/$oem$/$$/Panther/unattend.xml so Windows
/// setup consumes it during the OOBE phase.
pub fn build_unattend_xml(o: &WueOptions, username: &str, locale: &str, tz: &str) -> String {
    let mut oobe = String::new();
    if o.no_online_account {
        oobe.push_str("        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>\n");
        oobe.push_str("        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>\n");
    }
    if o.disable_data_collection {
        oobe.push_str("        <ProtectYourPC>3</ProtectYourPC>\n");
        oobe.push_str("        <HideEULAPage>true</HideEULAPage>\n");
    }
    let account = if o.local_account {
        format!(
            "      <UserAccounts>\n        <LocalAccounts>\n          <LocalAccount wcm:action=\"add\">\n            <Name>{username}</Name>\n            <DisplayName>{username}</DisplayName>\n            <Group>Administrators</Group>\n            <Password>\n              <Value></Value>\n              <PlainText>true</PlainText>\n            </Password>\n          </LocalAccount>\n        </LocalAccounts>\n      </UserAccounts>\n"
        )
    } else {
        String::new()
    };
    let locale_block = if o.regional_same_as_user {
        format!(
            "    <component name=\"Microsoft-Windows-International-Core\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n      <InputLocale>{locale}</InputLocale>\n      <SystemLocale>{locale}</SystemLocale>\n      <UILanguage>{locale}</UILanguage>\n      <UserLocale>{locale}</UserLocale>\n    </component>\n"
        )
    } else {
        String::new()
    };
    // Rufus wue.c: SanPolicy = 4 ("Offline Internal") keeps Windows To Go
    // from mounting (and silently upgrading the file system of) the machine's
    // internal drives
    let san_block = if o.offline_internal_drives {
        "  <settings pass=\"offlineServicing\">\n    <component name=\"Microsoft-Windows-PartitionManager\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n      <SanPolicy>4</SanPolicy>\n    </component>\n  </settings>\n"
    } else {
        ""
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<unattend xmlns=\"urn:schemas-microsoft-com:unattend\">\n{san_block}  <settings pass=\"windowsPE\">\n{locale_block}  </settings>\n  <settings pass=\"oobeSystem\">\n    <component name=\"Microsoft-Windows-International-Core\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n      <InputLocale>{locale}</InputLocale>\n      <SystemLocale>{locale}</SystemLocale>\n      <UILanguage>{locale}</UILanguage>\n      <UserLocale>{locale}</UserLocale>\n    </component>\n    <component name=\"Microsoft-Windows-Shell-Setup\" processorArchitecture=\"amd64\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\">\n{account}      <OOBE>\n{oobe}      </OOBE>\n      <TimeZone>{tz}</TimeZone>\n    </component>\n  </settings>\n</unattend>\n"
    )
}

// ms-sys DOS/FreeDOS FAT32 boot-record fragments and FreeDOS files (GPL,
// from the Rufus project, same layout Rufus writes: fragments at 0x0, 0x52
// and 0x3F0 over a 1536-byte, three-sector boot record).
const FREEDOS_BR_0X0: &[u8] = include_bytes!("../assets/freedos/br_fat32_0x0.bin");
const FREEDOS_BR_0X52: &[u8] = include_bytes!("../assets/freedos/br_fat32_0x52.bin");
const FREEDOS_BR_FD_0X3F0: &[u8] = include_bytes!("../assets/freedos/br_fat32fd_0x3f0.bin");
const FREEDOS_KERNEL: &[u8] = include_bytes!("../assets/freedos/KERNEL.SYS");
const FREEDOS_COMMAND: &[u8] = include_bytes!("../assets/freedos/COMMAND.COM");

/// Assemble the FreeDOS FAT32 boot record (1536 bytes: sectors 0-2) around a
/// formatter-produced BPB. The loader code reads the BPB at runtime.
fn assemble_freedos_br(bpb: &[u8]) -> Vec<u8> {
    const _: () = {
        assert!(FREEDOS_BR_0X0.len() == 0x0B, "br_fat32_0x0 fragment size");
        assert!(
            FREEDOS_BR_0X52.len() == 0x3E8 - 0x52,
            "br_fat32_0x52 fragment size"
        );
        assert!(
            FREEDOS_BR_FD_0X3F0.len() == 0x600 - 0x3F0,
            "br_fat32fd_0x3f0 fragment size"
        );
    };
    let mut br = vec![0u8; 0x600];
    br[0x0..0x0B].copy_from_slice(FREEDOS_BR_0X0);
    br[0x0B..0x52].copy_from_slice(&bpb[..0x52 - 0x0B]);
    br[0x52..0x3E8].copy_from_slice(FREEDOS_BR_0X52);
    br[0x3F0..0x600].copy_from_slice(FREEDOS_BR_FD_0X3F0);
    br[0x40] = 0x80; // BIOS drive number (first hard disk)
    br
}

/// Assemble the ms-sys NTFS partition boot record over the freshly formatted
/// volume: the fragments carry the bootmgr loader code while the BPB region
/// (0x0B..0x54, including the $MFT pointers) written by mkfs.ntfs is kept
/// verbatim — the same layout Rufus uses via ms-sys's write_ntfs_br().
fn assemble_ntfs_pbr(bpb: &[u8]) -> Vec<u8> {
    const _: () = {
        assert!(NTFS_BR_0X0.len() == 0x0B, "br_ntfs_0x0 fragment size");
    };
    let mut br = vec![0u8; 0x54 + NTFS_BR_0X54.len()];
    br[0x0..0x0B].copy_from_slice(NTFS_BR_0X0);
    br[0x0B..0x54].copy_from_slice(&bpb[..0x54 - 0x0B]);
    br[0x54..].copy_from_slice(NTFS_BR_0X54);
    br
}

/// Locate an external tool, trying /usr/sbin (elevated PATHs often miss it).
fn find_tool(name: &str) -> Option<String> {
    if Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some(name.to_string());
    }
    for dir in ["/usr/sbin", "/sbin", "/usr/local/sbin"] {
        let p = format!("{dir}/{name}");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    None
}

/// Case-insensitive lookup of `sources/install.wim|install.esd|install.swm`
/// in the ISO (Windows images mix casing; our extractor preserves ISO names).
pub fn find_windows_wim(iso: &IsoImage) -> Option<&iso9660::IsoEntry> {
    const NAMES: [&str; 3] = [
        "sources/install.wim",
        "sources/install.esd",
        "sources/install.swm",
    ];
    iso.entries
        .iter()
        .find(|e| !e.is_dir && NAMES.iter().any(|n| e.path.eq_ignore_ascii_case(n)))
}

/// One edition of an install.wim: 1-based index + display name.
#[derive(Debug, Clone, PartialEq)]
pub struct WimImage {
    pub index: u32,
    pub name: String,
}

/// List the editions inside the Windows image of an ISO without extracting
/// the whole WIM: the WIM header (first bytes of the install.wim extent)
/// points at its XML metadata resource, which holds the image names.
pub fn wim_image_list(iso_path: &Path) -> Result<Vec<WimImage>> {
    let mut f = std::fs::File::open(iso_path)?;
    let iso = iso9660::parse_iso(&mut f)?;
    let entry = find_windows_wim(&iso)
        .ok_or_else(|| anyhow::anyhow!("no sources/install.wim in the ISO"))?;
    let base = entry.extent_lba * iso9660::SECTOR;
    // struct wim_header_disk (wimlib header.h): xml_data_reshdr at +0x48
    let mut hdr = [0u8; 0x48 + 24];
    f.seek(SeekFrom::Start(base))?;
    f.read_exact(&mut hdr)?;
    if &hdr[0..5] != b"MSWIM" {
        bail!("not a WIM file: {:?}", entry.path);
    }
    // struct wim_reshdr_disk: size_in_wim[7] | flags | offset_in_wim | uncompressed_size
    let size = u64::from_le_bytes([
        hdr[0x48], hdr[0x49], hdr[0x4A], hdr[0x4B], hdr[0x4C], hdr[0x4D], hdr[0x4E], 0,
    ]);
    let flags = hdr[0x4F];
    let off = u64::from_le_bytes(hdr[0x50..0x58].try_into().unwrap());
    let _uncompressed = u64::from_le_bytes(hdr[0x58..0x60].try_into().unwrap());
    if flags != 0 {
        bail!("compressed WIM XML metadata is not supported");
    }
    if size == 0 || size > 16 * 1024 * 1024 {
        bail!("implausible WIM XML size {size}");
    }
    let mut xml = vec![0u8; size as usize];
    f.seek(SeekFrom::Start(base + off))?;
    f.read_exact(&mut xml)?;
    Ok(parse_wim_xml(&xml))
}

/// Extract the `<IMAGE INDEX="n">` names from WIM XML metadata
/// (DISPLAYNAME, falling back to NAME then DESCRIPTION like Rufus).
fn parse_wim_xml(xml: &[u8]) -> Vec<WimImage> {
    // UTF-16LE BOM → convert to UTF-8; otherwise treat as UTF-8
    let text = if xml.starts_with(&[0xFF, 0xFE]) {
        String::from_utf16_lossy(
            &xml[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(xml).to_string()
    };
    let mut out = Vec::new();
    for block in text.split("<IMAGE ") {
        let Some(idx_pos) = block.find("INDEX=\"") else {
            continue;
        };
        let rest = &block[idx_pos + 7..];
        let Some(idx_end) = rest.find('"') else {
            continue;
        };
        let Ok(index) = rest[..idx_end].parse::<u32>() else {
            continue;
        };
        let name = ["DISPLAYNAME", "NAME", "DESCRIPTION"]
            .iter()
            .find_map(|tag| {
                let open = format!("<{tag}>");
                let close = format!("</{tag}>");
                let s = block.find(&open)? + open.len();
                let e = block[s..].find(&close)? + s;
                Some(block[s..e].to_string())
            })
            .unwrap_or_default();
        out.push(WimImage { index, name });
    }
    out.sort_by_key(|i| i.index);
    out
}

/// Candidate relative paths of the WIM (and split parts) inside the ISO,
/// tried in order on the loop mount.
fn wim_candidates(swm_parts: &[iso9660::IsoEntry], primary: &iso9660::IsoEntry) -> Vec<String> {
    if swm_parts.is_empty() {
        vec![primary.path.clone()]
    } else {
        swm_parts.iter().map(|e| e.path.clone()).collect()
    }
}

/// Case-insensitive lookup of an ISO entry; returns a clone of the entry.
fn find_case_insensitive_path(iso: &IsoImage, rel: &str) -> Option<iso9660::IsoEntry> {
    iso.entries
        .iter()
        .find(|e| !e.is_dir && e.path.to_lowercase() == rel.to_lowercase())
        .cloned()
}

/// Case-insensitive file lookup under `root` (component-wise, like NTFS).
fn find_case_insensitive(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    for comp in rel.split('/') {
        let mut found = None;
        for e in std::fs::read_dir(&cur).ok()?.flatten() {
            if e.file_name().to_string_lossy().eq_ignore_ascii_case(comp) {
                found = Some(e.path());
                break;
            }
        }
        cur = found?;
    }
    if cur.is_file() {
        Some(cur)
    } else {
        None
    }
}

/// Random GUID string "{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}".
fn random_guid_string() -> String {
    let b: [u8; 16] = random_bytes(16).try_into().expect("16 bytes");
    let g = crate::gpt::Guid(b);
    // Display is uppercase; build_bcd normalizes to the lowercase form
    format!("{{{g}}}")
}

/// Fixed boot-entry GUID for the QEMU boot test (deterministic BCDs).
pub fn test_entry_guid() -> &'static str {
    "3afd365b-4349-11e3-b0da-a722f0df0a34"
}

/// Main partition prepared by [`Engine::partition_and_format`].
struct PreppedPart {
    part_dev: String,
    part_start_lba: u64,
    sector_size: u64,
    #[allow(dead_code)] // read by partition_node at format time via struct field
    /// partition index of the main partition (1 normally; 3 on WTG GPT
    /// where the ESP and the MSR occupy 1 and 2)
    main_part_index: u32,
    /// Windows To Go GPT: start LBA of the EFI System Partition (0 = none)
    esp_lba: u64,
    /// MBR disk signature (0 on GPT) — feeds the BCD device element
    disk_sig: u32,
    /// GPT disk GUID (EMPTY on MBR)
    disk_guid: crate::gpt::Guid,
    /// GPT GUID of the main partition (EMPTY on MBR)
    part_guid: crate::gpt::Guid,
}

pub struct Engine {
    pub options: Options,
    /// cooperative cancellation flag checked between steps and in copy/write loops
    cancel: Option<Arc<AtomicBool>>,
}

/// Block I/O abstraction so the bad blocks test can also run against
/// in-memory media in tests (the real target is `Disk`).
pub trait BlockIo {
    fn size(&self) -> u64;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
}

impl BlockIo for Disk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        Disk::read_at(self, offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        Disk::write_at(self, offset, buf)
    }
}

/// Fill `buffer` with the byte repetition of `pattern`, mirroring the
/// original pattern_fill() from e2fsprogs: the pattern bytes repeat in
/// big-endian order; pattern 0 fills zeros; u32::MAX fills random bytes.
fn fill_pattern(buffer: &mut [u8], pattern: u32) {
    if pattern == u32::MAX {
        // "Testing with random pattern" (e2fsprogs -t random)
        for chunk in buffer.chunks_mut(64 * 1024) {
            chunk.copy_from_slice(&random_bytes(chunk.len()));
        }
        return;
    }
    let mut bpattern = [0u8; 4];
    let mut pat = pattern;
    let mut i = 0usize;
    while i < bpattern.len() {
        if pat == 0 {
            break;
        }
        bpattern[i] = (pat & 0xFF) as u8;
        pat >>= 8;
        i += 1;
    }
    let nb = if i > 0 { i - 1 } else { 0 };
    let mut idx = nb;
    for b in buffer.iter_mut() {
        *b = bpattern[idx];
        idx = if idx == 0 { nb } else { idx - 1 };
    }
}

/// bb_output(): add a block to the sorted, unique bad blocks list;
/// returns true when the block is new.
fn record_bad_block(list: &mut Vec<u64>, blk: u64) -> bool {
    match list.binary_search(&blk) {
        Ok(_) => false,
        Err(pos) => {
            list.insert(pos, blk);
            true
        }
    }
}

/// Entropy from /dev/urandom with a time-based fallback (disk signatures and
/// GUIDs must differ between sticks).
fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        return buf;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for (i, x) in buf.iter_mut().enumerate() {
        *x = (nanos >> ((i % 8) * 8)) as u8 ^ (i as u8).wrapping_mul(37);
    }
    buf
}

/// Simpler: a writer tracking position manually.
struct RegionWriter<'a> {
    disk: &'a mut Disk,
    base: u64,
    pos: u64,
}
impl<'a> Write for RegionWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.disk
            .write_at(self.base + self.pos, buf)
            .map_err(std::io::Error::other)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> Seek for RegionWriter<'a> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => self.disk.size as i128 + n as i128,
        };
        if new < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "negative seek",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

pub fn detect_image_kind(path: &Path) -> ImageKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "iso" | "img" | "isohybrid" => {
            // .img ambiguous: check ISO magic at sector 16
            if ext == "iso" || ext == "isohybrid" {
                ImageKind::Iso
            } else {
                // peek
                if let Ok(mut f) = std::fs::File::open(path) {
                    use std::io::{Read, Seek};
                    let mut buf = [0u8; 7];
                    if f.seek(SeekFrom::Start(16 * 2048)).is_ok()
                        && f.read_exact(&mut buf).is_ok()
                        && &buf[1..6] == b"CD001"
                    {
                        ImageKind::Iso
                    } else {
                        ImageKind::Raw
                    }
                } else {
                    ImageKind::Raw
                }
            }
        }
        _ => ImageKind::Raw,
    }
}

/// Trailing-partition layout, computed from the disk end backwards:
/// `[main][persistence][UEFI:NTFS]["BIOS Compatibility"]` <- end of disk.
struct Layout {
    /// last LBA (inclusive) of the main partition
    part_end_lba: u64,
    /// start LBA of the UEFI:NTFS partition (0 when unused)
    uefi_ntfs_start: u64,
    /// start LBA of the persistence partition (0 when unused)
    persist_start: u64,
    /// persistence partition size in sectors (0 when unused)
    persist_len: u64,
    /// start LBA of the old-BIOS "BIOS Compatibility" partition (0 when unused)
    compat_start: u64,
}

/// Layout inputs (all sector counts in 512-byte units).
struct LayoutReq {
    /// first LBA of the main partition
    part_start_lba: u64,
    uefi_ntfs_sectors: u64,
    persist_sectors: u64,
    /// size of the "BIOS Compatibility" partition (one track; 0 = off)
    compat_sectors: u64,
    /// track size in 512-byte sectors (extra partitions are track-aligned,
    /// like Rufus's FLOOR_ALIGN(offset, bytes_per_track)); must be > 0
    track_sectors: u64,
    gpt: bool,
}

fn compute_layout(disk_sectors: u64, req: LayoutReq) -> crate::error::Result<Layout> {
    const TAIL_GAP: u64 = 2048; // 1 MiB kept free at the disk end (GPT backup etc.)
    let align = |lba: u64| (lba / 2048) * 2048;
    let track_align = |lba: u64| lba / req.track_sectors * req.track_sectors;
    // The "BIOS Compatibility" partition sits at the very end of the disk —
    // GPT keeps its 33-sector backup header/entries area above it. Without
    // it, the other trailing partitions keep the 1 MiB tail gap.
    let disk_end = disk_sectors - if req.gpt { 33 } else { 0 };
    let compat_start = if req.compat_sectors > 0 {
        track_align(disk_end.saturating_sub(req.compat_sectors))
    } else {
        0
    };
    let upper_end = if compat_start > 0 {
        compat_start
    } else {
        disk_sectors.saturating_sub(TAIL_GAP)
    };
    let uefi_ntfs_start = if req.uefi_ntfs_sectors > 0 {
        align(upper_end.saturating_sub(req.uefi_ntfs_sectors + req.persist_sectors))
    } else {
        0
    };
    let persist_start = if req.persist_sectors > 0 {
        let raw = if uefi_ntfs_start > 0 {
            uefi_ntfs_start
        } else {
            upper_end
        };
        align(raw.saturating_sub(req.persist_sectors))
    } else {
        0
    };
    let (persist_len, part_end_lba) = if req.persist_sectors > 0 {
        let end = if uefi_ntfs_start > 0 {
            uefi_ntfs_start
        } else {
            upper_end
        };
        (end - persist_start, persist_start.saturating_sub(1))
    } else if uefi_ntfs_start > 0 {
        (0, uefi_ntfs_start.saturating_sub(1))
    } else if compat_start > 0 {
        (0, compat_start - 1)
    } else {
        (0, disk_sectors.saturating_sub(TAIL_GAP))
    };
    if part_end_lba <= req.part_start_lba {
        return Err(crate::error::Error::DeviceTooSmall);
    }
    Ok(Layout {
        part_end_lba,
        uefi_ntfs_start,
        persist_start,
        persist_len,
        compat_start,
    })
}

impl Engine {
    pub fn new(options: Options) -> Self {
        Engine {
            options,
            cancel: None,
        }
    }

    /// Attach a cooperative cancellation flag (GUI stop button).
    pub fn with_cancel(mut self, token: Arc<AtomicBool>) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Abort with an error when the cancel flag is set.
    fn check_cancel(&self) -> Result<()> {
        if let Some(c) = &self.cancel {
            if c.load(Ordering::Relaxed) {
                bail!("write cancelled by user");
            }
        }
        Ok(())
    }

    /// Full pipeline. `progress` called with phase updates.
    pub fn write_iso(
        &self,
        dev: &Device,
        image_path: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        // refuse to wipe the system disk (unless explicitly overridden)
        if !self.options.allow_system_disk {
            if let Some(mnt) = crate::device::system_root_mount(&dev.path) {
                bail!(
                    "{} hosts the root filesystem (mounted at {mnt}) — refusing to overwrite. \
                     Pass --force if you really mean it.",
                    dev.path
                );
            }
        }
        self.check_cancel()?;
        match self.options.write_mode {
            WriteMode::NonBootable => self.format_only(dev, progress),
            WriteMode::FreeDos => self.write_freedos(dev, progress),
            WriteMode::WindowsToGo => self.write_windows_to_go(dev, image_path, progress),
            WriteMode::DiskImage => match detect_image_kind(image_path) {
                ImageKind::Iso => self.write_iso_contents(dev, image_path, progress),
                ImageKind::Raw => self.write_raw(dev, image_path, progress),
            },
        }
    }

    /// Raw byte-for-byte write (DD mode). Gzip-compressed images (.gz) are
    /// decompressed on the fly (flate2).
    fn write_raw(
        &self,
        dev: &Device,
        image_path: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let is_gz = image_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("gz"))
            .unwrap_or(false);
        let src_file = std::fs::File::open(image_path)?;
        let (img_size, mut src): (u64, Box<dyn Read>) = if is_gz {
            // the gzip footer (ISIZE) holds the uncompressed size mod 2^32 —
            // use it for accurate progress when available, else the device
            // size is the bound
            let mut sz = dev.size;
            if let Ok(mut f) = std::fs::File::open(image_path) {
                if f.seek(SeekFrom::End(-4)).is_ok() {
                    let mut b = [0u8; 4];
                    if f.read_exact(&mut b).is_ok() {
                        let isize_ = u32::from_le_bytes(b) as u64;
                        if isize_ > 0 {
                            sz = isize_;
                        }
                    }
                }
            }
            (sz, Box::new(flate2::read::GzDecoder::new(src_file)))
        } else {
            let sz = std::fs::metadata(image_path)?.len();
            if sz > dev.size {
                bail!("image ({sz}) is larger than device ({})", dev.size);
            }
            (sz, Box::new(src_file))
        };
        progress(Progress {
            phase: "open".into(),
            done: 0,
            total: img_size,
            message: format!("Opening {}", dev.path),
        });
        let mut disk = Disk::open(&dev.path)?;
        if self.options.bb_test != BadBlocksTest::Disabled {
            self.badblocks_test(&mut disk, progress)?;
        }
        progress(Progress {
            phase: "write".into(),
            done: 0,
            total: img_size,
            message: "Writing image...".into(),
        });
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut off = 0u64;
        loop {
            self.check_cancel()?;
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if off + n as u64 > dev.size {
                bail!("image is larger than device ({})", dev.size);
            }
            disk.write_at(off, &buf[..n])?;
            off += n as u64;
            progress(Progress {
                phase: "write".into(),
                done: off,
                total: img_size,
                message: String::new(),
            });
        }
        disk.file.sync_all()?;
        progress(Progress {
            phase: "done".into(),
            done: img_size,
            total: img_size,
            message: "Raw write complete".into(),
        });
        Ok(())
    }

    /// ISO contents mode: partition + FAT32 + copy files + boot files.
    fn write_iso_contents(
        &self,
        dev: &Device,
        image_path: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        // 1. open ISO and get info
        progress(Progress {
            phase: "inspect".into(),
            done: 0,
            total: 0,
            message: "Reading ISO...".into(),
        });
        let mut iso_file = std::fs::File::open(image_path)?;
        let iso: IsoImage = iso9660::parse_iso(&mut iso_file)?;
        let total_files = iso.entries.iter().filter(|e| !e.is_dir).count() as u64;
        let has_efi = iso.entries.iter().any(|e| {
            e.path.eq_ignore_ascii_case("efi/boot/bootx64.efi")
                || e.path.to_lowercase().starts_with("efi/boot/")
        });
        let syslinux_dir = iso
            .entries
            .iter()
            .any(|e| e.path.starts_with("isolinux/") || e.path.starts_with("syslinux/"));
        tracing::info!(
            "ISO '{}' files={} efi={} syslinux={}",
            iso.volume_id,
            total_files,
            has_efi,
            syslinux_dir
        );

        // 2. open disk + wipe
        progress(Progress {
            phase: "open".into(),
            done: 0,
            total: 0,
            message: format!("Opening {}", dev.path),
        });
        let mut disk = Disk::open(&dev.path)?;
        let PreppedPart {
            part_dev,
            part_start_lba,
            sector_size,
            ..
        } = self.partition_and_format(dev, &mut disk, progress)?;

        // 5. mount main partition
        let mnt = format!("/tmp/rufus-linux-{}", dev.name);
        std::fs::create_dir_all(&mnt)?;
        // desktop automounters may have attached the freshly formatted
        // partition in the meantime — detach before mounting
        unmount_device_partitions(&dev.path);
        progress(Progress {
            phase: "mount".into(),
            done: 0,
            total: 0,
            message: format!("Mounting {part_dev} at {mnt}"),
        });
        mount_fs(&part_dev, &mnt, self.options.fs)?;

        // 6. copy files
        let result = (|| -> Result<()> {
            let mnt_path = PathBuf::from(&mnt);
            // byte-based progress: a single 4 GiB install.wim would otherwise
            // sit at "1 file" for minutes
            let total_bytes: u64 = iso
                .entries
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| e.size)
                .sum();
            progress(Progress {
                phase: "copy".into(),
                done: 0,
                total: total_bytes,
                message: "Copying files...".into(),
            });
            let mut copied = 0u64;
            let mut bytes_done = 0u64;
            let mut last_report = 0u64;
            for e in &iso.entries {
                self.check_cancel()?;
                if e.is_dir {
                    std::fs::create_dir_all(mnt_path.join(&e.path))?;
                    continue;
                }
                let dest = mnt_path.join(&e.path);
                let written = iso9660::extract_file(&mut iso_file, e, &dest)?;
                self.apply_preserved_mtime(&dest, e);
                if written != e.size {
                    tracing::warn!(
                        "{} truncated in image ({} bytes missing)",
                        e.path,
                        e.size - written
                    );
                }
                bytes_done += written;
                copied += 1;
                if copied.is_multiple_of(25)
                    || copied == total_files
                    || bytes_done - last_report >= 16 * 1024 * 1024
                {
                    last_report = bytes_done;
                    progress(Progress {
                        phase: "copy".into(),
                        done: bytes_done,
                        total: total_bytes,
                        message: String::new(),
                    });
                }
            }
            // verify while the filesystem is still mounted and before any
            // post-copy mutations (the wim split removes install.wim and the
            // boot install adds ldlinux.sys)
            if self.options.verify {
                self.verify_copy(&mnt_path, &iso, image_path, total_files, progress)?;
            }
            // Rufus "Windows User Experience" customizations
            if let Some(wue) = self.options.wue {
                self.apply_windows_user_experience(&mnt_path, &wue, progress)?;
            }
            // split install.wim if needed
            self.check_cancel()?;
            self.handle_install_wim(&mnt_path, progress)?;
            // boot files
            let boot_target = BootTarget {
                part_dev: &part_dev,
                part_start_lba,
                sector_size,
            };
            self.install_boot(&mnt_path, &iso, syslinux_dir, &boot_target, progress)?;
            // extended label + icon files (last, like Rufus's finalize step)
            self.write_autorun_files(&mnt_path)?;
            Ok(())
        })();

        // unmount regardless (a real unmount flushes the volume), then a
        // final device-level fsync so nothing is dirty at "Done!"
        let _ = unmount(&mnt);
        flush_partition(&part_dev);
        result?;
        disk.file.sync_all()?;
        progress(Progress {
            phase: "done".into(),
            done: total_files,
            total: total_files,
            message: "Done!".into(),
        });
        Ok(())
    }

    /// Shared write path: wipe, partition, format and prepare the trailing
    /// partitions (persistence / UEFI:NTFS). Used by the ISO, non-bootable
    /// and FreeDOS modes. Returns the prepared main partition.
    fn partition_and_format(
        &self,
        dev: &Device,
        disk: &mut Disk,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<PreppedPart> {
        // All partition/layout math below is done in 512-byte units while the
        // kernel interprets MBR/GPT LBAs in the device's logical sector size:
        // on 4Kn drives the written table would point at a different byte
        // offset than the one we format — silent, unrecoverable corruption.
        // Refuse instead (raw/dd mode has no such math and stays allowed).
        if disk.sector_size != 512 {
            bail!(
                "{} has {}-byte logical sectors — the partitioned write modes \
                 only support 512-byte-sector devices for now. Use a \
                 512-byte-sector drive, or the raw/dd mode.",
                dev.path,
                disk.sector_size
            );
        }
        let bps = 512u32; // == disk.sector_size after the check above
        if self.options.bb_test != BadBlocksTest::Disabled {
            self.badblocks_test(disk, progress)?;
        }
        // log the partition table we are about to erase
        let mut cur_sec = [0u8; 512];
        if disk.read_at(0, &mut cur_sec).is_ok() {
            let style = match crate::mbr::detect_style(&cur_sec) {
                crate::mbr::PartitionStyle::Gpt => "GPT".to_string(),
                crate::mbr::PartitionStyle::Mbr(ps) => format!("MBR ({} partitions)", ps.len()),
                crate::mbr::PartitionStyle::Empty => "empty".to_string(),
            };
            tracing::info!("erasing existing partition table: {style}");
        }
        progress(Progress {
            phase: "wipe".into(),
            done: 0,
            total: 0,
            message: "Wiping signatures...".into(),
        });
        disk.wipe_signatures()?;

        // 3. partition table
        // Windows To Go (Rufus parity): NTFS only, no persistence; GPT gets
        // an ESP (260 MB) + MSR (128 MB) in front of the main partition.
        let wtg = self.options.write_mode == WriteMode::WindowsToGo;
        // For NTFS/exFAT we add a trailing UEFI:NTFS partition holding the raw
        // uefi-ntfs.img (like Rufus XP_UEFI_NTFS). For Windows To Go Rufus
        // keeps it only on MBR — there it is the only way UEFI can boot the
        // NTFS volume, since WTG MBR has no ESP (GPT WTG uses the real ESP).
        let use_uefi_ntfs = matches!(self.options.fs, FsKind::Ntfs | FsKind::Exfat)
            && (!wtg || self.options.scheme == PartitionScheme::Mbr);
        let use_persistent = self.options.persistence_mib > 0 && !wtg;
        let uefi_ntfs_sectors = if use_uefi_ntfs {
            (UEFI_NTFS_IMG.len() as u64).div_ceil(512)
        } else {
            0
        };
        let disk_sectors = disk.size / 512;
        // "Add fixes for old BIOSes" (Rufus XP_COMPAT): a one-track "BIOS
        // Compatibility" partition at the end of the disk for BIOSes that
        // won't boot without it, and on MBR the first partition starts on a
        // track boundary instead of the MS 1 MiB wastage (doubled so the
        // GRUB2 core still fits, like Rufus does)
        let (_, heads, spt) = disk.geometry();
        let track_sectors = spt.max(1) as u64 * disk.sector_size / 512;
        let compat_sectors = if self.options.old_bios_fixes {
            track_sectors
        } else {
            0
        };
        // Rufus/drive.c WTG layout: ESP 260 MB "to keep everyone happy,
        // including 4K sector users", MSR 128 MB, both ahead of the data
        // partition on GPT
        const WTG_ESP_SECTORS: u64 = (260 * 1024 * 1024) / 512;
        const WTG_MSR_SECTORS: u64 = (128 * 1024 * 1024) / 512;
        let part_start_lba = if wtg && self.options.scheme == PartitionScheme::Gpt {
            2048 + WTG_ESP_SECTORS + WTG_MSR_SECTORS
        } else if self.options.old_bios_fixes && self.options.scheme == PartitionScheme::Mbr {
            2 * track_sectors
        } else {
            2048 // 1 MiB
        };
        // trailing partitions live at the disk end, backwards order:
        // [main][persistence][UEFI:NTFS]["BIOS Compatibility"] <- end of disk
        let persist_sectors = if use_persistent {
            self.options.persistence_mib.saturating_mul(1024 * 1024) / 512
        } else {
            0
        };
        let Layout {
            part_end_lba,
            uefi_ntfs_start,
            persist_start,
            persist_len,
            compat_start,
        } = compute_layout(
            disk_sectors,
            LayoutReq {
                part_start_lba,
                uefi_ntfs_sectors,
                persist_sectors,
                compat_sectors,
                track_sectors,
                gpt: self.options.scheme == PartitionScheme::Gpt,
            },
        )?;
        if compat_sectors > 0 {
            tracing::info!(
                "old-BIOS fixes: 'BIOS Compatibility' partition at LBA {compat_start} \
                 ({compat_sectors} sectors), first partition starts at LBA {part_start_lba}"
            );
        }

        progress(Progress {
            phase: "partition".into(),
            done: 0,
            total: 0,
            message: "Writing partition table...".into(),
        });
        let main_fs_mbr = match self.options.fs {
            FsKind::Fat32 => FsType::Fat32Lba,
            FsKind::Ntfs | FsKind::Exfat => FsType::Ntfs,
        };
        let mut main_part_index = 1u32;
        let mut esp_lba = 0u64;
        let mut disk_sig_ret = 0u32;
        let mut disk_guid_ret = crate::gpt::Guid::EMPTY;
        let mut part_guid_ret = crate::gpt::Guid::EMPTY;
        match self.options.scheme {
            PartitionScheme::Mbr => {
                // a "Non bootable" write gets no bootable flag (Rufus:
                // BT_NON_BOOTABLE only formats, the MBR is zeroed)
                let bootable =
                    !self.options.efi_only && self.options.write_mode != WriteMode::NonBootable;
                let mut parts = vec![Partition {
                    bootable,
                    fs: main_fs_mbr,
                    start_lba: part_start_lba,
                    sectors: part_end_lba - part_start_lba + 1,
                }];
                // random disk signature — every stick should be unique
                let sig = u32::from_le_bytes(random_bytes(4).try_into().expect("4 bytes"));
                disk_sig_ret = sig;
                main_part_index = 1;
                if use_persistent {
                    parts.push(Partition {
                        bootable: false,
                        fs: FsType::Linux,
                        start_lba: persist_start,
                        sectors: persist_len,
                    });
                }
                if use_uefi_ntfs {
                    parts.push(Partition {
                        bootable: false,
                        fs: FsType::GptProtective,
                        start_lba: uefi_ntfs_start,
                        // with the compat partition at the tail, the UEFI:NTFS
                        // entry must stop right below it
                        sectors: (if compat_start > 0 {
                            compat_start
                        } else {
                            disk_sectors
                        }) - uefi_ntfs_start,
                    });
                    // fix type byte for UEFI:NTFS partition to 0xEF (ESP)
                    // FsType has no Esp variant; encode via raw? use Fat32Lba is wrong.
                    // We'll patch the built sector manually.
                }
                if compat_start > 0 {
                    // Rufus "BIOS Compatibility" partition (XP_COMPAT): one
                    // track, MBR type 0xEA (RUFUS_EXTRA_PARTITION_TYPE)
                    parts.push(Partition {
                        bootable: false,
                        fs: FsType::RufusExtra,
                        start_lba: compat_start,
                        sectors: compat_sectors,
                    });
                }
                // "Use Rufus MBR with BIOS ID": checked = the Rufus MBR boot
                // code, unchecked = the Windows 7 MBR (Rufus format.c parity);
                // UEFI-only/non-bootable writes get a zeroed boot code
                let code: Option<&[u8]> =
                    if self.options.efi_only || self.options.write_mode == WriteMode::NonBootable {
                        None
                    } else if self.options.rufus_mbr {
                        tracing::info!("Using Rufus MBR");
                        Some(MBR_BOOTCODE)
                    } else {
                        tracing::info!("Using Windows 7 MBR");
                        Some(MBR_WIN7)
                    };
                let mut mbr_sec = mbr::build_mbr(&parts, code, sig, heads as u16, spt);
                // Rufus format.c: "Set first partition bootable - masquerade
                // as per the DiskID selected" — only for bootable BIOS writes
                // with the Rufus MBR
                if bootable && self.options.rufus_mbr && self.options.disk_id != 0x80 {
                    mbr_sec[0x1BE] = self.options.disk_id;
                    tracing::info!(
                        "Set bootable USB partition as 0x{:02X}",
                        self.options.disk_id
                    );
                }
                // patch extra partition types: persistence=0x83 already Linux, uefi-ntfs=0xEF
                let mut idx = 1usize;
                if use_persistent {
                    idx += 1;
                }
                if use_uefi_ntfs {
                    mbr_sec[0x1BE + idx * 16 + 4] = 0xEF;
                }
                disk.write_at(0, &mbr_sec)?;
            }
            PartitionScheme::Gpt => {
                if disk.sector_size != 512 {
                    bail!(
                        "GPT is not supported yet on {}-byte-sector devices — use MBR",
                        disk.sector_size
                    );
                }
                let sectors = disk_sectors;
                // helper: write primary + backup GPT tables
                fn write_gpt(
                    disk: &mut Disk,
                    sectors: u64,
                    mbr_sec: &[u8; 512],
                    hdr: &[u8; 512],
                    entries: &[[u8; 512]],
                ) -> Result<()> {
                    disk.write_at(0, mbr_sec)?;
                    disk.write_at(512, hdr)?;
                    for (i, es) in entries.iter().enumerate() {
                        disk.write_at((2 + i as u64) * 512, es)?;
                    }
                    // backup GPT: header at last LBA, entries just before it
                    let last = sectors - 1;
                    let ent_lba = last - 32;
                    let mut hdr2 = *hdr;
                    hdr2[24..32].copy_from_slice(&last.to_le_bytes()); // MyLBA
                    hdr2[32..40].copy_from_slice(&1u64.to_le_bytes()); // alternate
                    hdr2[72..80].copy_from_slice(&ent_lba.to_le_bytes());
                    hdr2[16..20].copy_from_slice(&0u32.to_le_bytes());
                    let c = crate::gpt::crc32(&hdr2[..92]);
                    hdr2[16..20].copy_from_slice(&c.to_le_bytes());
                    disk.write_at(last * 512, &hdr2)?;
                    for (i, es) in entries.iter().enumerate() {
                        disk.write_at((ent_lba + i as u64) * 512, es)?;
                    }
                    Ok(())
                }
                if wtg {
                    // Rufus WTG GPT layout: [ESP][MSR][main]
                    let esp_guid = crate::gpt::Guid(random_bytes(16).try_into().expect("16"));
                    let msr_guid = crate::gpt::Guid(random_bytes(16).try_into().expect("16"));
                    let part_guid = crate::gpt::Guid(random_bytes(16).try_into().expect("16"));
                    let disk_guid = crate::gpt::Guid(random_bytes(16).try_into().expect("16"));
                    let esp_start = 2048;
                    let (mbr_sec, hdr, entries) = crate::gpt::build_gpt_entries(
                        sectors,
                        disk_guid,
                        &[
                            GptPartSpec {
                                type_guid: crate::gpt::Guid::EFI_SYSTEM,
                                unique_guid: esp_guid,
                                first_lba: esp_start,
                                last_lba: esp_start + WTG_ESP_SECTORS - 1,
                                name: "EFI System Partition".into(),
                            },
                            GptPartSpec {
                                type_guid: crate::gpt::Guid::MS_RESERVED,
                                unique_guid: msr_guid,
                                first_lba: esp_start + WTG_ESP_SECTORS,
                                last_lba: esp_start + WTG_ESP_SECTORS + WTG_MSR_SECTORS - 1,
                                name: "Microsoft Reserved Partition".into(),
                            },
                            GptPartSpec {
                                type_guid: crate::gpt::Guid::MS_BASIC_DATA,
                                unique_guid: part_guid,
                                first_lba: part_start_lba,
                                last_lba: part_end_lba,
                                name: self.options.fs_label.clone(),
                            },
                        ],
                    )?;
                    write_gpt(disk, sectors, &mbr_sec, &hdr, &entries)?;
                    esp_lba = esp_start;
                    main_part_index = 3;
                    disk_sig_ret = 0;
                    disk_guid_ret = disk_guid;
                    part_guid_ret = part_guid;
                } else {
                    // GPT path: main partition from build_gpt_single, plus optional
                    // persistence / UEFI:NTFS entries in partition order (2, 3).
                    let guid = crate::gpt::Guid(random_bytes(16).try_into().expect("16 bytes"));
                    let (mbr_sec, mut hdr, mut entries) =
                        crate::gpt::build_gpt_single(sectors, part_start_lba, part_end_lba, guid)?;
                    let set_entry = |entries: &mut [[u8; 512]],
                                     idx: usize,
                                     type_guid: &crate::gpt::Guid,
                                     first: u64,
                                     last: u64,
                                     name: &str| {
                        let e = &mut entries[0][idx * 128..(idx + 1) * 128];
                        e[0..16].copy_from_slice(&type_guid.0);
                        e[16..32].copy_from_slice(&guid.0);
                        e[32..40].copy_from_slice(&first.to_le_bytes());
                        e[40..48].copy_from_slice(&last.to_le_bytes());
                        for (i, c) in name.encode_utf16().take(36).enumerate() {
                            e[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
                        }
                    };
                    let mut idx = 1usize;
                    if use_persistent {
                        set_entry(
                            &mut entries,
                            idx,
                            &crate::gpt::Guid::LINUX_FS,
                            persist_start,
                            persist_start + persist_len - 1,
                            "casper-rw",
                        );
                        idx += 1;
                    }
                    if use_uefi_ntfs {
                        // end the partition before the backup GPT area (last usable
                        // LBA), or below the compat partition when present
                        let uefi_last = if compat_start > 0 {
                            compat_start - 1
                        } else {
                            sectors.saturating_sub(34)
                        };
                        set_entry(
                            &mut entries,
                            idx,
                            &crate::gpt::Guid::EFI_SYSTEM,
                            uefi_ntfs_start,
                            uefi_last,
                            "UEFI:NTFS",
                        );
                    }
                    if compat_start > 0 {
                        // Rufus "BIOS Compatibility": a Microsoft basic data entry
                        // on GPT, same one-track size as the MBR variant
                        let compat_idx = if use_uefi_ntfs { idx + 1 } else { idx };
                        set_entry(
                            &mut entries,
                            compat_idx,
                            &crate::gpt::Guid::MS_BASIC_DATA,
                            compat_start,
                            compat_start + compat_sectors - 1,
                            "BIOS Compatibility",
                        );
                    }
                    // recompute entries CRC
                    let mut eb = Vec::with_capacity(entries.len() * 512);
                    for s in &entries {
                        eb.extend_from_slice(s);
                    }
                    let ecrc = crate::gpt::crc32(&eb) as u64;
                    hdr[88..96].copy_from_slice(&ecrc.to_le_bytes());
                    hdr[16..20].copy_from_slice(&0u32.to_le_bytes());
                    let hcrc = crate::gpt::crc32(&hdr[..92]);
                    hdr[16..20].copy_from_slice(&hcrc.to_le_bytes());
                    write_gpt(disk, sectors, &mbr_sec, &hdr, &entries)?;
                    disk_guid_ret = guid;
                    part_guid_ret = guid;
                }
            }
        }
        // like Rufus's ClearPartition(): zero the compat partition so no
        // stale filesystem signature from a previous layout survives
        if compat_start > 0 {
            let zeros = vec![0u8; (compat_sectors * 512) as usize];
            disk.write_at(compat_start * 512, &zeros)?;
        }
        // udisks/desktop may have auto-mounted old partitions — detach them first
        detach_mounts(&dev.path);
        // retry BLKRRPART a few times (kernel can be slow to release)
        let mut tries = 0;
        loop {
            match disk.reread_partitions() {
                Ok(()) => break,
                Err(e) if tries < 5 => {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    tries += 1;
                    if tries == 5 {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // 4. format main partition
        let part_bytes = (part_end_lba - part_start_lba + 1) * 512;
        let part_dev = crate::device::partition_node(&dev.path, main_part_index);
        progress(Progress {
            phase: "format".into(),
            done: 0,
            total: part_bytes,
            message: format!("Formatting {}...", self.options.fs.name()),
        });
        match self.options.fs {
            FsKind::Fat32 => {
                let mut w = RegionWriter {
                    disk,
                    base: part_start_lba * 512,
                    pos: 0,
                };
                let fat_label = sanitize_fat_label(&self.options.fs_label);
                fat32::format_fat32(
                    &mut w,
                    part_bytes,
                    bps,
                    &fat_label,
                    part_start_lba as u32,
                    !self.options.quick,
                    &mut |d, t| {
                        progress(Progress {
                            phase: "format".into(),
                            done: d,
                            total: t,
                            message: "Formatting FAT32...".into(),
                        });
                    },
                )?;
            }
            FsKind::Ntfs | FsKind::Exfat => {
                // external mkfs on the partition device node; mkfs.ntfs lives
                // in /usr/sbin which is missing from some elevated PATHs
                let tool = if matches!(self.options.fs, FsKind::Ntfs) {
                    find_tool("mkfs.ntfs")
                        .context("mkfs.ntfs not found (install the ntfs-3g package)")?
                } else {
                    find_tool("mkfs.exfat").context("mkfs.exfat not found (install exfatprogs)")?
                };
                let mut args: Vec<String> = vec!["-F".into()]; // fast/force where supported
                if matches!(self.options.fs, FsKind::Ntfs) {
                    args = vec![
                        "-Q".into(),
                        "-F".into(),
                        "-L".into(),
                        self.options.fs_label.clone(),
                    ];
                } else {
                    args = vec!["-n".into(), self.options.fs_label.clone()];
                }
                args.push(part_dev.clone());
                let st = Command::new(&tool)
                    .args(&args)
                    .status()
                    .with_context(|| format!("{tool} failed/missing"))?;
                if !st.success() {
                    bail!("{tool} exited with {st}");
                }
            }
        }
        disk.file.sync_all()?;

        // 4a. Windows To Go on GPT: format the ESP with our native FAT32
        // formatter (Rufus formats it FAT32 with a cluster size >= the
        // sector size and no label)
        if wtg && esp_lba > 0 {
            let esp_bytes = WTG_ESP_SECTORS * 512;
            progress(Progress {
                phase: "format".into(),
                done: 0,
                total: esp_bytes,
                message: "Formatting EFI System Partition...".into(),
            });
            let mut w = RegionWriter {
                disk,
                base: esp_lba * 512,
                pos: 0,
            };
            fat32::format_fat32(
                &mut w,
                esp_bytes,
                bps,
                "",
                esp_lba as u32,
                !self.options.quick,
                &mut |d, t| {
                    progress(Progress {
                        phase: "format".into(),
                        done: d,
                        total: t,
                        message: "Formatting ESP (FAT32)...".into(),
                    });
                },
            )?;
            disk.file.sync_all()?;
        }

        // 4b. write UEFI:NTFS image raw to trailing partition
        if use_uefi_ntfs {
            let p2 = crate::device::partition_node(&dev.path, if use_persistent { 3 } else { 2 });
            progress(Progress {
                phase: "uefi-ntfs".into(),
                done: 0,
                total: UEFI_NTFS_IMG.len() as u64,
                message: "Writing UEFI:NTFS partition...".into(),
            });
            let mut f = crate::disk::open_partition(&p2)?;
            use std::io::Write as _;
            f.write_all(UEFI_NTFS_IMG)?;
            f.sync_all()?;
        }

        // 4c. persistent partition: mkfs.ext4 -L casper-rw
        // partition order is main=1, persistence=2, [UEFI:NTFS=3]
        if use_persistent {
            let persist_dev = crate::device::partition_node(&dev.path, 2);
            let st = Command::new("mkfs.ext4")
                .args(["-q", "-F", "-L", "casper-rw", &persist_dev])
                .status();
            match st {
                Ok(s) if s.success() => progress(Progress {
                    phase: "format".into(),
                    done: 1,
                    total: 1,
                    message: "Persistence partition (casper-rw) ready".into(),
                }),
                _ => progress(Progress {
                    phase: "format".into(),
                    done: 0,
                    total: 1,
                    message: "WARNING: mkfs.ext4 failed — persistence unavailable".into(),
                }),
            }
        }
        Ok(PreppedPart {
            part_dev,
            part_start_lba,
            sector_size: disk.sector_size,
            main_part_index,
            esp_lba,
            disk_sig: disk_sig_ret,
            disk_guid: disk_guid_ret,
            part_guid: part_guid_ret,
        })
    }

    /// "Preserve timestamps" hidden setting (Rufus PreserveTimestamps): stamp
    /// the written file with the ISO directory-record date. Best-effort — a
    /// failed mtime never fails the write itself.
    fn apply_preserved_mtime(&self, dest: &Path, e: &iso9660::IsoEntry) {
        if !self.options.preserve_timestamps || e.mtime <= 0 {
            return;
        }
        let Ok(f) = std::fs::File::options().write(true).open(dest) else {
            return;
        };
        let t = std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(e.mtime as u64));
        if let Err(err) = f.set_times(t) {
            tracing::debug!("could not set mtime on {}: {err}", e.path);
        }
    }

    /// Verify copied files by SHA-256 content comparison against the ISO.
    /// Must run while the target filesystem is still mounted.
    fn verify_copy(
        &self,
        mnt: &Path,
        iso: &IsoImage,
        image_path: &Path,
        total_files: u64,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        progress(Progress {
            phase: "verify".into(),
            done: 0,
            total: total_files,
            message: "Verifying (SHA-256)...".into(),
        });
        let mut iso_file = std::fs::File::open(image_path)?;
        let mut buf = vec![0u8; 1 << 20];
        let files: Vec<&iso9660::IsoEntry> = iso.entries.iter().filter(|e| !e.is_dir).collect();
        for (i, e) in files.iter().enumerate() {
            // hash the file as stored in the ISO
            let mut h_iso = Sha256::new();
            iso_file.seek(SeekFrom::Start(e.extent_lba * iso9660::SECTOR))?;
            let mut remaining = e.size;
            while remaining > 0 {
                let want = remaining.min(buf.len() as u64) as usize;
                let n = iso_file.read(&mut buf[..want])?;
                if n == 0 {
                    bail!(
                        "verify: {} truncated in image ({} bytes missing)",
                        e.path,
                        remaining
                    );
                }
                h_iso.update(&buf[..n]);
                remaining -= n as u64;
            }
            // hash the copied file on the target
            let mut f = std::fs::File::open(mnt.join(&e.path))
                .with_context(|| format!("verify: file missing on target: {}", e.path))?;
            let mut h_tgt = Sha256::new();
            // digest 0.11 hashers no longer implement std::io::Write, so
            // hash the copied file with an explicit read loop.
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h_tgt.update(&buf[..n]);
            }
            if h_iso.finalize() != h_tgt.finalize() {
                bail!("verify failed: {} content mismatch", e.path);
            }
            self.check_cancel()?;
            progress(Progress {
                phase: "verify".into(),
                done: (i + 1) as u64,
                total: total_files,
                message: "Verifying (SHA-256)...".into(),
            });
        }
        progress(Progress {
            phase: "verify".into(),
            done: total_files,
            total: total_files,
            message: "Verify OK".into(),
        });
        Ok(())
    }

    /// "Non bootable" mode (Rufus): wipe + partition + format only.
    /// Encode a string as UTF-16LE with a BOM — the original writes
    /// autorun.inf through fopen "w, ccs=UTF-16LE".
    fn utf16le_with_bom(s: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity((s.len() + 1) * 2);
        out.extend_from_slice(&[0xFF, 0xFE]); // BOM
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    /// Rufus "Create extended label and icon files" (icon.c SetAutorun):
    /// autorun.inf in UTF-16LE with the volume label + Rufus icon line, and
    /// autorun.ico next to it, so file managers can show labels longer than
    /// the 11/32-char filesystem limit and assign a device icon. An existing
    /// autorun.inf (e.g. from the image itself) is never overwritten.
    fn write_autorun_files(&self, mnt: &Path) -> Result<()> {
        if !self.options.autorun {
            return Ok(());
        }
        let inf = mnt.join("autorun.inf");
        if inf.exists() {
            tracing::info!("{} already exists - keeping it", inf.display());
            return Ok(());
        }
        let text = format!(
            "; Created by rufus-linux {}\r\n; https://github.com/pbatard/rufus\r\n\
             [autorun]\r\nicon  = autorun.ico\r\nlabel = {}\r\n",
            env!("CARGO_PKG_VERSION"),
            self.options.fs_label
        );
        std::fs::write(&inf, Self::utf16le_with_bom(&text))?;
        tracing::info!("Created: {}", inf.display());
        let ico = mnt.join("autorun.ico");
        if !ico.exists() {
            std::fs::write(&ico, AUTORUN_ICON)?;
            tracing::info!("Created: {}", ico.display());
        }
        Ok(())
    }

    fn format_only(&self, dev: &Device, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        let mut disk = Disk::open(&dev.path)?;
        let prep = self.partition_and_format(dev, &mut disk, progress)?;
        // Rufus keeps writing the extended label/icon files on a
        // non-bootable volume
        if self.options.autorun {
            let mnt = format!("/tmp/rufus-linux-nonboot-{}", dev.name);
            std::fs::create_dir_all(&mnt)?;
            detach_mounts(&dev.path);
            match mount_fs(&prep.part_dev, &mnt, self.options.fs) {
                Ok(()) => {
                    let r = self.write_autorun_files(Path::new(&mnt));
                    let _ = unmount(&mnt);
                    flush_partition(&prep.part_dev);
                    r?;
                }
                Err(e) => {
                    tracing::warn!("could not mount volume for autorun files: {e}");
                }
            }
            let _ = std::fs::remove_dir_all(&mnt);
        }
        disk.file.sync_all()?;
        progress(Progress {
            phase: "done".into(),
            done: 1,
            total: 1,
            message: "Non-bootable format complete".into(),
        });
        Ok(())
    }

    /// "FreeDOS" mode (Rufus): FAT32 volume with the ms-sys DOS/FreeDOS boot
    /// record (assets/freedos/br_fat32*.bin) and the FreeDOS boot files.
    fn write_freedos(&self, dev: &Device, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        if self.options.fs != FsKind::Fat32 {
            bail!("FreeDOS requires the FAT32 file system");
        }
        let mut disk = Disk::open(&dev.path)?;
        if disk.sector_size != 512 {
            bail!(
                "FreeDOS BIOS boot is not possible on {}-byte sectors",
                disk.sector_size
            );
        }
        let prep = self.partition_and_format(dev, &mut disk, progress)?;

        // Assemble the ms-sys FAT32 boot record over the freshly formatted
        // volume: the fragments carry the loader code while our formatter's
        // BPB (0x0B..0x52) is kept verbatim, exactly like Rufus.
        let mut bpb = [0u8; 0x52 - 0x0B];
        disk.read_at(prep.part_start_lba * 512 + 0x0B, &mut bpb)?;
        let br = assemble_freedos_br(&bpb);
        disk.write_at(prep.part_start_lba * 512, &br)?;
        // mirror onto the backup boot record (sectors 6-8)
        disk.write_at((prep.part_start_lba + 6) * 512, &br)?;
        disk.file.sync_all()?;

        // mount and write the FreeDOS files
        let mnt = format!("/tmp/rufus-linux-freedos-{}", dev.name);
        std::fs::create_dir_all(&mnt)?;
        detach_mounts(&dev.path);
        mount_fs(&prep.part_dev, &mnt, FsKind::Fat32)?;
        let result = (|| -> Result<()> {
            progress(Progress {
                phase: "copy".into(),
                done: 0,
                total: 2,
                message: "Writing FreeDOS files...".into(),
            });
            std::fs::write(Path::new(&mnt).join("KERNEL.SYS"), FREEDOS_KERNEL)?;
            std::fs::write(Path::new(&mnt).join("COMMAND.COM"), FREEDOS_COMMAND)?;
            progress(Progress {
                phase: "copy".into(),
                done: 2,
                total: 2,
                message: "Writing FreeDOS files...".into(),
            });
            // minimal DOS configuration (CRLF line endings — DOS requires them)
            std::fs::write(
                Path::new(&mnt).join("AUTOEXEC.BAT"),
                b"@ECHO OFF\r\nCLS\r\nVER\r\n",
            )?;
            std::fs::write(
                Path::new(&mnt).join("CONFIG.SYS"),
                b"FILES=40\r\nBUFFERS=20\r\nLASTDRIVE=Z\r\n",
            )?;
            self.write_autorun_files(Path::new(&mnt))?;
            Ok(())
        })();
        let _ = unmount(&mnt);
        flush_partition(&prep.part_dev);
        let _ = std::fs::remove_dir_all(&mnt);
        result?;
        disk.file.sync_all()?;
        progress(Progress {
            phase: "done".into(),
            done: 1,
            total: 1,
            message: "FreeDOS stick ready".into(),
        });
        Ok(())
    }

    /// "Windows To Go" image option (Rufus): apply the OS image from
    /// sources/install.wim onto an NTFS volume (wimlib, like Rufus 4.x)
    /// and set up the boot files ourselves, since bcdboot.exe is
    /// Windows-only: on GPT the ESP receives bootmgfw.efi + a generated
    /// BCD; on MBR the NTFS volume receives bootmgr + BCD + an NTFS
    /// partition boot record (ms-sys). Recovery is disabled and the WUE
    /// unattend.xml goes to \Windows\Panther, matching Rufus.
    fn write_windows_to_go(
        &self,
        dev: &Device,
        image_path: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        if self.options.fs != FsKind::Ntfs {
            bail!("Windows To Go can only be applied with the NTFS file system");
        }
        // 1. inspect the ISO
        progress(Progress {
            phase: "inspect".into(),
            done: 0,
            total: 0,
            message: "Reading ISO...".into(),
        });
        let mut iso_file = std::fs::File::open(image_path)?;
        let iso: IsoImage = iso9660::parse_iso(&mut iso_file)?;
        let wim_entry = find_windows_wim(&iso).ok_or_else(|| {
            anyhow::anyhow!(
                "the image has no sources/install.wim — Windows To Go needs a Windows install ISO"
            )
        })?;
        let wim_size = wim_entry.size;
        let is_swm = wim_entry.path.to_lowercase().ends_with(".swm");
        let swm_parts: Vec<iso9660::IsoEntry> = if is_swm {
            iso.entries
                .iter()
                .filter(|e| {
                    !e.is_dir
                        && e.path.to_lowercase().starts_with("sources/install")
                        && e.path.to_lowercase().ends_with(".swm")
                })
                .cloned()
                .collect()
        } else {
            vec![]
        };

        // 2. wipe + partition + format (NTFS main, ESP/MSR on GPT)
        progress(Progress {
            phase: "open".into(),
            done: 0,
            total: 0,
            message: format!("Opening {}", dev.path),
        });
        let mut disk = Disk::open(&dev.path)?;
        let prep = self.partition_and_format(dev, &mut disk, progress)?;

        // 3. make the WIM reachable: loop-mount the ISO read-only, else
        //    extract install.wim natively to a temp dir (progress-reported)
        let wim_dir = format!("/tmp/rufus-linux-iso-{}", std::process::id());
        let mut mounted_iso = false;
        let wim_path: PathBuf = {
            std::fs::create_dir_all(&wim_dir)?;
            let ok = Command::new("mount")
                .args(["-o", "loop,ro", &image_path.to_string_lossy(), &wim_dir])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let mut found = None;
            if ok {
                mounted_iso = true;
                // locate the (possibly differently cased) WIM on the mount
                for rel in wim_candidates(&swm_parts, wim_entry) {
                    let p = Path::new(&wim_dir).join(&rel);
                    if p.is_file() {
                        found = Some(PathBuf::from(&p));
                        break;
                    }
                }
                if found.is_none() {
                    let _ = unmount(&wim_dir);
                    mounted_iso = false;
                }
            }
            match found {
                Some(p) => p,
                None => {
                    // native extraction fallback (no root loop-mount available)
                    let names: Vec<String> = if is_swm {
                        swm_parts.iter().map(|e| e.path.clone()).collect()
                    } else {
                        vec![wim_entry.path.clone()]
                    };
                    let out_dir =
                        std::env::temp_dir().join(format!("rufus-wim-{}", std::process::id()));
                    std::fs::create_dir_all(&out_dir)?;
                    for (i, name) in names.iter().enumerate() {
                        let e = iso
                            .entries
                            .iter()
                            .find(|e| &e.path == name)
                            .ok_or_else(|| anyhow::anyhow!("missing {name} in ISO"))?;
                        let dest = out_dir.join(name.rsplit('/').next().unwrap_or(name));
                        progress(Progress {
                            phase: "wtg-wim".into(),
                            done: i as u64,
                            total: names.len() as u64,
                            message: format!("Extracting {} ...", e.path),
                        });
                        iso9660::extract_file(&mut iso_file, e, &dest)?;
                    }
                    let first =
                        out_dir.join(wim_entry.path.rsplit('/').next().unwrap_or("install.wim"));
                    first
                }
            }
        };
        tracing::info!(
            "WTG: applying {} (mounted ISO: {mounted_iso})",
            wim_path.display()
        );

        // 4. apply the image onto the NTFS volume (wimlib NTFS mode, like
        //    Rufus's WimApplyImage); keep the volume unmounted meanwhile
        let index = self.options.wim_index.max(1);
        self.apply_wim(&wim_path, index, &prep.part_dev, progress)?;

        // 5. boot files + unattend (both need the applied volume mounted)
        let result = self.setup_wtg_boot(dev, &prep, image_path, progress);

        // 6. cleanup mounts
        if mounted_iso {
            let _ = unmount(&wim_dir);
        }
        let _ = std::fs::remove_dir_all(&wim_dir);
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("rufus-wim-{}", std::process::id())),
        );
        result?;
        disk.file.sync_all()?;
        progress(Progress {
            phase: "done".into(),
            done: 1,
            total: 1,
            message: "Windows To Go drive ready".into(),
        });
        let _ = wim_size;
        Ok(())
    }

    /// Run `wimlib-imagex apply <wim> <index> <target>` with a cancellation
    /// watchdog (Rufus aborts WimApplyImage through its progress callback).
    fn apply_wim(
        &self,
        wim_path: &Path,
        index: u32,
        target: &str,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let tool = find_tool("wimlib-imagex").ok_or_else(|| {
            anyhow::anyhow!("wimlib-imagex not found — install the wimtools package")
        })?;
        progress(Progress {
            phase: "wtg-apply".into(),
            done: 0,
            total: 0,
            message: format!("Applying Windows image (index {index})..."),
        });
        // split WIMs: primary part + --ref for the remaining parts
        let mut args = vec![
            "apply".to_string(),
            wim_path.to_string_lossy().to_string(),
            index.to_string(),
            target.to_string(),
        ];
        if let Some(parent) = wim_path.parent() {
            if let Ok(rd) = std::fs::read_dir(parent) {
                let stem = wim_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.to_lowercase().ends_with(".swm") && n != stem {
                        args.push(format!("--ref={}", parent.join(n).to_string_lossy()));
                    }
                }
            }
        }
        let mut child = Command::new(&tool)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("{tool} apply failed to start"))?;
        loop {
            match child.try_wait()? {
                Some(status) => {
                    if status.success() {
                        progress(Progress {
                            phase: "wtg-apply".into(),
                            done: 1,
                            total: 1,
                            message: "Windows image applied".into(),
                        });
                        return Ok(());
                    }
                    bail!("wimlib-imagex apply exited with {status} — check that the target is a valid NTFS volume and that wimlib has NTFS support");
                }
                None => {
                    if let Some(c) = &self.cancel {
                        if c.load(Ordering::Relaxed) {
                            let _ = child.kill();
                            let _ = child.wait();
                            bail!("write cancelled by user");
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }

    /// Mount the applied volume(s) and install the boot files:
    /// GPT: bootmgfw.efi + BCD on the ESP; MBR: bootmgr + BCD on the NTFS
    /// volume plus the ms-sys NTFS partition boot record. Then drop the WUE
    /// unattend.xml into \Windows\Panther (Rufus's ApplyWindowsCustomization
    /// with UNATTEND_WINDOWS_TO_GO).
    fn setup_wtg_boot(
        &self,
        dev: &Device,
        prep: &PreppedPart,
        image_path: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let gpt = prep.esp_lba > 0;
        if !gpt {
            // NTFS partition boot record: keep the BPB/$MFT pointers that
            // mkfs.ntfs wrote and overlay the ms-sys bootmgr loader code
            progress(Progress {
                phase: "wtg-boot".into(),
                done: 0,
                total: 1,
                message: "Writing NTFS boot sector...".into(),
            });
            let mut bpb = [0u8; 0x54 - 0x0B];
            let mut disk = Disk::open(&dev.path)?;
            disk.read_at(prep.part_start_lba * 512 + 0x0B, &mut bpb)?;
            let br = assemble_ntfs_pbr(&bpb);
            disk.write_at(prep.part_start_lba * 512, &br)?;
            disk.file.sync_all()?;
        }

        // mount the NTFS volume (bootmgr copy + unattend target)
        let mnt = format!("/tmp/rufus-linux-wtg-{}", dev.name);
        std::fs::create_dir_all(&mnt)?;
        detach_mounts(&prep.part_dev);
        progress(Progress {
            phase: "mount".into(),
            done: 0,
            total: 0,
            message: format!("Mounting {} at {mnt}", prep.part_dev),
        });
        mount_fs(&prep.part_dev, &mnt, FsKind::Ntfs)?;

        // ESP mount for the GPT layout
        let esp_mnt = format!("/tmp/rufus-linux-wtg-esp-{}", dev.name);
        let mut esp_mounted = false;
        if gpt {
            let esp_dev = crate::device::partition_node(&dev.path, 1);
            std::fs::create_dir_all(&esp_mnt)?;
            detach_mounts(&esp_dev);
            mount_fs(&esp_dev, &esp_mnt, FsKind::Fat32)?;
            esp_mounted = true;
        }

        let result = (|| -> Result<()> {
            let mnt_path = PathBuf::from(&mnt);
            // edition name for the boot entry description (Rufus uses it)
            let description = wim_image_list(image_path)
                .ok()
                .and_then(|list| {
                    let idx = self.options.wim_index.max(1);
                    list.into_iter().find(|i| i.index == idx).map(|i| i.name)
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Windows To Go".to_string());
            // the ISO image (parsed once) supplies the real BCD store
            let iso = std::fs::File::open(image_path)
                .ok()
                .and_then(|mut f| iso9660::parse_iso(&mut f).ok());
            let entry_guid = random_guid_string();
            let boot_dev = if gpt {
                BcdDevice::Gpt {
                    disk_guid: prep.disk_guid,
                    partition_guid: prep.part_guid,
                }
            } else {
                BcdDevice::Mbr {
                    disk_signature: prep.disk_sig,
                    partition_offset: prep.part_start_lba * 512,
                }
            };
            let loader = if gpt {
                "\\Windows\\system32\\winload.efi"
            } else {
                "\\Windows\\system32\\winload.exe"
            };
            // Primary path: reuse the BCD store that ships on the Windows ISO
            // (a real bcdboot-made hive, guaranteed bootmgfw-compatible —
            // verified in QEMU) and retarget its "Windows Setup" osloader
            // entry at our Windows partition. Fallback: build one from scratch.
            let iso_bcd_path = if gpt {
                iso.as_ref()
                    .and_then(|iso| find_case_insensitive_path(iso, "efi/microsoft/boot/bcd"))
            } else {
                iso.as_ref()
                    .and_then(|iso| find_case_insensitive_path(iso, "boot/bcd"))
            };
            let make_bcd = |loader: &str| -> Result<Vec<u8>> {
                let iso_bcd_path = iso_bcd_path.clone();
                match iso_bcd_path {
                    Some(entry) => {
                        let mut store = Vec::with_capacity(entry.size as usize);
                        let mut f = std::fs::File::open(image_path)?;
                        match iso9660::read_file(&mut f, &entry, &mut store) {
                            Ok(_) => match bcd::patch_bcd_for_wtg(
                                &mut store,
                                &boot_dev,
                                loader,
                                &description,
                            ) {
                                Ok(()) => {
                                    tracing::info!("WTG: retargeted the ISO's own BCD store");
                                    Ok(store)
                                }
                                Err(e) => {
                                    tracing::warn!("WTG: BCD patch failed ({e:#}) — building a store from scratch");
                                    Ok(bcd::build_bcd(&bcd::BcdSpec {
                                        boot_device: boot_dev.clone(),
                                        os_device: boot_dev.clone(),
                                        entry_guid: entry_guid.clone(),
                                        entry_description: description.clone(),
                                        loader_path: loader.to_string(),
                                        locale: "en-US".into(),
                                        kernel_debug: false,
                                    })?)
                                }
                            },
                            Err(e) => {
                                tracing::warn!("WTG: could not read the ISO BCD ({e}) — building a store from scratch");
                                Ok(bcd::build_bcd(&bcd::BcdSpec {
                                    boot_device: boot_dev.clone(),
                                    os_device: boot_dev.clone(),
                                    entry_guid: entry_guid.clone(),
                                    entry_description: description.clone(),
                                    loader_path: loader.to_string(),
                                    locale: "en-US".into(),
                                    kernel_debug: false,
                                })?)
                            }
                        }
                    }
                    None => Ok(bcd::build_bcd(&bcd::BcdSpec {
                        boot_device: boot_dev.clone(),
                        os_device: boot_dev.clone(),
                        entry_guid: entry_guid.clone(),
                        entry_description: description.clone(),
                        loader_path: loader.to_string(),
                        locale: "en-US".into(),
                        kernel_debug: false,
                    })?),
                }
            };
            // On MBR, Rufus's bcdboot /f ALL lands both BIOS and UEFI boot
            // files on the NTFS volume (UEFI boots it through the trailing
            // uefi-ntfs partition), each with its own BCD loader variant
            let (bcd, bcd_efi) = if gpt {
                (make_bcd(loader)?, None)
            } else {
                (
                    make_bcd("\\Windows\\system32\\winload.exe")?,
                    Some(make_bcd("\\Windows\\system32\\winload.efi")?),
                )
            };

            if gpt {
                // bootmgfw.efi from the applied image → ESP\EFI\Boot\bootx64.efi
                let src = find_case_insensitive(&mnt_path, "Windows/Boot/EFI/bootmgfw.efi")
                    .ok_or_else(|| {
                        anyhow::anyhow!("applied image lacks Windows/Boot/EFI/bootmgfw.efi")
                    })?;
                let esp_path = PathBuf::from(&esp_mnt);
                std::fs::create_dir_all(esp_path.join("EFI/Boot"))?;
                std::fs::create_dir_all(esp_path.join("EFI/Microsoft/Boot"))?;
                std::fs::copy(&src, esp_path.join("EFI/Boot/bootx64.efi"))?;
                std::fs::write(esp_path.join("EFI/Microsoft/Boot/BCD"), &bcd)?;
                progress(Progress {
                    phase: "wtg-boot".into(),
                    done: 1,
                    total: 2,
                    message: "UEFI boot files written to the ESP".into(),
                });
            } else {
                // bootmgr at the volume root + \Boot\BCD (BIOS), plus the
                // UEFI files bcdboot /f ALL would add for the uefi-ntfs path
                let pcat = find_case_insensitive(&mnt_path, "Windows/Boot/PCAT/bootmgr")
                    .ok_or_else(|| {
                        anyhow::anyhow!("applied image lacks Windows/Boot/PCAT/bootmgr")
                    })?;
                std::fs::copy(&pcat, mnt_path.join("bootmgr"))?;
                std::fs::create_dir_all(mnt_path.join("Boot"))?;
                std::fs::write(mnt_path.join("Boot/BCD"), &bcd)?;
                if let Some(bcd_efi) = bcd_efi {
                    let efi = find_case_insensitive(&mnt_path, "Windows/Boot/EFI/bootmgfw.efi")
                        .ok_or_else(|| {
                            anyhow::anyhow!("applied image lacks Windows/Boot/EFI/bootmgfw.efi")
                        })?;
                    std::fs::create_dir_all(mnt_path.join("EFI/Boot"))?;
                    std::fs::create_dir_all(mnt_path.join("EFI/Microsoft/Boot"))?;
                    std::fs::copy(&efi, mnt_path.join("EFI/Boot/bootx64.efi"))?;
                    std::fs::write(mnt_path.join("EFI/Microsoft/Boot/BCD"), &bcd_efi)?;
                }
                progress(Progress {
                    phase: "wtg-boot".into(),
                    done: 1,
                    total: 2,
                    message: "BIOS + UEFI boot files written".into(),
                });
            }

            // WUE: unattend.xml goes to \Windows\Panther for Windows To Go
            if let Some(wue) = self.options.wue {
                if wue.bypass_tpm {
                    tracing::info!(
                        "TPM/SecureBoot bypass does not apply to Windows To Go — ignored"
                    );
                }
                self.apply_wtg_unattend(&mnt_path, &wue, progress)?;
            }
            Ok(())
        })();

        let _ = unmount(&mnt);
        let _ = std::fs::remove_dir_all(&mnt);
        if esp_mounted {
            let _ = unmount(&esp_mnt);
            let _ = std::fs::remove_dir_all(&esp_mnt);
            flush_partition(&crate::device::partition_node(&dev.path, 1));
        }
        flush_partition(&prep.part_dev);
        result
    }

    /// WTG variant of the unattend customization: Rufus's
    /// ApplyWindowsCustomization(UNATTEND_WINDOWS_TO_GO) copies the XML to
    /// X:\Windows\Panther\unattend.xml (picked up by Windows at first boot).
    fn apply_wtg_unattend(
        &self,
        mnt: &Path,
        wue: &WueOptions,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let username = std::env::var("USER").unwrap_or_else(|_| "user".into());
        let locale = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into());
        let locale = locale
            .split('.')
            .next()
            .unwrap_or("en_US")
            .replace('_', "-");
        let tz = std::fs::read_to_string("/etc/timezone")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "UTC".into());
        let xml = build_unattend_xml(wue, &username, &locale, &tz);
        let panther = mnt.join("Windows").join("Panther");
        std::fs::create_dir_all(&panther)?;
        std::fs::write(panther.join("unattend.xml"), xml)?;
        progress(Progress {
            phase: "wue".into(),
            done: 2,
            total: 2,
            message: "Windows unattend.xml written".into(),
        });
        Ok(())
    }

    /// Apply the "Windows User Experience" customizations on the mounted
    /// stick: drop the unattend.xml into sources/$oem$/$$/Panther and, when
    /// the TPM/RAM/SecureBoot bypass is requested, inject the LabConfig
    /// registry keys into sources/boot.wim (needs wimlib-imagex + chntpw).
    fn apply_windows_user_experience(
        &self,
        mnt: &Path,
        wue: &WueOptions,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let username = std::env::var("USER").unwrap_or_else(|_| "user".into());
        let locale = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".into());
        let locale = locale
            .split('.')
            .next()
            .unwrap_or("en_US")
            .replace('_', "-");
        let tz = std::fs::read_to_string("/etc/timezone")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "UTC".into());
        let xml = build_unattend_xml(wue, &username, &locale, &tz);
        let panther = mnt.join("sources").join("$oem$").join("$$").join("Panther");
        std::fs::create_dir_all(&panther)?;
        std::fs::write(panther.join("unattend.xml"), xml)?;
        progress(Progress {
            phase: "wue".into(),
            done: 1,
            total: 2,
            message: "Windows unattend.xml written".into(),
        });
        if wue.bypass_tpm {
            self.patch_boot_wim_labconfig(mnt, progress)?;
        }
        Ok(())
    }

    /// Inject HKLM\SYSTEM\Setup\LabConfig bypass keys into sources/boot.wim.
    /// Requires wimlib-imagex (WIM edit) and chntpw (offline registry edit).
    fn patch_boot_wim_labconfig(
        &self,
        mnt: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let boot_wim = mnt.join("sources").join("boot.wim");
        if !boot_wim.is_file() {
            progress(Progress {
                phase: "wue".into(),
                done: 2,
                total: 2,
                message: "WARNING: boot.wim not found — TPM bypass skipped".into(),
            });
            return Ok(());
        }
        let have = |tool: &str| {
            Command::new("sh")
                .args(["-c", &format!("command -v {tool} >/dev/null 2>&1")])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !have("wimlib-imagex") || !have("chntpw") {
            progress(Progress {
                phase: "wue".into(),
                done: 2,
                total: 2,
                message: "WARNING: TPM bypass needs wimlib-imagex + chntpw — install them".into(),
            });
            tracing::warn!("TPM/SecureBoot bypass skipped: wimlib-imagex and/or chntpw missing");
            return Ok(());
        }
        progress(Progress {
            phase: "wue".into(),
            done: 2,
            total: 2,
            message: "Patching boot.wim LabConfig bypass keys...".into(),
        });
        let tmp = std::env::temp_dir().join(format!("rufus-wim-{}", std::process::id()));
        std::fs::create_dir_all(&tmp)?;
        let boot_wim_s = boot_wim.to_string_lossy().to_string();
        let tmp_s = tmp.to_string_lossy().to_string();
        let _ = Command::new("wimlib-imagex")
            .args([
                "extract",
                &boot_wim_s,
                "2",
                "/Windows/System32/config/SYSTEM",
                "--dest-dir",
                &tmp_s,
            ])
            .status()?;
        // chntpw: add HKLM\SYSTEM\Setup\LabConfig bypass DWORDs
        let script = "cd Microsoft\\Windows\\CurrentVersion\\Setup\nnk LabConfig\ncd LabConfig\nnv 3 v BypassTPMCheck\ned BypassTPMCheck\nd:0x00000001\nnv 3 v BypassRAMCheck\ned BypassRAMCheck\nd:0x00000001\nnv 3 v BypassSecureBootCheck\ned BypassSecureBootCheck\nd:0x00000001\nq\ny\n";
        let out = Command::new("chntpw")
            .arg(tmp.join("SYSTEM"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write as _;
                c.stdin.as_mut().unwrap().write_all(script.as_bytes())?;
                c.wait_with_output()
            });
        match out {
            Ok(o) if o.status.success() => {
                progress(Progress {
                    phase: "wue".into(),
                    done: 2,
                    total: 2,
                    message: "LabConfig bypass keys injected".into(),
                });
            }
            Ok(o) => {
                tracing::warn!(
                    "chntpw returned {} — TPM bypass may not have applied",
                    o.status
                );
            }
            Err(e) => {
                tracing::warn!("chntpw failed: {e}");
            }
        }
        let _ = Command::new("wimlib-imagex")
            .args([
                "update",
                &boot_wim_s,
                "2",
                "--delete=/Windows/System32/config/SYSTEM",
                &format!("--add={tmp_s}/SYSTEM=/Windows/System32/config/SYSTEM"),
            ])
            .status()?;
        let _ = std::fs::remove_dir_all(&tmp);
        Ok(())
    }

    fn handle_install_wim(&self, mnt: &Path, progress: &mut dyn FnMut(Progress)) -> Result<()> {
        self.check_cancel()?;
        // Windows ISOs: sources/install.wim or install.esd > 4 GiB must be split
        // for FAT32 (NTFS/exFAT have no 4 GiB file-size limit).
        if self.options.fs != FsKind::Fat32 {
            return Ok(());
        }
        for name in [
            "sources/install.wim",
            "sources/install.esd",
            "Sources/Install.wim",
            "Sources/Install.esd",
        ] {
            let p = mnt.join(name);
            if !p.exists() {
                continue;
            }
            let size = std::fs::metadata(&p)?.len();
            let limit = 4u64 << 30; // 4 GiB safe limit
            if size <= limit {
                return Ok(());
            }
            progress(Progress {
                phase: "wim".into(),
                done: 0,
                total: size,
                message: format!("Splitting {name} for FAT32..."),
            });
            // wimlib-imagex split: install.wim -> install.swm, install2.swm, ...
            // (the names Windows setup looks for)
            let out_pattern = p.with_extension("swm");
            let status = Command::new("wimlib-imagex")
                .arg("split")
                .arg(&p)
                .arg(&out_pattern)
                .arg("4000") // MiB chunk
                .status();
            match status {
                Ok(s) if s.success() => {
                    let _ = std::fs::remove_file(&p);
                    progress(Progress {
                        phase: "wim".into(),
                        done: size,
                        total: size,
                        message: "WIM split complete".into(),
                    });
                }
                _ => {
                    tracing::warn!("wimlib-imagex split failed or not installed; install.wim >4GiB won't fit FAT32");
                }
            }
            return Ok(());
        }
        Ok(())
    }

    fn install_boot(
        &self,
        mnt: &Path,
        iso: &IsoImage,
        syslinux_dir: bool,
        target: &BootTarget<'_>,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        // UEFI: ensure /EFI/BOOT/BOOTX64.EFI exists; if ISO only has bootia32 etc., note.
        let efi_boot = mnt.join("EFI/BOOT");
        if efi_boot.exists() {
            progress(Progress {
                phase: "boot".into(),
                done: 1,
                total: 1,
                message: "UEFI boot files present".into(),
            });
        } else {
            progress(Progress {
                phase: "boot".into(),
                done: 0,
                total: 1,
                message: "No UEFI boot files in image".into(),
            });
        }
        // BIOS boot: prefer GRUB2 when ISO ships grub.cfg+core.img, else syslinux.
        // BIOS int13 cannot read 4096-byte sectors — skip on 4Kn devices.
        let has_grub = iso
            .entries
            .iter()
            .any(|e| e.path.eq_ignore_ascii_case("boot/grub/grub.cfg"));
        if target.sector_size != 512 && !self.options.efi_only {
            progress(Progress {
                phase: "boot".into(),
                done: 0,
                total: 1,
                message: format!(
                    "BIOS boot skipped ({}-byte sectors) — UEFI only",
                    target.sector_size
                ),
            });
        } else if !self.options.efi_only {
            if has_grub {
                self.install_grub2(mnt, target.part_dev, target.part_start_lba, progress)?;
            } else if syslinux_dir {
                self.install_syslinux(mnt, target.part_dev, target.part_start_lba, progress)?;
            } else {
                progress(Progress {
                    phase: "boot".into(),
                    done: 0,
                    total: 1,
                    message:
                        "No syslinux/grub files in image; BIOS boot may not work (UEFI still OK)"
                            .into(),
                });
            }
        }
        Ok(())
    }

    /// Install GRUB2 i386-pc boot chain (BIOS), grub-bios-setup parity:
    /// keep core.img as a file on the FAT volume, locate its on-disk sectors
    /// with a FAT walk, patch the diskboot blocklist table inside the file,
    /// and write boot.img (carrying the real BPB) to the partition boot
    /// sector. The old implementation wrote boot.img+core.img raw at
    /// partition sectors 0..86, destroying FSInfo, the backup boot sector
    /// and the start of the first FAT — the "corrupted flash" bug.
    fn install_grub2(
        &self,
        mnt: &Path,
        part_dev: &str,
        part_start_lba: u64,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        const CORE_REL: &str = "boot/grub/i386-pc/core.img";
        let core_path = mnt.join(CORE_REL);
        // prefer the ISO's own core.img (version-matched to its grub.cfg);
        // fall back to our asset when the image ships none
        if !core_path.is_file() {
            if let Some(dir) = core_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&core_path, GRUB2_CORE).context("failed to write core.img")?;
        }
        let core_sectors = std::fs::metadata(&core_path)?.len().div_ceil(512);
        flush_file_tree(&core_path, mnt);
        // the volume's FAT + directory data must be on disk before the raw
        // partition walk
        // SAFETY: libc::sync() flushes all filesystems; no memory unsafety.
        unsafe { libc::sync() };

        // 1. locate core.img's sectors on the volume
        let mut lbas = map_volume_file(part_dev, CORE_REL)?;
        if lbas.len() as u64 != core_sectors {
            bail!(
                "core.img sector map mismatch ({} != {core_sectors})",
                lbas.len()
            );
        }

        // 2. patch the diskboot blocklists inside the file (same-size
        //    rewrite so the clusters cannot move)
        let mut core = std::fs::read(&core_path)?;
        bootpatch::patch_core_blocklists(&mut core, &lbas, part_start_lba)?;
        rewrite_same_size(&core_path, &core)?;

        // 3. the rewrite must not have moved the file
        let lbas2 = map_volume_file(part_dev, CORE_REL)?;
        if lbas2 != lbas {
            bail!("core.img clusters moved while patching — filesystem changed underneath us");
        }
        lbas = lbas2;

        // 4. boot sector: copy the formatted VBR's BPB into boot.img and
        //    point kernel_sector at the file's first (disk-absolute) sector
        let mut vbr = [0u8; 512];
        {
            let mut part = crate::disk::open_partition(part_dev)?;
            part.read_exact(&mut vbr)?;
        }
        let mut boot = GRUB2_BOOT.to_vec();
        bootpatch::patch_grub_boot(&mut boot, &vbr, part_start_lba + lbas[0])?;
        write_partition_sector(part_dev, 0, &boot)?;
        progress(Progress {
            phase: "boot".into(),
            done: 1,
            total: 1,
            message: "GRUB2 installed (BIOS boot)".into(),
        });
        Ok(())
    }

    /// Install syslinux boot chain natively (Rufus InstallSyslinux parity):
    /// 1. write ldlinux.sys (loader image zero-padded to whole sectors + a
    ///    2-sector ADV) to the filesystem root
    /// 2. ensure a syslinux.cfg at root pointing at the ISO's config
    /// 3. flush, then locate the file's on-disk sectors with a FAT walk
    /// 4. patch the loader in place (patch area, extents, ADV, checksum)
    /// 5. assemble the VBR from the .bss template + the formatted BPB with
    ///    the ldlinux first-sector pointer, and write it to sector 0
    fn install_syslinux(
        &self,
        mnt: &Path,
        part_dev: &str,
        part_start_lba: u64,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        // syslinux 6.04 pair; the v4 assets are kept for a future fallback
        let ldlinux = bootpatch::build_ldlinux_file(LDLINUX_V6_SYS);
        std::fs::write(mnt.join("ldlinux.sys"), &ldlinux).context("failed to write ldlinux.sys")?;

        // config: find ISO's syslinux/isolinux cfg and mirror to root
        let cfg_candidates = [
            "syslinux.cfg",
            "isolinux.cfg",
            "syslinux/syslinux.cfg",
            "isolinux/isolinux.cfg",
            "boot/syslinux/syslinux.cfg",
            "boot/isolinux/isolinux.cfg",
        ];
        let mut cfg_copied = false;
        for cand in cfg_candidates {
            let src = mnt.join(cand);
            if src.is_file() {
                if cand != "syslinux.cfg" {
                    // copy to root as syslinux.cfg
                    if let Err(e) = std::fs::copy(&src, mnt.join("syslinux.cfg")) {
                        tracing::warn!("cfg copy failed: {e}");
                    } else {
                        cfg_copied = true;
                    }
                } else {
                    cfg_copied = true;
                }
                break;
            }
        }
        if !cfg_copied {
            // minimal default config so ldlinux has something to read
            std::fs::write(
                mnt.join("syslinux.cfg"),
                "DEFAULT menu.c32
PROMPT 0
TIMEOUT 50
",
            )?;
        }

        // flush + locate ldlinux.sys's sectors on the volume
        let ld_path = mnt.join("ldlinux.sys");
        flush_file_tree(&ld_path, mnt);
        // SAFETY: libc::sync() flushes all filesystems; no memory unsafety.
        unsafe { libc::sync() };
        let nsect = ldlinux.len() / 512;
        let lbas = map_volume_file(part_dev, "ldlinux.sys")?;
        if lbas.len() != nsect {
            bail!(
                "ldlinux.sys sector map mismatch ({} != {nsect})",
                lbas.len()
            );
        }

        // patch the loader in place (same-size rewrite keeps the clusters)
        let mut buf = std::fs::read(&ld_path)?;
        let patch = bootpatch::patch_ldlinux(&mut buf, LDLINUX_V6_SYS.len(), &lbas)?;
        rewrite_same_size(&ld_path, &buf)?;
        // the rewrite must not have moved the file
        let lbas2 = map_volume_file(part_dev, "ldlinux.sys")?;
        if lbas2 != lbas {
            bail!("ldlinux.sys clusters moved while patching — filesystem changed underneath us");
        }

        // VBR: bss jump/OEM + formatted BPB + bss loader code, with the
        // ldlinux first-sector pointer (volume-relative; the loader adds the
        // BPB hidden-sectors field at boot)
        let mut cur = [0u8; 512];
        {
            let mut part = crate::disk::open_partition(part_dev)?;
            part.read_exact(&mut cur)?;
        }
        let vbr = bootpatch::assemble_syslinux_vbr(
            LDLINUX_V6_BSS,
            &cur,
            lbas[0],
            part_start_lba as u32,
            patch.sect1ptr0,
            patch.sect1ptr1,
        )?;
        write_partition_sector(part_dev, 0, &vbr)?;
        progress(Progress {
            phase: "boot".into(),
            done: 1,
            total: 1,
            message: "syslinux VBR installed (BIOS boot)".into(),
        });
        Ok(())
    }

    /// Rufus "Check device for bad blocks" — destructive write test.
    /// Faithful port of test_rw()/BadBlocks() from the original badblocks.c
    /// (e2fsprogs-derived): every pass writes a test pattern across the whole
    /// device in 512 KiB blocks (DESTROYING all data), then reads it back and
    /// compares block by block to detect unreliable media. Aborts after
    /// BB_THRESHOLD bad blocks, like the original.
    fn badblocks_test(
        &self,
        io: &mut dyn BlockIo,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        let mode = self.options.bb_test;
        let passes = mode.passes();
        let patterns = mode.patterns();
        let last_block = io.size() / BB_BLOCK_SIZE;
        if last_block == 0 {
            bail!("device is too small for the bad blocks test (needs >= 512 KiB)");
        }
        let total = (passes * 1000) as u64;
        progress(Progress {
            phase: "badblocks".into(),
            done: 0,
            total,
            message: format!(
                "Bad Blocks: Checking from block 0 to {} (1 block = 512 KB)",
                last_block - 1
            ),
        });

        // sorted, unique bad blocks list (bb_list in the original)
        let mut bb_list: Vec<u64> = Vec::new();
        let (mut num_read, mut num_write, mut num_corrupt) = (0u32, 0u32, 0u32);
        let chunk = BB_BLOCKS_AT_ONCE;
        let blk_sz = BB_BLOCK_SIZE as usize;
        let mut write_buf = vec![0u8; (chunk * BB_BLOCK_SIZE) as usize];
        let mut read_buf = vec![0u8; (chunk * BB_BLOCK_SIZE) as usize];

        for (pass, &pattern) in patterns.iter().flat_map(|p| p.iter()).enumerate() {
            let base = (pass * 1000) as u64;
            // -------- write pass: fill the media with the test pattern --------
            fill_pattern(&mut write_buf, pattern);
            progress(Progress {
                phase: "badblocks".into(),
                done: base,
                total,
                message: format!("Bad Blocks: Writing test pattern 0x{pattern:02X}"),
            });
            let mut blk = 0u64;
            while blk < last_block {
                self.check_cancel()?;
                let tryout = (last_block - blk).min(chunk);
                let n = (tryout * BB_BLOCK_SIZE) as usize;
                if io.write_at(blk * BB_BLOCK_SIZE, &write_buf[..n]).is_err() {
                    // chunked write failed — retry block by block to locate the
                    // bad ones (do_write() degrade logic in the original)
                    for b in blk..blk + tryout {
                        // bb_output() dedupes: a block already in the list is
                        // not counted again
                        if io
                            .write_at(b * BB_BLOCK_SIZE, &write_buf[..blk_sz])
                            .is_err()
                            && record_bad_block(&mut bb_list, b)
                        {
                            num_write += 1;
                        }
                    }
                }
                blk += tryout;
                if bb_list.len() >= BB_THRESHOLD {
                    bail!(
                        "Too many bad blocks, aborting test ({} bad blocks found)",
                        bb_list.len()
                    );
                }
                progress(Progress {
                    phase: "badblocks".into(),
                    done: base + 500 * blk / last_block,
                    total,
                    message: format!(
                        "Bad Blocks: Write pass {}/{} - {:.2}% ({}/{}/{} errors)",
                        pass + 1,
                        passes,
                        100.0 * blk as f64 / last_block as f64,
                        num_read,
                        num_write,
                        num_corrupt
                    ),
                });
            }

            // -------- read pass: read back and compare --------
            progress(Progress {
                phase: "badblocks".into(),
                done: base + 500,
                total,
                message: "Bad Blocks: Reading and comparing".into(),
            });
            let mut blk = 0u64;
            while blk < last_block {
                self.check_cancel()?;
                let tryout = (last_block - blk).min(chunk);
                let n = (tryout * BB_BLOCK_SIZE) as usize;
                if io.read_at(blk * BB_BLOCK_SIZE, &mut read_buf[..n]).is_err() {
                    for b in blk..blk + tryout {
                        if io
                            .read_at(b * BB_BLOCK_SIZE, &mut read_buf[..blk_sz])
                            .is_err()
                        {
                            if record_bad_block(&mut bb_list, b) {
                                num_read += 1;
                            }
                        } else if read_buf[..blk_sz] != write_buf[..blk_sz]
                            && record_bad_block(&mut bb_list, b)
                        {
                            num_corrupt += 1;
                        }
                    }
                } else {
                    for i in 0..tryout {
                        let s = (i * BB_BLOCK_SIZE) as usize;
                        // blocks that already failed (e.g. write errors) are
                        // deduped by bb_output() and never counted twice
                        if read_buf[s..s + blk_sz] != write_buf[s..s + blk_sz]
                            && record_bad_block(&mut bb_list, blk + i)
                        {
                            num_corrupt += 1;
                        }
                    }
                }
                blk += tryout;
                if bb_list.len() >= BB_THRESHOLD {
                    bail!(
                        "Too many bad blocks, aborting test ({} bad blocks found)",
                        bb_list.len()
                    );
                }
                progress(Progress {
                    phase: "badblocks".into(),
                    done: base + 500 + 500 * blk / last_block,
                    total,
                    message: format!(
                        "Bad Blocks: Read pass {}/{} - {:.2}% ({}/{}/{} errors)",
                        pass + 1,
                        passes,
                        100.0 * blk as f64 / last_block as f64,
                        num_read,
                        num_write,
                        num_corrupt
                    ),
                });
            }
        }

        progress(Progress {
            phase: "badblocks".into(),
            done: total,
            total,
            message: format!(
                "Bad Blocks: Check completed, {} bad block(s) found. ({}/{}/{} errors)",
                bb_list.len(),
                num_read,
                num_write,
                num_corrupt
            ),
        });
        if !bb_list.is_empty() {
            for &b in bb_list.iter().take(50) {
                tracing::warn!("Bad Blocks: block {b}");
            }
            if bb_list.len() > 50 {
                tracing::warn!("Bad Blocks: ... and {} more", bb_list.len() - 50);
            }
            bail!(
                "Check completed: {} bad block(s) found ({} read, {} write, {} corruption errors) — device unreliable",
                bb_list.len(), num_read, num_write, num_corrupt
            );
        }
        Ok(())
    }
}

/// fsync one path (file or directory), best effort.
fn fsync_path(p: &Path) {
    if let Ok(f) = std::fs::File::open(p) {
        if let Err(e) = f.sync_all() {
            tracing::debug!("fsync of {} failed: {e}", p.display());
        }
    }
}

/// fsync `p` and every ancestor up to and including `mnt` — makes a freshly
/// written file durable (data + FAT chain + directory entries) before the
/// volume is read raw for the boot-loader patches.
fn flush_file_tree(p: &Path, mnt: &Path) {
    let mut cur = Some(p);
    while let Some(c) = cur {
        fsync_path(c);
        if c == mnt {
            break;
        }
        cur = c.parent();
    }
}

/// Locate `path` on a freshly written FAT volume by walking the raw
/// partition device (BPB -> directories -> cluster chain); returns the
/// file's volume-relative 512-byte LBAs.
fn map_volume_file(part_dev: &str, rel_path: &str) -> Result<Vec<u64>> {
    let mut part = crate::disk::open_partition(part_dev)?;
    let vol = fat32::FatVolume::open(&mut part)?;
    let e = vol
        .find(&mut part, rel_path)?
        .with_context(|| format!("{rel_path} not found on the target volume"))?;
    Ok(vol.file_lbas(&mut part, e.first_cluster, e.size)?)
}

/// Rewrite `path` with `data` at the exact same length, without truncating —
/// same size means the FAT clusters cannot move (the Rufus-installer trick).
fn rewrite_same_size(path: &Path, data: &[u8]) -> Result<()> {
    let old = std::fs::metadata(path)?.len();
    if data.len() as u64 != old {
        bail!(
            "rewrite of {} would change its size ({} -> {})",
            path.display(),
            old,
            data.len()
        );
    }
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

/// Write one 512-byte sector at LBA `sector` of the partition (boot sectors).
fn write_partition_sector(part_dev: &str, sector: u64, buf: &[u8]) -> Result<()> {
    if buf.len() != 512 {
        bail!(
            "write_partition_sector: expected 512 bytes, got {}",
            buf.len()
        );
    }
    let mut part = crate::disk::open_partition(part_dev)?;
    part.seek(SeekFrom::Start(sector * 512))?;
    part.write_all(buf)?;
    part.sync_all()?;
    Ok(())
}

/// fsync a partition node (best effort) — final flush after unmounting, so
/// nothing is left dirty when "Done!" is reported.
fn flush_partition(part_dev: &str) {
    if let Ok(f) = crate::disk::open_partition(part_dev) {
        if let Err(e) = f.sync_all() {
            tracing::debug!("fsync of {part_dev} failed: {e}");
        }
    }
}

/// Detach any mounted filesystems on partitions of `dev` (e.g. /dev/sdb1).
fn unmount_device_partitions(dev: &str) {
    detach_mounts(dev);
}

const MOUNT_DATA: &str = "uid=0,gid=0,umask=022";

fn mount_fs(dev: &str, mnt: &str, fs: FsKind) -> Result<()> {
    use nix::mount::{mount, MsFlags};
    let fstype = match fs {
        FsKind::Fat32 => "vfat",
        FsKind::Ntfs => "ntfs3",
        FsKind::Exfat => "exfat",
    };
    let flags = MsFlags::MS_NODEV | MsFlags::MS_NOEXEC | MsFlags::MS_NOSUID;
    for attempt in 0..2 {
        let err = match mount(Some(dev), mnt, Some(fstype), flags, Some(MOUNT_DATA)) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if attempt == 0 && err == nix::errno::Errno::EBUSY {
            // a desktop automounter grabbed the freshly formatted partition —
            // detach it and retry once
            tracing::warn!("mount of {dev} busy — detaching automounts and retrying");
            detach_mounts(dev);
            std::thread::sleep(std::time::Duration::from_millis(400));
            continue;
        }
        // fallback for NTFS via ntfs-3g FUSE
        if matches!(fs, FsKind::Ntfs) {
            let st = Command::new("ntfs-3g")
                .args(["-o", MOUNT_DATA, dev, mnt])
                .status();
            if st.map(|s| s.success()).unwrap_or(false) {
                return Ok(());
            }
        }
        bail!("mount {dev} as {fstype} failed: {err}");
    }
    unreachable!("mount loop always returns or bails")
}

/// Detach any mounted filesystem whose source is `prefix` or a partition of it.
fn detach_mounts(prefix: &str) {
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            let src = line.split_whitespace().next().unwrap_or("");
            let is_ours = src == prefix
                || (src.starts_with(prefix)
                    && src[prefix.len()..].starts_with(|c: char| c.is_ascii_digit() || c == 'p'));
            if is_ours {
                // a real unmount flushes; keep the lazy detach as fallback
                let ok = Command::new("umount")
                    .arg(src)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    let _ = Command::new("umount").arg("-l").arg(src).status();
                }
            }
        }
    }
}

fn unmount(mnt: &str) -> Result<()> {
    use nix::mount::{umount, umount2, MntFlags};
    // A real (non-lazy) unmount syncs the filesystem before returning.
    // MNT_DETACH returns immediately while the writeback still runs in the
    // background — reporting "Done!" on top of that risks the user yanking
    // a half-flushed stick, so lazy detach is only the busy-volume fallback.
    if umount(mnt).is_ok() {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    tracing::warn!("unmount of {mnt} failed ({e}) — detaching lazily");
    if let Err(e2) = umount2(mnt, MntFlags::MNT_DETACH) {
        let _ = Command::new("umount").arg("-l").arg(mnt).status();
        return Err(e2.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout helper with Rufus-typical geometry (63 sectors/track).
    fn compute(
        disk_sectors: u64,
        uefi_ntfs_sectors: u64,
        persist_sectors: u64,
        compat_sectors: u64,
        gpt: bool,
    ) -> crate::error::Result<Layout> {
        compute_layout(
            disk_sectors,
            LayoutReq {
                part_start_lba: 2048,
                uefi_ntfs_sectors,
                persist_sectors,
                compat_sectors,
                track_sectors: 63,
                gpt,
            },
        )
    }

    #[test]
    fn layout_plain_main_partition() {
        // 8 GiB disk, no extras: main ends 1 MiB before the disk end
        let l = compute(16_777_216, 0, 0, 0, false).unwrap();
        assert_eq!(l.part_end_lba, 16_777_216 - 2048);
        assert_eq!(l.uefi_ntfs_start, 0);
        assert_eq!(l.persist_start, 0);
        assert_eq!(l.persist_len, 0);
        assert_eq!(l.compat_start, 0);
    }

    #[test]
    fn layout_uefi_ntfs_only() {
        // uefi-ntfs image = 1 MiB = 2048 sectors at the tail
        let l = compute(16_777_216, 2048, 0, 0, false).unwrap();
        assert_eq!(l.uefi_ntfs_start, 16_777_216 - 2048 - 2048);
        assert_eq!(l.part_end_lba, l.uefi_ntfs_start - 1);
        assert_eq!(l.persist_len, 0);
    }

    #[test]
    fn layout_persist_only_no_underflow() {
        // regression: persist without UEFI:NTFS used to underflow (0 - 1)
        let l = compute(16_777_216, 0, 262_144, 0, false).unwrap();
        assert_eq!(l.persist_start + l.persist_len, 16_777_216 - 2048);
        assert_eq!(l.persist_len, 262_144);
        assert_eq!(l.part_end_lba, l.persist_start - 1);
        assert!(l.part_end_lba > 2048);
    }

    #[test]
    fn layout_persist_and_uefi_ntfs() {
        let l = compute(16_777_216, 2048, 262_144, 0, false).unwrap();
        // from the end backwards: [uefi-ntfs][persist][main]
        assert_eq!(l.uefi_ntfs_start, 16_777_216 - 2048 - 2048 - 262_144);
        assert_eq!(l.persist_start + l.persist_len, l.uefi_ntfs_start);
        assert_eq!(l.persist_len, 262_144);
        assert_eq!(l.part_end_lba, l.persist_start - 1);
        // all three regions tile the disk without overlap
        assert!(l.part_end_lba < l.persist_start);
        assert!(l.persist_start + l.persist_len <= l.uefi_ntfs_start);
    }

    #[test]
    fn layout_too_small_device_bails() {
        assert!(compute(4096, 0, 0, 0, false).is_err());
        // 4 MiB disk cannot hold a 512 MiB persistence partition
        assert!(compute(8192, 0, 262_144, 0, false).is_err());
    }

    #[test]
    fn layout_compat_partition_at_disk_end_mbr() {
        // Rufus XP_COMPAT: one 63-sector track at the very end, floor-aligned
        // to a track boundary, main partition shrunk right below it
        let l = compute(16_777_216, 0, 0, 63, false).unwrap();
        assert_eq!(l.compat_start, (16_777_216 - 63) / 63 * 63);
        assert_eq!(l.compat_start % 63, 0, "track-aligned");
        // the partition never reaches past the end of the disk (≤ 1 track slack)
        assert!(l.compat_start + 63 <= 16_777_216);
        assert_eq!(l.part_end_lba, l.compat_start - 1);
        assert!(l.part_end_lba > 2048);
    }

    #[test]
    fn layout_compat_partition_gpt_keeps_backup_area() {
        // GPT: the 33-sector backup header/entries area stays above the compat
        // partition, like Rufus's last_offset -= 33 sectors
        let l = compute(16_777_216, 0, 0, 63, true).unwrap();
        let usable_end = 16_777_216 - 33;
        assert_eq!(l.compat_start, (usable_end - 63) / 63 * 63);
        assert!(l.compat_start + 63 <= usable_end);
        assert_eq!(l.part_end_lba, l.compat_start - 1);
    }

    #[test]
    fn layout_compat_with_persistence_and_uefi_ntfs() {
        // from the end backwards: [compat][UEFI:NTFS][persistence][main]
        let l = compute(16_777_216, 2048, 262_144, 63, false).unwrap();
        assert_eq!(l.compat_start % 63, 0);
        assert!(l.compat_start + 63 <= 16_777_216);
        // persistence and UEFI:NTFS stack up right below the compat partition
        assert_eq!(l.persist_start + l.persist_len, l.uefi_ntfs_start);
        assert!(l.uefi_ntfs_start < l.compat_start);
        assert_eq!(l.part_end_lba, l.persist_start - 1);
    }

    #[test]
    fn layout_compat_with_uefi_ntfs_only() {
        // NTFS + old-BIOS fixes: UEFI:NTFS ends right below the compat partition
        let l = compute(16_777_216, 2048, 0, 63, false).unwrap();
        assert_eq!(l.uefi_ntfs_start, (l.compat_start - 2048) / 2048 * 2048);
        assert_eq!(l.part_end_lba, l.uefi_ntfs_start - 1);
    }

    #[test]
    fn detect_image_kind_by_extension_and_magic() {
        let dir = std::env::temp_dir();
        let iso_path = dir.join(format!("rufus-test-{}.iso", std::process::id()));
        let img_path = dir.join(format!("rufus-test-{}.img", std::process::id()));
        let raw_path = dir.join(format!("rufus-test-raw-{}.img", std::process::id()));
        let mut iso_data = vec![0u8; 17 * 2048];
        iso_data[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
        std::fs::write(&iso_path, &iso_data).unwrap();
        std::fs::write(&img_path, &iso_data).unwrap();
        std::fs::write(&raw_path, vec![0u8; 17 * 2048]).unwrap();
        assert_eq!(detect_image_kind(&iso_path), ImageKind::Iso);
        assert_eq!(detect_image_kind(&img_path), ImageKind::Iso); // magic peek
        assert_eq!(detect_image_kind(&raw_path), ImageKind::Raw); // no magic
        let _ = std::fs::remove_file(&iso_path);
        let _ = std::fs::remove_file(&img_path);
        let _ = std::fs::remove_file(&raw_path);
    }

    #[test]
    fn unattend_xml_flags() {
        let all = WueOptions {
            bypass_tpm: true,
            no_online_account: true,
            disable_data_collection: true,
            local_account: true,
            regional_same_as_user: true,
            offline_internal_drives: true,
        };
        let xml = build_unattend_xml(&all, "testuser", "en-US", "Africa/Cairo");
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<HideOnlineAccountScreens>true</HideOnlineAccountScreens>"));
        assert!(xml.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(xml.contains("<Name>testuser</Name>"));
        assert!(xml.contains("<InputLocale>en-US</InputLocale>"));
        assert!(xml.contains("<TimeZone>Africa/Cairo</TimeZone>"));
        // offline internal disks (Rufus MSG_332): offlineServicing SanPolicy 4
        assert!(xml.contains("<settings pass=\"offlineServicing\">"));
        assert!(xml.contains("<SanPolicy>4</SanPolicy>"));
        // nothing enabled -> no OOBE flags, no account, no SanPolicy
        let none = WueOptions {
            bypass_tpm: false,
            no_online_account: false,
            disable_data_collection: false,
            local_account: false,
            regional_same_as_user: false,
            offline_internal_drives: false,
        };
        let xml = build_unattend_xml(&none, "user", "en-US", "UTC");
        assert!(!xml.contains("HideOnlineAccountScreens"));
        assert!(!xml.contains("UserAccounts"));
        assert!(!xml.contains("SanPolicy"));
    }

    #[test]
    fn badblocks_tables_match_rufus() {
        // Rufus's "Number of passes" combobox: sel -> (passes, pattern set)
        assert_eq!(BadBlocksTest::from_sel(0).passes(), 1);
        assert_eq!(BadBlocksTest::from_sel(0).patterns(), &[&[0x55]]);
        assert_eq!(BadBlocksTest::from_sel(1).passes(), 2);
        assert_eq!(BadBlocksTest::from_sel(1).patterns(), &[&[0x55], &[0xaa]]);
        for sel in 2..=4 {
            assert_eq!(BadBlocksTest::from_sel(sel).passes(), 4);
        }
        assert_eq!(
            BadBlocksTest::from_sel(2).patterns(),
            &[&[0x00, 0xff, 0x55, 0xaa]]
        );
        assert_eq!(
            BadBlocksTest::from_sel(3).patterns(),
            &[&[0x00, 0xff, 0x33, 0xcc]]
        );
        assert_eq!(
            BadBlocksTest::from_sel(4).patterns(),
            &[&[0x00, 0xff, 0x1c71c7, 0xe38e38]]
        );
        assert_eq!(BB_PASSES_NAMES.len(), 5);
        assert_eq!(BB_PASSES_NAMES[0], "1 pass");
        assert_eq!(BB_PASSES_NAMES[4], "4 passes for TLC NAND");
    }

    #[test]
    fn fill_pattern_repeats_pattern_bytes() {
        let mut b = vec![0u8; 1000];
        // single-byte patterns fill every byte
        fill_pattern(&mut b, 0x55);
        assert!(b.iter().all(|&x| x == 0x55));
        fill_pattern(&mut b, 0x00);
        assert!(b.iter().all(|&x| x == 0));
        fill_pattern(&mut b, 0xff);
        assert!(b.iter().all(|&x| x == 0xff));
        // multi-byte patterns repeat big-endian, like the original pattern_fill()
        fill_pattern(&mut b, 0x1c71c7);
        assert_eq!(&b[..6], &[0x1c, 0x71, 0xc7, 0x1c, 0x71, 0xc7]);
        fill_pattern(&mut b, 0xe38e38);
        assert_eq!(&b[..6], &[0xe3, 0x8e, 0x38, 0xe3, 0x8e, 0x38]);
        // random pattern just fills something
        fill_pattern(&mut b, u32::MAX);
    }

    #[test]
    fn record_bad_block_is_sorted_and_unique() {
        let mut l = Vec::new();
        assert!(record_bad_block(&mut l, 5));
        assert!(record_bad_block(&mut l, 1));
        assert!(!record_bad_block(&mut l, 5));
        assert!(record_bad_block(&mut l, 3));
        assert_eq!(l, vec![1, 3, 5]);
    }

    // ---------- Windows To Go ----------

    fn empty_iso(entries: Vec<iso9660::IsoEntry>) -> IsoImage {
        IsoImage {
            volume_id: "TEST".into(),
            logical_block_size: 2048,
            path_table_lba: 0,
            entries,
            boot_catalog_lba: None,
            boot_images: vec![],
        }
    }

    #[test]
    fn find_windows_wim_is_case_insensitive() {
        let iso = empty_iso(vec![
            iso9660::IsoEntry {
                path: "boot/bcd".into(),
                is_dir: false,
                extent_lba: 0,
                size: 0,
                mtime: 0,
            },
            iso9660::IsoEntry {
                path: "SOURCES/INSTALL.WIM".into(),
                is_dir: false,
                extent_lba: 0,
                size: 10,
                mtime: 0,
            },
        ]);
        let w = find_windows_wim(&iso).expect("wim must be found");
        assert_eq!(w.path, "SOURCES/INSTALL.WIM");
        assert!(
            find_windows_wim(&empty_iso(vec![iso9660::IsoEntry {
                path: "sources/setup.exe".into(),
                is_dir: false,
                extent_lba: 0,
                size: 1,
                mtime: 0
            },]))
            .is_none(),
            "no install.wim → None"
        );
    }

    #[test]
    fn parse_wim_xml_lists_editions() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?><WIM><IMAGE INDEX="2"><NAME>Windows 11 Pro</NAME><DESCRIPTION>d2</DESCRIPTION></IMAGE><IMAGE INDEX="1"><NAME>Windows 11 Home</NAME><DESCRIPTION>d1</DESCRIPTION></IMAGE></WIM>"#;
        let list = parse_wim_xml(xml);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].index, 1, "sorted by index");
        assert_eq!(list[0].name, "Windows 11 Home");
        assert_eq!(list[1].name, "Windows 11 Pro");
        // DISPLAYNAME wins over NAME (Rufus's fallback order)
        let l2 = parse_wim_xml(
            br#"<IMAGE INDEX="1"><NAME>N</NAME><DISPLAYNAME>Display N</DISPLAYNAME></IMAGE>"#,
        );
        assert_eq!(l2[0].name, "Display N");
        // UTF-16LE XML (some WIMs store the metadata that way)
        let mut u16xml = vec![0xFF, 0xFE];
        for c in "<IMAGE INDEX=\"3\"><NAME>X</NAME></IMAGE>".encode_utf16() {
            u16xml.extend_from_slice(&c.to_le_bytes());
        }
        let l3 = parse_wim_xml(&u16xml);
        assert_eq!(l3.len(), 1);
        assert_eq!(l3[0].index, 3);
        assert_eq!(l3[0].name, "X");
    }

    #[test]
    fn ntfs_pbr_assembly_keeps_bpb_and_magic() {
        let mut bpb = [0xEEu8; 0x54 - 0x0B];
        bpb[0] = 0x00; // whatever mkfs wrote
        let br = assemble_ntfs_pbr(&bpb);
        // the ms-sys NTFS boot record spans the 8-sector $Boot area
        assert_eq!(br.len(), 0x54 + NTFS_BR_0X54.len());
        assert_eq!(br.len(), 4136);
        assert_eq!(
            &br[0x00..0x0B],
            &[0xEB, 0x52, 0x90, b'N', b'T', b'F', b'S', b' ', b' ', b' ', b' ']
        );
        assert_eq!(
            &br[0x0B..0x54],
            &bpb[..],
            "BPB written by mkfs.ntfs must be kept verbatim"
        );
        assert_eq!(&br[0x1FE..0x200], &[0x55, 0xAA], "boot signature");
    }

    #[test]
    fn wim_image_list_rejects_non_wim() {
        // a zeroed file is not a WIM
        let dir = std::env::temp_dir().join(format!("rufus-wimhdr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("fake.iso");
        std::fs::write(&p, vec![0u8; 40 * 2048]).unwrap();
        assert!(wim_image_list(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In-memory media for badblocks tests; specific 512 KiB blocks can fail
    /// on write, fail on read or silently return damaged content.
    struct MockMedia {
        size: u64,
        stored: std::collections::BTreeMap<u64, Vec<u8>>,
        bad_write: Vec<u64>,
        bad_read: Vec<u64>,
        corrupt: Vec<u64>,
    }

    impl MockMedia {
        fn new(blocks: u64) -> Self {
            MockMedia {
                size: blocks * BB_BLOCK_SIZE,
                stored: Default::default(),
                bad_write: vec![],
                bad_read: vec![],
                corrupt: vec![],
            }
        }
    }

    impl BlockIo for MockMedia {
        fn size(&self) -> u64 {
            self.size
        }
        fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
            let blk = off / BB_BLOCK_SIZE;
            let n = buf.len() as u64 / BB_BLOCK_SIZE;
            if (blk..blk + n).any(|b| self.bad_read.contains(&b)) {
                bail!("injected read fault");
            }
            for b in blk..blk + n {
                let s = ((b - blk) * BB_BLOCK_SIZE) as usize;
                let e = s + BB_BLOCK_SIZE as usize;
                match self.stored.get(&b) {
                    Some(d) if !self.corrupt.contains(&b) => buf[s..e].copy_from_slice(d),
                    _ => buf[s..e].fill(0xEE), // never written or damaged media
                }
            }
            Ok(())
        }
        fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
            let blk = off / BB_BLOCK_SIZE;
            let n = buf.len() as u64 / BB_BLOCK_SIZE;
            if (blk..blk + n).any(|b| self.bad_write.contains(&b)) {
                bail!("injected write fault");
            }
            for b in blk..blk + n {
                let s = ((b - blk) * BB_BLOCK_SIZE) as usize;
                let mut d = buf[s..s + BB_BLOCK_SIZE as usize].to_vec();
                if self.corrupt.contains(&b) {
                    d[..8].fill(0xEE); // media retains the pattern but damaged
                }
                self.stored.insert(b, d);
            }
            Ok(())
        }
    }

    #[test]
    fn badblocks_healthy_media_passes_and_writes_pattern() {
        let mut io = MockMedia::new(8);
        let eng = Engine::new(Options {
            bb_test: BadBlocksTest::TwoPasses,
            ..Default::default()
        });
        let mut msgs = 0;
        eng.badblocks_test(&mut io, &mut |_| {
            msgs += 1;
        })
        .unwrap();
        assert!(msgs > 0);
        // the final pass's pattern (0xaa) must cover the whole device
        for b in 0..8 {
            assert!(
                io.stored[&b].iter().all(|&x| x == 0xaa),
                "block {b} not patterned"
            );
        }
    }

    #[test]
    fn badblocks_reports_faulty_blocks() {
        let mut io = MockMedia::new(8);
        io.bad_write = vec![3];
        io.bad_read = vec![5];
        io.corrupt = vec![7];
        let eng = Engine::new(Options {
            bb_test: BadBlocksTest::OnePass,
            ..Default::default()
        });
        let err = eng.badblocks_test(&mut io, &mut |_| {}).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("3 bad block(s) found"), "{msg}");
        assert!(msg.contains("1 read, 1 write, 1 corruption"), "{msg}");
    }

    #[test]
    fn badblocks_aborts_at_threshold() {
        // every block bad: the test must abort at BB_THRESHOLD like Rufus
        let blocks = (BB_THRESHOLD + 8) as u64;
        let mut io = MockMedia::new(blocks);
        io.bad_write = (0..blocks).collect();
        io.bad_read = (0..blocks).collect();
        let eng = Engine::new(Options {
            bb_test: BadBlocksTest::OnePass,
            ..Default::default()
        });
        let err = eng.badblocks_test(&mut io, &mut |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("Too many bad blocks"));
    }
}

#[cfg(test)]
mod mbr_id_tests {
    use super::*;

    #[test]
    fn utf16le_bom_encoding() {
        // BOM + "A" = FF FE 41 00; label with non-ASCII round-trips
        let b = Engine::utf16le_with_bom("A\u{00e9}");
        assert_eq!(b, vec![0xFF, 0xFE, 0x41, 0x00, 0xE9, 0x00]);
    }

    #[test]
    fn fat_label_is_clamped_to_what_the_boot_sector_carries() {
        // uppercase + valid FAT characters pass through
        assert_eq!(sanitize_fat_label("my stick"), "MY STICK");
        assert_eq!(sanitize_fat_label("Win11_25H2"), "WIN11_25H2");
        // Rufus's invalid-character replacement
        assert_eq!(sanitize_fat_label("a*b?c/d"), "A_B_C_D");
        assert_eq!(sanitize_fat_label("data:vol"), "DATA_VOL");
        // longer than the 11-byte boot-sector field
        assert_eq!(sanitize_fat_label("ubuntu-24.04.1-desktop"), "UBUNTU-24.0");
        // non-ASCII (UTF-8) bytes cannot live in a FAT label — underscored
        assert_eq!(
            sanitize_fat_label("\u{627}\u{644}\u{627}\u{633}\u{645}"),
            "_____"
        );
        // trailing spaces are trimmed; an all-invalid label still fits 11
        assert_eq!(sanitize_fat_label("abc   "), "ABC");
        assert_eq!(sanitize_fat_label("").len(), 0);
    }

    #[test]
    fn mbr_assets_are_the_ms_sys_templates() {
        // Rufus MBR ("AKEO" boot code) and the Windows 7 MBR differ; both
        // leave the partition table area untouched (build_mbr copies only
        // the first 440 bytes, so a full 512-byte sector also works)
        assert_eq!(MBR_WIN7.len(), 440);
        assert_eq!(&MBR_BOOTCODE[..4], b"AKEO");
        assert_ne!(&MBR_WIN7[..4], b"AKEO");
        assert_ne!(MBR_BOOTCODE[..440], MBR_WIN7[..440]);
    }

    #[test]
    fn autorun_icon_is_an_ico_file() {
        // ICONDIR: reserved=0, type=1, count>=1
        assert_eq!(&AUTORUN_ICON[..4], &[0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn disk_id_masquerade_patches_the_boot_flag() {
        // the format.c patch: buffer[0x1be] = the selected BIOS disk ID,
        // applied after build_mbr, only when the Rufus MBR + bootable
        let parts = vec![Partition {
            bootable: true,
            fs: FsType::Ntfs,
            start_lba: 2048,
            sectors: 1000,
        }];
        let mut sec = mbr::build_mbr(&parts, Some(MBR_BOOTCODE), 0x1234_5678, 255, 63);
        assert_eq!(sec[0x1BE], 0x80); // default boot flag
        sec[0x1BE] = 0x81; // what the masquerade patch writes
        let (parsed, _, _) = crate::mbr::parse_mbr(&sec);
        // parse_mbr only knows 0x80 = bootable; the raw byte is the masquerade
        assert_eq!(sec[0x1BE], 0x81);
        assert!(parsed[0].start_lba == 2048);
    }

    #[test]
    fn autorun_files_created_with_utf16_label() {
        // SetAutorun parity: autorun.inf (UTF-16LE, CRLF) with the volume
        // label + icon line, and autorun.ico next to it
        let dir = std::env::temp_dir().join(format!("rufus-autorun-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let eng = Engine::new(Options {
            fs_label: "MY_STICK".into(),
            ..Default::default()
        });
        eng.write_autorun_files(&dir).unwrap();
        let inf = std::fs::read(dir.join("autorun.inf")).unwrap();
        assert_eq!(&inf[..2], &[0xFF, 0xFE]); // BOM
        let text: String = inf[2..]
            .chunks_exact(2)
            .map(|c| char::from_u32(u16::from_le_bytes([c[0], c[1]]) as u32).unwrap())
            .collect();
        assert!(text.contains("[autorun]"), "{text:?}");
        assert!(text.contains("icon  = autorun.ico"), "{text:?}");
        assert!(text.contains("label = MY_STICK"), "{text:?}");
        assert!(text.contains("\r\n")); // DOS line endings
        assert!(text.starts_with("; Created by rufus-linux"));
        assert_eq!(
            std::fs::read(dir.join("autorun.ico")).unwrap(),
            AUTORUN_ICON
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_autorun_inf_is_kept() {
        // Rufus SetAutorun: an existing autorun.inf is never overwritten
        let dir = std::env::temp_dir().join(format!("rufus-autorun-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("autorun.inf"), b"KEEP ME").unwrap();
        let eng = Engine::new(Options::default());
        eng.write_autorun_files(&dir).unwrap();
        assert_eq!(std::fs::read(dir.join("autorun.inf")).unwrap(), b"KEEP ME");
        assert!(!dir.join("autorun.ico").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn autorun_disabled_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("rufus-autorun-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let eng = Engine::new(Options {
            autorun: false,
            ..Default::default()
        });
        eng.write_autorun_files(&dir).unwrap();
        assert!(!dir.join("autorun.inf").exists());
        assert!(!dir.join("autorun.ico").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
