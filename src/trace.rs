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
        Self {
            origin: Instant::now(),
            spans: Vec::new(),
        }
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
                let args: serde_json::Map<String, serde_json::Value> = s
                    .args
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into()))
                    .collect();
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn chrome_json_has_complete_events_in_microseconds() {
        let mut t = Trace::new();
        t.push(Span {
            name: "step 2/3 RUN x".into(),
            start: Duration::from_millis(5),
            dur: Duration::from_millis(1500),
            args: vec![("exit".into(), "0".into())],
        });
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
