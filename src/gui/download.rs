//! Rufus's "⬇ DOWNLOAD" dialog: official Windows + Linux ISO images.
//!
//! Mirrors Rufus 4.6's download dialog: the Release dropdown lists the
//! Windows releases (fetched live from Microsoft's software-download-connector
//! API, exactly like Fido) followed by the official Linux distros (resolved
//! against their published SHA-256 sums). Everything heavy runs on background
//! threads reporting through an mpsc channel polled by `pump_worker`.

use super::*;
use crate::download as net;

/// Background jobs → GUI messages (polled every event loop pass).
pub(super) enum DlMsg {
    Langs(Result<Vec<net::WinLang>, String>),
    Links(Result<Vec<net::WinLink>, String>),
    Resolved(Result<net::IsoResolved, String>),
    Progress { done: u64, total: Option<u64> },
    Done(Result<net::DownloadResult, String>),
}

/// Fetch state of one of the dialog's dynamic dropdowns.
#[derive(Clone)]
enum Fetch<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Fetch<T> {
    fn is_ready(&self) -> bool {
        matches!(self, Fetch::Ready(_))
    }
}

#[derive(Clone)]
enum DlPhase {
    Idle,
    Downloading { done: u64, total: Option<u64> },
    Failed(String),
}

/// All dialog state (private to this module; mod.rs drives it through the
/// pub(super) App methods below).
pub(super) struct DlDialog {
    open: bool,
    sel_rel: usize,
    sel_ed: usize,
    sel_lang: usize,
    sel_arch: usize,
    sel_var: usize,
    langs: Fetch<Vec<net::WinLang>>,
    links: Fetch<Vec<net::WinLink>>,
    resolved: Fetch<net::IsoResolved>,
    phase: DlPhase,
}

impl DlDialog {
    pub(super) fn new() -> Self {
        DlDialog {
            open: false,
            sel_rel: 0, // Windows 11 first, like Rufus
            sel_ed: 0,
            sel_lang: 0,
            sel_arch: 0,
            sel_var: 0,
            langs: Fetch::Idle,
            links: Fetch::Idle,
            resolved: Fetch::Idle,
            phase: DlPhase::Idle,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Current selection index of one of the dialog's dropdowns.
    pub(super) fn selection(&self, dd: DD) -> usize {
        match dd {
            DD::DlRelease => self.sel_rel,
            DD::DlEdition => self.sel_ed,
            DD::DlLanguage => self.sel_lang,
            DD::DlArch => self.sel_arch,
            DD::DlVariant => self.sel_var,
            _ => 0,
        }
    }

    fn is_windows(&self) -> bool {
        self.sel_rel < net::WIN_RELEASES.len()
    }

    fn busy(&self) -> bool {
        matches!(self.phase, DlPhase::Downloading { .. })
    }

    fn ready_to_download(&self) -> bool {
        !self.busy()
            && if self.is_windows() {
                self.links.is_ready()
            } else {
                self.resolved.is_ready()
            }
    }
}

// ---------- dropdown item providers ----------

/// Items for one of the dialog's dropdowns (Release merges Windows + Linux,
/// like Rufus 4.6's "Select Operating System" dialog).
pub(super) fn items_for_dialog(d: &DlDialog, dd: DD) -> Vec<String> {
    match dd {
        DD::DlRelease => {
            let mut v: Vec<String> = net::WIN_RELEASES
                .iter()
                .map(|r| r.name.to_string())
                .collect();
            v.extend(net::DISTROS.iter().map(|d| d.name.to_string()));
            v
        }
        DD::DlEdition => {
            if d.is_windows() {
                net::WIN_RELEASES[d.sel_rel]
                    .editions
                    .iter()
                    .map(|e| e.name.to_string())
                    .collect()
            } else {
                Vec::new()
            }
        }
        DD::DlLanguage => match &d.langs {
            Fetch::Ready(v) => v.iter().map(|l| l.display.clone()).collect(),
            _ => Vec::new(),
        },
        DD::DlArch => match &d.links {
            Fetch::Ready(v) => v.iter().map(|l| l.arch.clone()).collect(),
            _ => Vec::new(),
        },
        DD::DlVariant => {
            if !d.is_windows() {
                net::DISTROS[d.sel_rel - net::WIN_RELEASES.len()]
                    .variants
                    .iter()
                    .map(|x| x.label.to_string())
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Text shown inside a closed combo for the dynamic dropdowns.
fn combo_text(d: &DlDialog, dd: DD) -> String {
    let items = items_for_dialog(d, dd);
    let sel = d.selection(dd);
    if let Some(t) = items.get(sel.min(items.len().saturating_sub(1))) {
        return t.clone();
    }
    match dd {
        DD::DlLanguage => match &d.langs {
            Fetch::Idle => "…".into(),
            Fetch::Loading => "Fetching languages…".into(),
            Fetch::Failed(_) => "(unavailable)".into(),
            _ => "…".into(),
        },
        DD::DlArch => match &d.links {
            Fetch::Idle => "…".into(),
            Fetch::Loading => "Fetching architectures…".into(),
            Fetch::Failed(_) => "(unavailable)".into(),
            _ => "…".into(),
        },
        _ => "…".into(),
    }
}

fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------- App integration (called from gui/mod.rs) ----------

impl App {
    pub(super) fn open_download(&mut self) {
        self.ui.dl.open = true;
        self.ui.open_dd = None;
        self.ui_dirty = true;
    }

    /// Esc / Close — closing mid-download cancels it (curl is killed and the
    /// partial file removed by the engine).
    pub(super) fn dl_close(&mut self) {
        if let Some(t) = &self.ui.dl_cancel {
            t.store(true, Ordering::Relaxed);
        }
        self.dl_close_silent();
        self.ui_dirty = true;
    }

    fn dl_close_silent(&mut self) {
        self.ui.dl.open = false;
        self.ui.dl.phase = DlPhase::Idle;
        self.ui.open_dd = None;
        self.ui.dl_rx = None;
        self.ui.dl_cancel = None;
        let _ = std::fs::remove_dir_all(net::jar_dir());
    }

    /// Modal click handling while the download dialog is open.
    pub(super) fn dl_click(&mut self, mx: f32, my: f32) {
        // an open dropdown list swallows the click (item pick / outside close)
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
        for (r, w) in self.ui.widgets.iter().rev() {
            if r.contains(mx, my) {
                match w {
                    Widget::Combo(dd) => {
                        self.ui.open_dd = if self.ui.open_dd == Some(*dd) {
                            None
                        } else {
                            Some(*dd)
                        };
                        self.ui.dd_scroll = 0;
                    }
                    Widget::DlDownload => self.dl_start(),
                    Widget::DlCancel => {
                        if let Some(t) = &self.ui.dl_cancel {
                            t.store(true, Ordering::Relaxed);
                        }
                    }
                    Widget::DlClose => self.dl_close(),
                    _ => {}
                }
                break;
            }
        }
    }

    /// Dropdown selection coming from select_dd (gui/mod.rs).
    pub(super) fn dl_select(&mut self, dd: DD, idx: usize) {
        match dd {
            DD::DlRelease => self.dl_select_release(idx),
            DD::DlEdition => self.dl_select_edition(idx),
            DD::DlLanguage => self.dl_select_language(idx),
            DD::DlArch => {
                self.ui.dl.sel_arch = idx;
                self.ui.open_dd = None;
                self.ui_dirty = true;
            }
            DD::DlVariant => self.dl_select_variant(idx),
            _ => {}
        }
    }

    fn dl_select_release(&mut self, idx: usize) {
        let n = (net::WIN_RELEASES.len() + net::DISTROS.len()).saturating_sub(1);
        {
            let d = &mut self.ui.dl;
            d.sel_rel = idx.min(n);
            d.sel_ed = 0;
            d.sel_lang = 0;
            d.sel_arch = 0;
            d.sel_var = 0;
            d.langs = Fetch::Idle;
            d.links = Fetch::Idle;
            d.resolved = Fetch::Idle;
            d.phase = DlPhase::Idle;
        }
        self.ui.open_dd = None;
        self.ui_dirty = true;
        // selecting a Linux distro resolves its default image right away
        if self.ui.dl.sel_rel >= net::WIN_RELEASES.len() {
            self.dl_select_variant(0);
        }
    }

    fn dl_select_edition(&mut self, idx: usize) {
        let rel = self.ui.dl.sel_rel;
        let Some(ed) = net::WIN_RELEASES.get(rel).and_then(|r| r.editions.get(idx)) else {
            return;
        };
        {
            let d = &mut self.ui.dl;
            d.sel_ed = idx;
            d.sel_lang = 0;
            d.sel_arch = 0;
            d.links = Fetch::Idle;
            d.phase = DlPhase::Idle;
            d.langs = Fetch::Loading;
        }
        self.ui.open_dd = None;
        self.ui_dirty = true;
        // Fido parity: the language list needs a fresh whitelisted session per
        // productEditionId and "may require a few attempts"
        let ids: Vec<u32> = ed.ids.to_vec();
        let tmp = net::jar_dir();
        let (tx, rx) = channel();
        self.ui.dl_rx = Some(rx);
        self.push_log(format!("fetching language list for {} …", ed.name));
        std::thread::spawn(move || {
            let res = net::win_fetch_langs(&ids, &tmp).map_err(|e| format!("{e:#}"));
            let _ = tx.send(DlMsg::Langs(res));
        });
    }

    fn dl_select_language(&mut self, idx: usize) {
        let langs = match &self.ui.dl.langs {
            Fetch::Ready(v) => v.clone(),
            _ => return,
        };
        let Some(lang) = langs.get(idx.min(langs.len().saturating_sub(1))) else {
            return;
        };
        {
            let d = &mut self.ui.dl;
            d.sel_lang = idx.min(langs.len().saturating_sub(1));
            d.sel_arch = 0;
            d.phase = DlPhase::Idle;
            d.links = Fetch::Loading;
        }
        self.ui.open_dd = None;
        self.ui_dirty = true;
        let lang = lang.clone();
        let (tx, rx) = channel();
        self.ui.dl_rx = Some(rx);
        self.push_log(format!("fetching architectures for {} …", lang.display));
        std::thread::spawn(move || {
            // Fido hardcodes the windows11 referer for every release
            let res = net::win_fetch_links(&lang, "windows11").map_err(|e| format!("{e:#}"));
            let _ = tx.send(DlMsg::Links(res));
        });
    }

    fn dl_select_variant(&mut self, idx: usize) {
        let rel = self.ui.dl.sel_rel;
        let Some(di) = rel.checked_sub(net::WIN_RELEASES.len()) else {
            return;
        };
        let Some(variant) = net::DISTROS.get(di).and_then(|d| d.variants.get(idx)) else {
            return;
        };
        {
            let d = &mut self.ui.dl;
            d.sel_var = idx;
            d.phase = DlPhase::Idle;
            d.resolved = Fetch::Loading;
        }
        self.ui.open_dd = None;
        self.ui_dirty = true;
        let target = variant.target;
        let (tx, rx) = channel();
        self.ui.dl_rx = Some(rx);
        self.push_log(format!(
            "resolving {} — {} …",
            net::DISTROS[di].name,
            variant.label
        ));
        std::thread::spawn(move || {
            let res = net::resolve_target(&target).map_err(|e| format!("{e:#}"));
            let _ = tx.send(DlMsg::Resolved(res));
        });
    }

    /// DOWNLOAD pressed: spawn the curl download on a background thread with
    /// its own cancel token, exactly like the write pipeline does.
    pub(super) fn dl_start(&mut self) {
        if self.ui.dl.busy() {
            return;
        }
        let (url, expected, hint) = if self.ui.dl.is_windows() {
            let d = &self.ui.dl;
            let Fetch::Ready(links) = &d.links else {
                return;
            };
            let i = d.sel_arch.min(links.len().saturating_sub(1));
            let l = &links[i];
            let hint = url_hint(&l.url);
            (l.url.clone(), None, hint)
        } else {
            let d = &self.ui.dl;
            let Fetch::Ready(r) = &d.resolved else { return };
            (r.url.clone(), r.sha256.clone(), r.file_name.clone())
        };
        let dir = net::downloads_dir();
        let token = Arc::new(AtomicBool::new(false));
        self.ui.dl_cancel = Some(token.clone());
        let (tx, rx) = channel();
        self.ui.dl_rx = Some(rx);
        self.ui.dl.phase = DlPhase::Downloading {
            done: 0,
            total: None,
        };
        self.ui.open_dd = None;
        self.ui_dirty = true;
        self.push_log(format!("downloading {hint} …"));
        self.push_log(format!("saving to {}", dir.display()));
        std::thread::spawn(move || {
            let res = net::download_iso(
                &url,
                &dir,
                expected.as_deref(),
                &token,
                &mut |done, total| {
                    let _ = tx.send(DlMsg::Progress { done, total });
                },
            );
            let _ = tx.send(DlMsg::Done(res.map_err(|e| format!("{e:#}"))));
        });
    }

    /// Drain the download channel (called from pump_worker).
    pub(super) fn dl_pump(&mut self) {
        let mut msgs = Vec::new();
        if let Some(rx) = &self.ui.dl_rx {
            while let Ok(m) = rx.try_recv() {
                msgs.push(m);
            }
        }
        if msgs.is_empty() {
            return;
        }
        self.ui_dirty = true;
        for m in msgs {
            self.dl_apply(m);
        }
    }

    fn dl_apply(&mut self, m: DlMsg) {
        match m {
            DlMsg::Langs(res) => {
                self.ui.dl.langs = match res {
                    Ok(v) => {
                        self.ui.dl.sel_lang =
                            net::default_lang_index(&v, &std::env::var("LANG").unwrap_or_default());
                        self.push_log(format!("{} languages available", v.len()));
                        Fetch::Ready(v)
                    }
                    Err(e) => {
                        self.push_log(format!("ERROR: {e}"));
                        Fetch::Failed(e)
                    }
                };
            }
            DlMsg::Links(res) => {
                self.ui.dl.links = match res {
                    Ok(v) => {
                        self.ui.dl.sel_arch = net::default_arch_index(&v);
                        Fetch::Ready(v)
                    }
                    Err(e) => {
                        self.push_log(format!("ERROR: {e}"));
                        Fetch::Failed(e)
                    }
                };
            }
            DlMsg::Resolved(res) => {
                self.ui.dl.resolved = match res {
                    Ok(v) => {
                        self.push_log(format!(
                            "official image: {}{}",
                            v.file_name,
                            if v.sha256.is_some() {
                                " (SHA-256 published)"
                            } else {
                                " (no published SHA-256 — verification skipped)"
                            }
                        ));
                        Fetch::Ready(v)
                    }
                    Err(e) => {
                        self.push_log(format!("ERROR: {e}"));
                        Fetch::Failed(e)
                    }
                };
            }
            DlMsg::Progress { done, total } => {
                if let DlPhase::Downloading { done: d, total: t } = &mut self.ui.dl.phase {
                    *d = done;
                    *t = total;
                }
            }
            DlMsg::Done(res) => {
                self.ui.dl_cancel = None;
                match res {
                    Ok(r) => {
                        let path = r.path.to_string_lossy().to_string();
                        self.push_log(format!(
                            "downloaded {} ({}, {}{})",
                            r.file_name,
                            fmt_size(r.bytes),
                            if r.verified {
                                "SHA-256 verified"
                            } else {
                                "unverified source"
                            },
                            if r.verified {
                                format!(" — {}", &r.sha256[..16.min(r.sha256.len())])
                            } else {
                                String::new()
                            }
                        ));
                        // load it like any hand-picked image — Rufus closes the
                        // dialog and selects the freshly downloaded ISO
                        self.ui.recent.retain(|x| x != &path);
                        self.ui.recent.insert(0, path.clone());
                        self.ui.recent.truncate(5);
                        self.ui.image_path = path;
                        self.mark_image_dirty();
                        self.ui.status = "Download complete.".into();
                        self.ui.status_is_error = false;
                        self.ui.status_until = Some(Instant::now() + Duration::from_secs(3));
                        self.dl_close_silent();
                    }
                    Err(e) => {
                        if e.contains("cancelled") {
                            self.push_log("download cancelled");
                        } else {
                            self.push_log(format!("ERROR: {e}"));
                        }
                        self.ui.dl.phase = DlPhase::Failed(e);
                    }
                }
            }
        }
        self.ui_dirty = true;
    }
}

fn url_hint(url: &str) -> String {
    let n = net::url_file_name(url);
    trunc(&n, 64)
}

// ---------- rendering (called from App::render, before the dropdown overlay) ----------

/// Draws the centered "Download ISO image" dialog and returns the combo rects
/// so the generic dropdown-overlay code can render their open lists on top.
pub(super) fn render_dl(
    ui: &mut Ui,
    p: &mut Painter,
    mouse: (f32, f32),
    width: f32,
    height: f32,
) -> Vec<(DD, Rect)> {
    let mut combo_rects = Vec::new();
    if !ui.dl.open {
        return combo_rects;
    }
    let lang = ui.locale;
    let pw = 500.0;
    let ph = 384.0;
    let panel = Rect {
        x: (width - pw) / 2.0,
        y: (height - ph) / 2.0,
        w: pw,
        h: ph,
    };
    p.fill(panel, FIELD);
    p.frame(panel, ACCENT);
    // header band (same style as the Windows User Experience dialog)
    let header = Rect {
        x: panel.x,
        y: panel.y,
        w: panel.w,
        h: 34.0,
    };
    p.fill(header, 0xDCEBF5);
    p.fill(
        Rect {
            x: panel.x,
            y: panel.y + 34.0,
            w: panel.w,
            h: 1.0,
        },
        BORDER,
    );
    p.text(
        panel.x + 14.0,
        panel.y + 9.0,
        crate::locale::tr(lang, "Download ISO Image"),
        14.0,
        TEXT,
        true,
    );

    let combo_x = panel.x + 14.0;
    let combo_w = pw - 28.0;
    let d = &ui.dl;
    let windows = d.is_windows();
    let clickable = !d.busy();
    let mut y = panel.y + 48.0;

    // Release (Windows releases + Linux distros, like Rufus 4.6)
    p.text(
        combo_x,
        y,
        crate::locale::tr(lang, "Release"),
        12.0,
        TEXT,
        false,
    );
    y += 15.0;
    let items = items_for_dialog(d, DD::DlRelease);
    let rel_text = items
        .get(d.selection(DD::DlRelease))
        .cloned()
        .unwrap_or_default();
    let r = Rect {
        x: combo_x,
        y,
        w: combo_w,
        h: COMBO_H,
    };
    App::draw_combo(
        p,
        r,
        &rel_text,
        clickable,
        ui.open_dd == Some(DD::DlRelease),
        mouse,
    );
    combo_rects.push((DD::DlRelease, r));
    ui.widgets.push((r, Widget::Combo(DD::DlRelease)));
    y += COMBO_H + 10.0;

    if windows {
        // Edition
        p.text(
            combo_x,
            y,
            crate::locale::tr(lang, "Edition"),
            12.0,
            TEXT,
            false,
        );
        y += 15.0;
        let ed_text = combo_text(d, DD::DlEdition);
        let r = Rect {
            x: combo_x,
            y,
            w: combo_w,
            h: COMBO_H,
        };
        App::draw_combo(
            p,
            r,
            &ed_text,
            clickable,
            ui.open_dd == Some(DD::DlEdition),
            mouse,
        );
        combo_rects.push((DD::DlEdition, r));
        ui.widgets.push((r, Widget::Combo(DD::DlEdition)));
        y += COMBO_H + 10.0;

        // Language (fetched live from Microsoft, like Fido)
        p.text(
            combo_x,
            y,
            crate::locale::tr(lang, "Language"),
            12.0,
            TEXT,
            false,
        );
        y += 15.0;
        let lang_text = combo_text(d, DD::DlLanguage);
        let r = Rect {
            x: combo_x,
            y,
            w: combo_w,
            h: COMBO_H,
        };
        let lang_enabled = clickable && d.langs.is_ready();
        App::draw_combo(
            p,
            r,
            &lang_text,
            lang_enabled,
            ui.open_dd == Some(DD::DlLanguage),
            mouse,
        );
        combo_rects.push((DD::DlLanguage, r));
        ui.widgets.push((r, Widget::Combo(DD::DlLanguage)));
        y += COMBO_H + 10.0;

        // Architecture (x64 preselected, like Fido)
        p.text(
            combo_x,
            y,
            crate::locale::tr(lang, "Architecture"),
            12.0,
            TEXT,
            false,
        );
        y += 15.0;
        let arch_text = combo_text(d, DD::DlArch);
        let r = Rect {
            x: combo_x,
            y,
            w: combo_w,
            h: COMBO_H,
        };
        let arch_enabled = clickable && d.links.is_ready();
        App::draw_combo(
            p,
            r,
            &arch_text,
            arch_enabled,
            ui.open_dd == Some(DD::DlArch),
            mouse,
        );
        combo_rects.push((DD::DlArch, r));
        ui.widgets.push((r, Widget::Combo(DD::DlArch)));
        y += COMBO_H + 6.0;

        p.text(
            combo_x,
            y,
            "The ISO downloads straight from Microsoft's official servers.",
            11.0,
            0x808080,
            false,
        );
    } else {
        // Image variant (per-distro flavors/editions)
        p.text(
            combo_x,
            y,
            crate::locale::tr(lang, "Image"),
            12.0,
            TEXT,
            false,
        );
        y += 15.0;
        let var_text = combo_text(d, DD::DlVariant);
        let r = Rect {
            x: combo_x,
            y,
            w: combo_w,
            h: COMBO_H,
        };
        App::draw_combo(
            p,
            r,
            &var_text,
            clickable,
            ui.open_dd == Some(DD::DlVariant),
            mouse,
        );
        combo_rects.push((DD::DlVariant, r));
        ui.widgets.push((r, Widget::Combo(DD::DlVariant)));
        y += COMBO_H + 10.0;

        match &d.resolved {
            Fetch::Idle => p.text(
                combo_x,
                y,
                crate::locale::tr(lang, "Select a distro and image."),
                11.0,
                0x808080,
                false,
            ),
            Fetch::Loading => p.text(
                combo_x,
                y,
                crate::locale::tr(lang, "Looking up the official image…"),
                11.0,
                0x808080,
                false,
            ),
            Fetch::Failed(e) => p.text(combo_x, y, &trunc(e, 68), 11.0, PROGRESS_ERR, false),
            Fetch::Ready(v) => p.text(
                combo_x,
                y,
                &trunc(
                    &format!(
                        "{} — official SHA-256 {}",
                        v.file_name,
                        if v.sha256.is_some() {
                            "published ✓"
                        } else {
                            "not published"
                        }
                    ),
                    68,
                ),
                11.0,
                0x404040,
                false,
            ),
        }
    }

    // status / progress zone
    let zone_y = panel.y + ph - 106.0;
    match &d.phase {
        DlPhase::Idle => {}
        DlPhase::Downloading { done, total } => {
            let track = Rect {
                x: combo_x,
                y: zone_y,
                w: combo_w,
                h: 14.0,
            };
            p.fill(track, 0xE8E8E8);
            p.frame(track, BORDER);
            let (frac, txt) = match total {
                Some(t) if *t > 0 => (
                    (*done as f64 / *t as f64) as f32,
                    format!(
                        "{} of {} ({:.0}%)",
                        fmt_size(*done),
                        fmt_size(*t),
                        100.0 * *done as f64 / *t as f64
                    ),
                ),
                _ => (0.0, fmt_size(*done)),
            };
            if frac > 0.0 {
                p.fill(
                    Rect {
                        x: track.x,
                        y: track.y,
                        w: (track.w * frac.min(1.0)).max(1.0),
                        h: track.h,
                    },
                    ACCENT,
                );
            }
            p.text(combo_x, track.y + 20.0, &txt, 11.0, 0x404040, false);
        }
        DlPhase::Failed(e) => {
            let msg = crate::locale::trf(lang, "Download failed: {0}", &[&e.to_string()]);
            p.text(combo_x, zone_y, &trunc(&msg, 76), 11.0, PROGRESS_ERR, false);
        }
    }

    // buttons: [⬇ DOWNLOAD / STOP] [Close]
    let by = panel.y + ph - 42.0;
    let close = Rect {
        x: panel.x + pw - 100.0,
        y: by,
        w: 86.0,
        h: 30.0,
    };
    App::draw_button(
        p,
        close,
        crate::locale::tr(lang, "Close"),
        true,
        mouse,
        false,
    );
    ui.widgets.push((close, Widget::DlClose));
    let dl_rect = Rect {
        x: panel.x + pw - 100.0 - 178.0 - 10.0,
        y: by,
        w: 178.0,
        h: 30.0,
    };
    if d.busy() {
        App::draw_button(
            p,
            dl_rect,
            crate::locale::tr(lang, "STOP"),
            true,
            mouse,
            false,
        );
        ui.widgets.push((dl_rect, Widget::DlCancel));
    } else {
        let dl_label = format!("\u{2193} {}", crate::locale::tr(lang, "Download"));
        App::draw_button(p, dl_rect, &dl_label, d.ready_to_download(), mouse, true);
        ui.widgets.push((dl_rect, Widget::DlDownload));
    }
    combo_rects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_list_merges_windows_and_linux() {
        let v = items_for_dialog(&DlDialog::new(), DD::DlRelease);
        assert_eq!(v.len(), net::WIN_RELEASES.len() + net::DISTROS.len());
        assert!(v.first().unwrap().contains("Windows 11"));
        assert!(v.iter().any(|s| s == "Ubuntu"));
        assert!(v.iter().any(|s| s == "Arch Linux"));
    }

    #[test]
    fn edition_and_variant_items_follow_release() {
        let d = DlDialog::new();
        assert!(!items_for_dialog(&d, DD::DlEdition).is_empty());
        assert!(items_for_dialog(&d, DD::DlVariant).is_empty());
        // a Linux release: variants instead of editions
        let mut d = DlDialog::new();
        d.sel_rel = net::WIN_RELEASES.len(); // first distro
        assert!(items_for_dialog(&d, DD::DlEdition).is_empty());
        assert!(!items_for_dialog(&d, DD::DlVariant).is_empty());
    }

    #[test]
    fn language_and_arch_items_need_ready_state() {
        let mut d = DlDialog::new();
        d.langs = Fetch::Loading;
        assert!(items_for_dialog(&d, DD::DlLanguage).is_empty());
        d.langs = Fetch::Ready(vec![net::WinLang {
            name: "en-us".into(),
            display: "English (United States)".into(),
            skus: vec![],
        }]);
        assert_eq!(
            items_for_dialog(&d, DD::DlLanguage),
            vec!["English (United States)".to_string()]
        );
        d.links = Fetch::Ready(vec![net::WinLink {
            arch: "x64".into(),
            url: "u".into(),
        }]);
        assert_eq!(items_for_dialog(&d, DD::DlArch), vec!["x64".to_string()]);
    }

    #[test]
    fn dialog_defaults_match_rufus() {
        let d = DlDialog::new();
        assert!(!d.is_open()); // closed until the user clicks ⬇ DOWNLOAD
        assert_eq!(d.selection(DD::DlRelease), 0);
        assert_eq!(d.selection(DD::DlArch), 0);
        assert!(!d.ready_to_download()); // nothing fetched yet
    }

    #[test]
    fn combo_text_states() {
        let mut d = DlDialog::new();
        assert_eq!(combo_text(&d, DD::DlLanguage), "…");
        d.langs = Fetch::Loading;
        assert_eq!(combo_text(&d, DD::DlLanguage), "Fetching languages…");
        d.langs = Fetch::Failed("boom".into());
        assert_eq!(combo_text(&d, DD::DlLanguage), "(unavailable)");
    }

    #[test]
    fn url_hint_strips_and_truncates() {
        assert_eq!(
            url_hint("https://x/y/Win11_25H2_English_x64.iso?t=1"),
            "Win11_25H2_English_x64.iso"
        );
        let long = url_hint(&format!("https://x/y/{}.iso", "a".repeat(100)));
        assert!(long.chars().count() <= 64);
    }
}
