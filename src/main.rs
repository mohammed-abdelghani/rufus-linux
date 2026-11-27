mod bcd;
mod bootpatch;
mod config;
mod device;
mod disk;
mod download;
mod engine;
mod error;
mod fat32;
mod gpt;
mod gui;
mod iso9660;
mod locale;
mod mbr;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "rufus-linux",
    version,
    about = "Linux rewrite of Rufus - create bootable USB drives"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List detected USB/removable block devices
    List {
        /// include fixed/internal drives too
        #[arg(long)]
        all: bool,
    },
    /// Write an image to a device (ISO contents mode or raw DD mode)
    Write {
        /// device path, e.g. /dev/sdb
        device: String,
        /// image file (iso/img/bin/gz/wim) — not needed for --mode non-bootable
        image: Option<PathBuf>,
        /// filesystem label
        #[arg(long, default_value = "RUFUS")]
        label: String,
        /// GPT instead of MBR
        #[arg(long)]
        gpt: bool,
        /// quick format (skips zeroing the whole partition)
        #[arg(long, default_value_t = true)]
        quick: bool,
        /// filesystem: fat32 | ntfs | exfat
        #[arg(long, default_value = "fat32")]
        fs: String,
        /// "Check device for bad blocks": DESTRUCTIVE write test (Rufus
        /// passes selector: 0=1 pass, 1=2 passes, 2=SLC/3=MLC/4=TLC 4 passes)
        #[arg(long, value_name = "0-4")]
        badblocks: Option<u8>,
        /// UEFI-only (skip BIOS boot attempts)
        #[arg(long)]
        efi_only: bool,
        /// verify copied files (SHA-256, slower)
        #[arg(long)]
        verify: bool,
        /// create a casper-rw persistence partition (default 512 MiB)
        #[arg(long, value_name = "MIB", num_args = 0..=1, default_missing_value = "512")]
        persistent: Option<u64>,
        /// add fixes for old BIOSes (extra "BIOS Compatibility" partition,
        /// track-aligned first partition)
        #[arg(long)]
        old_bios_fixes: bool,
        /// write the Windows 7 MBR instead of the Rufus MBR
        /// (Rufus "Use Rufus MBR with BIOS ID", unchecked)
        #[arg(long)]
        no_rufus_mbr: bool,
        /// masquerade the first partition boot flag as a different BIOS disk
        /// (Rufus IDC_DISK_ID): 0x80 (default) or 0x81-0x87 (Disk 2-8)
        #[arg(long, value_name = "ID", default_value = "0x80")]
        disk_id: String,
        /// skip the extended label/icon files (autorun.inf + autorun.ico)
        #[arg(long)]
        no_autorun: bool,
        /// skip the y/N confirmation prompt
        #[arg(short, long)]
        yes: bool,
        /// allow overwriting a device that hosts the root filesystem (DANGEROUS)
        #[arg(long)]
        force: bool,
        /// write mode: auto | disk-image | non-bootable | freedos
        #[arg(long, default_value = "auto")]
        mode: String,
        /// image option for Windows ISOs: standard | wtg (Windows To Go)
        #[arg(long, default_value = "standard")]
        image_option: String,
        /// Windows To Go: 1-based index of the install.wim edition
        #[arg(long, default_value_t = 1)]
        wim_index: u32,
    },
    /// Show ISO9660 info
    IsoInfo { iso: PathBuf },
    /// Inspect the partition table of an image or device (read-only)
    DiskInfo { path: String },
    /// Launch the GUI
    Gui {
        /// UI language as a Rufus locale code (e.g. "ar-SA", "fr-FR",
        /// "zh-CN"); empty = detect from the system locale like Rufus
        #[arg(long, value_name = "CODE")]
        locale: Option<String>,
    },
    /// (debug) format a file as FAT32
    FmtFile { file: PathBuf, size_mb: u64 },
    /// (debug) build a BCD store (QEMU Windows To Go boot test helper)
    BcdGen {
        /// output file
        out: PathBuf,
        /// GPT device references (default: MBR signature + offset)
        #[arg(long)]
        gpt: bool,
        /// GPT disk GUID ("3344d1eb-..."; mixed-endian like the GPT header)
        #[arg(long, default_value = "00000000-0000-0000-0000-000000000000")]
        disk_guid: String,
        /// GPT GUID of the OS partition
        #[arg(long, default_value = "00000000-0000-0000-0000-000000000000")]
        part_guid: String,
        /// MBR disk signature
        #[arg(long, default_value_t = 0)]
        mbr_sig: u32,
        /// MBR partition byte offset
        #[arg(long, default_value_t = 0)]
        part_offset: u64,
        /// serial kernel-debug chain (COM1 handshake proves the OS booted)
        #[arg(long)]
        kernel_debug: bool,
        /// use "device boot" references instead of partitions (diagnostic)
        #[arg(long)]
        boot_device: bool,
    },
}

/// Parse "g1-g2-g3-g4-g5" GUID text into the mixed-endian GPT byte layout.
fn parse_guid(s: &str) -> Result<crate::gpt::Guid> {
    let s = s.trim().trim_matches(['{', '}']).to_lowercase();
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        anyhow::bail!("bad GUID format: {s}");
    }
    let d1 = u32::from_str_radix(parts[0], 16)?;
    let d2 = u16::from_str_radix(parts[1], 16)?;
    let d3 = u16::from_str_radix(parts[2], 16)?;
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&d1.to_le_bytes());
    g[4..6].copy_from_slice(&d2.to_le_bytes());
    g[6..8].copy_from_slice(&d3.to_le_bytes());
    let rest = format!("{}{}", parts[3], parts[4]);
    for i in 0..8 {
        g[8 + i] = u8::from_str_radix(&rest[i * 2..i * 2 + 2], 16)?;
    }
    Ok(crate::gpt::Guid(g))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        None => gui::run(None),
        Some(Cmd::Gui { locale }) => {
            let idx = locale
                .as_deref()
                .filter(|c| !c.is_empty())
                .map(|code| {
                    crate::locale::lang_index(code).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown locale '{code}' — valid codes: {}",
                            crate::locale::LANGS
                                .iter()
                                .map(|l| l.code)
                                .collect::<Vec<_>>()
                                .join(" ")
                        )
                    })
                })
                .transpose()?;
            gui::run(idx)
        }
        Some(Cmd::List { all }) => {
            let devs = device::list_devices(if all {
                device::DevFilter::All
            } else {
                device::DevFilter::Hotplug
            });
            if devs.is_empty() {
                println!("No removable/USB devices found.");
            }
            for d in devs {
                println!(
                    "{:<10} {:>10}  {:<20} {:<10} {:<6} removable={} {}",
                    d.path,
                    d.size_human(),
                    d.model,
                    d.vendor,
                    d.bus,
                    if d.removable { "yes" } else { "no" },
                    if d.serial.is_empty() {
                        String::new()
                    } else {
                        format!("SN={}", d.serial)
                    }
                );
            }
            Ok(())
        }
        Some(Cmd::IsoInfo { iso }) => {
            let mut f = std::fs::File::open(&iso)?;
            let info = iso9660::parse_iso(&mut f)?;
            println!("Volume ID : {}", info.volume_id);
            println!("Block size: {}", info.logical_block_size);
            println!("Path table: LBA {}", info.path_table_lba);
            println!("Files     : {}", info.entries.len());
            println!("Boot cat  : {:?}", info.boot_catalog_lba);
            for b in &info.boot_images {
                println!(
                    "  boot: platform={:#04x} media={} bootable={} sys_type={:#04x} lba={} sectors={} seg={:#06x}",
                    b.platform, b.boot_media, b.bootable, b.system_type, b.image_lba, b.sector_count, b.load_segment
                );
            }
            for e in info.entries.iter().filter(|e| !e.is_dir).take(30) {
                println!("  {:>10}  {}", e.size, e.path);
            }
            if info.entries.iter().filter(|e| !e.is_dir).count() > 30 {
                println!("  ...");
            }
            Ok(())
        }
        Some(Cmd::DiskInfo { path }) => {
            let mut f = std::fs::File::open(&path)?;
            match disk::inspect_table(&mut f)? {
                disk::TableInfo::Mbr(parts) => {
                    println!("Partition table: MBR");
                    for (i, p) in parts.iter().enumerate() {
                        println!(
                            "  #{} {:<14} start={:>10} sectors={:>10} ({}) boot={}",
                            i + 1,
                            p.fs.name(),
                            p.start_lba,
                            p.sectors,
                            device::human_size(p.size_bytes()),
                            if p.bootable { "yes" } else { "no" }
                        );
                    }
                }
                disk::TableInfo::Gpt(parts) => {
                    println!("Partition table: GPT");
                    for p in &parts {
                        println!(
                            "  #{} {}  LBA {}..{} ({})  \"{}\"  guid={}",
                            p.index + 1,
                            p.type_guid,
                            p.first_lba,
                            p.last_lba,
                            device::human_size(p.size_sectors() * disk::SECTOR),
                            p.name,
                            p.unique_guid
                        );
                    }
                }
                disk::TableInfo::Empty => println!("No partition table found."),
            }
            Ok(())
        }
        Some(Cmd::FmtFile { file, size_mb }) => {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&file)?;
            f.set_len(size_mb * 1024 * 1024)?;
            let mut w = std::io::BufWriter::new(f);
            fat32::format_fat32(
                &mut w,
                size_mb * 1024 * 1024,
                512,
                "TEST",
                0,
                false,
                &mut |d, t| {
                    eprintln!("fmt {}/{}", d, t);
                },
            )?;
            use std::io::Write;
            w.flush()?;
            println!("formatted {}", file.display());
            Ok(())
        }
        Some(Cmd::BcdGen {
            out,
            gpt,
            disk_guid,
            part_guid,
            mbr_sig,
            part_offset,
            kernel_debug,
            boot_device,
        }) => {
            let (boot_dev, os_dev) = if boot_device {
                let dev = crate::bcd::BcdDevice::Boot;
                (dev.clone(), dev)
            } else if gpt {
                let disk = parse_guid(&disk_guid)?;
                let part = parse_guid(&part_guid)?;
                let dev = crate::bcd::BcdDevice::Gpt {
                    disk_guid: disk,
                    partition_guid: part,
                };
                (dev.clone(), dev)
            } else {
                let dev = crate::bcd::BcdDevice::Mbr {
                    disk_signature: mbr_sig,
                    partition_offset: part_offset,
                };
                (dev.clone(), dev)
            };
            let spec = crate::bcd::BcdSpec {
                boot_device: boot_dev,
                os_device: os_dev,
                entry_guid: format!("{{{}}}", crate::engine::test_entry_guid()),
                entry_description: "Windows To Go".into(),
                loader_path: if gpt {
                    "\\Windows\\system32\\winload.efi".into()
                } else {
                    "\\Windows\\system32\\winload.exe".into()
                },
                locale: "en-US".into(),
                kernel_debug,
            };
            let bcd = crate::bcd::build_bcd(&spec)?;
            std::fs::write(&out, &bcd)?;
            println!("wrote {} ({} bytes)", out.display(), bcd.len());
            Ok(())
        }
        Some(Cmd::Write {
            device,
            image,
            label,
            gpt,
            quick,
            badblocks,
            efi_only,
            verify,
            persistent,
            old_bios_fixes,
            no_rufus_mbr,
            disk_id,
            no_autorun,
            yes,
            force,
            mode,
            fs,
            image_option,
            wim_index,
        }) => {
            let devs = device::list_devices(device::DevFilter::All);
            let dev = devs
                .iter()
                .find(|d| d.path == device)
                .cloned()
                .unwrap_or_else(|| device::Device {
                    path: device.clone(),
                    name: device.rsplit('/').next().unwrap_or(&device).to_string(),
                    size: std::fs::read_to_string(format!(
                        "/sys/block/{}/size",
                        device.rsplit('/').next().unwrap_or("")
                    ))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|v| v.saturating_mul(512))
                    .or_else(|| std::fs::metadata(&device).ok().map(|m| m.len()))
                    .unwrap_or(0),
                    model: String::new(),
                    vendor: String::new(),
                    serial: String::new(),
                    removable: true,
                    bus: "?".into(),
                    tran: "?".into(),
                });
            let write_mode = match mode.as_str() {
                "non-bootable" => engine::WriteMode::NonBootable,
                "freedos" => engine::WriteMode::FreeDos,
                "disk-image" => engine::WriteMode::DiskImage,
                _ => engine::WriteMode::DiskImage, // auto
            };
            // the "Image option" of the Rufus GUI: Windows To Go applies the
            // install.wim OS image instead of copying ISO files
            if image_option == "wtg" {
                if write_mode != engine::WriteMode::DiskImage {
                    anyhow::bail!("--image-option wtg requires --mode disk-image (the default)");
                }
                if fs != "ntfs" {
                    anyhow::bail!("Windows To Go can only be applied with --fs ntfs");
                }
            }
            let write_mode = if image_option == "wtg" {
                engine::WriteMode::WindowsToGo
            } else {
                write_mode
            };
            let bb_test = match badblocks {
                None => engine::BadBlocksTest::Disabled,
                Some(v @ 0..=4) => engine::BadBlocksTest::from_sel(v as usize),
                Some(_) => anyhow::bail!("--badblocks must be 0-4 (Rufus passes selector)"),
            };
            // Rufus IDC_DISK_ID: hex, 0x80 (default) through 0x87
            let disk_id_str = disk_id.trim();
            let hex = disk_id_str
                .strip_prefix("0x")
                .or_else(|| disk_id_str.strip_prefix("0X"))
                .unwrap_or(disk_id_str);
            let disk_id = u8::from_str_radix(hex, 16)
                .ok()
                .filter(|v| (0x80..=0x87).contains(v))
                .ok_or_else(|| {
                    anyhow::anyhow!("--disk-id must be 0x80-0x87 (Rufus disk masquerade)")
                })?;
            if write_mode == engine::WriteMode::DiskImage && image.is_none() {
                anyhow::bail!("--mode disk-image requires an image file");
            }
            // destructive-write confirmation: show exactly what will be erased
            let image_desc = match (&image, write_mode) {
                (Some(p), _) => format!("'{}'", p.display()),
                (None, engine::WriteMode::NonBootable) => "(no image — non-bootable format)".into(),
                (None, engine::WriteMode::FreeDos) => "(FreeDOS boot files)".into(),
                _ => "(no image)".into(),
            };
            println!(
                "About to write {} to {} ({}, {})\nALL DATA ON {} WILL BE DESTROYED.",
                image_desc,
                dev.path,
                dev.size_human(),
                if dev.model.is_empty() {
                    "unknown model"
                } else {
                    &dev.model
                },
                dev.path
            );
            if let Some(mib) = persistent {
                println!("  persistence partition: {mib} MiB (casper-rw)");
            }
            // Rufus MSG_098: WTG on REMOVABLE media usually freezes at boot
            if write_mode == engine::WriteMode::WindowsToGo && dev.removable {
                println!(
                    "  IMPORTANT: 'Windows To Go' on a REMOVABLE drive is not supported by Microsoft and will most likely freeze during boot."
                );
            }
            if bb_test != engine::BadBlocksTest::Disabled {
                println!(
                    "  bad blocks check: DESTRUCTIVE write test, {} pass(es) — test patterns overwrite the whole device",
                    bb_test.passes()
                );
            }
            if !yes {
                print!("Continue? [y/N] ");
                use std::io::Write as _;
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                let answer = answer.trim().to_ascii_lowercase();
                if answer != "y" && answer != "yes" {
                    anyhow::bail!("aborted by user");
                }
            }
            let mut fs = fs;
            if write_mode == engine::WriteMode::FreeDos {
                fs = "fat32".into(); // DOS requires FAT
            }
            let opts = engine::Options {
                fs: match fs.as_str() {
                    "ntfs" => engine::FsKind::Ntfs,
                    "exfat" => engine::FsKind::Exfat,
                    _ => engine::FsKind::Fat32,
                },
                scheme: if gpt {
                    engine::PartitionScheme::Gpt
                } else {
                    engine::PartitionScheme::Mbr
                },
                fs_label: label,
                quick,
                verify,
                efi_only,
                persistence_mib: persistent.unwrap_or(0),
                write_mode,
                old_bios_fixes,
                wue: None,
                allow_system_disk: force,
                bb_test,
                wim_index: wim_index.max(1),
                rufus_mbr: !no_rufus_mbr,
                disk_id,
                autorun: !no_autorun,
                preserve_timestamps: false,
            };
            let eng = engine::Engine::new(opts);
            let image = image.unwrap_or_default();
            let mut last = String::new();
            eng.write_iso(&dev, &image, &mut |p: engine::Progress| {
                let msg = format!("[{}] {} {}/{} {}", p.phase, p.message, p.done, p.total, "");
                if msg != last {
                    println!("{}", msg.trim_end());
                    last = msg;
                }
            })?;
            println!("Done.");
            Ok(())
        }
    }
}
