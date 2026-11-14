use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;

const WAIT: Duration = Duration::from_secs(15);

type Rx = mpsc::Receiver<(String, PtyEvent)>;

fn channel_sink() -> (PtySink, Rx) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: PtySink = Arc::new(move |id: &str, event| {
        let _ = lock(&tx).send((id.to_owned(), event));
    });
    (sink, rx)
}

/// Collects output until `needle` appears; returns (output, exit if seen).
fn read_until(rx: &Rx, mut done: impl FnMut(&str, Option<i64>) -> bool) -> (String, Option<i64>) {
    let deadline = Instant::now() + WAIT;
    let mut out = String::new();
    let mut exit = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok((_, PtyEvent::Data(text))) => out.push_str(&text),
            Ok((_, PtyEvent::Exit(code))) => exit = Some(code),
            Err(_) => break,
        }
        if done(&out, exit) {
            return (out, exit);
        }
    }
    panic!("timed out; output so far: {out:?}, exit: {exit:?}");
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

#[cfg(windows)]
fn interactive_shell() -> String {
    "cmd.exe".to_owned()
}

#[cfg(not(windows))]
fn interactive_shell() -> String {
    "/bin/sh".to_owned()
}

fn options(shell: String, args: Vec<String>, attached: bool) -> CreateOptions {
    CreateOptions {
        cols: 80,
        rows: 24,
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        shell: Some(shell),
        args,
        attached,
        ..CreateOptions::default()
    }
}

#[test]
fn spawn_echo_streams_output_then_exit_code() {
    let manager = PtyManager::default();
    let (sink, rx) = channel_sink();
    let (shell, args) = one_shot("hello_pty", 3);
    let created = manager.create(options(shell, args, true), sink).unwrap();
    assert_eq!(created.id, "0");
    assert!(created.pid.is_some());
    assert_eq!(created.windows_pty.is_some(), cfg!(windows));
    let (out, exit) = read_until(&rx, |_, exit| exit.is_some());
    assert!(out.contains("hello_pty"), "{out:?}");
    assert_eq!(exit, Some(3));
    // Exit is the last event and the session is forgotten.
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(manager.ids(), Vec::<String>::new());
    assert_eq!(
        manager.write(&created.id, b"x"),
        Err(PtyError::NotFound("0".to_owned()))
    );
}

#[test]
fn write_read_roundtrip_and_exit() {
    let manager = PtyManager::default();
    let (sink, rx) = channel_sink();
    let created = manager
        .create(options(interactive_shell(), Vec::new(), true), sink)
        .unwrap();
    manager.write(&created.id, b"echo round_trip_42\r").unwrap();
    // Echoed input plus the command output: the marker shows up twice.
    read_until(&rx, |out, _| out.matches("round_trip_42").count() >= 2);
    manager.write(&created.id, b"exit 7\r").unwrap();
    let (_, exit) = read_until(&rx, |_, exit| exit.is_some());
    assert_eq!(exit, Some(7));
}

#[test]
fn resize_is_visible_to_the_shell() {
    let manager = PtyManager::default();
    let (sink, rx) = channel_sink();
    let created = manager
        .create(options(interactive_shell(), Vec::new(), true), sink)
        .unwrap();
    manager.resize(&created.id, 101, 33).unwrap();
    assert!(matches!(
        manager.resize(&created.id, 0, 10),
        Err(PtyError::App(_))
    ));
    if cfg!(windows) {
        manager.write(&created.id, b"mode con\r").unwrap();
        read_until(&rx, |out, _| out.contains("101") && out.contains("33"));
    } else {
        manager.write(&created.id, b"stty size\r").unwrap();
        read_until(&rx, |out, _| out.contains("33 101"));
    }
    assert!(manager.kill(&created.id));
}

#[test]
fn kill_terminates_tree_and_silences() {
    let manager = PtyManager::default();
    let (sink, rx) = channel_sink();
    let created = manager
        .create(options(interactive_shell(), Vec::new(), true), sink)
        .unwrap();
    manager.write(&created.id, b"echo alive_marker\r").unwrap();
    read_until(&rx, |out, _| out.matches("alive_marker").count() >= 2);
    let pid = created.pid.unwrap();
    assert!(manager.kill(&created.id));
    assert!(!manager.kill(&created.id));
    assert_eq!(manager.ids(), Vec::<String>::new());
    // No event (in particular no Exit) after dispose.
    while let Ok((_, event)) = rx.recv_timeout(Duration::from_millis(500)) {
        assert!(!matches!(event, PtyEvent::Exit(_)), "event after dispose");
    }
    assert!(!process_alive(pid), "shell {pid} still running");
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn detached_output_is_buffered_until_attach() {
    let manager = PtyManager::default();
    let (sink, rx) = channel_sink();
    let (shell, args) = one_shot("buffered_out", 0);
    let created = manager.create(options(shell, args, false), sink).unwrap();
    // Nothing is delivered while detached, even after the shell exited.
    assert!(rx.recv_timeout(Duration::from_millis(1500)).is_err());
    manager.attach(&created.id).unwrap();
    let (out, exit) = read_until(&rx, |_, exit| exit.is_some());
    assert!(out.contains("buffered_out"), "{out:?}");
    assert_eq!(exit, Some(0));
    assert_eq!(manager.ids(), Vec::<String>::new());
}

#[test]
fn concurrent_sessions_are_independent() {
    let manager = PtyManager::default();
    let (sink_a, rx_a) = channel_sink();
    let (sink_b, rx_b) = channel_sink();
    let (shell, args) = one_shot("from_a", 1);
    let a = manager.create(options(shell, args, true), sink_a).unwrap();
    let (shell, args) = one_shot("from_b", 2);
    let b = manager.create(options(shell, args, true), sink_b).unwrap();
    assert_ne!(a.id, b.id);
    let (out_a, exit_a) = read_until(&rx_a, |_, exit| exit.is_some());
    let (out_b, exit_b) = read_until(&rx_b, |_, exit| exit.is_some());
    assert!(out_a.contains("from_a") && !out_a.contains("from_b"));
    assert!(out_b.contains("from_b") && !out_b.contains("from_a"));
    assert_eq!((exit_a, exit_b), (Some(1), Some(2)));
}

#[test]
fn flow_control_pauses_until_ack() {
    let manager = PtyManager::new(PtyConfig {
        flow_high_water: 1,
        ..PtyConfig::default()
    });
    let (sink, rx) = channel_sink();
    let (shell, args) = one_shot("flow_marker", 0);
    let mut opts = options(shell, args, true);
    opts.flow_control = true;
    let created = manager.create(opts, sink).unwrap();
    // Collect what arrives without acking: the reader stops after one chunk.
    let mut out = String::new();
    let mut acked_all = false;
    let deadline = Instant::now() + WAIT;
    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok((_, PtyEvent::Data(text))) => {
                out.push_str(&text);
                if acked_all {
                    manager.ack(&created.id, utf16_len(&text)).ok();
                }
            }
            Ok((_, PtyEvent::Exit(code))) => {
                assert_eq!(code, 0);
                break;
            }
            Err(_) => {
                // Stalled: grant credit for everything seen so far, then keep acking.
                acked_all = true;
                manager.ack(&created.id, usize::MAX).ok();
            }
        }
        assert!(
            Instant::now() < deadline,
            "flow control never resumed: {out:?}"
        );
    }
    assert!(out.contains("flow_marker"));
}

#[test]
fn utf8_stream_holds_split_code_points() {
    let mut decoder = Utf8Stream::default();
    let bytes = "a中b".as_bytes();
    assert_eq!(decoder.push(&bytes[..2]), "a");
    assert_eq!(decoder.push(&bytes[2..4]), "中");
    assert_eq!(decoder.push(&bytes[4..]), "b");
    assert_eq!(decoder.push(&[0xff, b'x']), "\u{FFFD}x");
    assert_eq!(decoder.push(&[0xe4]), "");
    assert_eq!(decoder.finish(), "\u{FFFD}");
}

#[test]
fn env_resolution_matches_ts() {
    let base = EnvMap::from_pairs([("TERM", "dumb"), ("CI", "1"), ("LANG", "C")]);
    let env = resolve_env(&base);
    assert_eq!(env.get("TERM"), Some("xterm-256color"));
    assert_eq!(env.get("COLORTERM"), Some("truecolor"));
    assert_eq!(env.get("CI"), None);
    let fallback = if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    };
    assert_eq!(env.get("LANG"), Some(fallback));
    assert_eq!(env.get("LC_CTYPE"), Some(fallback));
    assert_eq!(env.get("LC_ALL"), None);

    let inherited = EnvMap::from_pairs([
        ("LC_CTYPE", "zh_CN.UTF-8"),
        ("LC_ALL", "POSIX"),
        ("COLORTERM", " 24bit "),
    ]);
    let env = resolve_env(&inherited);
    assert_eq!(env.get("LANG"), Some("zh_CN.UTF-8"));
    assert_eq!(env.get("LC_CTYPE"), Some("zh_CN.UTF-8"));
    assert_eq!(env.get("LC_ALL"), Some("zh_CN.UTF-8"));
    assert_eq!(env.get("COLORTERM"), Some("24bit"));
}

#[test]
fn shell_and_cwd_resolution_match_ts() {
    let env = EnvMap::from_process();
    let shell = resolve_shell(&env).unwrap();
    if cfg!(windows) {
        assert!(
            ["pwsh.exe", "powershell.exe", "cmd.exe"].contains(&shell.as_str())
                || env.get("ComSpec") == Some(shell.as_str()),
            "{shell}"
        );
    }
    let empty = EnvMap::default();
    if cfg!(windows) {
        assert_eq!(
            resolve_shell(&empty),
            Err("No usable Windows shell found for terminal startup".to_owned())
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_string_lossy().into_owned();
    assert_eq!(resolve_cwd(Some(&d), &env).unwrap(), d);
    let missing = dir.path().join("missing").to_string_lossy().into_owned();
    let fallback = resolve_cwd(Some(&missing), &env).unwrap();
    assert_ne!(fallback, missing);
    assert!(std::path::Path::new(&fallback).is_dir());
}

#[test]
fn bad_shell_reports_ts_error() {
    let manager = PtyManager::default();
    let (sink, _rx) = channel_sink();
    assert_eq!(
        manager.create(
            CreateOptions {
                cols: 0,
                rows: 24,
                ..CreateOptions::default()
            },
            Arc::clone(&sink)
        ),
        Err(PtyError::App(BAD_SIZE_MESSAGE.to_owned()))
    );
    let error = manager
        .create(
            options("definitely-not-a-shell-xyz".to_owned(), Vec::new(), true),
            sink,
        )
        .unwrap_err();
    let PtyError::App(message) = error else {
        panic!("{error:?}")
    };
    assert!(
        message.starts_with("Failed to start terminal with shell 'definitely-not-a-shell-xyz'"),
        "{message}"
    );
    assert_eq!(manager.ids(), Vec::<String>::new());
}

#[test]
fn conpty_dll_points_at_an_existing_file() {
    if let Some(dll) = conpty_dll() {
        assert!(dll.is_file());
        assert_eq!(dll.file_name().and_then(|n| n.to_str()), Some("conpty.dll"));
    }
}
