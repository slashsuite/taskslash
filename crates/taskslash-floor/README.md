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

Example output (50 runs each, on a desktop with other builds running, so the load marks it busy):

```
machine: load 3.25 pressure cpu 0.80% memory 0.00% io some 5.63% io full 4.52%  BUSY: the numbers read high, do not compare them
bare       min    0.377  median    0.428  p95    0.508  p99    0.812  max    0.812  ms (50 runs)
floor      min    0.974  median    1.158  p95    1.491  p99    3.227  max    3.227  ms (50 runs)
taskslash  min    9.013  median   16.656  p95   20.582  p99   21.809  max   21.809  ms (50 runs)
  connect  min    0.017  median    0.021  p95    0.072  p99    0.074  max    0.074  ms
  admit    min    0.212  median    0.259  p95    0.372  p99    1.011  max    1.011  ms
  launch   min    2.147  median    2.607  p95    3.600  p99    3.783  max    3.783  ms
  command  min    4.609  median   11.621  p95   15.181  p99   15.695  max   15.695  ms
  drain    min    0.001  median    0.024  p95    0.043  p99    1.380  max    1.380  ms
  report   min    0.297  median    0.388  p95    0.768  p99    1.335  max    1.335  ms
  other    min    1.205  median    1.416  p95    1.726  p99    1.965  max    1.965  ms
{"runs":50,"floor_version":"0.1.0","taskslash_version":"taskslash 0.1.0","commit":"d7e0ed8","load_1m":3.25,"cpu_pressure_some_avg10":0.80,"memory_pressure_some_avg10":0.00,"io_pressure_some_avg10":5.63,"io_pressure_full_avg10":4.52,"held_runs":0,"held_reason":null,"busy":true,"rows":[...]}
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

The machine line and the JSON carry the 1-minute load average and the pressure `avg10` figures (CPU
`some`, memory `some`, I/O `some` and `full`) read before the first start. Past a load of 2 or a
CPU pressure of 5% the run is marked busy (`BUSY` on the machine line, `"busy":true` in the JSON):
its numbers read high, so do not compare it with a run on an idle machine.

A machine can have a low load and still hold every job: I/O stalled past the daemon's `full` gate
(after a boot, or while something writes heavily) stops admission for all of them. A `taskslash`
run whose job the daemon held is known by the `taskslash: job N waiting ...` line it writes, and the
bench prints a `held:` line under the machine line with how many runs were held, the first one's
admit time and the daemon's reason; the JSON has `held_runs` and `held_reason`. Any held run marks
the run busy, since its admit is the gate's wait, not TaskSlash's. On a private daemon, with no
managed job running, a held job starts once it has waited 3 minutes (the wait cap), so a held run
shows an `admit` of about 180 s:

```
held: 1 of 1 runs waited in admit, the first 180.1 s: io full pressure 12% over the 5% gate
```

and the daemon's log has `job 1 waiting (1st): io full pressure 12% over the 5% gate` and then
`job 1 started (normal) past its 3 min wait cap`.

The last line is JSON with every row and phase, both versions, the repo commit and the machine's
state. Record each run as a new row in the project's benchmark log, with its machine state; never
replace an earlier row.
