mod common;
use common::*;

fn setup(addr: &str, text: &str) -> (u64, serde_json::Value) {
    let rs = j(&request(addr, "POST", "/api/rulesets",
        Some(r#"{"label":"r","case_fold":true,"normalization":"NFC","strip_default_ignorable":true}"#),
        None).unwrap().body);
    let body = serde_json::json!({"label":"d","text": text}).to_string();
    let ds = j(&request(addr, "POST", "/api/datasets", Some(&body), None)
        .unwrap()
        .body);
    let an = j(&request(
        addr,
        "POST",
        "/api/analyses",
        Some(&serde_json::json!({"dataset_id": ds["id"], "ruleset_id": rs["id"]}).to_string()),
        None,
    )
    .unwrap()
    .body);
    (rs["id"].as_u64().unwrap(), an)
}

fn create_plan(addr: &str, analysis_id: u64) -> u64 {
    j(&request(
        addr,
        "POST",
        "/api/plans",
        Some(&serde_json::json!({"analysis_id": analysis_id}).to_string()),
        None,
    )
    .unwrap()
    .body)["id"]
        .as_u64()
        .unwrap()
}

#[test]
fn invalid_ruleset_scripts_are_rejected() {
    let mut s = Server::start();
    let r = request(
        &s.addr,
        "POST",
        "/api/rulesets",
        Some(r#"{"label":"r","case_fold":false,"normalization":"NFC","restrict_script":"WXYZ"}"#),
        None,
    )
    .unwrap();
    assert_eq!(r.status, 400);
    s.kill();
}

#[test]
fn duplicate_import_record_numbers_rejected() {
    let mut s = Server::start();
    let r = request(
        &s.addr,
        "POST",
        "/api/datasets",
        Some(&serde_json::json!({"label":"d","text":"a\tA\na\tB"}).to_string()),
        None,
    )
    .unwrap();
    assert_eq!(r.status, 400);
    s.kill();
}

#[test]
fn alias_cycles_are_reported_and_block_approval() {
    let mut s = Server::start();
    // Two distinct collision groups; cross-renaming targets at each other's
    // old values while keeping aliases creates a directed cycle.
    let (_, an) = setup(&s.addr, "1\tAB\n2\tab\n3\tCD\n4\tcd");
    let aid = an["id"].as_u64().unwrap();
    let pid = create_plan(&s.addr, aid);
    let plan = j(
        &request(&s.addr, "GET", &format!("/api/plans/{pid}"), None, None)
            .unwrap()
            .body,
    );
    let mut decisions = serde_json::Map::new();
    let buckets: Vec<_> = plan["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["record_nos"].as_array().unwrap().len() > 1)
        .cloned()
        .collect();
    assert_eq!(buckets.len(), 2);
    let (ab, cd) = (
        buckets[0]["canonical"].as_str().unwrap(),
        buckets[1]["canonical"].as_str().unwrap(),
    );
    decisions.insert(
        buckets[0]["bucket_hex"].as_str().unwrap().to_string(),
        serde_json::json!({"action":"rename","new_value": cd,"keep_old_aliases": true}),
    );
    decisions.insert(
        buckets[1]["bucket_hex"].as_str().unwrap().to_string(),
        serde_json::json!({"action":"rename","new_value": ab,"keep_old_aliases": true}),
    );
    let put = request(
        &s.addr,
        "PUT",
        &format!("/api/plans/{pid}/decisions"),
        Some(&serde_json::json!({"decisions": decisions}).to_string()),
        None,
    )
    .unwrap();
    assert_eq!(put.status, 200);

    let sim = j(&request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/simulate"),
        Some(r#"{"extra_records":[]}"#),
        None,
    )
    .unwrap()
    .body);
    assert_eq!(sim["ok"], false);
    assert!(
        sim["cycles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.as_array().unwrap().len() >= 2),
        "cycle reported: {sim}"
    );

    let appr = request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/approve"),
        Some(r#"{"commit":true}"#),
        None,
    )
    .unwrap();
    assert_eq!(appr.status, 409, "cycle must block all-or-nothing commit");
    s.kill();
}

#[test]
fn visual_lookalikes_do_not_collapse() {
    let mut s = Server::start();
    let (_, an) = setup(&s.addr, "1\ta\n2\tа"); // Latin a vs Cyrillic а
    let singletons = an["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["record_nos"].as_array().unwrap().len() == 1)
        .count();
    assert_eq!(singletons, 2, "lookalikes stay in separate buckets");
    // And the cyrillic record is flagged via its script set (report, not merge).
    let detail = request(
        &s.addr,
        "GET",
        &format!("/api/analyses/{}", an["id"].as_u64().unwrap()),
        None,
        None,
    )
    .unwrap();
    let detail_body = j(&detail.body);
    let recs = detail_body["records"].as_array().unwrap();
    assert!(recs
        .iter()
        .any(|r| r["scripts"].as_array().unwrap().iter().any(|x| x == "Cyrl")));
    s.kill();
}

#[test]
fn approved_plan_is_not_recomputed_on_upgrade_but_comparable() {
    let mut s = Server::start();
    let (_, an1) = setup(&s.addr, "1\tx\u{00b2}"); // superscript 2; NFKC -> "x2"
    let pid = create_plan(&s.addr, an1["id"].as_u64().unwrap());
    // No conflicts: approval works with zero decisions.
    let appr = request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/approve"),
        Some(r#"{"commit":true}"#),
        None,
    )
    .unwrap();
    assert_eq!(appr.status, 200, "{}", appr.body);

    // A newer analysis under another ruleset can be compared; the approved
    // mapping itself stays pinned to its rule snapshot.
    let up = request(&s.addr, "POST", "/api/rulesets",
        Some(r#"{"label":"r2","case_fold":true,"normalization":"NFKC","strip_default_ignorable":true}"#),
        None).unwrap();
    assert_eq!(up.status, 201);
    let rs2 = j(&up.body)["id"].as_u64().unwrap();
    let an2 = j(&request(
        &s.addr,
        "POST",
        "/api/analyses",
        Some(&serde_json::json!({"dataset_id": an1["dataset_id"], "ruleset_id": rs2}).to_string()),
        None,
    )
    .unwrap()
    .body);
    let cmp = j(&request(
        &s.addr,
        "GET",
        &format!(
            "/api/compare?a={}&b={}",
            an1["id"].as_u64().unwrap(),
            an2["id"].as_u64().unwrap()
        ),
        None,
        None,
    )
    .unwrap()
    .body);
    assert!(cmp["changed_records"].as_array().unwrap().len() >= 1);
    let exported = j(&request(
        &s.addr,
        "GET",
        &format!("/api/plans/{pid}/export"),
        None,
        None,
    )
    .unwrap()
    .body);
    assert_eq!(
        exported["document"]["rule_snapshot"]["config"]["normalization"],
        "nfc"
    );
    s.kill();
}
