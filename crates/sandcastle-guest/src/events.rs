//! Appends observability events to `/out/events.jsonl`, one JSON line per
//! write. Failing to record never fails the job: the first write error is
//! reported once and recording stops.

use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use sandcastle_proto::{Event, MAX_CMD_EVENTS};

/// Bytes of `cmd` lines written per job, well below the host's
/// `MAX_EVENTS_BYTES` so the events after them always fit.
const CMD_BYTES_BUDGET: usize = 6 << 20;

pub struct Recorder {
    file: RefCell<Option<File>>,
    start: Instant,
    cmds: Cell<usize>,
    cmd_bytes: Cell<usize>,
    dropped: Cell<u64>,
    /// Line of the most recent dropped cmd, written by `finish` so the last
    /// cmd in the file is the last one started.
    last_dropped: RefCell<Option<Vec<u8>>>,
    truncated: Cell<u64>,
}

impl Recorder {
    pub fn create(path: &Path, start: Instant) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: RefCell::new(Some(file)),
            start,
            cmds: Cell::new(0),
            cmd_bytes: Cell::new(0),
            dropped: Cell::new(0),
            last_dropped: RefCell::new(None),
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

    /// Records a traced command. Past the count or byte budget it is only
    /// counted; one slot of `MAX_CMD_EVENTS` is kept for the last dropped
    /// command, which `finish` writes.
    pub fn cmd(&self, text: &str, start_us: u64) {
        let line = encode(&Event::Cmd {
            text: text.to_string(),
            start_us,
        });
        let bytes = self.cmd_bytes.get() + line.len();
        // Once one is dropped, every later one is too: cmds stay in order.
        let full = self.dropped.get() > 0
            || self.cmds.get() >= MAX_CMD_EVENTS - 1
            || bytes > CMD_BYTES_BUDGET;
        if full {
            self.dropped.set(self.dropped.get() + 1);
            *self.last_dropped.borrow_mut() = Some(line);
            return;
        }
        self.cmds.set(self.cmds.get() + 1);
        self.cmd_bytes.set(bytes);
        self.write_line(&line);
    }

    pub fn add_truncated(&self, n: u64) {
        self.truncated.set(self.truncated.get() + n);
    }

    /// Writes the limits and end events and syncs, so the host sees every
    /// event before it sees `status.json`.
    pub fn finish(self) -> io::Result<()> {
        if let Some(line) = self.last_dropped.take() {
            self.write_line(&line);
            self.dropped.set(self.dropped.get() - 1);
        }
        self.write(&Event::Limits {
            cmd_events_dropped: self.dropped.get(),
            truncated: self.truncated.get(),
        });
        self.write(&Event::End {
            at_us: self.now_us(),
        });
        match self.file.into_inner() {
            Some(file) => file.sync_all(),
            None => Ok(()),
        }
    }

    fn write(&self, event: &Event) {
        self.write_line(&encode(event));
    }

    fn write_line(&self, line: &[u8]) {
        let mut slot = self.file.borrow_mut();
        let Some(file) = slot.as_mut() else { return };
        if let Err(e) = file.write_all(line) {
            eprintln!("sandcastle-guest: not recording further events: {e}");
            *slot = None;
        }
    }
}

fn encode(event: &Event) -> Vec<u8> {
    let mut line = serde_json::to_vec(event).expect("events serialize");
    line.push(b'\n');
    line
}

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
        assert_eq!(
            events[0],
            Event::Boot {
                kernel_boot_us: 91_000
            }
        );
        assert!(
            matches!(&events[1], Event::Phase { name, detail: Some(d), .. } if name == "unpack" && d == "sha256:1a2b")
        );
        assert!(matches!(&events[2], Event::Cmd { text, .. } if text == "true"));
        assert!(matches!(
            events[3],
            Event::Limits {
                cmd_events_dropped: 0,
                truncated: 0
            }
        ));
        assert!(matches!(events[4], Event::End { .. }));
    }

    #[test]
    fn cmd_events_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let rec = Recorder::create(&path, Instant::now()).unwrap();
        for i in 0..MAX_CMD_EVENTS + 5 {
            rec.cmd(&format!("x{i}"), 1);
        }
        rec.add_truncated(2);
        rec.finish().unwrap();
        let events = read(&path);
        let cmds = cmd_texts(&events);
        assert_eq!(cmds.len(), MAX_CMD_EVENTS);
        assert_eq!(
            cmds.last().unwrap(),
            &format!("x{}", MAX_CMD_EVENTS + 4),
            "the last command started is kept"
        );
        assert!(events.contains(&Event::Limits {
            cmd_events_dropped: 5,
            truncated: 2
        }));
        assert!(matches!(events.last(), Some(Event::End { .. })));
    }

    #[test]
    fn cmd_bytes_are_capped_below_the_host_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let rec = Recorder::create(&path, Instant::now()).unwrap();
        // Control characters escape to six bytes each in JSON.
        let text = "\u{1}".repeat(sandcastle_proto::MAX_CMD_TEXT);
        for _ in 0..MAX_CMD_EVENTS + 5 {
            rec.cmd(&text, 1);
        }
        rec.cmd("final", 2);
        rec.finish().unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < sandcastle_proto::MAX_EVENTS_BYTES, "{size} bytes");
        let events = read(&path);
        let cmds = cmd_texts(&events);
        assert_eq!(cmds.last().map(String::as_str), Some("final"));
        let dropped = MAX_CMD_EVENTS + 6 - cmds.len();
        assert!(events.contains(&Event::Limits {
            cmd_events_dropped: dropped as u64,
            truncated: 0
        }));
        assert!(matches!(events.last(), Some(Event::End { .. })));
    }

    fn cmd_texts(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Cmd { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}
