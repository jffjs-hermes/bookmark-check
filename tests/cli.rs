//! End-to-end CLI tests (SPEC §9).
//!
//! Each test runs the compiled `bookmark-check` binary as a subprocess against a
//! fixture Markdown file in `tests/fixtures/`, asserting stdout/stderr, exit
//! codes, and JSON output. Cases that need a real HTTP exchange (normal, broken,
//! redirect, timeout) talk to a tiny in-test `TcpListener` serving canned
//! responses — there is no external network access.
//!
//! The fixtures hard-code loopback ports (48_4xx) so they stay real files; each
//! distinct fixture uses a distinct port so tests can run in parallel without
//! colliding.

use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

const FIXTURES: &str = "tests/fixtures";

/// Absolute path to the freshly-built binary (cargo sets this for integration
/// tests).
fn bin() -> String {
    env::var("CARGO_BIN_EXE_bookmark-check")
        .expect("CARGO_BIN_EXE_bookmark-check should be set by cargo")
}

/// Run the binary with `args`; returns (exit code, stdout, stderr).
fn run(args: Vec<String>) -> (i32, String, String) {
    let out = Command::new(bin())
        .args(&args)
        .output()
        .expect("spawn bookmark-check");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (code, stdout, stderr)
}

/// Path to `tests/fixtures/<name>` relative to the crate root (cargo runs
/// integration tests with cwd = crate root).
fn fixture(name: &str) -> String {
    format!("{FIXTURES}/{name}")
}

/// A minimal canned HTTP responder bound to a loopback port.
///
/// It accepts a loop of connections in a spawned thread. For each request it
/// reads the head, extracts the path, and replies with the route's raw HTTP
/// response (or, for a `None` route, sleeps then closes so the caller times
/// out). The thread exits when `stop` is set, so the test tears the server down
/// deterministically.
fn spawn_server(
    port: u16,
    routes: std::collections::HashMap<String, String>,
    stop: Arc<AtomicBool>,
) {
    // Build the listener in the spawned thread; the caller polls the port until
    // it is accepting.
    thread::spawn(move || {
        let listener =
            TcpListener::bind(format!("127.0.0.1:{port}")).expect("bind canned listener");
        listener.set_nonblocking(true).ok();
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    handle(&mut stream, &routes);
                }
                Err(_) => {
                    // No connection ready yet; stop being a hot poll.
                    thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
        let _ = listener;
    });
    // Wait until the listener is actually accepting before returning.
    loop {
        let Ok(stream) = TcpStream::connect(format!("127.0.0.1:{port}")) else {
            thread::sleep(std::time::Duration::from_millis(2));
            continue;
        };
        let _ = stream;
        break;
    }
}

/// Read the request head on `stream`, route by path, and write the canned reply
/// (or stall for the timeout route). Panic-free: any I/O error just closes the
/// connection.
fn handle(stream: &mut TcpStream, routes: &std::collections::HashMap<String, String>) {
    let mut buf = [0u8; 2048];
    let n: usize = match stream.read(&mut buf[..]) {
        Ok(n) => n,
        Err(_) => return,
    };
    if n == 0 {
        return;
    }
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let path = request_path(&head);
    match routes.get(&path) {
        Some(resp) => {
            let bytes = resp.as_bytes();
            stream.write_all(bytes).ok();
        }
        None => {
            // Timeout / unknown route: accept, read, then stall and close so
            // the client's own timeout fires first.
            thread::sleep(std::time::Duration::from_millis(3500));
        }
    }
}

fn is_sep(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\r' || c == '\n'
}

/// Extract the request URI path from the request head (`METHOD /path HTTP/1.1`).
/// The METHOD is the first whitespace-delimited token; the path is the second.
/// Falls back to `/` when the request line is malformed.
fn request_path(head: &str) -> String {
    let mut result = String::new();
    let n = head.len();
    let mut i: usize = 0;
    // Skip the METHOD token.
    while i < n {
        let c = head.chars().nth(i).unwrap();
        if is_sep(c) {
            break;
        }
        i += 1;
    }
    // Skip inter-token whitespace.
    while i < n {
        let c = head.chars().nth(i).unwrap();
        if !is_sep(c) {
            break;
        }
        i += 1;
    }
    // Collect the path token.
    while i < n {
        let c = head.chars().nth(i).unwrap();
        if is_sep(c) {
            break;
        }
        result.push(c);
        i += 1;
    }
    if result.is_empty() {
        "/".to_string()
    } else {
        result
    }
}

fn ok_resp() -> String {
    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
}

fn broken_resp() -> String {
    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
}

/// A 301 that points the client back at the same server's `/ok` endpoint.
fn redirect_resp(port: u16) -> String {
    format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:{port}/ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

// ---------------------------------------------------------------------------
// Normal / healthy cases (no server needed)
// ---------------------------------------------------------------------------

#[test]
fn healthy_document_exits_zero() {
    let (code, stdout, _) = run(vec![fixture("healthy.md")]);
    assert_eq!(code, 0, "out={stdout}");
    // Nothing checkable: relative/mailto/code-fenced URLs are excluded.
    assert!(
        stdout.trim().is_empty(),
        "expected no output, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Malformed links
// ---------------------------------------------------------------------------

#[test]
fn malformed_links_are_reported() {
    let (code, stdout, _) = run(vec![fixture("malformed.md")]);
    assert_eq!(code, 0, "malformed links never fail the process");
    // Three links are malformed: empty URL, missing host, unmatched brackets.
    assert!(stdout.contains("MALFORMED empty url"), "got: {stdout}");
    assert!(stdout.contains("MALFORMED missing host"), "got: {stdout}");
    assert!(
        stdout.contains("MALFORMED unmatched brackets"),
        "got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Usage / error exit codes (no server needed)
// ---------------------------------------------------------------------------

#[test]
fn missing_file_argument_is_usage_error() {
    // Unknown flag -> clap usage error on stderr, exit 2.
    let (code, _, stderr) = run(vec!["--bogus".to_string(), fixture("healthy.md")]);
    assert_eq!(code, 2, "stderr={stderr}");
    assert!(stderr.contains("unexpected argument"), "got: {stderr}");
}

#[test]
fn no_files_is_usage_error() {
    let (code, _, stderr) = run(vec![]);
    assert_eq!(code, 2, "stderr={stderr}");
    assert!(stderr.contains("required arguments"), "got: {stderr}");
}

#[test]
fn unreadable_file_is_usage_error() {
    let missing = "/definitely/not/a/real/file.md";
    let (code, _, stderr) = run(vec![missing.to_string()]);
    assert_eq!(code, 2, "stderr={stderr}");
    assert!(stderr.contains("failed to read"), "got: {stderr}");
}

// ---------------------------------------------------------------------------
// HTTP cases served by the canned TcpListener
// ---------------------------------------------------------------------------

#[test]
fn normal_link_reports_ok() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut m = std::collections::HashMap::new();
    m.insert("/ok".to_string(), ok_resp());
    spawn_server(48421, m, Arc::clone(&stop));

    let (code, stdout, stderr) = run(vec![fixture("normal.md")]);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(code, 0, "stderr={stderr} out={stdout}");
    assert!(stdout.contains("OK"), "got: {stdout}");
    assert!(stdout.contains("200"), "got: {stdout}");
    assert!(
        stdout.contains("http://127.0.0.1:48421/ok"),
        "got: {stdout}"
    );
}

#[test]
fn broken_link_reports_and_exits_one() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut m = std::collections::HashMap::new();
    m.insert("/broken".to_string(), broken_resp());
    spawn_server(48422, m, Arc::clone(&stop));

    let (code, stdout, stderr) = run(vec![fixture("broken.md")]);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(
        code, 1,
        "broken link must exit 1; stderr={stderr} out={stdout}"
    );
    assert!(stdout.contains("BROKEN"), "got: {stdout}");
    assert!(stdout.contains("404"), "got: {stdout}");
}

#[test]
fn redirect_is_followed_and_reported() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut m = std::collections::HashMap::new();
    // /redirect -> 301 -> /ok -> 200. The /ok hop is a real second request to
    // the same server.
    m.insert("/redirect".to_string(), redirect_resp(48423));
    m.insert("/ok".to_string(), ok_resp());
    spawn_server(48423, m, Arc::clone(&stop));

    let (code, stdout, stderr) = run(vec![fixture("redirect.md")]);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(
        code, 0,
        "followed redirect ending 2xx is healthy; stderr={stderr} out={stdout}"
    );
    assert!(stdout.contains("REDIRECT"), "got: {stdout}");
    assert!(stdout.contains("301"), "got: {stdout}");
}

#[test]
fn redirect_unfollowed_exits_one() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut m = std::collections::HashMap::new();
    m.insert("/redirect".to_string(), redirect_resp(48425));
    m.insert("/ok".to_string(), ok_resp());
    m.insert("*".to_string(), ok_resp());
    spawn_server(48425, m, Arc::clone(&stop));

    let (code, stdout, stderr) = run(vec![
        "--no-follow-redirects".to_string(),
        fixture("redirect_unfollowed.md"),
    ]);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(code, 1, "unfollowed redirect is a finding; stderr={stderr}");
    assert!(stdout.contains("REDIRECT"), "got: {stdout}");
}

#[test]
fn timeout_reports_error() {
    let stop = Arc::new(AtomicBool::new(false));
    // Route table with NO matching path -> the handler accepts, reads, then
    // stalls, so the client's --timeout 1 fires well before the server closes.
    let m = std::collections::HashMap::new();
    spawn_server(48424, m, Arc::clone(&stop));

    let (code, stdout, stderr) = run(vec![
        "--timeout".to_string(),
        "1".to_string(),
        fixture("timeout.md"),
    ]);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(
        code, 0,
        "timeout is an ERROR, not a process failure; stderr={stderr}"
    );
    // The URL is reported as an error. The exact transport message may vary
    // ("timed out" / "timed out reading response"), so assert on the prefix and
    // the URL.
    assert!(stdout.contains("ERROR"), "got: {stdout}");
    assert!(
        stdout.contains("http://127.0.0.1:48424/timeout"),
        "got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

#[test]
fn json_reports_summary_and_checks() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut m = std::collections::HashMap::new();
    m.insert("/ok".to_string(), ok_resp());
    m.insert("/broken".to_string(), broken_resp());
    m.insert("*".to_string(), ok_resp());
    spawn_server(48426, m, Arc::clone(&stop));

    let (code, stdout, _) = run(vec!["--json".to_string(), fixture("json.md")]);
    stop.store(true, Ordering::Relaxed);

    assert_eq!(code, 1, "JSON run with a broken link exits 1");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(parsed["version"], 1);
    let summary = parsed["summary"].clone();
    assert_eq!(summary["total"], 2);
    assert_eq!(summary["ok"], 1);
    assert_eq!(summary["broken"], 1);
    assert_eq!(summary["redirects"], 0);
    assert_eq!(summary["malformed"], 0);
    // Deterministic order: entries sorted by (file, line, column).
    let checks = parsed["checks"].clone();
    assert_eq!(checks[0]["status"], "ok");
    assert_eq!(checks[0]["http_status"], 200);
    assert_eq!(checks[1]["status"], "broken");
    assert_eq!(checks[1]["http_status"], 404);
    let loc = checks[1]["location"].clone();
    assert_eq!(loc["file"], "tests/fixtures/json.md");
}

#[test]
fn json_malformed_are_reported() {
    let (code, stdout, _) = run(vec!["--json".to_string(), fixture("malformed.md")]);
    assert_eq!(code, 0, "malformed links never fail the process");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(parsed["summary"]["malformed"], 3);
    assert_eq!(parsed["checks"][0]["status"], "malformed");
}

#[test]
fn json_empty_document() {
    let (code, stdout, _) = run(vec!["--json".to_string(), fixture("healthy.md")]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(parsed["summary"]["total"], 0);
    assert_eq!(parsed["summary"]["ok"], 0);
    let checks = parsed["checks"].as_array();
    assert_eq!(checks.unwrap().len(), 0);
}
