# TaskSlash: repo conventions

This file is the operating agreement for contributors and agents working here: the gate, the landing
rule and the invariants. Keep it accurate.

## The gate

A moon workspace (moon 2.6.5, pinned in the root `package.json`) on Bun workspaces.
`.moon/workspace.yml` lists the projects, `.moon/tasks/*.yml` holds what they inherit (shared inputs,
the Vitest `test` task for the projects tagged `ts`, and the cargo tasks for the ones tagged `rust`),
and each project has a `moon.yml`. There is no toolchain file: every task runs on the system
toolchain, so moon never downloads Bun or runs `bun install`. Keep it that way (no
`.moon/toolchains.yml`, `pipeline.installDependencies: false`).

| Project | Path | What it is |
| --- | --- | --- |
| `engine` | `packages/engine` | the engine, a TypeScript library |
| `core` | `crates/taskslash-core` | the engine, a Rust library: the socket protocol, the daemon and the `run` client |
| `bin` | `crates/taskslash` | the `taskslash` binary (argv only, over `taskslash-core`) and its systemd user unit |
| `scripts` | `scripts` | the repo checks (typecheck and lint only) |

The `gate` job in `.github/workflows/ci.yml` runs these on GitHub-hosted runners (`ubuntu-latest`),
and a change is green when all of them pass **from the worktree**:

```bash
bun scripts/check-workflows.ts .github/workflows/*.yml   # no unguarded self-hosted job
bun install --frozen-lockfile     # a fresh worktree has no node_modules
bun run typecheck                 # moon run :typecheck  (tsc, strict)
bun run lint                      # moon run :lint, then check-dashes
bun run format:check              # oxfmt --check (`bun run format` fixes)
bun run test                      # moon run :test
bun run build                     # moon run :build
```

For the crates, `lint` is `cargo clippy --all-targets -- -D warnings`, `test` is `cargo test` and
`build` is `cargo build`, all `--locked`; `format:check` also runs `moon run :format-check`
(`cargo fmt --check`, and `cargo fmt` fixes it). On a new machine run `rustup toolchain install`
first: it installs the toolchain `rust-toolchain.toml` pins (the `gate` job does the same).

The `taskslash` tests run every job on the launch path the machine has: a systemd scope where there
is a user session, else a plain child, and they assert which one each run took. The `gate` job
starts a user session (`loginctl enable-linger`) and sets `TASKSLASH_REQUIRE_SCOPE=1`, so a missing
session fails the tests there instead of quietly testing only the plain path. Without a session
they say so on stderr.

One project: `bunx moon run scripts:typecheck`. moon caches in `.moon/cache` (gitignored); add `--force` to
rerun a cached task. A shared moon remote cache sits behind a bearer token read from
`MOON_REMOTE_TOKEN`; unset, moon warns about a 401 and runs on the local cache, so CI passes with no
secret. Lint rules are all errors; fix the code, do not disable a rule. A TODO carries an issue:
`TODO(#123)`.

`bunfig.toml` sets `[install] linker = "isolated"`: a package sees its own declared dependencies and
the root's, not another package's. Declare what a package imports in that package's `package.json`.

**Generated files are not formatted.** Anything a tool writes is excluded in `.oxfmtrc.json`'s
`ignorePatterns`. `**/*.md` is ignored too.

**/tmp is a quota'd tmpfs; do not fill it.** Every test run goes through `scripts/test-tmp-scope.ts`
(a Vitest and Playwright `globalSetup`), which gives it a fresh `TMPDIR` and removes it after. A new
project's `vitest.config.ts` lists the same `globalSetup`. Scratch goes under `$TMPDIR`, never a
hardcoded `/tmp`.

## Landing

- Code changes land through pull requests; nobody pushes to `main`. Branch, gate, rebase on
  `origin/main`, push, open the PR.
- Commit subjects are Conventional Commits: `<type>(<scope>): <subject>`, with type one of `feat fix
  docs chore refactor test perf build ci style revert`. The squash keeps the subject.
- Stage files by name or `git add -A`; never `git commit -am` (it drops new files).
- No `Co-Authored-By` trailer and no AI attribution line, in a commit or a pull request.
- No em dash or en dash anywhere (`scripts/check-dashes.ts` fails the lint on one).
- A pull request that changes what TaskSlash does, where it runs or its licence updates [`FACTS.md`](FACTS.md) in the same pull request, and moves its "Checked against" line to the commit and date it was checked.

## Invariants

1. **Exit codes are a contract.** `taskslash` exits 0 on success, 1 when the run failed for a
   reason that is not usage, 2 on a usage error. `taskslash run` exits with its command's code
   instead (128+N when signal N killed it, 127 when it cannot be started); it exits 1 itself only
   when it never ran the command (no daemon, a refused job).
2. **The TypeScript code writes through `process.stdout` / `process.stderr`**, not `console` (the
   lint bans it outside `scripts/`).
3. **A secret is read from the environment, never from the tree.**
