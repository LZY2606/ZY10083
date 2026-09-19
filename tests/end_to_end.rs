use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    address: String,
    data_dir: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "unicode-workbench-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start_server(name: &str) -> Server {
    let data_dir = temp_dir(name);
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .args(["--listen", "127.0.0.1:0", "--data-dir"])
        .arg(&data_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let output = child.stdout.take().unwrap();
    let address = read_address(output);
    Server {
        child,
        address,
        data_dir,
    }
}

fn read_address(mut output: impl Read) -> String {
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut reader = BufReader::new(&mut output);
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.contains("listening on ") {
            return line
                .trim()
                .strip_prefix("listening on ")
                .unwrap()
                .strip_prefix("http://")
                .unwrap()
                .to_owned();
        }
    }
    panic!("server did not print listen address; last line: {line}")
}

fn request(
    address: &str,
    method: &str,
    path: &str,
    body: &str,
    key: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let mut headers = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if let Some(key) = key {
        headers.push_str(&format!("Idempotency-Key: {key}\r\n"));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = String::from_utf8_lossy(&response);
    let (head, body_text) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, body_text.to_string())
}

fn get(address: &str, path: &str) -> (u16, serde_json::Value) {
    let (status, body) = request(address, "GET", path, "", None);
    (status, serde_json::from_str(&body).unwrap())
}

fn post(
    address: &str,
    path: &str,
    value: serde_json::Value,
    key: Option<&str>,
) -> (u16, serde_json::Value) {
    let body = serde_json::to_string(&value).unwrap();
    let (status, text) = request(address, "POST", path, &body, key);
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::json!({"raw":text})),
    )
}

fn rules() -> serde_json::Value {
    serde_json::json!({"unicode_table":"u16-0","normalization":"NFC","case_fold":true})
}

fn create_demo(address: &str) -> String {
    let payload = serde_json::json!({
        "name": "demo",
        "records": [
            {"id":"r1","text":"Foo"},
            {"id":"r2","text":"FOO"},
            {"id":"r3","text":"Straße"},
            {"id":"r4","text":"STRASSE"}
        ]
    });
    let (status, body) = post(address, "/api/datasets", payload, Some("demo-key"));
    assert_eq!(status, 200, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

#[test]
fn duplicate_import_request_is_idempotent() {
    let server = start_server("idempotent");
    let first_id = create_demo(&server.address);
    let second_id = create_demo(&server.address);
    assert_eq!(first_id, second_id);
    let (status, list) = get(&server.address, "/api/datasets");
    assert_eq!(status, 200);
    assert_eq!(
        list["datasets"][&first_id]["records"]
            .as_object()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn rejects_illegal_plan_state_transition() {
    let server = start_server("states");
    let dataset = create_demo(&server.address);
    let payload = serde_json::json!({"dataset_id":dataset,"expected_dataset_version":1,"from_rules":{"unicode_table":"u15-1"},"rules":rules()});
    let (status, plan) = post(&server.address, "/api/plans", payload, None);
    assert_eq!(status, 200, "{plan}");
    let plan_id = plan["id"].as_str().unwrap();
    let apply = serde_json::json!({"expected_version":1});
    let (status, error) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/apply"),
        apply.clone(),
        None,
    );
    assert_eq!(status, 409);
    assert_eq!(error["error"], "illegal-state-transition");
}

#[test]
fn recovers_events_after_hard_exit() {
    let mut server = start_server("recovery");
    let dataset = create_demo(&server.address);
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .args(["--listen", "127.0.0.1:0", "--data-dir"])
        .arg(&server.data_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let address = read_address(child.stdout.take().unwrap());
    let (status, body) = get(&address, &format!("/api/datasets/{dataset}"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dataset"]["version"], 1);
    let replay_payload = serde_json::json!({
        "name": "demo",
        "records": [
            {"id":"r1","text":"Foo"},
            {"id":"r2","text":"FOO"},
            {"id":"r3","text":"Straße"},
            {"id":"r4","text":"STRASSE"}
        ]
    });
    let (status, replay) = post(&address, "/api/datasets", replay_payload, Some("demo-key"));
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["id"], dataset);
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn deterministic_export_contains_snapshot_mappings_and_decisions() {
    let server = start_server("export");
    let dataset = create_demo(&server.address);
    let plan_payload = serde_json::json!({"dataset_id":dataset,"expected_dataset_version":1,"from_rules":{"unicode_table":"u15-1"},"rules":rules()});
    let (_, plan) = post(&server.address, "/api/plans", plan_payload, None);
    let plan_id = plan["id"].as_str().unwrap().to_owned();
    let decisions = plan["groups"]
        .as_object()
        .unwrap()
        .keys()
        .map(|bucket| {
            let records = plan["groups"][bucket].as_array().unwrap();
            let _primary = records[0].as_str().unwrap();
            (
                bucket.clone(),
                serde_json::json!({
                    "action":"rename",
                    "replacements": records.iter().enumerate()
                        .map(|(i, id)| (id.as_str().unwrap().to_owned(), serde_json::Value::String(format!("renamed-{i}-{bucket}"))))
                        .collect::<serde_json::Map<_,_>>(),
                    "keep_old_alias": false
                }),
            )
        })
        .collect::<serde_json::Map<_,_>>();
    let update =
        serde_json::json!({"expected_version":1,"status":"approved","decisions":decisions});
    let (status, updated) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/decisions"),
        update,
        None,
    );
    assert_eq!(status, 200, "{updated}");
    let (_, first) = request(
        &server.address,
        "GET",
        &format!("/api/plans/{plan_id}/export"),
        "",
        None,
    );
    let (_, second) = request(
        &server.address,
        "GET",
        &format!("/api/plans/{plan_id}/export"),
        "",
        None,
    );
    assert_eq!(first, second);
    let value: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(value["rule_snapshot"]["unicode_version"], "16.0.0");
    assert_eq!(value["source_rule_snapshot"]["unicode_version"], "15.1.0");
    assert!(value["mappings"].is_array());
    assert!(value["export_hash_fnv1a_64"].is_string());
}

#[test]
fn new_collision_makes_apply_all_or_nothing() {
    let server = start_server("apply");
    let dataset = create_demo(&server.address);
    let plan_payload = serde_json::json!({"dataset_id":dataset,"expected_dataset_version":1,"from_rules":{"unicode_table":"u15-1"},"rules":rules()});
    let (_, plan) = post(&server.address, "/api/plans", plan_payload, None);
    let plan_id = plan["id"].as_str().unwrap().to_owned();
    let decisions = plan["groups"]
        .as_object()
        .unwrap()
        .keys()
        .map(|bucket| {
            let primary = plan["groups"][bucket][0].as_str().unwrap();
            (
                bucket.clone(),
                serde_json::json!({"action":"keep-alias","primary_record_id":primary}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let update =
        serde_json::json!({"expected_version":1,"status":"approved","decisions":decisions});
    let (status, approved) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/decisions"),
        update,
        None,
    );
    assert_eq!(status, 200, "{approved}");
    let append = serde_json::json!({"expected_version":1,"records":[{"id":"new","text":"foo"}]});
    let (status, appended) = post(
        &server.address,
        &format!("/api/datasets/{dataset}/records"),
        append,
        None,
    );
    assert_eq!(status, 200, "{appended}");
    let apply = serde_json::json!({"expected_version":2});
    let (status, failure) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/apply"),
        apply,
        None,
    );
    assert_eq!(status, 409);
    assert_eq!(failure["error"], "simulation-failed");
    assert_eq!(
        failure["details"]["difference_from_baseline"]["current_dataset_version"],
        2
    );
    let (_, plan_check) = get(&server.address, &format!("/api/plans/{plan_id}"));
    assert_eq!(plan_check["status"], "approved");
}

#[test]
fn stale_concurrent_write_returns_both_differences() {
    let server = start_server("concurrency");
    let dataset = create_demo(&server.address);
    let first = serde_json::json!({"expected_version":1,"records":[{"id":"new-a","text":"A"}]});
    let second = serde_json::json!({"expected_version":1,"records":[{"id":"new-b","text":"B"}]});
    let (status, first_body) = post(
        &server.address,
        &format!("/api/datasets/{dataset}/records"),
        first,
        None,
    );
    assert_eq!(status, 200, "{first_body}");
    let (status, second_body) = post(
        &server.address,
        &format!("/api/datasets/{dataset}/records"),
        second,
        None,
    );
    assert_eq!(status, 409);
    assert_eq!(second_body["error"], "version-conflict");
    assert_eq!(second_body["details"]["current_version"], 2);
    assert_eq!(second_body["details"]["supplied_version"], 1);
    assert!(second_body["details"]["server_records"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "new-a"));
}

#[test]
fn approved_plan_without_conflicts_applies_atomically() {
    let server = start_server("apply-ok");
    let payload = serde_json::json!({
        "name":"unique",
        "records":[{"id":"a","text":"alpha"},{"id":"b","text":"beta"}]
    });
    let (status, dataset) = post(
        &server.address,
        "/api/datasets",
        payload,
        Some("unique-key"),
    );
    assert_eq!(status, 200, "{dataset}");
    let dataset_id = dataset["id"].as_str().unwrap();
    let plan_payload = serde_json::json!({"dataset_id":dataset_id,"expected_dataset_version":1,"from_rules":{"unicode_table":"u15-1"},"rules":rules()});
    let (status, plan) = post(&server.address, "/api/plans", plan_payload, None);
    assert_eq!(status, 200, "{plan}");
    let plan_id = plan["id"].as_str().unwrap().to_owned();
    let update = serde_json::json!({"expected_version":1,"status":"approved","decisions":{}});
    let (status, approved) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/decisions"),
        update,
        None,
    );
    assert_eq!(status, 200, "{approved}");
    let apply = serde_json::json!({"expected_version":2});
    let (status, applied) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/apply"),
        apply.clone(),
        None,
    );
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["applied"], true);
    let (status, again) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/apply"),
        apply,
        None,
    );
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["idempotent"], true);
}

#[test]
fn alias_cycle_is_rejected_on_approval() {
    let server = start_server("alias-cycle");
    let dataset = create_demo(&server.address);
    let plan_payload = serde_json::json!({"dataset_id":dataset,"expected_dataset_version":1,"from_rules":{"unicode_table":"u15-1"},"rules":rules()});
    let (_, plan) = post(&server.address, "/api/plans", plan_payload, None);
    let plan_id = plan["id"].as_str().unwrap().to_owned();
    let decisions = serde_json::json!({
        "foo": {
            "action": "rename",
            "keep_old_alias": true,
            "replacements": {"r1": "strasse", "r2": "x-foo"}
        },
        "strasse": {
            "action": "rename",
            "keep_old_alias": true,
            "replacements": {"r3": "foo", "r4": "x-strasse"}
        }
    });
    let update =
        serde_json::json!({"expected_version":1,"status":"approved","decisions":decisions});
    let (status, error) = post(
        &server.address,
        &format!("/api/plans/{plan_id}/decisions"),
        update,
        None,
    );
    assert_eq!(status, 422, "{error}");
    assert_eq!(error["error"], "alias-cycle");
}
