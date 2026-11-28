//! The workspace, the query and the graph against a trimmed copy of a real `moon query tasks`
//! (moon 2.6.5): three projects, an uncached generator two tasks share, and a test on a build.

use super::*;

const QUERY: &str = include_str!("query.json");

fn targets(nodes: &[Node]) -> Vec<&str> {
    nodes.iter().map(|n| n.target.as_str()).collect()
}

fn ask(requested: &[&str]) -> Result<Vec<Node>, Error> {
    let requested: Vec<String> = requested.iter().map(|&t| t.to_owned()).collect();
    graph(&parse(QUERY).unwrap(), &requested)
}

fn task(target: &str, deps: &[&str]) -> Task {
    let (project, task) = target.split_once(':').unwrap();
    Task {
        project: project.into(),
        task: task.into(),
        target: target.into(),
        deps: deps.iter().map(|&d| d.to_owned()).collect(),
    }
}

#[test]
fn the_query_is_read_by_project_then_task() {
    let tasks = parse(QUERY).unwrap();
    let all: Vec<&str> = tasks.iter().map(|t| t.target.as_str()).collect();
    assert_eq!(
        all,
        [
            "admin:build",
            "admin:generate-routes",
            "admin:test",
            "admin:typecheck",
            "cli:test",
            "cli:typecheck",
            "dashboard:generate-routes",
            "dashboard:test",
            "dashboard:typecheck",
        ]
    );
    let test = tasks.iter().find(|t| t.target == "dashboard:test").unwrap();
    assert_eq!(
        (test.project.as_str(), test.task.as_str()),
        ("dashboard", "test")
    );
    assert_eq!(test.deps, ["dashboard:generate-routes"]);
    assert!(parse("{\"tasks\": 3}").is_err());
}

#[test]
fn a_bare_task_is_every_project_that_has_it_and_its_deps_come_first() {
    let nodes = ask(&[":test"]).unwrap();
    assert_eq!(
        targets(&nodes),
        [
            "admin:build",
            "cli:test",
            "dashboard:generate-routes",
            "admin:test",
            "dashboard:test",
        ]
    );
    // Each node's deps point at nodes before it.
    for (i, node) in nodes.iter().enumerate() {
        assert!(node.deps.iter().all(|&d| d < i), "{node:?}");
    }
    assert_eq!(nodes[3].deps, [0], "admin:test on admin:build");
    assert_eq!(nodes[4].deps, [2], "dashboard:test on the routes");
}

#[test]
fn a_shared_dependency_is_one_node() {
    let nodes = ask(&[":typecheck", ":test", "dashboard:test"]).unwrap();
    let routes = nodes
        .iter()
        .filter(|n| n.target == "dashboard:generate-routes")
        .count();
    assert_eq!(routes, 1);
    assert_eq!(nodes.len(), 9, "{:?}", targets(&nodes));
}

#[test]
fn a_project_target_is_exactly_that_one_plus_what_it_needs() {
    let nodes = ask(&["admin:test"]).unwrap();
    assert_eq!(targets(&nodes), ["admin:build", "admin:test"]);
    assert_eq!(nodes[1].deps, [0]);
    assert_eq!(
        (nodes[1].project.as_str(), nodes[1].task.as_str()),
        ("admin", "test")
    );
}

#[test]
fn a_target_that_is_malformed_or_matches_nothing_is_a_usage_error() {
    for bad in [
        "test",
        ":",
        "cli:",
        "#tag:test",
        "^:build",
        "a:b:c",
        ":lint",
        "nope:test",
    ] {
        let e = ask(&[bad]).unwrap_err();
        assert!(e.is_usage(), "{bad}: {e}");
        assert!(e.to_string().contains(bad), "{bad}: {e}");
    }
}

/// The hint line under `<target> matches no task`.
fn hint(tasks: &[Task], requested: &str) -> String {
    let e = graph(tasks, &[requested.to_owned()]).unwrap_err();
    assert!(e.is_usage(), "{e}");
    let text = e.to_string();
    let (first, rest) = text.split_once('\n').expect("a hint line");
    assert_eq!(first, format!("{requested} matches no task"));
    rest.to_owned()
}

#[test]
fn a_target_that_matches_nothing_lists_what_exists() {
    let tasks = [
        task("cli:test", &[]),
        task("cli:lint", &[]),
        task("web:test", &[]),
        task("web:build", &[]),
    ];
    assert_eq!(
        hint(&tasks, "nosuch:test"),
        "  projects with test: cli, web"
    );
    assert_eq!(hint(&tasks, "cli:nosuch"), "  tasks in cli: lint, test");
    assert_eq!(hint(&tasks, ":nosuch"), "  tasks: build, lint, test");
    assert_eq!(
        hint(&tasks, "nosuch:nosuch"),
        "  no project nosuch; projects: cli, web"
    );
    let e = graph(&[], &["a:test".to_owned()]).unwrap_err();
    assert_eq!(
        e.to_string(),
        "a:test matches no task",
        "no tasks, no hint line"
    );

    let many: Vec<Task> = (0..15)
        .map(|i| task(&format!("p{i:02}:test"), &[]))
        .collect();
    assert_eq!(
        hint(&many, "nosuch:test"),
        "  projects with test: p00, p01, p02, p03, p04, p05, p06, p07, p08, p09, p10, p11, \
         and 3 more"
    );
}

#[test]
fn no_moon_says_to_install_first() {
    let base = std::env::temp_dir().join(format!("tks-nomoon-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let e = moon_binary_on(&base, Some(base.clone().into_os_string())).unwrap_err();
    assert!(!e.is_usage(), "{e}");
    assert!(
        e.to_string()
            .ends_with("; run your package manager's install (bun install) first"),
        "{e}"
    );
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn a_moon_target_resolves_to_its_key_only_inside_a_workspace() {
    let base = std::env::temp_dir().join(format!("tks-tkey-{}", std::process::id()));
    let shop = base.join("shop");
    fs::create_dir_all(shop.join(".moon")).unwrap();
    fs::write(shop.join(WORKSPACE_FILE), "projects: {}\n").unwrap();
    fs::create_dir_all(shop.join("apps/cli")).unwrap();

    let inside = shop.join("apps/cli");
    assert_eq!(
        target_key(&inside, "cli:test").as_deref(),
        Some("shop/cli:test")
    );
    for not_a_target in ["shop/cli:test", "cli", ":test", "cli:", "a:b:c", "bun test"] {
        assert_eq!(target_key(&inside, not_a_target), None, "{not_a_target}");
    }
    assert_eq!(target_key(&base, "cli:test"), None, "outside a workspace");
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_origin_names_the_repository_whatever_its_url_looks_like() {
    for (url, name) in [
        ("git@github.com:me/shop.git", Some("shop")),
        ("https://github.com/me/shop", Some("shop")),
        ("https://github.com/me/shop.git/", Some("shop")),
        ("\"ssh://git@host:2222/me/shop.git\"", Some("shop")),
        ("/srv/git/shop.git", Some("shop")),
        ("host:shop", Some("shop")),
        ("", None),
    ] {
        let config = format!("[remote \"origin\"]\n\tfetch = +refs/*\n\turl = {url}\n");
        assert_eq!(origin_name(&config).as_deref(), name, "{url}");
    }
    assert_eq!(
        origin_name("[remote \"upstream\"]\n\turl = x/shop.git\n"),
        None,
        "only origin counts"
    );
}

#[test]
fn a_cycle_is_an_error_not_a_hang() {
    let tasks = [
        task("p:a", &["p:b"]),
        task("p:b", &["p:c"]),
        task("p:c", &["p:a"]),
        task("p:d", &[]),
    ];
    let e = graph(&tasks, &[":a".to_owned(), ":d".to_owned()]).unwrap_err();
    assert!(!e.is_usage());
    assert_eq!(
        e.to_string(),
        "moon reports a dependency cycle among p:a, p:b, p:c"
    );
}

#[test]
fn a_dependency_moon_does_not_list_is_an_error() {
    let e = graph(&[task("p:a", &["q:gone"])], &["p:a".to_owned()]).unwrap_err();
    assert_eq!(
        e.to_string(),
        "p:a depends on q:gone, which moon does not list"
    );
}

#[test]
fn the_kind_comes_from_the_task_name() {
    assert_eq!(kind("test"), Kind::Test);
    assert_eq!(kind("typecheck"), Kind::Typecheck);
    assert_eq!(kind("build"), Kind::Build);
    assert_eq!(kind("generate-routes"), Kind::Command);
    assert_eq!(kind("lint"), Kind::Command);
}

#[test]
fn the_root_and_the_repository_name() {
    let base = std::env::temp_dir().join(format!("tks-moon-{}", std::process::id()));
    let main = base.join("shop");
    let wt = base.join("shop.feature-x");
    fs::create_dir_all(main.join(".git/worktrees/shop.feature-x")).unwrap();
    fs::create_dir_all(main.join(".moon")).unwrap();
    fs::write(main.join(WORKSPACE_FILE), "projects: {}\n").unwrap();
    fs::create_dir_all(wt.join("apps/cli")).unwrap();
    fs::create_dir_all(wt.join(".moon")).unwrap();
    fs::write(wt.join(WORKSPACE_FILE), "projects: {}\n").unwrap();
    let gitdir = main.join(".git/worktrees/shop.feature-x");
    fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    fs::write(gitdir.join("commondir"), "../..\n").unwrap();

    assert_eq!(find_root(&wt.join("apps/cli")), Some(wt.as_path()));
    assert_eq!(find_root(&main), Some(main.as_path()));
    assert_eq!(repo_name(&main), "shop");
    assert_eq!(
        repo_name(&wt),
        "shop",
        "a worktree is named for its main checkout"
    );

    // A gitdir that is not a worktree's (a submodule's) leaves the root's own name.
    let sub = base.join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join(".git"), "gitdir: ../shop/.git/modules/sub\n").unwrap();
    assert_eq!(repo_name(&sub), "sub");
    assert_eq!(repo_name(&base.join("none")), "none", "no .git at all");

    // With an origin, its name wins over the folder's, for the clone and every worktree of it.
    fs::write(
        main.join(".git/config"),
        "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = git@github.com:other/fork.git\n\
         [remote \"origin\"]\n\turl = git@github.com:me/storefront.git\n",
    )
    .unwrap();
    assert_eq!(repo_name(&main), "storefront");
    assert_eq!(
        repo_name(&wt),
        "storefront",
        "a worktree reads its main checkout's origin"
    );

    // A worktree of a bare repository reads the bare repository's config.
    let bare = base.join("shop.git");
    let bare_wt = base.join("bare-wt");
    fs::create_dir_all(bare.join("worktrees/bare-wt")).unwrap();
    fs::write(
        bare.join("config"),
        "[remote \"origin\"]\n\turl = git@github.com:me/storefront.git\n",
    )
    .unwrap();
    fs::write(bare.join("worktrees/bare-wt/commondir"), "../..\n").unwrap();
    fs::create_dir_all(&bare_wt).unwrap();
    fs::write(
        bare_wt.join(".git"),
        format!("gitdir: {}\n", bare.join("worktrees/bare-wt").display()),
    )
    .unwrap();
    assert_eq!(repo_name(&bare_wt), "storefront");

    // node_modules' moon first.
    let bin = main.join("node_modules/.bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("moon"), "").unwrap();
    assert_eq!(
        moon_command(&main).unwrap(),
        [bin.join("moon").into_os_string()]
    );
    fs::remove_dir_all(&base).unwrap();
}

#[test]
fn a_repos_moon_wrapper_runs_under_bun_in_place_of_moon() {
    let base = std::env::temp_dir().join(format!("tks-wrap-{}", std::process::id()));
    let root = base.join("shop");
    let tools = base.join("tools");
    fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::create_dir_all(&tools).unwrap();
    fs::write(root.join("node_modules/.bin/moon"), "").unwrap();
    let path = || Some(tools.clone().into_os_string());

    // No wrapper: moon itself.
    assert_eq!(
        moon_command_on(&root, path()).unwrap(),
        [root.join("node_modules/.bin/moon").into_os_string()]
    );

    // A wrapper and no bun to run it: an error that names the wrapper.
    let wrapper = root.join("scripts/moon.ts");
    fs::write(&wrapper, "").unwrap();
    let e = moon_command_on(&root, path()).unwrap_err();
    assert!(!e.is_usage(), "{e}");
    assert!(
        e.to_string().contains("scripts/moon.ts runs moon here")
            && e.to_string().contains("no bun on PATH"),
        "{e}"
    );

    // With bun on PATH, `bun <wrapper>`.
    fs::write(tools.join("bun"), "").unwrap();
    assert_eq!(
        moon_command_on(&root, path()).unwrap(),
        [tools.join("bun").into_os_string(), wrapper.into_os_string()]
    );

    // A wrapper does not stand in for an install: with no moon, the install hint still comes.
    fs::remove_file(root.join("node_modules/.bin/moon")).unwrap();
    let e = moon_command_on(&root, path()).unwrap_err();
    assert!(e.to_string().contains("bun install"), "{e}");
    fs::remove_dir_all(&base).unwrap();
}
