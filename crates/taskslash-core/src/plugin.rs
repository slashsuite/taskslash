//! Command plugins: `taskslash <name> [args...]` runs the first `taskslash-<name>` executable on
//! `PATH` in place of taskslash, as git and cargo do. Built-ins always win; the caller checks them.
//!
//! The plugin's environment carries the contract: `TASKSLASH_PROTOCOL` (the protocol version),
//! `TASKSLASH_SOCKET` (the daemon socket, when it resolves) and `TASKSLASH_BIN` (this binary).

use crate::Error;
use crate::protocol::PROTOCOL_VERSION;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a plugin's program is named: `taskslash-<name>`.
pub const PREFIX: &str = "taskslash-";

/// A plugin name found on `PATH`.
#[derive(Debug, PartialEq, Eq)]
pub struct Plugin {
    pub name: String,
    /// The first one on `PATH`: the one that runs.
    pub path: PathBuf,
    /// Later ones with the same name, in `PATH` order. They never run.
    pub shadowed: Vec<PathBuf>,
    /// A built-in has this name, so the plugin never runs.
    pub builtin: bool,
}

/// A regular file (after following links) with an execute bit.
fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The directories of a `PATH` value, in order, without empty entries or repeats. A repeat is the
/// same directory by its resolved path, so `/bin` after `/usr/bin` on a merged-/usr system is one.
fn dirs(path: &OsStr) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    let mut dirs = Vec::new();
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let real = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen.contains(&real) {
            seen.push(real);
            dirs.push(dir);
        }
    }
    dirs
}

/// The program `taskslash <name>` runs: the first `taskslash-<name>` on `path` that is an
/// executable regular file. None for a name that is empty or holds a `/`.
pub fn find(name: &OsStr, path: &OsStr) -> Option<PathBuf> {
    if name.is_empty() || name.as_bytes().contains(&b'/') {
        return None;
    }
    let mut file = OsString::from(PREFIX);
    file.push(name);
    dirs(path)
        .into_iter()
        .map(|dir| dir.join(&file))
        .find(|candidate| executable(candidate))
}

/// Every plugin on `path`, by name; `builtins` are the names a plugin can never take. A file name
/// that is not UTF-8 is left out (`find` still runs it).
pub fn list(path: &OsStr, builtins: &[String]) -> Vec<Plugin> {
    let mut found: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for dir in dirs(path) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let file = entry.file_name();
            let Some(name) = file.to_str().and_then(|f| f.strip_prefix(PREFIX)) else {
                continue;
            };
            let path = entry.path();
            if !name.is_empty() && executable(&path) {
                found.entry(name.to_owned()).or_default().push(path);
            }
        }
    }
    found
        .into_iter()
        .map(|(name, mut paths)| {
            let path = paths.remove(0);
            Plugin {
                builtin: builtins.contains(&name),
                name,
                path,
                shadowed: paths,
            }
        })
        .collect()
}

/// The candidate most like `name`, when it is like enough: clap's own rule for the "a similar
/// subcommand exists" tip (Jaro similarity above 0.7), which an external subcommand bypasses.
pub fn closest<'a>(name: &str, candidates: &'a [String]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (strsim::jaro(name, c), c.as_str()))
        .filter(|(score, _)| *score > 0.7)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c)
}

/// One line per plugin: `<name>  <path>`, then what shadows or blocks it; with none, a line saying
/// so and what a plugin is.
pub fn list_text(plugins: &[Plugin]) -> String {
    if plugins.is_empty() {
        return format!("no plugins on PATH (a plugin is an executable named {PREFIX}<name>)\n");
    }
    let width = plugins.iter().map(|p| p.name.len()).max().unwrap_or(0);
    let mut text = String::new();
    for plugin in plugins {
        text.push_str(&format!(
            "{:width$}  {}",
            plugin.name,
            plugin.path.display()
        ));
        if !plugin.shadowed.is_empty() {
            let shadowed: Vec<String> = plugin
                .shadowed
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            text.push_str(&format!("  (shadows {})", shadowed.join(", ")));
        }
        if plugin.builtin {
            text.push_str("  (a built-in has this name: never run)");
        }
        text.push('\n');
    }
    text
}

/// The plugins as one JSON array, on one line. A path that is not UTF-8 is written lossily.
pub fn list_json(plugins: &[Plugin]) -> String {
    let lossy = |p: &PathBuf| p.to_string_lossy().into_owned();
    let array: Vec<serde_json::Value> = plugins
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "path": lossy(&p.path),
                "shadowed": p.shadowed.iter().map(lossy).collect::<Vec<_>>(),
                "builtin": p.builtin,
            })
        })
        .collect();
    format!("{}\n", serde_json::Value::Array(array))
}

/// Runs `program` with `args` in place of this process, with the plugin contract in its
/// environment. Returns only when it could not be started.
pub fn exec(program: &Path, args: &[OsString]) -> Error {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("TASKSLASH_PROTOCOL", PROTOCOL_VERSION.to_string());
    match crate::socket_path() {
        Ok(socket) => command.env("TASKSLASH_SOCKET", socket),
        Err(_) => command.env_remove("TASKSLASH_SOCKET"),
    };
    match std::env::current_exe() {
        Ok(bin) => command.env("TASKSLASH_BIN", bin),
        Err(e) => return Error::new(format!("cannot find the taskslash binary: {e}")),
    };
    let e = command.exec();
    Error::new(format!("cannot run {}: {e}", program.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("tkp-{}-{tag}", std::process::id()));
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

    fn write(path: &Path, mode: u32) {
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn join(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn find_takes_the_first_executable_and_skips_the_rest() {
        let tmp = Dir::new("find");
        let (a, b, c) = (tmp.sub("a"), tmp.sub("b"), tmp.sub("c"));
        write(&a.join("taskslash-x"), 0o644);
        fs::create_dir(b.join("taskslash-x")).unwrap();
        write(&c.join("taskslash-x"), 0o755);
        let path = join(&[&a, &b, &c]);
        assert_eq!(find(OsStr::new("x"), &path), Some(c.join("taskslash-x")));
        assert_eq!(find(OsStr::new("y"), &path), None);
        assert_eq!(find(OsStr::new(""), &path), None);
        assert_eq!(find(OsStr::new("../c/taskslash-x"), &path), None);
    }

    #[test]
    fn find_follows_a_link_to_an_executable() {
        let tmp = Dir::new("link");
        let (a, b) = (tmp.sub("a"), tmp.sub("b"));
        write(&a.join("real"), 0o755);
        symlink(a.join("real"), b.join("taskslash-x")).unwrap();
        symlink(a.join("missing"), a.join("taskslash-y")).unwrap();
        let path = join(&[&a, &b]);
        assert_eq!(find(OsStr::new("x"), &path), Some(b.join("taskslash-x")));
        assert_eq!(find(OsStr::new("y"), &path), None);
    }

    #[test]
    fn a_directory_reached_twice_is_read_once() {
        let tmp = Dir::new("dirs");
        let real = tmp.sub("real");
        symlink(&real, tmp.0.join("alias")).unwrap();
        let path = join(&[&real, Path::new(""), &tmp.0.join("alias"), &real]);
        assert_eq!(dirs(&path), vec![real]);
    }

    #[test]
    fn closest_names_a_near_builtin_and_nothing_far() {
        let builtins: Vec<String> = ["status", "run", "logs"].map(String::from).into();
        assert_eq!(closest("stauts", &builtins), Some("status"));
        assert_eq!(closest("stat", &builtins), Some("status"));
        assert_eq!(closest("bench", &builtins), None);
    }
}
