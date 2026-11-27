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
machine: load 1.92 cpu pressure 0.09%
bare       min    0.358  median    0.449  p95    0.532  p99    0.685  max    1.008  ms (100 runs)
floor      min    0.891  median    1.013  p95    1.262  p99    1.281  max    1.345  ms (100 runs)
taskslash  min   14.150  median   26.519  p95   34.964  p99   37.893  max   38.668  ms (100 runs)
  connect  min    0.009  median    0.012  p95    0.016  p99    0.018  max    0.019  ms
  admit    min    0.273  median    0.345  p95    0.431  p99    0.541  max    0.544  ms
  launch   min    1.660  median    1.860  p95    2.258  p99    2.394  max    2.733  ms
  command  min    3.854  median   12.541  p95   20.363  p99   22.489  max   22.903  ms
  drain    min    0.001  median    0.016  p95    0.030  p99    0.041  max    0.067  ms
  report   min    7.156  median   10.505  p95   14.524  p99   17.007  max   17.429  ms
  other    min    0.959  median    1.138  p95    1.266  p99    1.287  max    1.310  ms
{"runs":100,"floor_version":"0.1.0","taskslash_version":"taskslash 0.1.0","commit":"7b8fe80","load_1m":1.92,"cpu_pressure_some_avg10":0.09,"busy":false,"rows":[...]}
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
  `systemd-run` setting up the job's scope, most of the start.
- `drain`: from the exit to the last of the command's output copied.
- `report`: from there to the daemon's `recorded`: `exited` sent and the run's row written.
- `other`: the rest of the start to exit time, the client process starting and exiting.

The machine line and the JSON carry the 1-minute load average and CPU pressure (`some avg10`) read
before the first start. Past a load of 2 or a CPU pressure of 5% the run is marked busy
(`BUSY` on the machine line, `"busy":true` in the JSON): its numbers read high, so do not compare it
with a run on an idle machine.

The last line is JSON with every row and phase, both versions, the repo commit and the machine's
state. Record each run as a new row in the project's benchmark log, with its machine state; never
replace an earlier row.
