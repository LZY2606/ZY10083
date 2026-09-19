#![allow(dead_code)]
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "uiw-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub struct Server {
    pub child: Child,
    pub addr: String,
    pub dir: PathBuf,
}

impl Server {
    pub fn start() -> Server {
        let dir = tempdir();
        let port = pick_port();
        let addr = format!("127.0.0.1:{port}");
        let bin = env!("CARGO_BIN_EXE_server");
        let child = Command::new(bin)
            .args(["--listen", &addr, "--data-dir", dir.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start server");
        let mut s = Server {
            child,
            addr: addr.clone(),
            dir,
        };
        for _ in 0..200 {
            if request(&addr, "GET", "/api/version", None, None).is_ok() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        s.kill();
        panic!("server did not become ready on {addr}");
    }

    /// Kill without waiting (simulate abnormal termination).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn pick_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

pub fn request(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    idem: Option<&str>,
) -> std::io::Result<Response> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(k) = idem {
        head.push_str(&format!("Idempotency-Key: {k}\r\n"));
    }
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(b) = body {
        stream.write_all(b.as_bytes())?;
    }
    stream.flush()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (h, b) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = h
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(Response {
        status,
        body: b.to_string(),
    })
}

pub fn j(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({"raw": body}))
}

/// Restart an *already killed* server against the same data directory,
/// returning a new Server (the caller owns/drops it).
pub fn start_on(dir: &PathBuf) -> Server {
    let port = pick_port();
    let addr = format!("127.0.0.1:{port}");
    let bin = env!("CARGO_BIN_EXE_server");
    let child = Command::new(bin)
        .args(["--listen", &addr, "--data-dir", dir.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("restart server");
    let s = Server {
        child,
        addr: addr.clone(),
        dir: dir.clone(),
    };
    for _ in 0..200 {
        if request(&addr, "GET", "/api/version", None, None).is_ok() {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("restarted server did not become ready on {addr}");
}
