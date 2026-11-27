//! Persisted GUI settings (~/.config/rufus-linux/settings.json).
//! Remembers the last image, device and options between runs.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub image_path: String,
    pub label: String,
    /// "fat32" | "ntfs" | "exfat"
    #[serde(default = "default_fs")]
    pub fs: String,
    /// Rufus "Image option" for Windows ISOs: 0 = standard install,
    /// 1 = Windows To Go
    #[serde(default)]
    pub image_option: u8,
    pub gpt: bool,
    pub efi_only: bool,
    pub verify: bool,
    pub quick: bool,
    /// "Check device for bad blocks" — destructive write test (Rufus)
    #[serde(default)]
    pub badblocks: bool,
    /// Rufus "Number of passes" combobox selection (0..=4: 1 pass, 2 passes,
    /// 4 passes SLC/MLC/TLC)
    #[serde(default)]
    pub badblocks_sel: u8,
    /// Linux persistence partition size in GB (0 = disabled), Rufus-style
    #[serde(default)]
    pub persist_gb: u64,
    /// "List USB Hard Drives" — also list USB hard drives (rotational USB)
    #[serde(default = "default_true")]
    pub list_usb_hdd: bool,
    /// "Add fixes for old BIOSes (extra partition, align, etc.)"
    #[serde(default)]
    pub old_bios_fixes: bool,
    #[serde(default)]
    pub advanced_open: bool,
    /// recently selected images for the SELECT split-button dropdown
    #[serde(default)]
    pub recent_images: Vec<String>,
    pub last_device: String,
    // ---- Rufus hidden settings (⚙ Settings dialog / Alt shortcuts) ----
    /// "Extra hashes (SHA512)" (Rufus EnableExtraHashes, Alt-H): show the
    /// computed SHA-512 in the image hashes dialog
    #[serde(default)]
    pub extra_hashes: bool,
    /// "Use Rufus MBR" (Rufus !DisableRufusMBR, Alt-A): false writes a plain
    /// MBR partition table without the Rufus boot code
    #[serde(default = "default_true")]
    pub use_rufus_mbr: bool,
    /// "Force large FAT32 formatting" (Rufus ForceLargeFat32Format, Alt-L):
    /// keep FAT32 selectable on drives larger than 32 GiB
    #[serde(default)]
    pub large_fat32: bool,
    /// "Preserve timestamps" (Rufus PreserveTimestamps, Alt-T): copy the ISO
    /// file dates to the written files
    #[serde(default)]
    pub preserve_timestamps: bool,
    /// "Persistent log" (Rufus PersistentLog, Ctrl-P): save the GUI log on exit
    #[serde(default)]
    pub persistent_log: bool,
    /// "Enable USB enumeration debug" (Rufus EnableUsbDebug, Alt-.): list the
    /// verbose device details in the log while scanning
    #[serde(default)]
    pub enable_usb_debug: bool,
    /// UI language: a Rufus locale code ("ar-SA", "fr-FR", …); empty = detect
    /// from the system locale like Rufus's first-run language detection
    #[serde(default)]
    pub locale: String,
}

fn default_fs() -> String {
    "fat32".into()
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            image_path: String::new(),
            label: "RUFUS".into(),
            fs: default_fs(),
            image_option: 0,
            gpt: false,
            efi_only: false,
            verify: false,
            quick: true,
            badblocks: false,
            badblocks_sel: 0,
            persist_gb: 0,
            list_usb_hdd: true,
            old_bios_fixes: false,
            advanced_open: false,
            recent_images: Vec::new(),
            last_device: String::new(),
            extra_hashes: false,
            use_rufus_mbr: true,
            large_fat32: false,
            preserve_timestamps: false,
            persistent_log: false,
            enable_usb_debug: false,
            locale: String::new(),
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("rufus-linux").join("settings.json"))
}

/// Load saved settings, falling back to defaults on any error.
pub fn load() -> Settings {
    settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Best-effort save; failures are silent (settings are non-critical).
pub fn save(s: &Settings) {
    let Some(p) = settings_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(p, json);
    }
}

/// Path of the session log, next to the settings file (Rufus keeps
/// %LocalAppData%\Rufus\rufus.log).
pub fn log_path() -> Option<PathBuf> {
    settings_path().map(|p| p.with_file_name("rufus.log"))
}

/// Write the session log lines to `path`, appended when `append` is set
/// (Rufus parity: persistent-log mode appends across sessions, otherwise the
/// log is overwritten). Best-effort; returns the io error for callers that
/// report it.
pub fn write_log_at(path: &std::path::Path, lines: &[String], append: bool) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut out = lines.join("\n");
    if !lines.is_empty() {
        out.push('\n');
    }
    if append {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(out.as_bytes())
    } else {
        std::fs::write(path, out)
    }
}

/// "Persistent log" (Rufus parity): the session log is always written on exit
/// next to the settings file; persistent-log mode appends instead of
/// overwriting. Best-effort.
pub fn save_log(lines: &[String], append: bool) -> Option<PathBuf> {
    let p = log_path()?;
    write_log_at(&p, lines, append).ok()?;
    Some(p)
}

/// C `ctime()`-style timestamp like the session-start line Rufus writes in
/// persistent log mode: `[Fri Nov 21 19:52:04 2026]` (local time, day
/// space-padded like the C original).
pub fn ctime_now() -> String {
    const WDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = secs as libc::time_t;
    // SAFETY: `libc::tm` is a plain C struct of integer fields with no
    // niches, padding-sensitive invariants or references, so the
    // all-zeroes bit pattern is a valid value to hand to C code (which
    // `localtime_r` then populates).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `secs` is a valid, aligned `libc::time_t` and `&mut tm` a
    // valid, aligned `tm`; `localtime_r` writes only through its two
    // parameters and returns a pointer into `tm` or null, which the code
    // checks.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "[timestamp unavailable]".into();
    }
    let wday = WDAY[tm.tm_wday.clamp(0, 6) as usize];
    let mon = MON[tm.tm_mon.clamp(0, 11) as usize];
    format!(
        "[{} {} {:2} {:02}:{:02}:{:02} {}]",
        wday,
        mon,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        tm.tm_year + 1900
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let mut s = Settings {
            image_path: "/tmp/x.iso".into(),
            label: "TEST".into(),
            persist_gb: 4,
            badblocks: true,
            badblocks_sel: 3,
            recent_images: vec!["/tmp/x.iso".into()],
            ..Default::default()
        };
        s.locale = "ar-SA".into();
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.locale, "ar-SA");
        assert_eq!(back.image_path, "/tmp/x.iso");
        assert_eq!(back.label, "TEST");
        assert!(back.quick); // default preserved
        assert!(back.badblocks);
        assert_eq!(back.badblocks_sel, 3);
        assert_eq!(back.persist_gb, 4);
        assert!(back.list_usb_hdd);
        assert_eq!(back.recent_images.len(), 1);
    }

    #[test]
    fn settings_hidden_options_roundtrip() {
        let s = Settings {
            extra_hashes: true,
            use_rufus_mbr: false,
            large_fat32: true,
            preserve_timestamps: true,
            persistent_log: true,
            enable_usb_debug: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.locale, ""); // pre-locale settings.json still loads
        assert!(back.extra_hashes);
        assert!(!back.use_rufus_mbr); // explicit override survived
        assert!(back.large_fat32);
        assert!(back.preserve_timestamps);
        assert!(back.persistent_log);
        assert!(back.enable_usb_debug);
    }

    #[test]
    fn settings_hidden_options_fall_back() {
        // settings.json written before the hidden settings existed: Rufus MBR
        // stays enabled (the default), everything else off
        let old = r#"{"image_path":"","label":"RUFUS","fs":"fat32","gpt":false,
            "efi_only":false,"verify":false,"quick":true,"last_device":""}"#;
        let back: Settings = serde_json::from_str(old).unwrap();
        assert!(back.use_rufus_mbr);
        assert!(!back.extra_hashes);
        assert!(!back.large_fat32);
        assert!(!back.preserve_timestamps);
        assert!(!back.persistent_log);
        assert!(!back.enable_usb_debug);
    }

    #[test]
    fn write_log_overwrites_then_appends() {
        let p = std::env::temp_dir().join(format!(
            "rufus-linux-log-test-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let lines = vec!["Ready.".to_string(), "line two".to_string()];
        write_log_at(&p, &lines, false).unwrap();
        write_log_at(&p, &["Appended line".to_string()], true).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        // first call overwrites, second appends (Rufus persistent-log parity)
        assert_eq!(out, "Ready.\nline two\nAppended line\n");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn write_log_empty_lines_writes_nothing_new() {
        let p = std::env::temp_dir().join(format!(
            "rufus-linux-log-empty-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_log_at(&p, &[], false).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn ctime_now_format() {
        // C ctime shape: "[Www Mmm dd hh:mm:ss yyyy]" with a space-padded day
        let s = ctime_now();
        assert!(s.starts_with('[') && s.ends_with(']'), "got {s}");
        let inner = &s[1..s.len() - 1];
        let parts: Vec<&str> = inner.split(' ').filter(|p| !p.is_empty()).collect();
        assert_eq!(parts.len(), 5, "got {s}");
        assert!(
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"].contains(&parts[0]),
            "got {s}"
        );
        assert!(
            ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
                .contains(&parts[1]),
            "got {s}"
        );
        let hhmmss = parts[3].split(':').count();
        assert_eq!(hhmmss, 3, "got {s}");
        assert_eq!(parts[4].len(), 4, "got {s}");
    }

    #[test]
    fn settings_missing_new_fields_fall_back() {
        // an old settings.json (before these options existed) still loads
        let old = r#"{"image_path":"","label":"RUFUS","fs_sel":0,"gpt":false,
            "efi_only":false,"verify":false,"quick":true,"last_device":""}"#;
        let back: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(back.persist_gb, 0);
        assert_eq!(back.list_usb_hdd, true);
        assert_eq!(back.recent_images.len(), 0);
    }

    #[test]
    fn load_falls_back_to_defaults() {
        // load() must never panic on missing/corrupt files
        let s = load();
        assert_eq!(s.label, "RUFUS");
    }

    /// Full save()/load() round-trip through the real settings file, with
    /// the XDG config dir redirected to a temp directory. No other test in
    /// this binary reads the config dir, so the env switch is safe.
    #[test]
    fn save_load_roundtrip_in_temp_config_dir() {
        let tmp = std::env::temp_dir().join(format!("rufus-linux-test-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let old = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", &tmp);
        let s = Settings {
            image_path: "/tmp/roundtrip.iso".into(),
            label: "RT".into(),
            persist_gb: 4,
            ..Default::default()
        };
        save(&s);
        let file = tmp.join("rufus-linux").join("settings.json");
        assert!(
            file.exists(),
            "save() must write settings.json under the config dir"
        );
        let loaded = load();
        assert_eq!(loaded.image_path, "/tmp/roundtrip.iso");
        assert_eq!(loaded.label, "RT");
        assert_eq!(loaded.persist_gb, 4);
        match old {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
