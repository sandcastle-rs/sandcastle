//! Wire types shared by the host and the in-VM guest helper.
//!
//! The host writes a [`Job`] to `/out/job.json` on the per-job virtio-fs
//! share; the guest writes a [`Status`] to `/out/status.json` before exiting.

use serde::{Deserialize, Serialize};

/// Job file name inside the guest's `/out` share.
pub const JOB_FILE: &str = "job.json";
/// Status file name inside the guest's `/out` share.
pub const STATUS_FILE: &str = "status.json";
/// Append-only JSON lines the guest writes to `/out` for observability.
pub const EVENTS_FILE: &str = "events.jsonl";
/// Copy of the guest kernel's log, written only when [`KMSG_ENV`] is set.
pub const KMSG_FILE: &str = "kmsg.txt";
/// Guest helper environment variable that asks for [`KMSG_FILE`].
pub const KMSG_ENV: &str = "SANDCASTLE_KMSG";
/// Largest kernel log copied, in bytes.
pub const MAX_KMSG_BYTES: u64 = 1 << 20;
/// Most traced shell commands kept per step; the rest are counted only.
pub const MAX_CMD_EVENTS: usize = 10_000;
/// Longest traced command text kept, in bytes.
pub const MAX_CMD_TEXT: usize = 1024;
/// Largest events file the host reads; the guest keeps below it.
pub const MAX_EVENTS_BYTES: u64 = 8 << 20;

/// Uncompressed layer tar a `run` or `copy` job leaves in `/out`.
pub const LAYER_FILE: &str = "layer.tar";
/// Path of the guest helper inside the VM.
pub const GUEST_HELPER_PATH: &str = "/sandcastle-guest";

/// virtio-fs tags; the guest mounts each at `/<tag>`.
pub const SHARE_OUT: &str = "out";
pub const SHARE_CTX: &str = "ctx";
/// `<store>/images`, read-only: the erofs image of every layer, mounted as files.
pub const SHARE_LAYERS: &str = "layers";

/// Work the guest helper performs in one VM boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Job {
    /// Self-test used by `sandcastle doctor`. The helper exits with `exit_code`.
    Probe {
        exit_code: i32,
    },
    Run(RunJob),
    Copy(CopyJob),
}

impl Job {
    /// Every layer whose erofs image the job mounts, each once: the
    /// destination stack bottom first, then source layers not already in it.
    pub fn layers(&self) -> Vec<&str> {
        let (lower, source): (&[LowerLayer], &[LowerLayer]) = match self {
            Job::Probe { .. } => return Vec::new(),
            Job::Run(job) => (&job.lower, &[]),
            Job::Copy(job) => match &job.source {
                CopySource::Context => (&job.lower, &[]),
                CopySource::Layers { lower } => (&job.lower, lower),
            },
        };
        let mut layers: Vec<&str> = Vec::new();
        for layer in lower.iter().chain(source) {
            if !layers.contains(&layer.diff_id.as_str()) {
                layers.push(&layer.diff_id);
            }
        }
        layers
    }
}

/// One image layer a step builds on; the host attaches its erofs image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LowerLayer {
    pub diff_id: String,
}

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
    /// A shell command the traced shell (`set -x`) started.
    Cmd { text: String, start_us: u64 },
    Limits {
        cmd_events_dropped: u64,
        truncated: u64,
    },
    /// Written just before `status.json`; anchors the timeline on the host.
    End { at_us: u64 },
}

/// A `RUN` step. `lower` is bottom first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunJob {
    pub lower: Vec<LowerLayer>,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    /// Dockerfile `USER` value; empty means root.
    pub user: String,
    pub workdir: String,
    /// Contents of the step's `/etc/resolv.conf`.
    pub resolv_conf: String,
    /// `argv` is `["/bin/sh", "-c", cmd]` from a shell-form RUN; the guest
    /// traces it with `set -x` to report the last command started.
    #[serde(default)]
    pub shell_form: bool,
}

/// A `COPY` step. `lower` is the destination's stack, bottom first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyJob {
    pub lower: Vec<LowerLayer>,
    pub sources: Vec<String>,
    pub dest: String,
    pub workdir: String,
    #[serde(default)]
    pub source: CopySource,
}

/// Where a `COPY` reads its sources from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CopySource {
    /// The build context, shared at `/ctx`; files are owned by root.
    #[default]
    Context,
    /// The merged filesystem of a stage or image (`COPY --from`), bottom
    /// first; files keep their owners.
    Layers { lower: Vec<LowerLayer> },
}

/// Result written by the guest helper. Authoritative over the VM process
/// exit code, because libkrun's init reserves 125, 126 and 127.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReport>,
    /// diff_id of `/out/layer.tar`; `None` when the step changed nothing
    /// or the command failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// The helper itself failed; `exit_code` is meaningless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Guest capabilities found by [`Job::Probe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub kernel_release: String,
    /// Overlayfs accepts `lowerdir+` (kernel ≥ 6.8), needed for long layer stacks.
    pub overlay_lowerdir_plus: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Host and guest are built for different targets and copied separately,
    // so the JSON on the virtio-fs share is a contract; pin its exact shape.
    #[test]
    fn job_wire_format() {
        let job: Job = serde_json::from_str(r#"{"mode":"probe","exit_code":127}"#).unwrap();
        assert_eq!(job, Job::Probe { exit_code: 127 });
    }

    #[test]
    fn status_wire_format() {
        let status: Status = serde_json::from_str(
            r#"{"exit_code":0,"probe":{"kernel_release":"6.12.0","overlay_lowerdir_plus":true}}"#,
        )
        .unwrap();
        assert_eq!(status.probe.unwrap().kernel_release, "6.12.0");
        let bare: Status = serde_json::from_str(r#"{"exit_code":3}"#).unwrap();
        assert_eq!(
            bare,
            Status {
                exit_code: 3,
                ..Default::default()
            }
        );
    }

    #[test]
    fn run_job_wire_format() {
        let job: Job = serde_json::from_str(
            r#"{"mode":"run","lower":[{"diff_id":"sha256:aa"}],
                "argv":["/bin/sh","-c","true"],"env":["PATH=/bin"],"user":"","workdir":"/","resolv_conf":"nameserver 8.8.8.8\n"}"#,
        )
        .unwrap();
        let Job::Run(run) = job else {
            panic!("not a run job")
        };
        assert_eq!(run.lower[0].diff_id, "sha256:aa");
        assert_eq!(run.argv, ["/bin/sh", "-c", "true"]);
    }

    #[test]
    fn copy_job_wire_format() {
        // Jobs written before `source` existed copy from the context.
        let job: Job = serde_json::from_str(
            r#"{"mode":"copy","lower":[],"sources":["a","b/"],"dest":"/x/","workdir":"/app"}"#,
        )
        .unwrap();
        assert_eq!(
            job,
            Job::Copy(CopyJob {
                lower: vec![],
                sources: vec!["a".into(), "b/".into()],
                dest: "/x/".into(),
                workdir: "/app".into(),
                source: CopySource::Context,
            })
        );
    }

    #[test]
    fn copy_from_layers_wire_format() {
        let json = r#"{"mode":"copy","lower":[],"sources":["/out/app"],"dest":"/app","workdir":"/","source":{"kind":"layers","lower":[{"diff_id":"sha256:aa"}]}}"#;
        let job: Job = serde_json::from_str(json).unwrap();
        let Job::Copy(copy) = &job else {
            panic!("not a copy job")
        };
        let CopySource::Layers { lower } = &copy.source else {
            panic!("not a layers source")
        };
        assert_eq!(lower[0].diff_id, "sha256:aa");
        assert_eq!(serde_json::to_string(&job).unwrap(), json);
    }

    #[test]
    fn status_with_layer_and_error() {
        let status: Status =
            serde_json::from_str(r#"{"exit_code":0,"layer":"sha256:cc"}"#).unwrap();
        assert_eq!(status.layer.as_deref(), Some("sha256:cc"));
        let failed: Status =
            serde_json::from_str(r#"{"exit_code":1,"error":"mounting the overlay"}"#).unwrap();
        assert_eq!(failed.error.as_deref(), Some("mounting the overlay"));
        assert_eq!(
            serde_json::to_string(&Status {
                exit_code: 3,
                ..Default::default()
            })
            .unwrap(),
            r#"{"exit_code":3}"#
        );
    }

    #[test]
    fn event_wire_format() {
        let lines = [
            r#"{"type":"boot","kernel_boot_us":91000}"#,
            r#"{"type":"phase","name":"unpack","detail":"sha256:1a2b3c4d5e6f","start_us":1200,"dur_us":840000}"#,
            r#"{"type":"cmd","text":"apt-get install -y foo","start_us":2100000}"#,
            r#"{"type":"limits","cmd_events_dropped":3,"truncated":1}"#,
            r#"{"type":"end","at_us":4180000}"#,
        ];
        let events: Vec<Event> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            events[0],
            Event::Boot {
                kernel_boot_us: 91000
            }
        );
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
        let no_detail = Event::Phase {
            name: "commit".into(),
            detail: None,
            start_us: 1,
            dur_us: 2,
        };
        assert_eq!(
            serde_json::to_string(&no_detail).unwrap(),
            r#"{"type":"phase","name":"commit","start_us":1,"dur_us":2}"#
        );
    }

    fn layers(ids: &[&str]) -> Vec<LowerLayer> {
        ids.iter()
            .map(|id| LowerLayer {
                diff_id: id.to_string(),
            })
            .collect()
    }

    #[test]
    fn layers_list_the_destination_first_and_each_layer_once() {
        let copy = |source| {
            Job::Copy(CopyJob {
                lower: layers(&["a", "b", "a"]),
                sources: vec![],
                dest: "/".into(),
                workdir: "/".into(),
                source,
            })
        };
        assert_eq!(copy(CopySource::Context).layers(), ["a", "b"]);
        let from = CopySource::Layers {
            lower: layers(&["c", "b", "d"]),
        };
        assert_eq!(copy(from).layers(), ["a", "b", "c", "d"]);
        assert!(Job::Probe { exit_code: 0 }.layers().is_empty());
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
}
