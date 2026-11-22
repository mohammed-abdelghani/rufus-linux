//! BCD (Boot Configuration Data) store builder for Windows To Go.
//!
//! Rufus delegates boot-file creation to the Windows-only `bcdboot.exe`; on
//! Linux we reuse the BCD store that ships on the Windows ISO itself
//! ([`patch_bcd_for_wtg`]) and fall back to generating one from scratch
//! ([`build_bcd`]) when the image carries none. A BCD store is a Windows
//! registry hive (regf) holding the Windows Boot Manager object and one OS
//! loader object per entry:
//!
//! ```text
//! (root = "System")
//!   \Description        KeyName (mount metadata)
//!   \Objects
//!     \{bootmgr-guid}   Description\Type = 0x10100002
//!     |   \Elements\23000003\Element = {osloader-guid}
//!     |   \Elements\24000001\Element = displayorder (MULTI_SZ)
//!     |   \Elements\25000004\Element = timeout (u64)
//!     \{osloader-guid}  Description\Type = 0x10200003
//!         \Elements\11000001\Element = device   (OS volume)
//!         \Elements\12000002\Element = \Windows\system32\winload.efi
//!         \Elements\16000009\Element = 0  (Rufus: bcdedit recoveryenabled no)
//!         \Elements\21000001\Element = osdevice (OS volume)
//!         \Elements\22000002\Element = \Windows
//! ```
//!
//! Every BCD element is an Elements subkey named by its 8-digit hex element
//! id, holding the data in a value named "Element" — Windows Internals,
//! mistyprojects BCDEdit notes, verified against store exports.
//!
//! Device elements reference partitions the way bcdboot does (type 6,
//! "qualified partition"): disk signature + partition byte offset on MBR,
//! disk GUID + partition GUID on GPT. The 88-byte element layout was taken
//! from real bcdboot-created stores (mistyprojects BCDEdit notes, verified
//! against a Windows 7 store export and booted in QEMU).

use anyhow::bail;
use crate::error::Result;
use crate::gpt::Guid;

/// Windows Boot Manager object identifier (fixed, like `{bootmgr}`).
pub const BOOTMGR_GUID: &str = "9dea862c-5cdd-4e70-acc1-f32b344d4795";

// BCD element datatype codes (geoffchappell.com/notes/windows/boot/bcd).
const ELEM_DEVICE: u32 = 0x11000001; // library / device
const ELEM_PATH: u32 = 0x12000002; // library / string
const ELEM_DESCRIPTION: u32 = 0x12000004; // library / string
const ELEM_LOCALE: u32 = 0x12000005; // library / string
const ELEM_INHERIT: u32 = 0x14000006; // library / GUID list
const ELEM_RECOVERY_ENABLED: u32 = 0x16000009; // library / boolean
const ELEM_KERNEL_DEBUGGER_ENABLED: u32 = 0x260000A0; // application / boolean
const ELEM_OS_DEVICE: u32 = 0x21000001; // application / device
const ELEM_SYSTEM_ROOT: u32 = 0x22000002; // application / string
const ELEM_DEFAULT: u32 = 0x23000003; // bootmgr / GUID
const ELEM_DISPLAY_ORDER: u32 = 0x24000001; // bootmgr / GUID list
const ELEM_TIMEOUT: u32 = 0x25000004; // bootmgr / integer
const ELEM_NX: u32 = 0x25000020; // application / integer
const ELEM_DEBUG_TYPE: u32 = 0x15000011; // dbgsettings / integer (0 = serial)
const ELEM_DEBUG_PORT: u32 = 0x15000013; // dbgsettings / integer
const ELEM_BAUD_RATE: u32 = 0x15000014; // dbgsettings / integer
const DBGSETTINGS_GUID: &str = "4636856e-540f-4170-a130-a84776f4c654";

/// Object types (`Description\Type`, REG_DWORD).
/// NOTE: the boot manager is 0x10100002 (image type "firmware" — bootmgfw is
/// loaded by the UEFI firmware), NOT the 0x10200002 often quoted; verified
/// against real bcdboot-created stores.
const OBJ_BOOTMGR: u32 = 0x10100002;
const OBJ_OSLOADER: u32 = 0x10200003;

/// A partition referenced by a BCD device element ("qualified partition",
/// device type 6): disk signature + partition byte offset on MBR,
/// disk GUID + partition GUID on GPT.
#[derive(Debug, Clone, PartialEq)]
pub enum BcdDevice {
    /// "device boot" — the volume the boot manager was loaded from
    Boot,
    Mbr {
        disk_signature: u32,
        /// partition start offset in BYTES (LBA × 512)
        partition_offset: u64,
    },
    Gpt {
        disk_guid: Guid,
        partition_guid: Guid,
    },
}

/// The 88-byte device element blob, byte-for-byte like bcdboot writes it
/// (verified against real Windows 7/8.1 stores).
pub fn device_element(dev: &BcdDevice) -> Vec<u8> {
    if matches!(dev, BcdDevice::Boot) {
        // device boot: header + type 1 + zero payload
        let mut e = vec![0u8; 32];
        e[0x10..0x14].copy_from_slice(&1u32.to_le_bytes()); // BootDevice
        e[0x18..0x1C].copy_from_slice(&0x10u32.to_le_bytes()); // payload size
        return e;
    }
    let mut e = vec![0u8; 88];
    e[0x10..0x14].copy_from_slice(&6u32.to_le_bytes()); // QualifiedPartition
    e[0x18..0x1C].copy_from_slice(&0x48u32.to_le_bytes()); // payload size
    match dev {
        BcdDevice::Boot => unreachable!("handled above"),
        BcdDevice::Mbr { disk_signature, partition_offset } => {
            e[0x20..0x28].copy_from_slice(&partition_offset.to_le_bytes());
            e[0x34..0x38].copy_from_slice(&1u32.to_le_bytes()); // MBR style marker
            e[0x38..0x3C].copy_from_slice(&disk_signature.to_le_bytes());
        }
        BcdDevice::Gpt { disk_guid, partition_guid } => {
            e[0x20..0x30].copy_from_slice(&partition_guid.0); // partition GUID
            e[0x38..0x48].copy_from_slice(&disk_guid.0); // disk GUID
        }
    }
    e
}

/// The boot entry to install in a freshly built store.
#[derive(Debug, Clone)]
pub struct BcdSpec {
    /// volume bootmgr/bootmgfw is loaded from (ESP on GPT, the NTFS
    /// partition itself on MBR)
    pub boot_device: BcdDevice,
    /// volume holding \Windows
    pub os_device: BcdDevice,
    /// OS loader object GUID, formatted "{...}" (lowercase)
    pub entry_guid: String,
    /// description shown in the boot menu (e.g. the Windows edition)
    pub entry_description: String,
    /// "\Windows\system32\winload.efi" (UEFI) or winload.exe (BIOS)
    pub loader_path: String,
    /// boot menu locale, "en-US"
    pub locale: String,
    /// add the {dbgsettings} serial-debug chain (kernel prints its debugger
    /// init to COM1) — used by the QEMU boot test, off for real installs
    pub kernel_debug: bool,
}

// ---------------------------------------------------------------------------
// Minimal registry hive (regf) writer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum RegValue {
    /// REG_SZ (UTF-16LE, null-terminated)
    Sz(String),
    /// REG_MULTI_SZ (UTF-16LE list, double-null terminated)
    MultiSz(Vec<String>),
    /// REG_BINARY
    Binary(Vec<u8>),
    /// REG_DWORD
    Dword(u32),
}

impl RegValue {
    fn reg_type(&self) -> u32 {
        match self {
            RegValue::Sz(_) => 1,
            RegValue::MultiSz(_) => 7,
            RegValue::Binary(_) => 3,
            RegValue::Dword(_) => 4,
        }
    }
    fn data(&self) -> Vec<u8> {
        match self {
            RegValue::Sz(s) => to_utf16(s),
            RegValue::MultiSz(list) => {
                let mut v = Vec::new();
                for s in list {
                    v.extend_from_slice(&to_utf16(s));
                }
                v.extend_from_slice(&[0, 0]); // list terminator
                v
            }
            RegValue::Binary(b) => b.clone(),
            RegValue::Dword(d) => d.to_le_bytes().to_vec(),
        }
    }
}

/// UTF-16LE with a single null terminator (REG_SZ/MULTI_SZ member encoding).
fn to_utf16(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() * 2 + 2);
    for c in s.encode_utf16() {
        v.extend_from_slice(&c.to_le_bytes());
    }
    v.extend_from_slice(&[0, 0]);
    v
}

struct HiveKey {
    name: String,
    values: Vec<(String, RegValue)>,
    subkeys: Vec<HiveKey>,
}

impl HiveKey {
    fn new(name: &str) -> Self {
        HiveKey { name: name.to_string(), values: Vec::new(), subkeys: Vec::new() }
    }
    fn value(mut self, name: &str, v: RegValue) -> Self {
        self.values.push((name.to_string(), v));
        self
    }
    fn subkey(mut self, k: HiveKey) -> Self {
        self.subkeys.push(k);
        self
    }
    /// BCD element: real bcdboot stores hold each element as an Elements
    /// subkey named by its 8-digit hex element id, with the data in a value
    /// named "Element" inside it (Windows Internals / mistyprojects BCDEdit
    /// notes; verified against store exports).
    fn element(self, id: u32, v: RegValue) -> Self {
        self.subkey(HiveKey::new(&hex8(id)).value("Element", v))
    }
}

/// Sequential cell allocator over the hive bins data area.
/// Cell offsets are relative to the start of the hive bins data (the spec's
/// "offset ... relative from the start of the hive bins data"), i.e. the
/// file offset of a cell is 4096 + cell offset.
struct HiveCells {
    data: Vec<u8>,
}

impl HiveCells {
    fn new() -> Self {
        // the first 32 bytes hold the hbin header; cell offsets are relative
        // to the start of the hive bins data (i.e. they include those 32)
        HiveCells { data: vec![0u8; 32] }
    }

    /// Allocate an 8-aligned cell; `payload` is the record bytes. Returns the
    /// cell offset (start of the size field). Cells never span the 4096-byte
    /// bin boundary — real hives use 4096-byte bins and bootmgfw's parser
    /// walks them bin by bin.
    fn alloc(&mut self, payload: &[u8]) -> u32 {
        let need = (4 + payload.len() + 7) & !7usize;
        let bin_end = ((self.data.len() / 4096) + 1) * 4096;
        if self.data.len() + need + 8 > bin_end {
            // finish this bin with a free cell, then start a fresh one
            let free = bin_end - self.data.len();
            if free >= 8 {
                self.data.extend_from_slice(&(free as i32).to_le_bytes());
            }
            self.data.resize(bin_end, 0);
            self.data.extend_from_slice(b"hbin");
            self.data.extend_from_slice(&4096u32.to_le_bytes()); // bin size (fixed up later)
            self.data.extend_from_slice(&[0u8; 8]); // reserved
            self.data.extend_from_slice(&0u64.to_le_bytes()); // timestamp (first bin only)
            self.data.extend_from_slice(&0u32.to_le_bytes()); // spare
        }
        // align the cell start to 8 bytes (offsets are 8-aligned in real hives)
        while !self.data.len().is_multiple_of(8) {
            self.data.push(0);
        }
        let off = self.data.len() as u32;
        let total = (4 + payload.len() + 7) & !7usize; // size incl. field, 8-aligned
        self.data.extend_from_slice(&(-(total as i32)).to_le_bytes()); // negative = allocated
        self.data.extend_from_slice(payload);
        self.data.resize(off as usize + total, 0);
        off
    }

    fn alloc_data(&mut self, data: &[u8]) -> u32 {
        self.alloc(data)
    }

    /// Key value (vk) record for `name` = `value`; data > 4 bytes goes into
    /// its own cell, otherwise it is stored inline in the record.
    fn alloc_vk(&mut self, name: &str, value: &RegValue) -> u32 {
        let data = value.data();
        let name_bytes = name.as_bytes();
        // value names are pure ASCII here → KEY_COMP_NAME
        let flags: u16 = 0x0001;
        let mut vk = Vec::with_capacity(20 + name_bytes.len());
        vk.extend_from_slice(b"vk");
        vk.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        if data.is_empty() {
            vk.extend_from_slice(&0u32.to_le_bytes());
            vk.extend_from_slice(&0u32.to_le_bytes());
        } else if data.len() <= 4 {
            // inline: data size with the high bit set, data in the offset field
            let mut inline = [0u8; 4];
            inline[..data.len()].copy_from_slice(&data);
            vk.extend_from_slice(&((0x8000_0000u32 | data.len() as u32).to_le_bytes()));
            vk.extend_from_slice(&inline);
        } else {
            let cell = self.alloc_data(&data);
            vk.extend_from_slice(&(data.len() as u32).to_le_bytes());
            vk.extend_from_slice(&cell.to_le_bytes());
        }
        vk.extend_from_slice(&value.reg_type().to_le_bytes());
        vk.extend_from_slice(&flags.to_le_bytes());
        vk.extend_from_slice(&0u16.to_le_bytes()); // spare
        vk.extend_from_slice(name_bytes);
        self.alloc(&vk)
    }

    /// The cell holding the list of vk offsets ("key values list").
    fn alloc_value_list(&mut self, vk_offsets: &[u32]) -> u32 {
        let mut list = Vec::with_capacity(vk_offsets.len() * 4);
        for o in vk_offsets {
            list.extend_from_slice(&o.to_le_bytes());
        }
        self.alloc(&list)
    }

    /// Fast leaf (lf): subkeys sorted by name with 4-char name hints — the
    /// leaf type real BCD hives use (minor version 3).
    fn alloc_subkey_list(&mut self, nk_offsets: &[(String, u32)]) -> u32 {
        let mut entries: Vec<(u32, String)> =
            nk_offsets.iter().map(|(n, o)| (*o, n.clone())).collect();
        entries.sort_by_key(|(_, name)| name.to_uppercase());
        let mut lf = Vec::with_capacity(4 + entries.len() * 8);
        lf.extend_from_slice(b"lf");
        lf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (off, name) in entries {
            lf.extend_from_slice(&off.to_le_bytes());
            // name hint: first 4 ASCII chars, null-padded
            let mut hint = [0u8; 4];
            for (i, b) in name.bytes().take(4).enumerate() {
                hint[i] = b;
            }
            lf.extend_from_slice(&hint);
        }
        self.alloc(&lf)
    }

    /// Key node (nk) record; children must already be allocated. Their
    /// `parent` fields are patched afterwards (children allocate first).
    fn alloc_nk(
        &mut self,
        key: &HiveKey,
        parent_off: u32,
        is_root: bool,
        now: u64,
        security_off: u32,
    ) -> u32 {
        // values first (data cells, then vk records, then the list)
        let mut vk_offsets = Vec::with_capacity(key.values.len());
        let mut largest_value_name = 0usize;
        let mut largest_value_data = 0usize;
        for (name, v) in &key.values {
            vk_offsets.push(self.alloc_vk(name, v));
            largest_value_name = largest_value_name.max(name.len() * 2 + 2);
            let dlen = match v {
                RegValue::Binary(b) => b.len(),
                _ => to_utf16_len(v),
            };
            largest_value_data = largest_value_data.max(dlen);
        }
        let value_list = if vk_offsets.is_empty() {
            0xFFFF_FFFF
        } else {
            self.alloc_value_list(&vk_offsets)
        };
        // subkeys (depth-first: children allocate before the parent's nk)
        let mut sub_offsets = Vec::with_capacity(key.subkeys.len());
        let mut largest_subkey_name = 0usize;
        for sub in &key.subkeys {
            largest_subkey_name = largest_subkey_name.max(sub.name.len() * 2 + 2);
            let off = self.alloc_nk(sub, 0xFFFF_FFFF, false, now, security_off);
            sub_offsets.push((sub.name.clone(), off));
        }
        let subkey_list = if sub_offsets.is_empty() {
            0xFFFF_FFFF
        } else {
            self.alloc_subkey_list(&sub_offsets)
        };

        let name_bytes = key.name.as_bytes();
        let mut flags: u16 = 0x0020; // KEY_COMP_NAME (ASCII name)
        if is_root {
            // real BCD root: hive entry + no-delete + compressed name
            flags |= 0x0004 | 0x0008;
        }
        let mut nk = vec![0u8; 76 + name_bytes.len()];
        nk[0..2].copy_from_slice(b"nk");
        nk[2..4].copy_from_slice(&flags.to_le_bytes());
        nk[4..12].copy_from_slice(&now.to_le_bytes()); // last written
        // 0x0C spare = 0
        nk[16..20].copy_from_slice(&parent_off.to_le_bytes());
        nk[20..24].copy_from_slice(&(key.subkeys.len() as u32).to_le_bytes());
        // 0x18 volatile subkeys = 0
        nk[28..32].copy_from_slice(&subkey_list.to_le_bytes());
        nk[32..36].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // volatile list
        nk[36..40].copy_from_slice(&(key.values.len() as u32).to_le_bytes());
        nk[40..44].copy_from_slice(&value_list.to_le_bytes());
        nk[44..48].copy_from_slice(&security_off.to_le_bytes()); // security (sk)
        nk[48..52].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // class name
        nk[52..56].copy_from_slice(&(largest_subkey_name as u32).to_le_bytes());
        // 0x38 largest class name = 0
        nk[60..64].copy_from_slice(&(largest_value_name as u32).to_le_bytes());
        nk[64..68].copy_from_slice(&(largest_value_data as u32).to_le_bytes());
        // 0x44 workvar = 0
        nk[72..74].copy_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        // 0x4A class name length = 0
        nk[76..].copy_from_slice(name_bytes);
        let off = self.alloc(&nk);
        // patch the children's parent field now that this nk has an offset
        for (_, child) in &sub_offsets {
            let at = *child as usize + 4 + 16; // cell size field + nk parent
            self.data[at..at + 4].copy_from_slice(&off.to_le_bytes());
        }
        off
    }

    /// Security record (sk) with the standard BCD security descriptor; all
    /// key nodes share it (self-linked list — the kernel and bootmgfw both
    /// walk the security chain and reject hives without it).
    fn alloc_sk(&mut self) -> u32 {
        const SD: &[u8] = &[
            0x01, 0x00, 0x04, 0x80, 0x50, 0x00, 0x00, 0x00, 0x60, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, 0x02, 0x00, 0x3c, 0x00,
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x19, 0x00, 0x06, 0x00,
            0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00,
            0x20, 0x02, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x3f, 0x00, 0x0f, 0x00,
            0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x12, 0x00, 0x00, 0x00,
            0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0x01, 0x02, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x05, 0x20, 0x00, 0x00, 0x00, 0x20, 0x02, 0x00, 0x00,
            0x01, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x15, 0x00, 0x00, 0x00,
            0x59, 0x51, 0xb8, 0x17, 0x66, 0x72, 0x5d, 0x25, 0x64, 0x63, 0x3b, 0x0b,
            0xc1, 0x99, 0x21, 0x00,
        ];
        // allocate the record first to learn its offset, then fill in the
        // self-referencing flink/blink
        let off = self.alloc(&vec![0u8; 24 + SD.len()]);
        let mut sk = Vec::with_capacity(24 + SD.len());
        sk.extend_from_slice(b"sk");
        sk.extend_from_slice(&0u16.to_le_bytes()); // reserved
        sk.extend_from_slice(&off.to_le_bytes()); // flink → self
        sk.extend_from_slice(&off.to_le_bytes()); // blink → self
        sk.extend_from_slice(&1u32.to_le_bytes()); // reference count
        sk.extend_from_slice(&(SD.len() as u32).to_le_bytes());
        sk.extend_from_slice(SD);
        // rewrite the cell payload in place
        let at = off as usize + 4;
        self.data[at..at + sk.len()].copy_from_slice(&sk);
        off
    }
}

fn to_utf16_len(v: &RegValue) -> usize {
    match v {
        RegValue::Sz(s) => s.len() * 2 + 2,
        RegValue::MultiSz(l) => l.iter().map(|s| s.len() * 2 + 2).sum::<usize>() + 2,
        RegValue::Binary(b) => b.len(),
        RegValue::Dword(_) => 4,
    }
}

/// Current time as a Windows FILETIME (100 ns ticks since 1601-01-01 UTC).
fn filetime_now() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    now.as_secs() * 10_000_000 + (now.subsec_nanos() as u64 / 100) + 116_444_736_000_000_000
}

/// Serialize the key tree into a complete regf hive (4096-byte base block +
/// 4096-byte hive bins), with checksums. Layout matches the real bcdboot-
/// created stores: minor version 3 (lf leaves), a security record for
/// every key, 4096-byte bins.
fn build_hive(root: &HiveKey) -> Vec<u8> {
    let now = filetime_now();
    let mut cells = HiveCells::new();
    let sk_off = cells.alloc_sk();
    let root_off = cells.alloc_nk(root, 0, true, now, sk_off);
    // the root's parent points at itself (nothing to inherit)
    let at = root_off as usize + 4 + 16;
    cells.data[at..at + 4].copy_from_slice(&root_off.to_le_bytes());

    // the first bin's header was reserved in cells.data[0..32]; finish the
    // last bin with a free cell
    let bin_size = (cells.data.len() + 8).div_ceil(4096) * 4096;
    cells.data[0..4].copy_from_slice(b"hbin");
    cells.data[4..8].copy_from_slice(&0u32.to_le_bytes()); // bin offset (first bin)
    cells.data[8..12].copy_from_slice(&4096u32.to_le_bytes());
    // 12..20 reserved
    cells.data[20..28].copy_from_slice(&now.to_le_bytes()); // timestamp (first bin)
    // 28..32 spare = 0
    // free cell covering the rest of the last bin (positive = unallocated)
    let free = bin_size - cells.data.len();
    cells.data.extend_from_slice(&(free as i32).to_le_bytes());
    cells.data.resize(bin_size, 0);
    // stamp the header of every additional bin created by alloc()
    let mut boff = 4096usize;
    while boff < cells.data.len() {
        cells.data[boff..boff + 4].copy_from_slice(b"hbin");
        cells.data[boff + 4..boff + 8].copy_from_slice(&((boff as u32).to_le_bytes()));
        cells.data[boff + 8..boff + 12].copy_from_slice(&4096u32.to_le_bytes());
        boff += 4096;
    }

    // base block
    let mut base = vec![0u8; 4096];
    base[0..4].copy_from_slice(b"regf");
    base[4..8].copy_from_slice(&1u32.to_le_bytes()); // primary sequence
    base[8..12].copy_from_slice(&1u32.to_le_bytes()); // secondary sequence
    base[12..20].copy_from_slice(&now.to_le_bytes()); // last written
    base[20..24].copy_from_slice(&1u32.to_le_bytes()); // major version
    base[24..28].copy_from_slice(&3u32.to_le_bytes()); // minor version (lf leaves, like real BCD)
    base[28..32].copy_from_slice(&0u32.to_le_bytes()); // primary file
    base[32..36].copy_from_slice(&1u32.to_le_bytes()); // direct memory load
    base[36..40].copy_from_slice(&root_off.to_le_bytes()); // root cell offset
    base[40..44].copy_from_slice(&(bin_size as u32).to_le_bytes()); // bins data size
    base[44..48].copy_from_slice(&1u32.to_le_bytes()); // clustering factor
    // file name (debug string, UTF-16LE)
    let fname: Vec<u16> = "\\Boot\\BCD".encode_utf16().collect();
    for (i, c) in fname.iter().enumerate().take(32) {
        base[48 + i * 2..50 + i * 2].copy_from_slice(&c.to_le_bytes());
    }
    let checksum = hive_checksum(&base[..508]);
    base[508..512].copy_from_slice(&checksum.to_le_bytes());
    let mut out = base;
    out.extend_from_slice(&cells.data);
    out
}

/// XOR-32 checksum of the first 508 base-block bytes (0 → 1, !0 → !0-1).
fn hive_checksum(data: &[u8]) -> u32 {
    let mut c: u32 = 0;
    for chunk in data.as_chunks::<4>().0 {
        c ^= u32::from_le_bytes(*chunk);
    }
    if c == u32::MAX {
        u32::MAX - 1
    } else if c == 0 {
        1
    } else {
        c
    }
}

/// One value of a key: (vk cell, name, data_size field, data payload range)
type BcdValue = (u32, String, u32, Option<(usize, usize)>);

// Patch an existing BCD hive (the one shipped on the Windows ISO) into a
// Windows To Go store: the media's "Windows Setup" osloader entry becomes a
// WTG entry whose device/osdevice point at the drive's Windows partition
// and whose loader path is winload.efi/winload.exe. Rufus gets the same
// result from bcdboot; on Linux we reuse Microsoft's own store — it is
// guaranteed bootmgfw-compatible, verified by booting in QEMU.
//
// All edits stay inside existing cell payloads (device blobs shrink, strings
// only shrink or fit), so the hive layout needs no restructuring. The edits
// are collected read-only first and applied in one pass afterwards.
pub fn patch_bcd_for_wtg(
    hive: &mut [u8],
    dev: &BcdDevice,
    loader_path: &str,
    entry_description: &str,
) -> anyhow::Result<()> {
    struct CellMap {
        /// (cell offset, payload start, payload end) — file offsets
        cells: Vec<(u32, usize, usize)>,
    }
    impl CellMap {
        fn find(&self, co: u32) -> anyhow::Result<(usize, usize)> {
            self.cells
                .iter()
                .find(|(o, _, _)| *o == co)
                .map(|&(_, s, e)| (s, e))
                .ok_or_else(|| anyhow::anyhow!("cell {co:#x} missing"))
        }
    }

    if hive.len() < 4096 || &hive[0..4] != b"regf" {
        bail!("not a registry hive");
    }
    let bins_size = u32::from_le_bytes(hive[40..44].try_into().unwrap()) as usize;
    if 4096 + bins_size > hive.len() {
        bail!("hive bins size out of range");
    }
    let mut map = CellMap { cells: Vec::new() };
    let mut bin = 0usize;
    while bin + 4096 <= bins_size {
        if &hive[4096 + bin..4096 + bin + 4] != b"hbin" {
            bail!("bad hbin at {bin:#x}");
        }
        let mut off = bin + 32;
        let end = bin + 4096;
        while off + 8 <= end {
            let sz = i32::from_le_bytes(
                hive[4096 + off..4096 + off + 4].try_into().unwrap(),
            );
            if sz == 0 {
                break;
            }
            let abs = sz.unsigned_abs() as usize;
            if off + abs > end {
                bail!("cell at {off:#x} crosses the bin boundary");
            }
            // key: bins-relative cell offset (what records reference);
            // payload range: file offsets (cells live at file offset 4096+)
            map.cells.push((off as u32, 4096 + off + 4, 4096 + off + abs));
            off += abs;
        }
        bin += 4096;
    }

    // one value of a key: (vk cell, name, data_size, data payload range)
    fn values(
        hive: &[u8],
        map: &CellMap,
        nk: u32,
    ) -> anyhow::Result<Vec<BcdValue>> {
        let (s, e) = map.find(nk)?;
        let p = &hive[s..e];
        if &p[0..2] != b"nk" {
            bail!("cell {nk:#x} is not an nk");
        }
        let vals = u32::from_le_bytes(p[36..40].try_into().unwrap()) as usize;
        let mut out = Vec::new();
        if vals == 0 {
            return Ok(out);
        }
        let vlist = u32::from_le_bytes(p[40..44].try_into().unwrap());
        let (vs, ve) = map.find(vlist)?;
        let vp = &hive[vs..ve];
        for i in 0..vals {
            let voff = u32::from_le_bytes(vp[i * 4..i * 4 + 4].try_into().unwrap());
            let (s, e) = map.find(voff)?;
            let vk = &hive[s..e];
            if &vk[0..2] != b"vk" {
                bail!("value cell {voff:#x} is not a vk");
            }
            let nlen = u16::from_le_bytes(vk[2..4].try_into().unwrap()) as usize;
            let comp = u16::from_le_bytes(vk[16..18].try_into().unwrap()) & 0x0001 != 0;
            let name = if comp {
                String::from_utf8_lossy(&vk[20..20 + nlen]).to_string()
            } else {
                String::from_utf16_lossy(
                    &vk[20..20 + nlen]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .collect::<Vec<_>>(),
                )
            };
            let szf = u32::from_le_bytes(vk[4..8].try_into().unwrap());
            if szf & 0x8000_0000 != 0 {
                out.push((voff, name, szf, None));
            } else {
                let dvo = u32::from_le_bytes(vk[8..12].try_into().unwrap());
                let range = map.find(dvo)?;
                out.push((voff, name, szf, Some(range)));
            }
        }
        Ok(out)
    }

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|c| c.to_le_bytes())
            .collect()
    }

    // collect edits: (file position, replacement bytes)
    let mut edits: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut osloader_count = 0usize;

    for &(_nk, s, e) in &map.cells.clone() {
        let p = &hive[s..e];
        if &p[0..2] != b"nk" {
            continue;
        }
        let subs = u32::from_le_bytes(p[20..24].try_into().unwrap()) as usize;
        if subs == 0 {
            continue;
        }
        let slist = u32::from_le_bytes(p[28..32].try_into().unwrap());
        let (ls, le) = match map.find(slist) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let lp = hive[ls..le].to_vec();
        if &lp[0..2] != b"lf" && &lp[0..2] != b"lh" && &lp[0..2] != b"li" {
            continue;
        }
        let cnt = u16::from_le_bytes(lp[2..4].try_into().unwrap()) as usize;
        let stride = if &lp[0..2] == b"li" { 4 } else { 8 };
        let mut elements_cell = None;
        let mut is_osloader = false;
        for i in 0..cnt {
            let so = u32::from_le_bytes(
                lp[4 + i * stride..4 + i * stride + 4].try_into().unwrap(),
            );
            let (ss, se) = match map.find(so) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let sp = hive[ss..se].to_vec();
            if &sp[0..2] != b"nk" {
                continue;
            }
            let nlen = u16::from_le_bytes(sp[72..74].try_into().unwrap()) as usize;
            let comp = u16::from_le_bytes(sp[2..4].try_into().unwrap()) & 0x20 != 0;
            let name = if comp {
                String::from_utf8_lossy(&sp[76..76 + nlen]).to_string()
            } else {
                String::from_utf16_lossy(
                    &sp[76..76 + nlen]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .collect::<Vec<_>>(),
                )
            };
            if name == "Elements" {
                elements_cell = Some(so);
            } else if name == "Description" {
                for (voff, vname, szf, data) in values(hive, &map, so)? {
                    if vname != "Type" {
                        continue;
                    }
                    // the object type dword: real stores keep it inline (the
                    // payload lives in the vk record's data-offset field),
                    // but honor the external form too
                    let raw: Option<[u8; 4]> = if szf & 0x8000_0000 != 0 {
                        let (vs, _) = map.find(voff)?;
                        let mut b = [0u8; 4];
                        b.copy_from_slice(&hive[vs + 8..vs + 12]);
                        Some(b)
                    } else if szf as usize == 4 {
                        data.map(|(ds, _de)| {
                            let mut b = [0u8; 4];
                            b.copy_from_slice(&hive[ds..ds + 4]);
                            b
                        })
                    } else {
                        None
                    };
                    if raw == Some((0x10200003u32).to_le_bytes()) {
                        is_osloader = true;
                    }
                }
            }
        }
        let Some(elements) = elements_cell else { continue };
        if !is_osloader {
            continue;
        }
        osloader_count += 1;

        // patch the osloader's Elements values
        let dev_blob = device_element(dev);
        let (es, ee) = map.find(elements)?;
        let ep = hive[es..ee].to_vec();
        if &ep[0..2] != b"nk" {
            continue;
        }
        let subs = u32::from_le_bytes(ep[20..24].try_into().unwrap()) as usize;
        if subs == 0 {
            continue;
        }
        let slist = u32::from_le_bytes(ep[28..32].try_into().unwrap());
        let (ls, le) = map.find(slist)?;
        let lp = hive[ls..le].to_vec();
        let cnt = u16::from_le_bytes(lp[2..4].try_into().unwrap()) as usize;
        let stride = if &lp[0..2] == b"li" { 4 } else { 8 };
        for i in 0..cnt {
            let so = u32::from_le_bytes(
                lp[4 + i * stride..4 + i * stride + 4].try_into().unwrap(),
            );
            let (ss, se) = match map.find(so) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let sp = hive[ss..se].to_vec();
            if &sp[0..2] != b"nk" {
                continue;
            }
            let nlen = u16::from_le_bytes(sp[72..74].try_into().unwrap()) as usize;
            let comp = u16::from_le_bytes(sp[2..4].try_into().unwrap()) & 0x20 != 0;
            let ename = if comp {
                String::from_utf8_lossy(&sp[76..76 + nlen]).to_string()
            } else {
                String::from_utf16_lossy(
                    &sp[76..76 + nlen]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .collect::<Vec<_>>(),
                )
            };
            // each element lives in an Elements subkey named by its 8-digit
            // hex id, holding its data in a value named "Element" (real
            // bcdboot stores and our build_bcd alike)
            for (voff, _vname, mut szf, data) in values(hive, &map, so)? {
                match ename.as_str() {
                    "11000001" | "21000001" => {
                        // device/osdevice → our qualified partition blob.
                        // These are boot-critical: if the ISO's element can't
                        // hold the blob, the whole patch must fail so the
                        // engine falls back to a freshly built store — a
                        // "successful" patch with the old device would leave
                        // an unbootable drive.
                        let Some((ds, de)) = data else {
                            bail!("BCD device element {ename} is not a regular value — build a store from scratch instead");
                        };
                        if de - ds < dev_blob.len() {
                            bail!(
                                "the BCD device element ({de} bytes) can't hold a \
                                 qualified-partition blob ({} bytes) — build a store \
                                 from scratch instead",
                                dev_blob.len(),
                            );
                        }
                        edits.push((ds, dev_blob.clone()));
                        edits.push((
                            ds + dev_blob.len(),
                            vec![0u8; de - ds - dev_blob.len()],
                        ));
                        szf = (szf & 0x8000_0000) | dev_blob.len() as u32;
                    }
                    "12000002" => {
                        // loader path (boot-critical, see above)
                        let Some((ds, de)) = data else {
                            bail!("BCD loader path is not a regular value — build a store from scratch instead");
                        };
                        let new = utf16z(loader_path);
                        if de - ds < new.len() {
                            bail!(
                                "the BCD loader-path element can't hold \"{loader_path}\" \
                                 — build a store from scratch instead",
                            );
                        }
                        edits.push((ds, new.clone()));
                        edits.push((ds + new.len(), vec![0u8; de - ds - new.len()]));
                        szf = (szf & 0x8000_0000) | new.len() as u32;
                    }
                    "12000004" => {
                        // description — best effort
                        let Some((ds, de)) = data else { continue };
                        let new = utf16z(entry_description);
                        if de - ds >= new.len() && new.len() >= 4 {
                            edits.push((ds, new.clone()));
                            edits.push((ds + new.len(), vec![0u8; de - ds - new.len()]));
                            szf = (szf & 0x8000_0000) | new.len() as u32;
                        }
                    }
                    "22000002" => {
                        // systemroot → \Windows
                        let Some((ds, de)) = data else { continue };
                        let new = utf16z("\\Windows");
                        if de - ds >= new.len() {
                            edits.push((ds, new));
                            szf = (szf & 0x8000_0000) | ("\\Windows".len() * 2 + 2) as u32;
                        }
                    }
                    "26000022" => {
                        // winpe → off
                        let Some((ds, de)) = data else { continue };
                        if de - ds >= 1 {
                            edits.push((ds, vec![0]));
                        }
                    }
                    _ => continue,
                }
                // rewrite the vk record's data-size field (payload start + 4)
                let (vstart, _) = map.find(voff)?;
                edits.push((vstart + 4, szf.to_le_bytes().to_vec()));
            }
        }
    }
    if osloader_count == 0 {
        bail!("no osloader entry in the BCD");
    }

    // apply all edits (they never overlap — each touches a distinct cell)
    for (pos, bytes) in edits {
        hive[pos..pos + bytes.len()].copy_from_slice(&bytes);
    }
    Ok(())
}

/// Build the full BCD hive for a Windows To Go drive from scratch (the
/// primary WTG path patches the ISO's own BCD instead; see patch_bcd_for_wtg).
pub fn build_bcd(spec: &BcdSpec) -> Result<Vec<u8>> {
    let entry = spec.entry_guid.trim().to_lowercase();
    let entry = if entry.starts_with('{') { entry } else { format!("{{{entry}}}") };

    // inherited settings objects — bcdboot stores always carry the full
    // chain: {globalsettings} → {dbgsettings}/{emssettings}/{ramdefects},
    // {bootloadersettings} → {globalsettings}/{hypervisorsettings}
    const GLOBALSETTINGS_GUID: &str = "7ea2e1ac-2e61-4728-aaa3-896d9d0a9f0e";
    const BOOTLOADERSETTINGS_GUID: &str = "6efb52bf-1766-41db-a6b3-0ee5eff72bd7";
    const EMSSETTINGS_GUID: &str = "0ce4991b-e6b3-4b16-b23c-5e0d9250e5d9";
    const RAMDEFECTS_GUID: &str = "5189b25c-5558-4bf2-bca4-289b11bd29e2";
    const HYPERVISORSETTINGS_GUID: &str = "7ff607e0-4395-11db-b0de-0800200c9a66";
    let inherited = |name: &str, _guid: &str, elems: HiveKey| {
        HiveKey::new(name)
            .subkey(HiveKey::new("Description").value("Type", RegValue::Dword(0x2010_0000)))
            .subkey(elems)
    };
    let dbg_elems = HiveKey::new("Elements")
        .element(ELEM_DEBUG_TYPE, RegValue::Binary(0u64.to_le_bytes().to_vec()))
        .element(ELEM_DEBUG_PORT, RegValue::Binary(1u64.to_le_bytes().to_vec()))
        .element(ELEM_BAUD_RATE, RegValue::Binary(115200u64.to_le_bytes().to_vec()));
    let dbg = inherited(&format!("{{{DBGSETTINGS_GUID}}}"), DBGSETTINGS_GUID, dbg_elems);
    let ems = inherited(
        &format!("{{{EMSSETTINGS_GUID}}}"),
        EMSSETTINGS_GUID,
        HiveKey::new("Elements").element(0x26000020, RegValue::Binary(vec![0])),
    );
    let ramdefects = inherited(
        &format!("{{{RAMDEFECTS_GUID}}}"),
        RAMDEFECTS_GUID,
        HiveKey::new("Elements"),
    );
    let hypervisor = inherited(
        &format!("{{{HYPERVISORSETTINGS_GUID}}}"),
        HYPERVISORSETTINGS_GUID,
        HiveKey::new("Elements")
            .element(0x250000f3, RegValue::Binary(0u64.to_le_bytes().to_vec()))
            .element(0x250000f4, RegValue::Binary(1u64.to_le_bytes().to_vec()))
            .element(0x250000f5, RegValue::Binary(70000u64.to_le_bytes().to_vec())),
    );
    let globalsettings = HiveKey::new(&format!("{{{GLOBALSETTINGS_GUID}}}"))
        .subkey(HiveKey::new("Description").value("Type", RegValue::Dword(0x2010_0000)))
        .subkey(
            HiveKey::new("Elements").element(
                ELEM_INHERIT,
                RegValue::MultiSz(vec![
                    format!("{{{DBGSETTINGS_GUID}}}"),
                    format!("{{{EMSSETTINGS_GUID}}}"),
                    format!("{{{RAMDEFECTS_GUID}}}"),
                ]),
            ),
        );
    let bootloadersettings = HiveKey::new(&format!("{{{BOOTLOADERSETTINGS_GUID}}}"))
        .subkey(HiveKey::new("Description").value("Type", RegValue::Dword(0x2010_0000)))
        .subkey(
            HiveKey::new("Elements").element(
                ELEM_INHERIT,
                RegValue::MultiSz(vec![
                    format!("{{{GLOBALSETTINGS_GUID}}}"),
                    format!("{{{HYPERVISORSETTINGS_GUID}}}"),
                ]),
            ),
        );

    let bootmgr = HiveKey::new(&format!("{{{BOOTMGR_GUID}}}"))
        .subkey(HiveKey::new("Description").value("Type", RegValue::Dword(OBJ_BOOTMGR)))
        .subkey(
            HiveKey::new("Elements")
                .element(ELEM_DEVICE, RegValue::Binary(device_element(&spec.boot_device)))
                .element(ELEM_DESCRIPTION, RegValue::Sz("Windows Boot Manager".into()))
                .element(ELEM_LOCALE, RegValue::Sz(spec.locale.clone()))
                .element(ELEM_INHERIT, RegValue::MultiSz(vec![format!("{{{GLOBALSETTINGS_GUID}}}")]))
                .element(ELEM_DEFAULT, RegValue::Sz(entry.clone()))
                .element(ELEM_DISPLAY_ORDER, RegValue::MultiSz(vec![entry.clone()]))
                .element(ELEM_TIMEOUT, RegValue::Binary(30u64.to_le_bytes().to_vec())),
        );

    // the loader inherits {bootloadersettings} (+ {dbgsettings} when the
    // kernel debugger is on — one 14000006 element either way)
    let loader_inherit = if spec.kernel_debug {
        vec![
            format!("{{{BOOTLOADERSETTINGS_GUID}}}"),
            format!("{{{DBGSETTINGS_GUID}}}"),
        ]
    } else {
        vec![format!("{{{BOOTLOADERSETTINGS_GUID}}}")]
    };
    let mut osloader_elems = HiveKey::new("Elements")
        .element(ELEM_DEVICE, RegValue::Binary(device_element(&spec.os_device)))
        .element(ELEM_PATH, RegValue::Sz(spec.loader_path.clone()))
        .element(ELEM_DESCRIPTION, RegValue::Sz(spec.entry_description.clone()))
        .element(ELEM_LOCALE, RegValue::Sz(spec.locale.clone()))
        .element(ELEM_INHERIT, RegValue::MultiSz(loader_inherit))
        // Rufus: bcdedit /set {default} recoveryenabled no
        .element(ELEM_RECOVERY_ENABLED, RegValue::Binary(vec![0]))
        .element(ELEM_OS_DEVICE, RegValue::Binary(device_element(&spec.os_device)))
        .element(ELEM_SYSTEM_ROOT, RegValue::Sz("\\Windows".into()))
        // nx = OptIn, like bcdboot-created stores
        .element(ELEM_NX, RegValue::Binary(0u64.to_le_bytes().to_vec()));

    let mut objects = HiveKey::new("Objects")
        .subkey(bootmgr)
        .subkey(globalsettings)
        .subkey(ems)
        .subkey(ramdefects)
        .subkey(bootloadersettings)
        .subkey(hypervisor);
    if spec.kernel_debug {
        // /debug on: the kernel prints its debugger handshake to COM1, which
        // the QEMU boot test uses to prove the OS actually started from our BCD
        objects = objects.subkey(dbg);
        osloader_elems = osloader_elems
            .element(ELEM_KERNEL_DEBUGGER_ENABLED, RegValue::Binary(vec![1]));
    }
    let osloader = HiveKey::new(&entry)
        .subkey(HiveKey::new("Description").value("Type", RegValue::Dword(OBJ_OSLOADER)))
        .subkey(osloader_elems);
    objects = objects.subkey(osloader);
    // real BCD files: root key named "System", a Description child with a
    // single opaque KeyName value (no System/TreatAsSystem — those appear
    // only in the mounted view), then Objects
    // the real stores keep a 24-byte opaque blob here (REG_SZ-typed);
    // a timestamp followed by zeros is structurally identical
    let mut kn = filetime_now().to_le_bytes().to_vec();
    kn.extend_from_slice(&[0u8; 16]);
    let keyname = RegValue::Sz(String::from_utf16_lossy(
        &kn.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect::<Vec<_>>(),
    ));
    let root = HiveKey::new("System")
        .subkey(HiveKey::new("Description").value("KeyName", keyname))
        .subkey(objects);
    Ok(build_hive(&root))
}

fn hex8(v: u32) -> String {
    format!("{v:08x}")
}

// ---------------------------------------------------------------------------
// Tests: a minimal regf reader validating what we write
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Cell walk: (offset → payload) for allocated cells.
    pub(crate) struct HiveReader<'a> {
        pub(crate) bins: &'a [u8],
    }

    impl<'a> HiveReader<'a> {
        pub(crate) fn cell_payload(&self, off: u32) -> &'a [u8] {
            let start = off as usize;
            let size = i32::from_le_bytes(
                self.bins[start..start + 4].try_into().unwrap(),
            );
            assert!(size < 0, "cell at {off:#x} should be allocated");
            let size = (-size) as usize;
            &self.bins[start + 4..start + size]
        }
    }

    struct ParsedValue {
        name: String,
        reg_type: u32,
        data: Vec<u8>,
    }

    struct ParsedKey {
        name: String,
        subkeys: Vec<ParsedKey>,
        values: Vec<ParsedValue>,
    }

    fn parse_string16(data: &[u8]) -> String {
        let mut s = String::new();
        for c in data.chunks_exact(2) {
            let u = u16::from_le_bytes([c[0], c[1]]);
            if u == 0 {
                break;
            }
            s.push(char::from_u32(u as u32).unwrap_or('\u{FFFD}'));
        }
        s
    }

    fn parse_vk(cells: &HiveReader, payload: &[u8]) -> ParsedValue {
        assert_eq!(&payload[0..2], b"vk");
        let name_len = u16::from_le_bytes(payload[2..4].try_into().unwrap()) as usize;
        let size_field = u32::from_le_bytes(payload[4..8].try_into().unwrap());
        let off_field = u32::from_le_bytes(payload[8..12].try_into().unwrap());
        let reg_type = u32::from_le_bytes(payload[12..16].try_into().unwrap());
        let name = String::from_utf8_lossy(&payload[20..20 + name_len]).to_string();
        let data = if size_field & 0x8000_0000 != 0 {
            let n = (size_field & 0x7FFF_FFFF) as usize;
            off_field.to_le_bytes()[..n].to_vec()
        } else if size_field == 0 {
            Vec::new()
        } else {
            cells.cell_payload(off_field)[..size_field as usize].to_vec()
        };
        ParsedValue { name, reg_type, data }
    }

    fn parse_nk(cells: &HiveReader, off: u32) -> ParsedKey {
        let payload = cells.cell_payload(off);
        assert_eq!(&payload[0..2], b"nk");
        let flags = u16::from_le_bytes(payload[2..4].try_into().unwrap());
        let name_len = u16::from_le_bytes(payload[72..74].try_into().unwrap()) as usize;
        let name = if flags & 0x0020 != 0 {
            String::from_utf8_lossy(&payload[76..76 + name_len]).to_string()
        } else {
            parse_string16(&payload[76..76 + name_len])
        };
        // values
        let n_values = u32::from_le_bytes(payload[36..40].try_into().unwrap()) as usize;
        let mut values = Vec::new();
        if n_values > 0 {
            let list = u32::from_le_bytes(payload[40..44].try_into().unwrap());
            let lp = cells.cell_payload(list);
            for i in 0..n_values {
                let vk_off = u32::from_le_bytes(lp[i * 4..i * 4 + 4].try_into().unwrap());
                values.push(parse_vk(cells, cells.cell_payload(vk_off)));
            }
        }
        // subkeys (lf, the leaf type real BCD hives use)
        let n_subs = u32::from_le_bytes(payload[20..24].try_into().unwrap()) as usize;
        let mut subkeys = Vec::new();
        if n_subs > 0 {
            let list = u32::from_le_bytes(payload[28..32].try_into().unwrap());
            let lp = cells.cell_payload(list);
            assert_eq!(&lp[0..2], b"lf");
            let cnt = u16::from_le_bytes(lp[2..4].try_into().unwrap()) as usize;
            for i in 0..cnt {
                let sub_off =
                    u32::from_le_bytes(lp[4 + i * 8..4 + i * 8 + 4].try_into().unwrap());
                subkeys.push(parse_nk(cells, sub_off));
            }
        }
        ParsedKey { name, subkeys, values }
    }

    fn parse_hive(bytes: &[u8]) -> ParsedKey {
        assert_eq!(&bytes[0..4], b"regf");
        // base-block checksum
        let stored = u32::from_le_bytes(bytes[508..512].try_into().unwrap());
        assert_eq!(stored, hive_checksum(&bytes[..508]), "base block checksum");
        let root_off = u32::from_le_bytes(bytes[36..40].try_into().unwrap());
        let bins_size = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        let bins = &bytes[4096..4096 + bins_size];
        assert_eq!(&bins[0..4], b"hbin");
        assert_eq!(bins_size % 4096, 0, "bin size is a multiple of 4096");
        let cells = HiveReader { bins: &bins[..bins_size] };
        parse_nk(&cells, root_off)
    }

    fn key<'a>(k: &'a ParsedKey, name: &str) -> &'a ParsedKey {
        k.subkeys.iter().find(|s| s.name == name)
            .unwrap_or_else(|| panic!("missing subkey {name} under {}", k.name))
    }

    fn value<'a>(k: &'a ParsedKey, name: &str) -> &'a ParsedValue {
        k.values.iter().find(|v| v.name == name)
            .unwrap_or_else(|| panic!("missing value {name} under {}", k.name))
    }

    /// read a BCD element's data: Elements\<id> subkey, "Element" value
    fn elem<'a>(k: &'a ParsedKey, id: &str) -> &'a ParsedValue {
        value(key(k, id), "Element")
    }

    fn sample_spec() -> BcdSpec {
        BcdSpec {
            boot_device: BcdDevice::Mbr { disk_signature: 0x2344A9FF, partition_offset: 0x40100000 },
            os_device: BcdDevice::Mbr { disk_signature: 0x2344A9FF, partition_offset: 0x40100000 },
            entry_guid: "{3afd365b-4349-11e3-b0da-a722f0df0a34}".into(),
            entry_description: "Windows To Go".into(),
            loader_path: "\\Windows\\system32\\winload.exe".into(),
            locale: "en-US".into(),
            kernel_debug: false,
        }
    }

    #[test]
    fn device_element_matches_real_store_bytes() {
        // byte-identical to the qualified-partition element of a real
        // Windows 7 store (partition=C:, sig 0x2344A9FF, offset 0x40100000)
        let dev = BcdDevice::Mbr { disk_signature: 0x2344A9FF, partition_offset: 0x40100000 };
        let e = device_element(&dev);
        assert_eq!(e.len(), 88);
        assert_eq!(u32::from_le_bytes(e[0x10..0x14].try_into().unwrap()), 6);
        assert_eq!(u32::from_le_bytes(e[0x18..0x1C].try_into().unwrap()), 0x48);
        assert_eq!(u64::from_le_bytes(e[0x20..0x28].try_into().unwrap()), 0x40100000);
        assert_eq!(u32::from_le_bytes(e[0x34..0x38].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(e[0x38..0x3C].try_into().unwrap()), 0x2344A9FF);
        assert!(e[0x3C..].iter().all(|&x| x == 0));
    }

    #[test]
    fn device_element_gpt_layout() {
        // mirrors the real GPT store: partition GUID @0x20, disk GUID @0x38
        let disk = Guid([
            0xd2, 0x54, 0xac, 0xd9, 0x87, 0xea, 0x2c, 0x43,
            0x86, 0xb5, 0x66, 0xc4, 0xe8, 0x10, 0x5f, 0x9d,
        ]);
        let part = Guid([
            0x08, 0x63, 0xe5, 0xbb, 0xe7, 0x18, 0x34, 0x41,
            0x9b, 0x6c, 0x4b, 0x31, 0x4d, 0x12, 0xea, 0xb4,
        ]);
        let e = device_element(&BcdDevice::Gpt { disk_guid: disk, partition_guid: part });
        assert_eq!(e.len(), 88);
        assert_eq!(&e[0x20..0x30], &part.0);
        assert_eq!(u64::from_le_bytes(e[0x30..0x38].try_into().unwrap()), 0);
        assert_eq!(&e[0x38..0x48], &disk.0);
        assert!(e[0x48..].iter().all(|&x| x == 0));
    }

    #[test]
    fn hive_has_expected_tree_and_values() {
        let bytes = build_bcd(&sample_spec()).unwrap();
        let root = parse_hive(&bytes);
        assert_eq!(root.name, "System", "real BCD hives name their root System");

        let desc = key(&root, "Description");
        assert_eq!(value(desc, "KeyName").reg_type, 1); // REG_SZ

        let objects = key(&root, "Objects");
        let bootmgr = key(objects, "{9dea862c-5cdd-4e70-acc1-f32b344d4795}");
        let bm_type = value(key(bootmgr, "Description"), "Type");
        assert_eq!(bm_type.reg_type, 4);
        assert_eq!(bm_type.data, OBJ_BOOTMGR.to_le_bytes());
        let bm_elems = key(bootmgr, "Elements");
        let dev = elem(bm_elems, "11000001");
        assert_eq!(dev.reg_type, 3); // REG_BINARY
        assert_eq!(dev.data, device_element(&BcdDevice::Mbr {
            disk_signature: 0x2344A9FF, partition_offset: 0x40100000
        }));
        assert_eq!(elem(bm_elems, "12000004").data, to_utf16("Windows Boot Manager"));
        assert_eq!(elem(bm_elems, "25000004").data, 30u64.to_le_bytes());
        let default = elem(bm_elems, "23000003");
        assert_eq!(default.reg_type, 1); // REG_SZ
        assert_eq!(default.data, to_utf16("{3afd365b-4349-11e3-b0da-a722f0df0a34}"));
        let order = elem(bm_elems, "24000001");
        assert_eq!(order.reg_type, 7); // REG_MULTI_SZ
        let mut expect = to_utf16("{3afd365b-4349-11e3-b0da-a722f0df0a34}");
        expect.extend_from_slice(&[0, 0]);
        assert_eq!(order.data, expect);

        let entry = key(objects, "{3afd365b-4349-11e3-b0da-a722f0df0a34}");
        let e_type = value(key(entry, "Description"), "Type");
        assert_eq!(e_type.data, OBJ_OSLOADER.to_le_bytes());
        let e_elems = key(entry, "Elements");
        assert_eq!(elem(e_elems, "12000002").data, to_utf16("\\Windows\\system32\\winload.exe"));
        assert_eq!(elem(e_elems, "22000002").data, to_utf16("\\Windows"));
        assert_eq!(elem(e_elems, "12000004").data, to_utf16("Windows To Go"));
        // recoveryenabled = no → one 0 byte (like the real store's booleans)
        let rec = elem(e_elems, "16000009");
        assert_eq!(rec.reg_type, 3);
        assert_eq!(rec.data, vec![0]);
        assert_eq!(elem(e_elems, "25000020").data, [0u8; 8]);
        assert_eq!(elem(e_elems, "21000001").data, dev.data);
    }

    #[test]
    fn hive_cells_are_aligned_and_checksummed() {
        let bytes = build_bcd(&sample_spec()).unwrap();
        assert_eq!(bytes.len() % 4096, 0);
        // every allocated cell start is 8-aligned with a sane negative size
        let bins_size = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        let bins = &bytes[4096..4096 + bins_size];
        assert_eq!(bins_size % 4096, 0, "bins are 4096-byte multiples");
        let mut bin = 0usize;
        while bin + 4096 <= bins_size {
            assert_eq!(&bins[bin..bin + 4], b"hbin", "bin magic at {bin:#x}");
            assert_eq!(
                u32::from_le_bytes(bins[bin + 4..bin + 8].try_into().unwrap()),
                bin as u32,
                "bin offset field"
            );
            assert_eq!(
                u32::from_le_bytes(bins[bin + 8..bin + 12].try_into().unwrap()),
                4096,
                "bin size field"
            );
            let mut off = bin + 32;
            while off + 8 <= bin + 4096 {
                let size = i32::from_le_bytes(bins[off..off + 4].try_into().unwrap());
                if size == 0 {
                    break;
                }
                let abs = size.unsigned_abs() as usize;
                assert!(abs >= 8 && off + abs <= bin + 4096, "cell at {off:#x}");
                assert_eq!(abs % 8, 0, "cell size 8-aligned at {off:#x}");
                off += abs;
            }
            bin += 4096;
        }
    }


    #[test]
    fn patch_bcd_retargets_osloader_like_qemu_test() {
        // synthetic stand-in for the ISO's own store: built with build_bcd
        // (same structural conventions: "System" root, Description/Type,
        // Elements with id-named keys), with qualified-partition device
        // elements like a store patch_bcd_for_wtg can retarget, then
        // retargeted like the WTG flow
        let dev = BcdDevice::Gpt {
            disk_guid: Guid([1u8; 16]),
            partition_guid: Guid([2u8; 16]),
        };
        let spec = BcdSpec {
            boot_device: dev.clone(),
            os_device: dev.clone(),
            entry_guid: "{3afd365b-4349-11e3-b0da-a722f0df0a34}".into(),
            entry_description: "Windows Setup".into(),
            loader_path: "\\Windows\\system32\\boot\\winload.efi".into(),
            locale: "en-US".into(),
            kernel_debug: false,
        };
        let mut hive = build_bcd(&spec).unwrap();

        patch_bcd_for_wtg(
            &mut hive,
            &BcdDevice::Gpt {
                disk_guid: Guid([7u8; 16]),
                partition_guid: Guid([9u8; 16]),
            },
            "\\Windows\\system32\\winload.efi",
            "Windows To Go",
        )
        .expect("patch must succeed on our own store");

        // the osloader's device elements must now be our qualified partition
        let root = parse_hive(&hive);
        let objects = key(&root, "Objects");
        let entry = key(objects, "{3afd365b-4349-11e3-b0da-a722f0df0a34}");
        let elems = key(entry, "Elements");
        let want = device_element(&BcdDevice::Gpt {
            disk_guid: Guid([7u8; 16]),
            partition_guid: Guid([9u8; 16]),
        });
        assert_eq!(elem(elems, "11000001").data, want, "device retargeted");
        assert_eq!(elem(elems, "21000001").data, want, "osdevice retargeted");
        // loader path retargeted to winload.efi
        assert_eq!(
            elem(elems, "12000002").data,
            to_utf16("\\Windows\\system32\\winload.efi")
        );
        // description retargeted (cell allows it: "Windows To Go" fits where
        // "Windows Setup" was)
        assert_eq!(
            elem(elems, "12000004").data,
            to_utf16("Windows To Go")
        );
    }

    #[test]
    fn patch_bcd_rejects_non_hive() {
        let mut garbage = vec![0u8; 4096];
        assert!(patch_bcd_for_wtg(
            &mut garbage,
            &BcdDevice::Boot,
            "\\Windows\\system32\\winload.efi",
            "x",
        )
        .is_err());
    }

    #[test]
    fn patch_bcd_bails_when_device_element_too_small() {
        // an osloader whose device element is a small "boot device" blob
        // (like the plain Windows-ISO stores) can't hold the qualified
        // partition blob the WTG drive needs — the patch must fail so the
        // engine falls back to a freshly built store
        let spec = BcdSpec {
            boot_device: BcdDevice::Boot,
            os_device: BcdDevice::Boot,
            entry_guid: "{3afd365b-4349-11e3-b0da-a722f0df0a34}".into(),
            entry_description: "Windows Setup".into(),
            loader_path: "\\Windows\\system32\\boot\\winload.efi".into(),
            locale: "en-US".into(),
            kernel_debug: false,
        };
        let mut hive = build_bcd(&spec).unwrap();
        let res = patch_bcd_for_wtg(
            &mut hive,
            &BcdDevice::Gpt { disk_guid: Guid([7u8; 16]), partition_guid: Guid([9u8; 16]) },
            "\\Windows\\system32\\winload.efi",
            "Windows To Go",
        );
        let err = res.expect_err("too-small device element must fail the patch");
        assert!(
            err.to_string().contains("qualified-partition blob"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn build_bcd_accepts_guid_without_braces() {
        let mut spec = sample_spec();
        spec.entry_guid = "3afd365b-4349-11e3-b0da-a722f0df0a34".into();
        let bytes = build_bcd(&spec).unwrap();
        let root = parse_hive(&bytes);
        let objects = key(&root, "Objects");
        // both the bootmgr default and the object key use the braced form
        let bootmgr = key(objects, "{9dea862c-5cdd-4e70-acc1-f32b344d4795}");
        assert_eq!(
            elem(key(bootmgr, "Elements"), "23000003").data,
            to_utf16("{3afd365b-4349-11e3-b0da-a722f0df0a34}")
        );
        assert!(key(objects, "{3afd365b-4349-11e3-b0da-a722f0df0a34}").name.starts_with('{'));
    }
}
