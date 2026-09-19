//! Server-side identifier normalization pipeline.
//!
//! Stages (each is recorded separately for the UI and the export):
//!   0 raw        - permissive decode of the original bytes into code point units
//!   1 scalars    - drop invalid UTF-8 bytes and mark lone surrogates / noncharacters
//!   2 fold       - Unicode default case folding (full)
//!   3 strip_di   - remove Default_Ignorable_Code_Point units (optional)
//!   4 normalize  - NFC or NFKC
//!   5 scripts    - script/mixed-script and restriction check (report only)
//!
//! The original bytes are never modified; every stage keeps provenance indexes
//! back to stage 0, so the UI can expand exact code point changes.

use caseless::Caseless;
use serde::{Deserialize, Serialize};
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;
use unicode_script::{Script, UnicodeScript};

/// Raw decoding unit. `Invalid` bytes are kept in the trace forever but never
/// participate in later stages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    Scalar(char),
    /// A code point decoded from WTF-8-style surrogate bytes (0xD800..=0xDFFF).
    Surrogate(u32),
    /// A single byte that could not be decoded; the byte value is its identity.
    InvalidByte(u8),
}

impl Unit {
    fn as_char(self) -> Option<char> {
        match self {
            Unit::Scalar(c) => Some(c),
            _ => None,
        }
    }

    fn scalar_u32(self) -> Option<u32> {
        match self {
            Unit::Scalar(c) => Some(c as u32),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Normalization {
    Nfc,
    Nfkc,
}

impl Normalization {
    pub fn as_str(self) -> &'static str {
        match self {
            Normalization::Nfc => "NFC",
            Normalization::Nfkc => "NFKC",
        }
    }
}

/// A fixed, versioned set of transformation rules. Rulesets are immutable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleConfig {
    pub case_fold: bool,
    pub normalization: Normalization,
    pub strip_default_ignorable: bool,
    /// ISO-15924 4-letter script code, e.g. "Latn"; `None` disables restriction.
    pub restrict_script: Option<String>,
}

impl RuleConfig {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(code) = &self.restrict_script {
            Script::from_short_name(code)
                .ok_or_else(|| format!("unknown ISO-15924 script code: {code}"))?;
        }
        Ok(())
    }
}

/// Versions of the Unicode data tables actually linked into this binary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableVersions {
    pub unicode_normalization: String,
    pub unicode_casefold: String,
    pub unicode_general_category: String,
    pub unicode_script: String,
}

pub fn table_versions() -> TableVersions {
    let v64 = |(a, b, c): (u64, u64, u64)| format!("{a}.{b}.{c}");
    let v8 = |(a, b, c): (u8, u8, u8)| format!("{a}.{b}.{c}");
    TableVersions {
        unicode_normalization: v8(unicode_normalization::UNICODE_VERSION),
        unicode_casefold: v64(caseless::UNICODE_VERSION),
        unicode_general_category: v64(unicode_general_category::UNICODE_VERSION),
        unicode_script: v64(unicode_script::UNICODE_VERSION),
    }
}

// ---------------------------------------------------------------------------
// Stage 0: permissive decoding
// ---------------------------------------------------------------------------

/// Decode permissively. Valid UTF-8 sequences (including valid scalar values)
/// become `Scalar`; well-formed 3-byte sequences decoding to the surrogate range
/// become `Surrogate` (lone surrogate reporting); every malformed or truncated
/// byte becomes its own `InvalidByte`.
pub fn permissive_decode(bytes: &[u8]) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        match b0 {
            0x00..=0x7F => {
                out.push(Unit::Scalar(b0 as char));
                i += 1;
            }
            0xC2..=0xDF => {
                if i + 1 < bytes.len() && (bytes[i + 1] & 0xC0) == 0x80 {
                    let cp = (((b0 as u32) & 0x1F) << 6) | ((bytes[i + 1] as u32) & 0x3F);
                    out.push(Unit::Scalar(char::from_u32(cp).unwrap()));
                    i += 2;
                } else {
                    out.push(Unit::InvalidByte(b0));
                    i += 1;
                }
            }
            0xE0..=0xEF => {
                if i + 2 < bytes.len()
                    && (bytes[i + 1] & 0xC0) == 0x80
                    && (bytes[i + 2] & 0xC0) == 0x80
                {
                    let cp = (((b0 as u32) & 0x1F) << 12)
                        | (((bytes[i + 1] as u32) & 0x3F) << 6)
                        | ((bytes[i + 2] as u32) & 0x3F);
                    match cp {
                        0xD800..=0xDFFF => out.push(Unit::Surrogate(cp)),
                        _ => out.push(Unit::Scalar(char::from_u32(cp).unwrap())),
                    }
                    i += 3;
                } else {
                    out.push(Unit::InvalidByte(b0));
                    i += 1;
                }
            }
            0xF0..=0xF4
                if i + 3 < bytes.len()
                    && (bytes[i + 1] & 0xC0) == 0x80
                    && (bytes[i + 2] & 0xC0) == 0x80
                    && (bytes[i + 3] & 0xC0) == 0x80 =>
            {
                let cp = (((b0 as u32) & 0x07) << 18)
                    | (((bytes[i + 1] as u32) & 0x3F) << 12)
                    | (((bytes[i + 2] as u32) & 0x3F) << 6)
                    | ((bytes[i + 3] as u32) & 0x3F);
                match char::from_u32(cp) {
                    Some(c) => out.push(Unit::Scalar(c)),
                    None => out.push(Unit::InvalidByte(b0)),
                }
                i += 4;
            }
            _ => {
                out.push(Unit::InvalidByte(b0));
                i += 1;
            }
        }
    }
    out
}

/// UAX #44 noncharacter: U+nFFFE/U+nFFFF and U+FDD0..=U+FDEF.
pub fn is_noncharacter(cp: u32) -> bool {
    (0xFDD0..=0xFDEF).contains(&cp) || ((cp & 0xFFFF) == 0xFFFE || (cp & 0xFFFF) == 0xFFFF)
}

// ---------------------------------------------------------------------------
// Default ignorable detection
// ---------------------------------------------------------------------------

const DI_RANGES: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180D),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0xE0000, 0xE0000),
    (0xE0001, 0xE0001),
    (0xE0002, 0xE001F),
    (0xE0020, 0xE007F),
    (0xE0080, 0xE00FF),
    (0xE0100, 0xE01EF),
    (0xE01F0, 0xE0FFF),
];

/// Format (Cf) characters that are explicitly *not* default ignorable.
const CF_NOT_DI: &[u32] = &[
    0x00B7, 0x0375, 0x055A, 0x055B, 0x055C, 0x058A, 0x0591, 0x05A3, 0x05A4, 0x05B9, 0x05BA, 0x05BB,
    0x05BC, 0x05BD, 0x05BF, 0x05C1, 0x05C2, 0x0600, 0x0601, 0x0602, 0x0603, 0x0604, 0x0605, 0x06DD,
    0x070F, 0x08E2, 0x180E, 0x2015, 0x2022, 0x2032, 0x2033, 0x2035, 0x203B, 0x203C, 0x203D, 0x2042,
    0x2044, 0x2057, 0x205A, 0x205B, 0x205C, 0x205D, 0x205E, 0x2060, 0x20A0, 0x20A1, 0x20A2, 0x20A3,
    0x20A4, 0x20A6, 0x20A7, 0x20A8, 0x20A9, 0x20AA, 0x20AB, 0x20AC, 0x20AD, 0x20AE, 0x20AF, 0x20B0,
    0x20B1, 0x20B2, 0x20B3, 0x20B4, 0x20B5, 0x20B6, 0x20B7, 0x20B8, 0x20B9, 0x20BA,
];

/// Approximation of the Unicode `Default_Ignorable_Code_Point` property:
/// Cf characters (minus an explicit exception list) union the hardcoded
/// property ranges. Documented as an approximation in README.
pub fn is_default_ignorable(c: char) -> bool {
    let cp = c as u32;
    if DI_RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&cp)) {
        return true;
    }
    matches!(get_general_category(c), GeneralCategory::Format) && !CF_NOT_DI.contains(&cp)
}

// ---------------------------------------------------------------------------
// Tracing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Issue {
    InvalidUtf8 {
        index: usize,
        byte: u8,
    },
    /// WTF-8-style code point in the surrogate range.
    LoneSurrogate {
        index: usize,
        cp: u32,
    },
    Noncharacter {
        index: usize,
        cp: u32,
    },
    MixedScript {
        scripts: Vec<String>,
    },
    ScriptRestriction {
        found: String,
        allowed: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonUnit {
    /// Valid Unicode scalar value.
    Scalar(u32),
    Surrogate(u32),
    InvalidByte(u8),
}

impl JsonUnit {
    pub fn of(u: Unit) -> Self {
        match u {
            Unit::Scalar(c) => JsonUnit::Scalar(c as u32),
            Unit::Surrogate(cp) => JsonUnit::Surrogate(cp),
            Unit::InvalidByte(b) => JsonUnit::InvalidByte(b),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageView {
    pub name: String,
    /// Units at this stage.
    pub units: Vec<JsonUnit>,
    /// `provenance[i]` indexes the parent unit in the previous stage.
    pub provenance: Vec<usize>,
    /// Human-readable note per unit ("" when absent).
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub stages: Vec<StageView>,
    pub issues: Vec<Issue>,
    /// Final canonical value as UTF-8 (surrogates/invalid bytes never included).
    pub canonical: String,
    /// Canonical code points, for exact expansion in the UI.
    pub canonical_cps: Vec<u32>,
    pub scripts: Vec<String>,
    pub mixed_script: bool,
    pub restriction_violated: bool,
}

fn view(units: &[Unit], prov: Vec<usize>, notes: Vec<String>, name: &str) -> StageView {
    StageView {
        name: name.to_string(),
        units: units.iter().map(|u| JsonUnit::of(*u)).collect(),
        provenance: prov,
        notes,
    }
}

fn identity(units: &[Unit], name: &str) -> (StageView, Vec<Unit>, Vec<usize>, Vec<String>) {
    let prov: Vec<usize> = (0..units.len()).collect();
    let notes = vec![String::new(); units.len()];
    let st = StageView {
        name: name.to_string(),
        units: units.iter().map(|u| JsonUnit::of(*u)).collect(),
        provenance: prov.clone(),
        notes: notes.clone(),
    };
    (st, units.to_vec(), prov, notes)
}

/// Longest-common-subsequence based alignment between two scalar sequences.
/// Returns, for each position in `next`, an index into `prev`.
fn lcs_align(prev: &[char], next: &[char]) -> Vec<usize> {
    let n = prev.len();
    let m = next.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if prev[i] == next[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(m);
    let (mut i, mut j) = (0, 0);
    while j < m {
        if i < n && prev[i] == next[j] {
            out.push(i);
            i += 1;
            j += 1;
        } else if i < n && dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            // New/changed unit: attach to the nearest earlier prev position.
            out.push(i.saturating_sub(1));
            j += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

fn script_name(s: Script) -> String {
    s.short_name().to_string()
}

/// Run the full pipeline against one original byte string.
pub fn analyze(bytes: &[u8], cfg: &RuleConfig) -> Trace {
    // Stage 0: permissive decode.
    let raw = permissive_decode(bytes);
    let mut issues = Vec::new();
    let mut raw_notes = vec![String::new(); raw.len()];
    for (idx, u) in raw.iter().enumerate() {
        match u {
            Unit::InvalidByte(b) => {
                raw_notes[idx] = format!("invalid UTF-8 lead/trail byte 0x{b:02X}");
                issues.push(Issue::InvalidUtf8 {
                    index: idx,
                    byte: *b,
                });
            }
            Unit::Surrogate(cp) => {
                raw_notes[idx] = format!("lone surrogate U+{cp:04X}");
                issues.push(Issue::LoneSurrogate {
                    index: idx,
                    cp: *cp,
                });
            }
            _ => {}
        }
    }
    let stage0 = StageView {
        name: "raw".to_string(),
        units: raw.iter().map(|u| JsonUnit::of(*u)).collect(),
        provenance: (0..raw.len()).collect(),
        notes: raw_notes,
    };
    let mut stages = vec![stage0];

    // Stage 1: keep only valid scalar values; mark noncharacters.
    let mut cur: Vec<Unit> = Vec::new();
    let mut prov: Vec<usize> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for (idx, u) in raw.iter().enumerate() {
        if let Some(cp) = u.scalar_u32() {
            cur.push(*u);
            prov.push(idx);
            let mut note = String::new();
            if is_noncharacter(cp) {
                note = format!("noncharacter U+{cp:04X}");
                issues.push(Issue::Noncharacter {
                    index: cur.len() - 1,
                    cp,
                });
            }
            notes.push(note);
        }
    }
    stages.push(view(&cur, prov, notes, "scalars"));

    // Stage 2: default case folding (full).
    if cfg.case_fold {
        let input: String = cur.iter().filter_map(|u| u.as_char()).collect();
        let prev_chars: Vec<char> = input.chars().collect();
        let next_chars: Vec<char> = input.chars().default_case_fold().collect();
        let aligned = lcs_align(&prev_chars, &next_chars);
        let new_units: Vec<Unit> = next_chars.iter().map(|c| Unit::Scalar(*c)).collect();
        let fold_notes: Vec<String> = next_chars
            .iter()
            .enumerate()
            .map(|(j, c)| {
                let p = aligned[j];
                if p < prev_chars.len() && prev_chars[p] != *c {
                    format!(
                        "case fold: U+{:04X} \u{2192} U+{:04X}",
                        prev_chars[p] as u32, *c as u32
                    )
                } else {
                    String::new()
                }
            })
            .collect();
        let new_prov: Vec<usize> = aligned.to_vec();
        stages.push(view(&new_units, new_prov, fold_notes, "case_fold"));
        cur = new_units;
    } else {
        let (st, kept, _, _) = identity(&cur, "case_fold");
        stages.push(st);
        cur = kept;
    }

    // Stage 3: strip default-ignorable units.
    if cfg.strip_default_ignorable {
        let mut kept_units = Vec::new();
        let mut kept_prov = Vec::new();
        let mut kept_notes = Vec::new();
        for (idx, u) in cur.iter().enumerate() {
            let strip = match u {
                Unit::Scalar(c) => is_default_ignorable(*c),
                _ => false,
            };
            if strip {
                continue;
            }
            kept_units.push(*u);
            kept_prov.push(idx);
            kept_notes.push(String::new());
        }
        stages.push(view(&kept_units, kept_prov, kept_notes, "strip_ignorable"));
        cur = kept_units;
    } else {
        let (st, kept, _, _) = identity(&cur, "strip_ignorable");
        stages.push(st);
        cur = kept;
    }

    // Stage 4: NFC / NFKC.
    {
        let input: String = cur.iter().filter_map(|u| u.as_char()).collect();
        let prev_chars: Vec<char> = input.chars().collect();
        let next: String = match cfg.normalization {
            Normalization::Nfc => input.nfc().collect(),
            Normalization::Nfkc => input.nfkc().collect(),
        };
        let next_chars: Vec<char> = next.chars().collect();
        let aligned = lcs_align(&prev_chars, &next_chars);
        let new_units: Vec<Unit> = next_chars.iter().map(|c| Unit::Scalar(*c)).collect();
        let norm_notes: Vec<String> = next_chars
            .iter()
            .enumerate()
            .map(|(j, c)| {
                let p = aligned[j];
                if p < prev_chars.len() && prev_chars[p] != *c {
                    format!(
                        "{} \u{2192} U+{:04X}",
                        cfg.normalization.as_str(),
                        *c as u32
                    )
                } else {
                    String::new()
                }
            })
            .collect();
        stages.push(view(&new_units, aligned, norm_notes, "normalize"));
        cur = new_units;
    }

    // Script report over the final value.
    let final_chars: Vec<char> = cur.iter().filter_map(|u| u.as_char()).collect();
    let mut scripts: Vec<Script> = Vec::new();
    for c in &final_chars {
        // Noncharacters have no meaningful script identity (UAX #24 treats
        // them as Common); exclude so they cannot inflate mixed-script risk.
        if is_noncharacter(*c as u32) {
            continue;
        }
        let s = c.script();
        if !matches!(s, Script::Common | Script::Inherited | Script::Unknown)
            && !scripts.contains(&s)
        {
            scripts.push(s);
        }
    }
    let script_names: Vec<String> = scripts.iter().map(|s| script_name(*s)).collect();
    let mixed_script = scripts.len() > 1;
    if mixed_script {
        issues.push(Issue::MixedScript {
            scripts: script_names.clone(),
        });
    }

    let mut restriction_violated = false;
    if let Some(allowed_code) = &cfg.restrict_script {
        let allowed = Script::from_short_name(allowed_code);
        if let Some(allowed) = allowed {
            if !scripts.iter().all(|s| *s == allowed) {
                let found = script_names.join("+");
                issues.push(Issue::ScriptRestriction {
                    found: found.clone(),
                    allowed: allowed_code.clone(),
                });
                restriction_violated = true;
            }
        }
    }

    let canonical: String = final_chars.iter().collect();
    let canonical_cps = final_chars.iter().map(|c| *c as u32).collect();

    Trace {
        stages,
        issues,
        canonical,
        canonical_cps,
        scripts: script_names,
        mixed_script,
        restriction_violated,
    }
}

/// Convenience wrapper for ad-hoc text (search box, rename candidates).
pub fn analyze_str(s: &str, cfg: &RuleConfig) -> Trace {
    analyze(s.as_bytes(), cfg)
}
