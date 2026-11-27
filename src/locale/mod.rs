//! Multilingual support, mirroring Rufus's `res/loc/po` locales (gettext-style):
//! the English source string *is* the lookup key, translations fall back to it
//! when a string is untranslated — the same behavior as Rufus's empty msgstr.
//!
//! Language list, display names ("Arabic (العربية)") and most strings are
//! generated from upstream Rufus .po files (see `langs.rs` and the
//! `*_RS.rs`/`ar_sa.rs`-style tables); project-specific strings carry our own
//! translations. Regenerate with the i18n tooling, never edit by hand.

mod langs;

mod ar_sa;
mod bg_bg;
mod cs_cz;
mod da_dk;
mod de_de;
mod el_gr;
mod es_es;
mod fa_ir;
mod fi_fi;
mod fr_fr;
mod he_il;
mod hr_hr;
mod hu_hu;
mod id_id;
mod it_it;
mod ja_jp;
mod ko_kr;
mod lt_lt;
mod lv_lv;
mod ms_my;
mod nb_no;
mod nl_nl;
mod pl_pl;
mod pt_br;
mod pt_pt;
mod ro_ro;
mod ru_ru;
mod sk_sk;
mod sl_si;
mod sr_rs;
mod sv_se;
mod th_th;
mod tr_tr;
mod uk_ua;
mod vi_vn;
mod zh_cn;
mod zh_tw;

/// One selectable language (index 0 of [`LANGS`] is English, the source locale).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lang {
    /// Locale code as in Rufus's po file names ("ar-SA", "zh-CN", …).
    pub code: &'static str,
    /// Rufus dropdown display string from `X-Rufus-LanguageName`,
    /// e.g. "French (Français)".
    pub name: &'static str,
    /// Right-to-left script (Arabic/Persian/Hebrew).
    pub rtl: bool,
}

/// English first (source locale), then every Rufus locale in po-file order
/// (generated in `langs.rs`).
pub const LANGS: &[Lang] = langs::LANGS;

/// Look up `key` (the English source string) in `lang`'s table; the key itself
/// is the English fallback, like gettext with an empty msgstr.
pub fn tr(lang: usize, key: &'static str) -> &'static str {
    table(lang)
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
        .unwrap_or(key)
}

/// `tr` with positional `{0}`/`{1}` substitution for parameterized strings.
/// Rufus's `%s`/`%d` msgids are converted to `{n}` in the generated table
/// values, but the msgid keys keep their original `%s` spelling — so when a
/// language is untranslated (or English, the source locale) the raw key comes
/// back and its `%s`-style placeholders must be substituted too, in order.
pub fn trf(lang: usize, key: &'static str, args: &[&str]) -> String {
    let mut s = tr(lang, key).to_string();
    for (i, a) in args.iter().enumerate() {
        let ph = format!("{{{i}}}");
        if s.contains(&ph) {
            s = s.replace(&ph, a);
        }
    }
    // gettext-style "%s"/"%d" leftovers in the fallback key: replace each
    // with the next unused argument
    if s.contains('%') {
        let mut it = args.iter();
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%' && matches!(chars.peek(), Some('s') | Some('d')) {
                chars.next();
                if let Some(a) = it.next() {
                    out.push_str(a);
                }
            } else {
                out.push(c);
            }
        }
        s = out;
    }
    s
}

/// Index of a locale code; accepts "pt-BR", "pt_br", "ptbr" spellings.
pub fn lang_index(code: &str) -> Option<usize> {
    let norm = |c: &str| c.to_lowercase().replace(['_', '-'], "");
    let want = norm(code);
    if want.is_empty() {
        return None;
    }
    // exact code ("zh-tw" -> zh-TW), then script prefix ("zh" -> zh-CN)
    LANGS.iter().position(|l| norm(l.code) == want).or_else(|| {
        LANGS
            .iter()
            .position(|l| norm(l.code).starts_with(&want) && want.len() >= 2)
    })
}

/// Detect the UI language from the environment (LC_ALL > LC_MESSAGES > LANG),
/// like Rufus's GetUserDefaultUILanguage first-run detection.
pub fn detect_system() -> usize {
    let loc = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|v| {
            std::env::var(v)
                .ok()
                .filter(|s| !s.is_empty() && s != "C" && s != "POSIX")
        })
        .unwrap_or_default();
    // "pt_BR.UTF-8" -> "pt-BR", "ar_EG" -> "ar"
    let base = loc.split('.').next().unwrap_or("");
    if let Some(idx) = lang_index(base) {
        return idx;
    }
    let prefix = base.split(['_', '-']).next().unwrap_or("");
    lang_index(prefix).unwrap_or(0)
}

/// Generated per-language tables, indexed exactly like [`LANGS`] (0 = English).
fn table(lang: usize) -> &'static [(&'static str, &'static str)] {
    match lang {
        1 => ar_sa::table(),
        2 => bg_bg::table(),
        3 => cs_cz::table(),
        4 => da_dk::table(),
        5 => de_de::table(),
        6 => el_gr::table(),
        7 => es_es::table(),
        8 => fa_ir::table(),
        9 => fi_fi::table(),
        10 => fr_fr::table(),
        11 => he_il::table(),
        12 => hr_hr::table(),
        13 => hu_hu::table(),
        14 => id_id::table(),
        15 => it_it::table(),
        16 => ja_jp::table(),
        17 => ko_kr::table(),
        18 => lt_lt::table(),
        19 => lv_lv::table(),
        20 => ms_my::table(),
        21 => nb_no::table(),
        22 => nl_nl::table(),
        23 => pl_pl::table(),
        24 => pt_br::table(),
        25 => pt_pt::table(),
        26 => ro_ro::table(),
        27 => ru_ru::table(),
        28 => sk_sk::table(),
        29 => sl_si::table(),
        30 => sr_rs::table(),
        31 => sv_se::table(),
        32 => th_th::table(),
        33 => tr_tr::table(),
        34 => uk_ua::table(),
        35 => vi_vn::table(),
        36 => zh_cn::table(),
        37 => zh_tw::table(),
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_falls_back_to_the_key() {
        assert_eq!(tr(0, "Device"), "Device");
        assert_eq!(tr(0, "No such string"), "No such string");
        assert_eq!(tr(LANGS.len() + 7, "Device"), "Device"); // out of range
    }

    #[test]
    fn language_count_matches_rufus_locales() {
        // Rufus res/loc/po ships 37 .po files + the built-in English
        assert_eq!(LANGS.len(), 38);
        assert_eq!(
            LANGS[0],
            Lang {
                code: "en",
                name: "English",
                rtl: false
            }
        );
        assert_eq!(LANGS[1].code, "ar-SA");
        assert_eq!(LANGS[37].code, "zh-TW");
    }

    #[test]
    fn rufus_sourced_translations_resolve() {
        let ar = lang_index("ar-SA").unwrap();
        assert_eq!(tr(ar, "Device"), "الجهاز");
        let fr = lang_index("fr-FR").unwrap();
        assert_eq!(tr(fr, "Device"), "Périphérique");
        assert_eq!(tr(fr, "No such string"), "No such string"); // untranslated -> English
    }

    #[test]
    fn untranslated_strings_fall_back_to_english() {
        // "Windows To Go" is empty in several Rufus po files (fr, de, …)
        let fr = lang_index("fr-FR").unwrap();
        assert_eq!(tr(fr, "Windows To Go"), "Windows To Go");
        // "MBR" is untranslated everywhere
        for i in 0..LANGS.len() {
            assert_eq!(tr(i, "FreeDOS"), "FreeDOS");
        }
    }

    #[test]
    fn placeholder_substitution() {
        let ar = lang_index("ar-SA").unwrap();
        let s = trf(ar, "WARNING: ALL DATA ON DEVICE '{0}' WILL BE DESTROYED.\nTo continue with this operation, click OK. To quit click CANCEL.", &["/dev/sdb"]);
        assert!(s.contains("'/dev/sdb'"), "got {s}");
        assert!(s.contains('\n'));
        assert_eq!(trf(0, "{0} device found", &["1"]), "1 device found");
        // args beyond the placeholders present in the string are ignored
        assert_eq!(trf(0, "{0} of {1}", &["a"]), "a of {1}");
    }

    #[test]
    fn percent_placeholders_substitute_in_the_fallback_key() {
        // English (source locale) returns the msgid key itself, which keeps
        // Rufus's original "%s" spelling — the args must still land there
        assert_eq!(trf(0, "%s (Default)", &["FAT32"]), "FAT32 (Default)");
        assert_eq!(
            trf(0, "Show %s", &["advanced drive properties"]),
            "Show advanced drive properties"
        );
        assert_eq!(
            trf(0, "%s enabled", &["Use Rufus MBR"]),
            "Use Rufus MBR enabled"
        );
        // translated values carry {0} — both spellings resolve end to end
        let ar = lang_index("ar-SA").unwrap();
        let s = trf(ar, "%s (Default)", &["FAT32"]);
        assert!(s.contains("FAT32") && !s.contains('%'), "got {s}");
        let s = trf(ar, "%s enabled", &["X"]);
        assert!(s.contains("X") && !s.contains('%'), "got {s}");
        // "%d" behaves the same way
        assert_eq!(trf(0, "%d device found", &["2"]), "2 device found");
        // a literal % with no placeholder args stays untouched
        assert_eq!(trf(0, "100% done", &[]), "100% done");
    }

    #[test]
    fn lang_index_accepts_spelling_variants() {
        assert_eq!(lang_index("pt-BR"), lang_index("pt_br"));
        assert_eq!(lang_index("zh-TW"), lang_index("zh_tw"));
        assert_eq!(lang_index("zh-CN"), lang_index("zh_cn"));
        assert_eq!(lang_index("fr-FR"), lang_index("fr"));
        assert_eq!(lang_index("ar"), lang_index("ar-SA"));
        assert!(lang_index("xx").is_none());
        assert!(lang_index("").is_none());
    }

    #[test]
    fn system_detection_stays_in_range() {
        assert!(detect_system() < LANGS.len());
    }

    #[test]
    fn all_tables_are_wellformed() {
        for i in 1..LANGS.len() {
            let t = table(i);
            assert!(!t.is_empty(), "{} has an empty table", LANGS[i].code);
            for (k, v) in t {
                assert!(
                    !k.is_empty() && !v.is_empty(),
                    "{}: empty entry",
                    LANGS[i].code
                );
            }
            // keys are unique (binary-searchable later if needed)
            let mut ks: Vec<&str> = t.iter().map(|(k, _)| *k).collect();
            ks.sort_unstable();
            ks.dedup();
            assert_eq!(ks.len(), t.len(), "{}: duplicate keys", LANGS[i].code);
        }
    }

    #[test]
    fn rtl_flag_matches_script() {
        let rtl = |code: &str| LANGS[lang_index(code).unwrap()].rtl;
        assert!(rtl("ar-SA"));
        assert!(rtl("fa-IR"));
        assert!(rtl("he-IL"));
        assert!(!rtl("ru-RU"));
        assert!(!LANGS[0].rtl);
    }
}
