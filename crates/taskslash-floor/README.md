# taskslash-floor

The floor: how fast a command can run through a pass-through that does what `taskslash run` does at
the process level, with none of TaskSlash's own work. TaskSlash's overhead is read against it.

`taskslash-floor -- <command> [args...]` starts the command with stdout and stderr on pipes and
stdin inherited, copies each pipe to its own matching stream on one thread each, waits for the
command, and exits with its code (128 plus the signal number when a signal killed it, 127 when there
is no command or it cannot be started). No daemon, no admission, no leases, no log, no
pseudo-terminal, no process group, no signal forwarding.

`floor-bench` times starts of three rows, spawn to exit, and prints min, median, p95, p99 and max in
milliseconds, then where the `taskslash` row's time went, phase by phase, and the machine's state.

## Run it

```bash
bunx moon run floor:bench
```

It builds both binaries in release and runs `floor-bench` with 100 starts per row. Run it by hand
for other options: `target/release/floor-bench --runs 500 --taskslash ~/.cargo/bin/taskslash`.
It is not part of the gate.

To bench a build of your own against a daemon of its own (never the live one), start it on its own
socket, database and log directory and point the benchmark at it:

```bash
cargo build --release --locked --bins
B=$(mktemp -d)
export TASKSLASH_SOCKET=$B/d.sock TASKSLASH_DB=$B/d.db TASKSLASH_LOGS=$B/logs
target/release/taskslash daemon &   # stop it with kill %1 when done
target/release/floor-bench --taskslash target/release/taskslash
```

Example output (100 runs each, on an idle desktop):

```
machine: load 1.65 cpu pressure 0.00%
bare       min    0.341  median    0.398  p95    0.476  p99    0.532  max    0.535  ms (100 runs)
floor      min    0.854  median    0.943  p95    1.007  p99    1.041  max    1.097  ms (100 runs)
taskslash  min    6.491  median    6.961  p95    7.928  p99    8.653  max   18.897  ms (100 runs)
  connect  min    0.010  median    0.011  p95    0.015  p99    0.017  max    0.020  ms
  admit    min    0.139  median    0.169  p95    0.208  p99    0.259  max    0.265  ms
  launch   min    1.537  median    1.712  p95    2.037  p99    2.256  max    2.370  ms
  command  min    3.538  median    3.888  p95    4.342  p99    4.421  max   15.387  ms
  drain    min    0.001  median    0.015  p95    0.026  p99    0.028  max    0.044  ms
  report   min    0.186  median    0.226  p95    0.304  p99    0.332  max    0.343  ms
  other    min    0.837  median    0.964  p95    1.123  p99    1.201  max    1.314  ms
{"runs":100,"floor_version":"0.1.0","taskslash_version":"taskslash 0.1.0","commit":"c473414","load_1m":1.65,"cpu_pressure_some_avg10":0.00,"busy":false,"rows":[...]}
```

## Reading it

- `bare` runs `true` directly: what starting a process costs on this machine.
- `floor` runs `taskslash-floor -- true`: that cost plus a bare Rust pass-through (one extra exec,
  two pipes and two copy threads).
- `taskslash` runs `taskslash run --name overhead-probe --cpu 0.1 --mem 10 -- true` against the
  running daemon. When `taskslash status --json` does not exit 0 the row reads
  `skipped: daemon not answering` and the run still succeeds.

`taskslash` minus `floor` is TaskSlash's own overhead: everything it does beyond passing the command
through. The command's output goes to null, so `taskslash run` takes its pipe path, as the floor
does.

The `taskslash` row runs with `TASKSLASH_PHASES=1`, and each run's phases are summed over the runs
like the rows are:

- `connect`: opening the daemon's socket.
- `admit`: from `submit` to the daemon's `start`. A job held by a pressure gate waits here.
- `launch`: from `start` to the command spawned (the log opened, the executable checked).
- `command`: from the spawn to the command's exit. On a machine with a user session this includes
  `systemd-run` setting up the job's scope, most of the start. Started back to back the scope takes
  3 to 4 ms; after a gap of 50 ms or more, or on a machine that was just busy, systemd and the CPU
  wake cold and it takes 12 to 14 ms. That spread is systemd's, not TaskSlash's.
- `drain`: from the exit to the last of the command's output copied.
- `report`: from there to the daemon's `recorded`: `exited` sent and the run's row written. The
  database is in WAL mode with `synchronous = NORMAL`, so the write waits for no fsync but a
  checkpoint's (under 0.5 ms; in SQLite's default journal it was 8 to 11 ms on btrfs).
- `other`: the rest of the start to exit time, the client process starting and exiting.

The machine line and the JSON carry the 1-minute load average and CPU pressure (`some avg10`) read
before the first start. Past a load of 2 or a CPU pressure of 5% the run is marked busy
(`BUSY` on the machine line, `"busy":true` in the JSON): its numbers read high, so do not compare it
with a run on an idle machine.

The last line is JSON with every row and phase, both versions, the repo commit and the machine's
state. Record each run as a new row in the project's benchmark log, with its machine state; never
replace an earlier row.
