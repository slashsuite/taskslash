//! The store against an in-memory database.

use super::*;
use std::time::Duration;

fn words(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|&a| a.to_owned()).collect()
}

#[test]
fn the_key_is_the_name_else_two_words() {
    let argv = words(&["bunx", "moon", "run", ":test"]);
    assert_eq!(key(Some("gate"), &argv), "gate");
    assert_eq!(key(None, &argv), "bunx moon");
    assert_eq!(key(None, &words(&["true"])), "true");
}

#[test]
fn a_path_word_is_cut_to_its_last_part() {
    assert_eq!(key(None, &words(&["/usr/bin/cargo", "test"])), "cargo test");
    assert_eq!(
        key(None, &words(&["bun", "/home/me/.cache/tmp/x-1/run.ts"])),
        "bun run.ts"
    );
    assert_eq!(key(None, &words(&["./scripts/gate.sh"])), "gate.sh");
    assert_eq!(key(None, &words(&["../bin/", "x"])), "bin x");
    assert_eq!(
        key(None, &words(&["/", "x"])),
        "/ x",
        "nothing after the slash"
    );
    // A word with a slash inside is not a path: a moon key keeps its repository.
    assert_eq!(key(Some("shop/cli:test"), &[]), "shop/cli:test");
    assert_eq!(key(None, &words(&["bun", "test", "src/a.ts"])), "bun test");
    assert_eq!(
        key(
            Some("ob:cpu:browser:bun /home/me/.cache/tmp/x/spec.ts"),
            &[]
        ),
        "ob:cpu:browser:bun spec.ts",
        "a name's path words too"
    );
}

#[test]
fn a_shell_script_is_keyed_by_its_own_first_two_words() {
    for (shell, flag) in [("sh", "-c"), ("/bin/bash", "-euc")] {
        let argv = words(&[shell, flag, "cargo   test --locked", "x"]);
        assert_eq!(key(None, &argv), "cargo test", "{shell} {flag}");
    }
    assert_eq!(
        key(None, &words(&["sh", "-c", "/tmp/x/build.sh --all"])),
        "build.sh --all"
    );
    // Anything else keeps the argv's words.
    assert_eq!(
        key(None, &words(&["sh", "-c", "  "])),
        "sh -c",
        "an empty script"
    );
    assert_eq!(key(None, &words(&["sh", "-c"])), "sh -c", "no script");
    assert_eq!(key(None, &words(&["sh", "script.sh"])), "sh script.sh");
    assert_eq!(
        key(None, &words(&["sh", "--command", "x y"])),
        "sh --command"
    );
    assert_eq!(key(None, &words(&["sh", "-x", "x y"])), "sh -x");
    assert_eq!(key(None, &words(&["python", "-c", "x y"])), "python -c");
}

#[test]
fn a_long_key_keeps_its_start_and_end_around_a_hash() {
    let long = "ob:cpu:browser:bunx moon run storybook:visual -- --all --group navigation";
    let shaped = key(Some(long), &[]);
    assert_eq!(
        shaped,
        "ob:cpu:browser:bunx moon...53a45...--all --group navigation"
    );
    assert!(shaped.chars().count() <= KEY_CHARS, "{shaped}");
    assert_ne!(
        shaped,
        key(
            Some("ob:cpu:browser:bunx moon run storybook:visual -- --all --group overview"),
            &[]
        ),
        "the ends tell them apart"
    );
    assert_ne!(
        shaped,
        key(
            Some("ob:cpu:browser:bunx moon run uislash:visual --- --all --group navigation"),
            &[]
        ),
        "so does the hash, when only the middle differs"
    );
    let exact = "x".repeat(KEY_CHARS);
    assert_eq!(
        key(Some(&exact), &[]),
        exact,
        "at the cap it is left as it is"
    );
    let wide = "é".repeat(KEY_CHARS + 1);
    assert_eq!(
        key(Some(&wide), &[]).chars().count(),
        KEY_CHARS,
        "characters, not bytes"
    );
}

#[test]
fn shaping_a_key_again_leaves_it_as_it_is() {
    for name in [
        "gate",
        "shop/cli:test",
        "ob:cpu:browser:bunx moon run storybook:visual -- --all --group navigation",
        "ob:cpu:browser:bun /home/me/.cache/tmp/claude-1000/-home-me-dev-ui-645/2/visual.spec.ts --reporter line",
        "a  b",
        "",
    ] {
        let once = key(Some(name), &[]);
        assert_eq!(key(Some(&once), &[]), once, "{name}");
    }
}

fn finished<'a>(argv: &'a [String], launch: Launch, at: u64, cost: Cost) -> Finished<'a> {
    Finished {
        key: "k",
        kind: Kind::Test,
        argv,
        cwd: "/work",
        class: Class::Flake,
        submitter: "/work",
        launch,
        started_at: UNIX_EPOCH + Duration::from_millis(at),
        wall_ms: 12,
        cost,
        exit_code: Some(4),
        pressure: RunPressure::default(),
        cached: None,
        moon_hash: None,
        log_path: None,
    }
}

#[test]
fn a_plain_run_is_recorded_with_null_costs() {
    let store = Store::in_memory();
    let argv = words(&["sh", "-c", "exit 4"]);
    store
        .record(&finished(
            &argv,
            Launch::Plain,
            1_763_290_000_123,
            Cost::default(),
        ))
        .unwrap();
    let runs = store.history("k", HISTORY_LIMIT, None).unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.argv, argv);
    assert_eq!(run.launch, Launch::Plain);
    assert_eq!((run.class, run.submitter.as_str()), (Class::Flake, "/work"));
    assert_eq!(run.started_at, "2025-11-16T10:46:40.123Z");
    assert_eq!((run.wall_ms, run.exit_code), (12, Some(4)));
    assert_eq!(run.cpu_usec, None);
    assert_eq!(run.peak_cpu_5s, None);
    assert_eq!(run.peak_mem_bytes, None);
    assert_eq!(run.written_bytes, None);
}

#[test]
fn history_is_newest_first_and_at_most_20() {
    let store = Store::in_memory();
    let argv = words(&["true"]);
    let cost = Cost {
        cpu_usec: Some(5),
        peak_cpu_5s: Some(0.5),
        peak_mem_bytes: Some(6),
        written_bytes: Some(0),
    };
    for at in 0..25 {
        store
            .record(&finished(&argv, Launch::Scope, at * 1000, cost.clone()))
            .unwrap();
    }
    let runs = store.history("k", HISTORY_LIMIT, None).unwrap();
    assert_eq!(runs.len(), HISTORY_LIMIT);
    assert_eq!(runs[0].started_at, "1970-01-01T00:00:24.000Z");
    assert_eq!(runs[0].cpu_usec, Some(5));
    assert_eq!(runs[0].written_bytes, Some(0));
    assert!(
        store
            .history("other", HISTORY_LIMIT, None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_key_is_seen_once_a_run_finished_and_costs_are_its_measured_runs() {
    let store = Store::in_memory();
    let argv = words(&["true"]);
    assert!(!store.seen("k").unwrap());
    let mut dropped = finished(&argv, Launch::Scope, 1000, Cost::default());
    dropped.exit_code = None;
    store.record(&dropped).unwrap();
    assert!(
        !store.seen("k").unwrap(),
        "a dropped run does not make a key seen"
    );
    store
        .record(&finished(&argv, Launch::Plain, 2000, Cost::default()))
        .unwrap();
    assert!(store.seen("k").unwrap(), "an unmeasured finished run does");
    assert!(store.costs("k").unwrap().is_empty());

    let mut measured = finished(
        &argv,
        Launch::Scope,
        3000,
        Cost {
            cpu_usec: Some(7),
            peak_cpu_5s: Some(0.5),
            peak_mem_bytes: Some(8),
            written_bytes: None,
        },
    );
    measured.pressure = RunPressure {
        cpu: Some(1.5),
        mem: Some(0.25),
        io: None,
    };
    store.record(&measured).unwrap();
    let costs = store.costs("k").unwrap();
    assert_eq!(costs.len(), 1);
    assert_eq!(
        (costs[0].cpu_usec, costs[0].machine_cpu_some),
        (Some(7), Some(1.5))
    );
    let run = &store.history("k", HISTORY_LIMIT, None).unwrap()[0];
    assert_eq!(run.kind, Kind::Test);
    assert_eq!(
        (
            run.machine_cpu_some,
            run.machine_mem_some,
            run.machine_io_some
        ),
        (Some(1.5), Some(0.25), None)
    );
}

#[test]
fn a_cache_hit_and_its_hash_are_recorded_and_read_back() {
    let store = Store::in_memory();
    let argv = words(&["moon", "run", "a:test"]);
    let cost = Cost {
        cpu_usec: Some(5),
        peak_cpu_5s: Some(0.5),
        peak_mem_bytes: Some(6),
        written_bytes: Some(0),
    };
    let mut hit = finished(&argv, Launch::Scope, 2000, cost.clone());
    hit.cached = Some(true);
    hit.moon_hash = Some("74ea".into());
    store.record(&hit).unwrap();
    assert!(
        !store.seen("k").unwrap(),
        "a cache hit does not make a key seen"
    );
    store
        .record(&finished(&argv, Launch::Scope, 1000, cost))
        .unwrap();
    assert!(store.seen("k").unwrap());
    let runs = store.history("k", HISTORY_LIMIT, None).unwrap();
    assert_eq!(
        (runs[0].cached, runs[0].moon_hash.as_deref()),
        (Some(true), Some("74ea"))
    );
    assert_eq!((runs[1].cached, runs[1].moon_hash.as_deref()), (None, None));
    let costs = store.costs("k").unwrap();
    assert_eq!(
        costs.iter().map(|c| c.cached).collect::<Vec<_>>(),
        [true, false]
    );
}

#[test]
fn a_run_with_no_exit_code_is_not_a_failure_and_log_ends_add_the_wall_time() {
    let store = Store::in_memory();
    let argv = words(&["sh", "-c", "x"]);
    let record = |at: u64, code: Option<i32>| {
        let mut run = finished(&argv, Launch::Plain, at, Cost::default());
        run.exit_code = code;
        run.log_path = Some("/l");
        store.record(&run).unwrap()
    };
    let failed = record(1000, Some(1));
    // Newer, but its client went away: no exit code, so not a failure.
    record(2000, None);
    assert_eq!(
        store.last_failed(Some("k"), None).unwrap().map(|r| r.id),
        Some(failed)
    );
    // Started at 1000 ms and ran 12 ms.
    assert!(store.log_ends().unwrap().contains(&("/l".to_owned(), 1012)));
}

#[test]
fn last_failed_within_a_submitter_keeps_only_its_runs() {
    let store = Store::in_memory();
    let argv = words(&["sh", "-c", "x"]);
    let mine = store.record(&at("k", "/w/mine", &argv, 1000, 2)).unwrap();
    store.record(&at("k", "/w/mine", &argv, 1500, 0)).unwrap();
    let theirs = store.record(&at("j", "/w/theirs", &argv, 2000, 1)).unwrap();
    store
        .record(&at("k", "/w/notmine", &argv, 3000, 1))
        .unwrap();
    let id = |key, within| store.last_failed(key, within).unwrap().map(|r| r.id);
    assert_eq!(id(None, Some("/w/mine")), Some(mine));
    assert_eq!(id(None, Some("mine")), Some(mine), "its directory name");
    assert_eq!(id(Some("j"), Some("/w/theirs")), Some(theirs));
    assert_eq!(id(Some("j"), Some("/w/mine")), None);
    assert_eq!(id(None, Some("/w/nobody")), None);
    assert_ne!(id(None, None), Some(mine), "the whole machine's newest");
}

fn at<'a>(
    key: &'a str,
    submitter: &'a str,
    argv: &'a [String],
    ms: u64,
    exit: i32,
) -> Finished<'a> {
    Finished {
        key,
        submitter,
        exit_code: Some(exit),
        ..finished(argv, Launch::Plain, ms, Cost::default())
    }
}

#[test]
fn keys_are_newest_first_with_counts_and_the_newest_runs_exit_and_submitter() {
    let store = Store::in_memory();
    let argv = words(&["true"]);
    store.record(&at("old", "/w/a", &argv, 1_000, 0)).unwrap();
    store.record(&at("new", "/w/a", &argv, 2_000, 0)).unwrap();
    store.record(&at("new", "/w/b", &argv, 5_000, 3)).unwrap();
    store.record(&at("new", "/w/a", &argv, 4_000, 0)).unwrap();
    store.record(&at("mid", "/w/x", &argv, 3_000, 9)).unwrap();
    // Same start: the later row is the newest, as `history` orders them.
    store.record(&at("mid", "/w/c", &argv, 3_000, 1)).unwrap();
    let keys = store.keys().unwrap();
    let got: Vec<(&str, u64, Option<i32>, &str)> = keys
        .iter()
        .map(|k| {
            (
                k.key.as_str(),
                k.runs,
                k.last_exit_code,
                k.last_submitter.as_str(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("new", 3, Some(3), "/w/b"),
            ("mid", 2, Some(1), "/w/c"),
            ("old", 1, Some(0), "/w/a"),
        ]
    );
    assert_eq!(keys[0].last_started_at, "1970-01-01T00:00:05.000Z");
    assert!(Store::in_memory().keys().unwrap().is_empty());
}

#[test]
fn history_takes_a_limit_and_a_directory() {
    let store = Store::in_memory();
    let argv = words(&["true"]);
    for ms in 0..5 {
        store
            .record(&at("k", "/dev/shop", &argv, ms * 1000, 0))
            .unwrap();
    }
    store
        .record(&at("k", "/dev/myshop", &argv, 9_000, 0))
        .unwrap();
    store
        .record(&at("k", "/elsewhere/shop", &argv, 8_000, 0))
        .unwrap();
    assert_eq!(store.history("k", 3, None).unwrap().len(), 3);
    let shop = store.history("k", 20, Some("shop")).unwrap();
    let dirs: Vec<&str> = shop.iter().map(|r| r.submitter.as_str()).collect();
    assert_eq!(dirs.len(), 6, "the name, not a suffix of it: {dirs:?}");
    assert_eq!(dirs[0], "/elsewhere/shop");
    let full = store.history("k", 20, Some("/dev/shop")).unwrap();
    assert_eq!(full.len(), 5, "a full path matches the full path");
    assert_eq!(store.history("k", 2, Some("shop")).unwrap().len(), 2);
    assert!(store.history("k", 20, Some("hop")).unwrap().is_empty());
}

#[test]
fn a_database_on_disk_is_written_ahead_and_not_synced_on_every_row() {
    let dir = std::env::temp_dir().join(format!("tks-wal-{}", std::process::id()));
    // A failed earlier run with the same pid may have left its row.
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir.join("runs.db")).unwrap();
    let pragma = |name: &str| -> String {
        store
            .conn
            .query_row(&format!("PRAGMA {name}"), [], |row| {
                row.get::<_, rusqlite::types::Value>(0)
            })
            .map(|v| format!("{v:?}"))
            .unwrap()
    };
    assert_eq!(pragma("journal_mode"), "Text(\"wal\")");
    // 1 is NORMAL: a commit in WAL mode then waits for no fsync.
    assert_eq!(pragma("synchronous"), "Integer(1)");
    let argv = words(&["true"]);
    store
        .record(&finished(&argv, Launch::Plain, 1, Cost::default()))
        .unwrap();
    drop(store);
    // Opened again, the row is there.
    let store = Store::open(&dir.join("runs.db")).unwrap();
    assert_eq!(store.history("k", 5, None).unwrap().len(), 1);
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}
