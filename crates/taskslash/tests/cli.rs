use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_taskslash"))
        .args(args)
        .output()
        .expect("the binary runs")
}

#[test]
fn prints_its_version_and_the_commit_it_was_built_from() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let rest = text
        .strip_prefix(&format!("taskslash {} (", env!("CARGO_PKG_VERSION")))
        .and_then(|r| r.strip_suffix(")\n"))
        .unwrap_or_else(|| panic!("not a version and a commit: {text}"));
    assert!(
        rest.len() >= 7 && rest.chars().all(|c| c.is_ascii_hexdigit()),
        "{text}"
    );
}

#[test]
fn bare_taskslash_prints_the_help_and_exits_0() {
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(out.stdout, run(&["--help"]).stdout);
}

#[test]
fn the_help_lists_commands_by_use_then_examples_and_every_setting() {
    let help = String::from_utf8(run(&["--help"]).stdout).unwrap();
    let commands: Vec<&str> = help
        .split("Commands:\n")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert_eq!(
        commands,
        [
            "top", "status", "history", "logs", "why", "run", "moon", "cancel", "events",
            "plugins", "daemon", "help"
        ]
    );
    let examples = help.split("Examples:\n").nth(1).expect("examples");
    assert_eq!(
        examples.lines().take_while(|l| !l.is_empty()).count(),
        3,
        "{help}"
    );
    let environment = format!(
        "\n{}",
        help.split("Environment:\n").nth(1).expect("environment")
    );
    // Every TASKSLASH_ setting the code reads, but those for tests only and for plugins.
    for name in [
        "SOCKET",
        "DB",
        "LOGS",
        "CLASS",
        "PHASES",
        "LOG_DAYS",
        "LOG_MIB",
        "CPU_SHARE",
        "MEM_SHARE",
        "WRITE_MIB_S",
        "LAUNCH",
    ] {
        assert!(
            environment.contains(&format!("\n  TASKSLASH_{name} ")),
            "TASKSLASH_{name} missing: {help}"
        );
    }
}

#[test]
fn the_daemon_help_says_where_it_normally_runs() {
    let help = String::from_utf8(run(&["daemon", "--help"]).stdout).unwrap();
    assert!(
        help.contains("Normally runs as the systemd user unit: systemctl --user status taskslash"),
        "{help}"
    );
}

#[test]
fn a_usage_error_exits_2() {
    let out = run(&["nope"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn an_unknown_class_is_a_usage_error_from_the_flag_or_the_environment() {
    let out = run(&["run", "--class", "high", "--", "true"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = Command::new(env!("CARGO_BIN_EXE_taskslash"))
        .args(["run", "--", "true"])
        .env("TASKSLASH_CLASS", "high")
        .env("TASKSLASH_SOCKET", "/nonexistent/taskslash.sock")
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("TASKSLASH_CLASS"), "{err}");
}
