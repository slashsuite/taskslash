# TaskSlash facts

Checked against main at c5805f8 on 2026-11-28.

## What it is
TaskSlash stops builds and tests that start at the same time from overloading one computer: each one waits in a queue until your computer has capacity for it. It is early (alpha), version 0.1.0.

## Today
- `daemon-queue`: A daemon, `taskslash daemon`, holds the queue and decides when each submitted job starts.
- `run-command`: `taskslash run -- <command>` runs a command as a job, in your directory with your environment and terminal, and exits with the command's own exit code.
- `admission-by-capacity`: A job starts when its estimated CPU, memory and disk writes fit in the CPU, memory and disk writes the computer has free, and waits while the machine's CPU, memory or I/O pressure is over a set line. A job too big to ever fit starts when nothing else runs, and a job held only by load TaskSlash does not run starts after a set wait.
- `learned-estimates`: The daemon estimates what a job will cost from the recorded runs of the same name (a `--name`, else the command's first two words, or a `sh -c` script's, with paths cut to their last part and at most 60 characters), and falls back to a default for its kind (test, typecheck, build, browser, command) until there is history.
- `measured-runs`: With a systemd user session, each job runs in its own systemd scope and the daemon records its wall time, CPU, peak memory, disk writes and exit code in a SQLite database.
- `priority-classes`: A job has a priority class (urgent, flake, interactive, normal, background), and within a class the worktree using the least of the machine goes first.
- `waiting-reason`: A job that waits says its job number and why on stderr, naming the three biggest jobs that hold it and counting the rest, and the first time a job that never ran waits it says what it was assumed to take. `taskslash why <job>` prints the same reason, also as JSON, or for a job that ended since the daemon started, its run and exit code; `taskslash why` with no job lists every waiting job and why.
- `cancel`: `taskslash cancel <job>` stops a job from any terminal of the same user: a waiting job leaves the queue without running, and a running job's command gets SIGTERM, then SIGKILL after a grace period (10 seconds unless `--grace` says otherwise), and its run is recorded with the code it ended with.
- `moon-targets`: `taskslash moon <targets>` runs moon targets as one job per target, each once, after its dependencies passed, and ends with a line per target naming the key its run is recorded under and, for a failed target, the command that prints its log; a target that matches nothing lists the projects or tasks that do exist.
- `job-logs`: Every job's output is kept in a log file of its own until it passes a set age or the logs pass a set size, and `taskslash logs` prints it, by job (running or ended since the daemon started), run, name or the last failure in your worktree (or on the whole machine with `--all`), after a line saying whose run it is, its exit code and when it started.
- `history`: `taskslash history` lists every job name with runs, and `taskslash history <name>` prints a job's estimate and what its last runs cost and where they ran; an unknown name exits 1 and suggests names like it. Inside a moon workspace, `taskslash history <project>:<task>` finds that target's runs under the repository's name, which is its `origin` remote's name, so every clone and worktree of it shares one history.
- `status`: `taskslash status` shows the machine's load, pressure, memory, disks and the running and waiting jobs with the worktree each came from and how long each has run, or that no job runs or waits, also as JSON.
- `top`: `taskslash top` shows the machine's load and pressure, the running and waiting jobs, the jobs finished since it opened, and the selected job's output on one live terminal screen.
- `events`: `taskslash events` prints one JSON line per job queued, started and finished, for other tools to follow.
- `command-plugins`: A program named `taskslash-<name>` on `PATH` runs as `taskslash <name>` unless `<name>` is a built-in command, and `taskslash plugins` lists them.
- `desktop-proof`: On one desktop, three gates and two browser runs started together stayed under the pressure lines through TaskSlash, and the whole load finished sooner than without it, though each browser run took longer (docs/proof/phase-1.md).
- `no-release`: There is no release you can install yet: you build it from source with cargo.

## Planned
- `filter-plugins`: Filter plugins that rewrite a job's output on its way to the log and the terminal, such as secret redaction.
- `machine-pool`: Scheduling across a pool of machines.
- `ci-runners`: Scheduling for CI.

## Licence
- `licence`: open source under the MIT licence

## Where it runs
- `linux-systemd`: Linux with a systemd user session, where jobs are measured; `bun scripts/install-service.ts` builds it from the repo with cargo and installs the daemon as a systemd user service.
- `linux-no-systemd`: Linux without a systemd user session (a container or a CI runner), where jobs run unmeasured and only wall time and exit code are recorded.
