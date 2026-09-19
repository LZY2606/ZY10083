//! Minimal HTTP/1.1 server using only the standard library. The browser is a
//! view over the server-side rules; this module is deliberately thin.

use crate::service::{
    ApiError, App, ApproveReq, BuildAnalysisReq, DecisionsReq, ImportReq, RulesetReq,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MAX_BODY: usize = 8 * 1024 * 1024;

pub struct HttpServer {
    app: Arc<App>,
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn idempotency_key(&self) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("idempotency-key"))
            .map(|(_, v)| v.clone())
    }
}

impl HttpServer {
    pub fn new(app: App) -> Self {
        HttpServer { app: Arc::new(app) }
    }

    pub fn run(&self, listen: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(listen)?;
        eprintln!("listening on http://{listen}");
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let app = self.app.clone();
                    thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                        if let Err(e) = handle_connection(&app, stream) {
                            eprintln!("connection error: {e}");
                        }
                    });
                }
                Err(e) => eprintln!("accept error: {e}"),
            }
        }
        Ok(())
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed",
            ));
        }
        buf.push(byte[0]);
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "headers too large",
            ));
        }
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut headers = HashMap::new();
    for line in lines.filter(|l| !l.is_empty()) {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    let mut body = Vec::new();
    if let Some(len) = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > MAX_BODY {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "body too large",
            ));
        }
        let mut rest = vec![0u8; len];
        stream.read_exact(&mut rest)?;
        body = rest;
    }

    Ok(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

fn respond_json(
    stream: &mut TcpStream,
    status: u16,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    respond_raw(stream, status, "application/json; charset=utf-8", &body)
}

fn respond_raw(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = reason_phrase(status);
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn err_response(stream: &mut TcpStream, e: &ApiError) -> std::io::Result<()> {
    respond_json(stream, e.status, &e.json())
}

fn parse_json<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))
}

fn handle_connection(app: &App, mut stream: TcpStream) -> std::io::Result<()> {
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e),
    };

    let result = route(app, &req, &mut stream);
    if let Err(e) = result {
        let _ = err_response(&mut stream, &e);
    }
    Ok(())
}

fn route(app: &App, req: &Request, stream: &mut TcpStream) -> Result<(), ApiError> {
    let p = req.path.as_str();

    // Static entry point.
    if req.method == "GET" && (p == "/" || p == "/index.html") {
        let html = include_str!("../static/index.html");
        return respond_raw(stream, 200, "text/html; charset=utf-8", html.as_bytes())
            .map_err(io_to_api);
    }
    if req.method == "GET" && p == "/static/app.js" {
        let js = include_str!("../static/app.js");
        return respond_raw(
            stream,
            200,
            "application/javascript; charset=utf-8",
            js.as_bytes(),
        )
        .map_err(io_to_api);
    }
    if req.method == "GET" && p == "/static/styles.css" {
        let css = include_str!("../static/styles.css");
        return respond_raw(stream, 200, "text/css; charset=utf-8", css.as_bytes())
            .map_err(io_to_api);
    }

    match (req.method.as_str(), p) {
        ("GET", "/api/version") => {
            respond_json(
                stream,
                200,
                &serde_json::json!({
                    "tables": crate::store::current_tables(),
                }),
            )
            .map_err(io_to_api)?;
            Ok(())
        }
        ("GET", "/api/state") => {
            let g = app.store.lock();
            let s = g.state();
            respond_json(
                stream,
                200,
                &serde_json::json!({
                    "next_id": s.next_id,
                    "active_ruleset_id": s.active_ruleset_id,
                    "rulesets": sorted_values(&s.rulesets),
                    "datasets": s.datasets.values().map(dataset_summary).collect::<Vec<_>>(),
                    "analyses": s.analyses.values().map(analysis_summary).collect::<Vec<_>>(),
                    "plans": s.plans.values().map(plan_summary).collect::<Vec<_>>(),
                    "committed_count": s.committed.len(),
                }),
            )
            .map_err(io_to_api)?;
            Ok(())
        }
        ("POST", "/api/rulesets") => {
            let body: RulesetReq = parse_json(&req.body)?;
            let (status, rs) = app.create_ruleset(body)?;
            respond_json(stream, status, &serde_json::json!(rs)).map_err(io_to_api)?;
            Ok(())
        }
        ("POST", "/api/tables/upgrade") => {
            #[derive(Deserialize)]
            struct Up {
                #[serde(default)]
                note: String,
            }
            let body: Up = parse_json(&req.body)?;
            let (status, rs) = app.upgrade_tables(body.note)?;
            respond_json(stream, status, &serde_json::json!(rs)).map_err(io_to_api)?;
            Ok(())
        }
        ("POST", "/api/datasets") => {
            let body: ImportReq = parse_json(&req.body)?;
            let (status, v) = app.import_dataset(body, req.idempotency_key())?;
            respond_json(stream, status, &v).map_err(io_to_api)?;
            Ok(())
        }
        ("POST", "/api/analyses") => {
            let body: BuildAnalysisReq = parse_json(&req.body)?;
            let (status, an) = app.build_analysis(body)?;
            respond_json(stream, status, &serde_json::json!(an)).map_err(io_to_api)?;
            Ok(())
        }
        _ => route_dynamic(app, req, stream),
    }
}

fn sorted_values<T: serde::Serialize>(
    m: &std::collections::BTreeMap<crate::model::Id, T>,
) -> Vec<serde_json::Value> {
    m.values().map(|v| serde_json::json!(v)).collect()
}

fn dataset_summary(d: &crate::model::Dataset) -> serde_json::Value {
    serde_json::json!({"id": d.id, "label": d.label, "count": d.records.len()})
}

fn analysis_summary(a: &crate::model::Analysis) -> serde_json::Value {
    serde_json::json!({
        "id": a.id,
        "dataset_id": a.dataset_id,
        "ruleset_id": a.ruleset_id,
        "ruleset_revision": a.ruleset_revision,
        "buckets": a.buckets.len(),
        "conflict_buckets": a.buckets.iter().filter(|b| b.record_nos.len() > 1 && !b.identical_originals).count(),
    })
}

fn plan_summary(p: &crate::model::Plan) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "analysis_id": p.analysis_id,
        "status": p.status,
        "decisions": p.decisions.len(),
    })
}

fn io_to_api(e: std::io::Error) -> ApiError {
    ApiError {
        status: 500,
        code: "io".to_string(),
        message: e.to_string(),
        details: serde_json::Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Dynamic paths
// ---------------------------------------------------------------------------

fn query_params(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(urldecode(k), urldecode(v));
        } else {
            out.insert(urldecode(pair), String::new());
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hexval(bytes[i + 1]);
                let lo = hexval(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn route_dynamic(app: &App, req: &Request, stream: &mut TcpStream) -> Result<(), ApiError> {
    let p = req.path.as_str();
    let segs: Vec<&str> = p.trim_start_matches('/').split('/').collect();

    // GET /api/analyses/:id?stage=raw&issue=...&record=...
    if req.method == "GET" && segs.len() == 3 && segs[0] == "api" && segs[1] == "analyses" {
        let id: crate::model::Id = segs[2].parse().map_err(|_| ApiError::not_found("bad id"))?;
        let params = query_params(&req.query);
        let g = app.store.lock();
        let analysis = g
            .state()
            .analyses
            .get(&id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("analysis not found"))?;
        let stage = params.get("stage").cloned();
        let issue_only = params.contains_key("issue");
        let record = params.get("record").cloned();

        let records: Vec<_> = analysis
            .records
            .iter()
            .filter(|r| record.as_ref().is_none_or(|no| &r.record_no == no))
            .filter(|r| !issue_only || !r.trace.issues.is_empty())
            .map(|r| {
                let stages: Vec<_> = r
                    .trace
                    .stages
                    .iter()
                    .filter(|st| stage.as_ref().is_none_or(|name| &st.name == name))
                    .collect();
                serde_json::json!({
                    "record_no": r.record_no,
                    "raw_hex": crate::model::hex_bytes::to_hex(&r.raw),
                    "canonical": r.trace.canonical,
                    "canonical_cps": r.trace.canonical_cps,
                    "scripts": r.trace.scripts,
                    "mixed_script": r.trace.mixed_script,
                    "restriction_violated": r.trace.restriction_violated,
                    "issues": r.trace.issues,
                    "stages": stages,
                })
            })
            .collect();
        respond_json(
            stream,
            200,
            &serde_json::json!({
                "analysis": {
                    "id": analysis.id,
                    "dataset_id": analysis.dataset_id,
                    "ruleset_id": analysis.ruleset_id,
                    "ruleset_revision": analysis.ruleset_revision,
                    "config": analysis.config,
                    "tables": analysis.tables,
                },
                "buckets": analysis.buckets,
                "records": records,
            }),
        )
        .map_err(io_to_api)?;
        return Ok(());
    }

    // /api/plans/...
    if segs.first() == Some(&"api") && segs.get(1) == Some(&"plans") {
        return route_plans(app, req, stream, &segs);
    }

    // GET /api/search/:analysis_id?q=...
    if req.method == "GET" && segs.len() == 3 && segs[0] == "api" && segs[1] == "search" {
        let id: crate::model::Id = segs[2].parse().map_err(|_| ApiError::not_found("bad id"))?;
        let params = query_params(&req.query);
        let q = params.get("q").cloned().unwrap_or_default();
        let v = app.search(id, &q)?;
        respond_json(stream, 200, &v).map_err(io_to_api)?;
        return Ok(());
    }

    // GET /api/compare?a=ID&b=ID
    if req.method == "GET" && p == "/api/compare" {
        let params = query_params(&req.query);
        let a: crate::model::Id = params
            .get("a")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| ApiError::bad_request("missing a"))?;
        let b: crate::model::Id = params
            .get("b")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| ApiError::bad_request("missing b"))?;
        let v = app.compare_analyses(a, b)?;
        respond_json(stream, 200, &v).map_err(io_to_api)?;
        return Ok(());
    }

    Err(ApiError::not_found(format!(
        "no route for {} {}",
        req.method, p
    )))
}

fn route_plans(
    app: &App,
    req: &Request,
    stream: &mut TcpStream,
    segs: &[&str],
) -> Result<(), ApiError> {
    // POST /api/plans  {"analysis_id":...}
    if req.method == "POST" && segs.len() == 2 {
        #[derive(Deserialize)]
        struct Create {
            analysis_id: crate::model::Id,
        }
        let body: Create = parse_json(&req.body)?;
        let (status, plan) = app.create_plan(body.analysis_id)?;
        respond_json(stream, status, &serde_json::json!(plan)).map_err(io_to_api)?;
        return Ok(());
    }

    let Some(plan_id) = segs.get(2).and_then(|s| s.parse::<crate::model::Id>().ok()) else {
        return Err(ApiError::not_found("bad plan id"));
    };
    let resource = format!("plan:{plan_id}");

    // GET /api/plans/:id
    if req.method == "GET" && segs.len() == 3 {
        let g = app.store.lock();
        let plan = g
            .state()
            .plans
            .get(&plan_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("plan not found"))?;
        let version = g.state().version_of(&resource);
        let analysis = g.state().analyses.get(&plan.analysis_id).cloned();
        let mut v = serde_json::json!(plan);
        v["version"] = serde_json::json!(version);
        if let Some(an) = analysis {
            v["buckets"] = serde_json::json!(an.buckets);
            v["config"] = serde_json::json!(an.config);
        }
        respond_json(stream, 200, &v).map_err(io_to_api)?;
        return Ok(());
    }

    // PUT /api/plans/:id/decisions
    if req.method == "PUT" && segs.len() == 4 && segs[3] == "decisions" {
        let body: DecisionsReq = parse_json(&req.body)?;
        let (status, v) = app.set_decisions(plan_id, body)?;
        respond_json(stream, status, &v).map_err(io_to_api)?;
        return Ok(());
    }

    // POST /api/plans/:id/simulate  {"extra_records":[...]}
    if req.method == "POST" && segs.len() == 4 && segs[3] == "simulate" {
        #[derive(Deserialize)]
        struct Sim {
            #[serde(default)]
            extra_records: Vec<String>,
        }
        let body: Sim = parse_json(&req.body)?;
        let report = app.simulate(plan_id, &body.extra_records)?;
        respond_json(stream, 200, &serde_json::json!(report)).map_err(io_to_api)?;
        return Ok(());
    }

    // POST /api/plans/:id/approve
    if req.method == "POST" && segs.len() == 4 && segs[3] == "approve" {
        let body: ApproveReq = parse_json(&req.body)?;
        let (status, v) = app.approve(plan_id, body)?;
        respond_json(stream, status, &v).map_err(io_to_api)?;
        return Ok(());
    }

    // GET /api/plans/:id/export
    if req.method == "GET" && segs.len() == 4 && segs[3] == "export" {
        let g = app.store.lock();
        let doc = crate::service::export_plan(g.state(), plan_id)?;
        respond_json(stream, 200, &doc).map_err(io_to_api)?;
        return Ok(());
    }

    Err(ApiError::not_found(format!(
        "no plan route for {} {}",
        req.method, req.path
    )))
}

/// Helper retained for tests/tooling that want to open the app from a path.
pub fn open_app(dir: impl AsRef<Path>) -> std::io::Result<App> {
    App::open(dir)
}
