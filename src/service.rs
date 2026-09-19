use crate::engine::{
    analyze_records, analyze_text, base64_encode, validate_config, Analysis, EngineInput,
    RuleConfig,
};
use crate::store::{
    base64_decode, stable_id, Dataset, Event, GroupDecision, Plan, PlanStatus, Store,
    StoredMapping, StoredRecord, StoredRecordKind,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(Debug)]
pub struct ServiceError {
    pub status: u16,
    pub body: serde_json::Value,
}

impl ServiceError {
    fn new(status: u16, code: &str, details: serde_json::Value) -> Self {
        Self {
            status,
            body: serde_json::json!({ "error": code, "details": details }),
        }
    }
}

fn bad_request(message: impl Into<String>) -> ServiceError {
    ServiceError::new(
        400,
        "bad-request",
        serde_json::json!({"message": message.into()}),
    )
}

fn conflict(message: impl Into<String>) -> ServiceError {
    ServiceError::new(
        409,
        "conflict",
        serde_json::json!({"message": message.into()}),
    )
}

fn not_found(message: impl Into<String>) -> ServiceError {
    ServiceError::new(
        404,
        "not-found",
        serde_json::json!({"message": message.into()}),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRecord {
    pub id: String,
    pub text: Option<String>,
    pub raw_base64: Option<String>,
    pub codepoints: Option<Vec<u32>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDataset {
    pub name: String,
    pub records: Vec<ImportRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendRecordsReq {
    pub expected_version: u64,
    pub records: Vec<ImportRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePlan {
    pub dataset_id: String,
    pub expected_dataset_version: u64,
    #[serde(default)]
    pub from_rules: Option<RuleConfig>,
    pub rules: RuleConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupDecisionReq {
    pub action: String,
    #[serde(default)]
    pub primary_record_id: Option<String>,
    #[serde(default)]
    pub replacements: BTreeMap<String, String>,
    #[serde(default)]
    pub keep_old_alias: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePlanReq {
    pub expected_version: u64,
    pub status: Option<String>,
    #[serde(default)]
    pub decisions: BTreeMap<String, GroupDecisionReq>,
}

impl std::fmt::Display for crate::store::PlanStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPlanReq {
    pub expected_version: u64,
}

fn record_payload(record: &StoredRecord) -> Result<Vec<u8>, ServiceError> {
    if record.kind == StoredRecordKind::Codepoints {
        Ok(record.original_bytes_base64.as_bytes().to_vec())
    } else {
        base64_decode(&record.original_bytes_base64).map_err(bad_request)
    }
}

pub fn analysis_for(
    dataset: &crate::store::Dataset,
    rules: &RuleConfig,
) -> Result<Analysis, ServiceError> {
    let rules = validate_config(rules).map_err(bad_request)?;
    let payloads: Vec<Vec<u8>> = dataset
        .records
        .values()
        .map(record_payload)
        .collect::<Result<_, _>>()?;
    let inputs: Vec<EngineInput<'_>> = dataset
        .records
        .values()
        .zip(payloads.iter())
        .map(|(record, bytes)| EngineInput {
            record_id: &record.id,
            source_version: record.source_version,
            original_text: record.original_text.as_deref(),
            original_bytes: bytes,
            codepoint_source: record.kind == StoredRecordKind::Codepoints,
        })
        .collect();
    analyze_records(&rules, &inputs).map_err(bad_request)
}

fn convert_records(
    records: Vec<ImportRecord>,
    source_version: u64,
) -> Result<Vec<StoredRecord>, ServiceError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for record in records {
        if record.id.trim().is_empty() || !seen.insert(record.id.clone()) {
            return Err(bad_request("record id must be non-empty and unique"));
        }
        let count = record.text.is_some() as u8
            + record.raw_base64.is_some() as u8
            + record.codepoints.is_some() as u8;
        if count != 1 {
            return Err(bad_request("each record requires exactly one source form"));
        }
        if let Some(text) = record.text {
            out.push(StoredRecord {
                id: record.id,
                kind: StoredRecordKind::Text,
                original_text: Some(text.clone()),
                original_bytes_base64: base64_encode(text.as_bytes()),
                source_version,
            });
        } else if let Some(raw) = record.raw_base64 {
            let bytes = base64_decode(&raw).map_err(bad_request)?;
            out.push(StoredRecord {
                id: record.id,
                kind: StoredRecordKind::RawBase64,
                original_text: None,
                original_bytes_base64: base64_encode(&bytes),
                source_version,
            });
        } else if let Some(codepoints) = record.codepoints {
            if codepoints.iter().any(|cp| *cp > 0x10ffff) {
                return Err(bad_request("codepoint outside Unicode codespace"));
            }
            let json = serde_json::to_vec(&codepoints).map_err(|e| bad_request(e.to_string()))?;
            out.push(StoredRecord {
                id: record.id,
                kind: StoredRecordKind::Codepoints,
                original_text: None,
                original_bytes_base64: String::from_utf8(json)
                    .map_err(|e| bad_request(e.to_string()))?,
                source_version,
            });
        }
    }
    Ok(out)
}

fn dataset_view(dataset: &Dataset) -> serde_json::Value {
    serde_json::json!({
        "id": dataset.id,
        "name": dataset.name,
        "version": dataset.version,
        "record_count": dataset.records.len()
    })
}

fn append_command(
    store: &mut Store,
    key: Option<&String>,
    request_hash: String,
    event: Event,
    response: serde_json::Value,
) -> Result<Option<serde_json::Value>, ServiceError> {
    if let Some(key) = key {
        if let Some(receipt) = store.state().commands.get(key) {
            if receipt.request_hash != request_hash {
                return Err(conflict(
                    "Idempotency-Key was reused with a different request",
                ));
            }
            return Ok(Some(receipt.response.clone()));
        }
        let response = response;
        store
            .append(Event::Command {
                key: key.clone(),
                request_hash,
                command: Box::new(event),
                response,
            })
            .map_err(conflict)?;
        Ok(None)
    } else {
        store.append(event).map_err(conflict)?;
        Ok(None)
    }
}

fn request_hash(value: &serde_json::Value) -> String {
    stable_id("req", &[&serde_json::to_string(value).unwrap_or_default()])
}

pub fn create_dataset(
    store: &mut Store,
    request: CreateDataset,
    idempotency_key: Option<String>,
) -> Result<serde_json::Value, ServiceError> {
    let records = convert_records(request.records, 1)?;
    let shape = records
        .iter()
        .map(|record| format!("{}:{}", record.id, record.original_bytes_base64))
        .collect::<Vec<_>>()
        .join("|");
    let id = stable_id("ds", &[&request.name, &shape]);
    if let Some(existing) = store.state().datasets.get(&id) {
        return Ok(dataset_view(existing));
    }
    let payload = serde_json::json!({"name": request.name, "shape": shape});
    let dataset = Dataset {
        id: id.clone(),
        name: request.name,
        version: 1,
        records: records.into_iter().map(|r| (r.id.clone(), r)).collect(),
    };
    let response = dataset_view(&dataset);
    if let Some(replay) = append_command(
        store,
        idempotency_key.as_ref(),
        request_hash(&payload),
        Event::DatasetCreated { dataset },
        response.clone(),
    )? {
        return Ok(replay);
    }
    if idempotency_key.is_some() {
        return Ok(response);
    }
    Ok(dataset_view(store.state().datasets.get(&id).ok_or_else(
        || conflict("dataset vanished after append"),
    )?))
}

pub fn append_records(
    store: &mut Store,
    dataset_id: &str,
    request: AppendRecordsReq,
    idempotency_key: Option<String>,
) -> Result<serde_json::Value, ServiceError> {
    let dataset = store
        .state()
        .datasets
        .get(dataset_id)
        .ok_or_else(|| not_found("dataset not found"))?
        .clone();
    if dataset.version != request.expected_version {
        return Err(ServiceError::new(
            409,
            "version-conflict",
            serde_json::json!({
                "current_version": dataset.version,
                "supplied_version": request.expected_version,
                "server_records": dataset.records.keys().cloned().collect::<Vec<_>>(),
                "incoming_records": request.records.iter().map(|record| record.id.clone()).collect::<Vec<_>>()
            }),
        ));
    }
    let records = convert_records(request.records, dataset.version + 1)?;
    for record in &records {
        if dataset.records.contains_key(&record.id) {
            return Err(conflict(format!("record `{}` already exists", record.id)));
        }
    }
    let next_version = dataset.version + 1;
    let record_ids: Vec<String> = records.iter().map(|r| r.id.clone()).collect();
    let event = Event::RecordsAppended {
        dataset_id: dataset_id.to_owned(),
        version: next_version,
        records,
    };
    let payload = serde_json::json!({
        "dataset_id": dataset_id,
        "expected_version": request.expected_version,
        "record_ids": record_ids
    });
    let response = {
        let mut preview = store.state().datasets.get(dataset_id).unwrap().clone();
        preview.version = next_version;
        dataset_view(&preview)
    };
    if let Some(replay) = append_command(
        store,
        idempotency_key.as_ref(),
        request_hash(&payload),
        event,
        response.clone(),
    )? {
        return Ok(replay);
    }
    if idempotency_key.is_some() {
        return Ok(response);
    }
    Ok(dataset_view(
        store.state().datasets.get(dataset_id).unwrap(),
    ))
}

pub fn get_dataset(store: &Store, dataset_id: &str) -> Result<serde_json::Value, ServiceError> {
    let dataset = store
        .state()
        .datasets
        .get(dataset_id)
        .ok_or_else(|| not_found("dataset not found"))?;
    Ok(serde_json::json!({
        "dataset": dataset_view(dataset),
        "records": dataset.records
    }))
}

pub fn analyze_dataset(
    store: &Store,
    dataset_id: &str,
    rules: RuleConfig,
) -> Result<serde_json::Value, ServiceError> {
    let dataset = store
        .state()
        .datasets
        .get(dataset_id)
        .ok_or_else(|| not_found("dataset not found"))?;
    Ok(serde_json::to_value(analysis_for(dataset, &rules)?).unwrap_or_default())
}

pub fn create_plan(
    store: &mut Store,
    request: CreatePlan,
    idempotency_key: Option<String>,
) -> Result<serde_json::Value, ServiceError> {
    let dataset = store
        .state()
        .datasets
        .get(&request.dataset_id)
        .ok_or_else(|| not_found("dataset not found"))?
        .clone();
    if dataset.version != request.expected_dataset_version {
        return Err(plan_dataset_conflict(
            dataset.version,
            request.expected_dataset_version,
        ));
    }
    let from_rules = request.from_rules.clone().unwrap_or_default();
    let rules = validate_config(&request.rules).map_err(bad_request)?;
    validate_config(&from_rules).map_err(bad_request)?;
    let analysis = analysis_for(&dataset, &rules)?;
    let from_analysis = analysis_for(&dataset, &from_rules)?;
    let groups: BTreeMap<String, Vec<String>> = analysis
        .buckets
        .iter()
        .filter(|bucket| bucket.record_ids.len() > 1)
        .map(|bucket| (bucket.canonical.clone(), bucket.record_ids.clone()))
        .collect();
    let shape = serde_json::to_string(&(&dataset.id, dataset.version, &from_rules, &rules))
        .map_err(|e| bad_request(e.to_string()))?;
    let id = stable_id("plan", &[&shape]);
    if let Some(plan) = store.state().plans.get(&id) {
        return plan_view(store, plan);
    }
    let plan = Plan {
        id: id.clone(),
        dataset_id: dataset.id.clone(),
        dataset_version: dataset.version,
        from_rules,
        from_snapshot_json: serde_json::to_string(&from_analysis.rules).unwrap_or_default(),
        rules,
        rule_snapshot_json: serde_json::to_string(&analysis.rules).unwrap_or_default(),
        status: PlanStatus::Draft,
        version: 1,
        groups,
        decisions: BTreeMap::new(),
        baseline_mappings: Vec::new(),
    };
    let payload = serde_json::json!({"shape": shape});
    let response = serde_json::to_value(&plan).unwrap_or_default();
    if let Some(replay) = append_command(
        store,
        idempotency_key.as_ref(),
        request_hash(&payload),
        Event::PlanCreated { plan },
        response.clone(),
    )? {
        return Ok(replay);
    }
    if idempotency_key.is_some() {
        return Ok(response);
    }
    let plan = store
        .state()
        .plans
        .get(&id)
        .ok_or_else(|| conflict("plan vanished after append"))?;
    plan_view(store, plan)
}

fn plan_dataset_conflict(current: u64, supplied: u64) -> ServiceError {
    ServiceError::new(
        409,
        "version-conflict",
        serde_json::json!({
            "current_dataset_version": current,
            "supplied_dataset_version": supplied
        }),
    )
}

fn dataset_snapshot(store: &Store, plan: &Plan) -> Result<Dataset, ServiceError> {
    let dataset = store
        .state()
        .datasets
        .get(&plan.dataset_id)
        .ok_or_else(|| not_found("dataset not found"))?;
    let mut snapshot = dataset.clone();
    snapshot
        .records
        .retain(|_, record| record.source_version <= plan.dataset_version);
    Ok(snapshot)
}

fn canonical_by_record(analysis: &Analysis) -> BTreeMap<String, String> {
    analysis
        .records
        .iter()
        .filter(|record| record.eligible)
        .map(|record| (record.record_id.clone(), record.canonical.clone()))
        .collect()
}

fn requested_mappings(
    plan: &Plan,
    source: &BTreeMap<String, String>,
    target: &BTreeMap<String, String>,
) -> Result<Vec<StoredMapping>, ServiceError> {
    let mut mappings = Vec::new();
    let mut new_targets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (canonical, record_ids) in &plan.groups {
        let decision = plan
            .decisions
            .get(canonical)
            .ok_or_else(|| bad_request(format!("missing decision for bucket `{canonical}`")))?;
        match decision.action.as_str() {
            "reject" => {}
            "keep-alias" => {
                let primary = decision
                    .primary_record_id
                    .as_deref()
                    .ok_or_else(|| bad_request("keep-alias requires primary_record_id"))?;
                if !record_ids.iter().any(|id| id == primary) {
                    return Err(bad_request("primary record is outside its conflict group"));
                }
                let new_canonical = target
                    .get(primary)
                    .ok_or_else(|| bad_request("primary record is not eligible"))?;
                for record_id in record_ids {
                    mappings.push(StoredMapping {
                        record_id: record_id.clone(),
                        old_canonical: source
                            .get(record_id)
                            .cloned()
                            .unwrap_or_else(|| canonical.clone()),
                        new_canonical: new_canonical.clone(),
                        action: "keep-alias".into(),
                    });
                }
            }
            "rename" => {
                if decision.replacements.len() != record_ids.len() {
                    return Err(bad_request(
                        "rename requires one replacement per conflict record",
                    ));
                }
                for record_id in record_ids {
                    let replacement = decision
                        .replacements
                        .get(record_id)
                        .ok_or_else(|| bad_request("missing replacement for conflict record"))?;
                    if replacement.trim().is_empty() {
                        return Err(bad_request("replacement must not be empty"));
                    }
                    let replacement = canonical_text(plan, replacement)?;
                    new_targets
                        .entry(replacement.clone())
                        .or_default()
                        .insert(record_id.clone());
                    mappings.push(StoredMapping {
                        record_id: record_id.clone(),
                        old_canonical: source
                            .get(record_id)
                            .cloned()
                            .unwrap_or_else(|| canonical.clone()),
                        new_canonical: replacement.clone(),
                        action: "rename".into(),
                    });
                    if decision.keep_old_alias {
                        mappings.push(StoredMapping {
                            record_id: record_id.clone(),
                            old_canonical: canonical.clone(),
                            new_canonical: replacement.clone(),
                            action: "old-alias".into(),
                        });
                    }
                }
            }
            other => return Err(bad_request(format!("unknown action `{other}`"))),
        }
    }
    for (new_canonical, record_id) in &new_targets {
        if record_id.len() > 1 {
            return Err(bad_request(format!(
                "rename targets collide at `{new_canonical}`"
            )));
        }
        let record_id = record_id.iter().next().unwrap();
        if let Some(owner) = target.get(new_canonical) {
            let bucket_records = plan.groups.get(new_canonical);
            let owner_in_same_group =
                bucket_records.is_some_and(|records| records.iter().any(|id| id == owner));
            if !owner_in_same_group && owner != record_id {
                return Err(bad_request(format!(
                    "rename target `{new_canonical}` collides with an existing value"
                )));
            }
        }
    }
    Ok(mappings)
}

fn canonical_text(plan: &Plan, text: &str) -> Result<String, ServiceError> {
    let analysis = analyze_text(&plan.rules, text).map_err(bad_request)?;
    let record = analysis
        .records
        .into_iter()
        .next()
        .ok_or_else(|| bad_request("replacement did not produce an analysis"))?;
    if !record.eligible || record.canonical.is_empty() {
        return Err(bad_request(
            "replacement is not eligible under target rules",
        ));
    }
    Ok(record.canonical)
}

fn complete_mappings(
    plan: &Plan,
    source: &BTreeMap<String, String>,
    target: &BTreeMap<String, String>,
) -> Result<Vec<StoredMapping>, ServiceError> {
    let mut mappings = requested_mappings(plan, source, target)?;
    let handled: BTreeSet<String> = mappings
        .iter()
        .map(|mapping| mapping.record_id.clone())
        .collect();
    for (record_id, new_canonical) in target {
        if handled.contains(record_id) {
            continue;
        }
        mappings.push(StoredMapping {
            record_id: record_id.clone(),
            old_canonical: source.get(record_id).cloned().unwrap_or_default(),
            new_canonical: new_canonical.clone(),
            action: "auto".into(),
        });
    }
    mappings.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    Ok(mappings)
}

fn alias_cycle(store: &Store, _plan: &Plan, mappings: &[StoredMapping]) -> Option<Vec<String>> {
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for migration in store.state().migrations.values() {
        for mapping in &migration.mappings {
            if mapping.old_canonical != mapping.new_canonical {
                edges
                    .entry(mapping.old_canonical.clone())
                    .or_default()
                    .insert(mapping.new_canonical.clone());
            }
        }
    }
    for mapping in mappings {
        if mapping.old_canonical != mapping.new_canonical {
            edges
                .entry(mapping.old_canonical.clone())
                .or_default()
                .insert(mapping.new_canonical.clone());
        }
    }
    let mut stack = Vec::new();
    let mut finished = BTreeSet::new();
    for start in edges.keys().cloned().collect::<Vec<_>>() {
        if dfs_cycle(&start, &edges, &mut finished, &mut stack) {
            return Some(stack);
        }
    }
    None
}

fn dfs_cycle(
    node: &str,
    edges: &BTreeMap<String, BTreeSet<String>>,
    finished: &mut BTreeSet<String>,
    stack: &mut Vec<String>,
) -> bool {
    if stack.iter().any(|item| item == node) {
        stack.push(node.to_owned());
        return true;
    }
    if finished.contains(node) {
        return false;
    }
    stack.push(node.to_owned());
    if let Some(next) = edges.get(node) {
        for child in next {
            if dfs_cycle(child, edges, finished, stack) {
                return true;
            }
        }
    }
    stack.pop();
    finished.insert(node.to_owned());
    false
}

fn plan_data(
    store: &Store,
    plan: &Plan,
) -> Result<(Analysis, Analysis, Vec<StoredMapping>), ServiceError> {
    let dataset = dataset_snapshot(store, plan)?;
    let source_analysis = analysis_for(&dataset, &plan.from_rules)?;
    let target_analysis = analysis_for(&dataset, &plan.rules)?;
    let source = canonical_by_record(&source_analysis);
    let target = canonical_by_record(&target_analysis);
    let mappings = complete_mappings(plan, &source, &target)?;
    Ok((source_analysis, target_analysis, mappings))
}

pub fn update_plan(
    store: &mut Store,
    plan_id: &str,
    request: UpdatePlanReq,
    idempotency_key: Option<String>,
) -> Result<serde_json::Value, ServiceError> {
    let existing = store
        .state()
        .plans
        .get(plan_id)
        .ok_or_else(|| not_found("plan not found"))?
        .clone();
    if existing.version != request.expected_version {
        return Err(ServiceError::new(
            409,
            "version-conflict",
            serde_json::json!({
                "current_version": existing.version,
                "supplied_version": request.expected_version,
                "server_plan": existing
            }),
        ));
    }
    let mut updated = existing.clone();
    for (bucket, decision) in &request.decisions {
        if !updated.groups.contains_key(bucket) {
            return Err(bad_request(format!("unknown conflict bucket `{bucket}`")));
        }
        updated.decisions.insert(
            bucket.clone(),
            GroupDecision {
                action: decision.action.clone(),
                primary_record_id: decision.primary_record_id.clone(),
                replacements: decision.replacements.clone(),
                keep_old_alias: decision.keep_old_alias,
            },
        );
    }
    let mut approved_mappings = Vec::new();
    if let Some(status) = &request.status {
        updated.status = match (updated.status, status.as_str()) {
            (PlanStatus::Draft, "approved") | (PlanStatus::Draft, "rejected") => {
                if status == "approved" {
                    let (_, _, mappings) = plan_data(store, &updated)?;
                    if updated.decisions.len() != updated.groups.len() {
                        return Err(bad_request("every conflict group needs a decision"));
                    }
                    if let Some(cycle) = alias_cycle(store, &updated, &mappings) {
                        return Err(ServiceError::new(
                            422,
                            "alias-cycle",
                            serde_json::json!({ "cycle": cycle }),
                        ));
                    }
                    approved_mappings = mappings;
                }
                if status == "approved" {
                    PlanStatus::Approved
                } else {
                    PlanStatus::Rejected
                }
            }
            (current, requested) => {
                return Err(ServiceError::new(
                    409,
                    "illegal-state-transition",
                    serde_json::json!({
                        "from": current,
                        "requested": requested
                    }),
                ))
            }
        };
    }
    let next_version = updated.version + 1;
    if updated.status == PlanStatus::Approved {
        updated.version = next_version;
        updated.baseline_mappings = approved_mappings;
    } else {
        updated.version = next_version;
    }
    let payload = serde_json::to_value(&request).map_err(|e| bad_request(e.to_string()))?;
    let response = plan_view(store, &updated)?;
    if let Some(replay) = append_command(
        store,
        idempotency_key.as_ref(),
        request_hash(&payload),
        Event::PlanUpdated { plan: updated },
        response.clone(),
    )? {
        return Ok(replay);
    }
    if idempotency_key.is_some() {
        return Ok(response);
    }
    plan_view(
        store,
        store
            .state()
            .plans
            .get(plan_id)
            .ok_or_else(|| conflict("plan vanished after append"))?,
    )
}

pub fn plan_view(store: &Store, plan: &Plan) -> Result<serde_json::Value, ServiceError> {
    let details = plan_data(store, plan);
    let mut view = serde_json::to_value(plan).unwrap_or_default();
    if let Ok((source_analysis, target_analysis, mappings)) = details {
        view["source_analysis"] = serde_json::to_value(source_analysis).unwrap_or_default();
        view["target_analysis"] = serde_json::to_value(target_analysis).unwrap_or_default();
        view["mappings"] = serde_json::to_value(mappings).unwrap_or_default();
    }
    Ok(view)
}

pub fn apply_plan(
    store: &mut Store,
    plan_id: &str,
    request: ApplyPlanReq,
    idempotency_key: Option<String>,
) -> Result<serde_json::Value, ServiceError> {
    let plan = store
        .state()
        .plans
        .get(plan_id)
        .ok_or_else(|| not_found("plan not found"))?
        .clone();
    if plan.version != request.expected_version {
        return Err(ServiceError::new(
            409,
            "version-conflict",
            serde_json::json!({
                "current_version": plan.version,
                "supplied_version": request.expected_version,
                "server_plan": plan
            }),
        ));
    }
    if plan.status == PlanStatus::Applied {
        return Ok(serde_json::json!({
            "plan_id": plan_id,
            "idempotent": true,
            "migration": store.state().migrations.get(plan_id)
        }));
    }
    if plan.status != PlanStatus::Approved {
        return Err(ServiceError::new(
            409,
            "illegal-state-transition",
            serde_json::json!({"from": plan.status, "requested": "applied"}),
        ));
    }
    let (_, _, computed_mappings) = plan_data(store, &plan)?;
    let baseline_mappings = if plan.baseline_mappings.is_empty() {
        computed_mappings
    } else {
        plan.baseline_mappings.clone()
    };
    let dataset = store
        .state()
        .datasets
        .get(&plan.dataset_id)
        .ok_or_else(|| not_found("dataset not found"))?
        .clone();
    let current_analysis = analysis_for(&dataset, &plan.rules)?;
    let current_targets = canonical_by_record(&current_analysis);
    let baseline_ids: BTreeSet<String> = baseline_mappings
        .iter()
        .map(|mapping| mapping.record_id.clone())
        .collect();
    let mut registry: BTreeMap<String, String> = BTreeMap::new();
    for migration in store.state().migrations.values() {
        for mapping in &migration.mappings {
            registry.insert(mapping.record_id.clone(), mapping.new_canonical.clone());
        }
    }
    for mapping in &baseline_mappings {
        if let Some(existing) = registry.get(&mapping.record_id) {
            if existing != &mapping.new_canonical {
                return Err(ServiceError::new(
                    409,
                    "migration-conflict",
                    serde_json::json!({
                        "reason": "record already migrated to another canonical value",
                        "record_id": mapping.record_id,
                        "existing": existing
                    }),
                ));
            }
        }
    }
    let mut collisions = Vec::new();
    let baseline_targets: BTreeSet<(String, String)> = baseline_mappings
        .iter()
        .map(|mapping| (mapping.record_id.clone(), mapping.new_canonical.clone()))
        .collect();
    for bucket in &current_analysis.buckets {
        if bucket
            .record_ids
            .iter()
            .any(|id| !baseline_ids.contains(id))
            && bucket.record_ids.len() > 1
        {
            collisions.push(serde_json::json!({
                "canonical": bucket.canonical,
                "record_ids": bucket.record_ids
            }));
        }
    }
    for (record_id, canonical) in &current_targets {
        if !baseline_ids.contains(record_id) {
            if registry.values().any(|value| value == canonical)
                || baseline_mappings
                    .iter()
                    .any(|mapping| &mapping.new_canonical == canonical)
            {
                collisions.push(serde_json::json!({
                    "canonical": canonical,
                    "new_record_id": record_id,
                    "reason": "new record collides with migration baseline"
                }));
            }
        }
    }
    let _ = baseline_targets;
    if !collisions.is_empty() {
        return Err(ServiceError::new(
            409,
            "simulation-failed",
            serde_json::json!({
                "collisions": collisions,
                "difference_from_baseline": {
                    "dataset_version_at_plan": plan.dataset_version,
                    "current_dataset_version": dataset.version,
                    "added_records": dataset.records.values()
                        .filter(|record| record.source_version > plan.dataset_version)
                        .map(|record| serde_json::json!({"id": record.id, "source_version": record.source_version}))
                        .collect::<Vec<_>>()
                }
            }),
        ));
    }
    let migration = crate::store::AppliedMigration {
        plan_id: plan_id.to_owned(),
        dataset_id: plan.dataset_id.clone(),
        baseline_plan_version: plan.version,
        mappings: baseline_mappings.clone(),
    };
    let payload = serde_json::json!({"plan_id": plan_id, "expected_version": plan.version});
    let response = serde_json::json!({
        "applied": true,
        "migration": migration
    });
    if let Some(replay) = append_command(
        store,
        idempotency_key.as_ref(),
        request_hash(&payload),
        Event::PlanApplied {
            plan_id: plan_id.to_owned(),
            migration,
        },
        response.clone(),
    )? {
        return Ok(replay);
    }
    Ok(response)
}

pub fn analyze_query(query: String, rules: RuleConfig) -> Result<serde_json::Value, ServiceError> {
    let analysis = analyze_text(&rules, &query).map_err(bad_request)?;
    Ok(serde_json::json!({
        "query_original": query,
        "analysis": analysis
    }))
}

pub fn list_plans(store: &Store) -> serde_json::Value {
    serde_json::json!({
        "plans": store.state().plans.values().cloned().collect::<Vec<_>>()
    })
}

pub fn get_plan(store: &Store, plan_id: &str) -> Result<serde_json::Value, ServiceError> {
    let plan = store
        .state()
        .plans
        .get(plan_id)
        .ok_or_else(|| not_found("plan not found"))?;
    plan_view(store, plan)
}

pub fn export_plan(store: &Store, plan_id: &str) -> Result<(String, Vec<u8>), ServiceError> {
    let plan = store
        .state()
        .plans
        .get(plan_id)
        .ok_or_else(|| not_found("plan not found"))?
        .clone();
    let (source_analysis, target_analysis, mappings) = plan_data(store, &plan)?;
    let document = serde_json::json!({
        "format": "unicode-collision-workbench/export/v1",
        "rule_snapshot": target_analysis.rules,
        "source_rule_snapshot": source_analysis.rules,
        "plan": {
            "id": plan.id,
            "dataset_id": plan.dataset_id,
            "dataset_version": plan.dataset_version,
            "status": plan.status,
            "version": plan.version,
            "decisions": plan.decisions
        },
        "mappings": mappings,
        "records": target_analysis.records
    });
    let canonical = serde_json::to_vec(&document).map_err(|e| bad_request(e.to_string()))?;
    let hash = stable_id("sha256placeholder", &[&String::from_utf8_lossy(&canonical)]);
    let mut wrapped = document;
    wrapped["export_hash_fnv1a_64"] = serde_json::Value::String(hash);
    let bytes = serde_json::to_vec_pretty(&wrapped).map_err(|e| bad_request(e.to_string()))?;
    Ok((format!("plan-{plan_id}.json"), bytes))
}
