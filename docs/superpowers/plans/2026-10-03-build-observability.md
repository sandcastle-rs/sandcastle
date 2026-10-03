# Build Observability A Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Failed shell-form RUNs name the command that failed, every step prints its duration, and `--timings` / `--trace` show where each step's time goes.

**Architecture:**
- **Guest:** the helper records phases and traced shell commands as JSON lines in `/out/events.jsonl`.
  - Shell-form RUN runs as `sh -x` with a random `PS4` marker.
  - A pure filter removes the marker lines from stderr and turns them into `cmd` events.
- **Host:** after each VM exits, the host parses the events (guarded and capped), aligns them inside the step's `vm` span, and adds them to its own trace model.
  - The trace model prints durations and `--timings`, writes Chrome Trace JSON for `--trace`, and builds the failure message.

**Tech Stack:** Rust 2024 (MSRV 1.89), serde/serde_json, rustix (guest: `time` feature for `CLOCK_BOOTTIME`), std only for the pure parts.

**Spec:** `docs/superpowers/specs/2026-10-03-build-observability-design.md`

## Global Constraints

- Edition 2024, `rust-version = "1.89"`.
- **`unsafe`:** host `unsafe` only in `src/vm/krun.rs`; guest `unsafe` only the two existing `unshare_unsafe` calls in `run.rs`. No new `unsafe`.
- **Guest build:** the guest must cross-build for `<arch>-unknown-linux-musl` with pure-Rust dependencies only.
- **Guest unit tests:** guest pure modules (`trace_filter.rs`, `events.rs`) use std only and are unit-tested on macOS.
- **Verification:**
  - `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass.
  - The guest's Linux code is clippy-clean with `cargo clippy -p sandcastle-guest --target aarch64-unknown-linux-musl -- -D warnings`.
  - VM tests are `#[ignore]` and run with `just it`.
  - Never run the README conformance suite.
- **Limits, verbatim from the spec:**
  - the events file is read with at most 8 MiB;
  - at most 10,000 `cmd` events per step;
  - `cmd` text is truncated to 1 KiB;
  - control characters are escaped before guest text is printed.
- **Guest text is untrusted data.** Events never decide a step's success or failure; `status.json` alone does.
- **Output formats, verbatim from the spec:**
  - `[n/N] done in 4.21s`
  - `exited with 100; last command started: <cmd> (12.3s into the step)`
  - `--timings` phase line: `kernel boot 0.09s · vmm 0.21s · unpack 0.00s · command 3.80s · commit 0.19s · ingest 0.04s`
- **Commit messages** end with:
  ```
  Co-Authored-By: <model> <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_017321bxeGVNoZLQXWRDroYq
  ```

## Review Focus

1. **Endless stderr with no newline**, such as binary garbage or a progress bar without `\n`. The filter must forward it with bounded buffering, holding back at most a marker's length. Test: Task 2, `long_line_without_newline_is_forwarded_with_bounded_buffer`.
2. **Nested-shell tracing**, where bash repeats the first `PS4` character (`++sc-…`). The filter must still recognise and remove these lines. Test: Task 2, `bash_nesting_plus_signs_are_accepted`.
3. **A missing or garbage events file**, for example a probe job, a crashed guest or a hostile guest. The step outcome is unchanged and there are no guest spans. Test: Task 4, `parse_skips_garbage_and_caps`, plus `read_missing_file_is_none`.
4. **A background process that keeps the stderr pipe open after the shell exits.** The helper must still finish, because the PID namespace kills it. Test: the Task 3 run of the existing `run_kills_leftover_processes` VM test with tracing on.
5. **Terminal escape sequences inside a failing command's text.** They must be escaped in the error. Test: Task 4, `display_safe_escapes_controls`, plus the Task 6 end-to-end check.

---

## File Structure

```
crates/sandcastle-proto/src/lib.rs        Event enum, EVENTS_FILE, caps, RunJob.shell_form
crates/sandcastle-guest/src/trace_filter.rs  (pure) PS4-marker stderr filter → cmd texts
crates/sandcastle-guest/src/events.rs        (std) Recorder: appends events.jsonl, phases, caps, end+fsync
crates/sandcastle-guest/src/linux.rs         create Recorder, boot event, phases around store/copy/commit
crates/sandcastle-guest/src/store.rs         ensure_layers records unpack + sync phases
crates/sandcastle-guest/src/run.rs           sh -x + PS4, stderr pipe through MarkerFilter, command phase
src/trace.rs                                 (pure) Trace/Span model, Chrome JSON
src/build/events.rs                          (pure + guarded read) parse events, align into spans, last cmd, display_safe
src/build/config.rs                          Action::Run { argv, shell_form }
src/build/mod.rs                             timing, done lines, --timings, --trace, failure message
src/vm/mod.rs                                Finished { started, ended }
src/main.rs                                  --timings, --trace FILE, --no-trace-run
tests/vm.rs, tests/build.rs                  VM and end-to-end tests
```

---

### Task 1: Event wire types

**Files:**
- Modify: `crates/sandcastle-proto/src/lib.rs`

**Interfaces:**
- **Produces** (in `sandcastle_proto`):
  - `EVENTS_FILE = "events.jsonl"`
  - `MAX_CMD_EVENTS: usize = 10_000`
  - `MAX_CMD_TEXT: usize = 1024`
  - `enum Event`
  - `RunJob.shell_form: bool`, which defaults to `false` when absent.

- [ ] **Step 1: Failing tests** (append to the proto `tests` module)

```rust
    #[test]
    fn event_wire_format() {
        let lines = [
            r#"{"type":"boot","kernel_boot_us":91000}"#,
            r#"{"type":"phase","name":"unpack","detail":"sha256:1a2b3c4d5e6f","start_us":1200,"dur_us":840000}"#,
            r#"{"type":"cmd","text":"apt-get install -y foo","start_us":2100000}"#,
            r#"{"type":"limits","cmd_events_dropped":3,"truncated":1}"#,
            r#"{"type":"end","at_us":4180000}"#,
        ];
        let events: Vec<Event> = lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(events[0], Event::Boot { kernel_boot_us: 91000 });
        assert_eq!(
            events[1],
            Event::Phase {
                name: "unpack".into(),
                detail: Some("sha256:1a2b3c4d5e6f".into()),
                start_us: 1200,
                dur_us: 840000
            }
        );
        assert_eq!(events[4], Event::End { at_us: 4180000 });
        // Round trip keeps the exact wire shape.
        assert_eq!(serde_json::to_string(&events[2]).unwrap(), lines[2]);
        let no_detail = Event::Phase { name: "commit".into(), detail: None, start_us: 1, dur_us: 2 };
        assert_eq!(
            serde_json::to_string(&no_detail).unwrap(),
            r#"{"type":"phase","name":"commit","start_us":1,"dur_us":2}"#
        );
    }

    #[test]
    fn run_job_shell_form_defaults_to_false() {
        let job: Job = serde_json::from_str(
            r#"{"mode":"run","lower":[],"argv":["true"],"env":[],"user":"","workdir":"/","resolv_conf":""}"#,
        )
        .unwrap();
        let Job::Run(run) = job else { panic!() };
        assert!(!run.shell_form);
    }
```

- [ ] **Step 2: Run them to see them fail.** `cargo test -p sandcastle-proto`. Expected: compile errors.

- [ ] **Step 3: Implement.** Add the following to `crates/sandcastle-proto/src/lib.rs`:

```rust
/// Append-only JSON lines the guest writes to `/out` for observability.
pub const EVENTS_FILE: &str = "events.jsonl";
/// Most traced shell commands kept per step; the rest are counted only.
pub const MAX_CMD_EVENTS: usize = 10_000;
/// Longest traced command text kept, in bytes.
pub const MAX_CMD_TEXT: usize = 1024;

/// One line of `events.jsonl`. Times are microseconds since the guest
/// helper started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// `CLOCK_BOOTTIME` when the helper started: guest kernel boot time.
    Boot { kernel_boot_us: u64 },
    Phase {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        start_us: u64,
        dur_us: u64,
    },
    /// A shell command the traced `sh -x` started.
    Cmd { text: String, start_us: u64 },
    Limits { cmd_events_dropped: u64, truncated: u64 },
    /// Written just before `status.json`; anchors the timeline on the host.
    End { at_us: u64 },
}
```

Add a field to `RunJob`, after `resolv_conf`:

```rust
    /// `argv` is `["/bin/sh", "-c", cmd]` from a shell-form RUN; the guest
    /// traces it with `sh -x` to report the last command started.
    #[serde(default)]
    pub shell_form: bool,
```

Update every `RunJob { … }` literal in the workspace (`src/build/mod.rs` and `tests/vm.rs` `run_job`) with `shell_form: false`. Task 5 sets it properly.

- [ ] **Step 4: Run the checks.** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`. Expected: PASS.

- [ ] **Step 5: Commit.** Message: `Add guest event wire types and RunJob.shell_form`, with the trailer.

---

### Task 2: Guest stderr marker filter and event recorder (pure)

**Files:**
- Create: `crates/sandcastle-guest/src/trace_filter.rs`, `crates/sandcastle-guest/src/events.rs`
- Modify: `crates/sandcastle-guest/src/main.rs`

**Interfaces:**
- **Produces:**
  - `trace_filter::MarkerFilter::new(token: &str) -> MarkerFilter`
  - `MarkerFilter::ps4(&self) -> String`, which returns `+sc-<token>> `
  - `feed(&mut self, chunk: &[u8], out: &mut Vec<u8>, cmds: &mut Vec<String>)`
  - `finish(&mut self, out: &mut Vec<u8>, cmds: &mut Vec<String>)`
  - `events::Recorder::create(path: &Path, start: Instant) -> io::Result<Recorder>`
  - `boot(&self, kernel_boot_us: u64)`
  - `phase<T>(&self, name: &str, detail: Option<&str>, f: impl FnOnce() -> T) -> T`
  - `now_us(&self) -> u64`
  - `cmd(&self, text: &str, start_us: u64)`
  - `finish(self) -> io::Result<()>`

**Filter rules:**
- **Matching a marker line.** A line is a marker line if it starts with one or more `+` followed by `sc-<token>> `. Bash repeats the first `PS4` character for nested levels, which is why more than one `+` is allowed.
- **Marker lines** are removed from the output. Their remaining text, without the trailing newline and with trailing whitespace trimmed, becomes a cmd.
- **Everything else is forwarded as it arrives.** Only a line start that is still a possible marker prefix is held back, and that is bounded by the marker length plus the run of `+`. The `+` run is capped at 64; a longer run of `+` is ordinary output.
- `\r` is ordinary data.
- **Truncation.** Cmd text longer than `MAX_CMD_TEXT` bytes is truncated at a UTF-8 character boundary, and `truncated()` counts it.

- [ ] **Step 1: Failing tests** (in `trace_filter.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn run(token: &str, chunks: &[&[u8]]) -> (Vec<u8>, Vec<String>) {
        let mut f = MarkerFilter::new(token);
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        for c in chunks {
            f.feed(c, &mut out, &mut cmds);
        }
        f.finish(&mut out, &mut cmds);
        (out, cmds)
    }

    #[test]
    fn marker_lines_become_cmds_and_vanish() {
        let (out, cmds) = run("ab12", &[b"+sc-ab12> true\nreal output\n+sc-ab12> false\n"]);
        assert_eq!(out, b"real output\n");
        assert_eq!(cmds, ["true", "false"]);
    }

    #[test]
    fn marker_split_across_reads() {
        let (out, cmds) = run("ab12", &[b"+sc-a", b"b12> apt-get", b" install x\nok\n"]);
        assert_eq!(out, b"ok\n");
        assert_eq!(cmds, ["apt-get install x"]);
    }

    #[test]
    fn wrong_token_and_mid_line_markers_pass_through() {
        let (out, cmds) = run("ab12", &[b"+sc-zz99> fake\nsay +sc-ab12> no\n"]);
        assert_eq!(out, b"+sc-zz99> fake\nsay +sc-ab12> no\n");
        assert!(cmds.is_empty());
    }

    #[test]
    fn carriage_returns_pass_through() {
        let (out, _) = run("ab12", &[b"10%\r20%\r100%\n"]);
        assert_eq!(out, b"10%\r20%\r100%\n");
    }

    #[test]
    fn bash_nesting_plus_signs_are_accepted() {
        let (out, cmds) = run("ab12", &[b"+++sc-ab12> inner\n++ not a marker\n"]);
        assert_eq!(out, b"++ not a marker\n");
        assert_eq!(cmds, ["inner"]);
    }

    #[test]
    fn long_line_without_newline_is_forwarded_with_bounded_buffer() {
        let mut f = MarkerFilter::new("ab12");
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        let chunk = vec![b'x'; 1 << 20];
        f.feed(&chunk, &mut out, &mut cmds);
        assert_eq!(out.len(), chunk.len(), "nothing held back mid-line");
        f.feed(b"+sc-", &mut out, &mut cmds);
        assert_eq!(out.len(), chunk.len() + 4, "mid-line text is never a marker");
    }

    #[test]
    fn unterminated_marker_at_eof_is_a_cmd() {
        let (out, cmds) = run("ab12", &[b"+sc-ab12> last"]);
        assert!(out.is_empty());
        assert_eq!(cmds, ["last"]);
    }

    #[test]
    fn long_cmd_text_is_truncated_on_a_char_boundary() {
        let long = format!("+sc-ab12> {}\n", "é".repeat(2000));
        let mut f = MarkerFilter::new("ab12");
        let (mut out, mut cmds) = (Vec::new(), Vec::new());
        f.feed(long.as_bytes(), &mut out, &mut cmds);
        assert!(cmds[0].len() <= sandcastle_proto::MAX_CMD_TEXT);
        assert!(cmds[0].chars().all(|c| c == 'é'));
        assert_eq!(f.truncated(), 1);
    }
}
```

Add tests to `events.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::time::Instant;

    use sandcastle_proto::{Event, MAX_CMD_EVENTS};

    use super::*;

    fn read(path: &Path) -> Vec<Event> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn records_phases_cmds_and_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let rec = Recorder::create(&path, Instant::now()).unwrap();
        rec.boot(91_000);
        let v = rec.phase("unpack", Some("sha256:1a2b"), || 7);
        assert_eq!(v, 7);
        rec.cmd("true", rec.now_us());
        rec.finish().unwrap();
        let events = read(&path);
        assert_eq!(events[0], Event::Boot { kernel_boot_us: 91_000 });
        assert!(matches!(&events[1], Event::Phase { name, detail: Some(d), .. } if name == "unpack" && d == "sha256:1a2b"));
        assert!(matches!(&events[2], Event::Cmd { text, .. } if text == "true"));
        assert!(matches!(events[3], Event::Limits { cmd_events_dropped: 0, truncated: 0 }));
        assert!(matches!(events[4], Event::End { .. }));
    }

    #[test]
    fn cmd_events_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let rec = Recorder::create(&path, Instant::now()).unwrap();
        for _ in 0..MAX_CMD_EVENTS + 5 {
            rec.cmd("x", 1);
        }
        rec.add_truncated(2);
        rec.finish().unwrap();
        let events = read(&path);
        let cmds = events.iter().filter(|e| matches!(e, Event::Cmd { .. })).count();
        assert_eq!(cmds, MAX_CMD_EVENTS);
        assert!(events.contains(&Event::Limits { cmd_events_dropped: 5, truncated: 2 }));
    }
}
```

- [ ] **Step 2: Run them to see them fail.** `cargo test -p sandcastle-guest trace_filter events`. Expected: compile errors.

- [ ] **Step 3: Implement.**

`crates/sandcastle-guest/src/trace_filter.rs`:

```rust
//! Separates `sh -x` trace lines from a RUN step's stderr. The shell runs
//! with `PS4` set to a per-job marker; lines that start with it are the
//! commands the shell is about to run. Everything else is forwarded as it
//! arrives, holding back at most a possible marker prefix.

use sandcastle_proto::MAX_CMD_TEXT;

/// Longest run of leading `+` accepted (bash repeats PS4's first character
/// once per nesting level).
const MAX_PLUS: usize = 64;

pub struct MarkerFilter {
    /// The marker after the leading `+` run: `sc-<token>> `.
    tail: Vec<u8>,
    state: State,
    /// Bytes held back while a line start may still be a marker.
    pending: Vec<u8>,
    /// Command text of the marker line being read.
    cmd: Vec<u8>,
    truncated: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    /// At the start of a line, nothing held back.
    LineStart,
    /// `pending` is a possible marker prefix.
    Deciding,
    /// Inside an ordinary line.
    Passing,
    /// Inside a marker line; bytes go to `cmd`.
    Capturing,
}

impl MarkerFilter {
    pub fn new(token: &str) -> Self {
        Self {
            tail: format!("sc-{token}> ").into_bytes(),
            state: State::LineStart,
            pending: Vec::new(),
            cmd: Vec::new(),
            truncated: 0,
        }
    }

    /// The value for the shell's `PS4`.
    pub fn ps4(&self) -> String {
        format!("+{}", String::from_utf8_lossy(&self.tail))
    }

    pub fn truncated(&self) -> u64 {
        self.truncated
    }

    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>, cmds: &mut Vec<String>) {
        for &b in chunk {
            match self.state {
                State::Passing => {
                    out.push(b);
                    if b == b'\n' {
                        self.state = State::LineStart;
                    }
                }
                State::Capturing => {
                    if b == b'\n' {
                        self.emit(cmds);
                        self.state = State::LineStart;
                    } else {
                        self.cmd.push(b);
                    }
                }
                State::LineStart | State::Deciding => {
                    self.pending.push(b);
                    match self.classify() {
                        Prefix::Complete => {
                            self.pending.clear();
                            self.state = State::Capturing;
                        }
                        Prefix::Partial => self.state = State::Deciding,
                        Prefix::No => {
                            out.append(&mut self.pending);
                            self.state = if b == b'\n' { State::LineStart } else { State::Passing };
                        }
                    }
                }
            }
        }
    }

    /// End of stream: flush held bytes; an unterminated marker line is a cmd.
    pub fn finish(&mut self, out: &mut Vec<u8>, cmds: &mut Vec<String>) {
        match self.state {
            State::Capturing => self.emit(cmds),
            State::Deciding => out.append(&mut self.pending),
            State::LineStart | State::Passing => {}
        }
        self.state = State::LineStart;
    }

    fn classify(&self) -> Prefix {
        let plus = self.pending.iter().take_while(|&&b| b == b'+').count();
        if plus == 0 || plus > MAX_PLUS {
            return Prefix::No;
        }
        let rest = &self.pending[plus..];
        if rest.len() >= self.tail.len() {
            if rest.starts_with(&self.tail) { Prefix::Complete } else { Prefix::No }
        } else if self.tail.starts_with(rest) {
            Prefix::Partial
        } else {
            Prefix::No
        }
    }

    fn emit(&mut self, cmds: &mut Vec<String>) {
        let mut text = String::from_utf8_lossy(&self.cmd).trim_end().to_string();
        if text.len() > MAX_CMD_TEXT {
            let mut cut = MAX_CMD_TEXT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            self.truncated += 1;
        }
        cmds.push(text);
        self.cmd.clear();
    }
}

enum Prefix {
    No,
    Partial,
    Complete,
}
```

`Capturing` can collect an unbounded `cmd` for a huge trace line. Cap it: in the `Capturing` branch, push only while `self.cmd.len() <= MAX_CMD_TEXT * 4`. Bytes beyond that are discarded, and `emit` then truncates as above.

`crates/sandcastle-guest/src/events.rs`:

```rust
//! Appends observability events to `/out/events.jsonl`, one JSON line per
//! write. Failing to record never fails the job: the first write error is
//! reported once and recording stops.

use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use sandcastle_proto::{Event, MAX_CMD_EVENTS};

pub struct Recorder {
    file: RefCell<Option<File>>,
    start: Instant,
    cmds: Cell<usize>,
    dropped: Cell<u64>,
    truncated: Cell<u64>,
}

impl Recorder {
    pub fn create(path: &Path, start: Instant) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: RefCell::new(Some(file)),
            start,
            cmds: Cell::new(0),
            dropped: Cell::new(0),
            truncated: Cell::new(0),
        })
    }

    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    pub fn boot(&self, kernel_boot_us: u64) {
        self.write(&Event::Boot { kernel_boot_us });
    }

    pub fn phase<T>(&self, name: &str, detail: Option<&str>, f: impl FnOnce() -> T) -> T {
        let start_us = self.now_us();
        let value = f();
        self.write(&Event::Phase {
            name: name.to_string(),
            detail: detail.map(str::to_string),
            start_us,
            dur_us: self.now_us() - start_us,
        });
        value
    }

    pub fn cmd(&self, text: &str, start_us: u64) {
        if self.cmds.get() >= MAX_CMD_EVENTS {
            self.dropped.set(self.dropped.get() + 1);
            return;
        }
        self.cmds.set(self.cmds.get() + 1);
        self.write(&Event::Cmd { text: text.to_string(), start_us });
    }

    pub fn add_truncated(&self, n: u64) {
        self.truncated.set(self.truncated.get() + n);
    }

    /// Writes the limits and end events and syncs, so the host sees every
    /// event before it sees `status.json`.
    pub fn finish(self) -> io::Result<()> {
        self.write(&Event::Limits {
            cmd_events_dropped: self.dropped.get(),
            truncated: self.truncated.get(),
        });
        self.write(&Event::End { at_us: self.now_us() });
        match self.file.into_inner() {
            Some(file) => file.sync_all(),
            None => Ok(()),
        }
    }

    fn write(&self, event: &Event) {
        let mut slot = self.file.borrow_mut();
        let Some(file) = slot.as_mut() else { return };
        let mut line = serde_json::to_vec(event).expect("events serialize");
        line.push(b'\n');
        if let Err(e) = file.write_all(&line) {
            eprintln!("sandcastle-guest: not recording further events: {e}");
            *slot = None;
        }
    }
}
```

Add both modules to `main.rs`, marked as pure, the same way as `layer`:

```rust
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod events;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod trace_filter;
```

- [ ] **Step 4: Run the checks.** `cargo test -p sandcastle-guest && cargo build -p sandcastle-guest --target aarch64-unknown-linux-musl`. Expected: PASS. The musl clippy may report dead_code until Task 3 wires these modules in; Task 3 must leave it clean.

- [ ] **Step 5: Commit.** Message: `Add guest stderr trace filter and event recorder`, with the trailer.

---

### Task 3: Guest wiring: events for every job, traced shell-form RUN

**Files:**
- Modify: `crates/sandcastle-guest/src/linux.rs`, `store.rs`, `run.rs`, `crates/sandcastle-guest/Cargo.toml` (rustix feature `time`)
- Test: `tests/vm.rs`

**Interfaces:**
- **Consumes:** Task 1 (`Event`, `EVENTS_FILE`, `RunJob.shell_form`) and Task 2 (`Recorder`, `MarkerFilter`).
- **Produces** (used by Tasks 4–6): `/out/events.jsonl` for run and copy jobs, containing:
  - `boot`
  - phases named exactly `store mount`, `unpack` (detail: diff_id truncated to `sha256:` plus 12 hex), `overlay`, `command` (RUN) or `copy` (COPY), `commit`, `sync`, `unmount`
  - `cmd` events, for shell-form RUN only
  - `limits`
  - `end`

**Changes:**

`linux.rs`:
1. **At the very top of `job_main`**, before mounting `/out`:
   - `let start = Instant::now();`
   - read the kernel boot time: `let boot = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);` and convert it to µs (`tv_sec * 1_000_000 + tv_nsec / 1_000`).
2. **After mounting `/out`:** `let rec = Recorder::create(&out.join(EVENTS_FILE), start).ok();` then `if let Some(r) = &rec { r.boot(boot_us) }`. Failing to create the file only means there are no events.
3. **Pass the recorder down.** Pass `rec.as_ref()` into `run_job`, then `with_store`, `copy_job`, `run::run_job` and `commit_upper`. Use the signature `rec: Option<&Recorder>`, and add a helper:
   ```rust
   /// Times `f` as phase `name` when recording; plain call otherwise.
   pub fn phase<T>(rec: Option<&Recorder>, name: &str, detail: Option<&str>, f: impl FnOnce() -> T) -> T {
       match rec {
           Some(r) => r.phase(name, detail, f),
           None => f(),
       }
   }
   ```
4. **`with_store`:** wrap `Store::mount()` in phase `store mount` and `store.unmount()` in phase `unmount`.
5. **`copy_job`:**
   - `Overlay::mount` goes in phase `overlay`;
   - `copy.ensure_workdir().and_then(|()| copy.run(…))` goes in phase `copy`;
   - the `commit::commit` call inside `commit_upper` goes in phase `commit`;
   - `store.sync()` inside `commit_upper` goes in phase `sync`.
6. **Finish recording.** Before `write_status`, call `if let Some(r) = rec { let _ = r.finish(); }`. The recorder is moved out here, because `finish` takes `self`, so restructure so that `rec` is owned in `job_main` and borrowed by everything below it.

`store.rs`, `ensure_layers(&self, lower, rec: Option<&Recorder>)`:
- each `unpack::unpack` call goes in phase `unpack` with detail `&l.diff_id[..19.min(len)]` (`sha256:` plus 12 hex);
- each `self.sync()` goes in phase `sync`.

`run.rs`:
- **`run_job`:**
  - `Overlay::mount` goes in phase `overlay`;
  - `execute(…)` goes in phase `command`;
  - commit and sync go through `commit_upper(…, rec)`.
- **`execute(root, job, rec)`:** when `job.shell_form` is true and `job.argv` has the form `["/bin/sh", "-c", cmd]`:
  - Create the token: 6 bytes from `/dev/urandom`, as 12 lowercase hex characters, using `std::fs::File::open("/dev/urandom")` and `read_exact`. Then `let filter = MarkerFilter::new(&token);`.
  - `spec.argv = ["/bin/sh", "-x", "-c", cmd]`.
  - `spec.env.push(format!("PS4={}", filter.ps4()))`.
  - Spawn the `--exec` child with `.stderr(Stdio::piped())`. Take its stderr, then loop `read(&mut buf[..64 KiB])`:
    - `filter.feed(&buf[..n], &mut out, &mut cmds)`;
    - `io::stderr().write_all(&out)` and clear `out`;
    - for each cmd: `rec.cmd(&text, rec.now_us())`.
  - At EOF:
    - `filter.finish(…)`, then flush and record the same way;
    - `rec.add_truncated(filter.truncated())`;
    - `child.wait()`.

  Otherwise, keep the current `.status()` path with inherited stderr. EOF arrives once every process holding the pipe has exited. The command is init of its PID namespace, so leftover background processes are killed when it exits.

- [ ] **Step 1: Failing VM tests.** Add to `tests/vm.rs`. Set the existing `run_job` helper's `shell_form` from a new parameter by adding a variant `shell_job(lower, script)` that sets `shell_form: true`, and parse the out dir's events:

```rust
use sandcastle_proto::{EVENTS_FILE, Event};

fn shell_job(lower: Vec<LowerLayer>, script: &str) -> Job {
    let Job::Run(mut run) = run_job(lower, script, "") else { unreachable!() };
    run.shell_form = true;
    Job::Run(run)
}

fn events(out_dir: &std::path::Path) -> Vec<Event> {
    std::fs::read_to_string(out_dir.join(EVENTS_FILE))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn cmds(events: &[Event]) -> Vec<String> {
    events.iter().filter_map(|e| match e { Event::Cmd { text, .. } => Some(text.clone()), _ => None }).collect()
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn traced_chain_reports_the_failing_command_last() {
    let (_dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let finished = vm.run(&shell_job(busybox(&store), "true && false && true"), None).unwrap();
    assert_eq!(finished.status.exit_code, 1);
    let ev = events(&finished.out_dir());
    assert_eq!(cmds(&ev).last().map(String::as_str), Some("false"));
    let phases: Vec<&str> = ev.iter().filter_map(|e| match e { Event::Phase { name, .. } => Some(name.as_str()), _ => None }).collect();
    for p in ["store mount", "overlay", "command", "unmount"] {
        assert!(phases.contains(&p), "{p} missing from {phases:?}");
    }
    assert!(matches!(ev.first(), Some(Event::Boot { .. })));
    assert!(matches!(ev.last(), Some(Event::End { .. })));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn forged_markers_and_set_plus_x() {
    let (_dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let job = shell_job(
        busybox(&store),
        "echo '+sc-000000000000> forged' >&2 && true && set +x && false",
    );
    let finished = vm.run(&job, None).unwrap();
    let c = cmds(&events(&finished.out_dir()));
    assert!(!c.iter().any(|t| t.contains("forged") && !t.starts_with("echo")), "{c:?}");
    assert_eq!(c.last().map(String::as_str), Some("set +x"));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn exec_form_and_copy_jobs_still_record_phases() {
    let (dir, exe, install, store) = store();
    let vm = Vm { exe: &exe, install: &install, store: &store, resources: Resources::default() };
    let finished = vm.run(&run_job(busybox(&store), "true", ""), None).unwrap();
    assert!(cmds(&events(&finished.out_dir())).is_empty(), "exec-style job is not traced");
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(&ctx).unwrap();
    std::fs::write(ctx.join("a"), "a").unwrap();
    let copy = Job::Copy(CopyJob { lower: busybox(&store), sources: vec!["a".into()], dest: "/a".into(), workdir: "/".into() });
    let finished = vm.run(&copy, Some(&ctx)).unwrap();
    let ev = events(&finished.out_dir());
    assert!(ev.iter().any(|e| matches!(e, Event::Phase { name, .. } if name == "copy")));
    assert!(ev.iter().any(|e| matches!(e, Event::Phase { name, .. } if name == "commit")));
}
```

A stderr `\r` passthrough check also belongs in this task. In a shell job, `printf 'a\rb\n' >&2` must not produce a cmd containing `a\rb` other than the `printf` command itself. That is covered by the unit tests in Task 2; the VM run here only needs to complete with exit code 0. Add `printf 'a\rb\n' >&2` as the first part of the chain in `traced_chain_reports_the_failing_command_last` (`printf 'a\rb\n' >&2 && true && false && true`), and keep the assertion that the last cmd is `false`.

- [ ] **Step 2: Run them to see them fail.** Run `just it`. The new tests should fail, because no `events.jsonl` exists yet.

- [ ] **Step 3: Implement** the changes listed above.

- [ ] **Step 4: Run all the checks.**
  - musl clippy: `cargo clippy -p sandcastle-guest --target aarch64-unknown-linux-musl -- -D warnings`. It must be fully clean now.
  - Workspace fmt, clippy and test.
  - `just it`. All VM tests pass, old and new, including `run_kills_leftover_processes` and `run_job_exit_codes_and_no_op_steps`.

- [ ] **Step 5: Commit.** Message: `Record guest phases and trace shell-form RUN commands`, with the trailer.

---

### Task 4: Host trace model and guest event parsing (pure)

**Files:**
- Create: `src/trace.rs`, `src/build/events.rs`
- Modify: `src/lib.rs` (`pub mod trace;`), `src/build/mod.rs` (`pub mod events;`)

**Interfaces:**
- **Produces from `trace`:**
  - `pub struct Span { pub name: String, pub start: Duration, pub dur: Duration, pub args: Vec<(String, String)> }`. `start` is measured from the build start.
  - `pub struct Trace`, with:
    - `new() -> Trace`, which records the origin `Instant`
    - `origin(&self) -> Instant`
    - `at(&self, t: Instant) -> Duration`
    - `push(&mut self, span: Span)`
    - `spans(&self) -> &[Span]`
    - `to_chrome_json(&self) -> Vec<u8>`
- **Produces from `build::events`:**
  - `pub struct GuestEvents { pub kernel_boot_us: Option<u64>, pub phases: Vec<GuestPhase>, pub cmds: Vec<GuestCmd>, pub end_us: Option<u64>, pub dropped: u64, pub truncated: u64, pub malformed: usize }`
  - `pub struct GuestPhase { pub name: String, pub detail: Option<String>, pub start_us: u64, pub dur_us: u64 }`
  - `pub struct GuestCmd { pub text: String, pub start_us: u64 }`
  - `pub fn parse(bytes: &[u8]) -> GuestEvents`
  - `pub fn read(path: &Path) -> anyhow::Result<Option<GuestEvents>>`, which uses `vm::open_guest_file` and takes at most 8 MiB.
  - `pub fn guest_spans(ev: &GuestEvents, vm_start: Duration, vm_end: Duration) -> Vec<Span>`
  - `pub fn helper_start(ev: &GuestEvents, vm_end: Duration) -> Option<Duration>`
  - `pub fn display_safe(text: &str) -> String`

**`guest_spans` rules** (spec "Clock alignment"):
- Helper start `h = vm_end - end_us`. Without an `end` event there are no guest spans; return an empty vec.
- `kernel boot` span: `[h - kernel_boot_us, h)`, clamped at `vm_start`.
- `vmm setup` span: `[vm_start, kernel start)`, when the gap is positive.
- Each phase becomes a span at `h + start_us`, named by the phase name, with `detail` as the arg `detail`.
- Each cmd becomes a span named `cmd: <display_safe(text)>`. It ends at the next cmd's start or at the end of the `command` phase, whichever comes first, and otherwise at `end_us`.

- [ ] **Step 1: Failing tests.**

In `trace.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn chrome_json_has_complete_events_in_microseconds() {
        let mut t = Trace::new();
        t.push(Span { name: "step 2/3 RUN x".into(), start: Duration::from_millis(5), dur: Duration::from_millis(1500), args: vec![("exit".into(), "0".into())] });
        let v: serde_json::Value = serde_json::from_slice(&t.to_chrome_json()).unwrap();
        let e = &v.as_array().unwrap()[0];
        assert_eq!(e["ph"], "X");
        assert_eq!(e["name"], "step 2/3 RUN x");
        assert_eq!(e["ts"], 5000);
        assert_eq!(e["dur"], 1_500_000);
        assert_eq!(e["args"]["exit"], "0");
        assert_eq!(e["pid"], 1);
    }
}
```

In `build/events.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const GOOD: &str = r#"{"type":"boot","kernel_boot_us":90000}
{"type":"phase","name":"overlay","start_us":1000,"dur_us":2000}
{"type":"phase","name":"command","start_us":3000,"dur_us":7000}
{"type":"cmd","text":"true","start_us":4000}
{"type":"cmd","text":"false","start_us":6000}
{"type":"limits","cmd_events_dropped":0,"truncated":0}
{"type":"end","at_us":12000}
"#;

    #[test]
    fn parse_skips_garbage_and_caps() {
        let mut input = format!("not json\n{GOOD}{{\"type\":\"bogus\"}}\n");
        for _ in 0..sandcastle_proto::MAX_CMD_EVENTS + 3 {
            input.push_str(r#"{"type":"cmd","text":"x","start_us":1}"#);
            input.push('\n');
        }
        let ev = parse(input.as_bytes());
        assert_eq!(ev.malformed, 2);
        assert_eq!(ev.kernel_boot_us, Some(90_000));
        assert_eq!(ev.cmds.len(), sandcastle_proto::MAX_CMD_EVENTS);
        assert!(ev.dropped >= 3);
        assert_eq!(ev.end_us, Some(12_000));
    }

    #[test]
    fn read_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("events.jsonl")).unwrap().is_none());
    }

    #[test]
    fn guest_spans_align_to_vm_end() {
        let ev = parse(GOOD.as_bytes());
        // VM ran from 100ms to 300ms on the host clock.
        let spans = guest_spans(&ev, Duration::from_millis(100), Duration::from_millis(300));
        let find = |n: &str| spans.iter().find(|s| s.name == n).unwrap_or_else(|| panic!("{n}: {spans:?}"));
        // helper start = 300ms - 12ms = 288ms; kernel boot = [198ms, 288ms).
        assert_eq!(find("kernel boot").start, Duration::from_millis(198));
        assert_eq!(find("vmm setup").start, Duration::from_millis(100));
        assert_eq!(find("vmm setup").dur, Duration::from_millis(98));
        assert_eq!(find("command").start, Duration::from_millis(291));
        // "true" runs until "false" starts; "false" until command ends.
        assert_eq!(find("cmd: true").dur, Duration::from_millis(2));
        assert_eq!(find("cmd: false").dur, Duration::from_millis(4));
        assert_eq!(helper_start(&ev, Duration::from_millis(300)), Some(Duration::from_millis(288)));
    }

    #[test]
    fn no_end_event_gives_no_guest_spans() {
        let ev = parse(br#"{"type":"cmd","text":"x","start_us":1}"#);
        assert!(guest_spans(&ev, Duration::ZERO, Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn display_safe_escapes_controls() {
        assert_eq!(display_safe("rm \u{1b}[2Jx\tok"), "rm \\x1b[2Jx\\x09ok");
        assert_eq!(display_safe("café"), "café");
    }
}
```

- [ ] **Step 2: Run them to see them fail.** `cargo test -p sandcastle --lib trace build::events`. Expected: compile errors.

- [ ] **Step 3: Implement.**

`src/trace.rs`:

```rust
//! A build's timeline: finished spans with offsets from the build start,
//! written as Chrome Trace Event JSON (ui.perfetto.dev, chrome://tracing).
//! Guest phases arrive after the fact, so spans are recorded complete.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub name: String,
    pub start: Duration,
    pub dur: Duration,
    pub args: Vec<(String, String)>,
}

pub struct Trace {
    origin: Instant,
    spans: Vec<Span>,
}

impl Trace {
    pub fn new() -> Self {
        Self { origin: Instant::now(), spans: Vec::new() }
    }

    pub fn origin(&self) -> Instant {
        self.origin
    }

    pub fn at(&self, t: Instant) -> Duration {
        t.saturating_duration_since(self.origin)
    }

    pub fn push(&mut self, span: Span) {
        self.spans.push(span);
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn to_chrome_json(&self) -> Vec<u8> {
        let events: Vec<serde_json::Value> = self
            .spans
            .iter()
            .map(|s| {
                let args: serde_json::Map<String, serde_json::Value> =
                    s.args.iter().map(|(k, v)| (k.clone(), v.clone().into())).collect();
                serde_json::json!({
                    "name": s.name,
                    "ph": "X",
                    "ts": s.start.as_micros() as u64,
                    "dur": s.dur.as_micros() as u64,
                    "pid": 1,
                    "tid": 1,
                    "args": args,
                })
            })
            .collect();
        serde_json::to_vec(&events).expect("trace serializes")
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}
```

`src/build/events.rs`:

```rust
//! The guest's `events.jsonl`: parsed defensively (the guest is untrusted),
//! aligned onto the host's timeline, and made safe to print.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use sandcastle_proto::{Event, MAX_CMD_EVENTS, MAX_CMD_TEXT};

use crate::trace::Span;
use crate::vm::open_guest_file;

/// Largest events file read from the guest.
const MAX_EVENTS_BYTES: u64 = 8 << 20;

#[derive(Debug, Default)]
pub struct GuestEvents {
    pub kernel_boot_us: Option<u64>,
    pub phases: Vec<GuestPhase>,
    pub cmds: Vec<GuestCmd>,
    pub end_us: Option<u64>,
    pub dropped: u64,
    pub truncated: u64,
    pub malformed: usize,
}

#[derive(Debug, Clone)]
pub struct GuestPhase {
    pub name: String,
    pub detail: Option<String>,
    pub start_us: u64,
    pub dur_us: u64,
}

#[derive(Debug, Clone)]
pub struct GuestCmd {
    pub text: String,
    pub start_us: u64,
}

pub fn read(path: &Path) -> Result<Option<GuestEvents>> {
    let Some(file) = open_guest_file(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_EVENTS_BYTES).read_to_end(&mut bytes)?;
    Ok(Some(parse(&bytes)))
}

pub fn parse(bytes: &[u8]) -> GuestEvents {
    let mut ev = GuestEvents::default();
    for line in bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        match serde_json::from_slice::<Event>(line) {
            Ok(Event::Boot { kernel_boot_us }) => ev.kernel_boot_us = Some(kernel_boot_us),
            Ok(Event::Phase { name, detail, start_us, dur_us }) => {
                ev.phases.push(GuestPhase { name: clip(name), detail: detail.map(clip), start_us, dur_us })
            }
            Ok(Event::Cmd { text, start_us }) => {
                if ev.cmds.len() < MAX_CMD_EVENTS {
                    ev.cmds.push(GuestCmd { text: clip(text), start_us });
                } else {
                    ev.dropped += 1;
                }
            }
            Ok(Event::Limits { cmd_events_dropped, truncated }) => {
                ev.dropped += cmd_events_dropped;
                ev.truncated += truncated;
            }
            Ok(Event::End { at_us }) => ev.end_us = Some(at_us),
            Err(_) => ev.malformed += 1,
        }
    }
    ev
}

/// Guest strings are capped again on the host; the guest is not trusted to.
fn clip(mut s: String) -> String {
    if s.len() > MAX_CMD_TEXT {
        let mut cut = MAX_CMD_TEXT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

pub fn helper_start(ev: &GuestEvents, vm_end: Duration) -> Option<Duration> {
    ev.end_us.map(|end| vm_end.saturating_sub(Duration::from_micros(end)))
}

pub fn guest_spans(ev: &GuestEvents, vm_start: Duration, vm_end: Duration) -> Vec<Span> {
    let Some(h) = helper_start(ev, vm_end) else {
        return Vec::new();
    };
    let us = Duration::from_micros;
    let mut spans = Vec::new();
    let kernel_start = h.saturating_sub(us(ev.kernel_boot_us.unwrap_or(0))).max(vm_start);
    if ev.kernel_boot_us.is_some() {
        spans.push(span("kernel boot", kernel_start, h - kernel_start, vec![]));
    }
    if kernel_start > vm_start {
        spans.push(span(
            "vmm setup",
            vm_start,
            kernel_start - vm_start,
            vec![("note".into(), "includes VM teardown; split is approximate".into())],
        ));
    }
    for p in &ev.phases {
        let args = p.detail.iter().map(|d| ("detail".to_string(), d.clone())).collect();
        spans.push(span(&p.name, h + us(p.start_us), us(p.dur_us), args));
    }
    let command_end = ev
        .phases
        .iter()
        .find(|p| p.name == "command")
        .map(|p| p.start_us + p.dur_us)
        .or(ev.end_us)
        .unwrap_or(0);
    for (i, c) in ev.cmds.iter().enumerate() {
        let end = ev.cmds.get(i + 1).map_or(command_end, |n| n.start_us.min(command_end));
        let dur = us(end.saturating_sub(c.start_us));
        spans.push(span(&format!("cmd: {}", display_safe(&c.text)), h + us(c.start_us), dur, vec![]));
    }
    spans
}

fn span(name: &str, start: Duration, dur: Duration, args: Vec<(String, String)>) -> Span {
    Span { name: name.to_string(), start, dur, args }
}

/// Escapes control characters so guest text cannot drive the terminal.
pub fn display_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}
```

`display_safe` must escape C1 controls as well. `char::is_control` covers C0, DEL and C1; for a C1 character (U+0080–U+009F) `\x{:02x}` prints `\x9b` and so on, which is fine. Add `tempfile` to the root `[dev-dependencies]` if it isn't there already; the existing host unit tests already use it.

- [ ] **Step 4: Run the checks.** `cargo test -p sandcastle --lib && cargo clippy --workspace --all-targets -- -D warnings`. Expected: PASS.

- [ ] **Step 5: Commit.** Message: `Add host trace model and guest event parsing`, with the trailer.

---

### Task 5: Build wiring: durations, `--timings`, `--trace`, failure message, `--no-trace-run`

**Files:**
- Modify: `src/build/config.rs`, `src/build/mod.rs`, `src/vm/mod.rs`, `src/main.rs`, `tests/cli.rs`

**Interfaces:**
- **Consumes:** Task 4 (`Trace`, `Span`, `events::{read, guest_spans, helper_start, display_safe}`) and Task 1 (`EVENTS_FILE`, `RunJob.shell_form`).
- **Produces:**
  - `Action::Run { argv: Vec<String>, shell_form: bool }`
  - `vm::Finished { pub status, pub started: Instant, pub ended: Instant }`
  - `build::Options { …, pub timings: bool, pub trace: Option<PathBuf>, pub trace_run: bool }`
  - CLI flags `--timings`, `--trace FILE` and `--no-trace-run`.

**Changes:**
1. **`config.rs`:** `Action::Run { argv, shell_form }`, where `shell_form` is `matches!(command, Command::Shell(_))`. Update the existing `config.rs` tests' `Action::Run { argv: … }` literals: `shell_form: true` for shell form, `false` for exec form.
2. **`vm/mod.rs`:**
   - Add `pub started: Instant, pub ended: Instant` to `Finished`.
   - In `Vm::run`, set `started` immediately before `Command::new(self.exe)…status()` and `ended` immediately after it.
   - Initialise both with `Instant::now()` at construction.
3. **`build/mod.rs`:**
   - **`build`** creates `let mut trace = Trace::new();` and runs the existing body as `build_inner(exe, opts, &mut trace)`. Afterwards, when `opts.trace` is `Some(path)`, it writes `trace.to_chrome_json()` to `path`. It does this whether `build_inner` succeeded or failed. A write error prints `sandcastle: warning: could not write trace {path}: {e}` and does not change the result.
   - **`build_inner` spans.** It records spans named `parse`, `store open`, `pull` and `write layout` around those calls (start `trace.at(t0)`, duration `t0.elapsed()`), and one `step n/N <shown text>` span per step. The whole build gets a `build` span pushed at the end; on failure it carries an `error` arg.
   - **`FROM` progress line.** After the pull: `eprintln!("[1/{total}] done in {:.2}s", secs)`.
   - **`run_step`** gets `&mut Trace`, the step label and `opts`:
     - **Building the job.** `Action::Run { argv, shell_form }` builds a `RunJob` with `shell_form: shell_form && opts.trace_run`.
     - **Reading the events.** After `vm.run`, read `events::read(&finished.out_dir().join(EVENTS_FILE))` before `finished` is dropped. A read error becomes a warning and no events.
     - **Spans.** Push a `vm` span (`trace.at(finished.started)`..`finished.ended`) and the `guest_spans(…)`. Then push an `ingest` span around `ingest(…)`.
     - **Malformed lines:** when `malformed > 0`, print `sandcastle: warning: step n/N: skipped {malformed} malformed guest event lines`.
     - **Failure message.** When `status.exit_code != 0`, build it from the last cmd. `at = helper_start(ev, vm_end) + cmd.start_us - step_start`. Then:

       ```rust
       bail!(
           "exited with {}; last command started: {} ({:.1}s into the step)",
           status.exit_code,
           display_safe(&cmd.text),
           at.as_secs_f64()
       )
       ```

       With no cmd events, keep `exited with {code}`.
     - **Success line.** On success, print `[n/N] done in {:.2}s`, for RUN and COPY only; metadata steps print nothing extra.
     - **`--timings` per step.** With `opts.timings`, also print one phase line:
       - `  kernel boot {:.2}s · vmm {:.2}s · unpack {:.2}s · command {:.2}s · commit {:.2}s · ingest {:.2}s`
       - `unpack` is summed over all unpack phases; for COPY, `command` is replaced by `copy`; missing phases print `0.00s`.
       - For a shell-form RUN with cmd events, add up to 5 lines `    {:.2}s  {display_safe(text)}` for the slowest cmd spans, slowest first.
   - **`--timings` summary.** After the build, with `opts.timings`, print `Steps by duration:` and one line per step span, sorted by duration with the longest first: `  {:>8.2}s  {label}`.
4. **`main.rs`:** add the CLI fields and pass them into `Options`:

   ```rust
           /// Print each step's phases and a summary sorted by duration.
           #[arg(long)]
           timings: bool,
           /// Write a Chrome trace (open in ui.perfetto.dev) to FILE, also on failure.
           #[arg(long, value_name = "FILE")]
           trace: Option<PathBuf>,
           /// Run shell-form RUN without `sh -x` command tracing.
           #[arg(long)]
           no_trace_run: bool,
   ```

   Set `trace_run: !no_trace_run`.

- [ ] **Step 1: Failing tests.**
  - **`config.rs` tests:** extend the existing `cmd_entrypoint_and_run_argv` test to assert `shell_form: true` for `RUN echo $HOME` and `false` for `RUN ["a", "b"]`.
  - **`tests/cli.rs`** (no VM; the parse errors happen before any VM starts), a test that `--trace` writes a file even when the build fails early:

```rust
#[test]
fn trace_file_is_written_when_the_build_fails() {
    let ctx = tempfile::tempdir().unwrap();
    std::fs::write(ctx.path().join("Dockerfile"), "FROM alpine\nARG X\n").unwrap();
    let trace = ctx.path().join("trace.json");
    let output = sandcastle()
        .args(["build", "-t", "demo", "-o"])
        .arg(ctx.path().join("out"))
        .arg("--trace")
        .arg(&trace)
        .arg(ctx.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
    let names: Vec<&str> = v.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"build"), "{names:?}");
}
```

- [ ] **Step 2: Run them to see them fail.** `cargo test -p sandcastle`. Expected: compile errors or failures.

- [ ] **Step 3: Implement** the changes above. The `parse` failure path must still produce the `build` span. Push the `build` span from `build`, the outer function, using `trace.origin()`, so every exit path gets it.

- [ ] **Step 4: Run all the checks.** Workspace fmt, clippy and test, then `just it`. Existing build tests must still pass; the new `done in` lines don't break them.

- [ ] **Step 5: Commit.** Message: `Print step durations and add --timings, --trace and failing-command errors`, with the trailer.

---

### Task 6: End-to-end checks

**Files:**
- Modify: `tests/build.rs`

**Interfaces:**
- **Consumes:** the `sandcastle build` CLI with `--timings` and `--trace` (Task 5).

- [ ] **Step 1: Add tests.** Use the existing `build` helper, extended with extra args: add an `extra: &[&str]` parameter, and update the existing call sites to pass `&[]`.

```rust
#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn timings_and_trace_show_guest_phases() {
    let ctx = tempfile::tempdir().unwrap();
    let dockerfile = ctx.path().join("Dockerfile");
    std::fs::write(&dockerfile, "FROM mirror.gcr.io/library/alpine:3.20\nRUN echo one && echo two\n").unwrap();
    let trace = ctx.path().join("trace.json");
    let output = build_with(ctx.path(), Some(&dockerfile), &ctx.path().join("out"), &["--timings", "--trace", trace.to_str().unwrap()]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[2/2] done in "), "{stderr}");
    for phase in ["kernel boot", "vmm", "command", "commit", "ingest"] {
        assert!(stderr.contains(phase), "{phase} missing: {stderr}");
    }
    assert!(stderr.contains("Steps by duration:"), "{stderr}");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
    let names: Vec<String> = v.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    for n in ["build", "pull", "vm", "kernel boot", "command", "cmd: echo two", "ingest", "write layout"] {
        assert!(names.iter().any(|x| x == n), "{n} missing: {names:?}");
    }
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn failing_chain_names_its_command() {
    let stderr = failing_build("RUN true && printf '\\033[2J' >/dev/null && false && true\n");
    assert!(stderr.contains("exited with 1; last command started: false ("), "{stderr}");
    assert!(stderr.contains("s into the step)"), "{stderr}");
    // The printf's escape sequence is displayed escaped, never raw.
    assert!(!stderr.contains('\u{1b}'), "raw escape in output");
}
```

The `failing_build` helper already exists. `build_with` is the renamed and extended `build`; rename it and update its callers. Raw `\u{1b}` must not appear anywhere in stderr. The step output itself contains no ESC, because the `printf` writes to `/dev/null`; only the trace text could carry it, and that is escaped.

- [ ] **Step 2: Run the tests.** `just it`. They pass if Tasks 1–5 are right. If one fails, fix the cause in the owning code, never the assertion, and note it in the report.

- [ ] **Step 3: Commit.** Message: `Add end-to-end checks for timings, traces and failing-command errors`, with the trailer.
