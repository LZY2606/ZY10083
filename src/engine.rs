use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnicodeTable {
    U15_1,
    U16_0,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum NormalizationForm {
    Nfc,
    Nfkc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ScriptPolicy {
    #[default]
    Off,
    Single,
    Allowed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    #[serde(default)]
    pub unicode_table: Option<UnicodeTable>,
    #[serde(default)]
    pub case_fold: bool,
    #[serde(default)]
    pub normalization: Option<NormalizationForm>,
    #[serde(default)]
    pub strip_default_ignorable: bool,
    #[serde(default)]
    pub script_policy: ScriptPolicy,
    #[serde(default)]
    pub allowed_scripts: BTreeSet<String>,
}

impl RuleConfig {
    pub fn resolved(mut self) -> Self {
        if self.unicode_table.is_none() {
            self.unicode_table = Some(UnicodeTable::U16_0);
        }
        if self.normalization.is_none() {
            self.normalization = Some(NormalizationForm::Nfc);
        }
        self
    }

    pub fn snapshot(&self) -> RuleSnapshot {
        let table = self.unicode_table.unwrap_or(UnicodeTable::U16_0);
        let (icu, unicode) = match table {
            UnicodeTable::U15_1 => ("1.5.1", "15.1.0"),
            UnicodeTable::U16_0 => ("2.3.0", "16.0.0"),
        };
        RuleSnapshot {
            unicode_version: unicode.to_owned(),
            case_fold: self.case_fold,
            normalization: self.normalization.unwrap_or(NormalizationForm::Nfc),
            strip_default_ignorable: self.strip_default_ignorable,
            script_policy: self.script_policy,
            allowed_scripts: self.allowed_scripts.clone(),
            icu_casemap: icu.to_owned(),
            icu_normalizer: if table == UnicodeTable::U15_1 {
                "1.5.0".into()
            } else {
                icu.into()
            },
            icu_properties: icu.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleSnapshot {
    pub unicode_version: String,
    pub case_fold: bool,
    pub normalization: NormalizationForm,
    pub strip_default_ignorable: bool,
    pub script_policy: ScriptPolicy,
    pub allowed_scripts: BTreeSet<String>,
    pub icu_casemap: String,
    pub icu_normalizer: String,
    pub icu_properties: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodePoint {
    pub value: u32,
    pub hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character: Option<char>,
    pub surrogate: bool,
}

impl CodePoint {
    fn new(value: u32) -> Self {
        let surrogate = (0xd800..=0xdfff).contains(&value);
        Self {
            value,
            hex: format!("U+{value:04X}"),
            character: char::from_u32(value),
            surrogate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stage {
    pub name: String,
    pub codepoints: Vec<CodePoint>,
    pub text: Option<String>,
    pub rule_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub kind: String,
    pub offset: usize,
    pub codepoint: Option<String>,
    pub bytes: Vec<String>,
    pub message: String,
    pub script: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnalyzedRecord {
    pub record_id: String,
    pub source_version: u64,
    pub original_text: Option<String>,
    pub original_base64: String,
    pub stages: Vec<Stage>,
    pub canonical: String,
    pub issues: Vec<Issue>,
    pub eligible: bool,
    pub scripts: BTreeSet<String>,
    pub mixed_script: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub canonical: String,
    pub canonical_hex: Vec<String>,
    pub record_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Analysis {
    pub rules: RuleSnapshot,
    pub records: Vec<AnalyzedRecord>,
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Clone)]
pub struct SourceToken {
    pub codepoint: u32,
    pub offset: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DecodedSource {
    pub tokens: Vec<SourceToken>,
    pub issues: Vec<Issue>,
    pub valid_utf8: bool,
}

pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | input[i + 2] as u32;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        out.push(ALPHABET[(n & 63) as usize] as char);
        i += 3;
    }
    match input.len() - i {
        1 => {
            let n = (input[i] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

pub fn decode_utf8_strict(bytes: &[u8]) -> DecodedSource {
    let mut decoded = DecodedSource {
        valid_utf8: true,
        ..DecodedSource::default()
    };
    let mut offset = 0;
    while offset < bytes.len() {
        let start = offset;
        let first = bytes[offset];
        let len = if first < 0x80 {
            1
        } else if first >> 5 == 0b110 {
            2
        } else if first >> 4 == 0b1110 {
            3
        } else if first >> 3 == 0b11110 {
            4
        } else {
            decoded.valid_utf8 = false;
            decoded.issues.push(Issue {
                kind: "invalid-utf8".into(),
                offset: start,
                codepoint: None,
                bytes: vec![format!("0x{first:02X}")],
                message: "invalid UTF-8 lead byte".into(),
                script: None,
            });
            offset += 1;
            continue;
        };
        if start + len > bytes.len() {
            decoded.valid_utf8 = false;
            decoded.issues.push(Issue {
                kind: "invalid-utf8".into(),
                offset: start,
                codepoint: None,
                bytes: bytes[start..]
                    .iter()
                    .map(|b| format!("0x{b:02X}"))
                    .collect(),
                message: "truncated UTF-8 sequence".into(),
                script: None,
            });
            break;
        }
        let seq = &bytes[start..start + len];
        if let Some(codepoint) = decode_one_utf8(seq) {
            decoded.tokens.push(SourceToken {
                codepoint,
                offset: start,
            });
        } else {
            decoded.valid_utf8 = false;
            decoded.issues.push(Issue {
                kind: "invalid-utf8".into(),
                offset: start,
                codepoint: None,
                bytes: seq.iter().map(|b| format!("0x{b:02X}")).collect(),
                message: "malformed UTF-8 sequence".into(),
                script: None,
            });
        }
        offset += len;
    }
    decoded
}

fn decode_one_utf8(seq: &[u8]) -> Option<u32> {
    match seq.len() {
        1 => Some(seq[0] as u32),
        2 => {
            if seq[1] >> 6 != 0b10 {
                return None;
            }
            let value = (((seq[0] & 0x1f) as u32) << 6) | cont(seq[1]);
            (0x80..=0x7ff).contains(&value).then_some(value)
        }
        3 => {
            if seq[1] >> 6 != 0b10 || seq[2] >> 6 != 0b10 {
                return None;
            }
            let value = (((seq[0] & 0x0f) as u32) << 12) | (cont(seq[1]) << 6) | cont(seq[2]);
            if (0x800..=0xd7ff).contains(&value) || (0xe000..=0xffff).contains(&value) {
                Some(value)
            } else {
                None
            }
        }
        4 => {
            if seq[1] >> 6 != 0b10 || seq[2] >> 6 != 0b10 || seq[3] >> 6 != 0b10 {
                return None;
            }
            let value = (((seq[0] & 0x07) as u32) << 18)
                | (cont(seq[1]) << 12)
                | (cont(seq[2]) << 6)
                | cont(seq[3]);
            (0x10000..=0x10ffff).contains(&value).then_some(value)
        }
        _ => None,
    }
}

fn cont(byte: u8) -> u32 {
    (byte & 0x3f) as u32
}

pub fn is_noncharacter(codepoint: u32) -> bool {
    (0xfdd0..=0xfdef).contains(&codepoint) || matches!(codepoint & 0xffff, 0xfffe | 0xffff)
}

pub fn encode_wtf8_codepoints(codepoints: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for &codepoint in codepoints {
        if let Some(ch) = char::from_u32(codepoint) {
            let mut buf = [0; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        } else if (0xd800..=0xdfff).contains(&codepoint) {
            let value = codepoint - 0xd800;
            bytes.push(0xed);
            bytes.push(0xa0 | ((value >> 6) as u8));
            bytes.push(0x80 | ((value & 0x3f) as u8));
        }
    }
    bytes
}

pub fn codepoints_to_string(codepoints: &[u32]) -> Option<String> {
    codepoints
        .iter()
        .map(|value| char::from_u32(*value))
        .collect()
}

fn codepoints_from_string(text: &str) -> Vec<u32> {
    text.chars().map(|ch| ch as u32).collect()
}

fn normalize_codepoints(table: UnicodeTable, form: NormalizationForm, input: &[u32]) -> Vec<u32> {
    let bytes = encode_wtf8_codepoints(input);
    let text = String::from_utf8_lossy(&bytes);
    let normalized = match (table, form) {
        (UnicodeTable::U15_1, NormalizationForm::Nfc) => {
            icu_normalizer_old::ComposingNormalizer::new_nfc().normalize(&text)
        }
        (UnicodeTable::U15_1, NormalizationForm::Nfkc) => {
            icu_normalizer_old::ComposingNormalizer::new_nfkc().normalize(&text)
        }
        (UnicodeTable::U16_0, NormalizationForm::Nfc) => {
            icu_normalizer::ComposingNormalizer::new_nfc()
                .normalize(&text)
                .into_owned()
        }
        (UnicodeTable::U16_0, NormalizationForm::Nfkc) => {
            icu_normalizer::ComposingNormalizer::new_nfkc()
                .normalize(&text)
                .into_owned()
        }
    };
    codepoints_from_string(&normalized)
}

fn fold_codepoints(table: UnicodeTable, input: &[u32]) -> Vec<u32> {
    let bytes = encode_wtf8_codepoints(input);
    let text = String::from_utf8_lossy(&bytes);
    let folded = match table {
        UnicodeTable::U15_1 => icu_casemap_old::CaseMapper::new().fold_string(&text),
        UnicodeTable::U16_0 => icu_casemap::CaseMapper::new()
            .fold_string(&text)
            .into_owned(),
    };
    codepoints_from_string(&folded)
}

fn is_default_ignorable(table: UnicodeTable, codepoint: u32) -> bool {
    let Some(ch) = char::from_u32(codepoint) else {
        return false;
    };
    match table {
        UnicodeTable::U15_1 => {
            icu_properties_old::sets::default_ignorable_code_point().contains(ch)
        }
        UnicodeTable::U16_0 => icu_properties::CodePointSetData::new::<
            icu_properties::props::DefaultIgnorableCodePoint,
        >()
        .contains(ch),
    }
}

fn script_name(table: UnicodeTable, codepoint: u32) -> Option<String> {
    let ch = char::from_u32(codepoint)?;
    match table {
        UnicodeTable::U15_1 => icu_properties_old::Script::enum_to_long_name_mapper()
            .get(icu_properties_old::maps::script().get(ch))
            .map(str::to_owned),
        UnicodeTable::U16_0 => {
            icu_properties::PropertyNamesLong::<icu_properties::props::Script>::new()
                .get(
                    icu_properties::CodePointMapData::<icu_properties::props::Script>::new()
                        .get(ch),
                )
                .map(str::to_owned)
        }
    }
}

fn script_code_valid(table: UnicodeTable, name: &str) -> bool {
    match table {
        UnicodeTable::U15_1 => icu_properties_old::Script::name_to_enum_mapper()
            .get_loose(name)
            .is_some(),
        UnicodeTable::U16_0 => {
            icu_properties::PropertyParser::<icu_properties::props::Script>::new()
                .get_loose(name)
                .is_some()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInput<'a> {
    pub record_id: &'a str,
    pub source_version: u64,
    pub original_text: Option<&'a str>,
    pub original_bytes: &'a [u8],
    pub codepoint_source: bool,
}

pub fn analyze_one(config: &RuleConfig, input: EngineInput<'_>) -> AnalyzedRecord {
    let config = config.clone().resolved();
    let table = config.unicode_table.unwrap_or(UnicodeTable::U16_0);
    let form = config.normalization.unwrap_or(NormalizationForm::Nfc);
    let snapshot = config.snapshot();
    let original_base64 = base64_encode(input.original_bytes);
    let mut issues = Vec::new();
    let mut tokens = Vec::new();

    if input.codepoint_source {
        for (offset, &codepoint) in parse_codepoint_bytes(input.original_bytes)
            .iter()
            .enumerate()
        {
            let surrogate = (0xd800..=0xdfff).contains(&codepoint);
            tokens.push(SourceToken { codepoint, offset });
            if surrogate {
                issues.push(Issue {
                    kind: "lone-surrogate".into(),
                    offset,
                    codepoint: Some(CodePoint::new(codepoint).hex),
                    bytes: Vec::new(),
                    message: "unpaired Unicode surrogate".into(),
                    script: None,
                });
            }
        }
    } else {
        let decoded = decode_utf8_strict(input.original_bytes);
        issues.extend(decoded.issues);
        tokens = decoded.tokens;
    }

    for token in &tokens {
        if is_noncharacter(token.codepoint) {
            issues.push(Issue {
                kind: "noncharacter".into(),
                offset: token.offset,
                codepoint: Some(CodePoint::new(token.codepoint).hex),
                bytes: Vec::new(),
                message: "Unicode noncharacter code point".into(),
                script: None,
            });
        }
    }

    let source_codepoints: Vec<u32> = tokens.iter().map(|token| token.codepoint).collect();
    let normalized = normalize_codepoints(table, form, &source_codepoints);
    let folded = if config.case_fold {
        fold_codepoints(table, &normalized)
    } else {
        normalized.clone()
    };
    let stripped: Vec<u32> = if config.strip_default_ignorable {
        folded
            .iter()
            .copied()
            .filter(|codepoint| !is_default_ignorable(table, *codepoint))
            .collect()
    } else {
        folded.clone()
    };
    let canonical_codepoints = normalize_codepoints(table, form, &stripped);
    let canonical = codepoints_to_string(&canonical_codepoints).unwrap_or_default();

    let stage = |name: &str, codepoints: &[u32]| Stage {
        name: name.into(),
        codepoints: codepoints.iter().copied().map(CodePoint::new).collect(),
        text: codepoints_to_string(codepoints),
        rule_version: snapshot.unicode_version.clone(),
    };
    let stages = vec![
        stage("source", &source_codepoints),
        stage(
            match form {
                NormalizationForm::Nfc => "nfc-initial",
                NormalizationForm::Nfkc => "nfkc-initial",
            },
            &normalized,
        ),
        stage(
            if config.case_fold {
                "case-fold"
            } else {
                "case-fold-skipped"
            },
            &folded,
        ),
        stage(
            if config.strip_default_ignorable {
                "strip-default-ignorable"
            } else {
                "default-ignorable-retained"
            },
            &stripped,
        ),
        stage(
            match form {
                NormalizationForm::Nfc => "nfc-final",
                NormalizationForm::Nfkc => "nfkc-final",
            },
            &canonical_codepoints,
        ),
    ];

    let mut scripts = BTreeSet::new();
    for codepoint in &canonical_codepoints {
        if let Some(script) = script_name(table, *codepoint) {
            if script != "Common" && script != "Inherited" {
                scripts.insert(script);
            }
        }
    }
    let mixed_script = scripts.len() > 1;
    if mixed_script {
        issues.push(Issue {
            kind: "mixed-script-risk".into(),
            offset: 0,
            codepoint: None,
            bytes: Vec::new(),
            message: "multiple non-common scripts in normalized value".into(),
            script: Some(scripts.iter().cloned().collect::<Vec<_>>().join(",")),
        });
    }
    if config.script_policy == ScriptPolicy::Single && mixed_script {
        issues.push(Issue {
            kind: "script-restriction".into(),
            offset: 0,
            codepoint: None,
            bytes: Vec::new(),
            message: "single-script policy rejected this value".into(),
            script: None,
        });
    }
    if config.script_policy == ScriptPolicy::Allowed {
        let allowed: BTreeSet<String> = config
            .allowed_scripts
            .iter()
            .filter_map(|name| match table {
                UnicodeTable::U15_1 => Some(name.clone()),
                UnicodeTable::U16_0 => icu_properties::PropertyParser::<
                    icu_properties::props::Script,
                >::new()
                .get_loose(name)
                .and_then(|script| {
                    icu_properties::PropertyNamesLong::<icu_properties::props::Script>::new()
                        .get(script)
                        .map(str::to_owned)
                }),
            })
            .collect();
        for script in &scripts {
            if !allowed.contains(script) {
                issues.push(Issue {
                    kind: "script-restriction".into(),
                    offset: 0,
                    codepoint: None,
                    bytes: Vec::new(),
                    message: "script is not in allowed list".into(),
                    script: Some(script.clone()),
                });
            }
        }
    }

    let has_blocking_issue = issues.iter().any(|issue| {
        matches!(
            issue.kind.as_str(),
            "invalid-utf8" | "lone-surrogate" | "noncharacter" | "script-restriction"
        )
    });
    AnalyzedRecord {
        record_id: input.record_id.to_owned(),
        source_version: input.source_version,
        original_text: input.original_text.map(str::to_owned),
        original_base64,
        stages,
        canonical,
        issues,
        eligible: !has_blocking_issue,
        scripts,
        mixed_script,
    }
}

fn parse_codepoint_bytes(bytes: &[u8]) -> Vec<u32> {
    serde_json::from_slice(bytes).unwrap_or_default()
}

pub fn validate_config(config: &RuleConfig) -> Result<RuleConfig, String> {
    let resolved = config.clone().resolved();
    let table = resolved.unicode_table.unwrap_or(UnicodeTable::U16_0);
    for script in &resolved.allowed_scripts {
        if !script_code_valid(table, script) {
            return Err(format!("unknown script `{script}`"));
        }
    }
    Ok(resolved)
}

pub fn analyze_records(
    config: &RuleConfig,
    records: &[EngineInput<'_>],
) -> Result<Analysis, String> {
    let config = validate_config(config)?;
    let analyzed: Vec<_> = records
        .iter()
        .map(|record| analyze_one(&config, (*record).clone()))
        .collect();
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in &analyzed {
        if record.eligible {
            groups
                .entry(record.canonical.clone())
                .or_default()
                .push(record.record_id.clone());
        }
    }
    let buckets = groups
        .into_iter()
        .map(|(canonical, record_ids)| Bucket {
            canonical_hex: canonical
                .chars()
                .map(|ch| format!("U+{:04X}", ch as u32))
                .collect(),
            canonical,
            record_ids,
        })
        .collect();
    Ok(Analysis {
        rules: config.snapshot(),
        records: analyzed,
        buckets,
    })
}

pub fn analyze_text(config: &RuleConfig, text: &str) -> Result<Analysis, String> {
    let input = EngineInput {
        record_id: "query",
        source_version: 0,
        original_text: Some(text),
        original_bytes: text.as_bytes(),
        codepoint_source: false,
    };
    analyze_records(config, &[input])
}
