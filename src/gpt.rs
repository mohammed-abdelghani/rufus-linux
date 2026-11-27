//! Minimal GPT (GUID Partition Table) read/write.

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub const EMPTY: Guid = Guid([0u8; 16]);
    /// Microsoft basic data partition
    pub const MS_BASIC_DATA: Guid = Guid([
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ]);
    /// EFI System Partition
    pub const EFI_SYSTEM: Guid = Guid([
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ]);
    /// Linux filesystem data (0FC63DAF-8483-4772-8E79-3D69D8477DE4)
    pub const LINUX_FS: Guid = Guid([
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D,
        0xE4,
    ]);
    /// Microsoft Reserved Partition (E3C9E316-0B5C-4DB8-817D-F92DF00215AE) —
    /// required by Microsoft on every GPT disk Windows runs from (Windows To Go)
    pub const MS_RESERVED: Guid = Guid([
        0x16, 0xE3, 0xC9, 0xE3, 0x5C, 0x0B, 0xB8, 0x4D, 0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02, 0x15,
        0xAE,
    ]);
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let d1 = u32::from_le_bytes([self.0[0], self.0[1], self.0[2], self.0[3]]);
        let d2 = u16::from_le_bytes([self.0[4], self.0[5]]);
        let d3 = u16::from_le_bytes([self.0[6], self.0[7]]);
        write!(
            f,
            "{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            self.0[8],
            self.0[9],
            self.0[10],
            self.0[11],
            self.0[12],
            self.0[13],
            self.0[14],
            self.0[15]
        )
    }
}

#[derive(Debug, Clone)]
pub struct GptPartition {
    pub index: u32,
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub first_lba: u64,
    pub last_lba: u64,
    pub name: String,
}

impl GptPartition {
    pub fn size_sectors(&self) -> u64 {
        self.last_lba.saturating_sub(self.first_lba) + 1
    }
}

pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Parse GPT from raw sectors: sector1 = header, sectors 2.. = entries.
pub fn parse_gpt(header_sector: &[u8], entries_sectors: &[[u8; 512]]) -> Result<Vec<GptPartition>> {
    if header_sector.len() < 92 || &header_sector[0..8] != GPT_SIGNATURE {
        return Err(Error::NotGpt);
    }
    let entry_lba = u64::from_le_bytes(header_sector[72..80].try_into().unwrap());
    let num_entries = u32::from_le_bytes(header_sector[80..84].try_into().unwrap()) as usize;
    let entry_size = u32::from_le_bytes(header_sector[84..88].try_into().unwrap()) as usize;
    if entry_size == 0 || entry_size > 512 {
        return Err(Error::BadGptEntrySize(entry_size));
    }
    let _ = entry_lba;
    let mut parts = Vec::new();
    // entries_sectors are consecutive starting at entry_lba; assume entry_lba==2
    let mut idx = 0usize;
    'outer: for sec in entries_sectors.iter() {
        for off in (0..512).step_by(entry_size) {
            if idx >= num_entries {
                break 'outer;
            }
            let e = &sec[off..off + entry_size];
            let mut tg = [0u8; 16];
            let mut ug = [0u8; 16];
            tg.copy_from_slice(&e[0..16]);
            ug.copy_from_slice(&e[16..32]);
            let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
            let type_guid = Guid(tg);
            let used = type_guid != Guid::EMPTY;
            let mut name_bytes = [0u8; 72];
            name_bytes.copy_from_slice(&e[56..128.min(entry_size).min(128)]);
            // name is UTF-16LE up to 36 chars
            let mut name = String::new();
            for i in (0..72).step_by(2) {
                let c = u16::from_le_bytes([name_bytes[i], name_bytes[i + 1]]);
                if c == 0 {
                    break;
                }
                name.push(char::from_u32(c as u32).unwrap_or('\u{FFFD}'));
            }
            if used {
                parts.push(GptPartition {
                    index: idx as u32,
                    type_guid,
                    unique_guid: Guid(ug),
                    first_lba: first,
                    last_lba: last,
                    name,
                });
            }
            idx += 1;
        }
    }
    Ok(parts)
}

/// Built GPT: (protective MBR sector, primary header sector, entry sectors).
pub type GptTable = ([u8; 512], [u8; 512], Vec<[u8; 512]>);

/// One partition entry for [`build_gpt_entries`].
#[derive(Debug, Clone)]
pub struct GptPartSpec {
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub first_lba: u64,
    pub last_lba: u64,
    pub name: String,
}

/// Build a protective MBR + primary GPT with arbitrary partitions.
/// `disk_sectors`: total disk size in 512B sectors.
pub fn build_gpt_entries(
    disk_sectors: u64,
    disk_guid: Guid,
    parts: &[GptPartSpec],
) -> Result<GptTable> {
    if parts.is_empty() {
        return Err(Error::BadGptRange);
    }
    for p in parts {
        if p.last_lba <= p.first_lba || p.last_lba >= disk_sectors {
            return Err(Error::BadGptRange);
        }
    }

    // MBR: protective entry covering whole disk
    let mut mbr = [0u8; 512];
    mbr[0x1BF] = 0xFF;
    mbr[0x1C0] = 0xFF;
    mbr[0x1C1] = 0xFF;
    mbr[0x1C2] = 0xEE;
    mbr[0x1C3] = 0xFF;
    mbr[0x1C4] = 0xFF;
    mbr[0x1C5] = 0xFF;
    mbr[0x1C6..0x1CA].copy_from_slice(&1u32.to_le_bytes());
    let mbr_size = (disk_sectors - 1).min(u32::MAX as u64) as u32;
    mbr[0x1CA..0x1CE].copy_from_slice(&mbr_size.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    // GPT header at LBA1
    let mut hdr = [0u8; 512];
    hdr[0..8].copy_from_slice(GPT_SIGNATURE);
    hdr[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    hdr[12..16].copy_from_slice(&92u32.to_le_bytes()); // header size
    hdr[16..20].copy_from_slice(&0u32.to_le_bytes()); // crc32 (patched below)
    hdr[20..24].copy_from_slice(&0u32.to_le_bytes()); // reserved
    hdr[24..32].copy_from_slice(&1u64.to_le_bytes()); // my lba
    hdr[32..40].copy_from_slice(&(disk_sectors - 1).to_le_bytes()); // alternate lba
    hdr[40..48].copy_from_slice(&34u64.to_le_bytes()); // first usable
    hdr[48..56].copy_from_slice(&(disk_sectors - 34).to_le_bytes()); // last usable
    hdr[56..72].copy_from_slice(&disk_guid.0);
    hdr[72..80].copy_from_slice(&2u64.to_le_bytes()); // entries lba
    hdr[80..84].copy_from_slice(&128u32.to_le_bytes()); // num entries
    hdr[84..88].copy_from_slice(&128u32.to_le_bytes()); // entry size

    // entries: up to 128 partitions (32 sectors)
    let mut entries = vec![[0u8; 512]; 32];
    for (i, p) in parts.iter().enumerate() {
        let e = &mut entries[i / 4][(i % 4) * 128..(i % 4 + 1) * 128];
        e[0..16].copy_from_slice(&p.type_guid.0);
        e[16..32].copy_from_slice(&p.unique_guid.0);
        e[32..40].copy_from_slice(&p.first_lba.to_le_bytes());
        e[40..48].copy_from_slice(&p.last_lba.to_le_bytes());
        // attributes at 48..56 = 0
        for (j, c) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + j * 2..56 + j * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
    }
    let mut entries_bytes = Vec::with_capacity(entries.len() * 512);
    for s in &entries {
        entries_bytes.extend_from_slice(s);
    }
    let ecrc = crc32(&entries_bytes);
    hdr[88..96].copy_from_slice(&(ecrc as u64).to_le_bytes());
    // header crc
    hdr[16..20].copy_from_slice(&0u32.to_le_bytes());
    let hcrc = crc32(&hdr[..92]);
    hdr[16..20].copy_from_slice(&hcrc.to_le_bytes());

    Ok((mbr, hdr, entries))
}

/// Build a fresh protective MBR + primary GPT for a single FAT32 partition.
/// `disk_sectors`: total disk size in 512B sectors.
/// `part_start_lba`: first usable LBA for the partition.
pub fn build_gpt_single(
    disk_sectors: u64,
    part_start_lba: u64,
    part_end_lba: u64,
    disk_guid: Guid,
) -> Result<GptTable> {
    build_gpt_entries(
        disk_sectors,
        disk_guid,
        &[GptPartSpec {
            type_guid: Guid::MS_BASIC_DATA,
            unique_guid: disk_guid,
            first_lba: part_start_lba,
            last_lba: part_end_lba,
            name: "Rufus Linux".into(),
        }],
    )
}

/// Compute CRC32 (IEEE) of a byte slice — used for GPT header/entries CRC.
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn guid_strings_mixed_endian() {
        assert_eq!(
            Guid::EFI_SYSTEM.to_string(),
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        );
        assert_eq!(
            Guid::MS_BASIC_DATA.to_string(),
            "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        );
        assert_eq!(
            Guid::LINUX_FS.to_string(),
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
        );
    }

    #[test]
    fn build_gpt_single_layout_and_roundtrip() {
        let disk_sectors = 16_777_216u64; // 8 GiB
        let part_end = disk_sectors - 2049;
        let (mbr, hdr, entries) =
            build_gpt_single(disk_sectors, 2048, part_end, Guid([7u8; 16])).unwrap();
        // protective MBR
        assert_eq!(mbr[0x1C2], 0xEE);
        assert_eq!(mbr[510], 0x55);
        assert_eq!(mbr[511], 0xAA);
        // header fields
        assert_eq!(&hdr[0..8], GPT_SIGNATURE);
        assert_eq!(u64::from_le_bytes(hdr[72..80].try_into().unwrap()), 2); // entries LBA
        assert_eq!(u32::from_le_bytes(hdr[80..84].try_into().unwrap()), 128);
        assert_eq!(u32::from_le_bytes(hdr[84..88].try_into().unwrap()), 128);
        assert_eq!(u64::from_le_bytes(hdr[40..48].try_into().unwrap()), 34);
        assert_eq!(
            u64::from_le_bytes(hdr[48..56].try_into().unwrap()),
            disk_sectors - 34
        );
        // header CRC self-consistent
        let mut h = hdr;
        h[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(hdr[16..20].try_into().unwrap()),
            crc32(&h[..92])
        );
        // entries CRC matches
        let mut eb = Vec::with_capacity(entries.len() * 512);
        for s in &entries {
            eb.extend_from_slice(s);
        }
        assert_eq!(
            u64::from_le_bytes(hdr[88..96].try_into().unwrap()),
            crc32(&eb) as u64
        );
        // round-trip: parse back the built table
        let parts = parse_gpt(&hdr, &entries).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].type_guid, Guid::MS_BASIC_DATA);
        assert_eq!(parts[0].first_lba, 2048);
        assert_eq!(parts[0].last_lba, part_end);
        assert_eq!(parts[0].name, "Rufus Linux");
    }

    #[test]
    fn build_gpt_rejects_bad_range() {
        // end LBA must be strictly below the disk size
        assert!(build_gpt_single(100_000, 2048, 100_000, Guid([1u8; 16])).is_err());
        assert!(build_gpt_single(100_000, 2048, 2048, Guid([1u8; 16])).is_err());
        // empty range
    }
}
