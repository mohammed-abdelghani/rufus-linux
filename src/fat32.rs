//! Minimal FAT32 formatter + BPB generation.
//! Creates a valid FAT32 filesystem on a block device or file image.

use crate::error::{Error, Result};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};

/// Geometry chosen by formatter.
pub struct Fat32Params {
    pub bytes_per_sector: u32, // 512 or 4096
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32, // 32 typical
    pub num_fats: u32,         // 2
    pub sectors_per_fat: u32,
    pub root_cluster: u32, // 2
    pub total_sectors: u64,
    pub fat_start: u64,  // LBA of FAT1
    pub data_start: u64, // LBA of data area
    pub total_clusters: u32,
}

/// Decide FAT32 geometry for a given partition size.
///
/// `bps` must match the target device's logical sector size (512, or 4096 on
/// 4Kn drives). On 512-byte sectors BIOS/syslinux int13 can boot the result;
/// 4Kn volumes are UEFI-only.
pub fn compute_params(partition_bytes: u64, bps: u32) -> Result<Fat32Params> {
    let gib = 1024u64 * 1024 * 1024;
    let mib = 1024u64 * 1024;
    let total_sectors = partition_bytes / bps as u64;
    // the BPB stores the total sector count in a 32-bit field
    if total_sectors > u32::MAX as u64 {
        return Err(Error::PartitionTooLarge);
    }
    let reserved = 32u32;
    let num_fats = 2u32;
    // cluster size tiers (mirrors Windows/mkfs.vfat sizes), then shrink the
    // cluster if the FAT32 minimum of 65525 clusters wouldn't be met — needed
    // on 4096-byte-sector (4Kn) volumes where each sector is 8x bigger
    let mut spc = cluster_tier(partition_bytes, mib, gib);
    let data_estimate = total_sectors.saturating_sub(64); // reserved + FAT slack
    while spc > 1 && (data_estimate / spc as u64) < 65525 {
        spc /= 2;
    }
    // iterative: fat size depends on cluster count
    let mut spf: u32 = 0;
    let mut clusters;
    for _ in 0..8 {
        let data_start = (reserved + num_fats * spf) as u64;
        let data_sectors = total_sectors.saturating_sub(data_start);
        clusters = (data_sectors / spc as u64) as u32;
        // FAT entries needed = clusters + 2; bytes = entries*4; sectors = /bps
        let fat_bytes = (clusters as u64 + 2) * 4;
        let new_spf = fat_bytes.div_ceil(bps as u64) as u32;
        // round up to even
        let new_spf = new_spf + (new_spf & 1);
        if new_spf == spf {
            break;
        }
        spf = new_spf;
    }
    if spf == 0 {
        return Err(Error::FatSizeLoop);
    }
    let fat_start = reserved as u64;
    let data_start = reserved as u64 + num_fats as u64 * spf as u64;
    let data_sectors = total_sectors.saturating_sub(data_start);
    clusters = (data_sectors / spc as u64) as u32;
    // FAT32 requires at least 65525 data clusters
    if clusters < 65525 {
        return Err(Error::PartitionTooSmall);
    }
    if clusters > 0x0FFF_FFF6 {
        return Err(Error::PartitionTooLarge);
    }
    Ok(Fat32Params {
        bytes_per_sector: bps,
        sectors_per_cluster: spc,
        reserved_sectors: reserved,
        num_fats,
        sectors_per_fat: spf,
        root_cluster: 2,
        total_sectors,
        fat_start,
        data_start,
        total_clusters: clusters,
    })
}

fn cluster_tier(partition_bytes: u64, mib: u64, gib: u64) -> u32 {
    if partition_bytes <= 260 * mib {
        1
    } else if partition_bytes <= 8 * gib {
        8
    } else if partition_bytes <= 16 * gib {
        16
    } else if partition_bytes <= 32 * gib {
        32
    } else if partition_bytes <= 64 * gib {
        64
    } else {
        128
    }
}

/// Format a FAT32 filesystem onto `writer` (must be seekable, positioned at 0
/// of the partition). `partition_bytes` = size of the partition, `bps` = the
/// device's logical sector size (512 or 4096), `hidden_sectors` = the LBA the
/// partition starts at (BPB "hidden sectors", used by boot loaders).
///
/// The `progress` callback reports `(cumulative bytes written, grand total)`
/// over the bytes THIS formatter actually writes — not the partition size —
/// so a quick format (reserved + FATs + root only) still fills a progress
/// bar from 0% to 100% instead of sitting at ~0% the whole time.
pub fn format_fat32<W: Write + Seek>(
    w: &mut W,
    partition_bytes: u64,
    bps: u32,
    label: &str,
    hidden_sectors: u32,
    zero_data: bool,
    progress: &mut dyn FnMut(u64, u64), // (done, total)
) -> Result<Fat32Params> {
    let p = compute_params(partition_bytes, bps)?;
    let bps = p.bytes_per_sector as usize;
    // 1) boot sector + FSInfo + backup boot sector (reserved area = 32 sectors)
    let mut reserved = vec![0u8; p.reserved_sectors as usize * bps];
    write_boot_sector(&mut reserved[..bps], &p, label, hidden_sectors);
    write_fsinfo(&mut reserved[bps..2 * bps]);
    // backup boot sector at sector 6, backup FSInfo at sector 7
    write_boot_sector(&mut reserved[6 * bps..7 * bps], &p, label, hidden_sectors);
    write_fsinfo(&mut reserved[7 * bps..8 * bps]);
    let reserved_bytes = reserved.len() as u64;

    // 2) FATs
    let fat_bytes = p.sectors_per_fat as u64 * bps as u64;
    let fat_total = fat_bytes * p.num_fats as u64;

    // 3) root directory cluster (the volume label entry lives here)
    let root_off = cluster_offset(&p, p.root_cluster);
    let spc_bytes = p.sectors_per_cluster as u64 * bps as u64;
    let root_bytes = spc_bytes;

    // 4) zero rest of data area (optional — slow on USB)
    let data_end = p.data_start * bps as u64 + p.total_clusters as u64 * spc_bytes;
    let data_bytes = if zero_data {
        data_end.saturating_sub(root_off + spc_bytes)
    } else {
        0
    };
    let total = reserved_bytes + fat_total + root_bytes + data_bytes;

    let mut done = 0u64;
    w.write_all(&reserved)?;
    done += reserved_bytes;
    progress(done, total);

    let mut fat = vec![0u8; fat_bytes as usize];
    // cluster 0: media descriptor in the low byte, reserved bits high
    fat[0] = 0xF8;
    fat[1] = 0xFF;
    fat[2] = 0xFF;
    fat[3] = 0x0F;
    // cluster 1 = EOF; cluster 2 (root) = EOC
    fat[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    let eoc: u32 = 0x0FFF_FFFF;
    fat[8..12].copy_from_slice(&eoc.to_le_bytes());
    // mark all clusters EOF? No — free clusters are 0. Only used ones get values.
    // For a fresh FS: all clusters free (0), root dir cluster = EOC.
    for i in 0..p.num_fats {
        w.seek(SeekFrom::Start(
            (p.fat_start + i as u64 * p.sectors_per_fat as u64) * bps as u64,
        ))?;
        w.write_all(&fat)?;
    }
    done += fat_total;
    progress(done, total);

    // zero the root directory cluster (cluster 2) and write the volume
    // label entry (mkfs.vfat/Rufus parity: Windows shows it as the label)
    let mut root = vec![0u8; spc_bytes as usize];
    if !label.is_empty() {
        write_volume_label_entry(&mut root, label);
    }
    w.seek(SeekFrom::Start(root_off))?;
    w.write_all(&root)?;
    done += root_bytes;
    progress(done, total);

    if zero_data {
        let mut off = root_off + spc_bytes;
        let chunk = vec![0u8; 4 * 1024 * 1024];
        while off < data_end {
            let n = (data_end - off).min(chunk.len() as u64) as usize;
            w.seek(SeekFrom::Start(off))?;
            w.write_all(&chunk[..n])?;
            off += n as u64;
            done += n as u64;
            progress(done, total);
        }
    }
    // flush FATs again? already written.
    w.flush()?;
    Ok(p)
}

fn cluster_offset(p: &Fat32Params, cluster: u32) -> u64 {
    // data area starts at data_start sector; cluster 2 is first
    let idx = (cluster - 2) as u64;
    (p.data_start + idx * p.sectors_per_cluster as u64) * p.bytes_per_sector as u64
}

fn write_boot_sector(sec: &mut [u8], p: &Fat32Params, label: &str, hidden_sectors: u32) {
    let bps = p.bytes_per_sector as usize;
    debug_assert!(sec.len() >= bps);
    let s = &mut sec[..bps];
    // Jump + OEM
    s[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    s[3..11].copy_from_slice(b"MSWIN4.1");
    // BPB
    s[11..13].copy_from_slice(&(p.bytes_per_sector as u16).to_le_bytes());
    s[13] = p.sectors_per_cluster as u8;
    s[14..16].copy_from_slice(&(p.reserved_sectors as u16).to_le_bytes());
    s[16] = p.num_fats as u8;
    s[17..19].copy_from_slice(&0u16.to_le_bytes()); // root entries (unused in FAT32)
    s[19..21].copy_from_slice(&0u16.to_le_bytes()); // total16 (0 => use total32)
    s[21] = 0xF8; // media
    s[22..24].copy_from_slice(&0u16.to_le_bytes()); // FAT16 size (0)
    s[24..26].copy_from_slice(&63u16.to_le_bytes()); // sectors/track
    s[26..28].copy_from_slice(&255u16.to_le_bytes()); // heads
    s[28..32].copy_from_slice(&hidden_sectors.to_le_bytes()); // hidden (partition start LBA)
    s[32..36].copy_from_slice(&(p.total_sectors as u32).to_le_bytes());
    s[36..40].copy_from_slice(&p.sectors_per_fat.to_le_bytes());
    s[40..42].copy_from_slice(&0u16.to_le_bytes()); // flags
    s[42..44].copy_from_slice(&0u16.to_le_bytes()); // version
    s[44..48].copy_from_slice(&p.root_cluster.to_le_bytes());
    s[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
    s[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
                                                    // drive number etc
    s[64] = 0x80;
    s[66] = 0x29; // extended boot signature
                  // volume ID — derive from label hash for determinism
    let vid = simple_hash(label.as_bytes());
    s[67..71].copy_from_slice(&vid.to_le_bytes());
    // volume label at 71..82 (11 bytes, space-padded)
    let mut lbl = [b' '; 11];
    for (i, b) in label.bytes().take(11).enumerate() {
        lbl[i] = b.to_ascii_uppercase();
    }
    s[71..82].copy_from_slice(&lbl);
    s[82..90].copy_from_slice(b"FAT32   ");
    // boot sector signature lives in the last two bytes of the sector
    s[bps - 2] = 0x55;
    s[bps - 1] = 0xAA;
}

/// FSInfo sector (Microsoft FAT32 spec, 512 bytes regardless of bps):
/// lead sig @0, struct sig @484, free count @488, next-free @492,
/// reserved 496..508, trail sig 0xAA550000 @508 (bytes 510..512 = 55 AA).
fn write_fsinfo(sec: &mut [u8]) {
    let s = &mut sec[..512];
    s[0..4].copy_from_slice(b"RRaA");
    let fss = 484;
    s[fss..fss + 4].copy_from_slice(b"rrAa");
    // free cluster count (unknown -> 0xFFFFFFFF so OS scans)
    s[fss + 4..fss + 8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    // next free cluster hint
    s[fss + 8..fss + 12].copy_from_slice(&3u32.to_le_bytes());
    // trailing signature: FSI_TrailSig = 0xAA550000 LE => 55 AA in the last
    // two bytes (previously written at 508 — off by two vs the spec)
    s[510] = 0x55;
    s[511] = 0xAA;
}

/// Volume label directory entry (ATTR_VOLUME_ID), 32 bytes at the start of
/// the root directory — what Windows displays as the volume label.
fn write_volume_label_entry(root: &mut [u8], label: &str) {
    let e = &mut root[..32];
    let mut name = [b' '; 11];
    for (i, b) in label.bytes().take(11).enumerate() {
        name[i] = b.to_ascii_uppercase();
    }
    e[0..11].copy_from_slice(&name);
    e[11] = 0x08; // ATTR_VOLUME_ID
                  // rest stays zero: no timestamps/clusters required for a label entry
}

fn simple_hash(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &x in b {
        h ^= x as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

// ---------------------------------------------------------------------------
// FAT32 reader: BPB parse, directory lookup (8.3 + LFN) and cluster chains.
// Used to locate boot-loader files (ldlinux.sys / core.img) on a freshly
// written volume so the boot sectors can point at their real on-disk
// sectors, exactly like the upstream syslinux/grub installers.
// ---------------------------------------------------------------------------

/// Geometry parsed from a FAT32 boot sector.
pub struct FatVolume {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub root_cluster: u32,
    /// first sector of the data area (cluster 2), in device sectors
    pub data_start: u64,
}

/// A directory entry found by [`FatVolume::find`].
pub struct DirEntry {
    pub name: String,
    pub first_cluster: u32,
    pub size: u64,
    pub is_dir: bool,
}

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;

impl FatVolume {
    /// Parse the BPB from the boot sector at volume offset 0.
    pub fn open<R: Read + Seek>(r: &mut R) -> Result<FatVolume> {
        let mut bs = [0u8; 512];
        r.seek(SeekFrom::Start(0))?;
        r.read_exact(&mut bs)?;
        let bps = u16::from_le_bytes([bs[11], bs[12]]) as u32;
        let spc = bs[13] as u32;
        let reserved = u16::from_le_bytes([bs[14], bs[15]]) as u32;
        let nfats = bs[16] as u32;
        let mut spf_b = [0u8; 4];
        spf_b.copy_from_slice(&bs[36..40]);
        let spf = u32::from_le_bytes(spf_b);
        let mut rc_b = [0u8; 4];
        rc_b.copy_from_slice(&bs[44..48]);
        let root_cluster = u32::from_le_bytes(rc_b) & 0x0FFF_FFFF;
        if !matches!(bps, 512 | 1024 | 2048 | 4096) {
            return Err(Error::BadFat32("unsupported bytes/sector"));
        }
        if spc == 0 || spc > 128 || !spc.is_power_of_two() {
            return Err(Error::BadFat32("bad sectors/cluster"));
        }
        if reserved == 0 || nfats == 0 || spf == 0 || root_cluster < 2 {
            return Err(Error::BadFat32("bad BPB geometry"));
        }
        Ok(FatVolume {
            bytes_per_sector: bps,
            sectors_per_cluster: spc,
            reserved_sectors: reserved,
            root_cluster,
            data_start: reserved as u64 + nfats as u64 * spf as u64,
        })
    }

    /// Byte offset of `cluster` within the volume.
    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        (self.data_start + (u64::from(cluster) - 2) * self.sectors_per_cluster as u64)
            * self.bytes_per_sector as u64
    }

    /// Cluster chain starting at `first` (guarding against loops).
    pub fn chain<R: Read + Seek>(&self, r: &mut R, first: u32) -> Result<Vec<u32>> {
        let mut out = vec![first];
        let mut seen: HashSet<u32> = HashSet::new();
        let mut cur = first;
        loop {
            let e = self.fat_entry(r, cur)?;
            if e == 0 || e >= 0x0FFF_FFF8 {
                break; // free or end-of-chain
            }
            if e == 0x0FFF_FFF7 {
                return Err(Error::BadFat32("bad cluster in chain"));
            }
            if !seen.insert(e) {
                return Err(Error::BadFat32("cluster chain loop"));
            }
            out.push(e);
            cur = e;
        }
        Ok(out)
    }

    fn fat_entry<R: Read + Seek>(&self, r: &mut R, cluster: u32) -> Result<u32> {
        let off =
            self.reserved_sectors as u64 * self.bytes_per_sector as u64 + u64::from(cluster) * 4;
        r.seek(SeekFrom::Start(off))?;
        let mut b = [0u8; 4];
        r.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b) & 0x0FFF_FFFF)
    }

    /// Read one directory (a cluster chain of 32-byte entries), joining
    /// Windows long-filename (LFN) parts onto their short entry.
    pub fn read_dir<R: Read + Seek>(&self, r: &mut R, dir_cluster: u32) -> Result<Vec<DirEntry>> {
        let clusters = self.chain(r, dir_cluster)?;
        let spc_bytes = self.sectors_per_cluster as usize * self.bytes_per_sector as usize;
        let mut buf = vec![0u8; spc_bytes];
        let mut out = Vec::new();
        // pending LFN parts: (seq, checksum, 13 UTF-16 code units)
        let mut lfn: Vec<(u8, u8, [u16; 13])> = Vec::new();
        'clusters: for c in clusters {
            r.seek(SeekFrom::Start(self.cluster_offset(c)))?;
            r.read_exact(&mut buf)?;
            for slot in buf.as_chunks::<32>().0 {
                match slot[0] {
                    0x00 => break 'clusters, // end-of-directory marker
                    0xE5 => {
                        // deleted
                        lfn.clear();
                        continue;
                    }
                    _ => {}
                }
                let attr = slot[11];
                if attr == ATTR_LONG_NAME {
                    let seq = slot[0];
                    let mut chars = [0u16; 13];
                    let raw: Vec<u8> = slot[1..11]
                        .iter()
                        .chain(&slot[14..26])
                        .chain(&slot[28..32])
                        .copied()
                        .collect();
                    for (i, ch) in raw.as_chunks::<2>().0.iter().enumerate() {
                        chars[i] = u16::from_le_bytes([ch[0], ch[1]]);
                    }
                    lfn.push((seq & 0x1F, slot[13], chars));
                    continue;
                }
                if attr & (ATTR_VOLUME_ID | ATTR_DIRECTORY) == ATTR_VOLUME_ID {
                    lfn.clear();
                    continue; // volume label
                }
                // short (8.3) entry
                let mut short = [0u8; 11];
                short.copy_from_slice(&slot[0..11]);
                if short[0] == 0x05 {
                    short[0] = 0xE5; // KANJI escape
                }
                let mut name = short_name_str(&short);
                // LFN parts appear in descending sequence order before the
                // short entry; verify the checksum, else fall back to 8.3
                let joined = join_lfn(&lfn, lfn_checksum(&short));
                lfn.clear();
                if let Some(long) = joined {
                    name = long;
                }
                let cl_lo = u16::from_le_bytes([slot[26], slot[27]]) as u32;
                let cl_hi = u16::from_le_bytes([slot[20], slot[21]]) as u32;
                let mut sz_b = [0u8; 4];
                sz_b.copy_from_slice(&slot[28..32]);
                out.push(DirEntry {
                    name,
                    first_cluster: cl_lo | (cl_hi << 16),
                    size: u32::from_le_bytes(sz_b) as u64,
                    is_dir: attr & ATTR_DIRECTORY != 0,
                });
            }
        }
        Ok(out)
    }

    /// Locate `path` relative to the root (case-insensitive, '/'-separated).
    pub fn find<R: Read + Seek>(&self, r: &mut R, path: &str) -> Result<Option<DirEntry>> {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let last = comps.len().saturating_sub(1);
        let mut cur = self.root_cluster;
        for (i, comp) in comps.iter().enumerate() {
            let entries = self.read_dir(r, cur)?;
            let Some(e) = entries
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(comp))
            else {
                return Ok(None);
            };
            if i == last {
                return Ok(Some(e));
            }
            if !e.is_dir {
                return Ok(None);
            }
            cur = e.first_cluster;
        }
        Ok(None)
    }

    /// 512-byte LBAs (volume-relative) covering the first
    /// `size.div_ceil(512)` sectors of the file at `first_cluster`.
    pub fn file_lbas<R: Read + Seek>(
        &self,
        r: &mut R,
        first_cluster: u32,
        size: u64,
    ) -> Result<Vec<u64>> {
        let clusters = self.chain(r, first_cluster)?;
        let want = size.div_ceil(512) as usize;
        let sectors_per_cluster =
            self.sectors_per_cluster as u64 * self.bytes_per_sector as u64 / 512;
        let mut out = Vec::with_capacity(want);
        for c in clusters {
            let base = self.cluster_offset(c) / 512;
            for i in 0..sectors_per_cluster {
                if out.len() == want {
                    return Ok(out);
                }
                out.push(base + i);
            }
        }
        Ok(out)
    }
}

/// 8.3 short entry bytes -> display string ("LDLINUX SYS" -> "LDLINUX.SYS").
fn short_name_str(short: &[u8; 11]) -> String {
    let base = std::str::from_utf8(&short[0..8])
        .unwrap_or("________")
        .trim_end();
    let ext = std::str::from_utf8(&short[8..11])
        .unwrap_or("___")
        .trim_end();
    if ext.is_empty() {
        base.to_string()
    } else {
        format!("{base}.{ext}")
    }
}

/// LFN filename checksum over the 11-byte short name.
fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &b in short {
        sum = sum.rotate_right(1).wrapping_add(b);
    }
    sum
}

/// Reassemble pending LFN parts into the long name, requiring every part to
/// carry the short entry's checksum and form a complete run 1..=max_seq.
fn join_lfn(lfn: &[(u8, u8, [u16; 13])], sum: u8) -> Option<String> {
    if lfn.is_empty() {
        return None;
    }
    let max_seq = lfn.iter().map(|(s, _, _)| *s).max()?;
    if lfn.len() != max_seq as usize || lfn.iter().any(|(_, c, _)| *c != sum) {
        return None;
    }
    let mut units: Vec<u16> = Vec::new();
    for (_, _, chars) in lfn.iter().rev() {
        units.extend_from_slice(chars);
    }
    let mut name = String::new();
    for u in units {
        match u {
            0x0000 | 0xFFFF => break,
            _ => name.push(char::from_u32(u32::from(u)).unwrap_or('\u{FFFD}')),
        }
    }
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    #[test]
    fn params_64mib_geometry() {
        let p = compute_params(64 * MIB, 512).unwrap();
        assert_eq!(p.bytes_per_sector, 512);
        assert_eq!(p.sectors_per_cluster, 1); // 64 MiB is within the smallest tier
        assert_eq!(p.reserved_sectors, 32);
        assert_eq!(p.num_fats, 2);
        assert_eq!(p.root_cluster, 2);
        assert_eq!(p.total_sectors, 64 * MIB / 512);
        assert_eq!(
            compute_params(1024 * MIB, 512).unwrap().sectors_per_cluster,
            8
        );
    }

    #[test]
    fn params_cluster_size_tiers() {
        assert_eq!(
            compute_params(260 * MIB, 512).unwrap().sectors_per_cluster,
            1
        );
        assert_eq!(compute_params(8 * GIB, 512).unwrap().sectors_per_cluster, 8);
        assert_eq!(
            compute_params(16 * GIB, 512).unwrap().sectors_per_cluster,
            16
        );
        assert_eq!(
            compute_params(32 * GIB, 512).unwrap().sectors_per_cluster,
            32
        );
        assert_eq!(
            compute_params(64 * GIB, 512).unwrap().sectors_per_cluster,
            64
        );
        assert_eq!(
            compute_params(128 * GIB, 512).unwrap().sectors_per_cluster,
            128
        );
    }

    #[test]
    fn params_too_small_or_too_large() {
        assert!(compute_params(MIB, 512).is_err());
        // >2 TiB cannot be expressed in the 32-bit total-sectors field
        assert!(compute_params(3 * 1024 * GIB, 512).is_err());
    }

    #[test]
    fn format_produces_valid_on_disk_structures() {
        let bytes = 64 * MIB;
        let mut buf = Cursor::new(vec![0u8; bytes as usize]);
        let p = format_fat32(&mut buf, bytes, 512, "test", 2048, false, &mut |_, _| {}).unwrap();
        let v = buf.into_inner();
        // boot sector
        assert_eq!(&v[0..3], &[0xEB, 0x58, 0x90]);
        assert_eq!(u16::from_le_bytes([v[11], v[12]]), 512);
        assert_eq!(v[13] as u32, p.sectors_per_cluster);
        assert_eq!(u16::from_le_bytes([v[14], v[15]]), 32); // reserved
        assert_eq!(v[16], 2); // FATs
        assert_eq!(
            u32::from_le_bytes(v[36..40].try_into().unwrap()),
            p.sectors_per_fat
        );
        assert_eq!(u32::from_le_bytes(v[28..32].try_into().unwrap()), 2048); // hidden
        assert_eq!(&v[82..90], b"FAT32   ");
        assert_eq!(&v[510..512], &[0x55, 0xAA]);
        // FSInfo in reserved sector 1 (trail sig in the LAST two bytes per spec)
        assert_eq!(&v[512..516], b"RRaA");
        assert_eq!(&v[512 + 484..512 + 488], b"rrAa");
        assert_eq!(&v[512 + 510..512 + 512], &[0x55, 0xAA]);
        assert_eq!(&v[512 + 496..512 + 508], &[0u8; 12]); // reserved2 untouched
                                                          // backup boot sector (sector 6) mirrors the primary
        assert_eq!(&v[0..512], &v[6 * 512..7 * 512]);
        // backup FSInfo (sector 7) mirrors the primary
        assert_eq!(&v[512..1024], &v[7 * 512..8 * 512]);
        // FAT1 at sector 32: media, cluster1=EOF, cluster2(root)=EOC
        let fat_off = 32 * 512;
        assert_eq!(&v[fat_off..fat_off + 4], &[0xF8, 0xFF, 0xFF, 0x0F]);
        assert_eq!(
            u32::from_le_bytes(v[fat_off + 4..fat_off + 8].try_into().unwrap()),
            0xFFFF_FFFF
        );
        assert_eq!(
            u32::from_le_bytes(v[fat_off + 8..fat_off + 12].try_into().unwrap()),
            0x0FFF_FFFF
        );
        // FAT2 mirrors FAT1
        let fat2_off = (32 + p.sectors_per_fat as usize) * 512;
        assert_eq!(&v[fat_off..fat_off + 12], &v[fat2_off..fat2_off + 12]);
        // root directory cluster: volume label entry first, rest zeroed
        let root_off = p.data_start as usize * 512;
        let root_len = p.sectors_per_cluster as usize * 512;
        assert_eq!(v[root_off + 11], 0x08); // ATTR_VOLUME_ID
        assert_eq!(&v[root_off..root_off + 11], b"TEST       ");
        assert!(v[root_off + 32..root_off + root_len]
            .iter()
            .all(|&b| b == 0));
    }

    #[test]
    fn label_uppercased_and_space_padded() {
        let bytes = 64 * MIB;
        let mut buf = Cursor::new(vec![0u8; bytes as usize]);
        format_fat32(&mut buf, bytes, 512, "my usb", 0, false, &mut |_, _| {}).unwrap();
        let v = buf.into_inner();
        assert_eq!(&v[71..82], b"MY USB     ");
    }

    #[test]
    fn empty_label_writes_no_root_entry() {
        let bytes = 64 * MIB;
        let mut buf = Cursor::new(vec![0u8; bytes as usize]);
        let p = format_fat32(&mut buf, bytes, 512, "", 0, false, &mut |_, _| {}).unwrap();
        let v = buf.into_inner();
        let root_off = p.data_start as usize * 512;
        assert!(v[root_off..root_off + 32].iter().all(|&b| b == 0));
        // boot sector label field stays space-padded blanks
        assert_eq!(&v[71..82], &[b' '; 11]);
    }

    #[test]
    fn quick_format_progress_fills_0_to_100() {
        // a quick format writes only reserved + FATs + root: the callback
        // total covers exactly those bytes and ends at 100%, so a progress
        // bar actually moves instead of sitting at ~0% (partition-relative
        // percentages stay near zero for real drive sizes)
        let bytes = 64 * MIB;
        let mut buf = Cursor::new(vec![0u8; bytes as usize]);
        let mut calls: Vec<(u64, u64)> = Vec::new();
        format_fat32(&mut buf, bytes, 512, "", 0, false, &mut |d, t| {
            calls.push((d, t))
        })
        .unwrap();
        let (last_done, total) = *calls.last().unwrap();
        assert_eq!(last_done, total, "quick format must end at 100%");
        assert!(calls.iter().all(|&(_, t)| t == total), "total is constant");
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0), "monotonic");
        // metadata only — nowhere near the 64 MiB partition size
        assert!(
            total < bytes / 2,
            "total {total} must be the written metadata"
        );
        assert!(calls.len() >= 3, "one callback per format step");
    }

    #[test]
    fn full_format_progress_fills_0_to_100() {
        // non-quick format adds the data-area zeroing to the same total
        let bytes = 64 * MIB;
        let mut buf = Cursor::new(vec![0u8; bytes as usize]);
        let mut last = (0u64, 0u64);
        format_fat32(&mut buf, bytes, 512, "", 0, true, &mut |d, t| last = (d, t)).unwrap();
        assert_eq!(last.0, last.1, "full format must end at 100%");
        assert!(last.1 > bytes / 2, "zeroing dominates the total");
    }

    #[test]
    fn params_4kn_shrinks_cluster_to_meet_minimum() {
        // on 4096-byte sectors the byte-based tier alone gives too few clusters
        let p = compute_params(512 * MIB, 4096).unwrap();
        assert_eq!(p.bytes_per_sector, 4096);
        assert_eq!(p.sectors_per_cluster, 1); // shrunk from tier 8
        assert!(p.total_clusters >= 65525);
        // too small even with 1-sector clusters
        assert!(compute_params(64 * MIB, 4096).is_err());
    }

    #[test]
    fn format_4kn_layout() {
        let bytes = 280 * MIB; // smallest practical 4Kn FAT32
        let mut buf = Cursor::new(vec![0u8; bytes as usize]);
        let p = format_fat32(&mut buf, bytes, 4096, "4KN", 256, false, &mut |_, _| {}).unwrap();
        assert_eq!(p.bytes_per_sector, 4096);
        let v = buf.into_inner();
        // boot sector signature in the LAST two bytes of the 4096-byte sector
        assert_eq!(&v[4094..4096], &[0x55, 0xAA]);
        assert_eq!(v[11..13], [0x00, 0x10]); // BPB bytes-per-sector = 4096 LE
        assert_eq!(u32::from_le_bytes(v[28..32].try_into().unwrap()), 256); // hidden
                                                                            // FSInfo sits in reserved sector 1 (bytes 4096..8192), trail sig @510
        assert_eq!(&v[4096..4100], b"RRaA");
        assert_eq!(&v[4096 + 484..4096 + 488], b"rrAa");
        assert_eq!(&v[4096 + 510..4096 + 512], &[0x55, 0xAA]);
        // backup boot sector at sector 6 mirrors the primary
        assert_eq!(&v[0..4096], &v[6 * 4096..7 * 4096]);
        // FAT1 at reserved sector 32
        let fat_off = 32 * 4096;
        assert_eq!(&v[fat_off..fat_off + 4], &[0xF8, 0xFF, 0xFF, 0x0F]);
    }

    // ----- reader tests ---------------------------------------------------

    /// A formatted volume with helpers to grow directories and files.
    struct Vol {
        p: Fat32Params,
        buf: Vec<u8>,
    }

    impl Vol {
        fn new() -> Self {
            let bytes = 64 * MIB;
            let mut buf = vec![0u8; bytes as usize];
            let p = format_fat32(
                &mut Cursor::new(&mut buf[..]),
                bytes,
                512,
                "V",
                0,
                false,
                &mut |_, _| {},
            )
            .unwrap();
            Vol { p, buf }
        }
        fn fat(&self, c: u32) -> u32 {
            let off = 32 * 512 + c as usize * 4;
            u32::from_le_bytes(self.buf[off..off + 4].try_into().unwrap()) & 0x0FFF_FFFF
        }
        fn set_fat(&mut self, c: u32, v: u32) {
            let off = 32 * 512 + c as usize * 4;
            self.buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        /// Allocate `n` contiguous free clusters linked into a chain.
        fn alloc(&mut self, n: u32) -> u32 {
            let mut start = None;
            let mut count = 0u32;
            let mut last = 0u32;
            for c in 3..u32::MAX {
                if self.fat(c) != 0 {
                    continue;
                }
                if start.is_none() {
                    start = Some(c);
                } else {
                    self.set_fat(last, c);
                }
                last = c;
                count += 1;
                if count == n {
                    break;
                }
            }
            self.set_fat(last, 0x0FFF_FFFF); // EOC
            start.unwrap()
        }
        fn entry_off(&self, dir_cluster: u32, slot: usize) -> usize {
            self.p.data_start as usize * 512
                + (dir_cluster as usize - 2) * self.p.sectors_per_cluster as usize * 512
                + slot * 32
        }
        fn put_entry(&mut self, dir_cluster: u32, slot: usize, e: &[u8; 32]) {
            let off = self.entry_off(dir_cluster, slot);
            self.buf[off..off + 32].copy_from_slice(e);
        }
        fn short_entry(short: &[u8; 11], attr: u8, first: u32, size: u64) -> [u8; 32] {
            let mut e = [0u8; 32];
            e[0..11].copy_from_slice(short);
            e[11] = attr;
            e[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
            e[26..28].copy_from_slice(&((first & 0xFFFF) as u16).to_le_bytes());
            e[28..32].copy_from_slice(&(size as u32).to_le_bytes());
            e
        }
        /// One LFN part (on-disk order: highest seq with 0x40 first).
        fn lfn_entry(seq: u8, units: &[u16], sum: u8) -> [u8; 32] {
            let mut e = [0u8; 32];
            e[0] = seq;
            e[11] = ATTR_LONG_NAME;
            e[12] = 0;
            e[13] = sum;
            let mut raw = [0u8; 26];
            for (i, u) in units.iter().take(13).enumerate() {
                raw[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            e[1..11].copy_from_slice(&raw[0..10]);
            e[14..26].copy_from_slice(&raw[10..22]);
            e[28..32].copy_from_slice(&raw[22..26]);
            e
        }
    }

    fn lfn_units(name: &str) -> Vec<Vec<u16>> {
        let mut units: Vec<u16> = name.encode_utf16().collect();
        units.push(0x0000);
        while units.len() % 13 != 0 {
            units.push(0xFFFF);
        }
        units.chunks(13).map(|c| c.to_vec()).collect()
    }

    #[test]
    fn reader_finds_83_file_and_chain() {
        let mut vol = Vol::new();
        let first = vol.alloc(3);
        vol.put_entry(
            vol.p.root_cluster,
            1, // slot 0 is the volume label entry
            &Vol::short_entry(b"LDLINUX SYS", 0x20, first, 1500),
        );
        let mut cur = Cursor::new(&mut vol.buf[..]);
        let v = FatVolume::open(&mut cur).unwrap();
        let e = v.find(&mut cur, "ldlinux.sys").unwrap().unwrap();
        assert_eq!(e.first_cluster, first);
        assert_eq!(e.size, 1500);
        let lbas = v.file_lbas(&mut cur, e.first_cluster, e.size).unwrap();
        assert_eq!(lbas.len(), 3); // 1500 bytes -> 3 sectors
        assert_eq!(lbas[0], v.cluster_offset(first) / 512);
        assert_eq!(lbas[1], lbas[0] + 1);
        assert_eq!(lbas[2], lbas[0] + 2);
        assert!(v.find(&mut cur, "missing.sys").unwrap().is_none());
    }

    #[test]
    fn reader_walks_lfn_subdirs() {
        let mut vol = Vol::new();
        // /boot (short name)
        let boot_c = vol.alloc(1);
        vol.put_entry(
            vol.p.root_cluster,
            1,
            &Vol::short_entry(b"BOOT       ", ATTR_DIRECTORY, boot_c, 0),
        );
        // /boot/i386-pc — LFN over an alias short name
        let i386_c = vol.alloc(1);
        let alias = b"I386~1     ";
        let sum = lfn_checksum(alias);
        let units = lfn_units("i386-pc");
        vol.put_entry(
            boot_c,
            0,
            &Vol::lfn_entry(0x40 | units.len() as u8, &units[0], sum),
        );
        vol.put_entry(
            boot_c,
            1,
            &Vol::short_entry(alias, ATTR_DIRECTORY, i386_c, 0),
        );
        // /boot/i386-pc/core.img — 8.3 short name, contiguous chain of 86
        // clusters (44_000 bytes at 1 sector/cluster)
        let core_c = vol.alloc(86);
        vol.put_entry(
            i386_c,
            0,
            &Vol::short_entry(b"CORE    IMG", 0x20, core_c, 44_000),
        );
        let mut cur = Cursor::new(&mut vol.buf[..]);
        let v = FatVolume::open(&mut cur).unwrap();
        let e = v
            .find(&mut cur, "boot/i386-pc/core.img")
            .unwrap()
            .expect("core.img must be found through the LFN subdir");
        assert_eq!(e.first_cluster, core_c);
        let lbas = v.file_lbas(&mut cur, core_c, 44_000).unwrap();
        assert_eq!(lbas.len(), 44_000u64.div_ceil(512) as usize);
        assert!(lbas.windows(2).all(|w| w[1] == w[0] + 1));
        // case-insensitive 8.3 lookup
        assert!(v.find(&mut cur, "BOOT/I386-PC/core.img").unwrap().is_some());
    }

    #[test]
    fn reader_rejects_looping_chain() {
        let mut vol = Vol::new();
        let first = vol.alloc(3);
        // point the last cluster back at the first: loop
        vol.set_fat(first + 2, first);
        vol.put_entry(
            vol.p.root_cluster,
            1,
            &Vol::short_entry(b"LOOP    SYS", 0x20, first, 10_000),
        );
        let mut cur = Cursor::new(&mut vol.buf[..]);
        let v = FatVolume::open(&mut cur).unwrap();
        assert!(matches!(
            v.chain(&mut cur, first),
            Err(Error::BadFat32("cluster chain loop"))
        ));
        // find() only walks directories; the loop bites when the file's
        // cluster chain is followed
        let e = v.find(&mut cur, "loop.sys").unwrap().unwrap();
        assert!(v.file_lbas(&mut cur, e.first_cluster, e.size).is_err());
    }
}
