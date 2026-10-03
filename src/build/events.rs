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
            Ok(Event::Phase {
                name,
                detail,
                start_us,
                dur_us,
            }) => ev.phases.push(GuestPhase {
                name: clip(name),
                detail: detail.map(clip),
                start_us,
                dur_us,
            }),
            Ok(Event::Cmd { text, start_us }) => {
                if ev.cmds.len() < MAX_CMD_EVENTS {
                    ev.cmds.push(GuestCmd {
                        text: clip(text),
                        start_us,
                    });
                } else {
                    ev.dropped += 1;
                }
            }
            Ok(Event::Limits {
                cmd_events_dropped,
                truncated,
            }) => {
                ev.dropped = ev.dropped.saturating_add(cmd_events_dropped);
                ev.truncated = ev.truncated.saturating_add(truncated);
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
    ev.end_us
        .map(|end| vm_end.saturating_sub(Duration::from_micros(end)))
}

pub fn guest_spans(ev: &GuestEvents, vm_start: Duration, vm_end: Duration) -> Vec<Span> {
    let Some(h) = helper_start(ev, vm_end) else {
        return Vec::new();
    };
    let us = Duration::from_micros;
    let mut spans = Vec::new();
    let kernel_start = h
        .saturating_sub(us(ev.kernel_boot_us.unwrap_or(0)))
        .max(vm_start);
    if ev.kernel_boot_us.is_some() {
        spans.push(span(
            "kernel boot",
            kernel_start,
            h.saturating_sub(kernel_start),
            vec![],
        ));
    }
    if kernel_start > vm_start {
        spans.push(span(
            "vmm setup",
            vm_start,
            kernel_start - vm_start,
            vec![(
                "note".into(),
                "includes VM teardown; split is approximate".into(),
            )],
        ));
    }
    for p in &ev.phases {
        let args = p
            .detail
            .iter()
            .map(|d| ("detail".to_string(), d.clone()))
            .collect();
        spans.push(span(
            &p.name,
            h.saturating_add(us(p.start_us)),
            us(p.dur_us),
            args,
        ));
    }
    let command_end = ev
        .phases
        .iter()
        .find(|p| p.name == "command")
        .map(|p| p.start_us.saturating_add(p.dur_us))
        .or(ev.end_us)
        .unwrap_or(0);
    for (i, c) in ev.cmds.iter().enumerate() {
        let end = ev
            .cmds
            .get(i + 1)
            .map_or(command_end, |n| n.start_us.min(command_end));
        let dur = us(end.saturating_sub(c.start_us));
        spans.push(span(
            &format!("cmd: {}", display_safe(&c.text)),
            h.saturating_add(us(c.start_us)),
            dur,
            vec![],
        ));
    }
    spans
}

fn span(name: &str, start: Duration, dur: Duration, args: Vec<(String, String)>) -> Span {
    Span {
        name: name.to_string(),
        start,
        dur,
        args,
    }
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
        let find = |n: &str| {
            spans
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("{n}: {spans:?}"))
        };
        // helper start = 300ms - 12ms = 288ms; kernel boot = [198ms, 288ms).
        assert_eq!(find("kernel boot").start, Duration::from_millis(198));
        assert_eq!(find("vmm setup").start, Duration::from_millis(100));
        assert_eq!(find("vmm setup").dur, Duration::from_millis(98));
        assert_eq!(find("command").start, Duration::from_millis(291));
        // "true" runs until "false" starts; "false" until command ends.
        assert_eq!(find("cmd: true").dur, Duration::from_millis(2));
        assert_eq!(find("cmd: false").dur, Duration::from_millis(4));
        assert_eq!(
            helper_start(&ev, Duration::from_millis(300)),
            Some(Duration::from_millis(288))
        );
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
