//! Pure boot-loader patchers for GRUB2 (i386-pc) and syslinux.
//!
//! Both upstream installers work the same way: keep the loader file on the
//! filesystem, find its on-disk sectors, then patch the boot code to point at
//! them. The formats below were verified against the GRUB2 and syslinux
//! sources (grub `util/setup.c`, `grub-core/boot/i386/pc/{boot,diskboot}.S`,
//! `include/grub/offsets.h`; syslinux `libinstaller/{syslxmod.c,syslxint.h}`
//! as vendored in Rufus `src/syslinux/`) and against the bytes of our own
//! assets. Everything here is a pure function over buffers so it can be unit
//! tested without a device.

use anyhow::{bail, Result};

// ---------------------------------------------------------------------------
// GRUB2
// ---------------------------------------------------------------------------

/// boot.img: BPB copy area [0x03..0x5A), kernel_sector qword @0x5C,
/// boot_drive byte @0x64.
const GRUB_BPB_START: usize = 0x03;
const GRUB_BPB_END: usize = 0x5A;
const GRUB_KERNEL_SECTOR: usize = 0x5C;
const GRUB_BOOT_DRIVE: usize = 0x64;
/// diskboot blocklist: top of the entry stack inside core.img's first sector.
const GRUB_BLOCKLIST_TOP: usize = 0x1F4;
/// entry size: {u64 start_lba, u16 len_sectors, u16 segment}
const GRUB_BLOCKLIST_ENTRY: usize = 12;
/// slots between 0x14C and 0x200 — a 16th entry would clobber diskboot code
const GRUB_MAX_ENTRIES: usize = 15;
/// grub setup.c caps core.img at 888 sectors
const GRUB_MAX_CORE_SECTORS: u64 = 888;
/// destination segment of file sector 1 (0x8000 + 512 >> 4)
const GRUB_FIRST_SEG: u16 = 0x820;

/// Patch core.img's diskboot blocklist table with the file's real sectors.
///
/// `lbas[i]` is the volume-relative 512-byte LBA of file sector i (the full
/// file, including sector 0 which boot.img loads itself — it is excluded
/// here). `part_start_lba` converts them to the disk-absolute LBAs the
/// blocklist expects. Entries are written downward from 0x1F4, each
/// destination segment continuing where the previous one ended.
pub fn patch_core_blocklists(core: &mut [u8], lbas: &[u64], part_start_lba: u64) -> Result<()> {
    if !core.len().is_multiple_of(512) {
        bail!("core.img length {} is not sector-aligned", core.len());
    }
    let sectors = (core.len() / 512) as u64;
    if sectors > GRUB_MAX_CORE_SECTORS {
        bail!("core.img is too big ({sectors} sectors, max {GRUB_MAX_CORE_SECTORS})");
    }
    if lbas.len() != sectors as usize {
        bail!(
            "core.img sector map has {} entries, expected {sectors}",
            lbas.len()
        );
    }
    // extent list over file sectors 1..n (sector 0 is loaded by boot.img);
    // each extent is one int13 read, so it stays under 0x7f sectors
    // (setup.c save_blocklists merge rule)
    let mut extents: Vec<(u64, u16)> = Vec::new();
    for &l in &lbas[1..] {
        match extents.last_mut() {
            Some((start, len)) if *start + u64::from(*len) == l && *len < 0x7f => *len += 1,
            _ => extents.push((l, 1)),
        }
    }
    if extents.len() > GRUB_MAX_ENTRIES {
        bail!(
            "core.img is too fragmented ({} extents, max {GRUB_MAX_ENTRIES})",
            extents.len()
        );
    }
    // clear the blocklist area (0x14C..0x200 — the 15 usable slots), killing
    // any entries left over from a previous install of the ISO's core.img
    core[0x14C..GRUB_BLOCKLIST_TOP + GRUB_BLOCKLIST_ENTRY].fill(0);
    let mut seg = GRUB_FIRST_SEG;
    for (k, (start, len)) in extents.iter().enumerate() {
        let off = GRUB_BLOCKLIST_TOP - k * GRUB_BLOCKLIST_ENTRY;
        let e = &mut core[off..off + GRUB_BLOCKLIST_ENTRY];
        e[0..8].copy_from_slice(&(part_start_lba + start).to_le_bytes());
        e[8..10].copy_from_slice(&len.to_le_bytes());
        e[10..12].copy_from_slice(&seg.to_le_bytes());
        seg += (u32::from(*len) * 32) as u16;
    }
    Ok(())
}

/// Patch boot.img for a FAT volume: copy the formatted VBR's BPB over the
/// template's (grub-bios-setup parity: bytes 3..0x5A), point kernel_sector at
/// the file's first absolute sector, leave boot_drive = 0xFF ("use BIOS DL").
pub fn patch_grub_boot(boot: &mut [u8], formatted_vbr: &[u8], first_abs_lba: u64) -> Result<()> {
    if boot.len() < GRUB_BOOT_DRIVE + 1 || formatted_vbr.len() < GRUB_BPB_END {
        bail!("boot sector too small for GRUB2 patching");
    }
    boot[GRUB_BPB_START..GRUB_BPB_END]
        .copy_from_slice(&formatted_vbr[GRUB_BPB_START..GRUB_BPB_END]);
    boot[GRUB_KERNEL_SECTOR..GRUB_KERNEL_SECTOR + 8].copy_from_slice(&first_abs_lba.to_le_bytes());
    boot[GRUB_BOOT_DRIVE] = 0xFF;
    Ok(())
}

// ---------------------------------------------------------------------------
// syslinux
// ---------------------------------------------------------------------------

/// ldlinux patch-area / boot-sector magic ("SYSLINUX" installers scan for it)
pub const LDLINUX_MAGIC: u32 = 0x3EB202FE;
/// ADV_SIZE: the installer appends two 512-byte ADV sectors to the file
pub const ADV_SIZE: usize = 512;
/// ext patch area field offsets (relative to the ext patch area itself)
const EPA_ADV_PTR: usize = 0;
const EPA_SECPTR: usize = 10;
const EPA_SECPTR_CNT: usize = 12;
const EPA_SECT1_PTR0: usize = 14;
const EPA_SECT1_PTR1: usize = 16;
/// patch-area field offsets, relative to the LDLINUX_MAGIC dword itself:
/// magic @+0, instance @+4, data_sectors @+8, adv_sectors @+10, dwords @+12,
/// checksum @+16, maxtransfer @+20, epaoffset @+22
const PA_DATA_SECTORS: usize = 8;
const PA_ADV_SECTORS: usize = 10;
const PA_DWORDS: usize = 12;
const PA_CHECKSUM: usize = 16;
const PA_EPA_OFFSET: usize = 22;

/// Offsets the installer must also patch in the 512-byte .bss template.
pub struct LdlinuxPatch {
    pub sect1ptr0: usize,
    pub sect1ptr1: usize,
}

/// A blank ADV (adv.sys): two identical 512-byte halves with
/// ADV_MAGIC1 @0, ADV_MAGIC3 @508 and a checksum @4 balancing
/// ADV_MAGIC4 (0xA3041767) across the data area.
pub fn blank_adv() -> [u8; 2 * ADV_SIZE] {
    let mut adv = [0u8; 2 * ADV_SIZE];
    for half in adv.as_chunks_mut::<ADV_SIZE>().0 {
        half[0..4].copy_from_slice(&0x5A2D_2FA5u32.to_le_bytes());
        half[4..8].copy_from_slice(&0xA304_1767u32.to_le_bytes());
        half[508..512].copy_from_slice(&0xDD28_BF64u32.to_le_bytes());
    }
    adv
}

/// Build the ldlinux.sys file contents: the loader image zero-padded to a
/// whole number of sectors, then the 2-sector ADV appended (Rufus parity —
/// but padded so the ADV pointers land on real sector starts).
pub fn build_ldlinux_file(image: &[u8]) -> Vec<u8> {
    let sectors = image.len().div_ceil(512);
    let mut out = vec![0u8; sectors * 512 + 2 * ADV_SIZE];
    out[..image.len()].copy_from_slice(image);
    let adv = blank_adv();
    out[sectors * 512..].copy_from_slice(&adv);
    out
}

/// Patch an ldlinux.sys file buffer with its on-disk sector map.
///
/// `lbas[i]` = volume-relative 512-byte LBA of file sector i (all of them,
/// including sector 0 and the two trailing ADV sectors); `raw_image_len` is
/// the unpadded loader image length (the checksum domain, like the upstream
/// installer). Returns the .bss patch offsets for [`assemble_syslinux_vbr`].
pub fn patch_ldlinux(file: &mut [u8], raw_image_len: usize, lbas: &[u64]) -> Result<LdlinuxPatch> {
    if !file.len().is_multiple_of(512) || file.len() < 3 * 512 {
        bail!(
            "ldlinux.sys length {} is not a sane sector count",
            file.len()
        );
    }
    let nsect = file.len() / 512;
    if lbas.len() != nsect {
        bail!(
            "ldlinux.sys sector map has {} entries, expected {nsect}",
            lbas.len()
        );
    }
    // locate the patch area by magic (the upstream installer scans for it);
    // all field offsets below are relative to the magic dword
    let pa = (0..512usize)
        .step_by(4)
        .find(|&i| {
            u32::from_le_bytes([file[i], file[i + 1], file[i + 2], file[i + 3]]) == LDLINUX_MAGIC
        })
        .ok_or_else(|| anyhow::anyhow!("ldlinux.sys: patch area magic not found"))?;
    let epa_off =
        u16::from_le_bytes([file[pa + PA_EPA_OFFSET], file[pa + PA_EPA_OFFSET + 1]]) as usize;
    if epa_off + EPA_SECT1_PTR1 + 2 > file.len() {
        bail!("ldlinux.sys: ext patch area out of range");
    }
    let u16_at = |off: usize| u16::from_le_bytes([file[off], file[off + 1]]) as usize;
    let advptr_offset = u16_at(epa_off + EPA_ADV_PTR);
    let secptr_offset = u16_at(epa_off + EPA_SECPTR);
    let secptr_count = u16_at(epa_off + EPA_SECPTR_CNT);
    let sect1ptr0 = u16_at(epa_off + EPA_SECT1_PTR0);
    let sect1ptr1 = u16_at(epa_off + EPA_SECT1_PTR1);

    // patch-area counters
    file[pa + PA_DATA_SECTORS..pa + PA_DATA_SECTORS + 2]
        .copy_from_slice(&((nsect - 2) as u16).to_le_bytes());
    file[pa + PA_ADV_SECTORS..pa + PA_ADV_SECTORS + 2].copy_from_slice(&2u16.to_le_bytes());
    file[pa + PA_DWORDS..pa + PA_DWORDS + 4]
        .copy_from_slice(&((raw_image_len >> 2) as u32).to_le_bytes());

    // sector extents: cover the data sectors after sector 0, excluding the
    // two ADV sectors. Merge rules from syslinux generate_extents(): run on,
    // but never cross a 64 KiB destination boundary nor exceed one full
    // int13 read (< 64 KiB); destinations run 0x8000 + 512*sector_index.
    let first_data = 1usize;
    let last_data = nsect - 3; // inclusive index of the last non-ADV sector
    if last_data < first_data {
        bail!("ldlinux.sys too small to have data sectors");
    }
    let slots = &mut file[secptr_offset..secptr_offset + secptr_count * 10];
    slots.fill(0);
    let mut n_ext = 0usize;
    let mut k = first_data;
    while k <= last_data {
        let start_lba = lbas[k];
        let mut len = 1usize;
        while k + len <= last_data {
            let base = 0x8000u64 + 512 * k as u64;
            let grow = (len as u64 + 1) * 512;
            if lbas[k + len] != start_lba + len as u64
                || grow >= 65536
                || ((base ^ (base + grow - 1)) & 0xFFFF_0000) != 0
            {
                break;
            }
            len += 1;
        }
        if n_ext == secptr_count {
            bail!("ldlinux.sys: too many sector extents for the patch area");
        }
        let e = &mut slots[n_ext * 10..n_ext * 10 + 10];
        e[0..8].copy_from_slice(&start_lba.to_le_bytes());
        e[8..10].copy_from_slice(&(len as u16).to_le_bytes());
        n_ext += 1;
        k += len;
    }
    // ADV pointers: the last two sectors of the file
    let ap = &mut file[advptr_offset..advptr_offset + 16];
    ap[0..8].copy_from_slice(&lbas[nsect - 2].to_le_bytes());
    ap[8..16].copy_from_slice(&lbas[nsect - 1].to_le_bytes());

    // checksum: 0x3EB202FE minus the sum of the loader image's dwords
    // (the checksum field itself zeroed first, like the upstream installer)
    let cs = pa + PA_CHECKSUM;
    file[cs..cs + 4].copy_from_slice(&0u32.to_le_bytes());
    let mut csum = LDLINUX_MAGIC;
    let domain = raw_image_len & !3;
    for chunk in file[..domain].as_chunks::<4>().0 {
        csum = csum.wrapping_sub(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    file[cs..cs + 4].copy_from_slice(&csum.to_le_bytes());

    Ok(LdlinuxPatch {
        sect1ptr0,
        sect1ptr1,
    })
}

/// Assemble the syslinux VBR from the .bss template + the formatted volume's
/// boot sector: keep the bss jump/OEM and loader code, keep the formatted
/// BPB (11..90), then patch hidden sectors and the ldlinux.sys first-sector
/// pointer (syslinux_make_bootsect + syslinux_patch parity).
pub fn assemble_syslinux_vbr(
    bss: &[u8],
    formatted_vbr: &[u8],
    first_lba: u64,
    part_start_lba: u32,
    sect1ptr0: usize,
    sect1ptr1: usize,
) -> Result<[u8; 512]> {
    if bss.len() < 512 || formatted_vbr.len() < 0x5A {
        bail!("syslinux: bss/VBR templates too small");
    }
    let mut out = [0u8; 512];
    out[0..11].copy_from_slice(&bss[0..11]); // jump + OEM "SYSLINUX"
    out[11..90].copy_from_slice(&formatted_vbr[11..90]); // real BPB
    out[90..510].copy_from_slice(&bss[90..510]); // loader code
    out[0x1C..0x20].copy_from_slice(&part_start_lba.to_le_bytes()); // hidden
    out[sect1ptr0..sect1ptr0 + 4]
        .copy_from_slice(&((first_lba & 0xFFFF_FFFF) as u32).to_le_bytes());
    out[sect1ptr1..sect1ptr1 + 4].copy_from_slice(&((first_lba >> 32) as u32).to_le_bytes());
    out[510..512].copy_from_slice(&bss[510..512]); // 0x55AA
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- GRUB2 ----

    /// A synthetic core.img of `sectors` sectors with the pristine diskboot
    /// blocklist area (all zeros).
    fn fake_core(sectors: usize) -> Vec<u8> {
        let mut core = vec![0u8; sectors * 512];
        core[0] = 0x52; // diskboot prologue markers
        core[1] = 0x56;
        core[0x146] = 0xC3; // last code byte, like the real asset
        core
    }

    fn read_entry(core: &[u8], k: usize) -> (u64, u16, u16) {
        let off = GRUB_BLOCKLIST_TOP - k * GRUB_BLOCKLIST_ENTRY;
        let start = u64::from_le_bytes(core[off..off + 8].try_into().unwrap());
        let len = u16::from_le_bytes(core[off + 8..off + 10].try_into().unwrap());
        let seg = u16::from_le_bytes(core[off + 10..off + 12].try_into().unwrap());
        (start, len, seg)
    }

    #[test]
    fn grub_blocklist_contiguous() {
        // 85 contiguous sectors at volume LBA 1000, partition at 2048 —
        // exactly the shape of assets/core.img on a fresh volume
        let mut core = fake_core(85);
        let lbas: Vec<u64> = (1000..1085).collect();
        patch_core_blocklists(&mut core, &lbas, 2048).unwrap();
        // entry 0 covers sectors 1..84 (sector 0 belongs to boot.img)
        assert_eq!(read_entry(&core, 0), (2048 + 1001, 84, 0x820));
        // terminator below the last entry
        assert_eq!(&core[0x1E8..0x1F4], &[0u8; 12]);
        // no residue anywhere else in the blocklist area
        assert!(core[0x14C..0x1F4].iter().all(|&b| b == 0));
    }

    #[test]
    fn grub_blocklist_fragmented_and_segments() {
        let mut core = fake_core(10);
        // sectors: 0-2 @10..13, 3-4 @20..22, 5-9 @30..35
        let mut lbas = vec![10, 11, 12, 20, 21, 30, 31, 32, 33, 34];
        lbas.truncate(10);
        patch_core_blocklists(&mut core, &lbas, 100).unwrap();
        assert_eq!(read_entry(&core, 0), (100 + 11, 2, 0x820));
        assert_eq!(read_entry(&core, 1), (100 + 20, 2, 0x820 + 2 * 32));
        assert_eq!(read_entry(&core, 2), (100 + 30, 5, 0x820 + 4 * 32));
    }

    #[test]
    fn grub_blocklist_rejects_fragmentation_and_size() {
        // 16 extents -> over the 15-slot table
        let mut core = fake_core(32);
        let lbas: Vec<u64> = (0..32).map(|i| i * 100).collect();
        assert!(patch_core_blocklists(&mut core, &lbas, 0)
            .unwrap_err()
            .to_string()
            .contains("fragmented"));
        // over the 888-sector grub cap
        let mut core = fake_core(900);
        let lbas: Vec<u64> = (0..900).collect();
        assert!(patch_core_blocklists(&mut core, &lbas, 0)
            .unwrap_err()
            .to_string()
            .contains("too big"));
    }

    #[test]
    fn grub_boot_bpb_and_kernel_sector() {
        let mut boot = crate::engine::GRUB2_BOOT.to_vec();
        let mut vbr = vec![0u8; 512];
        for (i, b) in vbr.iter_mut().enumerate().skip(3).take(0x5A - 3) {
            *b = (i & 0xFF) as u8; // recognizable BPB bytes
        }
        vbr[510..512].copy_from_slice(&[0x55, 0xAA]);
        patch_grub_boot(&mut boot, &vbr, 2048 + 500).unwrap();
        assert_eq!(&boot[3..0x5A], &vbr[3..0x5A]);
        assert_eq!(
            u64::from_le_bytes(boot[0x5C..0x64].try_into().unwrap()),
            2548
        );
        assert_eq!(boot[0x64], 0xFF);
        // untouched boot.img fields
        assert_eq!(&boot[0x5A..0x5C], &[0x00, 0x80]); // kernel_address
        assert_eq!(&boot[510..512], &[0x55, 0xAA]);
    }

    // ---- syslinux ----

    fn ldlinux_v6() -> &'static [u8] {
        crate::engine::LDLINUX_V6_SYS
    }
    fn ldlinux_v4() -> &'static [u8] {
        crate::engine::LDLINUX_V4_SYS
    }

    #[test]
    fn blank_adv_structure() {
        let adv = blank_adv();
        for half in adv.chunks_exact(512) {
            assert_eq!(
                u32::from_le_bytes(half[0..4].try_into().unwrap()),
                0x5A2D_2FA5
            );
            // checksum balance: dword[4] + sum(dwords 8..508) == 0xA3041767
            let mut sum = u32::from_le_bytes(half[4..8].try_into().unwrap());
            for d in half[8..508].chunks_exact(4) {
                sum = sum.wrapping_add(u32::from_le_bytes(d.try_into().unwrap()));
            }
            assert_eq!(sum, 0xA304_1767);
            assert_eq!(
                u32::from_le_bytes(half[508..512].try_into().unwrap()),
                0xDD28_BF64
            );
        }
    }

    #[test]
    fn build_file_pads_and_appends_adv() {
        let f = build_ldlinux_file(ldlinux_v6());
        let img_sectors = ldlinux_v6().len().div_ceil(512);
        assert_eq!(f.len(), img_sectors * 512 + 1024);
        assert_eq!(&f[..ldlinux_v6().len()], ldlinux_v6());
        // padding is zero and the ADV halves are identical
        assert!(f[ldlinux_v6().len()..img_sectors * 512]
            .iter()
            .all(|&b| b == 0));
        assert_eq!(
            f[img_sectors * 512..img_sectors * 512 + 512],
            f[img_sectors * 512 + 512..]
        );
    }

    /// ext-patch-area offsets read out of a patched ldlinux.sys buffer.
    fn epa_fields(file: &[u8]) -> (usize, usize, usize) {
        let epa_off = u16::from_le_bytes(
            file[0x18 + PA_EPA_OFFSET..0x18 + PA_EPA_OFFSET + 2]
                .try_into()
                .unwrap(),
        ) as usize;
        let u16_at =
            |off: usize| u16::from_le_bytes(file[off..off + 2].try_into().unwrap()) as usize;
        (
            u16_at(epa_off + EPA_ADV_PTR),
            u16_at(epa_off + EPA_SECPTR),
            u16_at(epa_off + EPA_SECPTR_CNT),
        )
    }

    /// Reconstruct the checksum independently: sum over the whole patched
    /// image (with the checksum field zeroed) must equal 0x3EB202FE - csum.
    fn recompute_checksum(file: &[u8], raw_len: usize) -> u32 {
        let cs = 0x18 + PA_CHECKSUM; // magic sits at 0x18 in our assets
        let mut sum = 0u32;
        let domain = raw_len & !3;
        for (i, chunk) in file[..domain].chunks_exact(4).enumerate() {
            let mut v = u32::from_le_bytes(chunk.try_into().unwrap());
            if i * 4 == cs {
                v = 0; // the field is zeroed before the installer sums
            }
            sum = sum.wrapping_add(v);
        }
        LDLINUX_MAGIC.wrapping_sub(sum)
    }

    #[test]
    fn patch_v6_contiguous() {
        let image = ldlinux_v6();
        let raw_len = image.len();
        let mut file = build_ldlinux_file(image);
        let nsect = file.len() / 512;
        assert_eq!(nsect, 136);
        let lbas: Vec<u64> = (5000..5000 + nsect as u64).collect();
        let patch = patch_ldlinux(&mut file, raw_len, &lbas).unwrap();

        // patch-area counters (magic dword at 0x18)
        assert_eq!(
            u16::from_le_bytes(
                file[0x18 + PA_DATA_SECTORS..0x18 + PA_DATA_SECTORS + 2]
                    .try_into()
                    .unwrap()
            ),
            134
        );
        assert_eq!(
            u16::from_le_bytes(
                file[0x18 + PA_ADV_SECTORS..0x18 + PA_ADV_SECTORS + 2]
                    .try_into()
                    .unwrap()
            ),
            2
        );
        assert_eq!(
            u32::from_le_bytes(
                file[0x18 + PA_DWORDS..0x18 + PA_DWORDS + 4]
                    .try_into()
                    .unwrap()
            ),
            (raw_len >> 2) as u32
        );

        // extents: base for sector 1 is 0x8200, so the first run stops at
        // the 64 KiB boundary (63 sectors); the rest is contiguous
        let (advptr, secptr, secptr_cnt) = epa_fields(&file);
        assert_eq!(secptr, 0x1EE);
        assert_eq!(advptr, 0x16D8);
        assert_eq!(secptr_cnt, 192);
        let rd_ext = |k: usize| {
            let e = &file[secptr + k * 10..secptr + k * 10 + 10];
            (
                u64::from_le_bytes(e[0..8].try_into().unwrap()),
                u16::from_le_bytes(e[8..10].try_into().unwrap()),
            )
        };
        assert_eq!(rd_ext(0), (5001, 63));
        assert_eq!(rd_ext(1), (5064, 70));
        assert_eq!(rd_ext(2), (0, 0)); // terminator

        // ADV pointers = last two sectors
        assert_eq!(
            u64::from_le_bytes(file[0x16D8..0x16E0].try_into().unwrap()),
            5000 + nsect as u64 - 2
        );
        assert_eq!(
            u64::from_le_bytes(file[0x16E0..0x16E8].try_into().unwrap()),
            5000 + nsect as u64 - 1
        );

        // checksum matches an independent recomputation
        assert_eq!(
            u32::from_le_bytes(
                file[0x18 + PA_CHECKSUM..0x18 + PA_CHECKSUM + 4]
                    .try_into()
                    .unwrap()
            ),
            recompute_checksum(&file, raw_len)
        );

        // bss pointer offsets for the VBR assembler
        assert_eq!(patch.sect1ptr0, 0x11A);
        assert_eq!(patch.sect1ptr1, 0x120);
    }

    #[test]
    fn patch_v4_and_fragmented() {
        let image = ldlinux_v4();
        let raw_len = image.len();
        let mut file = build_ldlinux_file(image);
        let nsect = file.len() / 512;
        assert_eq!(nsect, 74);
        // fragmented map: gaps every 20 sectors
        let mut lbas = Vec::new();
        let mut l = 100u64;
        for i in 0..nsect {
            if i > 0 && i % 20 == 0 {
                l += 1000;
            }
            lbas.push(l);
            l += 1;
        }
        patch_ldlinux(&mut file, raw_len, &lbas).unwrap();
        // 71 data sectors (1..71) split at every gap: 19+20+20, then the
        // last run is cut by the 64 KiB dest rule at 0xF800 (4 sectors) and
        // continues aligned at 0x10000 (8 sectors)
        let (_, secptr, _) = epa_fields(&file);
        let rd_ext = |k: usize| {
            let e = &file[secptr + k * 10..secptr + k * 10 + 10];
            (
                u64::from_le_bytes(e[0..8].try_into().unwrap()),
                u16::from_le_bytes(e[8..10].try_into().unwrap()),
            )
        };
        assert_eq!(rd_ext(0), (101, 19));
        assert_eq!(rd_ext(1), (1120, 20));
        assert_eq!(rd_ext(2), (2140, 20));
        assert_eq!(rd_ext(3), (3160, 4));
        assert_eq!(rd_ext(4), (3164, 8));
        assert_eq!(rd_ext(5), (0, 0)); // terminator
        assert_eq!(
            u32::from_le_bytes(
                file[0x18 + PA_DWORDS..0x18 + PA_DWORDS + 4]
                    .try_into()
                    .unwrap()
            ),
            9122
        );
        assert_eq!(
            u32::from_le_bytes(
                file[0x18 + PA_CHECKSUM..0x18 + PA_CHECKSUM + 4]
                    .try_into()
                    .unwrap()
            ),
            recompute_checksum(&file, raw_len)
        );
    }

    #[test]
    fn assemble_vbr_layout() {
        let bss = crate::engine::LDLINUX_V6_BSS;
        let mut vbr = vec![0u8; 512];
        vbr[11..90].fill(0x5A); // synthetic BPB area
        let out = assemble_syslinux_vbr(bss, &vbr, 12345, 2048, 0x11A, 0x120).unwrap();
        assert_eq!(&out[0..3], &bss[0..3]); // jump
        assert_eq!(&out[3..11], b"SYSLINUX");
        // BPB kept from the volume, except the hidden-sectors field
        assert_eq!(&out[11..0x1C], &vbr[11..0x1C]);
        assert_eq!(&out[0x20..90], &vbr[0x20..90]);
        // loader code from the bss, except the two patched sector pointers
        assert_eq!(&out[90..0x11A], &bss[90..0x11A]);
        assert_eq!(&out[0x11E..0x120], &bss[0x11E..0x120]);
        assert_eq!(&out[0x124..510], &bss[0x124..510]);
        assert_eq!(
            u32::from_le_bytes(out[0x1C..0x20].try_into().unwrap()),
            2048
        );
        assert_eq!(
            u32::from_le_bytes(out[0x11A..0x11E].try_into().unwrap()),
            12345
        );
        assert_eq!(u32::from_le_bytes(out[0x120..0x124].try_into().unwrap()), 0);
        assert_eq!(&out[510..512], &[0x55, 0xAA]);
        // the placeholders from the pristine bss are gone
        assert_ne!(
            u32::from_le_bytes(out[0x11A..0x11E].try_into().unwrap()),
            0xDEAD_BEEF
        );
    }

    /// The v4/v6 .bss templates only differ in the instance constant — one
    /// patcher serves both, so the first-sector pointer offsets must match.
    #[test]
    fn bss_templates_share_patch_offsets() {
        for bss in [crate::engine::LDLINUX_V6_BSS, crate::engine::LDLINUX_V4_BSS] {
            assert_eq!(
                u32::from_le_bytes(bss[0x11A..0x11E].try_into().unwrap()),
                0xDEAD_BEEF
            );
            assert_eq!(
                u32::from_le_bytes(bss[0x120..0x124].try_into().unwrap()),
                0xFEED_FACE
            );
        }
    }
}
