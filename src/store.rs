//! File-backed persistence: a compacted JSON snapshot plus an append-only
//! JSON-lines write-ahead log. Every mutating request is one fsynced WAL
//! record; the snapshot is rewritten atomically during compaction. Each WAL
//! record carries a monotonic event sequence number; on startup the snapshot
//! state already contains every event up to its sequence, so only newer log
//! records are replayed. That makes a crash at any point (including between
//! the snapshot rename and the log truncation) recover correctly.

use crate::model::*;
use crate::unicode::{analyze, RuleConfig, TableVersions};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const SNAPSHOT: &str = "snapshot.json";
const WAL: &str = "wal.log";
const TMP: &str = "snapshot.json.tmp";
const COMPACT_EVERY: u64 = 50;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    RulesetCreated {
        ruleset: Ruleset,
    },
    DatasetImported {
        dataset: Dataset,
        idempotency_key: Option<String>,
        response_body: Option<String>,
    },
    AnalysisBuilt {
        analysis: Analysis,
    },
    /// Creates a new ruleset revision from the currently linked Unicode tables.
    /// Existing rulesets and plans stay pinned to their own revisions.
    TableUpgraded {
        new_ruleset: Ruleset,
        note: String,
    },
    PlanCreated {
        plan: Plan,
    },
    PlanDecisionsSet {
        plan_id: Id,
        decisions: BTreeMap<String, GroupDecision>,
    },
    PlanApproved {
        plan_id: Id,
        mapping: Vec<AppliedMapping>,
        committed: Vec<CommittedEntry>,
        approved_ruleset_revision: u32,
        approved_tables: TableVersions,
    },
}

#[derive(Serialize, Deserialize)]
struct WalRecord {
    event_seq: u64,
    #[serde(flatten)]
    event: Event,
}

/// One append in a resource's change history; backs the "both sides" 409 diff.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub label: String,
    pub summary: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub next_id: Id,
    /// Highest applied event sequence number.
    pub event_seq: u64,
    pub active_ruleset_id: Option<Id>,
    pub rulesets: BTreeMap<Id, Ruleset>,
    pub datasets: BTreeMap<Id, Dataset>,
    pub analyses: BTreeMap<Id, Analysis>,
    pub plans: BTreeMap<Id, Plan>,
    /// All approved canonical values and aliases (collision baseline).
    pub committed: Vec<CommittedEntry>,
    /// Per-resource version counters and append-only change histories.
    pub versions: BTreeMap<String, u64>,
    pub history: BTreeMap<String, Vec<HistoryEntry>>,
    pub idempotency: IdempotencyTable,
}

impl State {
    fn bump(&mut self, resource: &str, label: &str, summary: &str) {
        let v = self.versions.entry(resource.to_string()).or_insert(0);
        *v += 1;
        self.history
            .entry(resource.to_string())
            .or_default()
            .push(HistoryEntry {
                label: label.to_string(),
                summary: summary.to_string(),
            });
    }

    fn take_id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn version_of(&self, resource: &str) -> u64 {
        self.versions.get(resource).copied().unwrap_or(0)
    }

    /// Entries newer than the given resource version, with the current version.
    pub fn changes_since(&self, resource: &str, base: u64) -> (u64, Vec<HistoryEntry>) {
        let current = self.version_of(resource);
        let hist = self.history.get(resource).cloned().unwrap_or_default();
        let skip = (base as usize).min(hist.len());
        (current, hist.into_iter().skip(skip).collect())
    }

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::RulesetCreated { ruleset } => {
                let id = ruleset.id;
                self.next_id = self.next_id.max(id + 1);
                self.bump(
                    &format!("ruleset:{id}"),
                    "create",
                    &format!("ruleset {} rev {}", ruleset.label, ruleset.revision),
                );
                if self.active_ruleset_id.is_none() {
                    self.active_ruleset_id = Some(id);
                }
                self.rulesets.insert(id, ruleset);
            }
            Event::DatasetImported {
                dataset,
                idempotency_key,
                response_body,
            } => {
                let id = dataset.id;
                self.next_id = self.next_id.max(id + 1);
                self.bump(
                    &format!("dataset:{id}"),
                    "import",
                    &format!(
                        "dataset {} ({} records)",
                        dataset.label,
                        dataset.records.len()
                    ),
                );
                self.datasets.insert(id, dataset);
                if let (Some(key), Some(body)) = (idempotency_key, response_body) {
                    self.idempotency.entries.insert(key, (201, body));
                }
            }
            Event::AnalysisBuilt { analysis } => {
                let id = analysis.id;
                self.next_id = self.next_id.max(id + 1);
                self.bump(
                    &format!("analysis:{id}"),
                    "build",
                    &format!(
                        "dataset {} under ruleset {} ({} buckets)",
                        analysis.dataset_id,
                        analysis.ruleset_id,
                        analysis.buckets.len()
                    ),
                );
                self.analyses.insert(id, analysis);
            }
            Event::TableUpgraded { new_ruleset, note } => {
                let id = new_ruleset.id;
                self.next_id = self.next_id.max(id + 1);
                self.bump(
                    "tables",
                    "upgrade",
                    &format!(
                        "ruleset {} -> rev {} ({note})",
                        new_ruleset.label, new_ruleset.revision
                    ),
                );
                self.rulesets.insert(id, new_ruleset);
                self.active_ruleset_id = Some(id);
            }
            Event::PlanCreated { plan } => {
                let id = plan.id;
                self.next_id = self.next_id.max(id + 1);
                self.bump(
                    &format!("plan:{id}"),
                    "create",
                    &format!("plan for analysis {}", plan.analysis_id),
                );
                self.plans.insert(id, plan);
            }
            Event::PlanDecisionsSet { plan_id, decisions } => {
                self.bump(
                    &format!("plan:{plan_id}"),
                    "decisions",
                    &format!("{} group decisions set", decisions.len()),
                );
                if let Some(plan) = self.plans.get_mut(&plan_id) {
                    plan.decisions = decisions;
                }
            }
            Event::PlanApproved {
                plan_id,
                mapping,
                committed,
                approved_ruleset_revision,
                approved_tables,
            } => {
                self.bump(
                    &format!("plan:{plan_id}"),
                    "approve",
                    &format!("approved ({} mappings)", mapping.len()),
                );
                if let Some(plan) = self.plans.get_mut(&plan_id) {
                    plan.status = PlanStatus::Approved;
                    plan.mapping = mapping;
                    plan.approved_ruleset_revision = Some(approved_ruleset_revision);
                    plan.approved_tables = Some(approved_tables);
                }
                self.committed.extend(committed);
            }
        }
    }
}

pub struct Store {
    dir: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    state: State,
    wal: File,
    since_compact: u64,
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> std::io::Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let snap_path = dir.join(SNAPSHOT);
        let wal_path = dir.join(WAL);

        let state = if snap_path.exists() {
            let data = fs::read(&snap_path)?;
            serde_json::from_slice::<State>(&data).map_err(io_err)?
        } else {
            State::default()
        };

        let mut inner = Inner {
            state,
            wal: OpenOptions::new()
                .create(true)
                .append(true)
                .open(&wal_path)?,
            since_compact: 0,
        };

        if wal_path.exists() {
            let log = fs::read_to_string(&wal_path)?;
            for (lineno, line) in log.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let rec = serde_json::from_str::<WalRecord>(line)
                    .map_err(|e| io_err(format!("WAL line {} corrupt: {e}", lineno + 1)))?;
                if rec.event_seq <= inner.state.event_seq {
                    continue; // already included in the snapshot
                }
                inner.state.event_seq = rec.event_seq;
                inner.state.apply(rec.event);
                inner.since_compact += 1;
            }
        }

        Ok(Store {
            dir,
            inner: Mutex::new(inner),
        })
    }

    pub fn lock(&self) -> Guard<'_> {
        Guard {
            guard: self.inner.lock().expect("store mutex poisoned"),
            dir: &self.dir,
        }
    }
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

pub struct Guard<'a> {
    guard: MutexGuard<'a, Inner>,
    dir: &'a Path,
}

impl<'a> Guard<'a> {
    pub fn state(&self) -> &State {
        &self.guard.state
    }

    pub fn alloc_id(&mut self) -> Id {
        self.guard.state.take_id()
    }

    /// Durably append one event, then apply it in memory.
    pub fn commit(&mut self, ev: Event) -> std::io::Result<()> {
        self.guard.state.event_seq += 1;
        let rec = WalRecord {
            event_seq: self.guard.state.event_seq,
            event: ev,
        };
        let mut line = serde_json::to_string(&rec).map_err(io_err)?;
        line.push('\n');
        self.guard.wal.write_all(line.as_bytes())?;
        self.guard.wal.flush()?;
        self.guard.wal.sync_data()?;
        // Reconstruct the owned event for in-memory application.
        let ev2 = serde_json::to_value(&rec).map_err(io_err)?;
        let event: Event = serde_json::from_value(ev2).map_err(io_err)?;
        self.guard.state.apply(event);
        self.guard.since_compact += 1;
        if self.guard.since_compact >= COMPACT_EVERY {
            self.compact()?;
            self.guard.since_compact = 0;
        }
        Ok(())
    }

    fn compact(&mut self) -> std::io::Result<()> {
        let data = serde_json::to_vec(&self.guard.state).map_err(io_err)?;
        let tmp = self.dir.join(TMP);
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&data)?;
            f.flush()?;
            f.sync_data()?;
        }
        fs::rename(&tmp, self.dir.join(SNAPSHOT))?;
        if let Ok(dir) = File::open(self.dir) {
            let _ = dir.sync_all();
        }
        self.guard.wal.set_len(0)?;
        self.guard.wal.sync_data()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Analysis construction + table helpers
// ---------------------------------------------------------------------------

pub fn build_analysis_records(
    dataset: &Dataset,
    ruleset: &Ruleset,
) -> (Vec<AnalyzedRecord>, Vec<Bucket>) {
    let cfg = &ruleset.config;
    let mut records = Vec::with_capacity(dataset.records.len());
    for rec in &dataset.records {
        let trace = analyze(&rec.raw, cfg);
        records.push(AnalyzedRecord {
            record_no: rec.record_no.clone(),
            raw: rec.raw.clone(),
            trace,
        });
    }

    let mut by_key: BTreeMap<String, Vec<&AnalyzedRecord>> = BTreeMap::new();
    for ar in &records {
        by_key
            .entry(ar.trace.canonical.clone())
            .or_default()
            .push(ar);
    }
    let mut buckets = Vec::new();
    for (canonical, members) in by_key {
        let first_raw = &members[0].raw;
        let identical = members.iter().all(|m| &m.raw == first_raw);
        let mut record_nos: Vec<String> = members.iter().map(|m| m.record_no.clone()).collect();
        record_nos.sort();
        buckets.push(Bucket {
            canonical_cps: members[0].trace.canonical_cps.clone(),
            canonical: canonical.clone(),
            bucket_hex: hex_bytes::to_hex(canonical.as_bytes()),
            record_nos,
            identical_originals: identical,
        });
    }
    buckets.sort_by(|a, b| a.bucket_hex.cmp(&b.bucket_hex));
    (records, buckets)
}

pub fn validate_rule_config(cfg: &RuleConfig) -> Result<(), String> {
    cfg.validate()
}

pub fn current_tables() -> TableVersions {
    crate::unicode::table_versions()
}
