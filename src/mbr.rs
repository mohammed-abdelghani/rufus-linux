//! MBR partition table read/write.

use crate::disk::SECTOR;

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum FsType {
    Empty = 0x00,
    Fat16 = 0x06,
    Ntfs = 0x07,
    Extended = 0x05,
    Fat32Chs = 0x0B,
    Fat32Lba = 0x0C,
    Linux = 0x83,
    GptProtective = 0xEE,
    /// Rufus "BIOS Compatibility" extra partition
    /// (RUFUS_EXTRA_PARTITION_TYPE, added by the old-BIOS fixes)
    RufusExtra = 0xEA,
}

impl FsType {
    pub fn name(&self) -> &'static str {
        match self {
            FsType::Fat16 => "FAT16",
            FsType::Fat32Lba => "FAT32 (LBA)",
            FsType::Fat32Chs => "FAT32 (CHS)",
            FsType::Ntfs => "NTFS/exFAT",
            FsType::Linux => "Linux",
            FsType::Extended => "Extended",
            FsType::GptProtective => "GPT Protective",
            FsType::RufusExtra => "BIOS Compatibility",
            FsType::Empty => "Empty",
        }
    }
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x06 => FsType::Fat16,
            0x0B => FsType::Fat32Chs,
            0x0C => FsType::Fat32Lba,
            0x07 => FsType::Ntfs,
            0x05 => FsType::Extended,
            0xEE => FsType::GptProtective,
            0xEA => FsType::RufusExtra,
            0x83 => FsType::Linux,
            _ => FsType::Empty,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub bootable: bool,
    pub fs: FsType,
    pub start_lba: u64,
    pub sectors: u64, // size in sectors
}

impl Partition {
    pub fn size_bytes(&self) -> u64 {
        self.sectors * SECTOR
    }
}

/// Parse the 4 primary MBR entries. Returns (partitions, signature_ok).
pub fn parse_mbr(sector: &[u8; 512]) -> (Vec<Partition>, u16, u16) {
    let mut parts = Vec::new();
    for i in 0..4 {
        let off = 0x1BE + i * 16;
        let p = &sector[off..off + 16];
        let status = p[0];
        let start_lba = u32::from_le_bytes([p[8], p[9], p[10], p[11]]) as u64;
        let num_sectors = u32::from_le_bytes([p[12], p[13], p[14], p[15]]) as u64;
        if num_sectors == 0 && status == 0 {
            continue;
        }
        parts.push(Partition {
            bootable: status == 0x80,
            fs: FsType::from_byte(p[4]),
            start_lba,
            sectors: num_sectors,
        });
    }
    let sig = u16::from_le_bytes([sector[510], sector[511]]);
    (parts, sig, u16::from_le_bytes([sector[440], sector[441]]))
}

/// CHS encode for partition entry. For LBA-only we use max CHS values
/// (heads=254? use standard: head=254? Actually use 0xFFFFFF style via head=255? )
/// Windows/Linux accept start_head=254 trick; safest is to fill with the
/// geometry from HDIO_GETGEO. We use a common approach: head=254 not needed —
/// most tools ignore CHS when LBA fields are set. We write
/// head = num_heads-1 capped, sector/cyl computed loosely.
pub fn chs_fields(lba: u64, sectors: u64, heads: u16, sec_per_track: u8) -> (u8, u8, u8) {
    let hpc = heads.max(1) as u64;
    let spt = sec_per_track.max(1) as u64;
    let abs = lba;
    let head = (abs / spt) % hpc;
    let cyl = abs / (spt * hpc);
    let sect = (abs % spt) + 1;
    let end_abs = lba + sectors.saturating_sub(1);
    let e_head = (end_abs / spt) % hpc;
    let e_cyl = end_abs / (spt * hpc);
    let e_sect = (end_abs % spt) + 1;
    // encode: head byte, sector byte (bits 0-5 = sector-1, bits 6-7 = cyl high), cyl low byte
    let pack = |head: u64, cyl: u64, sect: u64| -> (u8, u8, u8) {
        let head = head.min(255) as u8;
        let cyl_low = (cyl & 0xFF) as u8;
        let cyl_high = ((cyl >> 8) & 0x3) as u8;
        let sect_enc = ((sect - 1) & 0x3F) as u8 | (cyl_high << 6);
        (head, sect_enc, cyl_low)
    };
    let (sh, ss, sc) = pack(head, cyl, sect);
    let (eh, es, ec) = pack(e_head, e_cyl, e_sect);
    // return packed (start_head, start_sect_enc, start_cyl_low) — caller stores raw
    let _ = (eh, es, ec);
    (sh, ss, sc)
}

/// Build a 512-byte MBR sector with given partitions.
/// `boot_code`: optional 440-byte boot code template (e.g. grub4dos mbr or syslinux mbr).
pub fn build_mbr(
    parts: &[Partition],
    boot_code: Option<&[u8]>, // >=440 bytes used
    disk_signature: u32,
    heads: u16,
    sec_per_track: u8,
) -> [u8; 512] {
    let mut sec = [0u8; 512];
    if let Some(code) = boot_code {
        let n = code.len().min(440);
        sec[..n].copy_from_slice(&code[..n]);
    }
    for (i, p) in parts.iter().take(4).enumerate() {
        let off = 0x1BE + i * 16;
        let (sh, ss, sc) = chs_fields(p.start_lba, p.sectors, heads, sec_per_track);
        // end chs
        let end_lba = p.start_lba + p.sectors.saturating_sub(1);
        let hpc = heads.max(1) as u64;
        let spt = sec_per_track.max(1) as u64;
        let e_head = ((end_lba / spt) % hpc).min(255) as u8;
        let e_cyl = end_lba / (spt * hpc);
        let e_sect = (end_lba % spt) + 1;
        let e_sect_enc = ((e_sect - 1) & 0x3F) as u8 | (((e_cyl >> 8) & 0x3) as u8) << 6;
        let e_cyl_low = (e_cyl & 0xFF) as u8;
        sec[off] = if p.bootable { 0x80 } else { 0x00 };
        sec[off + 1] = sh;
        sec[off + 2] = ss;
        sec[off + 3] = sc;
        sec[off + 4] = p.fs as u8;
        sec[off + 5] = e_head;
        sec[off + 6] = e_sect_enc;
        sec[off + 7] = e_cyl_low;
        sec[off + 8..off + 12].copy_from_slice(&(p.start_lba as u32).to_le_bytes());
        sec[off + 12..off + 16].copy_from_slice(&(p.sectors as u32).to_le_bytes());
    }
    // disk signature at 0x1B8
    sec[0x1B8..0x1BC].copy_from_slice(&disk_signature.to_le_bytes());
    // 0x55AA
    sec[510] = 0x55;
    sec[511] = 0xAA;
    sec
}

/// Sanity check an MBR signature.
pub fn valid_signature(sector: &[u8; 512]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xAA
}

/// Result of detecting partitioning style.
#[derive(Debug)]
pub enum PartitionStyle {
    Mbr(Vec<Partition>),
    Gpt,
    Empty,
}

pub fn detect_style(sector: &[u8; 512]) -> PartitionStyle {
    if !valid_signature(sector) {
        return PartitionStyle::Empty;
    }
    let (parts, _, _) = parse_mbr(sector);
    if parts.iter().any(|p| p.fs == FsType::GptProtective) {
        return PartitionStyle::Gpt;
    }
    if parts.is_empty() {
        PartitionStyle::Empty
    } else {
        PartitionStyle::Mbr(parts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_parse_roundtrip() {
        let parts = vec![
            Partition { bootable: true, fs: FsType::Fat32Lba, start_lba: 2048, sectors: 1_000_000 },
            Partition { bootable: false, fs: FsType::Linux, start_lba: 1_002_048, sectors: 524_288 },
        ];
        let sec = build_mbr(&parts, None, 0xDEAD_BEEF, 255, 63);
        assert!(valid_signature(&sec));
        let (parsed, sig, disk_sig) = parse_mbr(&sec);
        assert_eq!(parsed.len(), 2);
        assert!(parsed[0].bootable);
        assert_eq!(parsed[0].fs, FsType::Fat32Lba);
        assert_eq!(parsed[0].start_lba, 2048);
        assert_eq!(parsed[0].sectors, 1_000_000);
        assert!(!parsed[1].bootable);
        assert_eq!(parsed[1].fs, FsType::Linux);
        assert_eq!(parsed[1].start_lba, 1_002_048);
        assert_eq!(parsed[1].sectors, 524_288);
        assert_eq!(sig, 0xAA55); // boot signature
        // disk signature: parse_mbr returns the low 16 bits at 0x1B8
        assert_eq!(disk_sig, 0xBEEF);
    }

    #[test]
    fn rufus_extra_partition_type_roundtrip() {
        // the old-BIOS-fixes "BIOS Compatibility" partition (type 0xEA)
        let sec = build_mbr(
            &[Partition { bootable: false, fs: FsType::RufusExtra, start_lba: 130_977, sectors: 63 }],
            None,
            1,
            255,
            63,
        );
        let (parts, _, _) = parse_mbr(&sec);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].fs, FsType::RufusExtra);
        assert_eq!(parts[0].fs as u8, 0xEA);
        assert!(!parts[0].bootable);
        // must not be mistaken for a GPT protective entry
        assert!(matches!(detect_style(&sec), PartitionStyle::Mbr(_)));
    }

    #[test]
    fn boot_code_and_signature_placement() {
        let code = vec![0x90u8; 440];
        let sec = build_mbr(
            &[Partition { bootable: true, fs: FsType::Fat32Lba, start_lba: 2048, sectors: 100 }],
            Some(&code),
            0x1122_3344,
            255,
            63,
        );
        assert!(sec[..440].iter().all(|&b| b == 0x90));
        assert_eq!(
            u32::from_le_bytes(sec[0x1B8..0x1BC].try_into().unwrap()),
            0x1122_3344
        );
    }

    #[test]
    fn chs_fields_standard_geometry() {
        // lba 2048 with 255 heads / 63 spt -> cyl 0, head 32, sector 33
        let (h, s, c) = chs_fields(2048, 100, 255, 63);
        assert_eq!(h, 32);
        assert_eq!(s & 0x3F, 32);
        assert_eq!(c, 0);
    }

    #[test]
    fn detect_style_cases() {
        let empty = [0u8; 512];
        assert!(matches!(detect_style(&empty), PartitionStyle::Empty));
        let mut gpt = [0u8; 512];
        gpt[510] = 0x55;
        gpt[511] = 0xAA;
        gpt[0x1C2] = 0xEE;
        gpt[0x1CA..0x1CE].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(matches!(detect_style(&gpt), PartitionStyle::Gpt));
        let mbr = build_mbr(
            &[Partition { bootable: true, fs: FsType::Fat32Lba, start_lba: 2048, sectors: 1000 }],
            None,
            1,
            255,
            63,
        );
        assert!(matches!(detect_style(&mbr), PartitionStyle::Mbr(_)));
    }
}
