//! Official ISO downloads (Rufus parity): the "⬇ DOWNLOAD" feature.
//!
//! Windows images come straight from Microsoft through the
//! software-download-connector API, replaying the exact request sequence of
//! Rufus's Fido script (session whitelisting, ov-df anti-bot handshake,
//! getskuinformationbyproductedition, GetProductDownloadLinksBySku).
//!
//! Linux images come from an embedded catalog of official distro locations,
//! resolved against the distros' own published SHA-256 sums at download time
//! so file names can drift with point releases without breaking the app.
//!
//! There is no HTTP client dependency on purpose: every request runs through
//! the system `curl` binary (Rufus itself shells out to PowerShell/Fido, so a
//! subprocess is in keeping with the original). Progress and cancellation are
//! reported through callbacks/`AtomicBool` so the GUI can drive them.

use anyhow::{anyhow, bail, Result};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Browser-grade user agent — Microsoft's download endpoints refuse requests
/// without something recognizable (Fido gets away with PowerShell's default).
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

// Constants lifted verbatim from Fido.ps1 v1.70 (Microsoft-side identifiers).
const MSFT_ORG_ID: &str = "y6jn8c31";
const MSFT_PROFILE_ID: &str = "606624d44113";
const MSFT_INSTANCE_ID: &str = "560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175";

// ---------- curl subprocess engine ----------

fn curl_available() -> bool {
    Command::new("curl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// GET a URL via curl, returning the response body.
/// `extra` carries additional curl arguments (headers, referer...).
fn http_get(url: &str, extra: &[&str], jar: Option<&Path>) -> Result<Vec<u8>> {
    if !curl_available() {
        bail!("the 'curl' command is required for downloads (install curl and retry)");
    }
    let mut cmd = Command::new("curl");
    cmd.arg("-sS") // errors only (no meter)
        .arg("-L") // follow redirects
        .arg("--fail")
        .arg("--max-time=30")
        .arg("--retry=2")
        .arg("--retry-delay=1")
        .arg("--user-agent")
        .arg(USER_AGENT);
    if let Some(j) = jar {
        cmd.arg(format!("--cookie={}", j.display()));
        cmd.arg(format!("--cookie-jar={}", j.display()));
    }
    for a in extra {
        cmd.arg(a);
    }
    cmd.arg(url);
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "curl {}: {}",
            url_file_name(url),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn http_get_text(url: &str, extra: &[&str], jar: Option<&Path>) -> Result<String> {
    let body = http_get(url, extra, jar)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Content-Length of the final redirect target (best-effort, HEAD request).
pub fn content_length(url: &str) -> Option<u64> {
    let out = Command::new("curl")
        .args([
            "-sS",
            "-I",
            "-L",
            "--max-time=30",
            "--user-agent",
            USER_AGENT,
        ])
        .arg(url)
        .output()
        .ok()?;
    // several hops may print headers — the last content-length wins
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1)?.trim().parse().ok())
}

// ---------- URL/file helpers ----------

/// File name part of a download URL (query/hash stripped, ".iso" enforced).
pub fn url_file_name(url: &str) -> String {
    let core = url.split(['?', '#']).next().unwrap_or(url);
    let name = core.rsplit('/').next().unwrap_or(core);
    let name = percent_decode(name);
    if name.is_empty() {
        return "download.iso".into();
    }
    if name.to_lowercase().ends_with(".iso") {
        name
    } else {
        format!("{name}.iso")
    }
}

/// Minimal %XX decoder — Microsoft URLs occasionally encode the file name.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "dir part" of a URL: everything up to and including the last '/'.
fn dir_base(url: &str) -> String {
    match url.rfind('/') {
        Some(i) => url[..=i].to_string(),
        None => String::new(),
    }
}

/// Destination path that never overwrites an existing file: "x.iso" becomes
/// "x (1).iso", "x (2).iso", ... like most browsers do.
pub fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut candidate = dir.join(name);
    let mut n = 1u32;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    candidate
}

/// Where downloaded ISOs go (Rufus defaults to the user's Downloads folder).
pub fn downloads_dir() -> PathBuf {
    if let Some(d) = dirs::download_dir() {
        return d;
    }
    if let Some(h) = dirs::home_dir() {
        let d = h.join("Downloads");
        if d.is_dir() {
            return d;
        }
    }
    std::env::temp_dir()
}

/// Shared cookie-jar directory for the Microsoft session flow.
pub fn jar_dir() -> PathBuf {
    let d = std::env::temp_dir().join("rufus-linux-dl");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Random UUID v4 (session ids for the Microsoft flow), from /dev/urandom.
pub fn new_uuid_v4() -> String {
    let mut b = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok();
    if !ok {
        // crude fallback: mix the wall clock (never used on Linux)
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, chunk) in b.iter_mut().enumerate() {
            *chunk = (t >> (i * 8 % 120)) as u8 ^ (i as u8).wrapping_mul(0x9E);
        }
    }
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // variant 10xx
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------- Windows (Microsoft software-download-connector API, Fido flow) ----------

/// One edition of a release. Microsoft splits x64/ARM64 into separate
/// productEditionIds (Fido: "they have to handle multiple IDs per release"),
/// so an edition carries a list — each id gets its own session.
pub struct WinEdition {
    pub name: &'static str,
    pub ids: &'static [u32],
}

pub struct WinRelease {
    pub name: &'static str,
    pub editions: &'static [WinEdition],
}

/// The catalog Fido.ps1 v1.70 ships with (China editions are zh-specific and
/// omitted). Updates land with new Fido releases.
pub const WIN_RELEASES: &[WinRelease] = &[
    WinRelease {
        name: "Windows 11 25H2 (Build 26200.8037 — 2026.03)",
        editions: &[WinEdition {
            name: "Windows 11 Home/Pro/Edu",
            ids: &[3321, 3324],
        }],
    },
    WinRelease {
        name: "Windows 10 22H2 (Build 19045.2965 — 2023.05)",
        editions: &[WinEdition {
            name: "Windows 10 Home/Pro/Edu",
            ids: &[2618],
        }],
    },
];

/// A language and the SKU ids that carry it (one per edition id/session).
#[derive(Clone)]
pub struct WinLang {
    /// "en-us" style code
    pub name: String,
    /// localized display name, e.g. "English (United States)"
    pub display: String,
    pub skus: Vec<WinSku>,
}

#[derive(Clone)]
pub struct WinSku {
    pub sku_id: u32,
    /// session UUID that retrieved this SKU (Microsoft refuses a download
    /// whose session does not match the last SKU listing)
    pub session: String,
    pub jar: PathBuf,
}

#[derive(Clone)]
pub struct WinLink {
    pub arch: String,
    pub url: String,
}

/// Microsoft arch type code → name (Fido Get-Arch-From-Type).
pub fn arch_from_type(t: u64) -> &'static str {
    match t {
        0 => "x86",
        1 => "x64",
        2 => "ARM64",
        _ => "unknown",
    }
}

/// Keep the first link per architecture (x64 and ARM64 come from different
/// edition ids / sessions and would otherwise appear twice).
pub fn dedupe_archs(links: Vec<WinLink>) -> Vec<WinLink> {
    let mut out: Vec<WinLink> = Vec::new();
    for l in links {
        if !out.iter().any(|o| o.arch == l.arch) {
            out.push(l);
        }
    }
    out
}

/// Index of the architecture to preselect (prefer x64, like Fido).
pub fn default_arch_index(links: &[WinLink]) -> usize {
    links
        .iter()
        .position(|l| l.arch == "x64")
        .unwrap_or(0)
        .min(links.len().saturating_sub(1))
}

/// Language to preselect from the system locale (Fido Select-Language,
/// simplified to a two-letter prefix match).
pub fn default_lang_index(langs: &[WinLang], sys_locale: &str) -> usize {
    let sys = sys_locale
        .split(['_', '.', '-'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    if sys.is_empty() {
        return 0;
    }
    langs
        .iter()
        .position(|l| l.name.to_lowercase().starts_with(&sys))
        .unwrap_or(0)
}

/// Extract the `w` anti-bot parameter from the mdt.js payload (Fido regex
/// `[?&]w=([A-F0-9]+)`).
fn extract_w(body: &str) -> Option<String> {
    let b = body.as_bytes();
    for i in 0..b.len().saturating_sub(2) {
        if b[i] == b'w' && b[i + 1] == b'=' && i > 0 && (b[i - 1] == b'?' || b[i - 1] == b'&') {
            let end = b[i + 2..]
                .iter()
                .position(|c| !c.is_ascii_hexdigit())
                .map(|p| i + 2 + p)
                .unwrap_or(b.len());
            if end > i + 2 {
                return Some(body[i + 2..end].to_string());
            }
        }
    }
    None
}

/// Extract the `rticks` anti-bot parameter (Fido regex `rticks\=\"+?(\d+)` —
/// the payload embeds escaped quotes and an optional '+').
fn extract_rticks(body: &str) -> Option<String> {
    let idx = body.find("rticks=")?;
    let mut rest = &body[idx + "rticks=".len()..];
    rest = rest.trim_start_matches(['\\', '"', '+']);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// The session "protection" handshake: whitelist the session id through
/// vlscppe, then answer the ov-df challenge (mdt.js → w/rticks → reply).
fn msft_preflight(session: &str, jar: &Path) -> Result<()> {
    http_get(
        &format!("https://vlscppe.microsoft.com/tags?org_id={MSFT_ORG_ID}&session_id={session}"),
        &[],
        Some(jar),
    )?;
    let mdt = http_get(
        &format!("https://ov-df.microsoft.com/mdt.js?instanceId={MSFT_INSTANCE_ID}&PageId=si&session_id={session}"),
        &[],
        Some(jar),
    )?;
    let mdt = String::from_utf8_lossy(&mdt);
    let w = extract_w(&mdt).ok_or_else(|| anyhow!("could not extract ov-df 'w' data"))?;
    let rticks =
        extract_rticks(&mdt).ok_or_else(|| anyhow!("could not extract ov-df 'rticks' data"))?;
    http_get(
        &format!(
            "https://ov-df.microsoft.com/?session_id={session}&CustomerId={MSFT_INSTANCE_ID}&PageId=si&w={w}&mdt={}&rticks={rticks}",
            now_ms()
        ),
        &[],
        Some(jar),
    )?;
    Ok(())
}

/// Surface Microsoft's structured error payload (type 9 = IP banned).
fn json_first_error(v: &serde_json::Value) -> Option<String> {
    match v.get("Errors") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(a)) => {
            let e = a.first()?;
            let t = e.get("Type").and_then(|x| x.as_i64()).unwrap_or(0);
            let m = e
                .get("Value")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown error");
            if t == 9 {
                Some(format!(
                    "your IP address has been banned by Microsoft for issuing too many ISO \
                     download requests (message code 715-123130) — try again later: {m}"
                ))
            } else {
                Some(m.to_string())
            }
        }
        Some(_) => Some("unexpected error payload from the Microsoft server".into()),
    }
}

/// Languages for a product edition (Fido Get-Windows-Languages). One session
/// per edition id; "may require a few attempts" per Fido, so retry once.
pub fn win_fetch_langs(ids: &[u32], tmpdir: &Path) -> Result<Vec<WinLang>> {
    let mut langs: Vec<WinLang> = Vec::new();
    for (slot, id) in ids.iter().enumerate() {
        let session = new_uuid_v4();
        let jar = tmpdir.join(format!("msft-{slot}-{id}.jar"));
        msft_preflight(&session, &jar)?;
        let url = format!(
            "https://www.microsoft.com/software-download-connector/api/getskuinformationbyproductedition\
             ?profile={MSFT_PROFILE_ID}&productEditionId={id}&SKU=undefined&friendlyFileName=undefined\
             &Locale=en-US&sessionID={session}"
        );
        let mut body: Option<Vec<u8>> = None;
        let mut last_err = String::new();
        for attempt in 0..3 {
            if attempt != 0 {
                std::thread::sleep(Duration::from_secs(2));
            }
            match http_get(&url, &["-H", "Accept: application/json"], Some(&jar)) {
                Ok(b) => {
                    body = Some(b);
                    break;
                }
                Err(e) => last_err = format!("{e:#}"),
            }
        }
        let body = body
            .ok_or_else(|| anyhow!("could not retrieve languages from the server: {last_err}"))?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| anyhow!("unexpected response from the Microsoft server: {e}"))?;
        if let Some(e) = json_first_error(&v) {
            bail!("{e}");
        }
        let skus = v
            .get("Skus")
            .and_then(|s| s.as_array())
            .ok_or_else(|| anyhow!("could not parse languages from the Microsoft server"))?;
        for sku in skus {
            let name = sku
                .get("Language")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let display = sku
                .get("LocalizedLanguage")
                .and_then(|x| x.as_str())
                .unwrap_or(&name)
                .to_string();
            let sku_id = sku.get("Id").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            if name.is_empty() || sku_id == 0 {
                continue;
            }
            match langs.iter_mut().find(|l| l.name == name) {
                Some(l) => l.skus.push(WinSku {
                    sku_id,
                    session: session.clone(),
                    jar: jar.clone(),
                }),
                None => langs.push(WinLang {
                    name,
                    display,
                    skus: vec![WinSku {
                        sku_id,
                        session: session.clone(),
                        jar: jar.clone(),
                    }],
                }),
            }
        }
    }
    if langs.is_empty() {
        bail!("could not parse languages from the Microsoft server");
    }
    Ok(langs)
}

/// ISO download links per architecture for a language (Fido
/// Get-Windows-Download-Links). A Referer is required or the server may deny
/// the request.
pub fn win_fetch_links(lang: &WinLang, locale_url_id: &str) -> Result<Vec<WinLink>> {
    let mut links: Vec<WinLink> = Vec::new();
    for sku in &lang.skus {
        let url = format!(
            "https://www.microsoft.com/software-download-connector/api/GetProductDownloadLinksBySku\
             ?profile={MSFT_PROFILE_ID}&productEditionId=undefined&SKU={}&friendlyFileName=undefined\
             &Locale=en-US&sessionID={}",
            sku.sku_id, sku.session
        );
        let body = http_get(
            &url,
            &[
                "-H",
                "Accept: application/json",
                "--referer",
                &format!("https://www.microsoft.com/software-download/{locale_url_id}"),
            ],
            Some(&sku.jar),
        )?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| anyhow!("unexpected response from the Microsoft server: {e}"))?;
        if let Some(e) = json_first_error(&v) {
            bail!("{e}");
        }
        let opts = v
            .get("ProductDownloadOptions")
            .and_then(|p| p.as_array())
            .ok_or_else(|| anyhow!("could not retrieve ISO download links"))?;
        for o in opts {
            let t = o.get("DownloadType").and_then(|x| x.as_u64()).unwrap_or(9);
            let uri = o
                .get("Uri")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if uri.is_empty() {
                continue;
            }
            links.push(WinLink {
                arch: arch_from_type(t).to_string(),
                url: uri,
            });
        }
    }
    if links.is_empty() {
        bail!("could not retrieve ISO download links");
    }
    Ok(dedupe_archs(links))
}

// ---------- Linux catalog (official distro locations + published sums) ----------

/// How to obtain the official SHA-256 for a resolved file name.
#[derive(Clone, Copy)]
pub enum Sums {
    /// a fixed SHA256SUMS-style file URL
    Url(&'static str),
    /// <iso file name><suffix> (e.g. "x.iso" → "x.iso.sha256")
    SamePlus(&'static str),
    /// rename the iso file name (e.g. "-live.iso" → "-CHECKSUM", Fedora style)
    Replace(&'static str, &'static str),
}

/// Where an official image lives and how to pin down the exact file.
#[derive(Clone, Copy)]
pub enum IsoTarget {
    /// direct URL (stable names: Arch "latest", openSUSE "Current")
    Fixed {
        url: &'static str,
        sums: Option<Sums>,
    },
    /// pick the matching line of a published SHA256SUMS file (the URL is then
    /// the sums file's directory + the file name) — immune to point releases
    PickSums {
        sums_url: &'static str,
        must: &'static [&'static str],
        skip: &'static [&'static str],
    },
    /// pick the matching .iso of a directory listing (Fedora, Mint, Pop!_OS…)
    PickDir {
        dir_url: &'static str,
        must: &'static [&'static str],
        skip: &'static [&'static str],
        sums: Option<Sums>,
    },
}

pub struct DistroVariant {
    pub label: &'static str,
    pub target: IsoTarget,
}

pub struct Distro {
    pub name: &'static str,
    pub variants: &'static [DistroVariant],
}

/// Official distro locations, mirroring Rufus 4.6's download list. Extending
/// the list is a matter of adding entries here.
pub const DISTROS: &[Distro] = &[
    Distro {
        name: "Ubuntu",
        variants: &[DistroVariant {
            label: "24.04 LTS — Desktop (amd64)",
            target: IsoTarget::PickSums {
                sums_url: "https://releases.ubuntu.com/24.04/SHA256SUMS",
                must: &["desktop-amd64.iso"],
                skip: &["server", "wubi"],
            },
        }],
    },
    Distro {
        name: "Debian",
        variants: &[DistroVariant {
            label: "Latest stable — netinst (amd64)",
            target: IsoTarget::PickSums {
                sums_url: "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/SHA256SUMS",
                must: &["amd64-netinst.iso"],
                skip: &["firmware"],
            },
        }],
    },
    Distro {
        name: "Arch Linux",
        variants: &[DistroVariant {
            label: "Latest (x86_64)",
            target: IsoTarget::Fixed {
                url: "https://geo.mirror.pkgbuild.com/iso/latest/archlinux-x86_64.iso",
                sums: Some(Sums::Url("https://geo.mirror.pkgbuild.com/iso/latest/sha256sums.txt")),
            },
        }],
    },
    Distro {
        name: "Fedora",
        variants: &[DistroVariant {
            label: "Workstation 44 — Live (x86_64)",
            target: IsoTarget::PickDir {
                dir_url: "https://dl.fedoraproject.org/pub/fedora/linux/releases/44/Workstation/x86_64/iso/",
                must: &["Workstation", "x86_64"],
                skip: &["CHECKSUM", "aarch64", "armhfp"],
                sums: Some(Sums::Replace("-live.iso", "-CHECKSUM")),
            },
        }],
    },
    Distro {
        name: "Linux Mint",
        variants: &[DistroVariant {
            label: "22.x — Cinnamon (64-bit)",
            target: IsoTarget::PickDir {
                dir_url: "https://mirrors.edge.kernel.org/mint/stable/22.3/",
                must: &["cinnamon"],
                skip: &[],
                sums: Some(Sums::Url("https://mirrors.edge.kernel.org/mint/stable/22.3/sha256sum.txt")),
            },
        }],
    },
    Distro {
        name: "openSUSE",
        variants: &[DistroVariant {
            label: "Tumbleweed — DVD (x86_64)",
            target: IsoTarget::Fixed {
                url: "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-DVD-x86_64-Current.iso",
                sums: Some(Sums::Url(
                    "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-DVD-x86_64-Current.iso.sha256",
                )),
            },
        }],
    },
    Distro {
        name: "Pop!_OS",
        variants: &[DistroVariant {
            label: "24.04 LTS — Intel/AMD",
            target: IsoTarget::PickDir {
                dir_url: "https://iso.pop.system76.com/",
                must: &["24.04", "intel"],
                skip: &["nvidia"],
                sums: None,
            },
        }],
    },
    Distro {
        name: "Alpine Linux",
        variants: &[DistroVariant {
            label: "Latest stable — Standard (x86_64)",
            target: IsoTarget::PickDir {
                dir_url: "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64/",
                must: &["alpine-standard"],
                skip: &[],
                sums: Some(Sums::SamePlus(".sha256")),
            },
        }],
    },
];

/// A fully resolved, ready-to-download image.
#[derive(Clone)]
pub struct IsoResolved {
    pub url: String,
    pub file_name: String,
    pub sha256: Option<String>,
}

pub fn resolve_target(t: &IsoTarget) -> Result<IsoResolved> {
    match *t {
        IsoTarget::Fixed { url, sums } => {
            let name = url_file_name(url);
            let sha256 = sums.and_then(|s| fetch_sha256(s, &name, url));
            Ok(IsoResolved {
                url: url.into(),
                file_name: name,
                sha256,
            })
        }
        IsoTarget::PickSums {
            sums_url,
            must,
            skip,
        } => {
            let text = http_get_text(sums_url, &[], None)?;
            let (name, hash) = pick_sums(&text, must, skip).ok_or_else(|| {
                anyhow!(
                    "no matching image found in {} — the distro's layout may have changed",
                    sums_url
                )
            })?;
            Ok(IsoResolved {
                url: format!("{}{}", dir_base(sums_url), name),
                file_name: name,
                sha256: Some(hash),
            })
        }
        IsoTarget::PickDir {
            dir_url,
            must,
            skip,
            sums,
        } => {
            let text = http_get_text(dir_url, &[], None)?;
            let name = pick_dir_name(&text, must, skip).ok_or_else(|| {
                anyhow!(
                    "no matching image found in {} — the distro's layout may have changed",
                    dir_url
                )
            })?;
            let url = format!("{}{}", dir_base(dir_url), name);
            let sha256 = sums.and_then(|s| fetch_sha256(s, &name, &url));
            Ok(IsoResolved {
                url,
                file_name: name,
                sha256,
            })
        }
    }
}

/// Parse "hash  name" / "hash *name" / "SHA256 (name) = hash" lines — the
/// three formats distros actually publish.
fn parse_sums(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SHA256 (") {
            // Fedora CHECKSUM format: SHA256 (name) = hash
            if let (Some(name), Some(hash)) = (rest.split(") = ").next(), rest.rsplit("= ").next())
            {
                let name = name.trim();
                let hash = hash.trim().to_lowercase();
                if name.len() == 64 && hex_ok(name) {
                    // paranoid: "SHA256 (<64hex>) = <64hex>" is malformed; skip
                    continue;
                }
                if hex_ok(&hash) {
                    out.push((name.to_string(), hash));
                }
            }
            continue;
        }
        let mut it = line.split_whitespace();
        let (tok, name) = match (it.next(), it.next()) {
            (Some(t), Some(n)) => (t, n),
            _ => continue,
        };
        // sha256sum output: "hash  file" — binary mode prefixes the file with '*'
        let name = name.trim_start_matches('*');
        let tok = tok.to_lowercase();
        if tok.len() == 64 && hex_ok(&tok) {
            let name = name.rsplit('/').next().unwrap_or(name);
            out.push((name.to_string(), tok));
        }
    }
    out
}

fn hex_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Pick a sums entry whose name matches must/skip.
fn pick_sums(text: &str, must: &[&str], skip: &[&str]) -> Option<(String, String)> {
    parse_sums(text).into_iter().find(|(name, _)| {
        name.to_lowercase().contains_all_ci(must) && !name.to_lowercase().contains_any_ci(skip)
    })
}

trait ContainsCi {
    fn contains_ci(&self, pat: &str) -> bool;
    fn contains_all_ci(&self, pats: &[&str]) -> bool;
    fn contains_any_ci(&self, pats: &[&str]) -> bool;
}

impl ContainsCi for str {
    fn contains_ci(&self, pat: &str) -> bool {
        self.to_lowercase().contains(&pat.to_lowercase())
    }
    fn contains_all_ci(&self, pats: &[&str]) -> bool {
        pats.iter().all(|p| self.contains_ci(p))
    }
    fn contains_any_ci(&self, pats: &[&str]) -> bool {
        pats.iter().any(|p| self.contains_ci(p))
    }
}

/// Pick a file name from a directory listing (HTML autoindex or plain text):
/// candidates are quoted tokens ending in ".iso", filtered by must/skip,
/// preferring the lexicographically greatest (highest point release).
fn pick_dir_name(text: &str, must: &[&str], skip: &[&str]) -> Option<String> {
    text.split('"')
        .map(|tok| tok.rsplit('/').next().unwrap_or("").trim().to_string())
        .filter(|name| name.to_lowercase().ends_with(".iso"))
        .filter(|name| name.contains_all_ci(must) && !name.contains_any_ci(skip))
        .max()
}

/// Fetch the official SHA-256 for a resolved file name (None = not published
/// / unreachable — the download proceeds, verification is then skipped).
fn fetch_sha256(spec: Sums, file_name: &str, source_url: &str) -> Option<String> {
    let sums_url: String = match spec {
        Sums::Url(u) => u.to_string(),
        Sums::SamePlus(sfx) => format!("{}{file_name}{sfx}", dir_base(source_url)),
        Sums::Replace(from, to) => format!(
            "{}{}",
            dir_base(source_url),
            file_name.replacen(from, to, 1)
        ),
    };
    let text = http_get_text(&sums_url, &[], None).ok()?;
    let want = file_name.to_lowercase();
    parse_sums(&text)
        .into_iter()
        .find(|(name, _)| name.to_lowercase() == want || name.to_lowercase().contains(&want))
        .map(|(_, hash)| hash)
}

// ---------- download engine ----------

pub struct DownloadResult {
    pub path: PathBuf,
    pub file_name: String,
    pub sha256: String,
    pub bytes: u64,
    /// true when the digest matched the distro's published sums
    pub verified: bool,
}

/// SHA-1 + SHA-256 of a file, streamed in 1 MiB chunks (same recipe as the
/// GUI's image-hashes dialog).
fn stream_sha256(path: &Path) -> Result<String> {
    use sha2::Digest as _;
    let mut f = std::fs::File::open(path)?;
    let mut d = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        d.update(&buf[..n]);
    }
    Ok(hex(&d.finalize()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Download `url` into `dest_dir` via curl, reporting progress and honouring
/// `cancel`. The file is removed on cancellation, curl failure or digest
/// mismatch — a partial ISO never masquerades as a good one.
pub fn download_iso(
    url: &str,
    dest_dir: &Path,
    expected_sha256: Option<&str>,
    cancel: &Arc<AtomicBool>,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<DownloadResult> {
    let file_name = url_file_name(url);
    let dest = unique_dest(dest_dir, &file_name);
    let total = content_length(url);
    progress(0, total);

    let mut child = Command::new("curl")
        .arg("-sS")
        .arg("-L")
        .arg("--fail")
        .arg("--retry=2")
        .arg("--retry-delay=2")
        // abort when the transfer stalls (<10 KiB/s for 30 s)
        .arg("--speed-limit=10240")
        .arg("--speed-time=30")
        .arg("--user-agent")
        .arg(USER_AGENT)
        .arg("--output")
        .arg(&dest)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg(url)
        .spawn()
        .map_err(|e| anyhow!("could not start curl: {e}"))?;

    let mut last = 0u64;
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&dest);
            bail!("download cancelled by user");
        }
        if let Some(st) = child.try_wait()? {
            if !st.success() {
                let err = child
                    .stderr
                    .take()
                    .map(|mut s| {
                        let mut buf = Vec::new();
                        let _ = s.read_to_end(&mut buf);
                        String::from_utf8_lossy(&buf).trim().to_string()
                    })
                    .unwrap_or_default();
                let _ = std::fs::remove_file(&dest);
                bail!(
                    "download failed after {}: {}",
                    humantime_s(started.elapsed()),
                    if err.is_empty() {
                        "curl error".to_string()
                    } else {
                        err
                    }
                );
            }
            break;
        }
        let sz = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        if sz != last {
            progress(sz, total);
            last = sz;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    progress(bytes, total);

    let sha256 = stream_sha256(&dest)?;
    let verified = match expected_sha256 {
        Some(want) => {
            if !sha256.eq_ignore_ascii_case(want) {
                let _ = std::fs::remove_file(&dest);
                bail!(
                    "SHA-256 mismatch for {file_name}: got {}, expected {} — the download was deleted",
                    &sha256[..12.min(sha256.len())],
                    want
                );
            }
            true
        }
        None => false,
    };
    Ok(DownloadResult {
        path: dest,
        file_name,
        sha256,
        bytes,
        verified,
    })
}

fn humantime_s(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sums_handles_all_published_formats() {
        let h1 = "1a".repeat(32);
        let h2 = "2b".repeat(32);
        let h3 = "3c".repeat(32);
        let standard = format!(
            "{h1}  ubuntu-24.04.3-desktop-amd64.iso\n{h2} *ubuntu-24.04.3-live-server-amd64.iso\n"
        );
        let v = parse_sums(&standard);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].1, h1);
        assert_eq!(v[0].0, "ubuntu-24.04.3-desktop-amd64.iso");

        let fedora = format!("SHA256 (Fedora-Workstation-44-1.1-x86_64-live.iso) = {h3}\n");
        let v = parse_sums(&fedora);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "Fedora-Workstation-44-1.1-x86_64-live.iso");
        assert_eq!(v[0].1, h3);
    }

    #[test]
    fn pick_sums_filters_ubuntu_variants() {
        let h1 = "1a".repeat(32);
        let h2 = "2b".repeat(32);
        let h3 = "3c".repeat(32);
        let h4 = "4d".repeat(32);
        let h5 = "5e".repeat(32);
        let text = format!(
            "{h1}  ubuntu-24.04.3-desktop-amd64.iso\n\
             {h2}  ubuntu-24.04.3-live-server-amd64.iso\n\
             {h3}  wubi.exe\n"
        );
        let (name, hash) = pick_sums(&text, &["desktop-amd64.iso"], &["server", "wubi"]).unwrap();
        assert_eq!(name, "ubuntu-24.04.3-desktop-amd64.iso");
        assert_eq!(hash, h1);
        assert!(pick_sums(&text, &["nonexistent"], &[]).is_none());
        // firmware variants are skipped for Debian
        let deb = format!(
            "{h4}  firmware.debian-13.1.0-amd64-netinst.iso\n\
             {h5}  debian-13.1.0-amd64-netinst.iso\n"
        );
        let (name, _) = pick_sums(&deb, &["amd64-netinst.iso"], &["firmware"]).unwrap();
        assert_eq!(name, "debian-13.1.0-amd64-netinst.iso");
    }

    #[test]
    fn pick_dir_name_picks_highest_point_release() {
        let html = "<a href=\"Fedora-Workstation-44-1.1-x86_64-live.iso\">..</a>\n\
                    <a href=\"Fedora-Workstation-44-1.2-x86_64-live.iso\">..</a>\n\
                    <a href=\"Fedora-Workstation-44-1.2-x86_64-CHECKSUM\">..</a>\n\
                    <a href=\"Fedora-Workstation-44-1.2-aarch64-live.iso\">..</a>\n";
        let name =
            pick_dir_name(html, &["Workstation", "x86_64"], &["CHECKSUM", "aarch64"]).unwrap();
        assert_eq!(name, "Fedora-Workstation-44-1.2-x86_64-live.iso");

        let pop = "\"pop-os_24.04_amd64_nvidia_5.iso\" \"pop-os_24.04_amd64_intel_6.iso\"";
        let name = pick_dir_name(pop, &["24.04", "intel"], &["nvidia"]).unwrap();
        assert_eq!(name, "pop-os_24.04_amd64_intel_6.iso");
    }

    #[test]
    fn extracts_ovdf_w_and_rticks() {
        let body = r#"var x={url:"https://ov-df.microsoft.com/?w=ABCDEF0123456789&mdt=123"};rticks=\"+9876543210\";"#;
        assert_eq!(extract_w(body).unwrap(), "ABCDEF0123456789");
        assert_eq!(extract_rticks(body).unwrap(), "9876543210");
        // unescaped form too
        assert_eq!(extract_rticks("rticks=\"+42\"").unwrap(), "42");
        assert!(extract_w("no data here").is_none());
    }

    #[test]
    fn url_file_names_strip_query_and_enforce_iso() {
        assert_eq!(
            url_file_name("https://x/y/Win11_25H2_English_x64.iso?t=abc&sig=def"),
            "Win11_25H2_English_x64.iso"
        );
        assert_eq!(
            url_file_name("https://x/y/openSUSE-Tumbleweed-DVD-x86_64-Current.iso?xyz"),
            "openSUSE-Tumbleweed-DVD-x86_64-Current.iso"
        );
        assert_eq!(url_file_name("https://x/y/plainfile"), "plainfile.iso");
    }

    #[test]
    fn unique_dest_never_overwrites() {
        let base = std::env::temp_dir().join(format!("rufus-dl-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.iso"), b"x").unwrap();
        let d = unique_dest(&base, "a.iso");
        assert!(d.ends_with("a (1).iso"));
        std::fs::write(&d, b"x").unwrap();
        let d2 = unique_dest(&base, "a.iso");
        assert!(d2.ends_with("a (2).iso"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn arch_mapping_and_dedupe() {
        assert_eq!(arch_from_type(0), "x86");
        assert_eq!(arch_from_type(1), "x64");
        assert_eq!(arch_from_type(2), "ARM64");
        let links = vec![
            WinLink {
                arch: "x64".into(),
                url: "u1".into(),
            },
            WinLink {
                arch: "ARM64".into(),
                url: "u2".into(),
            },
            WinLink {
                arch: "x64".into(),
                url: "u3".into(),
            },
        ];
        let dd = dedupe_archs(links);
        assert_eq!(dd.len(), 2);
        assert_eq!(default_arch_index(&dd), 0); // x64 preferred
        assert_eq!(default_arch_index(&[]), 0);
    }

    #[test]
    fn default_lang_matches_system_locale_prefix() {
        let langs = vec![
            WinLang {
                name: "en-us".into(),
                display: "English".into(),
                skus: vec![],
            },
            WinLang {
                name: "ar-sa".into(),
                display: "Arabic".into(),
                skus: vec![],
            },
            WinLang {
                name: "fr-fr".into(),
                display: "French".into(),
                skus: vec![],
            },
        ];
        assert_eq!(default_lang_index(&langs, "ar_EG.UTF-8"), 1);
        assert_eq!(default_lang_index(&langs, "en_US.UTF-8"), 0);
        assert_eq!(default_lang_index(&langs, "de_DE.UTF-8"), 0);
        assert_eq!(default_lang_index(&[], "ar"), 0);
    }

    #[test]
    fn uuid_v4_is_well_formed() {
        let u = new_uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(u.matches('-').count(), 4);
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        // version nibble
        let v = u.chars().nth(14).unwrap();
        assert_eq!(v, '4');
    }

    #[test]
    fn catalog_is_sane() {
        // every release has editions with ids; every distro has variants
        assert!(!WIN_RELEASES.is_empty());
        for r in WIN_RELEASES {
            assert!(!r.editions.is_empty());
            for e in r.editions {
                assert!(!e.ids.is_empty());
            }
        }
        assert!(!DISTROS.is_empty());
        let mut names = Vec::new();
        for d in DISTROS {
            assert!(!d.variants.is_empty());
            names.push(d.name);
        }
        names.dedup();
        assert_eq!(names.len(), DISTROS.len(), "distro names must be unique");
        // spot-check the resolver data of a few entries
        for d in DISTROS {
            for v in d.variants {
                match v.target {
                    IsoTarget::Fixed { url, .. } => assert!(url.starts_with("https://")),
                    IsoTarget::PickSums { sums_url, .. } => {
                        assert!(sums_url.starts_with("https://"))
                    }
                    IsoTarget::PickDir { dir_url, .. } => assert!(dir_url.starts_with("https://")),
                }
            }
        }
    }

    #[test]
    fn dir_base_and_percent_decode() {
        assert_eq!(dir_base("https://x/y/SHA256SUMS"), "https://x/y/");
        assert_eq!(dir_base("no-slash"), "");
        assert_eq!(percent_decode("a%20b%2Bc.iso"), "a b+c.iso");
        assert_eq!(percent_decode("plain.iso"), "plain.iso");
    }
}
