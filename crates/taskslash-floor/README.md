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
export TASKSLASH_SOCKET=$TMPDIR/bench/d.sock TASKSLASH_DB=$TMPDIR/bench/d.db TASKSLASH_LOGS=$TMPDIR/bench/logs
target/release/taskslash daemon &   # stop it with kill %1 when done
target/release/floor-bench --taskslash target/release/taskslash
```

Example output (100 runs each, the copy-loop fix, on a busy desktop):

```
machine: load 9.09 cpu pressure 27.24%  BUSY: the numbers read high, do not compare them
bare       min    0.355  median    0.414  p95    0.499  p99    0.568  max    0.676  ms (100 runs)
floor      min    0.890  median    0.980  p95    1.122  p99    1.142  max    1.157  ms (100 runs)
taskslash  min   15.568  median   25.977  p95   36.941  p99   38.200  max   42.879  ms (100 runs)
  connect  min    0.010  median    0.014  p95    0.016  p99    0.020  max    0.023  ms
  admit    min    0.223  median    0.277  p95    0.329  p99    0.347  max    0.430  ms
  launch   min    1.581  median    1.843  p95    2.150  p99    2.225  max    2.283  ms
  command  min    3.858  median   12.582  p95   18.736  p99   19.776  max   20.718  ms
  drain    min    0.001  median    0.016  p95    0.032  p99    0.040  max    0.044  ms
  report   min    8.076  median    8.810  p95   17.475  p99   21.159  max   22.978  ms
  other    min    0.977  median    1.131  p95    1.250  p99    1.439  max    1.696  ms
{"runs":100,"floor_version":"0.1.0","taskslash_version":"taskslash 0.1.0","commit":"c238b06","load_1m":9.09,"cpu_pressure_some_avg10":27.24,"busy":true,"rows":[...]}
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
- `report`: from `exited` sent to the daemon's `recorded`, the run's row written.
- `other`: the rest of the start to exit time, the client process starting and exiting.

The machine line and the JSON carry the 1-minute load average and CPU pressure (`some avg10`) read
before the first start. Past a load of 2 or a CPU pressure of 5% the run is marked busy
(`BUSY` on the machine line, `"busy":true` in the JSON): its numbers read high, so do not compare it
with a run on an idle machine.

The last line is JSON with every row and phase, both versions, the repo commit and the machine's
state. Record each run as a new row in the project's benchmark log, with its machine state; never
replace an earlier row.
