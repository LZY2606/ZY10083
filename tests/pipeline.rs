use uiw::unicode::*;

fn cfg_default() -> RuleConfig {
    RuleConfig {
        case_fold: true,
        normalization: Normalization::Nfc,
        strip_default_ignorable: true,
        restrict_script: None,
    }
}

#[test]
fn case_fold_and_nfc_collision() {
    let cfg = cfg_default();
    // "K" + combining ring vs angstrom, plus case difference.
    // Kelvin sign folds to "k"; use Å (U+00C5 vs A + combining ring U+030A).
    let a = analyze("\u{00c5}NGSTR\u{00d6}M".as_bytes(), &cfg);
    let b = analyze("a\u{030a}ngstr\u{00f6}m".as_bytes(), &cfg);
    assert_eq!(a.canonical, b.canonical);
    assert!(a.canonical.starts_with("\u{00e5}"));
    assert_eq!(a.stages.len(), 5);
}

#[test]
fn nfkc_differs_from_nfc() {
    let fold = cfg_default();
    let mut nfkc = cfg_default();
    nfkc.normalization = Normalization::Nfkc;
    let superscript = "\u{00b2}"; // superscript two: NFKC -> "2"
    let a = analyze(superscript.as_bytes(), &fold);
    let b = analyze(superscript.as_bytes(), &nfkc);
    assert_ne!(a.canonical, b.canonical);
    assert_eq!(b.canonical, "2");
}

#[test]
fn invalid_utf8_is_reported_and_excluded() {
    let cfg = cfg_default();
    let bytes = b"ab\xff\x80c";
    let t = analyze(bytes, &cfg);
    assert!(t.issues.iter().any(|i| matches!(
        i,
        Issue::InvalidUtf8 {
            byte: 0xff | 0x80,
            ..
        }
    )));
    assert_eq!(t.canonical, "abc");
    // raw stage preserves every byte
    assert_eq!(t.stages[0].units.len(), 5);
}

#[test]
fn lone_surrogates_from_wtf8_are_reported() {
    let cfg = cfg_default();
    // ED A0 80 = U+D800 encoded in WTF-8 style.
    let t = analyze(&[0x41, 0xED, 0xA0, 0x80, 0x42], &cfg);
    assert!(t
        .issues
        .iter()
        .any(|i| matches!(i, Issue::LoneSurrogate { cp: 0xD800, .. })));
    assert_eq!(t.canonical, "ab");
}

#[test]
fn noncharacters_are_flagged_but_kept() {
    let cfg = cfg_default();
    let t = analyze("x\u{ffff}".as_bytes(), &cfg);
    assert!(t
        .issues
        .iter()
        .any(|i| matches!(i, Issue::Noncharacter { cp: 0xFFFF, .. })));
    assert!(t.canonical.contains('\u{ffff}'));
}

#[test]
fn mixed_script_and_restriction() {
    let cfg = cfg_default();
    let t = analyze("abc\u{0430}".as_bytes(), &cfg); // Latn + Cyrl 'а'
    assert!(t.mixed_script);
    let mut restricted = cfg_default();
    restricted.restrict_script = Some("Latn".to_string());
    let t2 = analyze("abc".as_bytes(), &restricted);
    assert!(!t2.restriction_violated);
    let t3 = analyze("a\u{0430}".as_bytes(), &restricted);
    assert!(t3.restriction_violated);
}

#[test]
fn default_ignorables_are_stripped() {
    let cfg = cfg_default();
    let t = analyze("a\u{200b}\u{fe0f}b".as_bytes(), &cfg);
    assert_eq!(t.canonical, "ab");
    let mut keep = cfg_default();
    keep.strip_default_ignorable = false;
    let t2 = analyze("a\u{200b}b".as_bytes(), &keep);
    assert_eq!(t2.canonical, "a\u{200b}b");
}

#[test]
fn stage_provenance_chains_to_raw() {
    let cfg = cfg_default();
    let t = analyze("A".as_bytes(), &cfg);
    // fold maps A->a: provenance of stage case_fold points at stage index 0
    let fold = t.stages.iter().find(|s| s.name == "case_fold").unwrap();
    assert_eq!(fold.units.len(), 1);
    assert_eq!(fold.provenance[0], 0);
    assert!(fold.notes[0].contains("case fold"));
}

#[test]
fn identical_originals_and_visual_lookalikes() {
    let cfg = cfg_default();
    // Cyrillic 'а' vs Latin 'a' must NOT collapse.
    let latin = analyze("a".as_bytes(), &cfg);
    let cyrillic = analyze("\u{0430}".as_bytes(), &cfg);
    assert_ne!(latin.canonical, cyrillic.canonical);
    assert!(cyrillic.mixed_script || latin.scripts != cyrillic.scripts);
}
