//! Raw block device access: open, read/write at offsets, ioctls.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

pub const SECTOR: u64 = 512;

// ioctl request codes. BLKGETSIZE64/BLKSSZGET/BLKRRPART follow the
// _IO(...) encoding with type 0x12; HDIO_GETGEO uses the old-style
// encoding (differs only on mips-like ports).
const BLKGETSIZE64: u64 = 0x8008_1272; // _IOR(0x12, 114, u64)
const BLKRRPART: u64 = 0x125F; // _IO(0x12, 95)
const HDIO_GETGEO: u64 = 0x0301;

pub struct Disk {
    pub file: File,
    pub size: u64,
    pub sector_size: u64,
}

// Mirror of the UAPI `struct hd_geometry` (linux/hdreg.h): heads, sectors,
// cylinders, then `unsigned long start`. On LP64 `start` is 8 bytes and the
// struct is 16 bytes (with 4 bytes of padding after `cylinders`); on 32-bit
// it is 8 bytes with `start` at offset 4. `#[repr(C)]` reproduces that
// layout exactly, which the layout test below checks.
#[repr(C)]
struct HdGeometry {
    heads: u8,
    sectors: u8,
    cylinders: u16,
    start: libc::c_ulong,
}

impl Disk {
    /// Open a whole-disk device for raw read/write. Requires root/CAP_SYS_RAWIO.
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open(path)
            .with_context(|| format!("cannot open {path} (need root)"))?;
        let fd = file.as_raw_fd();
        let mut size: u64 = 0;
        // SAFETY: `fd` is the valid descriptor of the device file opened
        // above; BLKGETSIZE64 is _IOR(0x12, 114, size_t) (linux/fs.h), so
        // the kernel writes exactly one `size_t` (= u64 on LP64) through
        // the pointer, and `&mut size` is a valid, aligned `u64`.
        unsafe {
            if libc::ioctl(fd, BLKGETSIZE64 as libc::Ioctl, &mut size) != 0 {
                // fallback: seek to end
                let mut f = File::open(path)?;
                size = f.seek(SeekFrom::End(0))?;
            }
        }
        let sector_size = Self::get_sector_size(&file).unwrap_or(SECTOR);
        Ok(Disk {
            file,
            size,
            sector_size,
        })
    }

    fn get_sector_size(file: &File) -> Option<u64> {
        // BLKSSZGET = 0x1268
        let mut sz: libc::c_int = 0;
        // SAFETY: `file` is an open `File`, so its raw fd is valid for the
        // duration of the call; BLKSSZGET is _IO(0x12, 104) (linux/fs.h),
        // which writes a single C `int` (the logical sector size) through
        // the pointer, and `&mut sz` is a valid, aligned `c_int`.
        let r = unsafe { libc::ioctl(file.as_raw_fd(), 0x1268, &mut sz) };
        if r == 0 && sz > 0 {
            Some(sz as u64)
        } else {
            None
        }
    }

    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }

    pub fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        self.file.flush()?;
        Ok(())
    }

    /// Re-read partition table from kernel after we wrote it.
    pub fn reread_partitions(&self) -> Result<()> {
        // SAFETY: `self.file` is an open `File`, so its raw fd is valid;
        // BLKRRPART is _IO(0x12, 95) (linux/fs.h) and takes no pointer —
        // the kernel dereferences nothing, so beyond the valid fd no
        // memory invariant applies.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), BLKRRPART as libc::Ioctl, 0) };
        if r != 0 {
            let e = std::io::Error::last_os_error();
            // regular image files are not block devices — nothing to re-read
            if e.raw_os_error() == Some(libc::ENOTTY) {
                return Ok(());
            }
            bail!("BLKRRPART failed: {e} (partitions may be busy)");
        }
        Ok(())
    }

    /// Erase partition table: wipe first MiB + last MiB signatures.
    pub fn wipe_signatures(&mut self) -> Result<()> {
        const MIB: u64 = 1024 * 1024;
        let zeros = vec![0u8; MIB as usize];
        self.write_at(0, &zeros)?;
        if self.size > 2 * MIB {
            self.write_at(self.size - MIB, &zeros)?;
        }
        Ok(())
    }

    /// Get CHS geometry (may fail on modern devices; returns defaults).
    pub fn geometry(&self) -> (u16, u8, u8) {
        let mut g = HdGeometry {
            heads: 0,
            sectors: 0,
            cylinders: 0,
            start: 0,
        };
        // SAFETY: `HdGeometry` above has the same layout as the kernel's
        // `struct hd_geometry` (linux/hdreg.h): 16 bytes on LP64 with
        // `start` (`unsigned long`) at offset 8, 8 bytes on 32-bit with
        // `start` at offset 4 — verified by hd_geometry_layout below.
        // `&mut g` is therefore a valid, writable, properly aligned local
        // of exactly the size HDIO_GETGEO (0x0301) writes through its
        // pointer, and `self.file`'s raw fd is valid for the call.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), HDIO_GETGEO as libc::Ioctl, &mut g) };
        if r == 0 && g.heads > 0 {
            (g.cylinders, g.heads, g.sectors)
        } else {
            // LBA-assist defaults
            (0, 255, 63)
        }
    }
}

/// Open a partition device (e.g. /dev/sdb1) — thin wrapper.
pub fn open_partition(path: &str) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_SYNC)
        .open(path)
        .with_context(|| format!("cannot open {path}"))
}

/// Result of parsing a partition table from an image or device.
#[derive(Debug)]
pub enum TableInfo {
    Mbr(Vec<crate::mbr::Partition>),
    Gpt(Vec<crate::gpt::GptPartition>),
    /// no 0x55AA signature or no partitions
    Empty,
}

/// Read and parse the partition table of an image or device (read-only).
pub fn inspect_table<R: Read + Seek>(r: &mut R) -> Result<TableInfo> {
    use crate::gpt;
    use crate::mbr::{detect_style, PartitionStyle};

    let mut sec = [0u8; 512];
    r.rewind()?;
    r.read_exact(&mut sec)?;
    match detect_style(&sec) {
        PartitionStyle::Gpt => {
            let mut hdr = [0u8; 512];
            r.seek(SeekFrom::Start(SECTOR))?;
            r.read_exact(&mut hdr)?;
            let entry_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
            let num_entries = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
            let entry_size = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
            if entry_size == 0 || entry_size > 512 {
                bail!("bad GPT entry size {entry_size}");
            }
            let nsec = (num_entries * entry_size).div_ceil(512);
            let mut buf = vec![0u8; nsec * 512];
            r.seek(SeekFrom::Start(entry_lba * SECTOR))?;
            r.read_exact(&mut buf)?;
            let chunks: Vec<[u8; 512]> = buf.as_chunks::<512>().0.to_vec();
            Ok(TableInfo::Gpt(gpt::parse_gpt(&hdr, &chunks)?))
        }
        PartitionStyle::Mbr(parts) => Ok(TableInfo::Mbr(parts)),
        PartitionStyle::Empty => Ok(TableInfo::Empty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpt::{build_gpt_single, Guid};
    use crate::mbr::{build_mbr, FsType, Partition};
    use std::io::Cursor;

    #[test]
    fn inspect_table_mbr_gpt_empty() {
        // MBR image
        let mbr = build_mbr(
            &[Partition {
                bootable: true,
                fs: FsType::Fat32Lba,
                start_lba: 2048,
                sectors: 1000,
            }],
            None,
            7,
            255,
            63,
        );
        let mut cur = Cursor::new(mbr.to_vec());
        match inspect_table(&mut cur).unwrap() {
            TableInfo::Mbr(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].fs, FsType::Fat32Lba);
            }
            _ => panic!("expected MBR"),
        }
        // GPT image: protective MBR + header + entries at the right LBAs
        let disk_sectors = 16_777_216u64;
        let (mbr, hdr, entries) =
            build_gpt_single(disk_sectors, 2048, disk_sectors - 2049, Guid([9u8; 16])).unwrap();
        let mut img = vec![0u8; disk_sectors as usize * 512];
        img[..512].copy_from_slice(&mbr);
        img[512..1024].copy_from_slice(&hdr);
        for (i, s) in entries.iter().enumerate() {
            let off = (2 + i) * 512;
            img[off..off + 512].copy_from_slice(s);
        }
        let mut cur = Cursor::new(img);
        match inspect_table(&mut cur).unwrap() {
            TableInfo::Gpt(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].first_lba, 2048);
                assert_eq!(parts[0].name, "Rufus Linux");
            }
            _ => panic!("expected GPT"),
        }
        // empty image
        let mut cur = Cursor::new(vec![0u8; 512]);
        assert!(matches!(inspect_table(&mut cur).unwrap(), TableInfo::Empty));
    }

    /// The HDIO_GETGEO buffer must mirror the kernel's `struct hd_geometry`
    /// exactly (linux/hdreg.h): heads, sectors, cylinders, then
    /// `unsigned long start`. The expected sizes/offsets below follow from
    /// the kernel header's field types, not from this program's output:
    /// on LP64 `start` is 8 bytes, so the struct is 16 bytes with `start`
    /// at offset 8; on 32-bit targets it is 8 bytes with `start` at
    /// offset 4.
    #[test]
    fn hd_geometry_layout_matches_kernel_struct() {
        assert_eq!(
            std::mem::size_of::<libc::c_ulong>(),
            std::mem::size_of::<usize>()
        );
        if cfg!(target_pointer_width = "64") {
            assert_eq!(std::mem::size_of::<HdGeometry>(), 16);
            assert_eq!(std::mem::offset_of!(HdGeometry, start), 8);
        } else if cfg!(target_pointer_width = "32") {
            assert_eq!(std::mem::size_of::<HdGeometry>(), 8);
            assert_eq!(std::mem::offset_of!(HdGeometry, start), 4);
        }
    }
}
