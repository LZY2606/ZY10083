use crate::engine::RuleConfig;
use crate::service::{
    analyze_dataset, analyze_query, append_records, apply_plan, create_dataset, create_plan,
    export_plan, get_dataset, get_plan, list_plans, update_plan, AppendRecordsReq, ApplyPlanReq,
    CreateDataset, CreatePlan, UpdatePlanReq,
};
use crate::store::Store;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

pub fn serve(mut store: Store, listen: &str) -> Result<(), String> {
    let listener = TcpListener::bind(listen).map_err(|e| e.to_string())?;
    let address = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| listen.to_owned());
    println!("listening on http://{address}");
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream, &mut store) {
                    let _ = write_json(
                        &mut stream,
                        500,
                        &serde_json::json!({"error":"internal","message":error}),
                    );
                }
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

fn handle_connection(stream: &mut TcpStream, store: &mut Store) -> Result<(), String> {
    let mut initial = [0u8; 4096];
    let size = stream.read(&mut initial).map_err(|e| e.to_string())?;
    let initial = String::from_utf8_lossy(&initial[..size]).to_string();
    let headers = initial
        .split("\r\n\r\n")
        .next()
        .unwrap_or(&initial)
        .to_owned();
    let mut content_length = 0usize;
    for line in headers.split("\r\n") {
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim())
        {
            content_length = value
                .parse()
                .map_err(|e: std::num::ParseIntError| e.to_string())?;
        }
    }
    let mut body_buffer = Vec::new();
    if let Some((_, partial)) = initial.split_once("\r\n\r\n") {
        body_buffer.extend_from_slice(partial.as_bytes());
    }
    while body_buffer.len() < content_length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        body_buffer.extend_from_slice(&chunk[..read]);
    }
    let mut raw = headers;
    raw.push_str("\r\n\r\n");
    raw.push_str(&String::from_utf8_lossy(&body_buffer));
    let mut lines = raw.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut idempotency_key = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.split_once(':').map(|(key, value)| {
            (
                key.trim().eq_ignore_ascii_case("idempotency-key"),
                value.trim(),
            )
        }) {
            if value.0 {
                idempotency_key = Some(value.1.to_owned());
            }
        }
    }
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    route(stream, store, method, path, query, &body, idempotency_key)
}

fn route(
    stream: &mut TcpStream,
    store: &mut Store,
    method: &str,
    path: &str,
    query: &str,
    body: &str,
    idempotency_key: Option<String>,
) -> Result<(), String> {
    match (method, path) {
        ("GET", "/") => write_html(stream, include_str!("../static/index.html")),
        ("GET", "/api/rules") => write_json(
            stream,
            200,
            &serde_json::json!({"supported":[{"unicode_version":"15.1.0"},{"unicode_version":"16.0.0"}],"default":"16.0.0"}),
        ),
        ("POST", "/api/analyze") => {
            let payload: serde_json::Value = parse_json(body)?;
            let rules: RuleConfig =
                serde_json::from_value(payload.get("rules").cloned().unwrap_or_default())
                    .map_err(|e| e.to_string())?;
            let query_text = payload
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            respond(stream, analyze_query(query_text, rules))
        }
        ("POST", "/api/datasets") => {
            let request: CreateDataset = parse_json(body)?;
            respond(stream, create_dataset(store, request, idempotency_key))
        }
        ("GET", "/api/datasets") => write_json(
            stream,
            200,
            &serde_json::json!({"datasets": store.state().datasets}),
        ),
        _ if method == "GET" && path.starts_with("/api/datasets/") => {
            let rest = &path["/api/datasets/".len()..];
            let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
            match suffix {
                "" => respond(stream, get_dataset(store, id)),
                "analyze" => {
                    let rules = rules_from_query(query)?;
                    respond(stream, analyze_dataset(store, id, rules))
                }
                _ => write_json(stream, 404, &serde_json::json!({"error":"not-found"})),
            }
        }
        _ if method == "POST"
            && path.starts_with("/api/datasets/")
            && path.ends_with("/records") =>
        {
            let id = &path["/api/datasets/".len()..path.len() - "/records".len()];
            let id = id.trim_end_matches('/');
            let request: AppendRecordsReq = parse_json(body)?;
            respond(stream, append_records(store, id, request, idempotency_key))
        }
        ("POST", "/api/plans") => {
            let request: CreatePlan = parse_json(body)?;
            respond(stream, create_plan(store, request, idempotency_key))
        }
        ("GET", "/api/plans") => write_json(stream, 200, &list_plans(store)),
        _ if method == "GET" && path.starts_with("/api/plans/") => {
            let rest = &path["/api/plans/".len()..];
            let (id, suffix) = rest.split_once('/').unwrap_or((rest, ""));
            match suffix {
                "" => respond(stream, get_plan(store, id)),
                "export" => match export_plan(store, id) {
                    Ok((name, bytes)) => write_download(stream, &name, &bytes),
                    Err(error) => write_json(stream, error.status, &error.body),
                },
                _ => write_json(stream, 404, &serde_json::json!({"error":"not-found"})),
            }
        }
        _ if method == "POST"
            && path.starts_with("/api/plans/")
            && path.ends_with("/decisions") =>
        {
            let id = &path["/api/plans/".len()..path.len() - "/decisions".len()];
            let id = id.trim_end_matches('/');
            let request: UpdatePlanReq = parse_json(body)?;
            respond(stream, update_plan(store, id, request, idempotency_key))
        }
        _ if method == "POST" && path.starts_with("/api/plans/") && path.ends_with("/apply") => {
            let id = &path["/api/plans/".len()..path.len() - "/apply".len()];
            let id = id.trim_end_matches('/');
            let request: ApplyPlanReq = parse_json(body)?;
            respond(stream, apply_plan(store, id, request, idempotency_key))
        }
        _ => write_json(
            stream,
            404,
            &serde_json::json!({"error":"not-found","path":path}),
        ),
    }
}

fn rules_from_query(query: &str) -> Result<RuleConfig, String> {
    let mut rules = RuleConfig::default();
    for pair in query.split('&').filter(|item| !item.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = url_decode(value);
        match key {
            "rules" => rules = serde_json::from_str(&value).map_err(|e| e.to_string())?,
            _ => {}
        }
    }
    Ok(rules)
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|error| format!("invalid JSON: {error}"))
}

fn respond(
    stream: &mut TcpStream,
    result: Result<serde_json::Value, crate::service::ServiceError>,
) -> Result<(), String> {
    match result {
        Ok(value) => write_json(stream, 200, &value),
        Err(error) => write_json(stream, error.status, &error.body),
    }
}

fn write_html(stream: &mut TcpStream, html: &str) -> Result<(), String> {
    write_response(
        stream,
        "200 OK",
        "text/html; charset=utf-8",
        html.as_bytes(),
        None,
    )
}

fn write_json(
    stream: &mut TcpStream,
    status: u16,
    value: &serde_json::Value,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let status_line = format!("{status} {status_text}");
    write_response_bytes(
        stream,
        &status_line,
        "application/json; charset=utf-8",
        &bytes,
        None,
    )
}

fn write_download(stream: &mut TcpStream, name: &str, bytes: &[u8]) -> Result<(), String> {
    write_response(
        stream,
        "200 OK",
        "application/json",
        bytes,
        Some(format!("attachment; filename=\"{name}\"")),
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    bytes: &[u8],
    disposition: Option<String>,
) -> Result<(), String> {
    write_response_bytes(stream, status, content_type, bytes, disposition)
}

fn write_response_bytes(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    bytes: &[u8],
    disposition: Option<String>,
) -> Result<(), String> {
    let disposition = disposition
        .map(|value| format!("Content-Disposition: {value}\r\n"))
        .unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{disposition}\r\n",
        bytes.len()
    );
    stream
        .write_all(header.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(bytes).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let high = (bytes[i + 1] as char).to_digit(16);
                let low = (bytes[i + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    out.push((high * 16 + low) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
