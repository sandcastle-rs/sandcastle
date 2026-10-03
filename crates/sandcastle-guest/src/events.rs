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
        self.write(&Event::Cmd {
            text: text.to_string(),
            start_us,
        });
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
        self.write(&Event::End {
            at_us: self.now_us(),
        });
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
        for _ in 0..MAX_CMD_EVENTS + 5 {
            rec.cmd("x", 1);
        }
        rec.add_truncated(2);
        rec.finish().unwrap();
        let events = read(&path);
        let cmds = events
            .iter()
            .filter(|e| matches!(e, Event::Cmd { .. }))
            .count();
        assert_eq!(cmds, MAX_CMD_EVENTS);
        assert!(events.contains(&Event::Limits {
            cmd_events_dropped: 5,
            truncated: 2
        }));
    }
}
