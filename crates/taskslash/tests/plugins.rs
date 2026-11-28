//! Command plugins: `taskslash <name>` runs `taskslash-<name>` from `PATH`, and `taskslash plugins`
//! lists them. Every test sets `PATH` (and the rest of the environment) itself, so the machine's own
//! `PATH` cannot make one pass.

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_taskslash");

/// A directory of its own under `TMPDIR`; removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tkpl-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn sub(&self, name: &str) -> PathBuf {
        let dir = self.0.join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes a `/bin/sh` script with `body` at `path`, with `mode`.
fn script(path: &Path, body: &str, mode: u32) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// `taskslash <args>` with an empty environment but `PATH` (and any `env`).
fn taskslash(path: &[&Path], args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env_clear()
        .env("PATH", std::env::join_paths(path).unwrap())
        .envs(env.iter().copied())
        .stdin(Stdio::null());
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_plugin_runs_in_place_of_taskslash_with_its_arguments_and_exit_code() {
    let tmp = Dir::new("run");
    let bin = tmp.sub("bin");
    script(
        &bin.join("taskslash-echo"),
        r#"printf '%s\n' "$$" "$@"; exit 7"#,
        0o755,
    );
    let child = taskslash(&[&bin], &["echo", "a", "b c"], &[])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    // The plugin's pid is the one taskslash started as: it replaced taskslash, not ran under it.
    assert_eq!(text(&out.stdout), format!("{pid}\na\nb c\n"));
}

#[test]
fn a_plugin_gets_the_protocol_the_socket_and_the_binary() {
    let tmp = Dir::new("env");
    let bin = tmp.sub("bin");
    script(
        &bin.join("taskslash-env"),
        r#"printf '%s\n' "$TASKSLASH_PROTOCOL" "${TASKSLASH_SOCKET-unset}" "$TASKSLASH_BIN""#,
        0o755,
    );
    let own = fs::canonicalize(BIN).unwrap();

    let out = taskslash(&[&bin], &["env"], &[("XDG_RUNTIME_DIR", "/run/user/test")])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        format!(
            "1\n/run/user/test/taskslash/daemon.sock\n{}\n",
            own.display()
        )
    );

    let out = taskslash(&[&bin], &["env"], &[("TASKSLASH_SOCKET", "/some/d.sock")])
        .output()
        .unwrap();
    assert_eq!(
        text(&out.stdout),
        format!("1\n/some/d.sock\n{}\n", own.display())
    );

    // Neither is set, so the socket does not resolve and the plugin is not given one.
    let out = taskslash(&[&bin], &["env"], &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), format!("1\nunset\n{}\n", own.display()));
}

#[test]
fn a_builtin_is_never_sent_to_a_plugin_of_the_same_name() {
    let tmp = Dir::new("builtin");
    let bin = tmp.sub("bin");
    let ran = tmp.0.join("ran");
    for name in ["status", "plugins"] {
        script(
            &bin.join(format!("taskslash-{name}")),
            &format!("echo {name} >> {}", ran.display()),
            0o755,
        );
    }
    let out = taskslash(
        &[&bin],
        &["status"],
        &[("TASKSLASH_SOCKET", "/nonexistent/d.sock")],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let out = taskslash(&[&bin], &["plugins"], &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        !ran.exists(),
        "a plugin ran: {}",
        fs::read_to_string(&ran).unwrap()
    );
}

#[test]
fn an_unknown_name_is_a_usage_error_naming_the_program() {
    let tmp = Dir::new("unknown");
    let bin = tmp.sub("bin");
    // Named taskslash-xyzzy but not executable, so it does not count.
    script(&bin.join("taskslash-xyzzy"), "exit 0", 0o644);
    let out = taskslash(&[&bin], &["xyzzy", "a"], &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = text(&out.stderr);
    assert!(err.contains("unrecognized subcommand 'xyzzy'"), "{err}");
    assert!(err.contains("taskslash-xyzzy"), "{err}");
    assert!(!err.contains("tip:"), "{err}");

    // A typo of a built-in still gets the tip.
    let out = taskslash(&[&bin], &["stauts"], &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = text(&out.stderr);
    assert!(
        err.contains("a similar subcommand exists: 'status'"),
        "{err}"
    );
    assert!(err.contains("taskslash-stauts"), "{err}");
    let out = taskslash(&[&bin], &["hist"], &[]).output().unwrap();
    let err = text(&out.stderr);
    assert!(
        err.contains("a similar subcommand exists: 'history'"),
        "{err}"
    );
}

#[test]
fn plugins_lists_shadows_and_marks_builtins() {
    let tmp = Dir::new("list");
    let (a, b) = (tmp.sub("a"), tmp.sub("b"));
    script(&a.join("taskslash-bench"), "exit 0", 0o755);
    script(&b.join("taskslash-bench"), "exit 0", 0o755);
    script(&b.join("taskslash-run"), "exit 0", 0o755);
    script(&b.join("taskslash-off"), "exit 0", 0o644);
    fs::create_dir(b.join("taskslash-dir")).unwrap();
    script(&b.join("unrelated"), "exit 0", 0o755);

    script(&b.join("taskslash-"), "exit 0", 0o755);
    // A repeated directory and one that is not there change nothing.
    let missing = tmp.0.join("missing");
    let path: &[&Path] = &[&a, &b, &a, &missing];

    let out = taskslash(path, &["plugins"], &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        format!(
            "bench  {}  (shadows {})\nrun    {}  (a built-in has this name: never run)\n",
            a.join("taskslash-bench").display(),
            b.join("taskslash-bench").display(),
            b.join("taskslash-run").display(),
        )
    );

    let out = taskslash(path, &["plugins", "--json"], &[])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    let path = |p: PathBuf| Value::from(p.to_str().unwrap());
    assert_eq!(
        json,
        serde_json::json!([
            {
                "name": "bench",
                "path": path(a.join("taskslash-bench")),
                "shadowed": [path(b.join("taskslash-bench"))],
                "builtin": false,
            },
            {
                "name": "run",
                "path": path(b.join("taskslash-run")),
                "shadowed": [],
                "builtin": true,
            },
        ])
    );
}

#[test]
fn plugins_with_none_says_so_and_what_a_plugin_is() {
    let tmp = Dir::new("none");
    let bin = tmp.sub("bin");
    script(&bin.join("unrelated"), "exit 0", 0o755);
    let out = taskslash(&[&bin], &["plugins"], &[]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        "no plugins on PATH (a plugin is an executable named taskslash-<name>)\n"
    );
    let out = taskslash(&[&bin], &["plugins", "--json"], &[])
        .output()
        .unwrap();
    assert_eq!(text(&out.stdout), "[]\n");
}

/// The example plugin in the README, between its marker comments, without the code fence.
fn readme_example() -> String {
    let readme = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
        .expect("the README is there");
    let start = "<!-- plugin-example:start -->";
    let end = "<!-- plugin-example:end -->";
    let from = readme.find(start).expect("the start marker") + start.len();
    let to = readme.find(end).expect("the end marker");
    let block = readme[from..to].trim();
    let body = block
        .strip_prefix("```sh\n")
        .and_then(|b| b.strip_suffix("```"))
        .expect("a ```sh block between the markers");
    body.to_owned()
}

#[test]
fn the_readme_example_plugin_runs() {
    let tmp = Dir::new("readme");
    let bin = tmp.sub("bin");
    let example = readme_example();
    assert!(example.starts_with("#!/bin/sh\n"), "{example}");
    let path = bin.join("taskslash-hello");
    fs::write(&path, &example).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

    let out: Output = taskslash(
        &[&bin],
        &["hello", "world"],
        &[("TASKSLASH_SOCKET", "/nonexistent/d.sock")],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        text(&out.stdout),
        "hello, world\nprotocol 1\nno daemon\n",
        "{}",
        text(&out.stderr)
    );
}
