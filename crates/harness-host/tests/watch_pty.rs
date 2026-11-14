//! Integration test for the Phase 3b services: spawns `harness-host` and
//! drives `watch.*` / `pty.*` over NDJSON (contract r2 §9).

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
        let mut host = Self {
            child,
            stdin,
            lines: rx,
        };
        let init = host.request(0, "initialize", &json!({"protocolVersion": 1}));
        let methods = init["result"]["methods"].as_array().unwrap().clone();
        for m in [
            "watch.subscribe",
            "watch.unsubscribe",
            "pty.spawn",
            "pty.write",
            "pty.resize",
            "pty.kill",
        ] {
            assert!(methods.contains(&json!(m)), "{m} not advertised");
        }
        host
    }

    fn send(&mut self, value: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(value.to_string().as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn next(&self) -> Value {
        self.lines
            .recv_timeout(Duration::from_secs(20))
            .expect("message")
    }

    /// Every message up to and including the response `id`.
    fn until_response(&self, id: u64) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let message = self.next();
            if message["id"] == json!(id) {
                return (before, message);
            }
            before.push(message);
        }
    }

    fn request(&mut self, id: u64, method: &str, params: &Value) -> Value {
        self.send(&json!({"id": id, "method": method, "params": params}));
        self.until_response(id).1
    }

    /// Messages until `pred` matches (inclusive).
    fn until(&self, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let message = self.next();
            let hit = pred(&message);
            seen.push(message);
            if hit {
                return seen;
            }
        }
    }

    /// Messages arriving within `quiet`.
    fn drain(&self, quiet: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(message) = self.lines.recv_timeout(quiet) {
            out.push(message);
        }
        out
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
fn one_shot(text: &str, code: i32) -> (String, Vec<String>) {
    (
        "cmd.exe".to_owned(),
        vec!["/c".to_owned(), format!("echo {text}& exit /b {code}")],
    )
}

#[cfg(not(windows))]
fn one_shot(text: &str, code: i32) -> (String, Vec<String>) {
    (
        "/bin/sh".to_owned(),
        vec!["-c".to_owned(), format!("echo {text}; exit {code}")],
    )
}

fn interactive() -> &'static str {
    if cfg!(windows) {
        "cmd.exe"
    } else {
        "/bin/sh"
    }
}

fn output_of(messages: &[Value], session: &Value) -> String {
    messages
        .iter()
        .filter(|m| m["method"] == "$/pty/output" && m["params"]["sessionId"] == *session)
        .map(|m| m["params"]["data"].as_str().unwrap().to_owned())
        .collect()
}

#[cfg(windows)]
fn process_alive(pid: u64) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

#[cfg(not(windows))]
fn process_alive(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn watch_subscribe_events_and_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_string_lossy().into_owned();
    let mut host = Host::spawn();

    let missing = tmp.path().join("missing").to_string_lossy().into_owned();
    let error = host.request(1, "watch.subscribe", &json!({"path": missing}));
    assert_eq!(error["error"]["code"], 2);
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "无法监视目录 '{missing}': ENOENT: no such file or directory, stat"
        )),
        "{message}"
    );

    let sub = host.request(2, "watch.subscribe", &json!({"path": root}));
    let watch_id = sub["result"]["watchId"].clone();
    assert!(watch_id.is_string());

    std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
    let seen = host.until(|m| m["method"] == "$/watch/event");
    let event = &seen.last().unwrap()["params"];
    assert_eq!(event["watchId"], watch_id);
    assert_eq!(event["event"]["dirPath"], json!(root));
    let changed = event["event"]["changedPath"].as_str().unwrap();
    assert!(changed.ends_with("a.txt"), "{changed}");
    assert!(event["event"].get("changedPath").is_some());

    // Several paths in one window: changedPath omitted, never null.
    std::fs::write(tmp.path().join("b.txt"), "x").unwrap();
    std::fs::write(tmp.path().join("c.txt"), "x").unwrap();
    let seen = host.until(|m| m["method"] == "$/watch/event");
    let event = &seen.last().unwrap()["params"]["event"];
    assert_eq!(*event, json!({"dirPath": root}));
    host.drain(Duration::from_millis(300));

    let un = host.request(3, "watch.unsubscribe", &json!({"watchId": watch_id}));
    assert_eq!(un["result"], Value::Null);
    std::fs::write(tmp.path().join("after.txt"), "x").unwrap();
    let late = host.drain(Duration::from_millis(500));
    assert!(late.is_empty(), "events after unsubscribe: {late:?}");
    let again = host.request(4, "watch.unsubscribe", &json!({"watchId": "nope"}));
    assert_eq!(again["result"], Value::Null);

    // Watched directory removed: final {dirPath} event, then $/watch/closed.
    let inner = tmp.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    let inner_str = inner.to_string_lossy().into_owned();
    let sub = host.request(5, "watch.subscribe", &json!({"path": inner_str}));
    let inner_id = sub["result"]["watchId"].clone();
    std::fs::remove_dir_all(&inner).unwrap();
    let seen = host.until(|m| m["method"] == "$/watch/closed");
    let n = seen.len();
    assert_eq!(seen[n - 1]["params"]["watchId"], inner_id);
    assert_eq!(seen[n - 2]["method"], "$/watch/event");
    assert_eq!(
        seen[n - 2]["params"]["event"],
        json!({"dirPath": inner_str})
    );
}

#[test]
fn pty_spawn_output_exit_write_resize_kill() {
    let mut host = Host::spawn();
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();

    let bad = host.request(1, "pty.spawn", &json!({"cols": 0, "rows": 24, "cwd": cwd}));
    assert_eq!(bad["error"]["code"], 2);
    assert_eq!(
        bad["error"]["message"],
        "resizing must be done using positive cols and rows"
    );

    // One-shot: response first, then output, then exit with the code.
    let (shell, args) = one_shot("hello_host_pty", 5);
    host.send(&json!({"id": 2, "method": "pty.spawn", "params": {
        "shell": shell, "args": args, "cwd": cwd, "cols": 80, "rows": 24,
        "name": "xterm-256color",
    }}));
    let (before, response) = host.until_response(2);
    assert!(
        before.iter().all(|m| m["method"] != "$/pty/output"),
        "output before spawn response"
    );
    let session = response["result"]["sessionId"].clone();
    assert!(response["result"]["pid"].as_u64().unwrap() > 0);
    assert_eq!(response["result"]["shell"], json!(shell));
    let seen = host.until(|m| m["method"] == "$/pty/exit");
    assert!(output_of(&seen, &session).contains("hello_host_pty"));
    let exit = seen.last().unwrap();
    assert_eq!(exit["params"]["sessionId"], session);
    assert_eq!(exit["params"]["exitCode"], 5);
    assert!(seen
        .iter()
        .all(|m| m["method"] != "$/pty/output" || m["params"]["data"] != ""));
    let gone = host.request(3, "pty.write", &json!({"sessionId": session, "data": "x"}));
    assert_eq!(
        gone["error"]["message"],
        format!("Terminal not found: {}", session.as_str().unwrap())
    );
    assert_eq!(
        host.request(4, "pty.kill", &json!({"sessionId": session}))["result"],
        Value::Null
    );

    // Interactive: write/echo roundtrip with pipelined writes (per-session FIFO).
    let spawned = host.request(
        5,
        "pty.spawn",
        &json!({"shell": interactive(), "args": [], "cwd": cwd, "cols": 80, "rows": 24}),
    );
    let session = spawned["result"]["sessionId"].clone();
    let pid = spawned["result"]["pid"].as_u64().unwrap();
    for (i, chunk) in ["echo ", "pipe", "lined", "_marker", "\r"]
        .iter()
        .enumerate()
    {
        host.send(&json!({"id": 10 + i, "method": "pty.write",
            "params": {"sessionId": session, "data": chunk}}));
    }
    let seen = host.until(|m| {
        m["method"] == "$/pty/output"
            && m["params"]["data"]
                .as_str()
                .is_some_and(|d| d.contains("pipelined_marker"))
    });
    for i in 10..15 {
        assert!(
            seen.iter()
                .any(|m| m["id"] == json!(i) && m["result"].is_null()),
            "write {i} not acknowledged before echo"
        );
    }

    let resized = host.request(
        20,
        "pty.resize",
        &json!({"sessionId": session, "cols": 120, "rows": 40}),
    );
    assert_eq!(resized["result"], Value::Null);
    let bad = host.request(
        21,
        "pty.resize",
        &json!({"sessionId": session, "cols": -1, "rows": 40}),
    );
    assert_eq!(
        bad["error"]["message"],
        "resizing must be done using positive cols and rows"
    );
    let unknown = host.request(
        22,
        "pty.resize",
        &json!({"sessionId": "nope", "cols": 10, "rows": 10}),
    );
    assert_eq!(unknown["error"]["message"], "Terminal not found: nope");

    // Kill: null, no $/pty/exit, tree gone.
    let killed = host.request(23, "pty.kill", &json!({"sessionId": session}));
    assert_eq!(killed["result"], Value::Null);
    let late = host.drain(Duration::from_millis(600));
    assert!(
        late.iter().all(|m| m["params"]["sessionId"] != session),
        "notifications after kill: {late:?}"
    );
    assert!(!process_alive(pid), "shell {pid} survived pty.kill");
}

#[test]
fn eof_kills_sessions_and_exits() {
    let mut host = Host::spawn();
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let spawned = host.request(
        1,
        "pty.spawn",
        &json!({"shell": interactive(), "cwd": cwd, "cols": 80, "rows": 24}),
    );
    let pid = spawned["result"]["pid"].as_u64().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    host.request(
        2,
        "watch.subscribe",
        &json!({"path": tmp.path().to_string_lossy(), "recursive": true}),
    );
    drop(host.stdin.take());
    let start = Instant::now();
    let status = loop {
        if let Some(status) = host.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "host did not exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status:?}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert!(!process_alive(pid), "shell {pid} orphaned after EOF");
}
