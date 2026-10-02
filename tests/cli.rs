//! End-to-end CLI test. Run with `just it`.

use std::process::Command;

#[test]
#[ignore = "needs bundled libkrun and a hypervisor; run with `just it`"]
fn doctor_json_reports_guest_kernel() {
    let bin = std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN");
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .args(["doctor", "--json"])
        .env("SANDCASTLE_ROOT", root.path().join("store"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["kernel_release"]
            .as_str()
            .is_some_and(|k| !k.is_empty()),
        "{report}"
    );
}
