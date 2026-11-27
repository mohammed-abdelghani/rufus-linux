//! ISO9660 reader: volume descriptors, directory records, Rock Ridge names,
//! El Torito boot catalog parsing.

use crate::error::{Error, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const SECTOR: u64 = 2048;

#[derive(Debug, Clone)]
pub struct IsoEntry {
    /// full path inside ISO, e.g. "isolinux/isolinux.bin"
    pub path: String,
    pub is_dir: bool,
    pub extent_lba: u64,
    pub size: u64,
    /// Unix mtime decoded from the ECMA-119 recording date — consumed by the
    /// "Preserve timestamps" hidden setting (Rufus parity)
    pub mtime: i64,
}

#[derive(Debug)]
pub struct IsoImage {
    pub volume_id: String,
    pub logical_block_size: u32,
    pub path_table_lba: u32,
    pub entries: Vec<IsoEntry>,
    pub boot_catalog_lba: Option<u32>,
    pub boot_images: Vec<BootImage>,
}

#[derive(Debug, Clone)]
pub struct BootImage {
    pub platform: u8, // 0=x86, 1=PPC, 2=Mac, 0xEF=UEFI
    pub bootable: bool,
    pub boot_media: u8, // 0=no emulation, 1=1.2M, 2=1.44M, 3=2.88M, 4=HD
    pub load_segment: u16,
    pub system_type: u8,
    /// virtual size in 512-byte sectors; 0 = whole image file
    pub sector_count: u16,
    pub image_lba: u32,
}

fn both32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn both16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

pub fn read_sector_at<R: Read + Seek>(r: &mut R, lba: u64, out: &mut [u8]) -> Result<()> {
    r.seek(SeekFrom::Start(lba * SECTOR))?;
    r.read_exact(out)?;
    Ok(())
}

pub struct DirRecord<'a> {
    pub len: usize,
    pub extent_lba: u64,
    pub size: u64,
    pub flags: u8,
    pub name: &'a [u8],
    pub system_use: &'a [u8],
    /// 7-byte ECMA-119 9.1.5 recording date: years since 1900, month, day,
    /// hour, minute, second, signed 15-minute timezone offset
    pub rec_date: [u8; 7],
}

pub fn parse_dir_record(buf: &[u8]) -> Result<Option<DirRecord<'_>>> {
    if buf.len() < 34 || buf[0] == 0 {
        return Ok(None);
    }
    let len = buf[0] as usize;
    if len < 34 || len > buf.len() {
        return Ok(None);
    }
    let extent_lba = both32(&buf[2..10]) as u64;
    let size = both32(&buf[10..18]) as u64;
    let flags = buf[25];
    let name_len = buf[32] as usize;
    let name_end = 33 + name_len;
    if name_end > len {
        return Err(Error::TruncatedRecord);
    }
    let name = &buf[33..name_end];
    // the identifier length already includes the ";1" version suffix; records
    // must be even-length, so a pad byte follows an odd-end identifier before
    // the system use area (ECMA-119 9.1.6)
    let su_start = (name_end + 1) & !1;
    if su_start > len {
        return Err(Error::TruncatedRecord);
    }
    Ok(Some(DirRecord {
        len,
        extent_lba,
        size,
        flags,
        name,
        system_use: &buf[su_start..len],
        rec_date: [
            buf[18], buf[19], buf[20], buf[21], buf[22], buf[23], buf[24],
        ],
    }))
}

/// Decode an ECMA-119 9.1.5 recording date (as stored in `DirRecord::rec_date`)
/// into Unix seconds.
pub fn iso_date_to_unix(d: &[u8; 7]) -> i64 {
    let year = 1900 + d[0] as i64;
    let month = (d[1] as i64).clamp(1, 12);
    let day = (d[2] as i64).clamp(1, 31);
    let secs_of_day = d[3] as i64 * 3600 + d[4] as i64 * 60 + d[5] as i64;
    // signed 15-minute offset from GMT — negative means west of Greenwich
    let tz_min = d[6] as i8 as i64 * 15;
    days_from_civil(year, month, day) * 86400 + secs_of_day - tz_min * 60
}

/// Days since 1970-01-01 for a proleptic Gregorian civil date
/// (Howard Hinnant's days_from_civil — avoids a calendar dependency).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Extract Rock Ridge NM entries -> display name.
/// SUSP entry layout: signature(2 bytes), length(1), version(1), payload...
pub fn rock_ridge_name(su: &[u8]) -> Option<String> {
    let mut name = String::new();
    let mut collecting = false;
    let mut i = 0usize;
    while i + 4 <= su.len() {
        let l = su[i + 2] as usize;
        if l < 4 || i + l > su.len() {
            break;
        }
        if &su[i..i + 2] == b"NM" {
            // RRIP: NM flags bit 0 = name continues in the next NM entry
            let cont = su[i + 4] & 0x01 != 0;
            if !collecting {
                name.clear();
                collecting = true;
            }
            name.push_str(&String::from_utf8_lossy(&su[i + 5..i + l]));
            if !cont {
                return Some(name);
            }
        } else if collecting {
            // NM sequence interrupted by another entry type
            collecting = false;
        }
        i += l;
    }
    if collecting {
        Some(name)
    } else {
        None
    }
}

/// Decode ISO9660 file identifier to a name.
pub fn iso_name_to_string(raw: &[u8]) -> String {
    if raw.len() == 1 && raw[0] == 0 {
        return ".".into();
    }
    if raw.len() == 1 && raw[0] == 1 {
        return "..".into();
    }
    let s = String::from_utf8_lossy(raw);
    s.split(';').next().unwrap_or("").to_string()
}

/// Parse PVD + walk directory tree + parse El Torito catalog.
pub fn parse_iso<R: Read + Seek>(r: &mut R) -> Result<IsoImage> {
    let mut sec = vec![0u8; SECTOR as usize];
    let mut pvd: Option<Vec<u8>> = None;
    for lba in 16..=19u64 {
        read_sector_at(r, lba, &mut sec)?;
        if &sec[1..6] == b"CD001" && sec[0] == 1 {
            pvd = Some(sec.clone());
            break;
        }
    }
    let pvd = pvd.ok_or(Error::NotAnIso)?;
    let volume_id = String::from_utf8_lossy(&pvd[40..72]).trim().to_string();
    let logical_block_size = u16::from_le_bytes([pvd[128], pvd[129]]) as u32;
    let path_table_lba = both32(&pvd[132..140]);
    let root_extent = both32(&pvd[158..166]) as u64;
    let root_size = both32(&pvd[166..174]) as u64;

    let mut entries = Vec::new();
    let mut stack: Vec<(u64, u64, String)> = vec![(root_extent, root_size, String::new())];
    let mut visited = 0usize;
    while let Some((lba, size, prefix)) = stack.pop() {
        visited += 1;
        if visited > 300_000 {
            return Err(Error::TooManyEntries);
        }
        let nsec = size.div_ceil(SECTOR) as usize;
        let mut data = vec![0u8; nsec * SECTOR as usize];
        r.seek(SeekFrom::Start(lba * SECTOR))?;
        r.read_exact(&mut data)?;
        let mut off = 0usize;
        while off < data.len() {
            let rec = match parse_dir_record(&data[off..]) {
                Ok(Some(r_)) => r_,
                _ => break,
            };
            off += rec.len;
            if rec.name.len() == 1 && (rec.name[0] == 0 || rec.name[0] == 1) {
                continue; // . and ..
            }
            let iso_name = iso_name_to_string(rec.name);
            let display = rock_ridge_name(rec.system_use).unwrap_or_else(|| iso_name.clone());
            let full = if prefix.is_empty() {
                display.clone()
            } else {
                format!("{prefix}/{display}")
            };
            let is_dir = rec.flags & 0x02 != 0;
            entries.push(IsoEntry {
                path: full.clone(),
                is_dir,
                extent_lba: rec.extent_lba,
                size: rec.size,
                mtime: iso_date_to_unix(&rec.rec_date),
            });
            if is_dir {
                stack.push((rec.extent_lba, rec.size, full));
            }
        }
    }

    // El Torito: boot record VD is sector 17
    let mut boot_catalog_lba = None;
    let mut boot_images = Vec::new();
    if let Ok(()) = read_sector_at(r, 17, &mut sec) {
        if &sec[1..6] == b"CD001" && sec[0] == 0 {
            let cat_lba = both32(&sec[0x47..0x4F]) as u32;
            if cat_lba != 0 {
                boot_catalog_lba = Some(cat_lba);
                let mut cat = vec![0u8; SECTOR as usize];
                if read_sector_at(r, cat_lba as u64, &mut cat).is_ok() {
                    // header at 0, validation entry; entries start at offset 32
                    let mut platform = cat[1];
                    let mut off = 32usize;
                    while off + 32 <= cat.len() {
                        let e = &cat[off..off + 32];
                        if e[0] == 0x00 && e[1..4] == [0, 0, 0] {
                            break;
                        }
                        if e[0] == 0x91 {
                            // section header: platform@1, count@2-3
                            platform = e[1];
                            off += 32;
                            continue;
                        }
                        if e[0] == 0x88 || e[0] == 0x00 {
                            let vsize = both16(&e[6..8]);
                            let lba = both32(&e[8..12]);
                            boot_images.push(BootImage {
                                platform,
                                bootable: e[0] == 0x88,
                                boot_media: e[1],
                                load_segment: both16(&e[2..4]),
                                system_type: e[4],
                                sector_count: vsize,
                                image_lba: lba,
                            });
                        }
                        off += 32;
                    }
                }
            }
        }
    }

    Ok(IsoImage {
        volume_id,
        logical_block_size,
        path_table_lba,
        entries,
        boot_catalog_lba,
        boot_images,
    })
}

/// Extract a single file's contents from the ISO to `out_path`.
pub fn extract_file<R: Read + Seek>(r: &mut R, entry: &IsoEntry, out_path: &Path) -> Result<u64> {
    if entry.is_dir {
        return Ok(0);
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(out_path)?;
    read_file(r, entry, &mut f)?;
    Ok(entry.size)
}

/// Stream an ISO entry into an arbitrary writer (in-memory BCD patches etc).
pub fn read_file<R: Read + Seek, W: Write>(r: &mut R, entry: &IsoEntry, out: &mut W) -> Result<()> {
    if entry.is_dir {
        return Ok(());
    }
    r.seek(SeekFrom::Start(entry.extent_lba * SECTOR))?;
    let mut remaining = entry.size;
    let mut buf = vec![0u8; 1 << 20];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        match r.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                out.write_all(&buf[..n])?;
                remaining -= n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_record(name: &[u8], extent: u32, size: u32, flags: u8, su: &[u8]) -> Vec<u8> {
        // model the ECMA-119 pad byte: odd-end identifier gets one zero byte
        // before the system use area
        let name_end = 33 + name.len();
        let pad = ((name_end + 1) & !1) - name_end;
        let len = name_end + pad + su.len();
        let mut buf = vec![0u8; len];
        buf[0] = len as u8;
        buf[2..6].copy_from_slice(&extent.to_le_bytes()); // both-endian LE half
        buf[10..14].copy_from_slice(&size.to_le_bytes());
        buf[25] = flags;
        buf[32] = name.len() as u8;
        buf[33..name_end].copy_from_slice(name);
        buf[name_end + pad..].copy_from_slice(su);
        buf
    }

    #[test]
    fn dir_record_fields() {
        // identifier length INCLUDES the ";1" version suffix
        let buf = file_record(b"BOOT.CAT;1", 37, 2048, 0, &[]);
        let rec = parse_dir_record(&buf).unwrap().unwrap();
        assert_eq!(rec.len, buf.len());
        assert_eq!(rec.extent_lba, 37);
        assert_eq!(rec.size, 2048);
        assert_eq!(rec.flags & 0x02, 0); // not a directory
        assert_eq!(iso_name_to_string(rec.name), "BOOT.CAT");
        assert!(rec.system_use.is_empty());
    }

    #[test]
    fn dir_record_directory_flag() {
        let buf = file_record(b"BOOT;1", 100, 2048, 0x02, &[]);
        let rec = parse_dir_record(&buf).unwrap().unwrap();
        assert_eq!(rec.flags & 0x02, 0x02);
    }

    #[test]
    fn iso_date_decoding() {
        // 2024-01-01 00:00:00 GMT (124 years after 1900)
        assert_eq!(iso_date_to_unix(&[124, 1, 1, 0, 0, 0, 0]), 1704067200);
        // the epoch itself
        assert_eq!(iso_date_to_unix(&[70, 1, 1, 0, 0, 0, 0]), 0);
        // 2000-03-01 12:34:56 stored with a +02:00 offset (8 * 15 min):
        // Unix time is UTC, so the stored local time shifts back 2 hours
        assert_eq!(iso_date_to_unix(&[100, 3, 1, 12, 34, 56, 8]), 951906896);
        // a west-of-Greenwich offset (-8 * 15 min = -2 h) shifts forward:
        // 2024-01-01 00:30 local at -02:00 = 2024-01-01 02:30 UTC
        assert_eq!(iso_date_to_unix(&[124, 1, 1, 0, 30, 0, 248]), 1704076200);
        // out-of-range month/day are clamped instead of panicking
        assert_eq!(iso_date_to_unix(&[124, 0, 0, 0, 0, 0, 0]), 1704067200);
    }

    #[test]
    fn dir_record_carries_recording_date() {
        let mut buf = file_record(b"TEST.;1", 10, 5, 0, &[]);
        // ECMA-119 9.1.5: 2024-06-15 10:20:30 GMT
        buf[18..25].copy_from_slice(&[124, 6, 15, 10, 20, 30, 0]);
        let rec = parse_dir_record(&buf).unwrap().unwrap();
        assert_eq!(rec.rec_date, [124, 6, 15, 10, 20, 30, 0]);
        assert_eq!(iso_date_to_unix(&rec.rec_date), 1718446830);
    }

    #[test]
    fn dir_record_dot_entries() {
        let mut dot = vec![0u8; 34];
        dot[0] = 34;
        dot[32] = 1;
        dot[33] = 0;
        assert_eq!(
            iso_name_to_string(parse_dir_record(&dot).unwrap().unwrap().name),
            "."
        );
        dot[33] = 1;
        assert_eq!(
            iso_name_to_string(parse_dir_record(&dot).unwrap().unwrap().name),
            ".."
        );
    }

    #[test]
    fn dir_record_zero_len_ends_scan() {
        assert!(parse_dir_record(&[0u8; 8]).unwrap().is_none());
        // truncated record is an error, not silent None: a 40-byte record whose
        // 10-byte identifier runs past the record length
        let mut buf = vec![0u8; 40];
        buf[0] = 40;
        buf[32] = 10;
        assert!(parse_dir_record(&buf).is_err());
    }

    #[test]
    fn dir_record_rr_names_after_version_suffix() {
        // NM system use entry directly after "README.;1" (9-byte identifier,
        // no pad: 33+9 is even); SUSP order: sig(2), len, ver, flags, name
        let su = [b'N', b'M', 11u8, 1, 0, b'R', b'E', b'A', b'D', b'M', b'E'];
        let buf = file_record(b"README.;1", 42, 100, 0, &su);
        let rec = parse_dir_record(&buf).unwrap().unwrap();
        assert_eq!(rec.system_use, &su[..]);
        assert_eq!(rock_ridge_name(rec.system_use).as_deref(), Some("README"));
    }

    #[test]
    fn dir_record_rr_names_after_pad_byte() {
        // "BOOT" (4-byte identifier): 33+4 is odd -> one pad byte, then RR/NM
        // entries in SUSP order, exactly as genisoimage writes them
        let mut su = vec![b'R', b'R', 5u8, 1, 0x81];
        su.extend_from_slice(&[b'N', b'M', 9u8, 1, 0, b'b', b'o', b'o', b't']);
        let buf = file_record(b"BOOT", 30, 2048, 0x02, &su);
        let rec = parse_dir_record(&buf).unwrap().unwrap();
        assert_eq!(rec.system_use, &su[..]);
        assert_eq!(rock_ridge_name(rec.system_use).as_deref(), Some("boot"));
        assert_eq!(iso_name_to_string(rec.name), "BOOT"); // ISO fallback name
    }

    #[test]
    fn rock_ridge_nm_single_entry() {
        let su = [b'N', b'M', 11u8, 1, 0, b'R', b'E', b'A', b'D', b'M', b'E'];
        assert_eq!(rock_ridge_name(&su).as_deref(), Some("README"));
    }

    #[test]
    fn rock_ridge_nm_continuation_flag() {
        // RRIP bit 0 (0x01) = name continues in the next NM entry
        let su = [
            b'N', b'M', 10u8, 1, 0x01, b'l', b'o', b'n', b'g', b'-', b'N', b'M', 9u8, 1, 0x00,
            b'n', b'a', b'm', b'e',
        ];
        assert_eq!(rock_ridge_name(&su).as_deref(), Some("long-name"));
    }

    #[test]
    fn rock_ridge_genisoimage_layout() {
        // bytes lifted from test.iso: RR then NM("boot") then PX entries
        let su = [
            b'R', b'R', 5u8, 1, 0x89, b'N', b'M', 9u8, 1, 0, b'b', b'o', b'o', b't', b'P', b'X',
            44u8, 1,
        ];
        let su: Vec<u8> = su
            .into_iter()
            .chain(std::iter::repeat(0u8).take(40))
            .collect();
        assert_eq!(rock_ridge_name(&su).as_deref(), Some("boot"));
    }

    #[test]
    fn rock_ridge_no_nm_entry() {
        // PX entry (mode/links/...), no NM anywhere
        let su = vec![b'P', b'X', 36u8, 1];
        let su = su
            .into_iter()
            .chain(std::iter::repeat(0u8).take(32))
            .collect::<Vec<u8>>();
        assert_eq!(rock_ridge_name(&su), None);
        assert_eq!(rock_ridge_name(&[]), None);
    }

    #[test]
    fn iso_names() {
        assert_eq!(iso_name_to_string(b"README.;1"), "README.");
        assert_eq!(iso_name_to_string(b"DATA.BIN"), "DATA.BIN");
        assert_eq!(iso_name_to_string(&[0]), ".");
        assert_eq!(iso_name_to_string(&[1]), "..");
    }

    #[test]
    fn parse_real_test_iso_uses_rr_names() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/test.iso");
        let Ok(mut f) = std::fs::File::open(path) else {
            return; // fixture not present; skip
        };
        let iso = parse_iso(&mut f).unwrap();
        println!("entries:");
        for e in &iso.entries {
            println!("  {:?}", e.path);
        }
        assert!(
            iso.entries.iter().any(|e| e.path == "boot/core.gz"),
            "expected Rock Ridge lowercase names (directories included)"
        );
    }

    /// Recording date bytes for 2026-01-01 00:00:00 UTC (years since 1900).
    const TEST_DATE: [u8; 7] = [126, 1, 1, 0, 0, 0, 0];
    /// Independently known epoch value for 2026-01-01T00:00:00Z.
    const TEST_DATE_UNIX: i64 = 1_767_225_600;

    /// ECMA-119 9.1 directory record: 34-byte header + name + system use.
    /// `name` is the raw ISO identifier; `su` the Rock Ridge system-use
    /// area. The even-boundary pad byte (ECMA-119 9.1.6) is inserted
    /// automatically between an odd-length identifier and `su`.
    fn test_dir_record(extent: u32, size: u32, flags: u8, name: &[u8], su: &[u8]) -> Vec<u8> {
        let name_end = 33 + name.len();
        let su_start = (name_end + 1) & !1;
        let mut r = vec![0u8; su_start + su.len()];
        r[0] = r.len() as u8;
        r[2..6].copy_from_slice(&extent.to_le_bytes());
        r[6..10].copy_from_slice(&extent.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[18..25].copy_from_slice(&TEST_DATE);
        r[25] = flags;
        r[28..30].copy_from_slice(&1u16.to_le_bytes());
        r[30..32].copy_from_slice(&1u16.to_be_bytes());
        r[32] = name.len() as u8;
        r[33..33 + name.len()].copy_from_slice(name);
        r[su_start..].copy_from_slice(su);
        r
    }

    /// One RRIP "NM" entry (signature, length, version, flags, name),
    /// padded to an even byte count.
    fn test_nm_entry(name: &str) -> Vec<u8> {
        let mut e = vec![b'N', b'M', (5 + name.len()) as u8, 1, 0];
        e.extend_from_slice(name.as_bytes());
        if e.len() % 2 != 0 {
            e.push(0);
        }
        e
    }

    /// Hand-built minimal ECMA-119 image (PVD + root dir + subdir + two
    /// files) read back through parse_iso/read_file/extract_file. Every
    /// expected value below comes from the bytes written here or from the
    /// ECMA-119 structure, never from the program's own output.
    #[test]
    fn parse_iso_generated_minimal_image_roundtrip() {
        const SEC: usize = 2048;

        let mut pvd = vec![0u8; SEC];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        // volume identifier, space-padded per ECMA-119 8.4.4
        pvd[40..47].copy_from_slice(b"TESTISO");
        for b in &mut pvd[47..72] {
            *b = b' ';
        }
        pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());
        // root directory record (34 bytes): sector 17, one sector long
        let root = test_dir_record(17, SEC as u32, 0x02, b"\x00", &[]);
        assert_eq!(root.len(), 34);
        pvd[156..190].copy_from_slice(&root);

        const README: &[u8] = b"hello rufus\n";
        const DATA: [u8; 5] = [0, 1, 2, 3, 4];

        let mut root_dir = Vec::new();
        root_dir.extend(test_dir_record(17, SEC as u32, 0x02, b"\x00", &[]));
        root_dir.extend(test_dir_record(17, SEC as u32, 0x02, b"\x01", &[]));
        root_dir.extend(test_dir_record(
            19,
            12,
            0,
            b"README.;1",
            &test_nm_entry("readme.txt"),
        ));
        root_dir.extend(test_dir_record(
            18,
            SEC as u32,
            0x02,
            b"SUB.;1",
            &test_nm_entry("sub"),
        ));

        let mut sub_dir = Vec::new();
        sub_dir.extend(test_dir_record(18, SEC as u32, 0x02, b"\x00", &[]));
        sub_dir.extend(test_dir_record(18, SEC as u32, 0x02, b"\x01", &[]));
        sub_dir.extend(test_dir_record(
            20,
            5,
            0,
            b"DATA.;1",
            &test_nm_entry("data.bin"),
        ));

        let mut img = vec![0u8; 21 * SEC];
        img[16 * SEC..17 * SEC].copy_from_slice(&pvd);
        img[17 * SEC..18 * SEC][..root_dir.len()].copy_from_slice(&root_dir);
        img[18 * SEC..19 * SEC][..sub_dir.len()].copy_from_slice(&sub_dir);
        img[19 * SEC..19 * SEC + README.len()].copy_from_slice(README);
        img[20 * SEC..20 * SEC + DATA.len()].copy_from_slice(&DATA);

        let mut cur = std::io::Cursor::new(img);
        let iso = parse_iso(&mut cur).unwrap();
        assert_eq!(iso.volume_id, "TESTISO");
        assert_eq!(iso.logical_block_size, 2048);
        // No El Torito boot descriptor in this image
        assert_eq!(iso.boot_catalog_lba, None);
        assert_eq!(iso.entries.len(), 3);
        let readme = iso
            .entries
            .iter()
            .find(|e| e.path == "readme.txt")
            .expect("readme.txt entry");
        assert!(!readme.is_dir);
        assert_eq!(readme.extent_lba, 19);
        assert_eq!(readme.size, 12);
        assert_eq!(readme.mtime, TEST_DATE_UNIX);
        let sub = iso
            .entries
            .iter()
            .find(|e| e.path == "sub")
            .expect("sub entry");
        assert!(sub.is_dir);
        assert_eq!(sub.extent_lba, 18);
        let data = iso
            .entries
            .iter()
            .find(|e| e.path == "sub/data.bin")
            .expect("sub/data.bin entry");
        assert_eq!(data.extent_lba, 20);
        assert_eq!(data.size, 5);
        assert_eq!(data.mtime, TEST_DATE_UNIX);

        let mut out = Vec::new();
        read_file(&mut cur, readme, &mut out).unwrap();
        assert_eq!(out, README);
        let tmp = std::env::temp_dir().join(format!("rufus-linux-test-iso-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let dest = tmp.join("extracted-readme.txt");
        assert_eq!(extract_file(&mut cur, readme, &dest).unwrap(), 12);
        assert_eq!(std::fs::read(&dest).unwrap(), README);
        let _ = std::fs::remove_dir_all(&tmp);
        let mut sec = [0u8; SEC];
        read_sector_at(&mut cur, 16, &mut sec).unwrap();
        assert_eq!(&sec[1..6], b"CD001");
    }
}
