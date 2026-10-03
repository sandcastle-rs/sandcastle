//! End-to-end CLI test. Run with `just it`.

use std::process::Command;

fn sandcastle() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sandcastle"))
}

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

#[test]
fn build_rejects_unsupported_instruction_before_pulling() {
    let ctx = tempfile::tempdir().unwrap();
    std::fs::write(ctx.path().join("Dockerfile"), "FROM alpine\nARG VERSION\n").unwrap();
    let output = sandcastle()
        .args(["build", "-t", "demo", "-o"])
        .arg(ctx.path().join("out"))
        .arg(ctx.path())
        .env("SANDCASTLE_LIBKRUN_DIR", "/nonexistent")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("line 2: ARG is not supported yet"),
        "{stderr}"
    );
}

#[test]
fn build_reports_missing_dockerfile() {
    let ctx = tempfile::tempdir().unwrap();
    let output = sandcastle()
        .args(["build", "-t", "demo", "-o", "out", "-f"])
        .arg(ctx.path().join("Nope"))
        .arg(ctx.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Nope"));
}

#[test]
fn build_rejects_zero_cpus_and_memory() {
    for flag in ["--cpus", "--memory"] {
        let output = sandcastle()
            .args(["build", "-t", "demo", "-o", "out", flag, "0", "."])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{flag}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(flag) && stderr.contains("not in 1.."),
            "{stderr}"
        );
    }
}
