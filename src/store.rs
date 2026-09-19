use crate::engine::RuleConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StoredRecordKind {
    Text,
    RawBase64,
    Codepoints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRecord {
    pub id: String,
    pub kind: StoredRecordKind,
    pub original_text: Option<String>,
    pub original_bytes_base64: String,
    pub source_version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub records: BTreeMap<String, StoredRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanStatus {
    Draft,
    Approved,
    Rejected,
    Applied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDecision {
    pub action: String,
    pub primary_record_id: Option<String>,
    pub replacements: BTreeMap<String, String>,
    pub keep_old_alias: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub dataset_id: String,
    pub dataset_version: u64,
    pub from_rules: RuleConfig,
    pub from_snapshot_json: String,
    pub rules: RuleConfig,
    pub rule_snapshot_json: String,
    pub status: PlanStatus,
    pub version: u64,
    pub groups: BTreeMap<String, Vec<String>>,
    pub decisions: BTreeMap<String, GroupDecision>,
    pub baseline_mappings: Vec<StoredMapping>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredMapping {
    pub record_id: String,
    pub old_canonical: String,
    pub new_canonical: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedMigration {
    pub plan_id: String,
    pub dataset_id: String,
    pub baseline_plan_version: u64,
    pub mappings: Vec<StoredMapping>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u64,
    pub datasets: BTreeMap<String, Dataset>,
    pub plans: BTreeMap<String, Plan>,
    pub migrations: BTreeMap<String, AppliedMigration>,
    pub commands: BTreeMap<String, CommandReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub request_hash: String,
    pub event_type: String,
    pub response: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    Command {
        key: String,
        request_hash: String,
        command: Box<Event>,
        response: serde_json::Value,
    },
    DatasetCreated {
        dataset: Dataset,
    },
    RecordsAppended {
        dataset_id: String,
        version: u64,
        records: Vec<StoredRecord>,
    },
    PlanCreated {
        plan: Plan,
    },
    PlanUpdated {
        plan: Plan,
    },
    PlanApplied {
        plan_id: String,
        migration: AppliedMigration,
    },
}

pub struct Store {
    state: State,
    log: File,
    _lock: File,
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, String> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(dir.join("workbench.lock"))
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                return Err("data directory is already open".into());
            }
            lock.set_len(0).map_err(|e| e.to_string())?;
            lock.write_all(format!("{}\n", std::process::id()).as_bytes())
                .map_err(|e| e.to_string())?;
        }
        let path = dir.join("events.jsonl");
        let had_log = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|e| e.to_string())?;
        let mut state = State::default();
        let mut valid_len = 0usize;
        for line in contents.split_inclusive('\n') {
            if serde_json::from_str::<Event>(line).is_ok() {
                valid_len += line.len();
            } else if had_log {
                break;
            }
        }
        if valid_len != contents.len() {
            file.set_len(valid_len as u64).map_err(|e| e.to_string())?;
        }
        for line in contents[..valid_len].lines() {
            apply_event(
                &mut state,
                serde_json::from_str(line).map_err(|e| e.to_string())?,
            );
        }
        Ok(Self {
            state,
            log: file,
            _lock: lock,
        })
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn append(&mut self, event: Event) -> Result<(), String> {
        let mut line = serde_json::to_vec(&event).map_err(|e| e.to_string())?;
        line.push(b'\n');
        self.log.write_all(&line).map_err(|e| e.to_string())?;
        self.log.sync_all().map_err(|e| e.to_string())?;
        apply_event(&mut self.state, event);
        Ok(())
    }
}

fn apply_event(state: &mut State, event: Event) {
    state.version += 1;
    match event {
        Event::DatasetCreated { dataset } => {
            state.datasets.insert(dataset.id.clone(), dataset);
        }
        Event::RecordsAppended {
            dataset_id,
            version,
            records,
        } => {
            if let Some(dataset) = state.datasets.get_mut(&dataset_id) {
                for record in records {
                    dataset.records.insert(record.id.clone(), record);
                }
                dataset.version = version;
            }
        }
        Event::PlanCreated { plan } | Event::PlanUpdated { plan } => {
            state.plans.insert(plan.id.clone(), plan);
        }
        Event::PlanApplied { plan_id, migration } => {
            if let Some(plan) = state.plans.get_mut(&plan_id) {
                plan.status = PlanStatus::Applied;
            }
            state.migrations.insert(plan_id, migration);
        }
        Event::Command {
            key,
            request_hash,
            command,
            response,
        } => {
            let event_type = command_event_type(command.as_ref());
            state.commands.insert(
                key,
                CommandReceipt {
                    request_hash,
                    event_type,
                    response: response.clone(),
                },
            );
            apply_event(state, *command);
        }
    }
}

fn command_event_type(event: &Event) -> String {
    match event {
        Event::Command { command, .. } => command_event_type(command),
        Event::DatasetCreated { .. } => "dataset-created".to_owned(),
        Event::RecordsAppended { .. } => "records-appended".to_owned(),
        Event::PlanCreated { .. } => "plan-created".to_owned(),
        Event::PlanUpdated { .. } => "plan-updated".to_owned(),
        Event::PlanApplied { .. } => "plan-applied".to_owned(),
    }
    .to_owned()
}

pub fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for byte in part.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{prefix}_{hash:016x}")
}

pub fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let clean: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let table = |byte: u8| -> Result<u32, String> {
        match byte {
            b'A'..=b'Z' => Ok((byte - b'A') as u32),
            b'a'..=b'z' => Ok((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((byte - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err("invalid base64".into()),
        }
    };
    if clean.is_empty() || clean.len() % 4 != 0 {
        return Err("invalid base64 length".into());
    }
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut values = [0u32; 4];
        let mut padding = 0;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                values[index] = 0;
                padding += 1;
            } else {
                values[index] = table(*byte)?;
            }
        }
        let n = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push((n >> 16) as u8);
        if padding < 2 {
            out.push((n >> 8) as u8);
        }
        if padding == 0 {
            out.push(n as u8);
        }
    }
    Ok(out)
}
