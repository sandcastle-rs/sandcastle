//! Wire types shared by the host and the in-VM guest helper.
//!
//! The host writes a [`Job`] to `/out/job.json` on the per-job virtio-fs
//! share; the guest writes a [`Status`] to `/out/status.json` before exiting.

use serde::{Deserialize, Serialize};

/// Job file name inside the guest's `/out` share.
pub const JOB_FILE: &str = "job.json";
/// Status file name inside the guest's `/out` share.
pub const STATUS_FILE: &str = "status.json";

/// Work the guest helper performs in one VM boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Job {
    /// Self-test used by `sandcastle doctor`. The helper exits with `exit_code`.
    Probe { exit_code: i32 },
}

/// Result written by the guest helper. Authoritative over the VM process
/// exit code, because libkrun's init reserves 125, 126 and 127.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReport>,
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
                probe: None
            }
        );
    }
}
