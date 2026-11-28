//! A moon workspace as `taskslash moon` sees it: where it is, which moon runs it, the repository's
//! name, and the graph of targets to run, read from `moon query tasks` (its JSON: `tasks`, by
//! project then task, each with a `target` and `deps` that each have a `target`).

use crate::Error;
use crate::protocol::Kind;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// The file that marks a moon workspace's root.
const WORKSPACE_FILE: &str = ".moon/workspace.yml";

/// The first directory from `cwd` up, `cwd` itself first, that holds `.moon/workspace.yml`.
pub fn find_root(cwd: &Path) -> Option<&Path> {
    cwd.ancestors()
        .find(|dir| dir.join(WORKSPACE_FILE).is_file())
}

/// `<root>/node_modules/.bin/moon` when it exists, else `moon` on `PATH`.
pub fn moon_binary(root: &Path) -> Result<PathBuf, Error> {
    moon_binary_on(root, env::var_os("PATH"))
}

/// `moon_binary` with `path` as `PATH`.
fn moon_binary_on(root: &Path, path: Option<OsString>) -> Result<PathBuf, Error> {
    let local = root.join("node_modules/.bin/moon");
    if local.is_file() {
        return Ok(local);
    }
    path.and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join("moon"))
            .find(|moon| moon.is_file())
    })
    .ok_or_else(|| {
        Error::new(format!(
            "no moon: neither {} nor moon on PATH; run your package manager's install \
                 (bun install) first",
            local.display()
        ))
    })
}

/// The repository's name, the same for every worktree and clone of it: the last part of the
/// `origin` remote's URL, less `.git` (`git@github.com:me/shop.git` is `shop`), read from the
/// config in `root`'s git directory (a worktree's common one). With no `origin`,
/// the basename of `root` in a clone (`.git` a directory), or of the main checkout a worktree's
/// `.git` file points to (`gitdir: <main>/.git/worktrees/<name>`). Anything else is the basename
/// of `root`.
pub fn repo_name(root: &Path) -> String {
    if let Some(name) = git_dir(root)
        .and_then(|dir| fs::read_to_string(dir.join("config")).ok())
        .and_then(|config| origin_name(&config))
    {
        return name;
    }
    let main = main_checkout(root);
    main.file_name().map_or_else(
        || main.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The last part of the `url` of `[remote "origin"]` in a git config, less `.git`; `None` when
/// there is none or it ends in nothing.
fn origin_name(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line.replace(char::is_whitespace, "") == "[remote\"origin\"]";
        } else if in_origin
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == "url"
        {
            let url = value.trim().trim_matches('"').trim_end_matches('/');
            let last = url.rsplit(['/', ':']).next()?;
            let last = last.strip_suffix(".git").unwrap_or(last);
            return (!last.is_empty()).then(|| last.to_owned());
        }
    }
    None
}

/// The git directory whose `config` `root` uses: `root/.git` in a clone, else what the `gitdir:` of
/// its `.git` file names, through that directory's `commondir` when it has one (a worktree, of a
/// bare repository too).
fn git_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let gitdir = root.join(text.strip_prefix("gitdir:")?.trim());
    Some(match fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => gitdir.join(common.trim()),
        Err(_) => gitdir,
    })
}

/// The checkout whose `.git` directory `root` uses: `root` itself, or the main checkout of the
/// worktree `root` is.
fn main_checkout(root: &Path) -> PathBuf {
    fs::read_to_string(root.join(".git"))
        .ok()
        .and_then(|text| {
            let gitdir = root.join(text.strip_prefix("gitdir:")?.trim());
            let worktrees = gitdir.parent()?;
            let dot_git = worktrees.parent()?;
            (worktrees.file_name()? == "worktrees" && dot_git.file_name()? == ".git")
                .then(|| dot_git.parent().map(Path::to_path_buf))?
        })
        .unwrap_or_else(|| root.to_path_buf())
}

/// The key `taskslash moon` records `key` under when `key` looks like a moon target
/// (`<project>:<task>`, both named, no `/`) and `cwd` is inside a moon workspace:
/// `<repo>/<project>:<task>`. `None` otherwise.
pub fn target_key(cwd: &Path, key: &str) -> Option<String> {
    let (project, task) = key.split_once(':')?;
    let looks = !project.is_empty()
        && !task.is_empty()
        && !task.contains(':')
        && !key.contains('/')
        && !key.contains(char::is_whitespace);
    looks.then_some(())?;
    Some(format!("{}/{key}", repo_name(find_root(cwd)?)))
}

/// At most 12 of `names`, comma separated, then `and N more` for the rest.
fn capped(names: &[&str]) -> String {
    const SHOWN: usize = 12;
    let mut text = names
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        text.push_str(&format!(", and {} more", names.len() - SHOWN));
    }
    text
}

/// What exists, for a target `<project>:<task>` (`project` empty for `:<task>`) that matches
/// nothing: the projects that have `task`, else the tasks `project` has, else (an unknown
/// project) the projects, else (`:<task>`) every task name. `None` for a workspace with no tasks.
fn no_match_hint(tasks: &[Task], project: &str, task: &str) -> Option<String> {
    let names = |pick: &dyn Fn(&Task) -> Option<&str>| -> Vec<&str> {
        let set: BTreeSet<&str> = tasks.iter().filter_map(pick).collect();
        set.into_iter().collect()
    };
    let with_task = names(&|t| (t.task == task).then_some(t.project.as_str()));
    if !with_task.is_empty() {
        return Some(format!("projects with {task}: {}", capped(&with_task)));
    }
    let of_project = names(&|t| (t.project == project).then_some(t.task.as_str()));
    if !of_project.is_empty() {
        return Some(format!("tasks in {project}: {}", capped(&of_project)));
    }
    if project.is_empty() {
        let all = names(&|t| Some(t.task.as_str()));
        return (!all.is_empty()).then(|| format!("tasks: {}", capped(&all)));
    }
    let all = names(&|t| Some(t.project.as_str()));
    (!all.is_empty()).then(|| format!("no project {project}; projects: {}", capped(&all)))
}

/// The cost class of a moon task, by its name.
pub fn kind(task: &str) -> Kind {
    match task {
        "test" => Kind::Test,
        "typecheck" => Kind::Typecheck,
        "build" => Kind::Build,
        _ => Kind::Command,
    }
}

#[derive(Deserialize)]
struct Query {
    tasks: BTreeMap<String, BTreeMap<String, QueryTask>>,
}

#[derive(Deserialize)]
struct QueryTask {
    target: String,
    #[serde(default)]
    deps: Vec<QueryDep>,
}

#[derive(Deserialize)]
struct QueryDep {
    target: String,
}

/// One task of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub project: String,
    pub task: String,
    pub target: String,
    pub deps: Vec<String>,
}

/// The tasks in `moon query tasks`'s output, by project then task.
pub fn parse(json: &str) -> Result<Vec<Task>, Error> {
    let query: Query = serde_json::from_str(json)
        .map_err(|e| Error::new(format!("cannot read moon query tasks: {e}")))?;
    Ok(query
        .tasks
        .into_iter()
        .flat_map(|(project, tasks)| {
            tasks.into_iter().map(move |(task, t)| Task {
                project: project.clone(),
                task,
                target: t.target,
                deps: t.deps.into_iter().map(|d| d.target).collect(),
            })
        })
        .collect())
}

/// One target to run, and the nodes (indices into the graph) it runs after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub project: String,
    pub task: String,
    pub target: String,
    pub deps: Vec<usize>,
}

/// The targets `requested` names (`:task` is that task in every project that has it,
/// `project:task` exactly that one) and every target they depend on, transitively, in dependency
/// order: each node after all of its `deps`. A target that is neither form, or matches nothing, is
/// a usage error naming it; a dependency on a target moon does not list, or a cycle, is an error.
pub fn graph(tasks: &[Task], requested: &[String]) -> Result<Vec<Node>, Error> {
    let by_target: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.target.as_str(), t)).collect();
    let mut wanted = BTreeSet::new();
    for target in requested {
        let (project, task) = target
            .split_once(':')
            .filter(|(p, t)| !t.is_empty() && !t.contains(':') && !p.starts_with(['#', '^', '~']))
            .ok_or_else(|| {
                Error::usage(format!(
                    "{target:?} is not a target: give :<task> or <project>:<task>"
                ))
            })?;
        let mut matched = false;
        for t in tasks
            .iter()
            .filter(|t| t.task == task && (project.is_empty() || t.project == project))
        {
            matched = true;
            wanted.insert(t.target.as_str());
        }
        if !matched {
            let hint = no_match_hint(tasks, project, task)
                .map(|hint| format!("\n  {hint}"))
                .unwrap_or_default();
            return Err(Error::usage(format!("{target} matches no task{hint}")));
        }
    }

    // Every dependency, transitively.
    let mut stack: Vec<&str> = wanted.iter().copied().collect();
    while let Some(target) = stack.pop() {
        for dep in &by_target[target].deps {
            if !by_target.contains_key(dep.as_str()) {
                return Err(Error::new(format!(
                    "{target} depends on {dep}, which moon does not list"
                )));
            }
            if wanted.insert(dep.as_str()) {
                stack.push(dep.as_str());
            }
        }
    }

    // Kahn's order, the smallest ready target first, so the order is the same every time.
    let mut left: BTreeMap<&str, BTreeSet<&str>> = wanted
        .iter()
        .map(|&t| (t, by_target[t].deps.iter().map(String::as_str).collect()))
        .collect();
    let mut order: Vec<&str> = Vec::with_capacity(left.len());
    while !left.is_empty() {
        let ready: Vec<&str> = left
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(&t, _)| t)
            .collect();
        if ready.is_empty() {
            let stuck: Vec<&str> = left.keys().copied().collect();
            return Err(Error::new(format!(
                "moon reports a dependency cycle among {}",
                stuck.join(", ")
            )));
        }
        for target in ready {
            left.remove(target);
            for deps in left.values_mut() {
                deps.remove(target);
            }
            order.push(target);
        }
    }

    let index: BTreeMap<&str, usize> = order.iter().enumerate().map(|(i, &t)| (t, i)).collect();
    Ok(order
        .iter()
        .map(|&target| {
            let task = by_target[target];
            Node {
                project: task.project.clone(),
                task: task.task.clone(),
                target: task.target.clone(),
                deps: task.deps.iter().map(|d| index[d.as_str()]).collect(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests;
