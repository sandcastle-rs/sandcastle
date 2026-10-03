//! The guest's `events.jsonl`: parsed defensively (the guest is untrusted),
//! aligned onto the host's timeline, and made safe to print.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use sandcastle_proto::{Event, MAX_CMD_EVENTS, MAX_CMD_TEXT, MAX_EVENTS_BYTES};

use crate::trace::Span;
use crate::vm::open_guest_file;

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

/// The phase names the guest helper emits. Anything else is not trusted
/// to name a span, so it cannot pose as a host-side span in summaries.
const PHASES: [&str; 8] = [
    "store mount",
    "unpack",
    "overlay",
    "command",
    "copy",
    "commit",
    "sync",
    "unmount",
];

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
            }) => {
                if !PHASES.contains(&name.as_str()) {
                    ev.malformed += 1;
                } else if ev.phases.len() < MAX_CMD_EVENTS {
                    ev.phases.push(GuestPhase {
                        name: clip(name),
                        detail: detail.map(clip),
                        start_us,
                        dur_us,
                    });
                } else {
                    ev.dropped = ev.dropped.saturating_add(1);
                }
            }
            Ok(Event::Cmd { text, start_us }) => {
                let cmd = GuestCmd {
                    text: clip(text),
                    start_us,
                };
                if ev.cmds.len() < MAX_CMD_EVENTS {
                    ev.cmds.push(cmd);
                } else if let Some(last) = ev.cmds.last_mut() {
                    // Keep the last command started; it names a failure.
                    *last = cmd;
                    ev.dropped = ev.dropped.saturating_add(1);
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

/// Where one VM's life sits on the build timeline, from host measurements.
#[derive(Debug, Clone, Copy)]
pub struct VmTimeline {
    /// The `__vm` process was spawned and exited.
    pub start: Duration,
    pub end: Duration,
    /// The child's start-up marks, when it wrote them.
    pub host: Option<HostMarks>,
    /// The guest's last write of `status.json` on the host clock: the end
    /// of the guest helper.
    pub helper_end: Option<Duration>,
}

impl VmTimeline {
    /// Process lifetime only, without marks.
    pub fn new(start: Duration, end: Duration) -> Self {
        Self {
            start,
            end,
            host: None,
            helper_end: None,
        }
    }
}

/// See [`crate::vm::VmMarks`]; offsets on the build timeline.
#[derive(Debug, Clone, Copy)]
pub struct HostMarks {
    pub main: Duration,
    pub loaded: Duration,
    pub configured: Duration,
    pub enter: Duration,
}

pub fn guest_spans(ev: &GuestEvents, vm: &VmTimeline) -> Vec<Span> {
    let vm_start = vm.start;
    let vm_end = vm.end.max(vm_start);
    // Marks only count when they are in order inside the process lifetime.
    let host = vm.host.filter(|m| {
        vm_start <= m.main
            && m.main <= m.loaded
            && m.loaded <= m.configured
            && m.configured <= m.enter
            && m.enter <= vm_end
    });
    let entered = host.map_or(vm_start, |m| m.enter);
    // The helper's end, stamped on the host clock, anchors the guest's
    // timeline; without it the helper is assumed to end as the VM exits.
    let helper_end = vm
        .helper_end
        .filter(|&t| entered <= t && t <= vm_end)
        .unwrap_or(vm_end);
    let Some(h) = ev
        .end_us
        .map(|end| helper_end.saturating_sub(Duration::from_micros(end)))
    else {
        return Vec::new();
    };
    let us = Duration::from_micros;
    let mut spans = Vec::new();
    let kernel_start = h
        .saturating_sub(us(ev.kernel_boot_us.unwrap_or(0)))
        .max(entered);
    if ev.kernel_boot_us.is_some() {
        spans.push(span(
            "kernel boot",
            kernel_start,
            h.saturating_sub(kernel_start),
            vec![],
        ));
    }
    match host {
        Some(m) => {
            spans.push(span("process start", vm_start, m.main - vm_start, vec![]));
            spans.push(span("libkrun load", m.main, m.loaded - m.main, vec![]));
            spans.push(span("vm configure", m.loaded, m.enter - m.loaded, vec![]));
            // libkrun's own VM setup is ~16 ms of this (strace on a bench
            // box); the rest is the guest kernel running before its boot
            // clock starts (early boot, CPU bring-up).
            spans.push(span("early boot", m.enter, kernel_start - m.enter, vec![]));
        }
        None if kernel_start > vm_start => spans.push(span(
            "vmm setup",
            vm_start,
            kernel_start - vm_start,
            vec![(
                "note".into(),
                "includes VM teardown; split is approximate".into(),
            )],
        )),
        None => {}
    }
    if helper_end < vm_end {
        spans.push(span("vm teardown", helper_end, vm_end - helper_end, vec![]));
    }
    for p in &ev.phases {
        let args = p
            .detail
            .iter()
            .map(|d| ("detail".to_string(), display_safe(d)))
            .collect();
        spans.push(span(
            &display_safe(&p.name),
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
    // The guest picks every offset; keep its spans inside the VM's lifetime.
    for s in &mut spans {
        let start = s.start.clamp(vm_start, vm_end);
        let end = s.start.saturating_add(s.dur).clamp(start, vm_end);
        s.start = start;
        s.dur = end - start;
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

/// Escapes control characters and invisible Unicode format characters
/// (bidi overrides and the like) so guest text can neither drive the
/// terminal nor spoof what is printed.
pub fn display_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else if is_format_char(c) {
            out.push_str(&format!("\\u{{{:04x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Unicode general category Cf characters worth escaping (std has no
/// category API, so the ranges are listed).
fn is_format_char(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
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
    fn over_the_cap_the_newest_cmd_replaces_the_last_kept() {
        let mut input = String::new();
        for i in 0..MAX_CMD_EVENTS + 5 {
            input.push_str(&format!(
                "{{\"type\":\"cmd\",\"text\":\"x{i}\",\"start_us\":{i}}}\n"
            ));
        }
        let ev = parse(input.as_bytes());
        assert_eq!(ev.cmds.len(), MAX_CMD_EVENTS);
        assert_eq!(
            ev.cmds[MAX_CMD_EVENTS - 2].text,
            format!("x{}", MAX_CMD_EVENTS - 2)
        );
        let last = ev.cmds.last().unwrap();
        assert_eq!(last.text, format!("x{}", MAX_CMD_EVENTS + 4));
        assert_eq!(last.start_us, (MAX_CMD_EVENTS + 4) as u64);
        assert_eq!(ev.dropped, 5);
    }

    #[test]
    fn unknown_phase_names_are_dropped() {
        let ev = parse(
            br#"{"type":"phase","name":"step 9/9 RUN x","start_us":1,"dur_us":2}
{"type":"phase","name":"ingest","start_us":1,"dur_us":2}
{"type":"phase","name":"unpack","start_us":1,"dur_us":2}
{"type":"end","at_us":10}
"#,
        );
        assert_eq!(ev.malformed, 2);
        let spans = guest_spans(
            &ev,
            &VmTimeline::new(Duration::ZERO, Duration::from_millis(1)),
        );
        assert!(spans.iter().any(|s| s.name == "unpack"));
        assert!(
            !spans
                .iter()
                .any(|s| s.name == "ingest" || s.name.starts_with("step"))
        );
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
        let spans = guest_spans(
            &ev,
            &VmTimeline::new(Duration::from_millis(100), Duration::from_millis(300)),
        );
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
    }

    fn marked(helper_end_ms: u64) -> VmTimeline {
        let ms = Duration::from_millis;
        VmTimeline {
            start: ms(100),
            end: ms(300),
            host: Some(HostMarks {
                main: ms(105),
                loaded: ms(140),
                configured: ms(150),
                enter: ms(151),
            }),
            helper_end: Some(ms(helper_end_ms)),
        }
    }

    #[test]
    fn host_marks_split_vmm_setup_and_teardown() {
        let ev = parse(GOOD.as_bytes());
        let spans = guest_spans(&ev, &marked(290));
        let find = |n: &str| {
            spans
                .iter()
                .find(|s| s.name == n)
                .unwrap_or_else(|| panic!("{n}: {spans:?}"))
        };
        let ms = Duration::from_millis;
        // Guest helper ends at 290ms (status.json mtime), 12ms after it
        // started at 278ms; the kernel started 90ms before that, at 188ms.
        assert_eq!(
            (find("process start").start, find("process start").dur),
            (ms(100), ms(5))
        );
        assert_eq!(
            (find("libkrun load").start, find("libkrun load").dur),
            (ms(105), ms(35))
        );
        assert_eq!(
            (find("vm configure").start, find("vm configure").dur),
            (ms(140), ms(11))
        );
        assert_eq!(
            (find("early boot").start, find("early boot").dur),
            (ms(151), ms(37))
        );
        assert_eq!(
            (find("kernel boot").start, find("kernel boot").dur),
            (ms(188), ms(90))
        );
        assert_eq!(
            (find("vm teardown").start, find("vm teardown").dur),
            (ms(290), ms(10))
        );
        assert_eq!(find("command").start, ms(281));
        assert!(spans.iter().all(|s| s.name != "vmm setup"), "{spans:?}");
    }

    #[test]
    fn out_of_order_marks_fall_back_to_vmm_setup() {
        let ev = parse(GOOD.as_bytes());
        let mut t = marked(290);
        if let Some(h) = t.host.as_mut() {
            h.loaded = Duration::from_millis(101); // before `main`
        }
        let spans = guest_spans(&ev, &t);
        assert!(spans.iter().any(|s| s.name == "vmm setup"), "{spans:?}");
        assert!(spans.iter().all(|s| s.name != "libkrun load"), "{spans:?}");
    }

    #[test]
    fn helper_end_outside_the_vm_is_ignored() {
        let ev = parse(GOOD.as_bytes());
        for bad in [350, 120] {
            let spans = guest_spans(&ev, &marked(bad));
            assert!(
                spans.iter().all(|s| s.name != "vm teardown"),
                "{bad}: {spans:?}"
            );
            // Falls back to anchoring the helper's end at the VM's exit.
            let command = spans.iter().find(|s| s.name == "command").unwrap();
            assert_eq!(command.start, Duration::from_millis(291), "{bad}");
        }
    }

    #[test]
    fn no_end_event_gives_no_guest_spans() {
        let ev = parse(br#"{"type":"cmd","text":"x","start_us":1}"#);
        assert!(
            guest_spans(
                &ev,
                &VmTimeline::new(Duration::ZERO, Duration::from_secs(1))
            )
            .is_empty()
        );
    }

    #[test]
    fn display_safe_escapes_controls() {
        assert_eq!(display_safe("rm \u{1b}[2Jx\tok"), "rm \\x1b[2Jx\\x09ok");
        assert_eq!(display_safe("café"), "café");
    }

    #[test]
    fn display_safe_escapes_c1_and_format_characters() {
        assert_eq!(display_safe("a\u{9b}b"), "a\\x9bb");
        assert_eq!(display_safe("a\u{202e}b\u{2066}"), "a\\u{202e}b\\u{2066}");
    }

    #[test]
    fn hostile_limits_cannot_overflow_dropped() {
        let mut input = String::from(
            "{\"type\":\"limits\",\"cmd_events_dropped\":18446744073709551615,\"truncated\":18446744073709551615}\n",
        );
        for _ in 0..sandcastle_proto::MAX_CMD_EVENTS + 3 {
            input.push_str("{\"type\":\"cmd\",\"text\":\"x\",\"start_us\":1}\n");
        }
        let ev = parse(input.as_bytes());
        assert_eq!(ev.dropped, u64::MAX);
        assert_eq!(ev.truncated, u64::MAX);
    }

    #[test]
    fn phases_are_capped() {
        let mut input = String::new();
        for _ in 0..sandcastle_proto::MAX_CMD_EVENTS + 2 {
            input.push_str("{\"type\":\"phase\",\"name\":\"copy\",\"start_us\":1,\"dur_us\":1}\n");
        }
        let ev = parse(input.as_bytes());
        assert_eq!(ev.phases.len(), sandcastle_proto::MAX_CMD_EVENTS);
        assert_eq!(ev.dropped, 2);
    }

    #[test]
    fn clip_truncates_at_a_char_boundary() {
        let s = "é".repeat(sandcastle_proto::MAX_CMD_TEXT);
        let clipped = clip(s);
        assert!(clipped.len() <= sandcastle_proto::MAX_CMD_TEXT);
        assert!(clipped.chars().all(|c| c == 'é'));
    }

    #[test]
    fn hostile_offsets_stay_inside_the_vm() {
        let max = u64::MAX;
        let input = format!(
            "{{\"type\":\"boot\",\"kernel_boot_us\":{max}}}\n\
             {{\"type\":\"phase\",\"name\":\"unpack\",\"detail\":\"d\\u202e\",\"start_us\":{max},\"dur_us\":{max}}}\n\
             {{\"type\":\"cmd\",\"text\":\"x\",\"start_us\":{max}}}\n\
             {{\"type\":\"end\",\"at_us\":{max}}}\n"
        );
        let ev = parse(input.as_bytes());
        let (a, b) = (Duration::from_millis(100), Duration::from_millis(300));
        let spans = guest_spans(&ev, &VmTimeline::new(a, b));
        assert!(!spans.is_empty());
        for s in &spans {
            assert!(s.start >= a && s.start + s.dur <= b, "{s:?}");
        }
        let p = spans.iter().find(|s| s.name == "unpack").unwrap();
        assert_eq!(p.args[0].1, "d\\u{202e}");
    }
}
