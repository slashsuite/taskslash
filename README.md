# TaskSlash

TaskSlash is admission control for builds and tests on one machine. Several gates and browser runs
started at once (by you, or by coding agents in several worktrees) overload a desktop until tests
time out for reasons that have nothing to do with the code. TaskSlash puts a small daemon in front of
them: each command asks it for a slot, and it keeps the queue and the record of what ran.

Every feature, with where it lives in the source: [FEATURES.md](FEATURES.md).

This version runs each job in its own systemd scope, measures it, and records what it cost, and the
daemon samples the whole machine once a second. From those records it estimates what each key will
cost, and a job starts only when its estimate fits in the room the machine has left. A job that
waits says what it waits for.

The proof that it works on a real desktop under load: [`docs/proof/phase-1.md`](docs/proof/phase-1.md).

## Use

```bash
taskslash daemon                          # run the daemon (normally under systemd, below)
taskslash run -- bun run test             # run a command as a job
taskslash run --name gate -- sh -c 'exit 3'; echo $?   # 3
taskslash run --kind browser --name visual -- bun run visual   # a cost class for its defaults
taskslash run --cpu 4 --mem 3000 -- bun run visual             # say what it costs (threads, MiB)
taskslash run --class urgent -- bun run test                     # first in line (default: normal)
taskslash moon :typecheck :test          # moon targets, one job per target
taskslash why                             # why each waiting job waits, one line each
taskslash why 7                           # why job 7 waits ("running", or what became of it)
taskslash why 7 --json                    # the reason as an object
taskslash cancel 7                        # take job 7 off the queue, or stop it (SIGTERM, SIGKILL 10s on)
taskslash cancel 7 --grace 0              # stop it with SIGKILL straight away
taskslash history                         # every key with runs, the most recently run first
taskslash history gate                    # the estimate of "gate", then what its last 20 runs cost
taskslash history "bun test"              # a run with no --name is keyed by its first two words
taskslash history shop/web:test           # a taskslash moon target: <repo>/<project>:<task>
taskslash history web:test                # the same, typed inside the shop workspace
taskslash history gate --limit 5 --where shop   # its last 5 runs submitted from the worktree "shop"
taskslash history gate --json             # the same, as {"estimate": ..., "runs": [...]}
taskslash status                          # is this machine busy, and with what
taskslash status --json                   # the same, as one line of JSON
taskslash top                             # the machine, the jobs and their output, live
taskslash logs 7                          # what job 7 has printed so far, running or ended
taskslash logs 7 -f                       # the same, followed until it ends
taskslash logs --last gate                # the newest run of "gate", running or not
taskslash logs --failed                   # the newest run that failed in this worktree
taskslash logs --failed --all             # the newest run that failed on the whole machine
taskslash events                          # one JSON line per job queued, started, finished
taskslash plugins                         # the plugins on PATH (below)
```

`taskslash run` runs the command itself, in your directory, with your environment and terminal, and
exits with its exit code (128+N when signal N killed it, 127 when it cannot be started). With no
daemon running it does not run the command and exits 1; so do `taskslash status`, `taskslash why`,
`taskslash cancel`, `taskslash logs <job>`, `taskslash events` and `taskslash top`. With no daemon,
`taskslash history` and `taskslash logs --run`, `--last` or `--failed` read the database
read-only, after one line on stderr saying so, and exit 1 only when there is no database.

`taskslash` with no command prints the help and exits 0; `taskslash --version` prints the version
and the commit it was built from, `taskslash 0.1.0 (b82b25e)`.

### Every job's output is kept

`taskslash run` copies everything the command writes into a log of the job's own, and still writes
it to you exactly as before. So `taskslash run -- bun run test | tail -20` shows you 20 lines, and
the whole output is still there afterwards: `taskslash logs --last 'bun run'`.

- **On a terminal** (your stdout is one), the command runs on a pseudo-terminal of its own, so it
  sees a terminal and keeps its colours and progress bars. Its window size follows yours, and what
  you type reaches it. Your terminal is put back as it was when the command ends.
- **Otherwise** (a pipe, a file, a background run), stdout and stderr stay separate: each goes to
  your matching stream and into the log, so `> out 2> err` still splits them.
- **Signals.** The command runs in a process group of its own. SIGINT, SIGTERM and SIGHUP sent to
  `taskslash run` (Ctrl-C included) are passed on to that group, so the command ends the way it
  would have without TaskSlash, and the exit code is still its own.

`taskslash moon` does the same for each target: its stdout and stderr go to you and into that
target's own log.

A log holds stdout and stderr together, in the order the client read them, with nothing added. It
is a file per job in `logs` beside the database (`~/.local/state/taskslash/logs/` by default, so
`TASKSLASH_DB` moves it too), or in `$TASKSLASH_LOGS`. The daemon creates the directory with mode 0700 when it is missing (one that is already there keeps its mode), and each log is 0600.
A log is named after the job's systemd unit, `taskslash-<daemon pid>-<job>.log`: job numbers start
again at 1 when the daemon restarts, and the pid keeps the names apart. `taskslash status --json`
shows each running job's `log_path`, and `taskslash history --json` each run's (null for a run
recorded before logs were kept).

**Retention.** At startup and once an hour, the daemon deletes the logs (`taskslash-*.log`, nothing
else in the directory) of jobs that ended over 7 days ago, then the oldest ended ones until the directory is under 1 GiB. A running job's log is never
deleted. `TASKSLASH_LOG_DAYS` (days, fractions allowed) and `TASKSLASH_LOG_MIB` change the limits.
The run's row keeps the path of a deleted log.

**Reading a log.** `taskslash logs` prints one log to stdout, unchanged, after one line on stderr
that says whose it is, so the log itself stays pipeable:

```
run 3140 taskslash/test in taskslash-wt, exit 2, 2026-11-17 20:22
job 12 taskslash/test in taskslash-wt, running since 20:22
```

The worktree is the last part of the submitter's path, as `top` and `history` show it, and the time
is local.

| Command | Picks |
| --- | --- |
| `taskslash logs <job>` | the job with that number, as `taskslash status` shows it: running, or ended since the daemon started (it remembers the newest 1000; a restart forgets them) |
| `taskslash logs --run <id>` | the recorded run with that id, as `taskslash history` shows it |
| `taskslash logs --last <key>` | the newest run of the key, running or not |
| `taskslash logs --failed [<key>]` | the newest run (of the key) submitted from this worktree with an exit code that is not 0; a signal's 128+N counts, a run with no exit code (its client went away) does not |
| `taskslash logs --failed [<key>] --all` | the same, from every worktree on the machine |

This worktree is the one `taskslash run` reports from here: the nearest directory above the current
one with a `.git`. On a shared machine the newest failure is usually someone else's, so `--failed`
looks only at yours until `--all` widens it.

`-f` follows a running job's log until the job ends, then exits. It exits 0 when it printed the log,
and 1 with one line on stderr when there is nothing to print:

| Case | Message |
| --- | --- |
| a job number the daemon does not know | `taskslash: no job 7 (the daemon restarted or forgot it); find the run with taskslash history <key>, then taskslash logs --run <id>` |
| a job that ended before it started | `taskslash: job 7 never ran (it ended before it started), so it has no log` |
| no failed run in this worktree | `taskslash: no failed run in shop; taskslash logs --failed --all searches the whole machine` |
| no run of the key | `taskslash: no run of gate` |
| a run recorded before logs were kept | `taskslash: run 12 has no log` |
| a log retention deleted | `taskslash: the log of run 12 has expired` |

It exits 2 on a usage error (`--all` without `--failed` is one). A daemon older than this client
answers with less: the header names only the run or job, and `--failed` searches the whole machine
and says so on stderr (`(this daemon is older: searched the whole machine; reinstall it)`).

### moon targets

```bash
taskslash moon :typecheck :test           # every project's typecheck and test
taskslash moon cli:test --force           # one target, with moon's cache bypassed
taskslash moon :test --class urgent       # the jobs' class (default: TASKSLASH_CLASS, else normal)
```

`taskslash moon` replaces `moon run --concurrency 3 :typecheck :test`. A fixed `--concurrency`
is blind to the other gates on the machine: three gates at once are nine heavy tasks. Here each
target is its own job, so the daemon's budget decides how many run at once, across every worktree.

It finds the workspace (the first directory from here up with `.moon/workspace.yml`) and its moon
(`node_modules/.bin/moon` there, else `moon` on `PATH`), and reads `moon query tasks`. A workspace
whose root has `scripts/moon.ts` runs moon through that wrapper instead: every moon command below
(the query and each target's `moon run`) runs as `bun scripts/moon.ts <the same arguments>`, the way
the repository's own scripts call it, so whatever the wrapper sets up (a toolchain found and put
first on `PATH`, a variable a task hashes) holds under TaskSlash too. A target is
`:<task>` (that task in every project that has it) or `<project>:<task>`; anything else, or one that
matches nothing, is a usage error (exit 2). One that matches nothing is followed by what exists: the
projects that have that task (`projects with test: cli, web`, at most 12), or, when only the task is
wrong, the project's tasks (`tasks in cli: lint, test`). With no moon found, it says to run your
package manager's install first (exit 1). The requested targets and every target they depend on,
requested or not, each run **exactly once**, after all their dependencies passed, as
`moon run <target> --upstream none` (plus `--force` when given), so moon never re-runs a dependency
TaskSlash already ran. That holds for a task with its cache off too: it runs once, before what needs
it. A target whose dependency failed, directly or not, is skipped; the others keep running. Each
job's output is passed through as moon prints it.

Each job is named `<repo>/<target>`, where `<repo>` is the repository's name, the same for every
worktree and clone of it, so they share one history: the last part of the `origin` remote's URL,
less `.git` (`shop` for `git@github.com:me/shop.git`, whatever folder it is cloned into). With no
`origin`, it is the folder name of the main checkout (`shop` for both `shop` and its worktree
`shop.feature-x`). Its kind is the task's name when that is `test`, `typecheck` or `build`, else `command`.
It ends with one line per target, in dependency order, each ending with the key its run is
recorded under, and a failed target followed by the command that prints its log:

```
a:gen pass 0.1s [shop/a:gen]
b:test pass 0.1s cached [shop/b:test]
b:typecheck fail 0.1s [shop/b:typecheck]
  log: taskslash logs --last shop/b:typecheck
b:lint skipped - [shop/b:lint]
```

`cached` marks a target moon replayed from its cache. That run is recorded with `cached` set and its
moon hash, and is left out of the estimate: replaying a cache costs nothing like the task. Exits 0
when every target passed, else 1.

A `moon run` typed by hand still works as it always did. The daemon does not see it as a job; it
is counted as unmanaged load, like anything else on the machine.

### What a run records

When `taskslash run` ends, the daemon writes a row: the key, the kind, the class, the command, its
directory, its submitter (below), the start time, wall time, CPU seconds, the peak CPU over any 5 seconds (in threads), peak memory,
MiB written, the exit code, and the machine's pressure while it ran (`machine_cpu_some`,
`machine_mem_some`, `machine_io_some`: the mean of the `some avg10` readings taken during the run,
null when none was). A `taskslash moon` job also records `cached` (moon replayed it from cache) and
`moon_hash`; both are null for any other run. The key is `--name` when given, else the first two words of the command
(`bunx moon`), or of the script of `sh -c '<script>'` (also `bash`, `dash`, `zsh`, `ksh`, and
flags such as `-lc`): `sh -c 'cargo test --locked'` is `cargo test`. A leading `cd <dir> &&` is
skipped and `env`, `nice` and `timeout` are unwrapped, so `sh -c 'cd /repo && nice cargo test'` is
`cargo test` and `env A=1 timeout 60 bun test` is `bun test`. In both, a word that is a
path (starting with `/`, `./`, `../` or `~/`) is cut to its last part (`/usr/bin/cargo test` is
`cargo test`), and a key over 60 characters keeps its first and last 24 or so around five hex digits of a hash of
the whole key (`...53a45...`), so two long names that differ only in the middle stay apart. Give a
`--name` to anything you run more than once, so its runs share a key you can type. A key typed to
`history` or `logs` is shaped the same way, so the whole long name still finds its runs (a key
recorded before keys were shaped is still found as it was typed). `taskslash history <key>` prints the newest 20 runs of a key (`--limit` for another
number, `--where <dir>` for only those submitted from a worktree, by its directory name or full
path), `-` for a value that was not measured, and the worktree each ran in; a run whose
`taskslash run` went away before the command ended has no exit code (the daemon stops what is left
of its command, SIGTERM then SIGKILL 10 seconds on, before it counts the job as ended). With no key it lists every key
that has runs, the most recently run first, with its run count, last run, last exit code and the
worktree it last ran in (`--json`: an array). A key with no runs exits 1 and names up to five known
keys like it on stderr. Inside a moon workspace, a key that has no runs and looks like a moon target
(`<project>:<task>`, no `/`) is tried as `<repo>/<project>:<task>`, the key `taskslash moon` records
it under; when that has runs they are shown, and `key: <repo>/<project>:<task>` goes first on stderr. A client newer than the daemon it talks to says so and asks you to reinstall
the daemon.

**Measurement needs a systemd user session.** The daemon checks once at startup whether
`systemd-run --user --scope` works. When it does, every job runs in a transient scope under
`taskslash.slice` (on systemd 254 and newer with `--expand-environment=no`, so `$` in your command
is left alone), and the daemon reads the scope's cgroup every 250 ms while it runs. When it does
not (CI, a container, macOS), the daemon logs `taskslash: no systemd user session, jobs run
unmeasured`, runs jobs as plain children, and records wall time and exit code only. Jobs are
measured, never capped. `TASKSLASH_LAUNCH=plain` makes the daemon use plain children even where a
user session exists (the tests use it to run both paths on one machine).

The database is SQLite, at `$TASKSLASH_DB`, else `$XDG_STATE_HOME/taskslash/taskslash.db`, else
`~/.local/state/taskslash/taskslash.db`. Only the daemon writes it. It is in WAL mode, so
recent rows can sit in `taskslash.db-wal` beside it: back it up with
`sqlite3 taskslash.db ".backup copy.db"`, not by copying the file.

The daemon listens on `$TASKSLASH_SOCKET`, or `$XDG_RUNTIME_DIR/taskslash/daemon.sock` when that is
unset. The socket's directory is created with mode 0700: only you can submit jobs.

With `TASKSLASH_PHASES=1`, `taskslash run` writes one more line on stderr when the job ends, how
many milliseconds each phase of the start took:
`taskslash: phases connect=0.014 admit=0.234 launch=1.856 command=12.059 drain=0.016 report=8.578 ms`.
The start benchmark (`crates/taskslash-floor`) reads it.

### Kinds and estimates

`--kind` is a job's cost class: `test`, `typecheck`, `build`, `browser` or `command` (the default).
It only matters until the key has history. `taskslash history <key>` prints the key's estimate above
its runs, each figure marked `history` or `default` (or `given` in `status`, for a run's `--cpu`
or `--mem`):

| Figure | From the key's runs | Default |
| --- | --- | --- |
| `mem_mib` | the largest peak memory of the newest 5 measured runs, plus 20% | `typecheck` 3 GiB, `build` and `browser` 4 GiB, else 2 GiB |
| `cpu_threads` | the 75th percentile (nearest rank) of mean threads, CPU seconds over wall time, over the newest 10 calm runs; `cpu_peak_5s` is the largest 5 second peak of the same runs | `build` and `browser` 4, else 2 |
| `write_mib_s` | the mean of MiB written per second over the newest 10 measured runs | 0 |
| `duration_s` | the median wall time of the newest 10 calm runs | none |

A run is **measured** when it has CPU seconds, peak memory and an exit code (the scope path) and is
not a moon cache hit, and
**calm** when it is measured and the machine's CPU `some avg10` during it is known and at most 40.
A run on a starved machine still tells the truth about its memory and bytes written, but not about
its CPU share or how long it takes, so those two use calm runs only. The kind for the defaults is
the newest run's.

### Admission

The daemon decides, for each waiting job in the queue's order (below), whether it starts now.
Three resources count: CPU (threads), memory (MiB) and disk writes (MiB/s).

- **Budget:** CPU is 85% of `cpu.threads` (the CPUs the daemon may use), memory 80% of
  `memory.total_mib`, disk writes 150 MiB/s. Override them in the daemon's environment with
  `TASKSLASH_CPU_SHARE`, `TASKSLASH_MEM_SHARE` and `TASKSLASH_WRITE_MIB_S`.
- **Committed:** each running job counts at the larger of its estimate and what it uses now (CPU
  and writes over the last 5 seconds, memory at its peak so far; an unmeasured job counts at its
  estimate), plus **unmanaged** use: what the machine shows minus what the jobs use now (an
  unmeasured job at its estimate, so it is not counted twice), floored at 0, the mean of the last
  30 one second samples. Machine CPU is `cpu.busy` times `cpu.threads`,
  memory `total_mib - available_mib`, writes the sum over the disks.
- **A job fits** when, for every resource, committed plus its estimate is within the budget.
- **Pressure gates:** while any holds nothing starts (but see the wait cap below): CPU `some avg10` over 40, memory `some avg10` over 5,
  I/O `full avg10` over 5 or `some avg10` over 25, `available_mib` under 10% of `total_mib`, a
  watched mount over 90% used.
- **Never seen:** a key is seen once one of its runs has finished. At most one job whose key was
  never seen runs at a time, unless the caller gave both `--cpu` and `--mem`.
- **Too big to ever fit:** a job whose estimate alone is over a budget starts when no other job is
  running and no pressure gate holds.
- **Wait cap:** when no managed job is running, a job that has waited 3 minutes on load TaskSlash
  does not manage starts anyway: on the CPU pressure gate, either I/O pressure gate, or a budget
  that unmanaged use fills. Under a pressure gate only the job first in line is looked at, so the cap starts
  nothing until that job has waited 3 minutes. Only one starts this way at a time, since once it
  runs a managed job does. The memory pressure, memory available and mount gates are never capped
  (running out of memory or disk does harm a wait avoids), so the cap applies only when none of
  them holds. The daemon's start line says so, e.g. `job 12 started (normal) past its 3 min wait
  cap: cpu pressure 52% over the 40% gate: <command>`.

`--cpu <threads>` and `--mem <MiB>` on `run` replace the learned estimate for that run.

### Order

`--class` gives a job its priority class: `urgent`, `flake`, `interactive`, `normal` or
`background`, highest first. Without it the class is `$TASKSLASH_CLASS` (so a launcher can set it
once for everything it runs), else `normal`. An unknown class, from either, is a usage error
(exit 2).

A job's **submitter** is the nearest directory, from its working directory up, that holds a `.git`
(a directory in a clone, a file in a worktree), else the working directory itself. Fair share is
counted per submitter.

Each pass over the queue (on every submit, end and machine sample) takes the waiting jobs in this
order:

1. **Effective class**, highest first. A job that has waited more than 15 minutes moves up one
   class, and one more for every further 15 minutes, never past `urgent`.
2. **Within a class**, the submitter holding the least of the machine first: for each resource, the
   sum of `max(estimate, observed)` of its running jobs over that resource's budget, and the
   largest of the three (Dominant Resource Fairness). A submitter with nothing running holds 0. A
   job the pass starts counts at once, so two jobs from one idle worktree do not both go first.
3. Then the job that has waited longest, then the lower job number.

A job that fits starts. A job that does not is passed over and the pass goes on, so a later,
smaller job may start (**backfill**), unless the one passed over waits for room and has waited more
than 5 minutes:
then nothing behind it starts until it has (**the starvation guard**), and those behind it wait
with `behind <key> (job <n>), which has waited over 5 minutes and is not passed`. A pressure gate holds every
job. A higher class never stops, kills or freezes a running job; it is only first in line.

**Weights.** On the scope path a job runs with `-p CPUWeight=<w>` from its class: `urgent` 400,
`flake` 300, `interactive` 200, `normal` 100, `background` 20. Weights share out a busy CPU between
running jobs; they do not admit. The plain path has no weight.

A job that waits is told why on stderr when it is queued and every 15 seconds after, with its job
number. The line names the first thing that holds it, checked in this order: a pressure gate, the never-seen
rule, a resource. A resource line names the three running jobs that hold the most of it, biggest
first, and counts the rest. The first line of a job whose key has never run (and that was given
neither `--cpu` nor `--mem`) also says what its kind is assumed to take:

```
taskslash: job 18 waiting 45s (2nd in line): cpu needs 4.8, free 1.8; held by cli-db:test (job 3), visual (job 5), lint (job 7) and 8 more
taskslash: job 19 waiting 0s (1st in line): cpu pressure 52% over the 40% gate (never run before: assumed 2.0 threads; --cpu 0.1 says otherwise)
```

A daemon older than the job number sends lines without it (`taskslash: waiting 30s ...`).

`taskslash why <job>` prints the same sentence and `running` for a running job. For a job that
ended since the daemon started it names the run: `job 1 ended: run 3140 of shop/web:test, exit 0
(taskslash logs 1 shows its log)` (or that it ended without running, or that its run was not
recorded). It exits 1 for a job the daemon does not know (it forgets ended jobs when it restarts,
and keeps the newest 1000; `taskslash history` lists the keys). `taskslash why` with no job prints
one line per waiting job, `job 18 shop.wt-2 shop/web:test: <reason>`, or `nothing is waiting`;
with `--json`, an array of `{"job", "key", "submitter", "reason"}` (`[]` when nothing waits). With
`--json` and a job it prints the
reason: `{"kind":"resource","resource":"cpu","needs":4.8,"free":1.8,"held_by":[{"job":3,"key":
"cli-db:test","amount":3.0}]}`, `{"kind":"gate","gate":"cpu_pressure","value":52.0,"limit":40.0}`,
`{"kind":"never_seen","job":3,"key":"..."}`, `{"kind":"behind","job":3,"key":"..."}`,
`{"kind":"running"}`, or for an ended job `{"kind":"ended","ended":{"as":"run","run":3140,"key":
"...","exit_code":0}}` (`as` is also `never_ran`, `unrecorded` or `recording`). A `taskslash run` stopped
while it waits leaves the queue and its command never runs.

**Cancel a job from any terminal.** `taskslash cancel <job>` takes the number `status`, `why` and the
waiting line show. A waiting job leaves the queue: its `taskslash run` prints `taskslash: job 12
cancelled while waiting` and exits 1 without running the command. A running job is stopped by its
own `taskslash run`, which prints `taskslash: job 12 cancelled` and sends the command's process
group SIGTERM, then SIGKILL after `--grace` seconds (10 by default, 0 for SIGKILL straight away) if
it is still running; the run is recorded with the code it ends with (143 after SIGTERM, 137 after
SIGKILL), and `run` exits with that code. `cancel` prints `job 12 cancelled (was waiting)` or `job
12 cancelled (was running; SIGTERM sent, SIGKILL in 10s if it has not ended)` and exits 0; a second
cancel of the same job exits 0 and says it is already being cancelled. A job that is neither
waiting nor running exits 1, saying how it ended when the daemon remembers. There is no other
access check than the socket's: it sits in `$XDG_RUNTIME_DIR` in a directory only you can open, so
only you can cancel your jobs (a `TASKSLASH_SOCKET` elsewhere is as private as its directory).

**A fake machine, for tests.** With `TASKSLASH_FAKE_MACHINE=<file>` in its environment the daemon
reads the machine from that file (a `machine` object as `status --json` prints it) on every sample
instead of from `/proc`, keeping the last good reading when the file cannot be read. It exists so
tests can set the machine's load and pressure; do not use it otherwise.

### What status shows

`taskslash status` answers from the daemon's latest machine reading, taken once a second (free
space every 30 seconds), and lists the running jobs with their last live reading. The plain form is
a few lines: the daemon's build, pid and how long it has been up since when (and a
note when this `taskslash` is a different build), then CPU, the `some avg10` pressures, memory, each disk, each mount, then one line per
running job and one per waiting job, each with the worktree it was submitted from, or `no jobs
running or waiting`:

```
job 3  12s  shop.wt-1  shop/web:test (normal) 1.31 threads 706 MiB
waiting 9  shop.wt-2  visual (browser, background, as normal) 960s (2nd in line): ...
```

A running job shows how long it has run (`12s`, `1m05s`, `1h02m`) and its last reading (`-` before
the first one); `as normal` is a class waiting has raised.

| Field (`--json`) | What it is |
| --- | --- |
| `machine.sampled_at` | when the reading was taken, RFC 3339 UTC |
| `cpu.threads` | the CPUs the daemon may run on (its `Cpus_allowed_list`): the budget jobs can use, which can be fewer than are online |
| `cpu.threads_online` | every CPU the kernel has online |
| `cpu.busy` | the busy share (0 to 1) of the `threads` CPUs over the last second, from `/proc/stat`; iowait counts as idle |
| `*.pressure` | `/proc/pressure/{cpu,memory,io}`: the share of time, in percent, that some (`some`) or all (`full`) runnable work waited, averaged over 10 and 60 seconds. A missing `full` line is 0 |
| `memory.*_mib` | `/proc/meminfo`: `MemTotal`, `MemAvailable` (what can be handed out without swapping), `SwapTotal`, `SwapFree` |
| `io.disks[]` | each physical disk in `/sys/block` (not `loop`, `ram`, `zram` or `dm-` devices): `busy` is the share of time it had I/O in flight, `write_mib_s` its write rate |
| `mounts[]` | `$HOME`, `$TMPDIR` and `/tmp`, one per file system: size, space free to you, and the share used |
| `load` | `/proc/loadavg`, for people only |
| `jobs[]` | each running job: `job`, `key`, `kind`, `class`, `submitter`, `started_at`, its `cpu_threads` and `mem_mib` now (null until the first reading and always on an unmeasured run), and its `estimate` |
| `waiting[]` | each waiting job, in the order of the last pass: `job`, `key`, `kind`, `class`, `effective_class` (only when waiting has raised it), `submitter`, `waited_s`, `position` (1-based), `reason` (the sentence), `why` (the reason object) and its `estimate` |

Shares and rates are 0 in the first second after the daemon starts, before there are two readings.

### taskslash top

`taskslash top` puts what `status`, `why` and `logs --follow` say on one screen, live:

```
cpu 35% busy on 12 threads, pressure 14.94   load 7.25 4.14 3.94
mem 11.1 of 31.3 GiB used, 64% available, pressure 0.09   io some 1.04 full 0.53
┌ jobs ────────────────────────────────────────────────────┐┌ job 110 web:test: running ───────┐
│  110 running            normal      repo.wt-1  web:test  ││ RUN  src/app.test.ts             │
│  111 waiting 1st 4s     background  repo.wt-2  e2e       ││ ✓ renders the list (12 ms)       │
│  112 exit 3 4s          normal      repo.wt-1  lint      ││                                  │
└──────────────────────────────────────────────────────────┘└──────────────────────────────────┘
q quit   j/k select   enter page the log   c cancel   ? keys
```

- **The strip** on top: CPU busy and the threads jobs may use, CPU pressure, the load average;
  memory used of total and the share available, memory pressure, and IO pressure (`some` and
  `full`). All pressures are `some avg10` unless named, in percent. It refreshes once a second.
- **The job list** on the left: the running jobs, then the waiting ones in queue order (place in
  line and time waited), then the finished runs, newest first (exit code, wall time, `cached`
  when moon replayed it, or `never started`). Each row has the job number (`r<id>`, the run id,
  for a run recorded before top opened or before the daemon restarted), its state, class, the
  submitting worktree's directory name and the key.
- **The output pane** on the right: the selected job's log, as many of its last lines as fit,
  following new output as it is written. Colours, cursor moves and other control characters are
  removed, so a job cannot scramble the screen. A waiting job shows why it waits; a job that left
  the queue before it was admitted shows `never started`.

Keys (`?` shows them on screen):

| Key | Does |
| --- | --- |
| up and down, `j` and `k` | move the selection, which stays on the same job as the list changes |
| enter | open the selected log in `$PAGER` (`less` when unset); quitting the pager returns to top |
| `c`, then `c` again | cancel the selected running or waiting job, as `taskslash cancel` does; any other key keeps it |
| `?` | show or hide the keys |
| `q`, Esc, Ctrl-C | quit and give the terminal back (Esc first closes the keys) |

The finished list holds at most 50: when top opens it takes the runs that ended last from the
daemon's records, and adds each job that finishes while it is open, the last to end first. A run recorded before logs were kept says
so in the output pane. A line at the bottom says when it missed
events because it read them too slowly, or when the daemon stops answering (top keeps trying).

`top` exits 1 with the message `status` prints when no daemon answers, and 2 when stdout is not a
terminal.

### Plugins

Anything else can plug into TaskSlash as a program of its own, in any language. A plugin is an
executable named `taskslash-<name>` on `PATH`, found the way git and cargo find theirs (so a repo can
ship one in its `node_modules/.bin`, and a Rust one installs to `~/.cargo/bin`).

```bash
taskslash plugins                         # every taskslash-<name> on PATH, and the path that runs
taskslash plugins --json                  # [{"name", "path", "shadowed", "builtin"}, ...]
taskslash hello world                     # runs taskslash-hello world
```

There are three kinds:

| Kind | What it does | |
| --- | --- | --- |
| command | `taskslash <name> [args...]` runs `taskslash-<name> [args...]` | here now |
| listener | follows the daemon's event stream (job queued, started, finished) and changes nothing | here now |
| filter | rewrites a job's output on its way to the log and the terminal (secret redaction) | coming |

A command plugin replaces the `taskslash` process, so its exit code and signals are its own. Built-in
commands always win: a `taskslash-run` never runs. With no `taskslash-<name>` on `PATH`,
`taskslash <name>` is a usage error (exit 2) that names the program it looked for. Only regular files
(or links to one) with an execute bit count. When two directories on `PATH` hold the same name, the
first one runs and `taskslash plugins` lists the later ones as shadowed. With none on `PATH`,
`taskslash plugins` says `no plugins on PATH` and what a plugin is named (`--json` prints `[]`).

A plugin needs no daemon to start, and gets this contract in its environment:

| Variable | What it holds |
| --- | --- |
| `TASKSLASH_PROTOCOL` | the version of the socket protocol, now `1`; it goes up on any breaking change |
| `TASKSLASH_SOCKET` | the daemon's socket path, as `taskslash` resolves it; unset when it does not resolve (no `XDG_RUNTIME_DIR`) |
| `TASKSLASH_BIN` | the absolute path of the running `taskslash`, to call back into it (`"$TASKSLASH_BIN" run -- ...`) |

An example: save this as an executable `taskslash-hello` on `PATH`, then run `taskslash hello world`.

<!-- plugin-example:start -->
```sh
#!/bin/sh
# taskslash-hello: greets, then says whether the daemon answers.
set -u
echo "hello, ${1:-you}"
echo "protocol $TASKSLASH_PROTOCOL"
if [ -S "${TASKSLASH_SOCKET:-}" ] && "$TASKSLASH_BIN" status >/dev/null 2>&1; then
  echo "the daemon is up"
else
  echo "no daemon"
fi
```
<!-- plugin-example:end -->

#### Listener plugins

A listener follows what the daemon does with jobs and cannot change any of it. It runs
`taskslash events`, which prints one line of JSON per job event on stdout as it happens, or it opens
`TASKSLASH_SOCKET` and subscribes itself: send `{"type":"subscribe"}` on a connection of its own, and
the daemon answers `{"type":"subscribed","protocol":1}`, then one event line per job event until the
listener hangs up. `taskslash events` prints only the event lines on stdout (on a terminal, one
`listening for job events` line on stderr first, so an empty screen does not look hung), exits 0 when the daemon goes away
and 1 when no daemon answers. The daemon starts no listener: run it yourself, or from a systemd unit
of your own.

```bash
taskslash events | jq -c 'select(.event.type == "finished" and .event.exit_code != 0)'
```

Each line is `{"type":"event","protocol":1,"seq":<n>,"event":{...}}`, with `seq` counting up per
daemon from 1. There are four events:

| `event.type` | Fields | When |
| --- | --- | --- |
| `queued` | `job`, `key`, `kind`, `class`, `submitter`, `cwd`, `argv`, `at` | the job is submitted, before it waits |
| `started` | `job`, `unit`, `launch`, `log`, `at` | the job is admitted and its command starts |
| `finished` | `job`, `key`, `exit_code`, `wall_ms`, `cached`, `moon_hash`, `run`, `log`, `at` | its run is recorded, or it left before it started |
| `dropped` | `count` | before the next event, when this listener missed some |

Every `queued` gets exactly one `finished`. `exit_code` is null when the job's client went away
before saying how the command ended, and `run` (the id `taskslash history` shows) is null when its
row was not written. A job that never started (its client hung up while it waited) has
`exit_code`, `run` and `log` null and `wall_ms` 0. `at` is RFC 3339, UTC. Events carry no output:
`log` is the job's log file, which the listener reads (`taskslash logs --run <id>`). A listener sees
only the events after it subscribed; nothing is replayed.

A slow listener never slows a job. The daemon queues at most 1024 events for each listener and
never waits for one: an event that does not fit is dropped for that listener, and the next event it
does get is preceded by one `dropped` with how many it missed (its `seq` is the last missed one's).
A listener whose connection fails is removed.

### Install the daemon as a systemd user unit

```bash
bun scripts/install-service.ts
journalctl --user -u taskslash -f         # the launch path, then one line per job start and end
```

The same command also upgrades: rerun it after pulling a new `main`, and a job running during the restart keeps running but is not recorded.

> **Status: alpha.** Changing fast. Expect breaking changes between versions.

## Layout

| Path | What it is |
| --- | --- |
| `packages/engine` | the engine, a TypeScript library |
| `crates/taskslash-core` | the engine, a Rust library |
| `crates/taskslash` | the `taskslash` command (the Rust binary) |
| `scripts` | the repo checks, and `record-machine.ts`, which records a proof run |

## Develop

```bash
bun install
bun run taskslash --version
bun run taskslash --help
bun run typecheck && bun run lint && bun run format:check && bun run test && bun run build
```

The Rust crates need [rustup](https://rustup.rs): `rust-toolchain.toml` pins the toolchain, and
`rustup toolchain install` installs it, once per machine. There is no release workflow for the
binary yet.

See [`AGENTS.md`](AGENTS.md) for the full gate and [`CONTRIBUTING.md`](CONTRIBUTING.md) for how to
send a change.

## Licence

MIT, see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
