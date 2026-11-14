//! Integration test: spawns the `harness-host` binary and speaks NDJSON.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
}

impl Host {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_harness-host"))
            .arg("--stdio")
            .env("HARNESS_HOST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let value: Value = serde_json::from_str(&line).expect("stdout must be NDJSON");
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines: rx,
        }
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn send(&mut self, value: &Value) {
        self.send_raw(&value.to_string());
    }

    fn next(&self) -> Value {
        self.lines
            .recv_timeout(Duration::from_secs(20))
            .expect("response")
    }

    /// Collects messages until the response with `id`; returns (progress, response).
    fn until_response(&self, id: u64) -> (Vec<Value>, Value) {
        let mut progress = Vec::new();
        loop {
            let message = self.next();
            if message["id"] == json!(id) {
                return (progress, message);
            }
            if message["method"] == "$/progress" && message["params"]["id"] == json!(id) {
                progress.push(message);
            }
        }
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> Value {
        self.send(&json!({"id": id, "method": method, "params": params}));
        self.until_response(id).1
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_errors_and_methods() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_string_lossy().into_owned();
    std::fs::write(tmp.path().join("hello.txt"), "alpha\nbeta needle\n").unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    for i in 0..300 {
        std::fs::write(tmp.path().join("sub").join(format!("f{i}.txt")), "needle\n").unwrap();
    }

    let mut host = Host::spawn();
    let early = host.request(5, "ping", &json!({}));
    assert_eq!(early["error"]["code"], -32002);

    let init = host.request(
        0,
        "initialize",
        &json!({"protocolVersion": 1, "client": "test"}),
    );
    let result = &init["result"];
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["server"], "harness-host");
    assert_eq!(
        result["services"],
        json!(["fs", "search", "git", "watch", "pty", "store"])
    );
    let methods: Vec<&str> = result["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    assert!(methods.contains(&"file.readdir") && methods.contains(&"git.refresh"));

    assert_eq!(
        host.request(1, "ping", &json!({}))["result"],
        json!({"pong": true})
    );

    host.send_raw("{not json");
    let parse_error = host.next();
    assert_eq!(parse_error["id"], Value::Null);
    assert_eq!(parse_error["error"]["code"], -32700);

    assert_eq!(
        host.request(2, "nope.method", &json!({}))["error"]["code"],
        -32601
    );

    let entries = host.request(3, "file.readdir", &json!({"path": root}));
    let names: Vec<&str> = entries["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["sub", "hello.txt"]);

    let missing = host.request(
        4,
        "file.stat",
        &json!({"path": format!("{root}/missing.txt")}),
    );
    assert_eq!(missing["error"]["code"], 1);
    assert_eq!(missing["error"]["data"]["errno"], "ENOENT");
    assert_eq!(missing["error"]["data"]["syscall"], "stat");

    let files = host.request(
        6,
        "file.searchWorkspaceFiles",
        &json!({"rootPath": root, "query": "hello", "limit": 5}),
    );
    assert_eq!(files["result"][0]["relativePath"], "hello.txt");
    let bad = host.request(
        7,
        "file.searchWorkspaceFiles",
        &json!({"rootPath": root, "query": 1}),
    );
    assert_eq!(bad["error"]["code"], 2);
    assert_eq!(
        bad["error"]["message"],
        "Invalid workspace file search query or limit"
    );

    host.send(
        &json!({"id": 8, "method": "search.text", "params": {"rootPath": root, "query": "needle"}}),
    );
    let (progress, response) = host.until_response(8);
    assert_eq!(response["result"]["matchCount"], 301);
    let streamed: usize = progress
        .iter()
        .map(|p| p["params"]["value"]["matches"].as_array().unwrap().len())
        .sum();
    assert_eq!(streamed, 301, "all progress must precede the response");

    // Unknown cancel ids are ignored; a cancelled request answers -32800 or its result.
    host.send(&json!({"method": "$/cancel", "params": {"id": 999}}));
    host.send(
        &json!({"id": 9, "method": "search.text", "params": {"rootPath": root, "query": "needle"}}),
    );
    host.send(&json!({"method": "$/cancel", "params": {"id": 9}}));
    let (_, cancelled) = host.until_response(9);
    assert!(
        cancelled["error"]["code"] == -32800 || cancelled["result"].is_object(),
        "{cancelled}"
    );

    let summary = host.request(
        10,
        "git.getRepositorySummary",
        &json!({"workspacePath": root}),
    );
    assert_eq!(summary["result"]["isRepository"], false);
    assert_eq!(summary["result"]["workspacePath"], json!(root));

    // EOF: the process exits 0 promptly.
    host.stdin = None;
    let started = Instant::now();
    loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "host must exit within 2s of EOF"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn shutdown_exits_cleanly() {
    let mut host = Host::spawn();
    host.request(0, "initialize", &json!({"protocolVersion": 1}));
    assert_eq!(
        host.request(1, "shutdown", &json!({}))["result"],
        Value::Null
    );
    let status = host.child.wait().unwrap();
    assert!(status.success());
}

/// The stdout thread coalesces queued messages into one write; every message
/// must still arrive whole, once, as its own NDJSON line. A single stdin write
/// carrying many requests also exercises the stdin thread's line splitting.
#[test]
fn burst_of_requests_gets_every_response_once() {
    let mut host = Host::spawn();
    host.request(0, "initialize", &json!({"protocolVersion": 1}));
    let count = 500_u64;
    let mut burst = String::new();
    for id in 1..=count {
        burst.push_str(&json!({"id": id, "method": "ping"}).to_string());
        burst.push('\n');
    }
    let stdin = host.stdin.as_mut().unwrap();
    stdin.write_all(burst.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut seen = vec![false; usize::try_from(count).unwrap() + 1];
    for _ in 0..count {
        let message = host.next();
        assert_eq!(message["result"], json!({"pong": true}), "{message}");
        let id = usize::try_from(message["id"].as_u64().unwrap()).unwrap();
        assert!(!seen[id], "duplicate response {id}");
        seen[id] = true;
    }
    assert!(seen[1..].iter().all(|s| *s));
    assert_eq!(
        host.request(count + 1, "shutdown", &json!({}))["result"],
        Value::Null
    );
    assert!(host.child.wait().unwrap().success());
}
