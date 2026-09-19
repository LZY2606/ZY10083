//! Business rules over the persistent store. The HTTP layer is a thin shell;
//! all normalization, collision, plan and concurrency semantics live here.

use crate::model::*;
use crate::store::{self, Event, Store};
use crate::unicode::{analyze, analyze_str, Normalization, RuleConfig};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: serde_json::Value,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError {
            status: 400,
            code: "bad_request".to_string(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        ApiError {
            status: 409,
            code: "conflict".to_string(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: 404,
            code: "not_found".to_string(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }
    pub fn with_details(mut self, d: serde_json::Value) -> Self {
        self.details = d;
        self
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": { "code": self.code, "message": self.message, "details": self.details }
        })
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RulesetReq {
    pub label: String,
    pub case_fold: bool,
    #[serde(default = "default_norm")]
    pub normalization: String,
    #[serde(default)]
    pub strip_default_ignorable: bool,
    #[serde(default)]
    pub restrict_script: Option<String>,
}

fn default_norm() -> String {
    "NFC".to_string()
}

#[derive(Deserialize)]
pub struct ImportReq {
    pub label: String,
    /// Lines: "<record_no>TAB<text>"; or {"records":[{"record_no","raw_hex"}]}.
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub records: Option<Vec<RawRecordReq>>,
}

#[derive(Deserialize)]
pub struct RawRecordReq {
    pub record_no: String,
    /// Raw bytes as hex.
    #[serde(default)]
    pub raw_hex: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Deserialize)]
pub struct BuildAnalysisReq {
    pub dataset_id: Id,
    pub ruleset_id: Id,
}

#[derive(Deserialize)]
pub struct DecisionsReq {
    /// Resource version the client last observed; OCC check target.
    #[serde(default)]
    pub base_version: Option<u64>,
    pub decisions: BTreeMap<String, GroupDecision>,
}

#[derive(Deserialize)]
pub struct ApproveReq {
    #[serde(default)]
    pub base_version: Option<u64>,
    /// All-or-nothing trial with extra (not yet imported) identifiers.
    #[serde(default)]
    pub extra_records: Vec<String>,
    /// When false, only simulate; nothing is persisted.
    #[serde(default = "default_true")]
    pub commit: bool,
}

fn default_true() -> bool {
    true
}

pub struct App {
    pub store: Store,
}

impl App {
    pub fn open(dir: impl AsRef<std::path::Path>) -> std::io::Result<App> {
        Ok(App {
            store: Store::open(dir)?,
        })
    }

    // -- rulesets -----------------------------------------------------------

    pub fn create_ruleset(&self, req: RulesetReq) -> ApiResult<(u16, Ruleset)> {
        let normalization = match req.normalization.to_uppercase().as_str() {
            "NFC" => Normalization::Nfc,
            "NFKC" => Normalization::Nfkc,
            other => {
                return Err(ApiError::bad_request(format!(
                    "unknown normalization: {other}"
                )))
            }
        };
        let config = RuleConfig {
            case_fold: req.case_fold,
            normalization,
            strip_default_ignorable: req.strip_default_ignorable,
            restrict_script: req.restrict_script.filter(|s| !s.is_empty()),
        };
        config.validate().map_err(ApiError::bad_request)?;

        let mut g = self.store.lock();
        let revision = g
            .state()
            .active_ruleset_id
            .map_or(1, |id| g.state().rulesets[&id].revision + 1);
        let ruleset = Ruleset {
            id: g.alloc_id(),
            label: req.label.clone(),
            revision,
            config,
            tables: store::current_tables(),
        };
        g.commit(Event::RulesetCreated {
            ruleset: ruleset.clone(),
        })
        .map_err(store_io)?;
        Ok((201, ruleset))
    }

    pub fn upgrade_tables(&self, note: String) -> ApiResult<(u16, Ruleset)> {
        let mut g = self.store.lock();
        let active = active_ruleset(g.state())?.clone();
        let next = Ruleset {
            id: g.alloc_id(),
            label: active.label.clone(),
            revision: active.revision + 1,
            config: active.config.clone(),
            tables: store::current_tables(),
        };
        g.commit(Event::TableUpgraded {
            new_ruleset: next.clone(),
            note,
        })
        .map_err(store_io)?;
        Ok((201, next))
    }
}

fn active_ruleset(state: &store::State) -> ApiResult<&Ruleset> {
    let id = state
        .active_ruleset_id
        .ok_or_else(|| ApiError::bad_request("no ruleset exists yet; create one first"))?;
    state
        .rulesets
        .get(&id)
        .ok_or_else(|| ApiError::not_found("active ruleset missing"))
}

fn store_io(e: std::io::Error) -> ApiError {
    ApiError {
        status: 500,
        code: "storage".to_string(),
        message: e.to_string(),
        details: serde_json::Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

fn parse_import(req: &ImportReq) -> ApiResult<Vec<ImportedRecord>> {
    let mut records = Vec::new();
    if let Some(text) = &req.text {
        for (lineno, line) in text.split('\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let (no, value) = line.split_once('\t').ok_or_else(|| {
                ApiError::bad_request(format!(
                    "line {}: expected '<record_no>TAB<text>'",
                    lineno + 1
                ))
            })?;
            if no.is_empty() {
                return Err(ApiError::bad_request(format!(
                    "line {}: empty record_no",
                    lineno + 1
                )));
            }
            records.push(ImportedRecord {
                record_no: no.to_string(),
                raw: value.as_bytes().to_vec(),
            });
        }
    }
    if let Some(raws) = &req.records {
        for r in raws {
            if r.record_no.is_empty() {
                return Err(ApiError::bad_request("record_no must not be empty"));
            }
            let raw = match (&r.raw_hex, &r.text) {
                (Some(h), _) => hex_bytes::from_hex(h).map_err(ApiError::bad_request)?,
                (None, Some(t)) => t.as_bytes().to_vec(),
                (None, None) => {
                    return Err(ApiError::bad_request(format!(
                        "record {} needs raw_hex or text",
                        r.record_no
                    )))
                }
            };
            records.push(ImportedRecord {
                record_no: r.record_no.clone(),
                raw,
            });
        }
    }
    if records.is_empty() {
        return Err(ApiError::bad_request("no records supplied"));
    }
    let mut seen = BTreeSet::new();
    for r in &records {
        if !seen.insert(r.record_no.clone()) {
            return Err(ApiError::bad_request(format!(
                "duplicate record_no in import: {}",
                r.record_no
            )));
        }
    }
    Ok(records)
}

impl App {
    pub fn import_dataset(
        &self,
        req: ImportReq,
        idempotency_key: Option<String>,
    ) -> ApiResult<(u16, serde_json::Value)> {
        if req.label.is_empty() {
            return Err(ApiError::bad_request("label must not be empty"));
        }
        let records = parse_import(&req)?;
        let mut g = self.store.lock();
        if let Some(key) = &idempotency_key {
            if let Some((status, body)) = g.state().idempotency.entries.get(key) {
                let v = serde_json::from_str::<serde_json::Value>(body)
                    .unwrap_or(serde_json::Value::Null);
                return Ok((*status, v));
            }
        }
        let id = g.alloc_id();
        let dataset = Dataset {
            id,
            label: req.label,
            records,
        };
        let body = serde_json::json!({
            "id": dataset.id, "label": dataset.label, "count": dataset.records.len()
        });
        let stored_body = if idempotency_key.is_some() {
            Some(serde_json::to_string(&body).unwrap())
        } else {
            None
        };
        g.commit(Event::DatasetImported {
            dataset,
            idempotency_key,
            response_body: stored_body,
        })
        .map_err(store_io)?;
        Ok((201, body))
    }

    // -- analyses -----------------------------------------------------------

    pub fn build_analysis(&self, req: BuildAnalysisReq) -> ApiResult<(u16, Analysis)> {
        let mut g = self.store.lock();
        let dataset = g
            .state()
            .datasets
            .get(&req.dataset_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("dataset not found"))?;
        let ruleset = g
            .state()
            .rulesets
            .get(&req.ruleset_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("ruleset not found"))?;
        let (records, buckets) = store::build_analysis_records(&dataset, &ruleset);
        let analysis = Analysis {
            id: g.alloc_id(),
            dataset_id: dataset.id,
            ruleset_id: ruleset.id,
            ruleset_revision: ruleset.revision,
            tables: ruleset.tables.clone(),
            config: ruleset.config.clone(),
            records,
            buckets,
        };
        g.commit(Event::AnalysisBuilt {
            analysis: analysis.clone(),
        })
        .map_err(store_io)?;
        Ok((201, analysis))
    }
}

// ---------------------------------------------------------------------------
// Plans: decisions, validation (alias cycles, collisions), approval
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SimulatedMapping {
    pub record_no: String,
    pub old_canonical: String,
    pub new_canonical: String,
    pub aliases: Vec<String>,
    pub rejected: bool,
    /// True for the one record in each group that owns new_canonical.
    pub owner: bool,
}

#[derive(Serialize)]
pub struct SimulationReport {
    pub plan_id: Id,
    pub plan_version: u64,
    pub ok: bool,
    pub collisions: Vec<CollisionInfo>,
    pub cycles: Vec<Vec<String>>,
    pub mapping: Vec<SimulatedMapping>,
}

#[derive(Serialize, Clone)]
pub struct CollisionInfo {
    pub value: String,
    pub record_nos: Vec<String>,
    pub reason: String,
}

impl App {
    pub fn create_plan(&self, analysis_id: Id) -> ApiResult<(u16, Plan)> {
        let mut g = self.store.lock();
        let analysis = g
            .state()
            .analyses
            .get(&analysis_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("analysis not found"))?;
        let mut decisions = BTreeMap::new();
        for b in &analysis.buckets {
            if b.record_nos.len() > 1 && !b.identical_originals {
                decisions.insert(b.bucket_hex.clone(), GroupDecision::default());
            }
        }
        let plan = Plan {
            id: g.alloc_id(),
            analysis_id,
            status: PlanStatus::Draft,
            decisions,
            mapping: Vec::new(),
            approved_ruleset_revision: None,
            approved_tables: None,
        };
        g.commit(Event::PlanCreated { plan: plan.clone() })
            .map_err(store_io)?;
        Ok((201, plan))
    }

    fn plan_resource(plan_id: Id) -> String {
        format!("plan:{plan_id}")
    }

    pub fn set_decisions(
        &self,
        plan_id: Id,
        req: DecisionsReq,
    ) -> ApiResult<(u16, serde_json::Value)> {
        let mut g = self.store.lock();
        let plan = g
            .state()
            .plans
            .get(&plan_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("plan not found"))?;
        if plan.status != PlanStatus::Draft {
            return Err(ApiError::bad_request(
                "illegal state transition: decisions can only change while the plan is draft",
            ));
        }
        let analysis = g
            .state()
            .analyses
            .get(&plan.analysis_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("analysis missing"))?;
        let valid: BTreeSet<String> = analysis
            .buckets
            .iter()
            .filter(|b| b.record_nos.len() > 1 && !b.identical_originals)
            .map(|b| b.bucket_hex.clone())
            .collect();
        for key in req.decisions.keys() {
            if !valid.contains(key) {
                return Err(ApiError::bad_request(format!(
                    "decision refers to unknown or non-conflicting bucket {key}"
                )));
            }
        }
        // Merge semantics: only groups actually present are replaced; omitted
        // groups keep their current decision.
        let mut merged = plan.decisions.clone();
        for (k, v) in req.decisions {
            merged.insert(k, v);
        }
        // Shape validation up front (rename needs a value).
        for (bucket_hex, d) in &merged {
            match d.action {
                GroupAction::Rename if d.new_value.is_empty() => {
                    return Err(ApiError::bad_request(format!(
                        "bucket {bucket_hex}: rename requires new_value"
                    )));
                }
                _ => {}
            }
        }

        let resource = Self::plan_resource(plan_id);
        let current = g.state().version_of(&resource);
        if let Some(base) = req.base_version {
            if base != current {
                return Err(occ_error(g.state(), &resource, base, current));
            }
        }

        g.commit(Event::PlanDecisionsSet {
            plan_id,
            decisions: merged.clone(),
        })
        .map_err(store_io)?;
        let new_version = g.state().version_of(&resource);
        Ok((
            200,
            serde_json::json!({ "version": new_version, "decisions": merged }),
        ))
    }

    /// Build the prospective mapping from decisions + extra trial records,
    /// returning every detected problem. Never mutates state.
    pub fn simulate(&self, plan_id: Id, extra_records: &[String]) -> ApiResult<SimulationReport> {
        run_simulation(self.store.lock().state(), plan_id, extra_records)
    }
}

fn run_simulation(
    state: &store::State,
    plan_id: Id,
    extra_records: &[String],
) -> ApiResult<SimulationReport> {
    let plan = state
        .plans
        .get(&plan_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    if plan.status != PlanStatus::Draft {
        return Err(ApiError::bad_request(
            "illegal state transition: plan is already approved",
        ));
    }
    let analysis = state
        .analyses
        .get(&plan.analysis_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("analysis missing"))?;

    // bucket hex -> bucket, for quick lookup.
    let bucket_of_canonical: BTreeMap<&str, &Bucket> = analysis
        .buckets
        .iter()
        .map(|b| (b.canonical.as_str(), b))
        .collect();

    let mut undecided: BTreeSet<String> = BTreeSet::new();
    let mut mapping: Vec<SimulatedMapping> = Vec::new();
    // prospective canonical value -> owning record_no
    let mut value_owner: BTreeMap<String, String> = BTreeMap::new();
    // prospective alias value -> owning record_no
    let mut alias_owner: BTreeMap<String, String> = BTreeMap::new();
    let mut collisions: Vec<CollisionInfo> = Vec::new();
    // rename target values produced (for cycle detection across groups)
    let mut edges: Vec<(String, String)> = Vec::new();

    // Pending "unresolved member" problems (renamed away without alias).
    let mut unresolved: Vec<(String, String)> = Vec::new();

    for ar in &analysis.records {
        let bucket = bucket_of_canonical[ar.trace.canonical.as_str()];
        let old = bucket.canonical.clone();
        let conflicting = bucket.record_nos.len() > 1 && !bucket.identical_originals;
        let representative = ar.record_no == bucket.record_nos[0];
        let d = plan
            .decisions
            .get(&bucket.bucket_hex)
            .cloned()
            .unwrap_or_default();

        if conflicting {
            match d.action {
                GroupAction::Undecided => {
                    undecided.insert(bucket.bucket_hex.clone());
                    // No mapping can be produced for undecided groups.
                    mapping.push(SimulatedMapping {
                        record_no: ar.record_no.clone(),
                        old_canonical: old,
                        new_canonical: String::new(),
                        aliases: Vec::new(),
                        rejected: false,
                        owner: false,
                    });
                    continue;
                }
                GroupAction::Reject => {
                    mapping.push(SimulatedMapping {
                        record_no: ar.record_no.clone(),
                        old_canonical: old,
                        new_canonical: String::new(),
                        aliases: Vec::new(),
                        rejected: true,
                        owner: false,
                    });
                    continue;
                }
                _ => {}
            }
        }

        let target = if conflicting && d.action == GroupAction::Rename {
            analyze_str(&d.new_value, &analysis.config).canonical
        } else {
            old.clone()
        };
        let renamed = target != old;
        let keep = match d.action {
            GroupAction::Rename | GroupAction::KeepAlias => d.keep_old_aliases,
            _ => false,
        };

        if conflicting && !representative {
            // A merge group has exactly one owner; every other original
            // survives only as a kept alias. rename without keeping the
            // old value leaves this member with no destination.
            if keep {
                mapping.push(SimulatedMapping {
                    record_no: ar.record_no.clone(),
                    old_canonical: old.clone(),
                    new_canonical: target.clone(),
                    aliases: vec![old.clone()],
                    rejected: false,
                    owner: false,
                });
                edges.push((old, target));
                continue;
            } else {
                unresolved.push((ar.record_no.clone(), old.clone()));
                mapping.push(SimulatedMapping {
                    record_no: ar.record_no.clone(),
                    old_canonical: old,
                    new_canonical: target.clone(),
                    aliases: Vec::new(),
                    rejected: false,
                    owner: false,
                });
                continue;
            }
        }

        // Singleton, or the representative of a conflicting group.
        let mut aliases = Vec::new();
        if conflicting && keep && renamed {
            aliases.push(old.clone());
            edges.push((old.clone(), target.clone()));
        }
        mapping.push(SimulatedMapping {
            record_no: ar.record_no.clone(),
            old_canonical: old,
            new_canonical: target.clone(),
            aliases: aliases.clone(),
            rejected: false,
            owner: true,
        });
        value_owner
            .entry(target)
            .or_insert_with(|| ar.record_no.clone());
        for a in aliases {
            alias_owner.entry(a).or_insert_with(|| ar.record_no.clone());
        }
    }

    for (record_no, value) in &unresolved {
        collisions.push(CollisionInfo {
            value: value.clone(),
            record_nos: vec![record_no.clone()],
            reason:
                "group member has no destination: rename must keep old aliases or group must reject"
                    .to_string(),
        });
    }

    // Collisions within the proposed mapping: two records own one value.
    let mut owners_by_value: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in &mapping {
        if m.rejected || !m.owner {
            continue;
        }
        owners_by_value
            .entry(m.new_canonical.clone())
            .or_default()
            .push(m.record_no.clone());
    }
    // Alias colliding with someone else's canonical value.
    for (value, mut owners) in owners_by_value.clone() {
        owners.sort();
        owners.dedup();
        if owners.len() > 1 {
            collisions.push(CollisionInfo {
                value: value.clone(),
                record_nos: owners,
                reason: "multiple records would share one canonical value".to_string(),
            });
        }
        if let Some(alias_holder) = alias_owner.get(&value) {
            if owners_by_value
                .get(&value)
                .map(|h| h.iter().any(|r| r != alias_holder))
                .unwrap_or(false)
            {
                let mut combined = owners_by_value.get(&value).cloned().unwrap_or_default();
                combined.push(alias_holder.clone());
                combined.sort();
                combined.dedup();
                collisions.push(CollisionInfo {
                    value: value.clone(),
                    record_nos: combined,
                    reason: "kept alias collides with another canonical value".to_string(),
                });
            }
        }
    }

    // Extra trial records and the committed baseline: the all-or-nothing
    // gate. Any new record colliding with the proposal or with something
    // already approved fails the commit.
    for raw in extra_records {
        let key = analyze_str(raw, &analysis.config).canonical;
        let mut hits: Vec<String> = Vec::new();
        if let Some(owner) = owners_by_value.get(&key) {
            if !owner.is_empty() {
                hits.push(format!("proposed:{}", owner[0]));
            }
        }
        if let Some(holder) = alias_owner.get(&key) {
            hits.push(format!("alias:{holder}"));
        }
        for c in &state.committed {
            if c.new_canonical == key {
                hits.push(format!("committed:{}", c.record_no));
            }
        }
        if !hits.is_empty() {
            collisions.push(CollisionInfo {
                value: key,
                record_nos: {
                    let mut v = hits;
                    v.sort();
                    v.dedup();
                    v
                },
                reason: format!("new record {raw:?} would collide"),
            });
        }
    }

    for hex in &undecided {
        collisions.push(CollisionInfo {
            value: format!("bucket:{hex}"),
            record_nos: Vec::new(),
            reason: "conflict group has no decision".to_string(),
        });
    }
    collisions.sort_by(|a, b| a.value.cmp(&b.value));
    collisions.dedup_by(|a, b| a.value == b.value && a.reason == b.reason);

    let cycles = detect_cycles(&edges);
    let plan_version = state.version_of(&App::plan_resource(plan_id));
    Ok(SimulationReport {
        plan_id,
        plan_version,
        ok: collisions.is_empty() && cycles.is_empty(),
        collisions,
        cycles,
        mapping,
    })
}

/// Find directed cycles in the alias/rename graph. Returns each cycle's nodes.
fn detect_cycles(edges: &[(String, String)]) -> Vec<Vec<String>> {
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (a, b) in edges {
        if a != b {
            adj.entry(a.as_str()).or_default().push(b.as_str());
        }
    }
    let mut color: BTreeMap<&str, u8> = BTreeMap::new(); // 0 white 1 gray 2 black
    let mut stack: Vec<&str> = Vec::new();
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let nodes: BTreeSet<&str> = adj.keys().copied().collect();

    fn dfs<'a>(
        node: &'a str,
        adj: &BTreeMap<&'a str, Vec<&'a str>>,
        color: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        color.insert(node, 1);
        stack.push(node);
        if let Some(nexts) = adj.get(node) {
            for next in nexts {
                match color.get(next).copied().unwrap_or(0) {
                    0 => dfs(next, adj, color, stack, cycles),
                    1 => {
                        if let Some(pos) = stack.iter().position(|n| *n == *next) {
                            let cyc: Vec<String> =
                                stack[pos..].iter().map(|s| s.to_string()).collect();
                            cycles.push(cyc);
                        }
                    }
                    _ => {}
                }
            }
        }
        stack.pop();
        color.insert(node, 2);
    }

    for n in nodes {
        if color.get(n).copied().unwrap_or(0) == 0 {
            dfs(n, &adj, &mut color, &mut stack, &mut cycles);
        }
    }
    cycles.sort();
    cycles.dedup();
    cycles
}

fn occ_error(state: &store::State, resource: &str, base: u64, current: u64) -> ApiError {
    let (_, mine) = state.changes_since(resource, base);
    let (_, theirs) = state.changes_since(resource, current.min(base));
    ApiError::conflict("resource version conflict; stale write rejected").with_details(
        serde_json::json!({
            "resource": resource,
            "base_version": base,
            "current_version": current,
            "changes_since_base": mine,
            "recent_history": theirs,
        }),
    )
}

// ---------------------------------------------------------------------------
// Approval (all-or-nothing commit) + search + compare + export
// ---------------------------------------------------------------------------

impl App {
    pub fn approve(&self, plan_id: Id, req: ApproveReq) -> ApiResult<(u16, serde_json::Value)> {
        let mut g = self.store.lock();
        let resource = Self::plan_resource(plan_id);
        let current = g.state().version_of(&resource);
        if let Some(base) = req.base_version {
            if base != current {
                return Err(occ_error(g.state(), &resource, base, current));
            }
        }
        let report = run_simulation(g.state(), plan_id, &req.extra_records)?;
        if !report.ok {
            return Err(
                ApiError::conflict("simulation failed; nothing committed").with_details(
                    serde_json::json!({
                        "collisions": report.collisions,
                        "cycles": report.cycles,
                        "baseline_committed": g.state().committed,
                    }),
                ),
            );
        }
        if !req.commit {
            return Ok((200, serde_json::json!({ "simulation": report })));
        }
        let plan = g.state().plans[&plan_id].clone();
        let analysis = g.state().analyses[&plan.analysis_id].clone();
        if plan.status == PlanStatus::Approved {
            return Err(ApiError::bad_request(
                "illegal state transition: plan already approved",
            ));
        }

        let mut applied = Vec::new();
        let mut committed = Vec::new();
        for m in &report.mapping {
            let raw = analysis
                .records
                .iter()
                .find(|r| r.record_no == m.record_no)
                .map(|r| r.raw.clone())
                .unwrap_or_default();
            applied.push(AppliedMapping {
                record_no: m.record_no.clone(),
                original_raw: raw,
                old_canonical: m.old_canonical.clone(),
                new_canonical: m.new_canonical.clone(),
                aliases: m.aliases.clone(),
                rejected: m.rejected,
            });
            if !m.rejected {
                committed.push(CommittedEntry {
                    plan_id,
                    record_no: m.record_no.clone(),
                    new_canonical: m.new_canonical.clone(),
                    aliases: m.aliases.clone(),
                });
            }
        }
        let revision = analysis.ruleset_revision;
        let tables = analysis.tables.clone();
        g.commit(Event::PlanApproved {
            plan_id,
            mapping: applied,
            committed,
            approved_ruleset_revision: revision,
            approved_tables: tables,
        })
        .map_err(store_io)?;
        let new_version = g.state().version_of(&resource);
        let (_, diff) = g.state().changes_since(&resource, current);
        Ok((
            200,
            serde_json::json!({
                "status": "approved",
                "plan_id": plan_id,
                "version": new_version,
                "changes": diff,
                "mapping_count": report.mapping.len(),
            }),
        ))
    }

    /// Search: the query itself is normalized with the viewed ruleset while
    /// the raw query is echoed back untouched.
    pub fn search(&self, analysis_id: Id, query: &str) -> ApiResult<serde_json::Value> {
        let g = self.store.lock();
        let analysis = g
            .state()
            .analyses
            .get(&analysis_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("analysis not found"))?;
        let trace = analyze(query.as_bytes(), &analysis.config);
        let mut hits = Vec::new();
        for ar in &analysis.records {
            let mut matched = Vec::new();
            if ar.raw == query.as_bytes() {
                matched.push("raw");
            }
            if ar.trace.canonical == trace.canonical {
                matched.push("canonical");
            }
            ar.trace
                .canonical
                .contains(&trace.canonical)
                .then(|| matched.push("substring"));
            if !matched.is_empty() {
                hits.push(serde_json::json!({
                    "record_no": ar.record_no,
                    "canonical": ar.trace.canonical,
                    "matched_on": matched,
                }));
            }
        }
        Ok(serde_json::json!({
            "query_raw": query,
            "query_canonical": trace.canonical,
            "query_trace": trace,
            "hits": hits,
        }))
    }

    /// Compare two analyses (typically different ruleset revisions): which
    /// buckets differ, merge or split.
    pub fn compare_analyses(&self, a_id: Id, b_id: Id) -> ApiResult<serde_json::Value> {
        let g = self.store.lock();
        let a = g
            .state()
            .analyses
            .get(&a_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("analysis not found"))?;
        let b = g
            .state()
            .analyses
            .get(&b_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("analysis not found"))?;
        let key_of = |an: &Analysis, no: &str| {
            an.buckets
                .iter()
                .find(|bk| bk.record_nos.contains(&no.to_string()))
                .map(|bk| bk.canonical.clone())
                .unwrap_or_default()
        };
        let mut changed = Vec::new();
        let mut all: BTreeSet<&String> = BTreeSet::new();
        for ar in &a.records {
            all.insert(&ar.record_no);
        }
        for no in &all {
            let ka = key_of(&a, no);
            let kb = key_of(&b, no);
            if ka != kb {
                changed.push(serde_json::json!({
                    "record_no": no, "from": ka, "to": kb
                }));
            }
        }
        Ok(serde_json::json!({
            "a": {"analysis_id": a.id, "ruleset_revision": a.ruleset_revision, "tables": a.tables},
            "b": {"analysis_id": b.id, "ruleset_revision": b.ruleset_revision, "tables": b.tables},
            "changed_records": changed,
        }))
    }
}

/// Deterministic export of one approved plan: rule snapshot, every mapping and
/// decision, plus a content hash. BTreeMap + stable field order + no timestamps.
pub fn export_plan(state: &store::State, plan_id: Id) -> ApiResult<serde_json::Value> {
    use sha2::{Digest, Sha256};
    let plan = state
        .plans
        .get(&plan_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    if plan.status != PlanStatus::Approved {
        return Err(ApiError::bad_request("only approved plans can be exported"));
    }
    let analysis = state
        .analyses
        .get(&plan.analysis_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("analysis missing"))?;

    let mut records = Vec::new();
    for m in &plan.mapping {
        records.push(serde_json::json!({
            "record_no": m.record_no,
            "original_hex": hex_bytes::to_hex(&m.original_raw),
            "old_canonical": m.old_canonical,
            "new_canonical": m.new_canonical,
            "aliases": m.aliases,
            "rejected": m.rejected,
        }));
    }
    records.sort_by(|a, b| a["record_no"].as_str().cmp(&b["record_no"].as_str()));

    let mut decisions: Vec<serde_json::Value> = plan
        .decisions
        .iter()
        .map(|(bucket_hex, d)| serde_json::json!({ "bucket_hex": bucket_hex, "decision": d }))
        .collect();
    decisions.sort_by(|a, b| a["bucket_hex"].as_str().cmp(&b["bucket_hex"].as_str()));

    let doc = serde_json::json!({
        "format": "unicode-identity-export/v1",
        "plan_id": plan.id,
        "analysis_id": analysis.id,
        "rule_snapshot": {
            "ruleset_id": analysis.ruleset_id,
            "revision": analysis.ruleset_revision,
            "config": analysis.config,
            "unicode_tables": analysis.tables,
            "approved_tables": plan.approved_tables,
        },
        "decisions": decisions,
        "mappings": records,
    });
    let canonical = serde_json::to_vec(&doc).map_err(io_json)?;
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    let hash = hex_bytes::to_hex(&hasher.finalize());
    Ok(serde_json::json!({ "document": doc, "sha256": hash }))
}

fn io_json(e: serde_json::Error) -> ApiError {
    ApiError {
        status: 500,
        code: "serialize".to_string(),
        message: e.to_string(),
        details: serde_json::Value::Null,
    }
}
