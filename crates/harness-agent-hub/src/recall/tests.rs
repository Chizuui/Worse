#![allow(clippy::useless_vec, clippy::assert_is_empty)]
use super::*;

fn store() -> (Store, PathBuf) {
    let home =
        std::env::temp_dir().join(format!("harness-recall-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir_all(&home).unwrap();
    (Store::new(&home), home)
}

fn fact(text: &str, project: Option<&str>) -> NewMemory {
    NewMemory {
        text: text.into(),
        project: project.map(str::to_owned),
        ..NewMemory::default()
    }
}

#[test]
fn add_reload_and_scopes() {
    let (store, home) = store();
    let a = store
        .add(fact("The API server listens on port 8642", Some("/p/one")))
        .unwrap();
    store.add(fact("Reply in Indonesian", None)).unwrap();
    store
        .add(fact("Other project fact", Some("/p/two")))
        .unwrap();
    let all = Store::new(&home).load();
    assert_eq!(all.len(), 3);
    assert_eq!(visible(&all, Some("/p/one")).len(), 2);
    assert_eq!(visible(&all, None).len(), 1);
    assert!(matches!(a, Added::New(_)));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn the_same_text_is_stored_once_per_scope() {
    let (store, home) = store();
    let first = store
        .add(fact("Tests run with  cargo test", Some("/p")))
        .unwrap();
    let again = store
        .add(fact("tests run with cargo test", Some("/p")))
        .unwrap();
    assert_eq!(again, Added::Duplicate(first.id().to_owned()));
    assert!(matches!(
        store.add(fact("tests run with cargo test", None)).unwrap(),
        Added::New(_)
    ));
    assert_eq!(store.load().len(), 2);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn replace_supersedes_and_forget_removes() {
    let (store, home) = store();
    let old = store.add(fact("Deploys go through Jenkins", None)).unwrap();
    let new = store
        .add(NewMemory {
            replaces: Some(old.id().to_owned()),
            ..fact("Deploys go through GitHub Actions", None)
        })
        .unwrap();
    let live = store.load();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, new.id());
    assert!(store
        .add(NewMemory {
            replaces: Some("nope".into()),
            ..fact("x y z", None)
        })
        .is_err());
    assert_eq!(store.forget(new.id()).unwrap(), 1);
    assert!(store.load().is_empty());
    // By exact text when no id matches.
    store.add(fact("Remember the milk", None)).unwrap();
    assert_eq!(store.forget("remember the  MILK").unwrap(), 1);
    assert_eq!(store.forget("nothing like it").unwrap(), 0);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn preferences_are_pinned_and_pins_can_change() {
    let (store, home) = store();
    let pref = store
        .add(NewMemory {
            kind: Some(Kind::Preference),
            ..fact("Short answers", None)
        })
        .unwrap();
    let plain = store.add(fact("Port 8642", None)).unwrap();
    assert!(
        store
            .load()
            .iter()
            .find(|m| m.id == pref.id())
            .unwrap()
            .pinned
    );
    assert!(store.set_pinned(plain.id(), true).unwrap());
    assert!(store.load().iter().all(|m| m.pinned));
    assert!(store.set_pinned(pref.id(), false).unwrap());
    assert!(
        !store
            .load()
            .iter()
            .find(|m| m.id == pref.id())
            .unwrap()
            .pinned
    );
    assert!(!store.set_pinned("unknown", true).unwrap());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn credentials_are_refused_and_redacted() {
    let (store, home) = store();
    for text in [
        "the key is sk-abcdefghijklmnopqrstuvwxyz012345",
        "password = hunter2secret9",
        "\"api_key\": \"a1b2c3d4e5f6\"",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
        "looks fine\u{200B}but hides text",
    ] {
        assert!(store.add(fact(text, None)).is_err(), "{text}");
    }
    // Words that only mention a credential are fine.
    for text in [
        "The token is read from the COMMAND_CODE_API_KEY variable",
        "password: required field in the login form",
        "commit 3f2a9c1e5b7d8a6f4c2e1b0a9d8c7b6a5f4e3d2c fixed it",
    ] {
        assert!(store.add(fact(text, None)).is_ok(), "{text}");
    }
    let cleaned = redact("use token=abc12345xyz then call sk-abcdefghijklmnopqrstuvwxyz with care");
    assert!(
        !cleaned.contains("abc12345xyz") && !cleaned.contains("sk-abc"),
        "{cleaned}"
    );
    assert!(cleaned.contains("[redacted]") && cleaned.ends_with("with care"));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn an_episode_takes_the_place_of_the_sessions_earlier_one() {
    let (store, home) = store();
    store
        .put_episode("s1", "/p/", "Fix pairing", "Codex", "Asked: fix it")
        .unwrap();
    store
        .put_episode(
            "s1",
            "/p",
            "Fix pairing",
            "Claude Code",
            "Asked: fix it | Result: done",
        )
        .unwrap();
    store
        .put_episode(
            "s2",
            "/p",
            "Other",
            "Codex",
            "Asked: other, token=abc12345xyz",
        )
        .unwrap();
    let live = store.load();
    assert_eq!(live.len(), 2);
    let first = live
        .iter()
        .find(|m| m.session.as_deref() == Some("s1"))
        .unwrap();
    assert_eq!(first.agent.as_deref(), Some("Claude Code"));
    assert_eq!(first.project.as_deref(), Some("/p"));
    assert!(first.text.ends_with("done"));
    assert!(!live.iter().any(|m| m.text.contains("abc12345xyz")));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn a_torn_last_line_is_skipped_and_the_next_write_survives() {
    let (store, home) = store();
    store.add(fact("first fact", None)).unwrap();
    let mut file = OpenOptions::new()
        .append(true)
        .open(store.log_path())
        .unwrap();
    file.write_all(b"{\"op\":\"put\",\"id\":\"m1\",\"te")
        .unwrap();
    drop(file);
    assert_eq!(store.load().len(), 1);
    store.add(fact("second fact", None)).unwrap();
    assert_eq!(store.load().len(), 2);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn dead_records_are_compacted_away() {
    let (store, home) = store();
    let keep = store.add(fact("kept forever", None)).unwrap();
    for i in 0..80 {
        store
            .put_episode("same-session", "/p", "t", "a", &format!("round {i}"))
            .unwrap();
    }
    let lines = fs::read_to_string(store.log_path())
        .unwrap()
        .lines()
        .count();
    assert!(lines < 70, "log has {lines} lines after 81 writes");
    let live = store.load();
    assert_eq!(live.len(), 2);
    assert!(live.iter().any(|m| m.id == keep.id()));
    assert!(live.iter().any(|m| m.text == "round 79"));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn concurrent_writers_lose_nothing() {
    let (store, home) = store();
    let mut threads = Vec::new();
    for t in 0..6 {
        let store = store.clone();
        threads.push(std::thread::spawn(move || {
            for i in 0..10 {
                store
                    .add(fact(&format!("writer {t} fact number {i}"), None))
                    .unwrap();
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(store.load().len(), 60);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn expired_memories_are_not_live() {
    let (store, home) = store();
    store
        .add(NewMemory {
            ttl_days: Some(1),
            ..fact("temporary", None)
        })
        .unwrap();
    assert_eq!(store.load().len(), 1);
    assert!(store.fold(now_ms() + 2 * 24 * 60 * 60 * 1000).0.is_empty());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn config_defaults_on_and_round_trips() {
    let (store, home) = store();
    assert_eq!(store.config(), Config::default());
    assert!(store.config().recall && store.config().capture && store.config().graph);
    store
        .set_config(Config {
            recall: false,
            ..Config::default()
        })
        .unwrap();
    assert_eq!(
        store.config(),
        Config {
            recall: false,
            ..Config::default()
        }
    );
    // A file written before the graph switch existed still reads, with the switch on.
    fs::write(
        home.join("memory").join("config.json"),
        "{\"recall\":true,\"capture\":false}",
    )
    .unwrap();
    assert_eq!(
        store.config(),
        Config {
            recall: true,
            capture: false,
            graph: true,
            concise: true
        }
    );
    let _ = fs::remove_dir_all(home);
}

fn memory(id: &str, at: i64, kind: Kind, text: &str) -> Memory {
    Memory {
        id: id.into(),
        at,
        kind,
        text: text.into(),
        project: Some("/p".into()),
        pinned: false,
        session: None,
        agent: None,
        title: None,
        key: None,
        replaces: None,
        expires: None,
    }
}

#[test]
fn words_split_identifiers_and_drop_filler() {
    assert_eq!(
        tokens("TokenPairing.is_live()"),
        ["token", "pairing", "tokenpairing", "live"]
    );
    assert_eq!(
        tokens("fix the HTTPServer tokens"),
        ["fix", "http", "server", "httpserver", "token"]
    );
    assert_eq!(
        tokens("gimana biar bisa build di Windows"),
        ["build", "window"]
    );
    assert!(tokens("the and of").is_empty());
}

#[test]
fn search_ranks_by_words_then_recency() {
    let all = vec![
        memory(
            "a",
            1_000,
            Kind::Fact,
            "The pairing token expires after ten minutes",
        ),
        memory(
            "b",
            2_000,
            Kind::Decision,
            "Use GitHub Actions for the Windows build",
        ),
        memory(
            "c",
            3_000,
            Kind::Fact,
            "TokenPairing reads the clock once per check",
        ),
        memory(
            "d",
            4_000,
            Kind::Fact,
            "The wallpaper is blurred on a worker thread",
        ),
    ];
    let refs: Vec<&Memory> = all.iter().collect();
    let ids = |text: &str, need: usize| -> Vec<&str> {
        search(
            &refs,
            &Query {
                text,
                limit: 5,
                need,
                kind: None,
            },
        )
        .iter()
        .map(|h| h.memory.id.as_str())
        .collect()
    };
    // Both pairing memories match; the one with both words comes first.
    assert_eq!(ids("pairing token expiry", 1), ["a", "c"]);
    assert_eq!(ids("windows build", 1), ["b"]);
    assert!(ids("database migration", 1).is_empty());
    // Two words needed: "pairing" alone is not enough for c… but c has token+pairing through its identifier.
    assert_eq!(ids("pairing clock", 2), ["c"]);
    // Same words, different age: the newer one wins the tie.
    let twins = vec![
        memory("old", 1_000, Kind::Fact, "deploy uses the staging cluster"),
        memory("new", 9_000, Kind::Fact, "deploy uses the staging cluster"),
    ];
    let refs: Vec<&Memory> = twins.iter().collect();
    let hits = search(
        &refs,
        &Query {
            text: "staging deploy",
            limit: 5,
            need: 1,
            kind: None,
        },
    );
    assert_eq!(hits[0].memory.id, "new");
    let only = search(
        &refs,
        &Query {
            text: "staging",
            limit: 5,
            need: 1,
            kind: Some(Kind::Decision),
        },
    );
    assert!(only.is_empty());
}

#[test]
fn recall_block_has_pinned_related_and_earlier_sessions() {
    let now = 10 * 24 * 60 * 60 * 1000;
    let hour = 60 * 60 * 1000;
    let mut pinned = memory(
        "pin",
        now - 5 * hour,
        Kind::Preference,
        "Reply in Indonesian, casual",
    );
    pinned.pinned = true;
    pinned.project = None;
    let mut related = memory(
        "rel",
        now - 3 * 24 * hour,
        Kind::Decision,
        "Pairing tokens expire after ten minutes because of replay attacks",
    );
    related.agent = Some("Claude Code".into());
    let unrelated = memory(
        "unr",
        now - hour,
        Kind::Fact,
        "The wallpaper is blurred on a worker thread",
    );
    let mut episode = memory(
        "epi",
        now - 2 * hour,
        Kind::Episode,
        "Asked: add the Windows build | Result: zip uploaded",
    );
    episode.session = Some("s-old".into());
    episode.agent = Some("Codex".into());
    episode.title = Some("Windows build".into());
    let mut own = memory(
        "own",
        now - hour,
        Kind::Episode,
        "Asked: pairing token expiry",
    );
    own.session = Some("s-now".into());
    let mut elsewhere = memory(
        "els",
        now - hour,
        Kind::Fact,
        "pairing token note of another project",
    );
    elsewhere.project = Some("/other".into());
    let all = vec![pinned, related, unrelated, episode, own, elsewhere];
    let block = recall_block(&RecallInput {
        memories: &all,
        project: Some("/p"),
        session: "s-now",
        query: "why does the pairing token expire so fast?",
        now,
    });
    assert!(block.starts_with("<harness-recall>"), "{block}");
    assert!(block.contains("Always:\n- [pin] preference: Reply in Indonesian"));
    assert!(block.contains(
        "Related to this request:\n- [rel] decision · 3d ago · Claude Code: Pairing tokens"
    ));
    assert!(block.contains(
        "Earlier sessions in this project:\n- [epi] 2h ago · Codex · \"Windows build\": Asked"
    ));
    for hidden in ["[unr]", "[own]", "[els]"] {
        assert!(!block.contains(hidden), "{hidden} in {block}");
    }
    assert!(block.ends_with(INSTRUCTIONS));
    // Nothing stored: only the instructions.
    let empty = recall_block(&RecallInput {
        memories: &[],
        project: Some("/p"),
        session: "s",
        query: "hi",
        now,
    });
    assert_eq!(empty, INSTRUCTIONS);
}

#[test]
fn recall_block_stays_inside_its_budget() {
    let now = 1_000_000_000;
    let all: Vec<Memory> = (0..60)
        .map(|i| {
            let mut m = memory(
                &format!("m{i}"),
                now - i64::from(i) * 1000,
                Kind::Fact,
                &format!("pairing token detail number {i} {}", "word ".repeat(80)),
            );
            m.pinned = i % 2 == 0;
            m
        })
        .collect();
    let block = recall_block(&RecallInput {
        memories: &all,
        project: Some("/p"),
        session: "s",
        query: "pairing token",
        now,
    });
    assert!(
        block.chars().count() < 3_200 + INSTRUCTIONS.len() + 400,
        "{}",
        block.chars().count()
    );
    assert!(block.contains("Always:") && block.contains("Related to this request:"));
}

#[test]
fn episode_text_keeps_first_ask_last_asks_result_tail_and_files() {
    let turns = vec![
        EpisodeTurn { user: "fix the pairing expiry bug".into(), assistant: "Looking.".into(), files: vec!["src/auth.rs".into()] },
        EpisodeTurn { user: "also add a test".into(), assistant: format!("{} The check now takes the time from the caller and a frozen-clock test covers it.", "preamble ".repeat(80)), files: vec!["src/auth.rs".into(), "src/tests.rs".into()] },
    ];
    let text = episode_text(&turns);
    assert!(
        text.starts_with("Asked: fix the pairing expiry bug | Then: also add a test | Result: …"),
        "{text}"
    );
    assert!(text.contains("frozen-clock test covers it. | Files: src/auth.rs, src/tests.rs"));
    assert!(episode_text(&[]).is_empty());
    assert_eq!(age(10 * 60_000, 0), "10m ago");
    assert_eq!(age(26 * 60 * 60_000, 0), "1d ago");
}

#[test]
fn project_keys_ignore_separators_and_trailing_slash() {
    assert_eq!(project_key("/home/u/proj/"), "/home/u/proj");
    assert_eq!(
        project_key("C:\\ws\\"),
        if cfg!(windows) { "c:/ws" } else { "C:/ws" }
    );
}
