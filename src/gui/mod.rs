//! Rufus-style GUI for rufus-linux (matches the Rufus 3.x dialog).
//! winit (X11) + softbuffer pixel buffer + ab_glyph text + custom widgets.
//!
//! Layout mirrors Rufus 3.20: "Drive Properties" section (Device combo with
//! the info button, Boot selection + check mark + split SELECT, Persistent
//! partition size slider, Partition scheme / Target system, advanced drive
//! properties), "Format Options" (Volume label, File system, Cluster size,
//! advanced format options), "Status" (grey READY bar turning into a green
//! progress bar), the small bottom icon buttons, START/CLOSE and the
//! "N devices found" + timer status strip. Extras that Rufus does not have
//! (SHA-256 verify, STOP, inline log) live in the advanced format options
//! with the same look.

use anyhow::Result;
use ab_glyph::{Font as _, ScaleFont as _};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha512};
use softbuffer::{Context, Surface};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::device::{self, Device};
use crate::engine::{self, Engine, Options, Progress, PartitionScheme};
use crate::iso9660;

// Rufus "⬇ DOWNLOAD" dialog (official Windows/Linux ISO downloads)
mod download;

const W: u32 = 640;
const COMBO_H: f32 = 24.0;
const DD_ITEM_H: f32 = 20.0;
const GB: u64 = 1024 * 1024 * 1024;

// Win32/Rufus light palette
const BG: u32 = 0xF0F0F0;
const FIELD: u32 = 0xFFFFFF;
const BORDER: u32 = 0xA0A0A0;
const BORDER_DIS: u32 = 0xD0D0D0;
const TEXT: u32 = 0x000000;
const TEXT_DIS: u32 = 0xA0A0A0;
const BTN: u32 = 0xE1E1E1;
const BTN_BORDER: u32 = 0xADADAD;
const BTN_HOVER: u32 = 0xE5F1FB;
const BTN_HOVER_BORDER: u32 = 0x0078D7;
const ACCENT: u32 = 0x0078D7;
const READY_GREY: u32 = 0xDCDCDC;
const PROGRESS_GREEN: u32 = 0x7CC47C;
const PROGRESS_ERR: u32 = 0xE81123;

fn nz(v: u32) -> std::num::NonZeroU32 {
    std::num::NonZeroU32::new(v.max(1)).unwrap()
}

/// Compact size for the info line: MiB below 1 GiB, GiB above.
fn fmt_size(bytes: u64) -> String {
    if bytes >= GB {
        format!("{:.1} GiB", bytes as f64 / GB as f64)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    }
}

/// Rufus LARGE_FAT32_SIZE (rufus.h): FAT32 is hidden from the filesystem
/// combo above 32 GiB unless "Force large FAT32 formatting" is on.
const LARGE_FAT32_SIZE: u64 = 32 * GB;

/// FAT32 stays selectable on drives up to 32 GiB, or on any drive when the
/// "Force large FAT32 formatting" hidden setting is enabled (Rufus Alt-L).
fn fat32_allowed(dev_size: u64, large_fat32: bool) -> bool {
    dev_size <= LARGE_FAT32_SIZE || large_fat32
}

/// Rufus-style progress bar wording for the engine phases, localized with
/// the gettext-style keys of the current UI language.
fn pretty_phase(lang: usize, phase: &str) -> String {
    let key = match phase {
        "inspect" => "Looking for ISO files",
        "open" => "Opening device",
        "badblocks" => "Checking device for bad blocks",
        "wipe" => "Wiping device signatures",
        "partition" => "Partitioning device",
        "format" => "Formatting",
        "uefi-ntfs" => "Writing UEFI:NTFS partition",
        "mount" => "Preparing file system",
        "copy" => "Copying ISO files",
        "wim" => "Splitting install.wim",
        "boot" => "Installing boot files",
        "wtg-wim" => "Extracting install.wim",
        "wtg-apply" => "Applying Windows image",
        "wtg-boot" => "Installing Windows boot files",
        "verify" => "Verifying (SHA-256)",
        "done" => "DONE",
        _ => return phase.to_string(),
    };
    crate::locale::tr(lang, key).to_string()
}

/// Line colors in the log panel. Rufus marks the level with hardcoded text
/// prefixes ("ERROR:", "WARNING:", "FATAL:", "Notice:", …); we additionally
/// colorize them.
fn log_line_color(line: &str) -> u32 {
    if line.starts_with("ERROR") || line.starts_with("FAILED") {
        PROGRESS_ERR
    } else if line.starts_with("WARNING") {
        0xB07000
    } else if line.starts_with("DONE") {
        PROGRESS_GREEN
    } else {
        0x404040
    }
}

// ---------- ISO summary (cached, parsed with debounce) ----------
#[derive(Clone)]
struct IsoSummary {
    volume_id: String,
    file_count: usize,
    total_bytes: u64,
    has_efi: bool,
    has_bios: bool,
    is_windows: bool,
    /// /bootmgr or /bootmgr.efi in the root (Rufus HAS_BOOTMGR)
    has_bootmgr: bool,
    /// WinPE 1.x layout: ntdetect.com/setupldr.bin/txtsetup.sif under
    /// /i386, /amd64 or /minint (Rufus HAS_WINPE — implies disk-ID
    /// masquerading to 0x81 by default)
    has_winpe: bool,
    /// install.wim/esd present → "Windows To Go" is offered (Rufus HAS_WININST)
    has_wininst: bool,
    large_wim: bool,
}

fn summarize_iso(iso: &iso9660::IsoImage) -> IsoSummary {
    const WIM_LIMIT: u64 = 4 << 30;
    let files: Vec<&iso9660::IsoEntry> = iso.entries.iter().filter(|e| !e.is_dir).collect();
    let is_windows = files.iter().any(|e| {
        let p = e.path.to_lowercase();
        p.starts_with("sources/")
            && (p.contains("setup.exe") || p.contains("install.wim") || p.contains("install.esd"))
    });
    let has_wininst = engine::find_windows_wim(iso).is_some();
    let large_wim = files
        .iter()
        .any(|e| e.path.to_lowercase().contains("install.wim") && e.size > WIM_LIMIT);
    // Rufus iso.c root scan: a bare "bootmgr"/"bootmgr.efi marks a Windows
    // boot manager; WinPE 1.x keeps its loader in /i386, /amd64 or /minint
    let root_file = |name: &str| {
        files
            .iter()
            .any(|e| e.path.eq_ignore_ascii_case(name))
    };
    let pe_file = |name: &str| {
        files.iter().any(|e| {
            let p = e.path.to_lowercase();
            (p.starts_with("i386/") || p.starts_with("amd64/") || p.starts_with("minint/"))
                && p.rsplit('/').next().is_some_and(|b| b.eq_ignore_ascii_case(name))
        })
    };
    IsoSummary {
        volume_id: iso.volume_id.clone(),
        file_count: files.len(),
        total_bytes: files.iter().map(|e| e.size).sum(),
        has_efi: iso.boot_images.iter().any(|b| b.platform == 0xEF),
        has_bios: iso.boot_images.iter().any(|b| b.platform == 0x00),
        is_windows,
        has_bootmgr: root_file("bootmgr") || root_file("bootmgr.efi"),
        has_winpe: ["ntdetect.com", "setupldr.bin", "txtsetup.sif"]
            .into_iter()
            .any(pe_file),
        has_wininst,
        large_wim,
    }
}

// ---------- built-in file browser (fallback when no system picker) ----------
struct FileBrowser {
    dir: std::path::PathBuf,
    entries: Vec<(String, bool, u64)>, // name, is_dir, size
    top: usize,
}

impl FileBrowser {
    fn new() -> Self {
        let dir = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/"));
        let mut b = FileBrowser { dir, entries: Vec::new(), top: 0 };
        b.reload();
        b
    }

    fn reload(&mut self) {
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                v.push((name, is_dir, size));
            }
        }
        v.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        self.entries = v;
        self.top = 0;
    }

    fn enter(&mut self, idx: usize) {
        // idx 0 = parent directory, i>0 = entries[top + i - 1]
        if idx == 0 {
            self.dir.pop();
            self.reload();
            return;
        }
        let i = self.top + idx - 1;
        if let Some((name, is_dir, _)) = self.entries.get(i).cloned() {
            if is_dir {
                self.dir.push(name);
                self.reload();
            }
        }
    }

    /// Some(absolute path) when the clicked entry is a supported image.
    fn pick(&mut self, idx: usize) -> Option<String> {
        let i = self.top + idx - 1;
        let (name, is_dir, _) = self.entries.get(i)?.clone();
        if is_dir || !Self::is_image(&name) {
            return None;
        }
        let full = self.dir.join(&name).to_string_lossy().to_string();
        Some(full)
    }

    fn scroll(&mut self, lines: i32) {
        let max = self.entries.len().saturating_sub(1) as i32;
        self.top = (self.top as i32 - lines).clamp(0, max.max(0)) as usize;
    }

    fn is_image(name: &str) -> bool {
        let n = name.to_lowercase();
        [".iso", ".img", ".gz", ".bin", ".vhd"]
            .iter()
            .any(|ext| n.ends_with(ext))
    }
}

// ---------- fonts ----------
/// One loaded font: the ab_glyph rasterizer font plus the raw bytes, kept so
/// rustybuzz can build a shaping face from the same file (glyph ids match).
#[derive(Clone)]
struct FontEntry {
    font: ab_glyph::FontArc,
    bytes: Arc<Vec<u8>>,
}

#[derive(Clone)]
struct Fonts {
    regular: FontEntry,
    bold: FontEntry,
    /// Extra fonts (Noto etc.) covering scripts the primary font lacks —
    /// CJK, Thai, Devanagari… consulted per character/run at draw time.
    fallbacks: Vec<FontEntry>,
}

/// System font locations for the primary UI font (DejaVu covers Latin, Greek,
/// Cyrillic, Hebrew, Arabic).
const PRIMARY_FONTS: [(&str, &str); 2] = [
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
     "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
    ("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
     "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf"),
];

/// Directories scanned for per-script fallback fonts (Debian/Ubuntu Noto
/// packaging paths; best-effort, missing entries are skipped silently).
const FALLBACK_DIRS: [&str; 4] = [
    "/usr/share/fonts/truetype/noto",
    "/usr/share/fonts/opentype/noto",
    "/usr/share/fonts/noto",
    "/usr/share/fonts/truetype/dejavu",
];

fn load_font_entry(path: &str) -> Option<FontEntry> {
    let bytes = std::fs::read(path).ok()?;
    let font = ab_glyph::FontArc::try_from_vec(bytes.clone()).ok()?;
    Some(FontEntry { font, bytes: Arc::new(bytes) })
}

fn load_fonts() -> Option<Fonts> {
    for (r, b) in PRIMARY_FONTS {
        if let (Some(fr), Some(fb)) = (load_font_entry(r), load_font_entry(b)) {
            return Some(Fonts { regular: fr, bold: fb, fallbacks: load_fallbacks() });
        }
    }
    None
}

/// Load up to 24 regular-weight Noto/DejaVu fonts as glyph fallbacks. Sorted
/// file order keeps the chain deterministic across runs.
fn load_fallbacks() -> Vec<FontEntry> {
    let mut out: Vec<FontEntry> = Vec::new();
    for dir in FALLBACK_DIRS {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut paths: Vec<std::path::PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else { return false };
                let lower = name.to_lowercase();
                (lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".ttc"))
                    && !lower.contains("italic")
                    && !lower.contains("bold")
                    && !lower.contains("light")
                    && !lower.contains("thin")
                    && !lower.contains("black")
                    && !lower.contains("color")
                    && (lower.contains("noto") || lower.contains("dejavusans"))
            })
            .collect();
        paths.sort();
        for p in paths {
            if out.len() >= 24 {
                return out;
            }
            if let Some(path) = p.to_str() {
                if let Some(entry) = load_font_entry(path) {
                    out.push(entry);
                }
            }
        }
    }
    out
}

/// Scripts that need OpenType shaping (contextual forms, mark attachment) or
/// bidi reordering: Arabic, Hebrew, Indic, Thai/Lao/Khmer/Myanmar families.
/// Everything else (Latin, Greek, Cyrillic, CJK…) renders glyph-by-glyph.
fn needs_shaping(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x05FF   // Hebrew (bidi reordering)
        | 0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF // Arabic
        | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF                   // Arabic presentation
        | 0x0900..=0x0DFF                                     // Indic
        | 0x0E00..=0x0EFF                                     // Thai, Lao
        | 0x1000..=0x109F                                     // Myanmar
        | 0x1780..=0x17FF                                     // Khmer
        | 0x1A00..=0x1A1F | 0xAA80..=0xAADF                   // Tai Tham, Cham
    )
}

/// One placed glyph: pixel x offset from the pen start, pixel y offset
/// (positive = up, from the baseline), glyph id and its font index.
struct LayoutGlyph {
    x: f32,
    y: f32,
    gid: u16,
    fi: usize,
}

// ---------- image hashes (Rufus check-icon dialog) ----------
#[derive(Clone)]
struct HashInfo {
    md5: String,
    sha1: String,
    sha256: String,
    sha512: String,
}

enum HashState {
    Computing,
    Ready(HashInfo),
    Failed(String),
}

/// Stream the image through all four digests (like Rufus's hash dialog).
fn compute_image_hashes(path: &std::path::Path) -> Result<HashInfo> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path)?;
    let (mut m5, mut s1, mut s256, mut s512) = (Md5::new(), Sha1::new(), Sha256::new(), Sha512::new());
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        m5.update(&buf[..n]);
        s1.update(&buf[..n]);
        s256.update(&buf[..n]);
        s512.update(&buf[..n]);
    }
    Ok(HashInfo {
        md5: format!("{:x}", m5.finalize()),
        sha1: format!("{:x}", s1.finalize()),
        sha256: format!("{:x}", s256.finalize()),
        sha512: format!("{:x}", s512.finalize()),
    })
}

// ---------- dropdowns ----------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DD {
    Device,
    Boot,
    Recent,
    ImageOption,
    Scheme,
    Target,
    Fs,
    Passes,
    /// Rufus IDC_DISK_ID: BIOS disk ID masquerade (advanced drive properties)
    DiskId,
    /// Rufus IDC_LANG: UI language selector (bottom-left combo)
    Lang,
    // ⬇ DOWNLOAD dialog dropdowns (Release/Edition/Language/Architecture/Image)
    DlRelease,
    DlEdition,
    DlLanguage,
    DlArch,
    DlVariant,
}

const FS_NAMES: [&str; 3] = ["NTFS", "FAT32 (Default)", "exFAT"];
const SCHEME_NAMES: [&str; 2] = ["MBR", "GPT (UEFI)"];
const TARGET_NAMES: [&str; 2] = ["BIOS or UEFI", "UEFI (non CSM)"];

/// Rufus IDC_DISK_ID combo items ("8 disks should be enough for anybody"):
/// the first partition's bootable-flag byte masquerades the stick as
/// disk 1-8 for old BIOSes
const DISK_ID_NAMES: [(&str, u8); 8] = [
    ("0x80 (Default)", 0x80),
    ("0x81 (Disk 2)", 0x81),
    ("0x82 (Disk 3)", 0x82),
    ("0x83 (Disk 4)", 0x83),
    ("0x84 (Disk 5)", 0x84),
    ("0x85 (Disk 6)", 0x85),
    ("0x86 (Disk 7)", 0x86),
    ("0x87 (Disk 8)", 0x87),
];

// ---------- UI state ----------
/// Rufus boot-selection modes: index into BOOT_MODE_NAMES.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BootMode {
    NonBootable,
    FreeDos,
    DiskImage,
}

const BOOT_MODE_NAMES: [&str; 3] = [
    "Non bootable",
    "FreeDOS",
    "Disk or ISO image (Please select)",
];

struct Ui {
    devices: Vec<Device>,
    sel_dev: usize,
    /// UI language index into locale::LANGS (0 = English source locale)
    locale: usize,
    /// Rufus boot-selection mode (Non bootable / FreeDOS / Disk or ISO image)
    boot_mode: BootMode,
    /// Rufus "Image option" (Windows ISOs): 0 = standard install, 1 = WTG
    image_option: usize,
    image_path: String,
    label: String,
    /// index into FS_NAMES: 0=NTFS 1=FAT32 2=exFAT
    fs_sel: usize,
    gpt: bool,
    efi_only: bool,
    quick: bool,
    badblocks: bool,
    /// Rufus "Number of passes" combobox index (0..=4)
    bb_sel: usize,
    verify: bool,
    /// Rufus "Persistent partition size" in GB (0 = disabled)
    persist_gb_str: String,
    list_usb_hdd: bool,
    old_bios_fixes: bool,
    use_mbr_id: bool,
    /// Rufus IDC_DISK_ID combo index: 0 = "0x80 (Default)", 1-7 = masquerade
    /// as disk 2-8 (not persisted — Rufus resets it with the image)
    disk_id_sel: usize,
    /// Rufus mbr_selected_by_user: the user manually toggled the MBR box
    mbr_selected_by_user: bool,
    /// stored "Use Rufus MBR" box state while the control is disabled
    /// (Rufus EnableMBRBootOptions uMBRChecked dance)
    mbr_saved: Option<bool>,
    /// Rufus "Create extended label and icon files" (autorun.inf + autorun.ico)
    extended_label: bool,
    advanced_open: bool,
    fmt_advanced_open: bool,
    recent: Vec<String>,
    log: Vec<String>,
    log_open: bool,
    /// Scroll offset (in lines from the tail); None = follow the bottom
    /// like Rufus's auto-scrolling log edit
    log_scroll: Option<usize>,
    progress: Option<(String, f64)>, // (phase, 0..1)
    running: bool,
    status: String,
    status_is_error: bool,
    /// Rufus PrintStatusTimeout parity: timed status flashes revert to READY
    status_until: Option<Instant>,
    // interaction state
    open_dd: Option<DD>,
    /// scroll offset for long dropdown lists (download languages etc.)
    dd_scroll: usize,
    dd_rect: Option<(DD, Rect)>,
    confirm: bool,
    about: bool,
    slider_rect: Option<(Rect, u64)>, // (track, max GB)
    widgets: Vec<(Rect, Widget)>,
    focus_label: bool,
    // cancellation flag shared with the worker thread
    cancel_token: Option<Arc<AtomicBool>>,
    // parsed-ISO cache: (path, Some(summary) | None = not an ISO)
    iso_cache: Option<(String, Option<IsoSummary>)>,
    iso_dirty_at: Option<Instant>,
    last_dev_refresh: Instant,
    start_time: Instant,
    desired_h: u32,
    // built-in file browser overlay (fallback when no system picker)
    browser: Option<FileBrowser>,
    // Rufus check-icon dialog: image hashes
    hash_state: Option<HashState>,
    hash_open: bool,
    sha512_show: bool,
    // Rufus hidden settings (⚙ Settings dialog / Alt shortcuts)
    settings_open: bool,
    /// "Extra hashes (SHA512)" — show SHA-512 in the image hashes dialog
    extra_hashes: bool,
    /// "Use Rufus MBR" — off writes a plain MBR without boot code
    use_rufus_mbr: bool,
    /// "Force large FAT32 formatting" — keep FAT32 on drives > 32 GiB
    large_fat32: bool,
    /// "Preserve timestamps" — copy ISO file dates to the written files
    preserve_timestamps: bool,
    /// "Persistent log" — save the GUI log on exit
    persistent_log: bool,
    /// "Enable USB enumeration debug" (Rufus EnableUsbDebug, Alt-.): list the
    /// verbose device details in the log while scanning
    usb_debug: bool,
    // Rufus "Windows User Experience" dialog (Windows ISOs only)
    wue: Option<engine::WueOptions>,
    wue_open: bool,
    // Rufus "Version selection" dialog (Windows To Go editions)
    wim_index: u32,
    wim_pick: Vec<engine::WimImage>,
    wim_pick_open: bool,
    wim_pick_sel: usize,
    /// START pressed with WTG selected, waiting for the edition list
    awaiting_wim_pick: bool,
    wim_rx: Option<Receiver<Option<Vec<engine::WimImage>>>>,
    // file dialog channel
    dlg_rx: Option<Receiver<Option<String>>>,
    /// result of the "Save log" dialog+write: Ok(path) or Err(reason)
    log_save_rx: Option<Receiver<Result<String, String>>>,
    // Rufus "⬇ DOWNLOAD" dialog: official Windows/Linux ISO downloads
    dl: download::DlDialog,
    dl_rx: Option<Receiver<download::DlMsg>>,
    dl_cancel: Option<Arc<AtomicBool>>,
    fonts: Option<Fonts>,
}

#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}
impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Clone)]
enum Widget {
    Combo(DD),
    DropdownItem(DD, usize),
    DeviceInfo,
    BootSelect,
    BootSelectArrow,
    SliderTrack,
    AdvancedToggle,
    FmtAdvancedToggle,
    LabelField,
    ChkListHdd,
    ChkOldBios,
    ChkMbrId,
    ChkQuick,
    ChkBadblocks,
    /// Rufus "Create extended label and icon files" (autorun.inf/ico)
    ChkExtendedLabel,
    ChkVerify,
    IconGlobe,
    IconInfo,
    IconSettings,
    IconLog,
    Start,
    Stop,
    Close,
    ConfirmOk,
    ConfirmCancel,
    AboutClose,
    HashBtn,
    HashOk,
    WueChk(usize),
    WueOk,
    WueCancel,
    WimPickRow(usize),
    WimPickOk,
    WimPickCancel,
    SettingsChk(usize),
    SettingsClose,
    BrowserRow(usize),
    BrowserCancel,
    DlDownload,
    DlCancel,
    DlClose,
    /// log panel: Clear | Save | Close row (Rufus IDD_LOG parity)
    LogClear,
    LogSave,
    LogClose,
    LogScroll,
}

pub fn run(locale_override: Option<usize>) -> Result<()> {
    // need root for raw disk access
    if unsafe { libc::geteuid() } != 0 && std::env::var_os("RUFUS_LINUX_NOROOT").is_none() {
        // try pkexec relaunch once — pkexec resets the environment, so the
        // display/session variables must be forwarded explicitly or the
        // elevated window could never open
        let exe = std::env::current_exe()?;
        let mut env_args = Vec::new();
        for var in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XAUTHORITY",
            "XDG_RUNTIME_DIR",
            "XDG_SESSION_TYPE",
            // settings/downloads must land in the user's home, not root's
            "HOME",
        ] {
            if let Ok(v) = std::env::var(var) {
                env_args.push(format!("{var}={v}"));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !env_args.iter().any(|a| a.starts_with("XAUTHORITY=")) {
                env_args.push(format!("XAUTHORITY={home}/.Xauthority"));
            }
        }
        let status = std::process::Command::new("pkexec")
            .arg("env")
            .args(&env_args)
            .arg(&exe)
            .arg("gui")
            .status();
        match status {
            Ok(s) if s.success() => return Ok(()),
            _ => {
                eprintln!("warning: not running as root — device writes will fail. Re-run with sudo/pkexec.");
            }
        }
    }

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new(locale_override);
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    window: Option<Arc<Window>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    ctx: Option<Context<Arc<Window>>>,
    ui: Ui,
    mouse: (f32, f32),
    mouse_down: bool,
    slider_drag: bool,
    /// redraw only when something changed — a full redraw loop burns a CPU
    ui_dirty: bool,
    modifiers: winit::event::Modifiers,
    work_rx: Option<Receiver<WorkerMsg>>,
    work_tx: Sender<WorkerMsg>,
    hash_rx: Option<Receiver<Result<HashInfo>>>,
}

enum WorkerMsg {
    Progress(Progress),
    Done(Result<String>),
}

impl App {
    fn new(locale_override: Option<usize>) -> Self {
        let (tx, rx) = channel();
        // restore persisted settings (image, device, options)
        let saved = crate::config::load();
        // UI language: CLI override > saved setting > system locale (Rufus
        // first-run parity: GetUserDefaultUILanguage / LC_ALL here)
        let locale = locale_override
            .or_else(|| crate::locale::lang_index(&saved.locale))
            .unwrap_or_else(crate::locale::detect_system);
        let devices = device::list_devices(if saved.list_usb_hdd {
            device::DevFilter::UsbHddToo
        } else {
            device::DevFilter::Hotplug
        });
        let sel_dev = devices
            .iter()
            .position(|d| d.path == saved.last_device)
            .unwrap_or(0);
        let fs_sel = match saved.fs.as_str() {
            "ntfs" => 0,
            "exfat" => 2,
            _ => 1, // FAT32 (Default)
        };
        let has_image = !saved.image_path.is_empty();
        App {
            window: None,
            surface: None,
            ctx: None,
            ui: Ui {
                devices,
                sel_dev,
                locale,
                boot_mode: BootMode::DiskImage, // Rufus default
                image_option: saved.image_option as usize,
                image_path: saved.image_path,
                label: saved.label,
                fs_sel,
                gpt: saved.gpt,
                efi_only: saved.efi_only,
                quick: saved.quick,
                badblocks: saved.badblocks,
                bb_sel: saved.badblocks_sel as usize,
                verify: saved.verify,
                persist_gb_str: saved.persist_gb.to_string(),
                list_usb_hdd: saved.list_usb_hdd,
                old_bios_fixes: saved.old_bios_fixes,
                use_mbr_id: false,
                disk_id_sel: 0,
                mbr_selected_by_user: false,
                mbr_saved: None,
                extended_label: true,
                advanced_open: saved.advanced_open,
                fmt_advanced_open: false,
                recent: saved.recent_images,
                log: if saved.persistent_log {
                    // Rufus parity: a persistent log starts with a ctime-style
                    // session-start timestamp line
                    vec![crate::config::ctime_now(), crate::locale::tr(locale, "READY").to_string()]
                } else {
                    vec![crate::locale::tr(locale, "READY").to_string()]
                },
                log_open: false,
                log_scroll: None,
                progress: None,
                running: false,
                status: String::new(),
                status_is_error: false,
                status_until: None,
                open_dd: None,
                dd_scroll: 0,
                dd_rect: None,
                confirm: false,
                about: false,
                slider_rect: None,
                widgets: Vec::new(),
                focus_label: false,
                cancel_token: None,
                iso_cache: None,
                iso_dirty_at: if has_image { Some(Instant::now()) } else { None },
                last_dev_refresh: Instant::now(),
                start_time: Instant::now(),
                desired_h: 560,
                browser: None,
                hash_state: None,
                hash_open: false,
                sha512_show: false,
                settings_open: false,
                extra_hashes: saved.extra_hashes,
                use_rufus_mbr: saved.use_rufus_mbr,
                large_fat32: saved.large_fat32,
                preserve_timestamps: saved.preserve_timestamps,
                persistent_log: saved.persistent_log,
                usb_debug: saved.enable_usb_debug,
                wue: None,
                wue_open: false,
                wim_index: 1,
                wim_pick: Vec::new(),
                wim_pick_open: false,
                wim_pick_sel: 0,
                awaiting_wim_pick: false,
                wim_rx: None,
                dlg_rx: None,
                log_save_rx: None,
                dl: download::DlDialog::new(),
                dl_rx: None,
                dl_cancel: None,
                fonts: load_fonts(),
            },
            mouse: (0.0, 0.0),
            mouse_down: false,
            slider_drag: false,
            ui_dirty: true,
            modifiers: winit::event::Modifiers::default(),
            work_rx: Some(rx),
            work_tx: tx,
            hash_rx: None,
        }
    }

    fn save_settings(&self) {
        let fs = match self.ui.fs_sel {
            0 => "ntfs",
            2 => "exfat",
            _ => "fat32",
        };
        let s = crate::config::Settings {
            image_path: self.ui.image_path.clone(),
            label: self.ui.label.clone(),
            fs: fs.to_string(),
            image_option: self.ui.image_option as u8,
            gpt: self.ui.gpt,
            efi_only: self.ui.efi_only,
            verify: self.ui.verify,
            quick: self.ui.quick,
            badblocks: self.ui.badblocks,
            badblocks_sel: self.ui.bb_sel as u8,
            persist_gb: self.ui.persist_gb_str.trim().parse::<u64>().unwrap_or(0).min(64),
            list_usb_hdd: self.ui.list_usb_hdd,
            old_bios_fixes: self.ui.old_bios_fixes,
            advanced_open: self.ui.advanced_open,
            recent_images: self.ui.recent.clone(),
            last_device: self.ui.devices.get(self.ui.sel_dev).map(|d| d.path.clone()).unwrap_or_default(),
            extra_hashes: self.ui.extra_hashes,
            use_rufus_mbr: self.ui.use_rufus_mbr,
            large_fat32: self.ui.large_fat32,
            preserve_timestamps: self.ui.preserve_timestamps,
            persistent_log: self.ui.persistent_log,
            enable_usb_debug: self.ui.usb_debug,
            locale: crate::locale::LANGS
                .get(self.ui.locale)
                .map(|l| l.code.to_string())
                .unwrap_or_default(),
        };
        crate::config::save(&s);
    }

    /// Session log on exit (Rufus parity): Rufus always writes rufus.log when
    /// it exits — appended when the "Persistent log" setting is on, otherwise
    /// overwritten. (Ctrl-P parity; "persistent" only controls the append.)
    fn save_persistent_log(&self) {
        if let Some(p) = crate::config::save_log(&self.ui.log, self.ui.persistent_log) {
            eprintln!("log saved to {}", p.display());
        }
    }

    fn push_log(&mut self, s: impl Into<String>) {
        self.ui.log.push(s.into());
        // deliberate deviation from Rufus (~2 GiB cap): bounded memory, but
        // big enough that "Save log" keeps the full session
        if self.ui.log.len() > 1000 {
            self.ui.log.remove(0);
            // keep the scroll window glued to the tail when we drop a line
            if let Some(off) = self.ui.log_scroll.as_mut() {
                *off = off.saturating_sub(1);
            }
        }
    }

    /// Rufus PrintStatusTimeout parity: flash "<setting>: enabled/disabled" in
    /// the status bar (when idle) and into the log; the flash reverts to
    /// READY after 3 seconds, like the original's timed status. `name` is the
    /// English setting string (the translation key).
    fn status_timeout(&mut self, name: &'static str, on: bool) {
        let l = self.ui.locale;
        // the generated tables key these as Rufus's original "%s enabled" /
        // "%s disabled" msgids; trf also substitutes a fallback key's own %s
        let msg = if on {
            crate::locale::trf(l, "%s enabled", &[crate::locale::tr(l, name)])
        } else {
            crate::locale::trf(l, "%s disabled", &[crate::locale::tr(l, name)])
        };
        if !self.ui.running {
            self.ui.status = msg.clone();
            self.ui.status_is_error = false;
            self.ui.status_until = Some(Instant::now() + Duration::from_secs(3));
        }
        self.push_log(msg);
        self.ui_dirty = true;
    }

    /// Filesystem combo entries with Rufus's large-FAT32 gating: FAT32
    /// disappears above 32 GiB unless forced; the entry gets Rufus's "Large"
    /// prefix when the large-drive path is in play.
    fn fs_entries(&self) -> Vec<(usize, String)> {
        let dev_size = self
            .ui
            .devices
            .get(self.ui.sel_dev)
            .map(|d| d.size)
            .unwrap_or(0);
        let large = dev_size > LARGE_FAT32_SIZE || self.ui.large_fat32;
        (0..FS_NAMES.len())
            .filter(|&i| i != 1 || fat32_allowed(dev_size, self.ui.large_fat32))
            .map(|i| {
                let name = if i == 1 && large {
                    format!("{} {}", crate::locale::tr(self.ui.locale, "Large"), Self::fs_display(self.ui.locale, i))
                } else {
                    Self::fs_display(self.ui.locale, i)
                };
                (i, name)
            })
            .collect()
    }

    /// Translated filesystem combo text: Rufus composes the "(Default)"
    /// suffix from the "%s (Default)" msgid.
    fn fs_display(l: usize, i: usize) -> String {
        match i {
            1 => crate::locale::trf(l, "%s (Default)", &["FAT32"]),
            2 => "exFAT".into(),
            _ => "NTFS".into(),
        }
    }

    /// IDC_DISK_ID combo items, translated like Rufus's
    /// "0x80 (Default)" / "0x%02X (Disk %d)" msgids.
    fn disk_id_items(l: usize) -> Vec<String> {
        (0..DISK_ID_NAMES.len())
            .map(|i| {
                let id = DISK_ID_NAMES[i].1;
                if i == 0 {
                    crate::locale::trf(l, "%s (Default)", &[&format!("0x{id:02X}")])
                } else {
                    crate::locale::trf(l, "0x{0} (Disk {1})", &[&format!("{id:02X}"), &(i + 1).to_string()])
                }
            })
            .collect()
    }

    /// Cluster-size combo text with the translated "(Default)" suffix.
    fn cluster_display(l: usize, bytes: usize) -> String {
        crate::locale::trf(l, "%s (Default)", &[&format!("{} {}", bytes, crate::locale::tr(l, "bytes"))])
    }

    /// When FAT32 is hidden for the selected device, fall back to NTFS and say
    /// so (Rufus silently picks the first allowed filesystem).
    fn revalidate_fs_sel(&mut self) {
        let Some(d) = self.ui.devices.get(self.ui.sel_dev) else {
            return;
        };
        if self.ui.fs_sel == 1 && !fat32_allowed(d.size, self.ui.large_fat32) {
            self.ui.fs_sel = 0; // NTFS
            self.push_log(crate::locale::tr(
                self.ui.locale,
                "drive is larger than 32 GiB — FAT32 hidden (⚙ Settings or Alt-L to force it); switched to NTFS",
            ));
        }
    }

    /// Rufus "magic" Alt keys: toggle the hidden settings in place, like the
    /// original's WM_SYSKEYDOWN handlers (Alt-H/A/L/T).
    fn handle_alt_key(&mut self, k: &str) {
        match k.to_ascii_uppercase().as_str() {
            "H" => {
                self.ui.extra_hashes = !self.ui.extra_hashes;
                self.status_timeout("Extra hashes (SHA512)", self.ui.extra_hashes);
            }
            "A" => {
                self.ui.use_rufus_mbr = !self.ui.use_rufus_mbr;
                self.status_timeout("Use Rufus MBR", self.ui.use_rufus_mbr);
            }
            "L" => {
                self.ui.large_fat32 = !self.ui.large_fat32;
                self.status_timeout("Force large FAT32 formatting", self.ui.large_fat32);
                self.revalidate_fs_sel();
            }
            "T" => {
                self.ui.preserve_timestamps = !self.ui.preserve_timestamps;
                self.status_timeout("Preserve timestamps", self.ui.preserve_timestamps);
            }
            // Rufus Alt-. : enable/disable USB enumeration debug — the
            // "trace" switch; it is persisted (EnableUsbDebug) and rescan the
            // devices to list their verbose details in the log
            "." => {
                self.ui.usb_debug = !self.ui.usb_debug;
                self.status_timeout("USB debug", self.ui.usb_debug);
                self.save_settings();
                self.refresh_devices();
            }
            _ => {}
        }
    }

    fn refresh_devices(&mut self) {
        let sel_path = self.ui.devices.get(self.ui.sel_dev).map(|d| d.path.clone());
        // "List USB Hard Drives" checkbox, like Rufus: USB hard drives join
        // the hotplug list; internal disks are never listed
        self.ui.devices = device::list_devices(if self.ui.list_usb_hdd {
            device::DevFilter::UsbHddToo
        } else {
            device::DevFilter::Hotplug
        });
        // Rufus uuprintf parity: with USB enumeration debug (Alt-.) on, every
        // scan lists the verbose details of the devices it processed
        if self.ui.usb_debug {
            let trace: Vec<String> = self.ui.devices.iter().map(|d| {
                format!(
                    "trace: {} \"{} {}\" {} tran={} bus={} removable={} serial={}",
                    d.path,
                    d.vendor,
                    d.model,
                    d.size_human(),
                    d.tran,
                    d.bus,
                    if d.removable { "yes" } else { "no" },
                    d.serial
                )
            }).collect();
            for t in trace {
                self.push_log(t);
            }
        }
        self.ui.sel_dev = sel_path
            .and_then(|p| self.ui.devices.iter().position(|d| d.path == p))
            .unwrap_or(0);
        self.revalidate_fs_sel();
    }

    /// Rufus-style option suggestions derived from the ISO contents.
    fn apply_suggestions(&mut self, s: &IsoSummary) {
        let l = self.ui.locale;
        if s.large_wim && self.ui.fs_sel != 0 {
            // FAT32 cannot hold a >4 GiB file; NTFS + the UEFI:NTFS helper
            // partition avoids depending on wimlib being installed
            self.ui.fs_sel = 0; // NTFS
            self.push_log(crate::locale::tr(
                l,
                "install.wim > 4 GiB — switched file system to NTFS",
            ));
        }
        if s.has_efi && !s.has_bios && !self.ui.efi_only {
            self.ui.efi_only = true;
            self.push_log(crate::locale::tr(
                l,
                "ISO is UEFI-only — target system set to UEFI (non CSM)",
            ));
        } else if s.has_bios && !s.has_efi && self.ui.efi_only {
            self.ui.efi_only = false;
            self.push_log(crate::locale::tr(
                l,
                "ISO has BIOS boot — target system set to BIOS or UEFI",
            ));
        }
    }

    /// Rufus EnableMBRBootOptions: is the "Use Rufus MBR with BIOS ID" row
    /// (checkbox + BIOS ID combo) usable for the current selection? It only
    /// applies to MBR + BIOS-target bootable writes; for images it needs a
    /// BIOS-bootable, non-DD Windows image.
    fn mbr_row_enabled(&self) -> bool {
        let mut enabled = !self.ui.gpt
            && !self.ui.efi_only
            && self.ui.boot_mode != BootMode::NonBootable;
        if self.ui.boot_mode == BootMode::DiskImage {
            match self.ui.iso_cache.as_ref().and_then(|(_, s)| s.clone()) {
                None => enabled = false, // raw/DD image: no volume boot code
                Some(s) => {
                    if !s.has_bios || !(s.has_bootmgr || s.has_winpe || s.is_windows) {
                        enabled = false;
                    }
                }
            }
        }
        enabled
    }

    /// Rufus EnableExtendedLabel: the box is disabled for raw/DD writes
    /// (there is no volume to hold autorun.inf). The supported main
    /// filesystems (FAT32/NTFS/exFAT) always allow it.
    fn extended_label_enabled(&self) -> bool {
        !matches!(self.ui.iso_cache, Some((_, None)))
            || self.ui.boot_mode != BootMode::DiskImage
    }

    /// Rufus SetMBRProps: auto-manage the "Use Rufus MBR with BIOS ID"
    /// checkbox and the BIOS ID combo as the selection changes, unless the
    /// user manually toggled the box (mbr_selected_by_user).
    fn update_mbr_props(&mut self) {
        let is_image_mode = self.ui.boot_mode == BootMode::DiskImage;
        if !self.mbr_row_enabled() {
            // Rufus stores the box state while the control is disabled and
            // restores it when it comes back (uMBRChecked dance)
            if self.ui.mbr_saved.is_none() {
                self.ui.mbr_saved = Some(self.ui.use_mbr_id);
            }
            self.ui.use_mbr_id = false;
            self.ui.disk_id_sel = 0;
            if is_image_mode {
                self.ui.mbr_selected_by_user = false;
            }
            return;
        }
        if let Some(saved) = self.ui.mbr_saved.take() {
            self.ui.use_mbr_id = saved;
            self.ui.disk_id_sel = 0;
        }
        if is_image_mode && !self.ui.mbr_selected_by_user {
            let Some(s) = self.ui.iso_cache.as_ref().and_then(|(_, s)| s.clone()) else {
                return;
            };
            if self.ui.image_option == 1 || self.ui.fs_sel != 0 {
                // WTG or non-NTFS: the Rufus MBR is not applied (SetMBRProps
                // unchecks and resets the combo)
                self.ui.use_mbr_id = false;
                self.ui.disk_id_sel = 0;
            } else {
                self.ui.use_mbr_id = s.has_bootmgr || s.has_winpe;
                // WinPE 1.x sticks masquerade as disk 2 by default
                self.ui.disk_id_sel = if s.has_winpe { 1 } else { 0 };
            }
        }
    }

    /// Re-parse the ISO behind the current image path (info + suggestions).
    fn reparse_iso(&mut self) {
        let path = self.ui.image_path.clone();
        let summary = std::fs::File::open(&path)
            .ok()
            .and_then(|mut f| iso9660::parse_iso(&mut f).ok())
            .map(|iso| summarize_iso(&iso));
        self.ui.iso_cache = Some((path.clone(), summary));
        // Rufus computes the image hashes in the background as soon as an
        // image is selected (MD5/SHA1/SHA256; SHA512 via Alt-H in the dialog)
        let (htx, hrx) = channel();
        self.hash_rx = Some(hrx);
        self.ui.hash_state = Some(HashState::Computing);
        self.ui.hash_open = false;
        self.ui.sha512_show = false;
        // a new image invalidates the Windows To Go edition list/choice
        self.ui.wim_pick.clear();
        self.ui.wim_pick_open = false;
        self.ui.wim_index = 1;
        self.ui.awaiting_wim_pick = false;
        let hpath = path.clone();
        std::thread::spawn(move || {
            let _ = htx.send(compute_image_hashes(std::path::Path::new(&hpath)));
        });
        if let Some(s) = self.ui.iso_cache.as_ref().and_then(|(_, s)| s.clone()) {
            // Rufus parity: the Volume label field follows the selected image
            // (the user can still type a custom label afterwards)
            self.ui.label = crate::engine::sanitize_fat_label(&s.volume_id);
            self.apply_suggestions(&s);
            self.push_log(format!(
                "{}: label '{}', {} files, {}",
                path.rsplit('/').next().unwrap_or(&path),
                s.volume_id,
                s.file_count,
                fmt_size(s.total_bytes)
            ));
        } else {
            self.push_log(crate::locale::tr(
                self.ui.locale,
                "image is not ISO9660 — raw/DD mode will be used",
            ));
        }
        self.update_mbr_props();
    }

    /// Flag the ISO cache stale if the image path actually changed.
    fn mark_image_dirty(&mut self) {
        let changed = self
            .ui
            .iso_cache
            .as_ref()
            .map(|(p, _)| *p != self.ui.image_path)
            .unwrap_or(true);
        if changed {
            self.ui.iso_dirty_at = Some(Instant::now());
        }
    }

    fn select_dd(&mut self, dd: DD, idx: usize) {
        match dd {
            DD::Device => {
                if idx < self.ui.devices.len() {
                    self.ui.sel_dev = idx;
                    // a newly picked drive may be too large for FAT32
                    self.revalidate_fs_sel();
                }
            }
            DD::Boot => {
                let l = self.ui.locale;
                self.ui.boot_mode = match idx {
                    0 => BootMode::NonBootable,
                    1 => BootMode::FreeDos,
                    _ => BootMode::DiskImage,
                };
                match self.ui.boot_mode {
                    BootMode::NonBootable => {
                        self.push_log(crate::locale::tr(
                            l,
                            "Non-bootable mode: the device will be wiped, partitioned and formatted",
                        ));
                    }
                    BootMode::FreeDos => {
                        self.ui.fs_sel = 1; // FAT32 — DOS requires it
                        self.push_log(crate::locale::tr(
                            l,
                            "FreeDOS mode: FAT32 volume + FreeDOS boot files",
                        ));
                    }
                    BootMode::DiskImage => {
                        if self.ui.image_path.is_empty() {
                            self.push_log(crate::locale::tr(
                                l,
                                "Disk or ISO image selected — click SELECT to choose a file",
                            ));
                        }
                    }
                }
            }
            DD::Recent => {
                // item 0 is the "⬇ DOWNLOAD" action, the rest are the recent
                // images (the split-SELECT dropdown merges both buttons)
                if idx == 0 {
                    self.open_download();
                } else if let Some(p) = self.ui.recent.get(idx - 1).cloned() {
                    self.ui.image_path = p;
                    self.mark_image_dirty();
                }
            }
            DD::ImageOption => {
                // Rufus IMOP_WIN_STANDARD / IMOP_WIN_TO_GO
                let has_wininst = self
                    .ui
                    .iso_cache
                    .as_ref()
                    .and_then(|(_, s)| s.as_ref())
                    .map(|s| s.has_wininst)
                    .unwrap_or(false);
                let new_opt = if has_wininst { idx.min(1) } else { 0 };
                if new_opt != self.ui.image_option {
                    self.ui.image_option = new_opt;
                    if new_opt == 1 {
                        self.ui.wim_index = 1; // reset edition choice
                        self.push_log(crate::locale::tr(
                            self.ui.locale,
                            "Windows To Go: the OS image from install.wim will be applied (NTFS only)",
                        ));
                    }
                }
            }
            DD::Passes => self.ui.bb_sel = idx.min(4),
            DD::DiskId => self.ui.disk_id_sel = idx.min(DISK_ID_NAMES.len() - 1),
            DD::Lang => {
                // Rufus applies the language immediately and persists it
                if idx < crate::locale::LANGS.len() && idx != self.ui.locale {
                    self.ui.locale = idx;
                    self.save_settings();
                    self.ui_dirty = true;
                }
            }
            DD::Scheme => {
                // Rufus couples scheme -> target system automatically
                self.ui.gpt = idx == 1;
                self.ui.efi_only = idx == 1;
            }
            DD::Target => self.ui.efi_only = idx == 1,
            DD::Fs => {
                // the combo can hide FAT32 on large drives (Rufus
                // LARGE_FAT32_SIZE) — map the filtered index back
                if let Some((i, _)) = self.fs_entries().get(idx) {
                    self.ui.fs_sel = *i;
                }
            }
            // ⬇ DOWNLOAD dialog dropdowns — handled by the download module
            DD::DlRelease | DD::DlEdition | DD::DlLanguage | DD::DlArch | DD::DlVariant => {
                self.dl_select(dd, idx);
            }
        }
        // selections that change the "Use Rufus MBR with BIOS ID" applicability
        if matches!(dd, DD::Boot | DD::ImageOption | DD::Scheme | DD::Target | DD::Fs) {
            self.update_mbr_props();
        }
    }

    fn dd_items(&self, dd: DD) -> Vec<String> {
        match dd {
            DD::Lang => crate::locale::LANGS.iter().map(|l| l.name.to_string()).collect(),
            DD::Device => self
                .ui
                .devices
                .iter()
                .map(|d| {
                    if d.model.is_empty() {
                        format!("{} [{}]", d.path, d.size_human())
                    } else {
                        format!("{} [{}]", d.model, d.size_human())
                    }
                })
                .collect(),
            DD::Boot => BOOT_MODE_NAMES
                .iter()
                .map(|s| crate::locale::tr(self.ui.locale, s).to_string())
                .collect(),
            DD::Recent => {
                // the DOWNLOAD action sits at item 0, above the recent images
                let mut v = vec![format!(
                    "\u{2193} {}",
                    crate::locale::tr(self.ui.locale, "Download")
                )];
                v.extend(
                    self.ui
                        .recent
                        .iter()
                        .map(|p| p.rsplit('/').next().unwrap_or(p).to_string()),
                );
                v
            }
            DD::ImageOption => {
                // Rufus shows "Windows To Go" only when the ISO ships an
                // install.wim/esd (HAS_WININTOGO)
                let has_wininst = self
                    .ui
                    .iso_cache
                    .as_ref()
                    .and_then(|(_, s)| s.as_ref())
                    .map(|s| s.has_wininst)
                    .unwrap_or(false);
                if has_wininst {
                    vec![
                        crate::locale::tr(self.ui.locale, "Standard Windows installation").to_string(),
                        crate::locale::tr(self.ui.locale, "Windows To Go").to_string(),
                    ]
                } else {
                    vec![crate::locale::tr(self.ui.locale, "Standard Windows installation").to_string()]
                }
            }
            DD::Scheme => SCHEME_NAMES
                .iter()
                .map(|s| crate::locale::tr(self.ui.locale, s).to_string())
                .collect(),
            DD::Target => TARGET_NAMES
                .iter()
                .map(|s| crate::locale::tr(self.ui.locale, s).to_string())
                .collect(),
            DD::Fs => self.fs_entries().into_iter().map(|(_, s)| s).collect(),
            DD::Passes => crate::engine::BB_PASSES_NAMES
                .iter()
                .map(|s| crate::locale::tr(self.ui.locale, s).to_string())
                .collect(),
            DD::DiskId => Self::disk_id_items(self.ui.locale),
            DD::DlRelease | DD::DlEdition | DD::DlLanguage | DD::DlArch | DD::DlVariant => {
                download::items_for_dialog(&self.ui.dl, dd)
            }
        }
    }

    fn dd_selected(&self, dd: DD) -> usize {
        match dd {
            DD::Device => self.ui.sel_dev,
            DD::Boot => match self.ui.boot_mode {
                BootMode::NonBootable => 0,
                BootMode::FreeDos => 1,
                BootMode::DiskImage => 2,
            },
            DD::Recent => self
                .ui
                .recent
                .iter()
                .position(|p| *p == self.ui.image_path)
                .map(|i| i + 1) // item 0 is the DOWNLOAD action
                .unwrap_or(usize::MAX),
            DD::ImageOption => self.ui.image_option.min(1),
            DD::Scheme => if self.ui.gpt { 1 } else { 0 },
            DD::Target => if self.ui.efi_only { 1 } else { 0 },
            DD::Fs => self
                .fs_entries()
                .iter()
                .position(|(i, _)| *i == self.ui.fs_sel.min(2))
                .unwrap_or(0),
            DD::Passes => self.ui.bb_sel.min(4),
            DD::DiskId => self.ui.disk_id_sel.min(DISK_ID_NAMES.len() - 1),
            DD::Lang => self.ui.locale,
            DD::DlRelease | DD::DlEdition | DD::DlLanguage | DD::DlArch | DD::DlVariant => {
                self.ui.dl.selection(dd)
            }
        }
    }

    fn start_write(&mut self) {
        let l = self.ui.locale;
        // Rufus shows the destroy warning before starting
        if self.ui.running {
            return;
        }
        if self.ui.devices.is_empty() {
            self.ui.status = crate::locale::tr(l, "Please select a device first.").to_string();
            self.ui.status_is_error = true;
            return;
        }
        if self.ui.boot_mode == BootMode::DiskImage && self.ui.image_path.is_empty() {
            self.ui.status = crate::locale::tr(l, "Please select an image with SELECT first.").to_string();
            self.ui.status_is_error = true;
            return;
        }
        self.ui.confirm = true;
    }

    /// Continuation of the START chain after the Windows To Go edition
    /// picker: Rufus then shows the "Windows User Experience" dialog.
    fn open_wue_or_start(&mut self, windows_iso: bool) {
        if windows_iso && self.ui.wue.is_none() {
            self.ui.wue = Some(engine::WueOptions {
                // Rufus default selection (UNATTEND_DEFAULT_SELECTION_MASK,
                // rufus.h): TPM/SecureBoot bypass + online-account removal +
                // offline internal disks — data collection stays opt-in
                bypass_tpm: true,
                no_online_account: true,
                disable_data_collection: false,
                local_account: false,
                regional_same_as_user: false,
                offline_internal_drives: true,
            });
            self.ui.wue_open = true;
            self.ui_dirty = true;
        } else {
            self.start_write();
        }
    }

    /// Windows To Go will actually run for the pending selection (Rufus
    /// offers it for a Windows ISO that ships install.wim + the image option
    /// set to "Windows To Go"). Takes &Ui (not &self) so it is callable from
    /// the draw pass, where self.ctx/surface are mutably borrowed.
    fn pending_is_wtg(ui: &Ui) -> bool {
        let windows_iso = matches!(
            (&ui.iso_cache, ui.boot_mode),
            (Some((p, Some(s))), BootMode::DiskImage) if *p == ui.image_path && s.is_windows
        );
        windows_iso
            && ui.image_option == 1
            && ui
                .iso_cache
                .as_ref()
                .and_then(|(_, s)| s.as_ref())
                .map(|s| s.has_wininst)
                .unwrap_or(false)
    }

    fn do_start(&mut self) {
        if self.ui.running {
            return;
        }
        if self.ui.devices.is_empty() {
            return;
        }
        // "Windows To Go" image option (Disk image mode + Windows ISO only)
        let windows_iso = matches!(
            (&self.ui.iso_cache, self.ui.boot_mode),
            (Some((p, Some(s))), BootMode::DiskImage) if *p == self.ui.image_path && s.is_windows
        );
        let wtg = Self::pending_is_wtg(&self.ui);
        let write_mode = match self.ui.boot_mode {
            BootMode::NonBootable => engine::WriteMode::NonBootable,
            BootMode::FreeDos => engine::WriteMode::FreeDos,
            BootMode::DiskImage if wtg => engine::WriteMode::WindowsToGo,
            BootMode::DiskImage => engine::WriteMode::DiskImage,
        };
        if write_mode == engine::WriteMode::DiskImage && self.ui.image_path.is_empty() {
            return;
        }
        let dev = self.ui.devices[self.ui.sel_dev].clone();
        let image = if matches!(
            write_mode,
            engine::WriteMode::DiskImage | engine::WriteMode::WindowsToGo
        ) {
            std::path::PathBuf::from(&self.ui.image_path)
        } else {
            std::path::PathBuf::new()
        };
        let persist_gb = self.ui.persist_gb_str.trim().parse::<u64>().unwrap_or(0).min(64);
        let opts = Options {
            fs: if write_mode == engine::WriteMode::FreeDos {
                engine::FsKind::Fat32 // DOS requires FAT
            } else if wtg {
                engine::FsKind::Ntfs // Rufus: WTG only works for NTFS
            } else {
                match self.ui.fs_sel {
                    0 => engine::FsKind::Ntfs,
                    2 => engine::FsKind::Exfat,
                    _ => engine::FsKind::Fat32,
                }
            },
            scheme: if self.ui.gpt { PartitionScheme::Gpt } else { PartitionScheme::Mbr },
            fs_label: self.ui.label.clone(),
            quick: self.ui.quick,
            verify: self.ui.verify,
            efi_only: self.ui.efi_only,
            // no persistence partition alongside Windows To Go (Rufus parity)
            persistence_mib: if wtg { 0 } else { persist_gb * 1024 },
            write_mode,
            old_bios_fixes: self.ui.old_bios_fixes,
            bb_test: if self.ui.badblocks {
                engine::BadBlocksTest::from_sel(self.ui.bb_sel.min(4))
            } else {
                engine::BadBlocksTest::Disabled
            },
            wue: if windows_iso { self.ui.wue } else { None },
            allow_system_disk: false, // the GUI never overwrites the system disk
            wim_index: self.ui.wim_index.max(1),
            // the settings-dialog "Use Rufus MBR" is the 4.x master override;
            // the visible checkbox drives IDC_RUFUS_MBR. When the row doesn't
            // apply (Linux image, GPT, UEFI-only, non-bootable) the checkbox
            // state is ignored — Rufus uses the bootloader's own MBR there,
            // and this project's equivalent is its Rufus MBR boot code.
            rufus_mbr: if self.mbr_row_enabled() {
                self.ui.use_rufus_mbr && self.ui.use_mbr_id
            } else {
                self.ui.use_rufus_mbr
            },
            disk_id: if self.mbr_row_enabled() && self.ui.use_mbr_id {
                DISK_ID_NAMES[self.ui.disk_id_sel.min(DISK_ID_NAMES.len() - 1)].1
            } else {
                0x80
            },
            autorun: self.ui.extended_label,
            preserve_timestamps: self.ui.preserve_timestamps,
        };
        let token = Arc::new(AtomicBool::new(false));
        self.ui.cancel_token = Some(token.clone());
        let tx = self.work_tx.clone();
        self.ui.running = true;
        self.ui.progress = Some(("start".into(), 0.0));
        self.ui.status.clear();
        self.ui.status_is_error = false;
        self.push_log(format!("START: {} -> {}", image.display(), dev.path));
        std::thread::spawn(move || {
            let eng = Engine::new(opts).with_cancel(token);
            let res = eng.write_iso(&dev, &image, &mut |p: Progress| {
                let _ = tx.send(WorkerMsg::Progress(p));
            });
            let _ = tx.send(WorkerMsg::Done(res.map(|_| "Done".to_string())));
        });
    }

    /// GB value from the slider x position.
    fn slider_value_at(&self, x: f32) -> u64 {
        if let Some((r, max)) = self.ui.slider_rect {
            if r.w <= 10.0 || max == 0 {
                return 0;
            }
            let t = ((x - r.x - 5.0) / (r.w - 10.0)).clamp(0.0, 1.0);
            (t * max as f32).round() as u64
        } else {
            0
        }
    }

    fn spawn_file_dialog(&mut self) {
        let (tx, rx) = channel();
        self.ui.dlg_rx = Some(rx);
        std::thread::spawn(move || {
            // desktop file pickers, in order of preference
            let candidates: [(&str, &[&str]); 3] = [
                ("zenity", &["--file-selection", "--title=Select image file"]),
                ("kdialog", &["--getopenfilename", ".", "*.iso *.img *.gz"]),
                ("yad", &["--file-selection", "--title=Select image file"]),
            ];
            for (prog, args) in candidates {
                let out = std::process::Command::new(prog).args(args).output();
                if let Ok(o) = out {
                    if o.status.success() {
                        let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        if !text.is_empty() {
                            let _ = tx.send(Some(text));
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(None);
        });
    }

    /// Rufus "Save" log button: a save dialog (default name rufus.log,
    /// "Rufus log | *.log" + "All files" filters like the original's
    /// IFileDialog) then write the session log to the chosen path. An empty
    /// log does nothing, like Rufus; the log text is snapshotted at click
    /// time exactly like Rufus reads the edit control before the dialog.
    fn spawn_save_log_dialog(&mut self) {
        if self.ui.log.is_empty() {
            return;
        }
        let (tx, rx) = channel();
        self.ui.log_save_rx = Some(rx);
        let lines = self.ui.log.clone();
        let l = self.ui.locale;
        std::thread::spawn(move || {
            // desktop save pickers, in order of preference; Rufus starts in
            // the user profile directory with the name pre-filled
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let zen_name = format!("--filename={home}/rufus.log");
            let yad_name = zen_name.clone();
            let candidates: [(&str, Vec<&str>); 3] = [
                (
                    "zenity",
                    vec![
                        "--file-selection",
                        "--save",
                        "--title=Save log file",
                        &zen_name,
                        "--file-filter=Rufus log | *.log",
                        "--file-filter=All files | *.*",
                    ],
                ),
                ("kdialog", vec!["--getsavefilename", &home, "Rufus log|*.log\nAll files|*.*"]),
                ("yad", vec!["--file", "--save", "--title=Save log file", &yad_name]),
            ];
            let Some(path) = candidates.iter().find_map(|(prog, args)| {
                let out = std::process::Command::new(prog).args(args).output().ok()?;
                if !out.status.success() {
                    return None;
                }
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if text.is_empty() { None } else { Some(text) }
            }) else {
                let _ = tx.send(Err(
                    crate::locale::tr(l, "no system file picker found — log not saved").to_string(),
                ));
                return;
            };
            match crate::config::write_log_at(std::path::Path::new(&path), &lines, false) {
                Ok(()) => {
                    let _ = tx.send(Ok(path));
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("could not save log to {path}: {e}")));
                }
            }
        });
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Rufus-linux")
            .with_inner_size(LogicalSize::new(W as f64, 560.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let ctx = Context::new(window.clone()).expect("softbuffer ctx");
        let surface = Surface::new(&ctx, window.clone()).expect("surface");
        self.ctx = Some(ctx);
        self.surface = Some(surface);
        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.save_settings();
                self.save_persistent_log();
                el.exit();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let s = self.window.as_ref().unwrap().scale_factor() as f32;
                self.mouse = (position.x as f32 / s, position.y as f32 / s);
                self.ui_dirty = true;
                // live slider drag
                if self.slider_drag && self.mouse_down {
                    let gb = self.slider_value_at(self.mouse.0);
                    self.ui.persist_gb_str = gb.to_string();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                self.ui_dirty = true;
                match state {
                    ElementState::Pressed => {
                        self.mouse_down = true;
                        self.handle_click();
                    }
                    ElementState::Released => {
                        self.mouse_down = false;
                        self.slider_drag = false;
                    }
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m;
                self.ui_dirty = true;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(b) = &mut self.ui.browser {
                    let lines = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, dy) => dy as i32,
                        winit::event::MouseScrollDelta::PixelDelta(px) => px.y as i32,
                    };
                    b.scroll(lines);
                    self.ui_dirty = true;
                } else if let Some(dd) = self.ui.open_dd {
                    // scroll long dropdown lists (download languages etc.)
                    let lines = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, dy) => dy as i32,
                        winit::event::MouseScrollDelta::PixelDelta(px) => px.y as i32,
                    };
                    let n = self.dd_items(dd).len();
                    let max = n.saturating_sub(15);
                    let next = self.ui.dd_scroll as i64 - lines as i64;
                    self.ui.dd_scroll = next.clamp(0, max as i64) as usize;
                    self.ui_dirty = true;
                } else if let Some((r, _)) = self
                    .ui
                    .widgets
                    .iter()
                    .find(|(_, w)| matches!(w, Widget::LogScroll))
                {
                    // scroll the log panel while the pointer is over it
                    // (Rufus's log edit has a vertical scrollbar)
                    if r.contains(self.mouse.0, self.mouse.1) {
                        let lines = match delta {
                            winit::event::MouseScrollDelta::LineDelta(_, dy) => dy as i32,
                            winit::event::MouseScrollDelta::PixelDelta(px) => px.y as i32,
                        };
                        let visible = ((r.h - 6.0) / 14.0) as usize; // matches render
                        let len = self.ui.log.len();
                        let cur = self
                            .ui
                            .log_scroll
                            .unwrap_or_else(|| len.saturating_sub(visible));
                        let next = (cur as i64 - lines as i64).clamp(0, len as i64) as usize;
                        self.ui.log_scroll = Some(next);
                        self.ui_dirty = true;
                    }
                }
            }
            WindowEvent::KeyboardInput { event: ke, .. } => {
                self.ui_dirty = true;
                // X11/winit delivers `text` (and the logical key) for BOTH the
                // KeyPress and the KeyRelease of every keystroke — handling
                // both is what made every typed character appear twice.
                // Everything below must run for the press only.
                if ke.state == ElementState::Pressed {
                    // Ctrl+V pastes into the focused field; Ctrl+P toggles the
                    // "Persistent log" hidden setting; Ctrl+L toggles the log
                    // (all Rufus parity)
                    if self.modifiers.state().control_key() {
                        if matches!(&ke.logical_key, Key::Character(s) if s.as_str().eq_ignore_ascii_case("v")) {
                            self.paste_clipboard();
                        }
                        if matches!(&ke.logical_key, Key::Character(s) if s.as_str().eq_ignore_ascii_case("p")) {
                            self.ui.persistent_log = !self.ui.persistent_log;
                            self.status_timeout("Persistent log", self.ui.persistent_log);
                        }
                        if matches!(&ke.logical_key, Key::Character(s) if s.as_str().eq_ignore_ascii_case("l")) {
                            self.ui.log_open = !self.ui.log_open;
                        }
                    }
                    // Alt+key types nothing — the char is a magic-key chord
                    if let Some(text) = ke.text.filter(|_| !self.modifiers.state().alt_key()) {
                        for c in text.chars() {
                            self.handle_char(c);
                        }
                    }
                    match ke.logical_key {
                        // Rufus magic Alt keys toggle the hidden settings in place
                        Key::Character(c)
                            if self.modifiers.state().alt_key()
                                && !self.ui.confirm
                                && !self.ui.wue_open
                                && !self.ui.wim_pick_open
                                && self.ui.browser.is_none() =>
                        {
                            self.handle_alt_key(c.as_str());
                        }
                        Key::Named(NamedKey::Backspace) => self.handle_backspace(),
                        Key::Named(NamedKey::Escape) => {
                            // Rufus: Esc cancels the dialog / closes the dropdown
                            if self.ui.dl.is_open() {
                                self.dl_close();
                            }
                            if self.ui.confirm {
                                self.ui.confirm = false;
                            }
                            if self.ui.about {
                                self.ui.about = false;
                            }
                            if self.ui.wim_pick_open {
                                self.ui.wim_pick_open = false;
                            }
                            if self.ui.settings_open {
                                self.ui.settings_open = false;
                            }
                            self.ui.open_dd = None;
                        }
                        Key::Named(NamedKey::Enter) => {
                            if self.ui.confirm {
                                self.ui.confirm = false;
                                self.do_start();
                            } else if !self.ui.focus_label {
                                self.start_write();
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.render();
                self.ui_dirty = false;
            }
            _ => {}
        }
        // pump worker messages
        self.pump_worker();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // Idle ticks must pump the worker channels too, or status flashes
        // would never revert to READY (and downloads/devices would stall)
        // until the next mouse event.
        self.pump_worker();
        // Throttle redraws: redrawing at max rate burned a whole CPU core.
        // Dirty state (input, progress, drag) redraws at ~20 fps; idle ticks
        // at 2 Hz just to advance the timer and poll devices.
        let now = Instant::now();
        if self.ui_dirty {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            el.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(50)));
        } else {
            el.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(500)));
        }
    }
}

impl App {
    fn pump_worker(&mut self) {
        let l = self.ui.locale;
        let mut msgs = Vec::new();
        if let Some(rx) = &self.work_rx {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        if !msgs.is_empty() {
            self.ui_dirty = true;
        }
        for msg in msgs {
            match msg {
                    WorkerMsg::Progress(p) => {
                        let total = p.total.max(1) as f64;
                        self.ui.progress = Some((p.phase.clone(), p.done as f64 / total));
                        self.push_log(format!("[{}] {} {}/{}", p.phase, p.message, p.done, p.total));
                    }
                WorkerMsg::Done(res) => {
                    self.ui.running = false;
                    self.ui.cancel_token = None;
                    // the bar reverts to READY after the operation — success
                    // and cancellation flash their message for 3s first
                    // (Rufus PrintStatusTimeout parity)
                    self.ui.progress = None;
                    match res {
                        Ok(_) => {
                            self.ui.status = crate::locale::tr(l, "Write complete.").to_string();
                            self.ui.status_is_error = false;
                            self.ui.status_until = Some(Instant::now() + Duration::from_secs(3));
                            self.push_log("DONE");
                            self.save_settings(); // remember image/device/options
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            if msg.contains("cancelled") {
                                self.ui.status = crate::locale::tr(l, "Operation cancelled by the user").to_string();
                                self.ui.status_is_error = false;
                                self.ui.status_until = Some(Instant::now() + Duration::from_secs(3));
                                self.push_log("CANCELLED");
                            } else {
                                self.ui.status =
                                    crate::locale::trf(l, "Error: {0}", &[&msg]);
                                self.ui.status_is_error = true;
                                self.push_log(format!("ERROR: {msg}"));
                            }
                        }
                    }
                }
            }
        }
        // background hash computation finished?
        if let Some(rx) = &self.hash_rx {
            if let Ok(res) = rx.try_recv() {
                self.ui.hash_state = Some(match res {
                    Ok(h) => HashState::Ready(h),
                    Err(e) => HashState::Failed(format!("{e:#}")),
                });
                self.ui_dirty = true;
            }
        }
        // ⬇ DOWNLOAD dialog background jobs (language lists, links,
        // resolutions, download progress/completion)
        self.dl_pump();
        // Windows To Go edition list arrived → show Rufus's "Version
        // selection" dialog when the WIM carries more than one edition
        if self.ui.awaiting_wim_pick {
                    if let Some(rx) = &self.ui.wim_rx {
                if let Ok(res) = rx.try_recv() {
                    self.ui.awaiting_wim_pick = false;
                    self.ui.wim_rx = None;
                    self.ui.status.clear();
                    match res {
                        Some(list) if list.len() > 1 => {
                            self.push_log(crate::locale::trf(
                                l,
                                "install.wim has {0} editions — pick one for Windows To Go",
                                &[&list.len().to_string()],
                            ));
                            self.ui.wim_pick = list;
                            self.ui.wim_pick_sel = 0;
                            self.ui.wim_pick_open = true;
                        }
                        Some(_) | None => {
                            // single edition or unreadable metadata: use index 1
                            if let Some(list) = res {
                                self.push_log(format!(
                                    "install.wim edition: {}",
                                    list.first().map(|i| i.name.as_str()).unwrap_or("(unnamed)")
                                ));
                            } else {
                                self.push_log(crate::locale::tr(
                                    l,
                                    "could not read the WIM edition list — using image 1",
                                ));
                            }
                            let windows_iso = matches!(
                                (&self.ui.iso_cache, self.ui.boot_mode),
                                (Some((p, Some(s))), BootMode::DiskImage)
                                    if *p == self.ui.image_path && s.is_windows
                            );
                            self.open_wue_or_start(windows_iso);
                        }
                    }
                    self.ui_dirty = true;
                }
            }
        }
        // file dialog result
        if let Some(rx) = &self.ui.dlg_rx {
            if let Ok(v) = rx.try_recv() {
                if let Some(p) = v {
                    // keep a recent list for the SELECT split-button dropdown
                    self.ui.recent.retain(|x| x != &p);
                    self.ui.recent.insert(0, p.clone());
                    self.ui.recent.truncate(5);
                    self.ui.image_path = p;
                    self.mark_image_dirty();
                } else {
                    // no system file picker (zenity/kdialog/yad) — open the
                    // built-in browser instead of failing silently
                    if self.ui.browser.is_none() {
                        self.ui.browser = Some(FileBrowser::new());
                        self.push_log(crate::locale::tr(
                            l,
                            "no system file picker found — opening the built-in browser",
                        ));
                    }
                    self.ui_dirty = true;
                }
                self.ui.dlg_rx = None;
            }
        }
        // "Save log" dialog result (Rufus parity): the outcome is reported
        // into the log itself
        if let Some(rx) = &self.ui.log_save_rx {
            if let Ok(v) = rx.try_recv() {
                match v {
                    Ok(p) => self.push_log(crate::locale::trf(l, "log saved to {0}", &[&p])),
                    Err(e) => self.push_log(format!("ERROR: {e}")),
                }
                self.ui.log_save_rx = None;
                self.ui_dirty = true;
            }
        }
        // debounced ISO re-parse after the image path settles
        if let Some(t) = self.ui.iso_dirty_at {
            if t.elapsed() >= Duration::from_millis(600) {
                self.ui.iso_dirty_at = None;
                self.reparse_iso();
                self.ui_dirty = true;
            }
        }
        // Rufus status flashes time out and revert to READY
        if let Some(t) = self.ui.status_until {
            if Instant::now() >= t {
                self.ui.status.clear();
                self.ui.status_until = None;
                self.ui_dirty = true;
            }
        }
        // Rufus auto-detects newly plugged devices — poll sysfs when idle
        if !self.ui.running && self.ui.last_dev_refresh.elapsed() >= Duration::from_secs(2) {
            self.ui.last_dev_refresh = Instant::now();
            let before = self.ui.devices.len();
            self.refresh_devices();
            if self.ui.devices.len() != before {
                self.ui_dirty = true;
            }
        }
    }

    fn handle_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        if self.ui.focus_label && self.ui.label.chars().count() < 11 {
            self.ui.label.push(c);
        }
    }
    fn handle_backspace(&mut self) {
        if self.ui.focus_label && !self.ui.label.is_empty() {
            self.ui.label.pop();
        }
    }

    /// Paste clipboard text into the focused field (Ctrl+V).
    fn paste_clipboard(&mut self) {
        let Ok(text) = arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) else {
            return;
        };
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if self.ui.focus_label {
            for c in text.chars() {
                if self.ui.label.chars().count() < 11 {
                    self.ui.label.push(c);
                }
            }
        }
    }

    fn handle_click(&mut self) {
        let (mx, my) = self.mouse;

        // Rufus "⬇ DOWNLOAD" dialog is the topmost modal overlay
        if self.ui.dl.is_open() {
            return self.dl_click(mx, my);
        }

        // confirm dialog takes priority over everything beneath it
        if self.ui.confirm {
            for (r, w) in self.ui.widgets.iter().rev() {
                if r.contains(mx, my) {
                    match w {
                        Widget::ConfirmOk => {
                            self.ui.confirm = false;
                            self.do_start();
                        }
                        Widget::ConfirmCancel => self.ui.confirm = false,
                        _ => {}
                    }
                    break;
                }
            }
            return;
        }
        // about dialog
        if self.ui.about {
            for (r, w) in self.ui.widgets.iter().rev() {
                if r.contains(mx, my) {
                    if matches!(w, Widget::AboutClose) {
                        self.ui.about = false;
                    }
                    break;
                }
            }
            return;
        }

        // Rufus "Version selection" dialog (Windows To Go editions) is modal
        if self.ui.wim_pick_open {
            for (r, w) in self.ui.widgets.iter().rev() {
                if r.contains(mx, my) {
                    match w {
                        Widget::WimPickOk => {
                            let sel = self.ui.wim_pick_sel.min(self.ui.wim_pick.len().saturating_sub(1));
                            if let Some(i) = self.ui.wim_pick.get(sel) {
                                self.ui.wim_index = i.index;
                                self.push_log(crate::locale::trf(
                                    self.ui.locale,
                                    "Windows To Go edition: {0} (index {1})",
                                    &[&i.name, &i.index.to_string()],
                                ));
                            }
                            self.ui.wim_pick_open = false;
                            let windows_iso = matches!(
                                (&self.ui.iso_cache, self.ui.boot_mode),
                                (Some((p, Some(s))), BootMode::DiskImage)
                                    if *p == self.ui.image_path && s.is_windows
                            );
                            self.open_wue_or_start(windows_iso);
                        }
                        Widget::WimPickCancel => {
                            self.ui.wim_pick_open = false;
                            self.ui.wim_pick.clear();
                        }
                        Widget::WimPickRow(i) => self.ui.wim_pick_sel = *i,
                        _ => {}
                    }
                    break;
                }
            }
            return;
        }

        // Rufus "Windows User Experience" dialog is modal
        if self.ui.wue_open {
            for (r, w) in self.ui.widgets.iter().rev() {
                if r.contains(mx, my) {
                    match w {
                        Widget::WueOk => {
                            self.ui.wue_open = false;
                            self.start_write();
                        }
                        Widget::WueCancel => {
                            self.ui.wue = None;
                            self.ui.wue_open = false;
                        }
                        Widget::WueChk(i) => {
                            if let Some(o) = &mut self.ui.wue {
                                match *i {
                                    0 => o.bypass_tpm = !o.bypass_tpm,
                                    1 => o.no_online_account = !o.no_online_account,
                                    2 => o.disable_data_collection = !o.disable_data_collection,
                                    3 => o.local_account = !o.local_account,
                                    4 => o.regional_same_as_user = !o.regional_same_as_user,
                                    _ => o.offline_internal_drives = !o.offline_internal_drives,
                                }
                            }
                            self.ui_dirty = true;
                        }
                        _ => {}
                    }
                    break;
                }
            }
            return;
        }

        // image hashes dialog is modal
        if self.ui.hash_open {
            for (r, w) in self.ui.widgets.iter().rev() {
                if r.contains(mx, my) {
                    if matches!(w, Widget::HashOk) {
                        self.ui.hash_open = false;
                    }
                    break;
                }
            }
            return;
        }

        // ⚙ Settings dialog is modal; checkboxes apply immediately
        if self.ui.settings_open {
            for (r, w) in self.ui.widgets.iter().rev() {
                if r.contains(mx, my) {
                    match w {
                        Widget::SettingsChk(i) => {
                            match *i {
                                0 => self.ui.extra_hashes = !self.ui.extra_hashes,
                                1 => self.ui.use_rufus_mbr = !self.ui.use_rufus_mbr,
                                2 => {
                                    self.ui.large_fat32 = !self.ui.large_fat32;
                                    self.revalidate_fs_sel();
                                }
                                3 => self.ui.preserve_timestamps = !self.ui.preserve_timestamps,
                                _ => self.ui.persistent_log = !self.ui.persistent_log,
                            }
                            self.ui_dirty = true;
                        }
                        Widget::SettingsClose => self.ui.settings_open = false,
                        _ => {}
                    }
                    break;
                }
            }
            return;
        }

        // built-in file browser is modal
        if self.ui.browser.is_some() {
            for (r, w) in self.ui.widgets.iter().rev() {
                if r.contains(mx, my) {
                    match w {
                        Widget::BrowserCancel => self.ui.browser = None,
                        Widget::BrowserRow(i) => {
                            let i = *i;
                            let mut chosen = None;
                            if let Some(b) = &mut self.ui.browser {
                                if let Some(full) = b.pick(i) {
                                    chosen = Some(full);
                                } else {
                                    b.enter(i);
                                }
                            }
                            if let Some(full) = chosen {
                                self.ui.recent.retain(|x| x != &full);
                                self.ui.recent.insert(0, full.clone());
                                self.ui.recent.truncate(5);
                                self.ui.image_path = full;
                                self.mark_image_dirty();
                                self.ui.browser = None;
                            }
                            self.ui_dirty = true;
                        }
                        _ => {}
                    }
                    break;
                }
            }
            return;
        }

        // open dropdown: item pick / click-outside closes
        if let Some((_, r)) = self.ui.dd_rect {
            if r.contains(mx, my) {
                for (wr, w) in self.ui.widgets.iter().rev() {
                    if wr.contains(mx, my) {
                        if let Widget::DropdownItem(dd2, idx) = w {
                            self.select_dd(*dd2, *idx);
                        }
                        break;
                    }
                }
            }
            self.ui.open_dd = None;
            return;
        }

        // slider: click sets the value and starts a drag
        if let Some((r, _)) = self.ui.slider_rect {
            if r.contains(mx, my) && !self.ui.running {
                let gb = self.slider_value_at(mx);
                self.ui.persist_gb_str = gb.to_string();
                self.slider_drag = true;
                return;
            }
        }

        // clear field focus unless clicking a text field
        let mut clicked_field = false;
        for (r, w) in self.ui.widgets.iter().rev() {
            if r.contains(mx, my) {
                match w {
                    Widget::Combo(dd) => {
                        // toggle the dropdown open/closed; fixed-value combos
                        // (Image option, GB unit) never open, and the passes
                        // combo stays closed while the bad blocks box is off
                        if !matches!(dd, DD::ImageOption)
                            && !(*dd == DD::Passes && !self.ui.badblocks)
                        {
                            self.ui.dd_scroll = 0;
                            self.ui.open_dd = if self.ui.open_dd == Some(*dd) {
                                None
                            } else {
                                Some(*dd)
                            };
                        }
                    }
                    Widget::DeviceInfo => self.ui.advanced_open = !self.ui.advanced_open,
                    Widget::HashBtn => {
                        // Rufus: the check circle opens the image hashes dialog
                        if self.ui.hash_state.is_some() {
                            self.ui.hash_open = true;
                            self.ui_dirty = true;
                        }
                    }
                    Widget::BootSelect => {
                        if self.ui.dlg_rx.is_none() && self.ui.browser.is_none() {
                            self.spawn_file_dialog();
                        }
                    }
                    Widget::BootSelectArrow => {
                        self.ui.dd_scroll = 0;
                        self.ui.open_dd = if self.ui.open_dd == Some(DD::Recent) {
                            None
                        } else {
                            Some(DD::Recent)
                        };
                    }
                    Widget::SliderTrack => {}
                    Widget::AdvancedToggle => self.ui.advanced_open = !self.ui.advanced_open,
                    Widget::FmtAdvancedToggle => self.ui.fmt_advanced_open = !self.ui.fmt_advanced_open,
                    Widget::LabelField => {
                        self.ui.focus_label = true;
                        clicked_field = true;
                    }
                    Widget::ChkListHdd if !self.ui.running => {
                        self.ui.list_usb_hdd = !self.ui.list_usb_hdd;
                        self.refresh_devices();
                    }
                    Widget::ChkOldBios if !self.ui.running => {
                        self.ui.old_bios_fixes = !self.ui.old_bios_fixes;
                    }
                    Widget::ChkMbrId if !self.ui.running && self.mbr_row_enabled() => {
                        // Rufus: a manual toggle takes the box away from the
                        // SetMBRProps auto-management (mbr_selected_by_user)
                        self.ui.use_mbr_id = !self.ui.use_mbr_id;
                        self.ui.mbr_selected_by_user = self.ui.use_mbr_id;
                        self.status_timeout(
                            "Use Rufus MBR with BIOS ID",
                            self.ui.use_mbr_id,
                        );
                    }
                    Widget::ChkExtendedLabel
                        if !self.ui.running && self.extended_label_enabled() =>
                    {
                        self.ui.extended_label = !self.ui.extended_label;
                        self.status_timeout(
                            "Create extended label and icon files",
                            self.ui.extended_label,
                        );
                    }
                    Widget::ChkQuick if !self.ui.running => self.ui.quick = !self.ui.quick,
                    Widget::ChkBadblocks if !self.ui.running => self.ui.badblocks = !self.ui.badblocks,
                    Widget::ChkVerify if !self.ui.running => self.ui.verify = !self.ui.verify,
                    Widget::IconLog => self.ui.log_open = !self.ui.log_open,
                    Widget::IconInfo => self.ui.about = true,
                    Widget::IconSettings => {
                        // Rufus's ⚙ opens the settings dialog
                        self.ui.settings_open = true;
                        self.ui_dirty = true;
                    }
                    Widget::IconGlobe => {
                        // the globe opens the ⬇ DOWNLOAD dialog, too
                        self.open_download();
                    }
                    Widget::Start => {
                        // Rufus shows the "Windows User Experience" dialog when
                        // writing a Windows ISO
                        let windows_iso = matches!(
                            (&self.ui.iso_cache, self.ui.boot_mode),
                            (Some((p, Some(s))), BootMode::DiskImage)
                                if *p == self.ui.image_path && s.is_windows
                        );
                        let wtg = windows_iso
                            && self.ui.image_option == 1
                            && self
                                .ui
                                .iso_cache
                                .as_ref()
                                .and_then(|(_, s)| s.as_ref())
                                .map(|s| s.has_wininst)
                                .unwrap_or(false);
                        if wtg && self.ui.wim_pick.is_empty() {
                            // Rufus's "Version selection": fetch the editions
                            // of the WIM in the background, then show a picker
                            self.ui.awaiting_wim_pick = true;
                            let (tx, rx) = channel();
                            self.ui.wim_rx = Some(rx);
                            self.ui.status = crate::locale::tr(
                                self.ui.locale,
                                "Reading Windows image editions…",
                            )
                            .into();
                            self.ui.status_is_error = false;
                            let path = self.ui.image_path.clone();
                            std::thread::spawn(move || {
                                let _ = tx.send(engine::wim_image_list(std::path::Path::new(&path)).ok());
                            });
                            self.ui_dirty = true;
                        } else {
                            self.open_wue_or_start(windows_iso);
                        }
                    }
                    Widget::Stop => {
                        if let Some(t) = &self.ui.cancel_token {
                            t.store(true, Ordering::Relaxed);
                            self.ui.status = crate::locale::tr(
                                self.ui.locale,
                                "Cancelling - Please wait...",
                            )
                            .into();
                            self.push_log(crate::locale::tr(self.ui.locale, "STOP requested"));
                        }
                    }
                    Widget::Close => {
                        self.save_settings();
                        self.save_persistent_log();
                        std::process::exit(0);
                    }
                    Widget::ConfirmOk | Widget::ConfirmCancel | Widget::AboutClose => {}
                    // log panel buttons (Rufus IDD_LOG parity)
                    Widget::LogClear => {
                        // Rufus Clear: empty the log text, nothing else
                        self.ui.log.clear();
                        self.ui.log_scroll = None;
                    }
                    Widget::LogSave => self.spawn_save_log_dialog(),
                    // Rufus Close: hide the log, focus stays in the main window
                    Widget::LogClose => self.ui.log_open = false,
                    Widget::LogScroll | Widget::DropdownItem(..) => {}
                    _ => {}
                }
                break;
            }
        }
        // clicking anything but the label field clears its focus
        if !clicked_field {
            self.ui.focus_label = false;
        }
    }
}

// ---------- rendering ----------
struct Painter<'a> {
    buf: &'a mut [u32],
    w: usize,
    h: usize,
    /// owned fonts (cheap Arc clones) so dialog modules can borrow the app
    /// and the painter independently
    fonts: Fonts,
}

impl<'a> Painter<'a> {
    fn fill(&mut self, r: Rect, c: u32) {
        let x0 = r.x.max(0.0) as usize;
        let y0 = r.y.max(0.0) as usize;
        let x1 = ((r.x + r.w) as usize).min(self.w);
        let y1 = ((r.y + r.h) as usize).min(self.h);
        for y in y0..y1 {
            for x in x0..x1 {
                self.buf[y * self.w + x] = c;
            }
        }
    }

    /// Font entry for a layout font index (0 = primary of the weight,
    /// 1.. = fallback chain).
    fn entry_at(&self, fi: usize, bold: bool) -> &FontEntry {
        if fi == 0 {
            if bold { &self.fonts.bold } else { &self.fonts.regular }
        } else {
            &self.fonts.fallbacks[fi - 1]
        }
    }

    /// First font (primary then fallbacks) that actually has a glyph for `c`.
    fn pick_font(&self, c: char, bold: bool) -> usize {
        let primary = if bold { &self.fonts.bold.font } else { &self.fonts.regular.font };
        if primary.glyph_id(c) != ab_glyph::GlyphId(0) {
            return 0;
        }
        for (i, f) in self.fonts.fallbacks.iter().enumerate() {
            if f.font.glyph_id(c) != ab_glyph::GlyphId(0) {
                return i + 1;
            }
        }
        0 // notdef tofu in the primary font
    }

    /// Lay out `s` at pixel scale: per-character advance with font fallback
    /// for simple scripts, rustybuzz shaping + unicode-bidi run reordering
    /// for Arabic/Hebrew/Indic/Thai scripts. Used by both text() and text_w()
    /// so widths and drawn text always agree.
    fn layout(&self, s: &str, size: f32, bold: bool) -> (Vec<LayoutGlyph>, f32) {
        let mut out = Vec::new();
        let mut cx = 0.0f32;
        if !s.chars().any(needs_shaping) {
            // fast path: plain left-to-right advance with per-char fallback
            let mut prev_fi: Option<usize> = None;
            let mut prev_gid = ab_glyph::GlyphId(0);
            for c in s.chars() {
                let fi = self.pick_font(c, bold);
                let sf = self.entry_at(fi, bold).font.as_scaled(size);
                let gid = sf.font().glyph_id(c);
                if prev_fi == Some(fi) {
                    cx += sf.kern(prev_gid, gid);
                }
                out.push(LayoutGlyph { x: cx, y: 0.0, gid: gid.0, fi });
                cx += sf.h_advance(gid);
                prev_fi = Some(fi);
                prev_gid = gid;
            }
            return (out, cx);
        }
        // shaping path: split into visual bidi runs, shape each with
        // harfbuzz-style OpenType shaping (rustybuzz)
        let bidi = unicode_bidi::BidiInfo::new(s, None);
        for para in &bidi.paragraphs {
            let (levels, runs) = bidi.visual_runs(para, para.range.clone());
            for run in runs {
                let run_text = &s[run.clone()];
                let rtl = levels[run.start].is_rtl();
                // pick the run's font by best glyph coverage
                let chars: Vec<char> = run_text.chars().collect();
                let mut best_fi = 0usize;
                let mut best_cov = -1i32;
                for fi in 0..=self.fonts.fallbacks.len() {
                    let f = &self.entry_at(fi, bold).font;
                    let cov = chars.iter().filter(|&&c| f.glyph_id(c) != ab_glyph::GlyphId(0) || c == ' ').count() as i32;
                    if cov > best_cov {
                        best_cov = cov;
                        best_fi = fi;
                    }
                    if cov as usize == chars.len() {
                        break;
                    }
                }
                let entry = self.entry_at(best_fi, bold);
                let face = match rustybuzz::Face::from_slice(&entry.bytes[..], 0) {
                    Some(f) => f,
                    None => continue,
                };
                let upem = face.units_per_em().max(1) as f32;
                let k = size / upem;
                let mut buf = rustybuzz::UnicodeBuffer::new();
                buf.push_str(run_text);
                buf.set_direction(if rtl {
                    rustybuzz::Direction::RightToLeft
                } else {
                    rustybuzz::Direction::LeftToRight
                });
                let shaped = rustybuzz::shape(&face, &[], buf);
                for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                    out.push(LayoutGlyph {
                        x: cx + pos.x_offset as f32 * k,
                        y: pos.y_offset as f32 * k,
                        gid: info.glyph_id as u16,
                        fi: best_fi,
                    });
                    cx += pos.x_advance as f32 * k;
                }
            }
        }
        (out, cx)
    }

    fn text(&mut self, x: f32, y: f32, s: &str, size: f32, color: u32, bold: bool) {
        let (glyphs, _) = self.layout(s, size, bold);
        let baseline = y + self.fonts.regular.font.as_scaled(size).ascent();
        for g in glyphs {
            let sf = self.entry_at(g.fi, bold).font.as_scaled(size);
            let glyph = ab_glyph::GlyphId(g.gid)
                .with_scale_and_position(ab_glyph::PxScale::from(size), ab_glyph::point(x + g.x, baseline - g.y));
            if let Some(og) = sf.outline_glyph(glyph) {
                let pb = og.px_bounds();
                    og.draw(|gx, gy, cov| {
                        let ix = pb.min.x as i32 + gx as i32;
                        let iy = pb.min.y as i32 + gy as i32;
                        if ix >= 0 && iy >= 0 && (ix as usize) < self.w && (iy as usize) < self.h {
                            let idx = iy as usize * self.w + ix as usize;
                            let dst = &mut self.buf[idx];
                            let a = (cov.clamp(0.0, 1.0) * 255.0) as u32;
                            let (r0, g0, b0) = ((color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff);
                            let or_ = (*dst >> 16) & 0xff;
                            let og_ = (*dst >> 8) & 0xff;
                            let ob = *dst & 0xff;
                            *dst = (((or_ * (255 - a) + r0 * a) / 255) << 16)
                                | (((og_ * (255 - a) + g0 * a) / 255) << 8)
                                | ((ob * (255 - a) + b0 * a) / 255);
                        }
                    });
            }
        }
    }

    fn text_w(&self, s: &str, size: f32, bold: bool) -> f32 {
        self.layout(s, size, bold).1
    }

    /// Filled circle.
    fn disc(&mut self, cx: f32, cy: f32, r: f32, color: u32) {
        let r2 = r * r;
        for dy in (-(r as i32))..=(r as i32) {
            for dx in (-(r as i32))..=(r as i32) {
                let (dx, dy) = (dx as f32, dy as f32);
                if dx * dx + dy * dy <= r2 {
                    let x = cx + dx;
                    let y = cy + dy;
                    if x >= 0.0 && y >= 0.0 && (x as usize) < self.w && (y as usize) < self.h {
                        self.buf[y as usize * self.w + x as usize] = color;
                    }
                }
            }
        }
    }

    fn frame(&mut self, r: Rect, c: u32) {
        self.fill(Rect { x: r.x, y: r.y, w: r.w, h: 1.0 }, c);
        self.fill(Rect { x: r.x, y: r.y + r.h - 1.0, w: r.w, h: 1.0 }, c);
        self.fill(Rect { x: r.x, y: r.y, w: 1.0, h: r.h }, c);
        self.fill(Rect { x: r.x + r.w - 1.0, y: r.y, w: 1.0, h: r.h }, c);
    }

    /// Rufus section header: bold title + rule to the right margin.
    fn section(&mut self, m: f32, width: f32, y: f32, title: &str) {
        p_section(self, m, width, y, title);
    }
}

fn p_section(p: &mut Painter, m: f32, width: f32, y: f32, title: &str) {
    p.text(m, y, title, 19.0, TEXT, true);
    let tw = p.text_w(title, 19.0, true);
    let lx = m + tw + 14.0;
    let ly = y + 11.0;
    if lx < width - m {
        p.fill(Rect { x: lx, y: ly, w: width - m - lx, h: 2.0 }, TEXT);
    }
}

impl App {
    fn render(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let size = window.inner_size();
        // snapshot dropdown/slider data before the painter borrows self
        let dd_state = self
            .ui
            .open_dd
            .map(|dd| (dd, self.dd_items(dd), self.dd_selected(dd)));
        // filesystem combo entries (Large-FAT32 gating) — computed before the
        // painter takes its mutable borrow of self; the disk-id list likewise
        let fs_entries_state = self.fs_entries();
        let disk_ids = Self::disk_id_items(self.ui.locale);
        // Image-option combo text/state (also needs pre-borrow computation)
        let imgopt_state = {
            let items = self.dd_items(DD::ImageOption);
            let sel = self.dd_selected(DD::ImageOption).min(items.len().saturating_sub(1));
            (items.get(sel).cloned().unwrap_or_default(), sel < items.len().saturating_sub(1) || items.len() > 1)
        };
        // advanced-options enable states (Rufus EnableMBRBootOptions /
        // EnableExtendedLabel parity) — pre-borrow computation like the above
        let mbr_row_state = self.mbr_row_enabled();
        let ext_label_state = self.extended_label_enabled();
        let (_ctx, surface) = match (self.ctx.as_mut(), self.surface.as_mut()) {
            (Some(c), Some(s)) => (c, s),
            _ => return,
        };
        // softbuffer requires resize before first buffer access
        let _ = surface.resize(nz(size.width), nz(size.height));
        let mut buffer = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => {
                surface.resize(nz(size.width), nz(size.height)).ok();
                return;
            }
        };
        let bw = buffer.width().get() as usize;
        let pixels = buffer.as_mut();
        for p in pixels.iter_mut() { *p = BG; }

        let Some(fonts) = self.ui.fonts.as_ref() else { return };
        let mut p = Painter { buf: pixels, w: bw, h: size.height as usize, fonts: fonts.clone() };
        self.ui.widgets.clear();
        self.ui.dd_rect = None;
        self.ui.slider_rect = None;

        let m = 16.0;
        let width = size.width as f32;
        let full_w = width - 2.0 * m;
        let half = full_w / 2.0 - 8.0;
        let running = self.ui.running;
        let enabled = !running;
        let lang = self.ui.locale;
        let mut y = 12.0;
        let mut dd_rects = std::collections::HashMap::new();

        // =============== Drive Properties ===============
        p.section(m, width, y, crate::locale::tr(lang, "Drive Properties"));
        y += 32.0;

        p.text(m, y, crate::locale::tr(lang, "Device"), 13.0, TEXT, false);
        y += 17.0;
        let dev_text = if self.ui.devices.is_empty() {
            crate::locale::tr(lang, "(no devices found — plug one in)").to_string()
        } else {
            let d = &self.ui.devices[self.ui.sel_dev.min(self.ui.devices.len() - 1)];
            if d.model.is_empty() {
                format!("{} [{}]", d.path, d.size_human())
            } else {
                format!("{} [{}]", d.model, d.size_human())
            }
        };
        let info_w = 34.0;
        let dev_rect = Rect { x: m, y, w: full_w - info_w - 8.0, h: COMBO_H };
        Self::draw_combo(&mut p, dev_rect, &dev_text, enabled && !self.ui.devices.is_empty(), false, self.mouse);
        self.ui.widgets.push((dev_rect, Widget::Combo(DD::Device)));
        dd_rects.insert(DD::Device, dev_rect);
        // small disk-icon button (toggles drive-properties section, like Rufus)
        let icon_rect = Rect { x: m + full_w - info_w, y: y - 2.0, w: info_w, h: COMBO_H + 4.0 };
        Self::draw_icon_button(&mut p, icon_rect, IconKind::Disk, true, self.mouse);
        self.ui.widgets.push((icon_rect, Widget::DeviceInfo));
        y += COMBO_H + 8.0;

        p.text(m, y, crate::locale::tr(lang, "Boot selection"), 13.0, TEXT, false);
        y += 17.0;
        let select_w = 104.0; // SELECT main segment + recent-files arrow
        let boot_parsed = matches!(&self.ui.iso_cache, Some((p_, Some(_))) if *p_ == self.ui.image_path);
        let hash_clickable = boot_parsed && self.ui.hash_state.is_some();
        let boot_text = match self.ui.boot_mode {
            BootMode::NonBootable => crate::locale::tr(lang, "Non bootable").to_string(),
            BootMode::FreeDos => "FreeDOS".to_string(),
            BootMode::DiskImage => {
                if self.ui.image_path.is_empty() {
                    // Rufus composes the "(Please select)" suffix (msgid
                    // "%s (Please select)")
                    crate::locale::trf(lang, "%s (Please select)", &[crate::locale::tr(lang, "Disk or ISO image")])
                } else {
                    self.ui.image_path.rsplit('/').next().unwrap_or(&self.ui.image_path).to_string()
                }
            }
        };
        // Rufus greys START until a bootable image is selected; the
        // non-bootable and FreeDOS modes need no image
        let start_enabled = enabled
            && !self.ui.devices.is_empty()
            && (self.ui.boot_mode != BootMode::DiskImage || !self.ui.image_path.is_empty());
        let boot_rect = Rect {
            x: m,
            y,
            w: full_w - select_w - 34.0,
            h: COMBO_H,
        };
        Self::draw_combo(&mut p, boot_rect, &boot_text, enabled, false, self.mouse);
        self.ui.widgets.push((boot_rect, Widget::Combo(DD::Boot)));
        dd_rects.insert(DD::Boot, boot_rect);
        // blue check circle — in Rufus clicking it opens the image hashes dialog
        {
            let cx = m + boot_rect.w + 14.0;
            let cy = y + COMBO_H / 2.0;
            let on = boot_parsed && hash_clickable;
            p.disc(cx, cy, 9.0, if on { ACCENT } else { TEXT_DIS });
            p.fill(Rect { x: cx - 5.0, y: cy - 1.0, w: 3.0, h: 5.0 }, FIELD);
            p.fill(Rect { x: cx - 3.0, y: cy + 1.0, w: 3.0, h: 5.0 }, FIELD);
            p.fill(Rect { x: cx - 1.0, y: cy - 3.0, w: 3.0, h: 5.0 }, FIELD);
            self.ui.widgets.push((Rect { x: cx - 12.0, y: cy - 12.0, w: 24.0, h: 24.0 }, Widget::HashBtn));
        }
        // split SELECT button (main + recent-files arrow); the "⬇ DOWNLOAD"
        // action lives at the top of the arrow's dropdown — one button, one
        // menu, instead of two side-by-side buttons
        let sel_main = Rect { x: m + full_w - select_w, y, w: 78.0, h: COMBO_H };
        let sel_arrow = Rect { x: m + full_w - select_w + 78.0, y, w: 26.0, h: COMBO_H };
        Self::draw_button(&mut p, sel_main, crate::locale::tr(lang, "SELECT"), enabled, self.mouse, false);
        Self::draw_combo(&mut p, sel_arrow, "", enabled, false, self.mouse);
        self.ui.widgets.push((sel_main, Widget::BootSelect));
        self.ui.widgets.push((sel_arrow, Widget::BootSelectArrow));
        // anchor the recent-files dropdown to the arrow, but wide enough for paths
        dd_rects.insert(DD::Recent, Rect { x: m + full_w - 300.0, y: sel_arrow.y, w: 300.0, h: COMBO_H });
        y += COMBO_H + 6.0;

        // --- Image option (Windows ISOs only, like Rufus) ---
        let windows_iso = self
            .ui
            .iso_cache
            .as_ref()
            .and_then(|(_, s)| s.as_ref())
            .map(|s| s.is_windows)
            .unwrap_or(false);
        let has_wininst = self
            .ui
            .iso_cache
            .as_ref()
            .and_then(|(_, s)| s.as_ref())
            .map(|s| s.has_wininst)
            .unwrap_or(false);
        let wtg = windows_iso && has_wininst && self.ui.image_option == 1;
        if windows_iso {
            p.text(m, y, crate::locale::tr(lang, "Image Option"), 13.0, TEXT, false);
            y += 17.0;
            let opt_rect = Rect { x: m, y, w: full_w, h: COMBO_H };
            let (opt_text, opt_has_wtg) = imgopt_state;
            // interactive once the ISO carries an install.wim/esd
            Self::draw_combo(&mut p, opt_rect, &opt_text, enabled && opt_has_wtg, false, self.mouse);
            self.ui.widgets.push((opt_rect, Widget::Combo(DD::ImageOption)));
            dd_rects.insert(DD::ImageOption, opt_rect);
            y += COMBO_H + 8.0;
        }

        // --- Persistent partition size (Linux ISOs only, like Rufus) ---
        let show_persist = self.ui.boot_mode == BootMode::DiskImage
            && !self.ui.image_path.is_empty()
            && !windows_iso
            && !wtg; // Rufus offers no persistence alongside Windows To Go
        if show_persist {
            p.text(m, y, crate::locale::tr(lang, "Persistent partition size"), 13.0, TEXT, false);
            y += 17.0;
            let slider_w = full_w - 160.0;
            let dev = self.ui.devices.get(self.ui.sel_dev);
            let iso_size = self
                .ui
                .iso_cache
                .as_ref()
                .and_then(|(_, s)| s.as_ref())
                .map(|s| s.total_bytes)
                .unwrap_or(0);
            let max_gb = match dev {
                Some(d) => {
                    let avail = d.size.saturating_sub(iso_size + GB);
                    (avail / GB).clamp(0, 32)
                }
                None => 32,
            };
            let cur_gb = self.ui.persist_gb_str.trim().parse::<u64>().unwrap_or(0).min(max_gb);
            self.ui.persist_gb_str = cur_gb.to_string();
            let slider = Rect { x: m + 2.0, y: y + 2.0, w: slider_w - 4.0, h: 20.0 };
            self.ui.slider_rect = Some((slider, max_gb));
            // track + fill + handle
            let track_y = y + 11.0;
            p.fill(Rect { x: slider.x, y: track_y, w: slider.w, h: 4.0 }, 0xB4B4B4);
            if max_gb > 0 {
                let frac = cur_gb as f32 / max_gb as f32;
                p.fill(Rect { x: slider.x, y: track_y, w: (slider.w * frac).max(1.0), h: 4.0 }, ACCENT);
                let hx = slider.x + slider.w * frac - 5.0;
                let handle = Rect { x: hx, y: y + 2.0, w: 10.0, h: 20.0 };
                p.fill(handle, ACCENT);
                p.frame(handle, 0x16608F);
            }
            if max_gb == 0 {
                // no room for persistence — grey the track out
                p.fill(Rect { x: slider.x, y: track_y, w: slider.w, h: 4.0 }, BORDER_DIS);
            }
            self.ui.widgets.push((slider, Widget::SliderTrack));
            // value field + GB unit combo
            let val_rect = Rect { x: m + full_w - 150.0, y, w: 62.0, h: COMBO_H };
            Self::draw_field(&mut p, val_rect, &self.ui.persist_gb_str, false, enabled);
            let gb_rect = Rect { x: m + full_w - 80.0, y, w: 80.0, h: COMBO_H };
            Self::draw_combo(&mut p, gb_rect, "GB", false, false, self.mouse);
            y += 32.0;
        }

        p.text(m, y, crate::locale::tr(lang, "Partition scheme"), 13.0, TEXT, false);
        p.text(m + full_w / 2.0 + 8.0, y, crate::locale::tr(lang, "Target system"), 13.0, TEXT, false);
        y += 17.0;
        let scheme_rect = Rect { x: m, y, w: half, h: COMBO_H };
        let target_rect = Rect { x: m + full_w / 2.0 + 8.0, y, w: half, h: COMBO_H };
        Self::draw_combo(&mut p, scheme_rect, crate::locale::tr(lang, SCHEME_NAMES[if self.ui.gpt { 1 } else { 0 }]), enabled, false, self.mouse);
        Self::draw_combo(&mut p, target_rect, crate::locale::tr(lang, TARGET_NAMES[if self.ui.efi_only { 1 } else { 0 }]), enabled, false, self.mouse);
        self.ui.widgets.push((scheme_rect, Widget::Combo(DD::Scheme)));
        self.ui.widgets.push((target_rect, Widget::Combo(DD::Target)));
        dd_rects.insert(DD::Scheme, scheme_rect);
        dd_rects.insert(DD::Target, target_rect);
        y += COMBO_H + 8.0;

        // --- advanced drive properties (Rufus "Show %s"/"Hide %s" msgids) ---
        let adv_text = if self.ui.advanced_open {
            format!("\u{25b4} {}", crate::locale::trf(lang, "Hide %s", &[crate::locale::tr(lang, "advanced drive properties")]))
        } else {
            format!("\u{25b8} {}", crate::locale::trf(lang, "Show %s", &[crate::locale::tr(lang, "advanced drive properties")]))
        };
        p.text(m, y, &adv_text, 13.0, TEXT, false);
        let adv_rect = Rect { x: m, y: y - 2.0, w: p.text_w(&adv_text, 13.0, false) + 12.0, h: 22.0 };
        self.ui.widgets.push((adv_rect, Widget::AdvancedToggle));
        y += 24.0;

        if self.ui.advanced_open {
            let list_hdd = crate::locale::tr(lang, "List USB Hard Drives");
            Self::draw_checkbox(&mut p, m, y, list_hdd, self.ui.list_usb_hdd, enabled);
            self.ui.widgets.push((Rect { x: m, y: y - 2.0, w: p.text_w(list_hdd, 13.0, false) + 30.0, h: 20.0 }, Widget::ChkListHdd));
            y += 21.0;
            let old_bios = crate::locale::tr(lang, "Add fixes for old BIOSes (extra partition, align, etc.)");
            Self::draw_checkbox(&mut p, m, y, old_bios, self.ui.old_bios_fixes, enabled);
            self.ui.widgets.push((Rect { x: m, y: y - 2.0, w: p.text_w(old_bios, 13.0, false) + 30.0, h: 20.0 }, Widget::ChkOldBios));
            y += 21.0;
            // "Use Rufus MBR with BIOS ID" + the IDC_DISK_ID masquerade combo
            // (rufus.c: enabled for MBR+BIOS bootable writes, Windows images)
            let mbr_enabled = enabled && mbr_row_state;
            let mbr_txt = crate::locale::tr(lang, "Use Rufus MBR with BIOS ID");
            Self::draw_checkbox(&mut p, m, y, mbr_txt, self.ui.use_mbr_id, mbr_enabled);
            self.ui.widgets.push((Rect { x: m, y: y - 2.0, w: p.text_w(mbr_txt, 13.0, false) + 30.0, h: 20.0 }, Widget::ChkMbrId));
            let id_rect = Rect { x: m + full_w / 2.0 + 8.0, y: y - 1.0, w: 220.0, h: COMBO_H };
            let id_sel = self.ui.disk_id_sel.min(disk_ids.len() - 1);
            Self::draw_combo(&mut p, id_rect, &disk_ids[id_sel],
                mbr_enabled, self.ui.open_dd == Some(DD::DiskId), self.mouse);
            self.ui.widgets.push((id_rect, Widget::Combo(DD::DiskId)));
            dd_rects.insert(DD::DiskId, id_rect);
            y += 26.0;
        }

        // =============== Format Options ===============
        p.section(m, width, y, crate::locale::tr(lang, "Format Options"));
        y += 32.0;

        p.text(m, y, crate::locale::tr(lang, "Volume label"), 13.0, TEXT, false);
        y += 17.0;
        let lbl_rect = Rect { x: m, y, w: full_w, h: COMBO_H };
        Self::draw_field(&mut p, lbl_rect, &self.ui.label, self.ui.focus_label, enabled);
        self.ui.widgets.push((lbl_rect, Widget::LabelField));
        y += COMBO_H + 8.0;

        p.text(m, y, crate::locale::tr(lang, "File system"), 13.0, TEXT, false);
        p.text(m + full_w / 2.0 + 8.0, y, crate::locale::tr(lang, "Cluster size"), 13.0, TEXT, false);
        y += 17.0;
        let fs_rect = Rect { x: m, y, w: half, h: COMBO_H };
        let clu_rect = Rect { x: m + full_w / 2.0 + 8.0, y, w: half, h: COMBO_H };
        // DOS requires FAT32; Windows To Go is NTFS-only (Rufus MSG_097)
        let fs_locked = self.ui.boot_mode == BootMode::FreeDos || wtg;
        let fs_text = if self.ui.boot_mode == BootMode::FreeDos {
            Self::fs_display(lang, 1)
        } else if wtg {
            Self::fs_display(lang, 0) // NTFS
        } else {
            fs_entries_state
                .iter()
                .find(|(i, _)| *i == self.ui.fs_sel.min(2))
                .map(|(_, s)| s.clone())
                .unwrap_or_else(|| Self::fs_display(lang, self.ui.fs_sel.min(2)))
        };
        Self::draw_combo(&mut p, fs_rect, &fs_text, enabled && !fs_locked, false, self.mouse);
        // honest cluster display: what the native formatter would pick for
        // the selected device (falls back to the Rufus default), with the
        // "%s (Default)" + "bytes" msgids for the "(Default)" suffix
        let cluster_text = match self.ui.devices.get(self.ui.sel_dev) {
            Some(d) => match crate::fat32::compute_params(d.size.saturating_sub(2048 * 512), 512) {
                Ok(prm) => Self::cluster_display(lang, (prm.sectors_per_cluster as usize) * 512),
                Err(_) => Self::cluster_display(lang, 4096),
            },
            None => Self::cluster_display(lang, 4096),
        };
        Self::draw_combo(&mut p, clu_rect, &cluster_text.clone(), false, false, self.mouse);
        self.ui.widgets.push((fs_rect, Widget::Combo(DD::Fs)));
        dd_rects.insert(DD::Fs, fs_rect);
        y += COMBO_H + 8.0;

        let fmt_text = if self.ui.fmt_advanced_open {
            format!("\u{25be} {}", crate::locale::trf(lang, "Hide %s", &[crate::locale::tr(lang, "advanced format options")]))
        } else {
            format!("\u{25b8} {}", crate::locale::trf(lang, "Show %s", &[crate::locale::tr(lang, "advanced format options")]))
        };
        p.text(m, y, &fmt_text, 13.0, TEXT, false);
        let fmt_rect = Rect { x: m, y: y - 2.0, w: p.text_w(&fmt_text, 13.0, false) + 12.0, h: 22.0 };
        self.ui.widgets.push((fmt_rect, Widget::FmtAdvancedToggle));
        y += 24.0;

        if self.ui.fmt_advanced_open {
            let quick = crate::locale::tr(lang, "Quick format");
            Self::draw_checkbox(&mut p, m, y, quick, self.ui.quick, enabled);
            self.ui.widgets.push((Rect { x: m, y: y - 2.0, w: p.text_w(quick, 13.0, false) + 30.0, h: 20.0 }, Widget::ChkQuick));
            y += 21.0;
            // Rufus IDC_EXTENDED_LABEL (checked by default): autorun.inf +
            // autorun.ico on the volume; disabled for raw/DD writes
            let xl_enabled = enabled && ext_label_state;
            let xl_txt = crate::locale::tr(lang, "Create extended label and icon files");
            Self::draw_checkbox(&mut p, m, y, xl_txt, self.ui.extended_label, xl_enabled);
            self.ui.widgets.push((Rect { x: m, y: y - 2.0, w: p.text_w(xl_txt, 13.0, false) + 30.0, h: 20.0 }, Widget::ChkExtendedLabel));
            y += 21.0;
            // Rufus: "Check device for bad blocks" + the "Number of passes"
            // combobox for the destructive write test (combo only enabled
            // while the checkbox is checked)
            let bb_text = crate::locale::tr(lang, "Check device for bad blocks");
            Self::draw_checkbox(&mut p, m, y, bb_text, self.ui.badblocks, enabled);
            self.ui.widgets.push((Rect { x: m, y: y - 2.0, w: p.text_w(bb_text, 13.0, false) + 30.0, h: 20.0 }, Widget::ChkBadblocks));
            let passes_rect = Rect { x: m + full_w - 190.0, y: y - 3.0, w: 190.0, h: COMBO_H };
            Self::draw_combo(&mut p, passes_rect, crate::locale::tr(lang, engine::BB_PASSES_NAMES[self.ui.bb_sel.min(4)]),
                enabled && self.ui.badblocks, self.ui.open_dd == Some(DD::Passes), self.mouse);
            self.ui.widgets.push((passes_rect, Widget::Combo(DD::Passes)));
            dd_rects.insert(DD::Passes, passes_rect);
            y += 26.0;
            let verify_txt = crate::locale::tr(lang, "Verify after write (SHA-256)");
            Self::draw_checkbox(&mut p, m, y, verify_txt, self.ui.verify, enabled);
            self.ui.widgets.push((Rect { x: m, y: y - 2.0, w: p.text_w(verify_txt, 13.0, false) + 30.0, h: 20.0 }, Widget::ChkVerify));
            y += 21.0;
            y += 6.0;
        }

        // =============== Status ===============
        p.section(m, width, y, crate::locale::tr(lang, "Status"));
        y += 34.0;

        let bar = Rect { x: m, y, w: full_w, h: 26.0 };
        let (fill_color, fill_frac, bar_text, text_color) = if self.ui.status_is_error {
            (READY_GREY, 0.0, crate::locale::trf(lang, "FAILED — {0}", &[&self.ui.status]), PROGRESS_ERR)
        } else if running {
            let (phase, frac) = self.ui.progress.clone().unwrap_or(("start".into(), 0.0));
            let pct = ((frac * 100.0) as u32).min(100);
            (PROGRESS_GREEN, frac, format!("{} ({}%)", pretty_phase(lang, &phase), pct), TEXT)
        } else if !self.ui.status.is_empty() {
            // Rufus PrintStatusTimeout: a timed flash (e.g. "Use Rufus MBR:
            // enabled") shown in the status bar until it expires
            (READY_GREY, 0.0, self.ui.status.clone(), TEXT)
        } else {
            (READY_GREY, 0.0, crate::locale::tr(lang, "READY").to_string(), TEXT)
        };
        p.fill(bar, READY_GREY);
        if fill_frac > 0.0 {
            let frac = fill_frac.clamp(0.0, 1.0) as f32;
            p.fill(Rect { x: bar.x, y: bar.y, w: (bar.w * frac).max(1.0), h: bar.h }, fill_color);
        }
        let btw = p.text_w(&bar_text, 13.0, false);
        p.text(bar.x + (bar.w - btw) / 2.0, bar.y + 6.0, &bar_text, 13.0, text_color, false);
        y += bar.h + 8.0;

        // --- inline log (toggled by the bottom log icon) ---
        // Rufus IDD_LOG parity: a scrolling log text with Clear | Save | Close
        if self.ui.log_open {
            let log_h = 150.0;
            let log_rect = Rect { x: m, y, w: full_w, h: log_h };
            p.fill(log_rect, FIELD);
            p.frame(log_rect, BORDER);
            self.ui.widgets.push((log_rect, Widget::LogScroll));
            let line_h = 14.0;
            let max_lines = ((log_rect.h - 6.0) / line_h) as usize;
            let len = self.ui.log.len();
            // log_scroll = absolute index of the first visible line; None
            // follows the tail like Rufus's auto-scrolling edit. A pinned
            // window that has run into the tail resumes auto-follow.
            if let Some(i) = self.ui.log_scroll {
                if i + max_lines >= len {
                    self.ui.log_scroll = None;
                }
            }
            let start = match self.ui.log_scroll {
                Some(i) => i.min(len.saturating_sub(1)),
                None => len.saturating_sub(max_lines),
            };
            let mut ly = log_rect.y + 4.0;
            for line in &self.ui.log[start..] {
                let disp: String = line.chars().take(96).collect();
                p.text(log_rect.x + 6.0, ly, &disp, 11.0, log_line_color(line), false);
                ly += line_h;
            }
            // Clear | Save | Close row, right-aligned under the text (Rufus)
            let row_h = 26.0;
            let btn_w = 76.0;
            let row_y = log_rect.y + log_rect.h + 4.0;
            let mut bx = log_rect.x + log_rect.w - (3.0 * btn_w + 2.0 * 8.0);
            for (label, w) in [
                (crate::locale::tr(lang, "Clear"), Widget::LogClear),
                (crate::locale::tr(lang, "Save"), Widget::LogSave),
                (crate::locale::tr(lang, "Close"), Widget::LogClose),
            ] {
                let r = Rect { x: bx, y: row_y, w: btn_w, h: row_h };
                Self::draw_button(&mut p, r, label, true, self.mouse, false);
                self.ui.widgets.push((r, w));
                bx += btn_w + 8.0;
            }
            y = row_y + row_h + 8.0;
        }

        // --- bottom row: language combo (Rufus IDC_LANG, bottom-left) +
        // icon buttons + STOP/START + CLOSE ---
        let btn_y = y + 4.0;
        let btn_h = 32.0;
        let lang_rect = Rect { x: m, y: btn_y, w: 132.0, h: btn_h };
        let lang_name = crate::locale::LANGS.get(lang).map(|l| l.name).unwrap_or("English");
        Self::draw_combo(&mut p, lang_rect, lang_name, true, self.ui.open_dd == Some(DD::Lang), self.mouse);
        self.ui.widgets.push((lang_rect, Widget::Combo(DD::Lang)));
        dd_rects.insert(DD::Lang, lang_rect);
        // the small icons sit right of the language combo; they overlap the
        // STOP button that appears while running, so they yield (Rufus
        // disables the advanced icons during an operation anyway)
        if !running {
            for (i, (kind, w)) in [
                (IconKind::Globe, Widget::IconGlobe),
                (IconKind::Info, Widget::IconInfo),
                (IconKind::Settings, Widget::IconSettings),
                (IconKind::Log, Widget::IconLog),
            ]
            .into_iter()
            .enumerate()
            {
                let r = Rect { x: m + 132.0 + 8.0 + i as f32 * 32.0, y: btn_y + 2.0, w: 28.0, h: 28.0 };
                Self::draw_icon_button(&mut p, r, kind, true, self.mouse);
                self.ui.widgets.push((r, w));
            }
        }
        let stop_w = if running { 90.0 } else { 0.0 };
        if running {
            let stop_rect = Rect { x: width - m - 92.0 - 150.0 - 12.0 - stop_w, y: btn_y, w: stop_w, h: btn_h };
            Self::draw_button(&mut p, stop_rect, crate::locale::tr(lang, "STOP"), true, self.mouse, false);
            self.ui.widgets.push((stop_rect, Widget::Stop));
        }
        let start_rect = Rect { x: width - m - 92.0 - 150.0 - 12.0, y: btn_y, w: 150.0, h: btn_h };
        Self::draw_button(&mut p, start_rect, if running { "WORKING..." } else { crate::locale::tr(lang, "START") }, start_enabled, self.mouse, true);
        self.ui.widgets.push((start_rect, Widget::Start));
        let close_rect = Rect { x: width - m - 92.0, y: btn_y, w: 92.0, h: btn_h };
        Self::draw_button(&mut p, close_rect, crate::locale::tr(lang, "CLOSE"), true, self.mouse, false);
        self.ui.widgets.push((close_rect, Widget::Close));
        y = btn_y + btn_h + 6.0;

        // --- bottom status strip: device count + session timer ---
        let strip = Rect { x: 0.0, y, w: width, h: 22.0 };
        p.fill(strip, 0xE6E6E6);
        p.fill(Rect { x: 0.0, y, w: width, h: 1.0 }, BORDER);
        let n = self.ui.devices.len();
        let devices_txt = if n == 1 {
            crate::locale::trf(lang, "{0} device found", &[&n.to_string()])
        } else {
            crate::locale::trf(lang, "{0} devices found", &[&n.to_string()])
        };
        p.text(10.0, y + 4.0, &devices_txt, 12.0, 0x505050, false);
        let t = self.ui.start_time.elapsed().as_secs();
        let timer = format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60);
        let tw = p.text_w(&timer, 12.0, false);
        p.text(width - tw - 10.0, y + 4.0, &timer, 12.0, 0x505050, false);
        y += 26.0;

        // grow/shrink the window to fit the computed content exactly
        let desired = (y + 4.0) as u32;
        if desired.abs_diff(self.ui.desired_h) > 2 {
            self.ui.desired_h = desired;
            let _ = window.request_inner_size(LogicalSize::new(W as f64, desired as f64));
        }

        // --- Rufus-style destroy confirmation dialog (Rufus MSG_003 msgid,
        // split on the \n into display lines) ---
        if self.ui.confirm {
            let dev = self.ui.devices.get(self.ui.sel_dev);
            let dev_desc = dev
                .map(|d| format!("{} [{}]", if d.model.is_empty() { &d.path } else { &d.model }, d.size_human()))
                .unwrap_or_default();
            // Rufus MSG_098: WTG on REMOVABLE media is not designed for and
            // usually freezes at boot — Rufus asks before proceeding; we fold
            // the note into the destroy confirmation
            let wtg_removable =
                Self::pending_is_wtg(&self.ui) && dev.map(|d| d.removable).unwrap_or(false);
            let pw = 460.0;
            let ph = if wtg_removable { 170.0 } else { 150.0 };
            let panel = Rect { x: (width - pw) / 2.0, y: (size.height as f32 - ph) / 2.0, w: pw, h: ph };
            p.fill(panel, FIELD);
            p.frame(panel, ACCENT);
            p.text(panel.x + 12.0, panel.y + 10.0, "Rufus-linux", 13.0, TEXT, true);
            let mut warning = crate::locale::trf(
                lang,
                "WARNING: ALL DATA ON DEVICE '{0}' WILL BE DESTROYED.\nTo continue with this operation, click OK. To quit click CANCEL.",
                &[&dev_desc],
            );
            if wtg_removable {
                warning.push('\n');
                warning.push_str(crate::locale::tr(
                    lang,
                    "IMPORTANT: 'Windows To Go' on a REMOVABLE drive is not supported by Microsoft and will most likely freeze during boot.",
                ));
            }
            for (i, line) in warning.split('\n').enumerate() {
                p.text(panel.x + 12.0, panel.y + 40.0 + i as f32 * 20.0, line, 12.0, TEXT, false);
            }
            let ok = Rect { x: panel.x + panel.w - 210.0, y: panel.y + ph - 44.0, w: 90.0, h: 30.0 };
            let cancel = Rect { x: panel.x + panel.w - 110.0, y: panel.y + ph - 44.0, w: 90.0, h: 30.0 };
            Self::draw_button(&mut p, ok, crate::locale::tr(lang, "OK"), true, self.mouse, true);
            Self::draw_button(&mut p, cancel, crate::locale::tr(lang, "Cancel"), true, self.mouse, false);
            self.ui.widgets.push((ok, Widget::ConfirmOk));
            self.ui.widgets.push((cancel, Widget::ConfirmCancel));
        }

        // --- about panel (info icon) ---
        if self.ui.about {
            let pw = 380.0;
            let ph = 140.0;
            let panel = Rect { x: (width - pw) / 2.0, y: (size.height as f32 - ph) / 2.0, w: pw, h: ph };
            p.fill(panel, FIELD);
            p.frame(panel, ACCENT);
            p.text(panel.x + 14.0, panel.y + 12.0, "Rufus-linux", 16.0, TEXT, true);
            p.text(panel.x + 14.0, panel.y + 42.0, crate::locale::tr(lang, "A Linux rewrite of Rufus"), 13.0, TEXT, false);
            p.text(panel.x + 14.0, panel.y + 62.0, crate::locale::tr(lang, "Create bootable USB drives from ISO images"), 12.0, 0x505050, false);
            p.text(panel.x + 14.0, panel.y + 82.0, "Licensed GPL-3.0 — https://github.com/pbatard/rufus", 12.0, 0x505050, false);
            let close = Rect { x: panel.x + panel.w - 100.0, y: panel.y + ph - 40.0, w: 86.0, h: 28.0 };
            Self::draw_button(&mut p, close, crate::locale::tr(lang, "Close"), true, self.mouse, false);
            self.ui.widgets.push((close, Widget::AboutClose));
        }

        // --- ⚙ Settings dialog: Rufus's hidden settings, ported to Linux ---
        if self.ui.settings_open {
            let pw = 520.0;
            let ph = 336.0;
            let panel = Rect { x: (width - pw) / 2.0, y: (size.height as f32 - ph) / 2.0, w: pw, h: ph };
            p.fill(panel, FIELD);
            p.frame(panel, ACCENT);
            // header band (same style as the Windows User Experience dialog)
            let header = Rect { x: panel.x, y: panel.y, w: panel.w, h: 34.0 };
            p.fill(header, 0xDCEBF5);
            p.fill(Rect { x: panel.x, y: panel.y + 34.0, w: panel.w, h: 1.0 }, BORDER);
            p.text(panel.x + 14.0, panel.y + 9.0, crate::locale::tr(lang, "Settings"), 14.0, TEXT, true);
            let mut cy = panel.y + 50.0;
            p.text(panel.x + 20.0, cy, crate::locale::tr(lang, "Image checksums"), 12.0, ACCENT, true);
            cy += 22.0;
            // "Extra hashes (SHA512)" — Rufus MSG_312 (EnableExtraHashes)
            let extra_hashes = crate::locale::tr(lang, "Extra hashes (SHA512)");
            Self::draw_checkbox(&mut p, panel.x + 30.0, cy, extra_hashes, self.ui.extra_hashes, true);
            self.ui.widgets.push((
                Rect { x: panel.x + 30.0, y: cy - 2.0, w: panel.w - 60.0, h: 20.0 },
                Widget::SettingsChk(0),
            ));
            p.text(panel.x + 46.0, cy + 17.0, crate::locale::tr(lang, "Show SHA-512 in the image hashes dialog — toggled with <Alt>-<H>"), 10.0, 0x808080, false);
            cy += 38.0;
            p.text(panel.x + 20.0, cy, crate::locale::tr(lang, "Hidden settings"), 12.0, ACCENT, true);
            cy += 22.0;
            let items: [(&str, &str, bool); 4] = [
                (
                    "Use Rufus MBR",
                    "write the Rufus MBR boot code (<Alt>-<A>); off = plain partition table",
                    self.ui.use_rufus_mbr,
                ),
                (
                    "Force large FAT32 formatting",
                    "keep FAT32 selectable on drives larger than 32 GiB (<Alt>-<L>)",
                    self.ui.large_fat32,
                ),
                (
                    "Preserve timestamps",
                    "copy the ISO file dates to the written files (<Alt>-<T>)",
                    self.ui.preserve_timestamps,
                ),
                (
                    "Persistent log",
                    "save the session log to rufus-linux/rufus.log on exit (<Ctrl>-<P>)",
                    self.ui.persistent_log,
                ),
            ];
            for (i, (label, hint, on)) in items.into_iter().enumerate() {
                Self::draw_checkbox(&mut p, panel.x + 30.0, cy, crate::locale::tr(lang, label), on, true);
                self.ui.widgets.push((
                    Rect { x: panel.x + 30.0, y: cy - 2.0, w: panel.w - 60.0, h: 20.0 },
                    Widget::SettingsChk(i + 1),
                ));
                p.text(panel.x + 46.0, cy + 17.0, crate::locale::tr(lang, hint), 10.0, 0x808080, false);
                cy += 36.0;
            }
            p.text(
                panel.x + 20.0,
                panel.y + ph - 34.0,
                crate::locale::tr(lang, "Changes apply immediately and persist between runs."),
                10.0,
                0x808080,
                false,
            );
            let close = Rect { x: panel.x + panel.w - 100.0, y: panel.y + ph - 44.0, w: 86.0, h: 28.0 };
            Self::draw_button(&mut p, close, crate::locale::tr(lang, "Close"), true, self.mouse, false);
            self.ui.widgets.push((close, Widget::SettingsClose));
        }

        // --- Rufus image hashes dialog ---
        if self.ui.hash_open {
            let pw = 480.0;
            let ph = 330.0;
            let panel = Rect { x: (width - pw) / 2.0, y: (size.height as f32 - ph) / 2.0, w: pw, h: ph };
            p.fill(panel, FIELD);
            p.frame(panel, ACCENT);
            let title = if self.ui.image_path.is_empty() {
                String::new()
            } else {
                self.ui.image_path.rsplit('/').next().unwrap_or(&self.ui.image_path).to_string()
            };
            p.text(panel.x + 14.0, panel.y + 12.0, &title, 14.0, TEXT, true);
            let rows: [(&str, &str); 4] = match &self.ui.hash_state {
                Some(HashState::Ready(h)) => [
                    ("MD5:", h.md5.as_str()),
                    ("SHA1:", h.sha1.as_str()),
                    ("SHA256:", h.sha256.as_str()),
                    ("SHA512:", if self.ui.extra_hashes || self.ui.sha512_show {
                        h.sha512.as_str()
                    } else {
                        crate::locale::tr(lang, "Enable \"Extra hashes\" in the ⚙ Settings dialog, or press <Alt>-<H>.")
                    }),
                ],
                Some(HashState::Computing) => [
                    ("MD5:", crate::locale::tr(lang, "Computing…")),
                    ("SHA1:", crate::locale::tr(lang, "Computing…")),
                    ("SHA256:", crate::locale::tr(lang, "Computing…")),
                    ("SHA512:", crate::locale::tr(lang, "Enable \"Extra hashes\" in the ⚙ Settings dialog, or press <Alt>-<H>.")),
                ],
                Some(HashState::Failed(e)) => [
                    ("MD5:", crate::locale::tr(lang, "Failed")),
                    ("SHA1:", crate::locale::tr(lang, "Failed")),
                    ("SHA256:", crate::locale::tr(lang, "Failed")),
                    ("SHA512:", e.as_str()),
                ],
                None => [
                    ("MD5:", ""), ("SHA1:", ""), ("SHA256:", ""), ("SHA512:", ""),
                ],
            };
            let mut ry = panel.y + 46.0;
            for (label, value) in rows {
                p.text(panel.x + 14.0, ry + 4.0, label, 13.0, TEXT, false);
                let box_rect = Rect { x: panel.x + 80.0, y: ry, w: pw - 100.0, h: 28.0 };
                p.fill(box_rect, FIELD);
                p.frame(box_rect, BORDER);
                let mono: String = value.chars().take(64).collect();
                p.text(box_rect.x + 6.0, box_rect.y + 7.0, &mono, 12.0, TEXT, false);
                ry += 44.0;
            }
            let ok = Rect { x: panel.x + panel.w - 100.0, y: panel.y + ph - 42.0, w: 86.0, h: 28.0 };
            Self::draw_button(&mut p, ok, crate::locale::tr(lang, "OK"), true, self.mouse, true);
            self.ui.widgets.push((ok, Widget::HashOk));
        }

        // --- Rufus "Windows User Experience" dialog ---
        if self.ui.wue_open {
            let pw = 520.0;
            let ph = 400.0;
            let panel = Rect { x: (width - pw) / 2.0, y: (size.height as f32 - ph) / 2.0, w: pw, h: ph };
            p.fill(panel, FIELD);
            p.frame(panel, ACCENT);
            // header band
            let header = Rect { x: panel.x, y: panel.y, w: panel.w, h: 34.0 };
            p.fill(header, 0xDCEBF5);
            p.fill(Rect { x: panel.x, y: panel.y + 34.0, w: panel.w, h: 1.0 }, BORDER);
            p.text(panel.x + 14.0, panel.y + 9.0, crate::locale::tr(lang, "Windows User Experience"), 14.0, TEXT, true);
            // ? icon
            p.disc(panel.x + 28.0, panel.y + 70.0, 14.0, ACCENT);
            p.text(panel.x + 22.0, panel.y + 58.0, "?", 18.0, FIELD, true);
            p.text(panel.x + 52.0, panel.y + 58.0, crate::locale::tr(lang, "Customize Windows installation?"), 14.0, TEXT, false);
            let wue = self.ui.wue.unwrap_or(engine::WueOptions {
                bypass_tpm: true, no_online_account: true, disable_data_collection: true,
                local_account: false, regional_same_as_user: false,
                offline_internal_drives: true,
            });
            let checks = [
                ("Remove requirement for 4GB+ RAM, Secure Boot and TPM 2.0", wue.bypass_tpm),
                ("Remove requirement for an online Microsoft account", wue.no_online_account),
                ("Disable data collection (Skip privacy questions)", wue.disable_data_collection),
                ("Set a local account using the same name as this user's", wue.local_account),
                // Rufus 4.x msgid wording (was "using the same values")
                ("Set regional options to the same values as this user's", wue.regional_same_as_user),
                // Rufus MSG_332: offered for Windows To Go (SanPolicy 4)
                ("Prevent Windows To Go from accessing internal disks", wue.offline_internal_drives),
            ];
            let mut cy = panel.y + 96.0;
            for (i, (text, checked)) in checks.into_iter().enumerate() {
                Self::draw_checkbox(&mut p, panel.x + 30.0, cy, crate::locale::tr(lang, text), checked, true);
                self.ui.widgets.push((
                    Rect { x: panel.x + 30.0, y: cy - 2.0, w: panel.w - 60.0, h: 20.0 },
                    Widget::WueChk(i),
                ));
                cy += 24.0;
            }
            let ok = Rect { x: panel.x + panel.w - 230.0, y: panel.y + ph - 46.0, w: 100.0, h: 30.0 };
            let cancel = Rect { x: panel.x + panel.w - 118.0, y: panel.y + ph - 46.0, w: 104.0, h: 30.0 };
            Self::draw_button(&mut p, ok, crate::locale::tr(lang, "OK"), true, self.mouse, true);
            Self::draw_button(&mut p, cancel, crate::locale::tr(lang, "Cancel"), true, self.mouse, false);
            self.ui.widgets.push((ok, Widget::WueOk));
            self.ui.widgets.push((cancel, Widget::WueCancel));
        }

        // --- Rufus "Version selection" dialog (Windows To Go editions) ---
        if self.ui.wim_pick_open {
            let pw = 460.0;
            let rows = self.ui.wim_pick.len().min(8) as f32;
            let ph = 210.0 + rows * DD_ITEM_H;
            let panel = Rect { x: (width - pw) / 2.0, y: (size.height as f32 - ph) / 2.0, w: pw, h: ph };
            p.fill(panel, FIELD);
            p.frame(panel, ACCENT);
            // header band
            let header = Rect { x: panel.x, y: panel.y, w: panel.w, h: 34.0 };
            p.fill(header, 0xDCEBF5);
            p.fill(Rect { x: panel.x, y: panel.y + 34.0, w: panel.w, h: 1.0 }, BORDER);
            p.text(panel.x + 14.0, panel.y + 9.0, crate::locale::tr(lang, "Version selection"), 14.0, TEXT, true);
            p.text(panel.x + 14.0, panel.y + 46.0,
                crate::locale::tr(lang, "Please select the version of Windows you want to install:"), 13.0, TEXT, false);
            // edition list
            let list = Rect { x: panel.x + 14.0, y: panel.y + 72.0, w: pw - 28.0, h: rows * DD_ITEM_H + 4.0 };
            p.fill(list, FIELD);
            p.frame(list, BORDER);
            for (i, img) in self.ui.wim_pick.iter().take(8).enumerate() {
                let (idx, name) = (img.index, img.name.as_str());
                let iy = list.y + 2.0 + i as f32 * DD_ITEM_H;
                let row = Rect { x: list.x + 1.0, y: iy, w: list.w - 2.0, h: DD_ITEM_H };
                let hot = row.contains(self.mouse.0, self.mouse.1);
                let sel = i == self.ui.wim_pick_sel;
                if hot || sel {
                    p.fill(row, if sel { 0xCCE8FF } else { 0xE8F2FC });
                }
                let color = if sel { ACCENT } else { TEXT };
                let row_text = crate::locale::trf(lang, "{0} (image {1})", &[name, &idx.to_string()]);
                p.text(row.x + 6.0, iy + 3.0, &row_text, 12.0, color, false);
                self.ui.widgets.push((row, Widget::WimPickRow(i)));
            }
            let ok = Rect { x: panel.x + panel.w - 210.0, y: panel.y + ph - 46.0, w: 90.0, h: 30.0 };
            let cancel = Rect { x: panel.x + panel.w - 108.0, y: panel.y + ph - 46.0, w: 94.0, h: 30.0 };
            Self::draw_button(&mut p, ok, crate::locale::tr(lang, "OK"), true, self.mouse, true);
            Self::draw_button(&mut p, cancel, crate::locale::tr(lang, "Cancel"), true, self.mouse, false);
            self.ui.widgets.push((ok, Widget::WimPickOk));
            self.ui.widgets.push((cancel, Widget::WimPickCancel));
        }

        // --- built-in file browser overlay ---
        if let Some(b) = &self.ui.browser {
            let pw = 520.0;
            let ph = 440.0;
            let panel = Rect { x: (width - pw) / 2.0, y: (size.height as f32 - ph) / 2.0, w: pw, h: ph };
            p.fill(panel, FIELD);
            p.frame(panel, ACCENT);
            p.text(panel.x + 12.0, panel.y + 10.0, crate::locale::tr(lang, "Select an image file"), 14.0, TEXT, true);
            // current directory, truncated from the left if long
            let dir_text = b.dir.to_string_lossy().to_string();
            let dt: String = if dir_text.chars().count() > 62 {
                format!("…{}", dir_text.chars().skip(dir_text.chars().count() - 62).collect::<String>())
            } else {
                dir_text
            };
            p.text(panel.x + 12.0, panel.y + 34.0, &dt, 11.0, 0x505050, false);
            let list = Rect { x: panel.x + 10.0, y: panel.y + 52.0, w: pw - 20.0, h: ph - 100.0 };
            p.fill(list, 0xFAFAFA);
            p.frame(list, BORDER);
            let row_h = 18.0;
            let visible = ((list.h - 6.0) / row_h) as usize;
            for i in 0..visible {
                let ry = list.y + 4.0 + i as f32 * row_h;
                let row = Rect { x: list.x + 2.0, y: ry, w: list.w - 4.0, h: row_h };
                if i == 0 {
                    let hot = row.contains(self.mouse.0, self.mouse.1);
                    if hot {
                        p.fill(row, 0xCCE8FF);
                    }
                    p.text(row.x + 6.0, ry + 2.0, crate::locale::tr(lang, ".. (parent directory)"), 12.0, ACCENT, false);
                    self.ui.widgets.push((row, Widget::BrowserRow(0)));
                } else {
                    let ei = b.top + i - 1;
                    let Some((name, is_dir, size)) = b.entries.get(ei) else { break };
                    let hot = row.contains(self.mouse.0, self.mouse.1);
                    if hot {
                        p.fill(row, 0xCCE8FF);
                    }
                    let clickable = *is_dir || FileBrowser::is_image(name);
                    let line = if *is_dir {
                        format!("[DIR]  {}", name)
                    } else {
                        format!("{}  {}", fmt_size(*size), name)
                    };
                    let color = if *is_dir {
                        0x0060C0
                    } else if clickable {
                        TEXT
                    } else {
                        TEXT_DIS
                    };
                    p.text(row.x + 6.0, ry + 2.0, &line, 12.0, color, false);
                    if clickable {
                        self.ui.widgets.push((row, Widget::BrowserRow(i)));
                    }
                }
            }
            let cancel = Rect { x: panel.x + panel.w - 100.0, y: panel.y + ph - 38.0, w: 86.0, h: 28.0 };
            Self::draw_button(&mut p, cancel, crate::locale::tr(lang, "Cancel"), true, self.mouse, false);
            self.ui.widgets.push((cancel, Widget::BrowserCancel));
        }

        // --- Rufus "⬇ DOWNLOAD" dialog (official Windows/Linux ISOs) ---
        let dl_rects = download::render_dl(&mut self.ui, &mut p, self.mouse, width, size.height as f32);
        for (dd, r) in dl_rects {
            dd_rects.insert(dd, r);
        }

        // --- open dropdown overlay (drawn last, on top of everything) ---
        if let Some((dd, items, sel)) = dd_state {
            if let Some(&base_rect) = dd_rects.get(&dd) {
                if !items.is_empty() {
                    // long lists (download languages etc.) scroll, 15 rows visible
                    const DD_MAX_ROWS: usize = 15;
                    let visible = items.len().min(DD_MAX_ROWS);
                    let start = self.ui.dd_scroll.min(items.len() - visible);
                    let list_h = visible as f32 * DD_ITEM_H + 4.0;
                    // the bottom-left language combo opens upward like Rufus's
                    // (there is no room below the status strip)
                    let list_y = if dd == DD::Lang {
                        (base_rect.y - list_h).max(2.0)
                    } else {
                        base_rect.y + COMBO_H
                    };
                    let list = Rect { x: base_rect.x, y: list_y, w: base_rect.w.max(170.0), h: list_h };
                    p.fill(list, FIELD);
                    p.frame(list, ACCENT);
                    let hot_idx = ((self.mouse.1 - list.y - 2.0) / DD_ITEM_H).floor() as i64 + start as i64;
                    for (i, item) in items.iter().enumerate().skip(start).take(visible) {
                        let iy = list.y + 2.0 + (i - start) as f32 * DD_ITEM_H;
                        let row = Rect { x: list.x + 1.0, y: iy, w: list.w - 2.0, h: DD_ITEM_H };
                        if hot_idx == i as i64 {
                            p.fill(row, 0xCCE8FF);
                        }
                        let color = if i == sel { ACCENT } else { TEXT };
                        p.text(row.x + 6.0, iy + 3.0, item, 12.0, color, false);
                        self.ui.widgets.push((row, Widget::DropdownItem(dd, i)));
                    }
                    self.ui.dd_rect = Some((dd, list));
                }
            }
        }

        buffer.present().ok();
    }

    fn draw_combo(p: &mut Painter, r: Rect, text: &str, enabled: bool, open: bool, mouse: (f32, f32)) {
        let hot = enabled && r.contains(mouse.0, mouse.1);
        p.fill(r, if enabled { FIELD } else { 0xF4F4F4 });
        let border = if !enabled {
            BORDER_DIS
        } else if open || hot {
            ACCENT
        } else {
            BORDER
        };
        p.frame(r, border);
        let tcolor = if enabled { TEXT } else { TEXT_DIS };
        p.text(r.x + 6.0, r.y + 5.0, text, 12.0, tcolor, false);
        // dropdown arrow
        let ax = r.x + r.w - 16.0;
        let ay = r.y + r.h / 2.0;
        for i in 0..5 {
            let w = 9 - i * 2;
            p.fill(Rect { x: ax - (w as f32) / 2.0, y: ay - 3.0 + i as f32, w: w as f32, h: 1.0 }, if enabled { 0x505050 } else { BORDER_DIS });
        }
    }

    fn draw_field(p: &mut Painter, r: Rect, text: &str, focused: bool, enabled: bool) {
        p.fill(r, if enabled { FIELD } else { 0xF4F4F4 });
        let border = match (enabled, focused) {
            (false, _) => BORDER_DIS,
            (true, false) => BORDER,
            (true, true) => ACCENT,
        };
        p.frame(r, border);
        let tcolor = if enabled { TEXT } else { TEXT_DIS };
        p.text(r.x + 6.0, r.y + 5.0, text, 12.0, tcolor, false);
        if focused && enabled {
            let tw = p.text_w(text, 12.0, false);
            p.fill(Rect { x: r.x + 6.0 + tw + 1.0, y: r.y + 4.0, w: 1.5, h: 16.0 }, TEXT);
        }
    }

    fn draw_checkbox(p: &mut Painter, x: f32, y: f32, text: &str, checked: bool, enabled: bool) {
        let box_rect = Rect { x, y: y + 1.0, w: 15.0, h: 15.0 };
        p.fill(box_rect, if enabled { FIELD } else { 0xF4F4F4 });
        p.frame(box_rect, if enabled { BORDER } else { BORDER_DIS });
        if checked {
            // Win32-style check mark (filled blue box + white check)
            p.fill(Rect { x: x + 1.0, y: y + 2.0, w: 13.0, h: 13.0 }, ACCENT);
            p.text(x + 2.0, y - 1.0, "\u{2713}", 12.0, FIELD, true);
        }
        let tcolor = if enabled { TEXT } else { TEXT_DIS };
        p.text(x + 22.0, y, text, 13.0, tcolor, false);
    }

    fn draw_button(p: &mut Painter, r: Rect, label: &str, enabled: bool, mouse: (f32, f32), default_border: bool) {
        let hot = enabled && r.contains(mouse.0, mouse.1);
        let (bg, border, tc) = if !enabled {
            (0xECECEC, BORDER_DIS, TEXT_DIS)
        } else if hot {
            (BTN_HOVER, BTN_HOVER_BORDER, TEXT)
        } else {
            (BTN, if default_border { ACCENT } else { BTN_BORDER }, TEXT)
        };
        p.fill(r, bg);
        p.frame(r, border);
        let tw = p.text_w(label, 13.0, false);
        p.text(r.x + (r.w - tw) / 2.0, r.y + 9.0, label, 13.0, tc, false);
    }

    fn draw_icon_button(p: &mut Painter, r: Rect, kind: IconKind, enabled: bool, mouse: (f32, f32)) {
        let hot = enabled && r.contains(mouse.0, mouse.1);
        p.fill(r, if hot { BTN_HOVER } else { BG });
        p.frame(r, if hot { BTN_HOVER_BORDER } else if enabled { BORDER } else { BORDER_DIS });
        let c = if enabled { ACCENT } else { TEXT_DIS };
        let cx = r.x + r.w / 2.0;
        let cy = r.y + r.h / 2.0;
        match kind {
            IconKind::Globe => {
                // circle + meridian lines
                p.disc(cx, cy, 8.0, c);
                p.disc(cx, cy, 6.0, BG);
                p.fill(Rect { x: cx - 7.0, y: cy, w: 14.0, h: 1.0 }, c);
                p.fill(Rect { x: cx - 3.0, y: cy - 7.0, w: 1.0, h: 14.0 }, c);
            }
            IconKind::Disk => {
                // hard-drive glyph: box + activity dot
                p.frame(Rect { x: cx - 9.0, y: cy - 6.0, w: 18.0, h: 12.0 }, c);
                p.fill(Rect { x: cx - 6.0, y: cy - 1.0, w: 8.0, h: 2.0 }, c);
                p.disc(cx + 6.0, cy, 1.5, c);
            }
            IconKind::Info => {
                p.text(cx - 3.0, cy - 8.0, "i", 14.0, c, true);
            }
            IconKind::Settings => {
                // slider-style settings glyph
                for (i, kx) in [0.0, 5.0, 2.0].into_iter().enumerate() {
                    let ly = cy - 6.0 + i as f32 * 6.0;
                    p.fill(Rect { x: cx - 8.0, y: ly, w: 16.0, h: 1.5 }, c);
                    p.fill(Rect { x: cx - 8.0 + kx, y: ly - 2.5, w: 4.0, h: 6.0 }, c);
                }
            }
            IconKind::Log => {
                p.frame(Rect { x: cx - 7.0, y: cy - 8.0, w: 14.0, h: 16.0 }, c);
                for i in 0..3 {
                    p.fill(Rect { x: cx - 5.0, y: cy - 5.0 + i as f32 * 4.0, w: 10.0, h: 1.0 }, c);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum IconKind {
    Globe,
    Disk,
    Info,
    Settings,
    Log,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_layout_handles_latin_shaped_and_fallback_scripts() {
        let Some(fonts) = load_fonts() else { return }; // no system fonts: skip
        let mut empty: [u32; 0] = [];
        let p = Painter { buf: &mut empty, w: 0, h: 0, fonts };
        // plain Latin: one glyph per char, positive advance
        let (glyphs, w) = p.layout("START", 13.0, false);
        assert_eq!(glyphs.len(), 5);
        assert!(w > 0.0);
        // Arabic goes through shaping + bidi; glyphs keep a positive advance
        let (glyphs, w) = p.layout("\u{62a}\u{630}\u{628}\u{64a}\u{631}", 13.0, false);
        assert!(!glyphs.is_empty());
        assert!(w > 0.0);
        // CJK chars pick a fallback font entry (or render as notdef in the
        // primary font when no fallback is installed — entries still layout)
        let (glyphs, _w) = p.layout("\u{4e2d}\u{6587}", 13.0, false);
        assert_eq!(glyphs.len(), 2);
        // text_w agrees with layout (same code path)
        assert_eq!(p.text_w("START", 13.0, false), w_latin(&p));
    }

    fn w_latin(p: &Painter) -> f32 {
        p.layout("START", 13.0, false).1
    }

    #[test]
    fn image_hashes_are_correct() {
        let p = std::env::temp_dir().join(format!("rufus-hashtest-{}", std::process::id()));
        std::fs::write(&p, b"hello world").unwrap();
        let h = compute_image_hashes(&p).unwrap();
        // known digests of "hello world"
        assert_eq!(h.md5, "5eb63bbbe01eeed093cb22bb8f5acdc3");
        assert_eq!(h.sha1, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed");
        assert_eq!(
            h.sha256,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert!(h.sha512.len() == 128);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn fat32_hidden_on_large_drives() {
        // Rufus LARGE_FAT32_SIZE: FAT32 stays available up to 32 GiB
        assert!(fat32_allowed(32 * GB, false));
        assert!(!fat32_allowed(32 * GB + 1, false));
        assert!(!fat32_allowed(200 * GB, false));
        // "Force large FAT32 formatting" (Rufus Alt-L) unlocks it
        assert!(fat32_allowed(200 * GB, true));
    }

    #[test]
    fn file_browser_image_filter() {
        assert!(FileBrowser::is_image("ubuntu-22.04.iso"));
        assert!(FileBrowser::is_image("TEST.ISO"));
        assert!(FileBrowser::is_image("raspberry.img"));
        assert!(FileBrowser::is_image("core.img.gz"));
        assert!(!FileBrowser::is_image("notes.txt"));
        assert!(!FileBrowser::is_image("photo.png"));
    }

    #[test]
    fn file_browser_navigation_and_pick() {
        let base = std::env::temp_dir().join(format!("rufus-browsertest-{}", std::process::id()));
        let subdir = base.join("isos");
        let _ = std::fs::create_dir_all(&subdir);
        std::fs::write(subdir.join("distro.iso"), b"x").unwrap();
        std::fs::write(base.join("readme.txt"), b"x").unwrap();

        let mut b = FileBrowser { dir: base.clone(), entries: Vec::new(), top: 0 };
        b.reload();
        // directories sort first, ".." is row 0
        assert_eq!(b.entries[0].0, "isos");
        assert!(b.entries[0].1);
        // entering a directory by row index (row 1 = first entry)
        b.enter(1);
        assert!(b.dir.ends_with("isos"));
        // picking the image by row index (row 0 = "..", row 1 = distro.iso)
        let picked = b.pick(1).expect("distro.iso should be pickable");
        assert!(picked.ends_with("distro.iso"));
        // non-image files are not pickable
        let mut b2 = FileBrowser { dir: base.clone(), entries: Vec::new(), top: 0 };
        b2.reload();
        let txt_row = b2.entries.iter().position(|(n, _d, _)| n == "readme.txt").unwrap() + 1;
        assert!(b2.pick(txt_row).is_none(), "readme.txt must not be pickable");
        let _ = std::fs::remove_dir_all(&base);
    }
}
