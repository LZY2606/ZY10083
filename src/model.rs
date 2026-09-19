//! Persisted domain entities. Everything that is a result of normalization
//! is versioned and immutable; original bytes are never overwritten.

use crate::unicode::{Normalization, RuleConfig, TableVersions, Trace};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Id = u64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ruleset {
    pub id: Id,
    pub label: String,
    /// Ruleset table revision. Bumped by an explicit rule-table "upgrade";
    /// approved plans stay pinned to the revision they were built from.
    pub revision: u32,
    pub config: RuleConfig,
    pub tables: TableVersions,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportedRecord {
    /// User-provided stable record number; scoped within a dataset.
    pub record_no: String,
    /// Original bytes, preserved forever. Hex in JSON to survive arbitrary bytes.
    #[serde(with = "hex_bytes")]
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dataset {
    pub id: Id,
    pub label: String,
    pub records: Vec<ImportedRecord>,
}

/// Per-record normalized result inside an analysis.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalyzedRecord {
    pub record_no: String,
    #[serde(with = "hex_bytes")]
    pub raw: Vec<u8>,
    pub trace: Trace,
}

/// A bucket keyed by identical canonical value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bucket {
    pub canonical: String,
    pub canonical_cps: Vec<u32>,
    /// Hex of the canonical UTF-8 bytes, used as a stable bucket id.
    pub bucket_hex: String,
    pub record_nos: Vec<String>,
    /// True when every member has byte-identical originals (plain duplicates).
    pub identical_originals: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Analysis {
    pub id: Id,
    pub dataset_id: Id,
    pub ruleset_id: Id,
    /// Pinned ruleset revision at build time.
    pub ruleset_revision: u32,
    pub tables: TableVersions,
    pub config: RuleConfig,
    pub records: Vec<AnalyzedRecord>,
    pub buckets: Vec<Bucket>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupAction {
    Undecided,
    Rename,
    KeepAlias,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDecision {
    pub action: GroupAction,
    /// Required for `rename`: the new canonical value (plain text; the server
    /// re-normalizes it with the analysis ruleset).
    #[serde(default)]
    pub new_value: String,
    /// Keep the pre-migration canonical values as aliases.
    #[serde(default)]
    pub keep_old_aliases: bool,
}

impl Default for GroupDecision {
    fn default() -> Self {
        GroupDecision {
            action: GroupAction::Undecided,
            new_value: String::new(),
            keep_old_aliases: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedMapping {
    pub record_no: String,
    #[serde(with = "hex_bytes")]
    pub original_raw: Vec<u8>,
    pub old_canonical: String,
    pub new_canonical: String,
    pub aliases: Vec<String>,
    pub rejected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    Approved,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub id: Id,
    pub analysis_id: Id,
    pub status: PlanStatus,
    /// bucket_hex -> decision
    pub decisions: BTreeMap<String, GroupDecision>,
    /// Populated only after approval; never recomputed afterwards.
    pub mapping: Vec<AppliedMapping>,
    pub approved_ruleset_revision: Option<u32>,
    pub approved_tables: Option<TableVersions>,
}

/// Record of a committed mapping, used by collision checks for later plans.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommittedEntry {
    pub plan_id: Id,
    pub record_no: String,
    pub new_canonical: String,
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct IdempotencyTable {
    /// key -> (http_status, json body)
    pub entries: BTreeMap<String, (u16, String)>,
}

// ---------------------------------------------------------------------------
// hex byte helper (serde with = "module")
// ---------------------------------------------------------------------------

pub mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&to_hex(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let v: String = Deserialize::deserialize(d)?;
        from_hex(&v).map_err(serde::de::Error::custom)
    }

    pub fn to_hex(bytes: &[u8]) -> String {
        const H: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(H[(b >> 4) as usize] as char);
            out.push(H[(b & 0x0F) as usize] as char);
        }
        out
    }

    pub fn from_hex(s: &str) -> Result<Vec<u8>, String> {
        let bytes = s.as_bytes();
        if !bytes.len().is_multiple_of(2) {
            return Err("hex string has odd length".to_string());
        }
        let mut out = Vec::with_capacity(bytes.len() / 2);
        let mut i = 0;
        while i < bytes.len() {
            let hi = hex_digit(bytes[i])?;
            let lo = hex_digit(bytes[i + 1])?;
            out.push((hi << 4) | lo);
            i += 2;
        }
        Ok(out)
    }

    fn hex_digit(b: u8) -> Result<u8, String> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(format!("invalid hex digit 0x{b:02X}")),
        }
    }
}

/// Normalization label reused in summaries/exports.
pub fn norm_label(n: Normalization) -> &'static str {
    n.as_str()
}

/// Wrapper so ruleset config validation is reachable from model tests.
pub fn validate_config(cfg: &RuleConfig) -> Result<(), String> {
    cfg.validate()
}
