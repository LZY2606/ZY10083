mod common;
use common::*;

fn make_ruleset(addr: &str) -> u64 {
    let body =
        r#"{"label":"r","case_fold":true,"normalization":"NFC","strip_default_ignorable":true}"#;
    let r = request(addr, "POST", "/api/rulesets", Some(body), None).unwrap();
    assert_eq!(r.status, 201);
    j(&r.body)["id"].as_u64().unwrap()
}

fn make_dataset(addr: &str) -> u64 {
    let body = serde_json::json!({
        "label": "d",
        "text": "1\tA\u{030a}NGSTR\u{00d6}M\n2\t\u{00c5}ngstr\u{00f6}m\n3\tx\n4\tX"
    })
    .to_string();
    let r = request(addr, "POST", "/api/datasets", Some(&body), Some("key-1")).unwrap();
    assert_eq!(r.status, 201);
    j(&r.body)["id"].as_u64().unwrap()
}

fn make_analysis(addr: &str, ds: u64, rs: u64) -> serde_json::Value {
    let body = serde_json::json!({"dataset_id": ds, "ruleset_id": rs}).to_string();
    let r = request(addr, "POST", "/api/analyses", Some(&body), None).unwrap();
    assert_eq!(r.status, 201);
    j(&r.body)
}

#[test]
fn duplicate_request_returns_same_result() {
    let mut s = Server::start();
    let body = serde_json::json!({"label":"dup","text":"a\tA\nb\tB"}).to_string();
    let r1 = request(
        &s.addr,
        "POST",
        "/api/datasets",
        Some(&body),
        Some("idem-77"),
    )
    .unwrap();
    let r2 = request(
        &s.addr,
        "POST",
        "/api/datasets",
        Some(&body),
        Some("idem-77"),
    )
    .unwrap();
    assert_eq!(r1.status, 201);
    assert_eq!(r2.status, 201);
    assert_eq!(r1.body, r2.body);
    // Without the key, a second import of the same body creates a new dataset.
    let r3 = request(&s.addr, "POST", "/api/datasets", Some(&body), None).unwrap();
    assert_ne!(j(&r3.body)["id"], j(&r1.body)["id"]);
    s.kill();
}

#[test]
fn end_to_end_collision_and_resolution() {
    let mut s = Server::start();
    let rs = make_ruleset(&s.addr);
    let ds = make_dataset(&s.addr);
    let an = make_analysis(&s.addr, ds, rs);
    let aid = an["id"].as_u64().unwrap();
    // Records 1 & 2 collide under fold+NFC; 3 & 4 collide too.
    let conflicts = an["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["record_nos"].as_array().unwrap().len() > 1)
        .count();
    assert_eq!(conflicts, 2);

    let created = request(
        &s.addr,
        "POST",
        "/api/plans",
        Some(&serde_json::json!({"analysis_id": aid}).to_string()),
        None,
    )
    .unwrap();
    assert_eq!(created.status, 201);
    let pid = j(&created.body)["id"].as_u64().unwrap();

    // Deciding every conflicting group is required: first simulation fails.
    let sim = request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/simulate"),
        Some(r#"{"extra_records":[]}"#),
        None,
    )
    .unwrap();
    assert_eq!(sim.status, 200);
    assert_eq!(j(&sim.body)["ok"], serde_json::json!(false));

    // Fetch plan and set rename decisions on every conflict bucket.
    let plan = request(&s.addr, "GET", &format!("/api/plans/{pid}"), None, None).unwrap();
    let pv = j(&plan.body)["version"].as_u64().unwrap();
    let buckets = j(&plan.body)["buckets"].as_array().unwrap().clone();
    let mut decisions = serde_json::Map::new();
    for b in &buckets {
        if b["record_nos"].as_array().unwrap().len() > 1 {
            // Bucket hex is derived from canonical UTF-8 bytes; assign the new
            // value by inspecting the canonical value itself.
            let new_value = if b["canonical"].as_str().unwrap().contains('x') {
                "x-v2"
            } else {
                "angstrom-v2"
            };
            decisions.insert(
                b["bucket_hex"].as_str().unwrap().to_string(),
                serde_json::json!({
                    "action":"rename",
                    "new_value": new_value,
                    "keep_old_aliases": true
                }),
            );
        }
    }
    let put = request(
        &s.addr,
        "PUT",
        &format!("/api/plans/{pid}/decisions"),
        Some(&serde_json::json!({"base_version": pv, "decisions": decisions}).to_string()),
        None,
    )
    .unwrap();
    assert_eq!(put.status, 200);

    // Simulation with an extra colliding record must fail all-or-nothing.
    let bad = request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/simulate"),
        Some(r#"{"extra_records":["ANGSTROM-V2"]}"#),
        None,
    )
    .unwrap();
    assert_eq!(j(&bad.body)["ok"], false);

    // Clean simulation then approve.
    let ok_sim = request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/simulate"),
        Some(r#"{"extra_records":[]}"#),
        None,
    )
    .unwrap();
    assert_eq!(j(&ok_sim.body)["ok"], true);

    let pv2 = j(
        &request(&s.addr, "GET", &format!("/api/plans/{pid}"), None, None)
            .unwrap()
            .body,
    )["version"]
        .as_u64()
        .unwrap();
    let appr = request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/approve"),
        Some(
            &serde_json::json!({"base_version": pv2, "extra_records": [], "commit": true})
                .to_string(),
        ),
        None,
    )
    .unwrap();
    assert_eq!(appr.status, 200, "{}", appr.body);

    // Re-approving is an illegal state transition.
    let again = request(
        &s.addr,
        "POST",
        &format!("/api/plans/{pid}/approve"),
        Some(r#"{"commit":true}"#),
        None,
    )
    .unwrap();
    assert_eq!(again.status, 400);
    assert!(j(&again.body)["error"]["code"]
        .as_str()
        .unwrap()
        .contains("bad"));

    // Editing an approved plan is rejected as an illegal transition.
    let edit = request(
        &s.addr,
        "PUT",
        &format!("/api/plans/{pid}/decisions"),
        Some(r#"{"base_version":1,"decisions":{}}"#),
        None,
    )
    .unwrap();
    assert_eq!(edit.status, 400);

    // Export exists and is deterministic.
    let e1 = request(
        &s.addr,
        "GET",
        &format!("/api/plans/{pid}/export"),
        None,
        None,
    )
    .unwrap();
    let e2 = request(
        &s.addr,
        "GET",
        &format!("/api/plans/{pid}/export"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(e1.body, e2.body);
    assert!(j(&e1.body)["sha256"].as_str().unwrap().len() == 64);

    s.kill();
}

#[test]
fn stale_write_returns_both_sides_diff() {
    let mut s = Server::start();
    let rs = make_ruleset(&s.addr);
    let ds = make_dataset(&s.addr);
    let an = make_analysis(&s.addr, ds, rs);
    let aid = an["id"].as_u64().unwrap();
    let created = request(
        &s.addr,
        "POST",
        "/api/plans",
        Some(&serde_json::json!({"analysis_id": aid}).to_string()),
        None,
    )
    .unwrap();
    let pid = j(&created.body)["id"].as_u64().unwrap();
    let plan = request(&s.addr, "GET", &format!("/api/plans/{pid}"), None, None).unwrap();
    let v0 = j(&plan.body)["version"].as_u64().unwrap();

    // Writer A updates at v0.
    let a = request(
        &s.addr,
        "PUT",
        &format!("/api/plans/{pid}/decisions"),
        Some(&serde_json::json!({"base_version": v0, "decisions":{}}).to_string()),
        None,
    )
    .unwrap();
    assert_eq!(a.status, 200);
    // Writer B, still holding v0, must get 409 with diffs, not silent overwrite.
    let b = request(
        &s.addr,
        "PUT",
        &format!("/api/plans/{pid}/decisions"),
        Some(&serde_json::json!({"base_version": v0, "decisions":{}}).to_string()),
        None,
    )
    .unwrap();
    assert_eq!(b.status, 409, "{}", b.body);
    let d = &j(&b.body)["error"]["details"];
    assert_eq!(d["base_version"], serde_json::json!(v0));
    assert_eq!(d["current_version"], serde_json::json!(v0 + 1));
    assert!(d["changes_since_base"].as_array().unwrap().len() >= 1);
    s.kill();
}

#[test]
fn draft_cannot_be_exported() {
    let mut s = Server::start();
    let rs = make_ruleset(&s.addr);
    let ds = make_dataset(&s.addr);
    let an = make_analysis(&s.addr, ds, rs);
    let aid = an["id"].as_u64().unwrap();
    let pid = j(&request(
        &s.addr,
        "POST",
        "/api/plans",
        Some(&serde_json::json!({"analysis_id": aid}).to_string()),
        None,
    )
    .unwrap()
    .body)["id"]
        .as_u64()
        .unwrap();
    let e = request(
        &s.addr,
        "GET",
        &format!("/api/plans/{pid}/export"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(e.status, 400);
    s.kill();
}

#[test]
fn simulate_on_missing_plan_is_404() {
    let mut s = Server::start();
    let r = request(
        &s.addr,
        "POST",
        "/api/plans/999/simulate",
        Some(r#"{"extra_records":[]}"#),
        None,
    )
    .unwrap();
    assert_eq!(r.status, 404);
    s.kill();
}
