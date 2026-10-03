# Build observability A: step timings, traces and failing-command reports

Sub-project A of issue #10 (Builds observability). B is network observability: DNS/connection logs, plain-HTTP detail and opt-in HTTPS interception, after a spike on libkrun TSI. C is OpenTelemetry export of this spec's trace model, behind a cargo feature. Both are later specs.

## Goal

Make a failed or slow build explainable without rebuilding:

1. **Which command failed.** A failing shell-form `RUN` names the command inside a long `&&` chain that was running when the step failed.
2. **Where the time went.** Every step prints its duration. On request, sandcastle breaks each step into phases (VM boot, unpack, the command, commit, ingest) and writes a timeline that opens in a trace viewer.

Success criteria:

- `RUN true && false && true` fails with an error naming `false` as the last command started.
- Every `FROM`, `RUN` and `COPY` step prints `[n/N] done in <seconds>` when it finishes.
- `--timings` and `--trace` attribute a step's time to host phases, guest phases, and the libkrun gap between them. That is enough to choose the default vCPU count and to locate the fixed ~300 ms of per-step overhead seen on Linux (benchmark, 2026-10-03).

## Decisions

| Topic | Decision |
|---|---|
| Failing-command detection | `sh -x` with a random `PS4` marker, always on for shell-form `RUN`; `--no-trace-run` turns it off |
| Trace file format | Chrome Trace Event JSON (`--trace FILE`), viewable in ui.perfetto.dev or `chrome://tracing` |
| Default output | Every `FROM`/`RUN`/`COPY` step prints its duration when it finishes |
| Phase breakdown | `--timings`: per-step phases plus a summary table |
| Guest → host transport | Append-only `/out/events.jsonl`, read after the VM exits |
| Trace model | Sandcastle's own `trace` module; no `tracing` crate dependency |

## Architecture

### Trace model (host, `src/trace.rs`)

`Trace` is a list of spans. Each span has:

- a name;
- a start offset from the build start;
- a duration;
- a parent span;
- a small set of string arguments.

The host times its own work with `std::time::Instant`. Guest phases arrive after the VM exits, with past timestamps, so they are added as finished spans. That is why the model is sandcastle's own rather than the `tracing` crate, which records spans live.

`Trace::to_chrome_json()` writes an array of complete events: `{"name","ph":"X","ts","dur","pid":1,"tid","args"}`, with `ts` and `dur` in microseconds. Nesting comes from the time ranges, with one `tid` for the whole build. Sub-project C will export the same spans to OpenTelemetry, which accepts explicit start and end times.

### Spans

**Host**

- `build`, containing:
  - `parse`
  - `pull`
  - `store open`
  - one `step n/N <instruction>` per step, containing:
    - `vm`: from spawning `__vm` to its exit
    - `ingest`: gzip, hash and verify the guest layer
  - `write layout`

**Guest** (inside `vm`, from `/out/events.jsonl`):

- `kernel boot`
- `store mount`
- `unpack <diff_id short>`, once per layer that was missing from the store
- `overlay`
- `command` (RUN) or `copy` (COPY)
- `commit`
- `sync`
- `unmount`
- `cmd <text>`: one span per traced shell command, inside `command`

**Derived**

- `vmm setup/teardown`: the part of `vm` not covered by guest spans. It is libkrun and VM creation overhead, recorded as an argument and shown by `--timings`.

### Clock alignment

The guest records event times as microseconds since the helper started (`CLOCK_MONOTONIC`). It also records two anchors:

- `kernel_boot_us`: `CLOCK_BOOTTIME` when the helper starts, i.e. the time from guest kernel start to the helper starting.
- An `end` event, written just before `status.json`.

The host knows `vm.start` and `vm.end` on its own clock. It places the guest timeline so that the helper's `end` falls at `vm.end`, treating teardown after the helper exits as negligible. That fixes:

- helper start = `vm.end` − `end.at_us`;
- kernel start = helper start − `kernel_boot_us`;
- `vmm setup` = kernel start − `vm.start`, which is libkrun and KVM/HVF VM creation.

If teardown is not negligible, it is counted into `vmm setup`. The totals stay exact and only the split is approximate. The trace arguments say so.

### Events file (guest → host)

The guest helper appends JSON lines to `/out/events.jsonl` (`sandcastle_proto::EVENTS_FILE`). Each line is one complete `write` with `O_APPEND`. The file is fsynced once, before `status.json` is written, so a killed VM still leaves every event written up to that point.

The line types are in `sandcastle-proto`, as serde enums tagged by `"type"`:

```json
{"type":"boot","kernel_boot_us":91000}
{"type":"phase","name":"unpack","detail":"sha256:1a2b3c4d5e6f","start_us":1200,"dur_us":840000}
{"type":"cmd","text":"apt-get install -y foo","start_us":2100000}
{"type":"limits","cmd_events_dropped":0,"truncated":0}
{"type":"end","at_us":4180000}
```

A `cmd` span ends where the next `cmd` starts, or at the end of `command`.

### Failing-command detection (guest)

- **Shell form only.** `RunJob` gains `shell_form: bool`. The host sets it when it built `["/bin/sh", "-c", cmd]` from a shell-form `RUN`. Exec form is never traced. With `--no-trace-run`, the host sends `shell_form: false`.
- **Traced invocation.** For a shell-form job, the guest runs `/bin/sh -x -c cmd`. It sets `PS4` to `+sc-<token>> `, where `<token>` is 12 random hex characters read from `/dev/urandom` for each job.
- **Splitting stderr.** The command's stderr is a pipe read by the helper; stdout stays on the console. The helper passes stderr through byte by byte as it arrives. Only a line that starts with the marker is held back until its newline; it is then removed from the output and becomes a `cmd` event with the arrival time. A partial line that could still turn out to be the marker is buffered only until it can be decided. Progress output using `\r` passes through unchanged.
- **Failure attribution.** When the command exits non-zero, the last `cmd` event is the last command started. In `a && b && c` that is the command that failed; in a pipeline it is the last stage started. The proto, the status and the guest do not interpret it further.

### Host failure message

The host builds the error from the step's events:

```
step 5/9 RUN apt-get update && apt-get install …: exited with 100; last command started: apt-get install -y foo (12.3s into the step)
```

- With no `cmd` events (exec form, `--no-trace-run`, or a missing or unreadable events file), the message is the current one: `exited with N`.
- Exec form needs no extra detail, because the instruction text is the command.

## CLI and output

**Default**

- Each `FROM`, `RUN` and `COPY` step ends with `[n/N] done in 4.21s` on its own line.
- Metadata instructions print nothing extra.

**`--timings`**

- Under each step's closing line it prints the phases:

  ```
  kernel boot 0.09s · vmm 0.21s · unpack 0.00s · command 3.80s · commit 0.19s · ingest 0.04s
  ```

- For a shell-form `RUN`, it also prints the 5 slowest traced commands.
- When the build ends, it prints a table of all steps sorted by duration.

**`--trace FILE`**

- Writes the Chrome trace at the end of the build, including a failed build.
- If writing fails, the build result is unchanged and a warning is printed.

**`--no-trace-run`**

- Runs shell-form `RUN` without `-x`.

## Error handling and limits

The guest is untrusted; events are data, never instructions.

- **Reading the file.** The host reads `events.jsonl` with `vm::open_guest_file`, which refuses links and special files, and caps the read at 8 MiB. A missing file means no guest spans.
- **Malformed lines** are skipped, with one warning per step that gives the count.
- **Caps, enforced by the guest and checked again by the host:**
  - at most 10,000 `cmd` events per step; more are counted in the `limits` event, not kept;
  - `cmd` text truncated to 1 KiB.
- **Printing guest text.** Before any command text is printed to the terminal, control characters are escaped: ESC becomes `\x1b`, and so on for the rest. Guest text cannot drive the terminal.
- **Unreadable events file.** The step's success or failure comes from `status.json` alone.

### Known limits (documented)

- `PS4` is visible in the step's environment.
- A script that runs `set +x` stops tracing from that point; the error then names the last traced command.
- A script started as its own shell (`sh build.sh`) is traced as one command.
- stderr now passes through the helper, so the ordering between stdout and stderr lines can shift slightly.

## Testing

**Unit tests (`cargo test`, macOS and Linux)**

- **Trace model:**
  - span offsets and nesting;
  - the `vmm setup/teardown` gap;
  - Chrome JSON. Tests parse the written JSON; they don't read internal types.
- **Marker filter** (guest, pure, no Linux APIs):
  - marker lines removed;
  - `\r` and partial lines passed through;
  - a marker split across two reads;
  - user output that looks like a marker but has another token passes through.
- **Events:**
  - parse, skip malformed lines, size and count caps, truncation;
  - escaping of control characters for display.
- **Failure message:**
  - shell form with a `cmd` event;
  - exec form;
  - missing events.

**VM tests (`just it`)**

- `true && false && true` fails, and the error contains `last command started: false`.
- A command that prints a forged marker line (wrong token) does not change the reported command.
- `set +x` partway through a chain reports the last command traced before it.
- A command writing `\r` progress to stderr reaches the host with the trace lines removed.
- `--trace` after a successful build and after a failed build produces valid JSON that contains guest phases.

**End to end (`tests/build.rs`)**

- `--timings` names the phases.
- A failing chain names its failing command in the CLI error.

## Out of scope

- Network observability (sub-project B).
- OpenTelemetry export (sub-project C).
- Live progress over vsock.
- `--debug-on-failure` shells.
- Counting a command's position in a chain, which would require parsing shell syntax.
- Tracing scripts that run as their own shell.
