//! Block device enumeration via sysfs.
//! Finds whole-disk devices that look like removable USB drives.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Device {
    /// e.g. "/dev/sdb"
    pub path: String,
    /// kernel name e.g. "sdb"
    pub name: String,
    /// size in bytes
    pub size: u64,
    pub model: String,
    pub vendor: String,
    pub serial: String,
    pub removable: bool,
    /// e.g. "usb", "sata", "nvme"
    pub bus: String,
    /// transport from /sys/block/X/device (e.g. "usb")
    pub tran: String,
}

impl Device {
    pub fn size_human(&self) -> String {
        human_size(self.size)
    }

    pub fn is_usb(&self) -> bool {
        self.tran == "usb" || self.bus == "usb"
    }
}

/// Human-readable byte size, e.g. "14.5 GiB".
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.1} {}", v, UNITS[u])
}

fn read_trim(p: &Path) -> String {
    fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Node path of partition `n` of whole disk `dev` — NVMe/MMC disks name
/// partitions with a "p" separator ("/dev/nvme0n1" -> "/dev/nvme0n1p1"),
/// everything else appends the number directly ("/dev/sdb" -> "/dev/sdb1").
pub fn partition_node(dev: &str, n: u32) -> String {
    let name = dev.rsplit('/').next().unwrap_or(dev);
    if name.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{dev}p{n}")
    } else {
        format!("{dev}{n}")
    }
}

/// Mountpoint of the root/boot filesystem if it lives on `dev` or one of its
/// partitions (reads /proc/mounts). Used to refuse wiping the system disk.
pub fn system_root_mount(dev: &str) -> Option<String> {
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    system_root_mount_in(&mounts, dev)
}

fn system_root_mount_in(mounts: &str, dev: &str) -> Option<String> {
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let src = fields.next().unwrap_or("");
        let mnt = fields.next().unwrap_or("");
        if !is_dev_or_partition_of(src, dev) {
            continue;
        }
        if matches!(mnt, "/" | "/boot" | "/efi" | "/boot/efi") {
            return Some(mnt.to_string());
        }
    }
    None
}

/// true if `src` is `dev` itself or a partition node of it (sda1, nvme0n1p1,
/// mmcblk0p1 — not sdaa, sda1p2 look-alikes).
fn is_dev_or_partition_of(src: &str, dev: &str) -> bool {
    if src == dev {
        return true;
    }
    let Some(rest) = src.strip_prefix(dev) else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let rest = rest.strip_prefix('p').unwrap_or(rest);
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Which devices to include in an enumeration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DevFilter {
    /// removable media + USB flash devices — the GUI/`list` default.
    /// Internal (SATA/NVMe/MMC) disks are NEVER listed here, like Rufus.
    Hotplug,
    /// Hotplug + USB hard drives (rotational USB devices) — the
    /// "List USB Hard Drives" checkbox
    UsbHddToo,
    /// everything except virtual devices — CLI `list --all`
    All,
}

/// Enumerate candidate whole-disk devices under /sys/block.
pub fn list_devices(filter: DevFilter) -> Vec<Device> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        // virtual / non-writable devices
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("zram")
            || name.starts_with("dm-")
            || name.starts_with("md")
            || name.starts_with("sr")
        {
            continue;
        }
        let dev_path = e.path();
        let removable = read_trim(&dev_path.join("removable")) == "1";
        let ro = read_trim(&dev_path.join("ro")) == "1";
        if ro {
            continue;
        }
        // transport: resolve the sysfs chain like udev does (scsi_device ->
        // scsi target/host -> usb interface -> usb device). The naive
        // "device/../tran" read never resolves to the transport and made
        // plenty of real USB sticks invisible.
        let devdir = dev_path.join("device");
        let (usb, bus) = detect_bus(&devdir);
        // Internal fixed disks (SATA/NVMe/MMC, non-USB, non-removable) are
        // never listed outside of DevFilter::All — that is what keeps the
        // system drive out of the GUI device combo. NOTE: most USB sticks
        // report queue/rotational=1 through usb-storage, so USB devices are
        // always included regardless of the rotational flag.
        let include = match filter {
            DevFilter::All => true,
            DevFilter::UsbHddToo => usb || removable,
            DevFilter::Hotplug => usb || removable,
        };
        if !include {
            continue;
        }
        let size = read_trim(&dev_path.join("size"))
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_mul(512);
        if size == 0 {
            continue;
        }
        // model/vendor/serial live on the device dir
        let model = read_trim(&devdir.join("model"));
        let vendor = read_trim(&devdir.join("vendor"));
        let serial = read_trim(&devdir.join("serial"));
        out.push(Device {
            path: format!("/dev/{name}"),
            name,
            size,
            model,
            vendor,
            serial,
            removable,
            bus: bus.clone(),
            tran: bus,
        });
    }
    // USB/removable devices first, then by name (ascending)
    out.sort_by(|a, b| b.is_usb().cmp(&a.is_usb()).then(a.name.cmp(&b.name)));
    out
}

/// Resolve the transport of a block device by walking up the sysfs chain the
/// way udev derives ID_BUS: each directory exposes a "subsystem" symlink.
/// way udev derives ID_BUS: each directory exposes a "subsystem" symlink.
/// way udev derives ID_BUS: each directory exposes a "subsystem" symlink.
/// Returns (is_usb, bus_name): scsi_device (6:0:0:0) -> scsi target/host ->
/// usb interface (1-2:1.0) -> usb device (1-2) -> usb bus (usb1) for sticks;
/// ata/nvme/mmc/virtio layers for internal disks.
fn detect_bus(devdir: &Path) -> (bool, String) {
    let Ok(mut cur) = devdir.canonicalize() else {
        return (false, String::new());
    };
    const GENERIC: [&str; 3] = ["scsi", "scsi_device", "scsi_host"];
    let mut generic = String::new();
    for _ in 0..8 {
        if let Ok(link) = fs::read_link(cur.join("subsystem")) {
            let name = link
                .to_string_lossy()
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            match name.as_str() {
                "usb" => return (true, "usb".to_string()),
                "ata" | "nvme" | "mmc" | "virtio" => return (false, name),
                _ => {
                    if generic.is_empty() && GENERIC.contains(&name.as_str()) {
                        generic = name;
                    }
                }
            }
        }
        // stop at the top of the sysfs device tree
        if !cur.pop() || cur == Path::new("/sys/devices") || cur == Path::new("/sys") {
            break;
        }
    }
    (false, generic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_node_naming() {
        assert_eq!(partition_node("/dev/sdb", 1), "/dev/sdb1");
        assert_eq!(partition_node("/dev/sdb", 2), "/dev/sdb2");
        assert_eq!(partition_node("/dev/nvme0n1", 1), "/dev/nvme0n1p1");
        assert_eq!(partition_node("/dev/nvme0n1", 3), "/dev/nvme0n1p3");
        assert_eq!(partition_node("/dev/mmcblk0", 2), "/dev/mmcblk0p2");
    }

    #[test]
    fn dev_or_partition_matching() {
        assert!(is_dev_or_partition_of("/dev/sda", "/dev/sda"));
        assert!(is_dev_or_partition_of("/dev/sda1", "/dev/sda"));
        assert!(is_dev_or_partition_of("/dev/nvme0n1p2", "/dev/nvme0n1"));
        assert!(is_dev_or_partition_of("/dev/mmcblk0p1", "/dev/mmcblk0"));
        // look-alikes must NOT match
        assert!(!is_dev_or_partition_of("/dev/sdaa", "/dev/sda"));
        assert!(!is_dev_or_partition_of("/dev/sdb1", "/dev/sda"));
        assert!(!is_dev_or_partition_of("/dev/mapper/root", "/dev/sda"));
        // "/dev/sda1p2" can never exist in the kernel, but matching it errs on
        // the safe side of the system-disk refusal
        assert!(is_dev_or_partition_of("/dev/sda1p2", "/dev/sda1"));
    }

    #[test]
    fn detect_bus_finds_usb_through_scsi_layers() {
        // fake sysfs tree shaped like a real USB stick:
        // 6:0:0:0 -> target -> host -> usb interface -> usb device -> usb bus
        let base = std::env::temp_dir().join(format!("rufus-bustest-{}", std::process::id()));
        let dev_dir = base.join("devices/pci0/usb1/1-2/1-2:1.0/host6/target6:0:0/6:0:0:0");
        let class = base.join("class");
        let _ = std::fs::create_dir_all(&dev_dir);
        let _ = std::fs::create_dir_all(class.join("usb"));
        let _ = std::fs::create_dir_all(class.join("scsi_device"));
        // subsystem symlinks (real sysfs uses relative ../../class/X links)
        std::os::unix::fs::symlink("../../class/scsi_device", dev_dir.join("subsystem")).unwrap();
        let iface = dev_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        std::os::unix::fs::symlink("../../class/usb", iface.join("subsystem")).unwrap();

        let (usb, bus) = detect_bus(&dev_dir);
        assert!(usb, "USB stick must be detected through scsi layers");
        assert_eq!(bus, "usb");

        // a SATA disk: scsi_device -> target -> host -> ata link
        let sata = base.join("devices/pci0/ata1/link1/dev1.0/host0/target0:0:0/0:0:0:0");
        let _ = std::fs::create_dir_all(&sata);
        std::os::unix::fs::symlink("../../class/scsi_device", sata.join("subsystem")).unwrap();
        let ata_dev = sata.parent().unwrap().parent().unwrap().parent().unwrap();
        let _ = std::fs::create_dir_all(class.join("ata"));
        std::os::unix::fs::symlink("../../class/ata", ata_dev.join("subsystem")).unwrap();
        let (usb, bus) = detect_bus(&sata);
        assert!(!usb);
        assert_eq!(bus, "ata");

        // plain scsi without a known transport
        let plain = base.join("devices/pci0/host9/target9:0:0/9:0:0:0");
        let _ = std::fs::create_dir_all(&plain);
        std::os::unix::fs::symlink("../../class/scsi_device", plain.join("subsystem")).unwrap();
        let (usb, bus) = detect_bus(&plain);
        assert!(!usb);
        assert_eq!(bus, "scsi_device");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn system_root_detection() {
        let mounts = "\
/dev/sda2 / ext4 rw 0 0
/dev/sda1 /boot/efi vfat rw 0 0
/dev/sdb1 /media/usb vfat rw 0 0
tmpfs /tmp tmpfs rw 0 0
";
        assert_eq!(
            system_root_mount_in(mounts, "/dev/sda").as_deref(),
            Some("/")
        );
        assert_eq!(
            system_root_mount_in(mounts, "/dev/sda1").as_deref(),
            Some("/boot/efi")
        );
        // USB stick with no root/boot mount -> None
        assert_eq!(system_root_mount_in(mounts, "/dev/sdb"), None);
        // look-alike /dev/sdaX device must not be treated as system disk
        assert_eq!(system_root_mount_in(mounts, "/dev/sdaa"), None);
    }
}
