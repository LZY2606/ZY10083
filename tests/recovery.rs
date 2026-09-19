mod common;
use common::*;

/// Abrupt process death (SIGKILL-style kill) followed by reopening the same
/// data directory must recover all committed state from snapshot + WAL and
/// never double-apply events during replay (versions stay correct).
#[test]
fn recovers_after_abnormal_exit() {
    let mut s = Server::start();
    let rs = j(&request(&s.addr, "POST", "/api/rulesets",
        Some(r#"{"label":"r","case_fold":true,"normalization":"NFKC","strip_default_ignorable":false}"#),
        None).unwrap().body);
    let rs_id = rs["id"].as_u64().unwrap();

    // Import several datasets to cross the compaction threshold too.
    for n in 0..70u32 {
        let body = serde_json::json!({
            "label": format!("d{n}"),
            "records": [{"record_no": "1", "text": format!("Value {n}")}]
        })
        .to_string();
        let r = request(
            &s.addr,
            "POST",
            "/api/datasets",
            Some(&body),
            Some(&format!("k{n}")),
        )
        .unwrap();
        assert_eq!(r.status, 201);
    }
    // Idempotency must still serve the stored response after many events.
    let again = request(
        &s.addr,
        "POST",
        "/api/datasets",
        Some(
            &serde_json::json!({"label":"d0","records":[{"record_no":"1","text":"Value 0"}]})
                .to_string(),
        ),
        Some("k0"),
    )
    .unwrap();
    assert_eq!(
        j(&again.body)["id"],
        serde_json::json!(1),
        "same idempotency result"
    );

    let state_before = request(&s.addr, "GET", "/api/state", None, None).unwrap();
    let next_id_before = j(&state_before.body)["next_id"].as_u64().unwrap();

    // Abrupt kill, no graceful shutdown.
    s.kill();

    // Reopen on the same directory with a brand new process.
    let s2 = common::start_on(&s.dir);
    let state_after = request(&s2.addr, "GET", "/api/state", None, None).unwrap();
    let sa = j(&state_after.body);
    assert_eq!(
        sa["next_id"],
        serde_json::json!(next_id_before),
        "no id reuse/double replay"
    );
    assert_eq!(sa["datasets"].as_array().unwrap().len(), 70);
    let ruleset = &sa["rulesets"].as_array().unwrap()[0];
    assert_eq!(ruleset["config"]["normalization"], "nfkc");
    assert_eq!(ruleset["id"], serde_json::json!(rs_id));

    // A new write after recovery works and continues the id sequence.
    let r = request(
        &s2.addr,
        "POST",
        "/api/datasets",
        Some(r#"{"label":"after","text":"x\tY"}"#),
        None,
    )
    .unwrap();
    assert_eq!(r.status, 201);
    assert!(j(&r.body)["id"].as_u64().unwrap() >= next_id_before);
}
