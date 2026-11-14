use std::path::Path;

use rusqlite::Connection;
use serde_json::json;

use super::*;

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        assert!(std::fs::create_dir_all(parent).is_ok());
    }
    assert!(std::fs::write(path, text).is_ok());
}

/// Writes the three fixture sources under `root` (`zcode/`, `claude/`).
pub(crate) fn fixtures(root: &Path) -> ImportRoots {
    let zcode = root.join("zcode");
    let legacy = json!({
        "meta": {"taskId": "task-1", "title": "Legacy task", "workspacePath": "/w/legacy",
                 "createdAt": 1000, "updatedAt": 2000},
        "messages": [
            {"role": "user", "content": "hello legacy"},
            {"role": "assistant", "content": [{"type": "text", "text": "hi"}],
             "tools": [{"title": "Read file"}, {"toolName": "bash"}]},
            {"role": "user", "content": "second"},
            {"role": "assistant", "content": "done"}
        ]
    });
    write(
        &zcode.join("v2/sessions/ws1/task-1.json"),
        &legacy.to_string(),
    );
    write(
        &zcode.join("v2/sessions/ws1/task-2.deleted.json"),
        &legacy.to_string(),
    );
    let db = zcode.join("cli/db/db.sqlite");
    if let Some(parent) = db.parent() {
        assert!(std::fs::create_dir_all(parent).is_ok());
    }
    let c = Connection::open(&db);
    assert!(c.is_ok());
    if let Ok(c) = c {
        let setup = c.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT,
                slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
             CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
             INSERT INTO session VALUES('cli-1','p',NULL,'s','/w/cli','CLI session','1',10,30,NULL);
             INSERT INTO session VALUES('cli-archived','p',NULL,'s','/w/cli','Old','1',10,20,15);
             INSERT INTO session VALUES('cli-child','p','cli-1','s','/w/cli','Child','1',10,20,NULL);
             INSERT INTO message VALUES('m1','cli-1',11,11,'{\"role\":\"user\"}',1);
             INSERT INTO message VALUES('m2','cli-1',12,12,'{\"role\":\"assistant\"}',2);
             INSERT INTO part VALUES('p1','m1','cli-1',11,11,'{\"type\":\"text\",\"text\":\"cli question\"}',1);
             INSERT INTO part VALUES('p2','m2','cli-1',12,12,'{\"type\":\"tool\",\"tool\":\"bash\",\"state\":{\"title\":\"List files\"}}',1);
             INSERT INTO part VALUES('p3','m2','cli-1',12,12,'{\"type\":\"text\",\"text\":\"cli answer\"}',2);
             INSERT INTO part VALUES('p4','m2','cli-1',12,12,'{\"type\":\"text\",\"text\":\"hidden\",\"synthetic\":true}',3);",
        );
        assert!(setup.is_ok(), "{setup:?}");
    }
    let claude = root.join("claude");
    let lines = [
        json!({"type": "user", "isMeta": true, "message": {"content": "<command-name>/init</command-name>"}}),
        json!({"type": "user", "cwd": "/w/claude", "timestamp": "2026-01-01T00:00:00Z",
               "message": {"role": "user", "content": "<ide_opened_file>x</ide_opened_file>fix the bug"}}),
        json!({"type": "assistant", "timestamp": "2026-01-01T00:00:05Z", "message": {"content": [
            {"type": "text", "text": "Looking"}, {"type": "tool_use", "name": "Read", "input": {}}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "file"}]}}),
        json!({"type": "assistant", "isSidechain": true, "message": {"content": "side"}}),
        json!({"type": "assistant", "timestamp": "2026-01-01T00:00:09Z", "message": {"content": [
            {"type": "text", "text": "Fixed"}]}}),
    ];
    let jsonl = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    write(&claude.join("projects/-w-claude/abc-123.jsonl"), &jsonl);
    write(&claude.join("settings.json"), "{\"must\":\"not be read\"}");
    ImportRoots {
        zcode: Some(zcode),
        claude: Some(claude),
        codex: None,
    }
}

#[tokio::test]
async fn scans_all_three_sources() {
    let dir = tempfile::tempdir();
    assert!(dir.is_ok());
    let Ok(dir) = dir else { return };
    let roots = fixtures(dir.path());
    let all = scan(&roots, &[]).await;
    let ids: Vec<&str> = all.iter().map(|s| s.external_id.as_str()).collect();
    assert_eq!(all.len(), 3, "{ids:?}");
    assert!(ids.contains(&"task-1") && ids.contains(&"cli-1") && ids.contains(&"abc-123"));

    let legacy = all.iter().find(|s| s.external_id == "task-1");
    assert!(legacy.is_some_and(|s| s.turns.len() == 2
        && s.turns[0].tools == ["Read file", "bash"]
        && s.title == "Legacy task"));

    let cli = all.iter().find(|s| s.external_id == "cli-1");
    assert!(cli.is_some_and(|s| s.turns.len() == 1
        && s.turns[0].user_text == "cli question"
        && s.turns[0].assistant_text == "cli answer"
        && s.turns[0].tools == ["List files"]));

    let claude = all.iter().find(|s| s.external_id == "abc-123");
    assert!(claude.is_some_and(|s| s.turns.len() == 1
        && s.turns[0].user_text == "fix the bug"
        && s.turns[0].assistant_text == "Looking\n\nFixed"
        && s.turns[0].tools == ["Read"]
        && s.workspace_path == "/w/claude"
        && s.title == "fix the bug"
        && s.key() == "claude-code:abc-123"));

    let only_claude = scan(&roots, &[ImportSource::ClaudeCode]).await;
    assert_eq!(only_claude.len(), 1);
}

#[test]
fn strips_local_tags() {
    assert_eq!(
        claude::strip_tags("<command-name>/x</command-name> a <b>keep</b>"),
        "a <b>keep</b>"
    );
}
