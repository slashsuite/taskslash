//! The wire format.

use super::*;

#[test]
fn messages_round_trip_as_tagged_json_lines() {
    let submit = ClientMessage::Submit {
        name: Some("probe".into()),
        kind: Kind::Browser,
        argv: vec!["sh".into(), "-c".into(), "exit 3".into()],
        cwd: "/work".into(),
        cpu: None,
        mem_mib: None,
        class: Class::Flake,
        submitter: "/work".into(),
    };
    let start = DaemonMessage::Start {
        job: 3,
        unit: "taskslash-1-3".into(),
        launch: Launch::Scope,
        log: "/logs/taskslash-1-3.log".into(),
        running: true,
    };
    let history = ClientMessage::History {
        key: "probe".into(),
        limit: None,
        within: None,
    };
    let mut wire = Vec::new();
    send(&mut wire, &submit).unwrap();
    send(&mut wire, &start).unwrap();
    send(&mut wire, &history).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&wire),
        "{\"type\":\"submit\",\"name\":\"probe\",\"kind\":\"browser\",\"argv\":[\"sh\",\"-c\",\"exit 3\"],\"cwd\":\"/work\",\"class\":\"flake\",\"submitter\":\"/work\"}\n\
         {\"type\":\"start\",\"job\":3,\"unit\":\"taskslash-1-3\",\"launch\":\"scope\",\"log\":\"/logs/taskslash-1-3.log\",\"running\":true}\n\
         {\"type\":\"history\",\"key\":\"probe\"}\n"
    );
    let mut reader = wire.as_slice();
    assert_eq!(receive(&mut reader).unwrap(), Some(submit));
    assert_eq!(receive(&mut reader).unwrap(), Some(start));
    assert_eq!(receive(&mut reader).unwrap(), Some(history));
    assert_eq!(receive::<DaemonMessage>(&mut reader).unwrap(), None);
}

#[test]
fn subscribe_and_events_are_tagged_json_lines() {
    let mut wire = Vec::new();
    send(&mut wire, &ClientMessage::Subscribe).unwrap();
    send(&mut wire, &DaemonMessage::Subscribed { protocol: 1 }).unwrap();
    let started = DaemonMessage::Event {
        protocol: 1,
        seq: 2,
        event: JobEvent::Started {
            job: 3,
            unit: "taskslash-1-3".into(),
            launch: Launch::Plain,
            log: "/logs/taskslash-1-3.log".into(),
            at: "2026-11-17T00:00:00.000Z".into(),
        },
    };
    send(&mut wire, &started).unwrap();
    let dropped = DaemonMessage::Event {
        protocol: 1,
        seq: 9,
        event: JobEvent::Dropped { count: 4 },
    };
    send(&mut wire, &dropped).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&wire),
        "{\"type\":\"subscribe\"}\n\
         {\"type\":\"subscribed\",\"protocol\":1}\n\
         {\"type\":\"event\",\"protocol\":1,\"seq\":2,\"event\":{\"type\":\"started\",\"job\":3,\"unit\":\"taskslash-1-3\",\"launch\":\"plain\",\"log\":\"/logs/taskslash-1-3.log\",\"at\":\"2026-11-17T00:00:00.000Z\"}}\n\
         {\"type\":\"event\",\"protocol\":1,\"seq\":9,\"event\":{\"type\":\"dropped\",\"count\":4}}\n"
    );
    let mut reader = wire.as_slice();
    assert_eq!(
        receive(&mut reader).unwrap(),
        Some(ClientMessage::Subscribe)
    );
    assert_eq!(
        receive(&mut reader).unwrap(),
        Some(DaemonMessage::Subscribed { protocol: 1 })
    );
    assert_eq!(receive(&mut reader).unwrap(), Some(started));
    assert_eq!(receive(&mut reader).unwrap(), Some(dropped));
}

#[test]
fn a_finished_event_spells_its_unknowns_as_null() {
    let finished = JobEvent::Finished {
        job: 1,
        key: "k".into(),
        exit_code: None,
        wall_ms: 0,
        cached: None,
        moon_hash: None,
        run: None,
        log: None,
        at: "2026-11-17T00:00:00.000Z".into(),
    };
    assert_eq!(
        serde_json::to_string(&finished).unwrap(),
        "{\"type\":\"finished\",\"job\":1,\"key\":\"k\",\"exit_code\":null,\"wall_ms\":0,\"cached\":null,\"moon_hash\":null,\"run\":null,\"log\":null,\"at\":\"2026-11-17T00:00:00.000Z\"}"
    );
}

#[test]
fn an_old_clients_history_parses_and_a_new_one_carries_limit_and_where() {
    let old: ClientMessage = serde_json::from_str("{\"type\":\"history\",\"key\":\"k\"}").unwrap();
    assert_eq!(
        old,
        ClientMessage::History {
            key: "k".into(),
            limit: None,
            within: None,
        }
    );
    let new = ClientMessage::History {
        key: "k".into(),
        limit: Some(3),
        within: Some("wt".into()),
    };
    assert_eq!(
        serde_json::to_string(&new).unwrap(),
        "{\"type\":\"history\",\"key\":\"k\",\"limit\":3,\"where\":\"wt\"}"
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::Keys).unwrap(),
        "{\"type\":\"keys\"}"
    );
}

#[test]
fn suggestions_are_substring_matches_first_then_the_closest_names() {
    let keys: Vec<String> = [
        "shop/web:test",
        "shop/api:test",
        "shop/web:build",
        "bun test",
        "gate",
        "gates",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        suggest("WEB", &keys),
        ["shop/web:test", "shop/web:build"],
        "case-insensitive substring, in the keys' order"
    );
    assert_eq!(
        suggest("GAET", &keys),
        ["gate", "gates"],
        "else closest by Jaro, any case"
    );
    let near: Vec<String> = ["gate", "late"].into_iter().map(String::from).collect();
    assert_eq!(
        suggest("gat", &near),
        ["gate"],
        "no Jaro once a key contains it"
    );
    assert_eq!(
        suggest("shop", &keys),
        ["shop/web:test", "shop/api:test", "shop/web:build"]
    );
    assert!(suggest("zzzzzz", &keys).is_empty());
    let many: Vec<String> = (0..9).map(|n| format!("job{n}")).collect();
    assert_eq!(suggest("job", &many).len(), 5, "at most five");
}

#[test]
fn where_keeps_a_full_path_or_a_directory_name() {
    assert!(submitted_from("/dev/shop", "shop"));
    assert!(submitted_from("/dev/shop", "/dev/shop"));
    assert!(!submitted_from("/dev/myshop", "shop"));
    assert!(!submitted_from("/dev/shop", "hop"));
    assert!(submitted_from("shop", "shop"));
}

#[test]
fn an_older_daemons_waiting_reads_with_no_job_number() {
    let line = r#"{"type":"waiting","reason":{"kind":"behind","job":3,"key":"k"},"waited_s":15,"position":2}"#;
    let message: DaemonMessage = serde_json::from_str(line).unwrap();
    assert_eq!(
        message,
        DaemonMessage::Waiting {
            job: None,
            reason: Reason::Behind {
                job: 3,
                key: "k".into()
            },
            waited_s: 15,
            position: 2,
            never_seen: false,
            assumed_threads: None,
        }
    );
}

#[test]
fn an_ended_job_reads_as_its_run_or_why_it_has_none() {
    let run = JobEnded::Run {
        run: 3140,
        key: "shop/web:test".into(),
        exit_code: Some(0),
    };
    assert_eq!(
        run.sentence(1),
        "job 1 ended: run 3140 of shop/web:test, exit 0 (taskslash logs 1 shows its log)"
    );
    assert_eq!(
        JobEnded::NeverRan.sentence(1),
        "job 1 ended without running (it left the queue, or its client went before it started)"
    );
    assert_eq!(
        JobEnded::Unrecorded.sentence(1),
        "job 1 ended, but its run was not recorded"
    );
    assert_eq!(
        JobEnded::Recording.sentence(1),
        "job 1 just ended; its run is being recorded, try again in a moment"
    );
    assert_eq!(
        ended_json(&run),
        "{\"kind\":\"ended\",\"ended\":{\"as\":\"run\",\"run\":3140,\"key\":\"shop/web:test\",\"exit_code\":0}}\n"
    );
}

#[test]
fn cancel_and_its_answers_round_trip_as_tagged_json_lines() {
    let ask = ClientMessage::Cancel {
        job: 12,
        grace_s: 10,
    };
    let tell = DaemonMessage::Cancel {
        job: 12,
        grace_s: 0,
    };
    let waiting = DaemonMessage::Cancelled {
        job: 12,
        was: Was::Waiting,
        already: false,
    };
    let again = DaemonMessage::Cancelled {
        job: 12,
        was: Was::Running,
        already: true,
    };
    let unknown = DaemonMessage::NotCancelled {
        job: 12,
        ended: None,
    };
    let ended = DaemonMessage::NotCancelled {
        job: 12,
        ended: Some(JobEnded::NeverRan),
    };
    let mut wire = Vec::new();
    send(&mut wire, &ask).unwrap();
    for message in [&tell, &waiting, &again, &unknown, &ended] {
        send(&mut wire, message).unwrap();
    }
    assert_eq!(
        String::from_utf8_lossy(&wire),
        "{\"type\":\"cancel\",\"job\":12,\"grace_s\":10}\n\
         {\"type\":\"cancel\",\"job\":12,\"grace_s\":0}\n\
         {\"type\":\"cancelled\",\"job\":12,\"was\":\"waiting\"}\n\
         {\"type\":\"cancelled\",\"job\":12,\"was\":\"running\",\"already\":true}\n\
         {\"type\":\"not_cancelled\",\"job\":12}\n\
         {\"type\":\"not_cancelled\",\"job\":12,\"ended\":{\"as\":\"never_ran\"}}\n"
    );
    let mut reader = wire.as_slice();
    assert_eq!(receive(&mut reader).unwrap(), Some(ask));
    for message in [tell, waiting, again, unknown, ended] {
        assert_eq!(
            receive::<DaemonMessage>(&mut reader).unwrap(),
            Some(message)
        );
    }
}

#[test]
fn running_round_trips() {
    let running = ClientMessage::Running {
        job: 7,
        pid: 4321,
        group: true,
    };
    let mut wire = Vec::new();
    send(&mut wire, &running).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&wire),
        "{\"type\":\"running\",\"job\":7,\"pid\":4321,\"group\":true}\n"
    );
    assert_eq!(receive(&mut wire.as_slice()).unwrap(), Some(running));
}

#[test]
fn an_old_shape_start_and_waiting_still_parse() {
    let start: DaemonMessage = serde_json::from_str(
        r#"{"type":"start","job":3,"unit":"taskslash-1-3","launch":"plain","log":"/l"}"#,
    )
    .unwrap();
    assert!(matches!(
        start,
        DaemonMessage::Start {
            job: 3,
            running: false,
            ..
        }
    ));
    let waiting: DaemonMessage = serde_json::from_str(
        r#"{"type":"waiting","job":4,"reason":{"kind":"behind","job":3,"key":"k"},"waited_s":0,"position":1}"#,
    )
    .unwrap();
    assert!(matches!(
        waiting,
        DaemonMessage::Waiting { job: Some(4), .. }
    ));
}
