use std::sync::mpsc;
use std::time::Duration;

use super::*;

const WAIT: Duration = Duration::from_secs(5);

fn fast_hub() -> WatchHub {
    WatchHub::new(HubConfig {
        debounce: Duration::from_millis(60),
        max_wait: Duration::from_millis(400),
        max_pending_paths: 8,
    })
}

fn channel_sink() -> (WatchSink, mpsc::Receiver<(String, WatchSignal)>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: WatchSink = Arc::new(move |id: &str, signal| {
        let _ = lock(&tx).send((id.to_owned(), signal));
    });
    (sink, rx)
}

fn dir_str(dir: &tempfile::TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

/// Collects change events until `pred` matches one or the deadline passes.
fn wait_for(
    rx: &mpsc::Receiver<(String, WatchSignal)>,
    pred: impl Fn(&WatchSignal) -> bool,
) -> Vec<WatchSignal> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok((_, signal)) => {
                let hit = pred(&signal);
                seen.push(signal);
                if hit {
                    return seen;
                }
            }
            Err(_) => break,
        }
    }
    panic!("no matching signal; saw {seen:?}");
}

/// Drains signals for `quiet` and returns them.
fn drain(rx: &mpsc::Receiver<(String, WatchSignal)>, quiet: Duration) -> Vec<WatchSignal> {
    let mut out = Vec::new();
    while let Ok((_, signal)) = rx.recv_timeout(quiet) {
        out.push(signal);
    }
    out
}

fn changed(signal: &WatchSignal) -> Option<&str> {
    match signal {
        WatchSignal::Change { event, .. } => event.changed_path.as_deref(),
        WatchSignal::Closed { .. } => None,
    }
}

fn expected(dir: &str, name: &str) -> String {
    harness_fs::node_join(dir, name)
}

#[test]
fn create_modify_rename_delete_report_single_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir_str(&dir);
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    let id = hub.watch(&root, WatchOptions::default(), sink).unwrap();
    assert_eq!(id, "0");

    let a = dir.path().join("a.txt");
    std::fs::write(&a, "1").unwrap();
    let want = expected(&root, "a.txt");
    wait_for(&rx, |s| changed(s) == Some(want.as_str()));
    drain(&rx, Duration::from_millis(200));

    std::fs::write(&a, "22").unwrap();
    wait_for(&rx, |s| changed(s) == Some(want.as_str()));
    drain(&rx, Duration::from_millis(200));

    // rename touches two paths in one window -> path-less event (TS parity).
    std::fs::rename(&a, dir.path().join("b.txt")).unwrap();
    let seen = wait_for(&rx, |s| matches!(s, WatchSignal::Change { .. }));
    let last = seen.last().unwrap();
    assert!(matches!(last, WatchSignal::Change { event, .. } if event.dir_path == root));
    drain(&rx, Duration::from_millis(200));

    std::fs::remove_file(dir.path().join("b.txt")).unwrap();
    let want_b = expected(&root, "b.txt");
    wait_for(&rx, |s| changed(s) == Some(want_b.as_str()));
    assert!(hub.is_active(&id));
    assert!(hub.unwatch(&id));
    assert!(!hub.unwatch(&id));
    assert!(hub.is_empty());
}

#[test]
fn burst_is_coalesced_into_one_event() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir_str(&dir);
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    hub.watch(&root, WatchOptions::default(), sink).unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
    }
    let first = wait_for(&rx, |_| true);
    let rest = drain(&rx, Duration::from_millis(300));
    let all: Vec<_> = first.into_iter().chain(rest).collect();
    // 5 writes inside one debounce window -> at most a couple of flushes, and
    // a multi-path window never carries changedPath.
    assert!(all.len() <= 2, "{all:?}");
    assert!(matches!(&all[0], WatchSignal::Change { event, .. } if event.changed_path.is_none()));
}

#[test]
fn max_wait_flushes_during_continuous_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir_str(&dir);
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    hub.watch(&root, WatchOptions::default(), sink).unwrap();
    let file = dir.path().join("log.txt");
    let start = Instant::now();
    let mut got_first = None;
    // Writes every 20 ms (< 60 ms debounce) for 1.2 s: only max-wait can flush.
    while start.elapsed() < Duration::from_millis(1200) {
        std::fs::write(&file, start.elapsed().as_millis().to_string()).unwrap();
        if got_first.is_none() && rx.try_recv().is_ok() {
            got_first = Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let first = got_first.expect("max-wait flush during continuous writes");
    assert!(first < Duration::from_millis(1000), "{first:?}");
}

#[test]
fn ignored_paths_are_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir_str(&dir);
    std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    std::fs::create_dir(dir.path().join("node_modules")).unwrap();
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    let rules = IgnoreRules::build(&root, true, &["node_modules/".to_owned()]).unwrap();
    hub.watch(
        &root,
        WatchOptions {
            recursive: true,
            ignore: Some(rules),
        },
        sink,
    )
    .unwrap();
    std::fs::write(dir.path().join("x.log"), "x").unwrap();
    std::fs::write(dir.path().join("node_modules").join("m.js"), "x").unwrap();
    let quiet = drain(&rx, Duration::from_millis(400));
    assert!(quiet.is_empty(), "ignored paths leaked: {quiet:?}");
    std::fs::write(dir.path().join("keep.txt"), "x").unwrap();
    let want = expected(&root, "keep.txt");
    wait_for(&rx, |s| changed(s) == Some(want.as_str()));
}

#[test]
fn recursive_reports_nested_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir_str(&dir);
    std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    hub.watch(
        &root,
        WatchOptions {
            recursive: true,
            ignore: None,
        },
        sink,
    )
    .unwrap();
    std::fs::write(dir.path().join("a").join("b").join("deep.txt"), "x").unwrap();
    let want = expected(&root, "a/b/deep.txt");
    let seen = wait_for(&rx, |s| {
        changed(s) == Some(want.as_str())
            || matches!(s, WatchSignal::Change { event, .. } if event.changed_path.is_none())
    });
    assert_ne!(seen.len(), 0);
}

#[test]
fn overflow_degrades_to_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir_str(&dir);
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    hub.watch(&root, WatchOptions::default(), sink).unwrap();
    for i in 0..40 {
        std::fs::write(dir.path().join(format!("o{i}.txt")), "x").unwrap();
    }
    let seen = wait_for(&rx, |s| {
        matches!(s, WatchSignal::Change { rescan: true, .. })
    });
    assert!(seen.iter().all(|s| changed(s).is_none()));
}

#[test]
fn deleting_watched_dir_emits_final_event_and_closes() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("watched");
    std::fs::create_dir(&target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    let id = hub
        .watch(&target_str, WatchOptions::default(), sink)
        .unwrap();
    std::fs::write(target.join("f.txt"), "x").unwrap();
    drain(&rx, Duration::from_millis(200));
    std::fs::remove_dir_all(&target).unwrap();
    let seen = wait_for(&rx, |s| matches!(s, WatchSignal::Closed { .. }));
    let n = seen.len();
    assert!(n >= 2);
    assert!(
        matches!(&seen[n - 2], WatchSignal::Change { event, .. } if event.changed_path.is_none() && event.dir_path == target_str)
    );
    assert!(!hub.is_active(&id));
    assert!(!hub.unwatch(&id));
}

#[test]
fn missing_path_fails_like_ts() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope").to_string_lossy().into_owned();
    let hub = fast_hub();
    let (sink, _rx) = channel_sink();
    let WatchError::App(message) = hub
        .watch(&missing, WatchOptions::default(), sink)
        .unwrap_err();
    assert!(
        message.starts_with(&format!("无法监视目录 '{missing}': ENOENT")),
        "{message}"
    );
}

#[test]
fn unwatch_stops_events_and_dispose_all_clears() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir_str(&dir);
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    let a = hub
        .watch(&root, WatchOptions::default(), Arc::clone(&sink))
        .unwrap();
    let b = hub.watch(&root, WatchOptions::default(), sink).unwrap();
    assert_ne!(a, b);
    hub.unwatch(&a);
    std::fs::write(dir.path().join("z.txt"), "x").unwrap();
    let seen = {
        let deadline = Instant::now() + WAIT;
        let mut ids = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left.min(Duration::from_millis(400))) {
                Ok((id, _)) => ids.push(id),
                Err(_) => break,
            }
        }
        ids
    };
    assert!(seen.iter().all(|id| *id == b), "{seen:?}");
    assert_ne!(seen.len(), 0);
    hub.dispose_all();
    assert!(hub.is_empty());
}

#[test]
fn event_serializes_like_ts() {
    let event = FileWatchEvent {
        dir_path: "/w".to_owned(),
        changed_path: None,
    };
    assert_eq!(
        serde_json::to_string(&event).unwrap(),
        r#"{"dirPath":"/w"}"#
    );
}

#[test]
fn deleting_empty_watched_dir_closes() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("empty");
    std::fs::create_dir(&target).unwrap();
    let target_str = target.to_string_lossy().into_owned();
    let hub = fast_hub();
    let (sink, rx) = channel_sink();
    let id = hub
        .watch(&target_str, WatchOptions::default(), sink)
        .unwrap();
    // A sibling change seen through the parent watch must not leak.
    std::fs::write(parent.path().join("sibling.txt"), "x").unwrap();
    assert_eq!(drain(&rx, Duration::from_millis(300)), Vec::new());
    std::fs::remove_dir(&target).unwrap();
    let seen = wait_for(&rx, |s| matches!(s, WatchSignal::Closed { .. }));
    assert!(matches!(
        seen.last(),
        Some(WatchSignal::Closed {
            reason: CloseReason::IdentityChanged
        })
    ));
    assert!(!hub.is_active(&id));
}
