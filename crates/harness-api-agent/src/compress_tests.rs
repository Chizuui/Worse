#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};

use super::*;
use crate::compress_tools;

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn call(id: &str, name: &str) -> Value {
    json!({"role": "assistant", "content": "", "tool_calls": [
        {"id": id, "type": "function", "function": {"name": name, "arguments": "{\"path\":\"src/lib.rs\"}"}}
    ]})
}

fn result(id: &str, text: &str) -> Value {
    json!({"role": "tool", "tool_call_id": id, "content": text})
}

fn answer(text: &str) -> Value {
    json!({"role": "assistant", "content": text})
}

fn sample() -> Vec<Value> {
    vec![
        user("fix the login bug"),
        call("c1", "read_file"),
        result("c1", "fn login() { token_expiry }"),
        answer("fixed login in src/lib.rs"),
        user("now add tests"),
        call("c2", "edit_file"),
        result("c2", "ok"),
        answer("tests added"),
        user("thanks"),
    ]
}

#[test]
fn fold_replaces_range_and_decompress_restores_it() {
    let messages = sample();
    let mut folds = Folds::default();
    let id = folds.compress(&messages, 0, 3, "Fixed login token expiry in src/lib.rs:12", false).unwrap();
    let view = folds.view(&messages);
    assert_eq!(view.len(), 6);
    let carrier = view[0]["content"].as_str().unwrap();
    assert!(carrier.contains(&format!("f{id}")) && carrier.contains("m0-m3"), "{carrier}");
    assert!(carrier.contains("src/lib.rs:12"));
    assert_eq!(view[1]["content"], "now add tests");
    let fold = folds.decompress(&messages, id).unwrap();
    assert_eq!((fold.start, fold.end), (0, 4));
    assert_eq!(folds.view(&messages), messages);
}

#[test]
fn fold_never_splits_a_call_from_its_result() {
    let messages = sample();
    let mut folds = Folds::default();
    assert!(folds.compress(&messages, 2, 3, "x", false).unwrap_err().contains("tool result"));
    assert!(folds.compress(&messages, 0, 1, "x", false).unwrap_err().contains("m2"));
    assert!(folds.compress(&messages, 0, 2, "", false).unwrap_err().contains("empty"));
    assert!(folds.compress(&messages, 0, 99, "x", false).is_err());
}

#[test]
fn outer_fold_absorbs_inner_and_partial_overlap_fails() {
    let messages = sample();
    let mut folds = Folds::default();
    let inner = folds.compress(&messages, 0, 3, "inner", false).unwrap();
    assert!(folds.compress(&messages, 1, 6, "x", false).unwrap_err().contains(&format!("f{inner}")));
    let outer = folds.compress(&messages, 0, 7, "outer", false).unwrap();
    assert_ne!(inner, outer);
    assert_eq!(folds.folds.len(), 1);
    assert_eq!(folds.view(&messages).len(), 2);
}

#[test]
fn state_round_trips_through_json() {
    let messages = sample();
    let mut folds = Folds::default();
    folds.compress(&messages, 4, 7, "tests", false).unwrap();
    folds.base = 10;
    let back = Folds::from_json(Some(&folds.to_json()));
    assert_eq!(back.folds, folds.folds);
    assert_eq!(back.base, 10);
    let mut again = back;
    let next = again.compress(&messages, 10, 13, "x", false);
    assert!(next.is_ok() || next.unwrap_err().contains("cuts"));
}

#[test]
fn fit_folds_oldest_turns_before_dropping_anything() {
    let big = "z".repeat(60_000);
    let mut messages = Vec::new();
    for i in 0..12 {
        messages.push(user(&format!("task {i}")));
        messages.push(call(&format!("c{i}"), "read_file"));
        messages.push(result(&format!("c{i}"), &big));
        messages.push(answer(&format!("done {i}")));
    }
    let count = messages.len();
    let mut folds = Folds::default();
    folds.fit(&mut messages);
    assert_eq!(messages.len(), count, "originals are kept");
    assert!(folds.view_size(&messages) <= VIEW_CHARS);
    assert!(folds.folds.iter().all(|f| f.auto));
    assert_eq!(folds.folds.len(), 1, "adjacent automatic folds merge");
    assert!(folds.folds[0].summary.contains("task 0"));
    let hits = compress_tools::search(&folds, &messages, "task 0").unwrap();
    assert!(hits.contains("folded in f"), "{hits}");
}

#[test]
fn status_lists_valid_ranges_and_nudge_fires_once_per_step() {
    let mut messages = sample();
    let mut folds = Folds::default();
    let status = compress_tools::status(&folds, &messages);
    assert!(status.contains("m0-m3"), "{status}");
    assert!(status.contains("m4-m7"), "{status}");
    assert!(compress_tools::nudge(&mut folds, &messages).is_none());
    let big = "q".repeat(170_000);
    messages.insert(2, result("c1", &big));
    messages.remove(3);
    for _ in 0..KEEP_TAIL {
        messages.push(answer("tail"));
    }
    let note = compress_tools::nudge(&mut folds, &messages).unwrap();
    assert!(note.contains("context_compress") && note.contains("m0-m2 ~42"), "{note}");
    assert!(compress_tools::nudge(&mut folds, &messages).is_none());
}
