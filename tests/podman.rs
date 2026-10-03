//! Runs a built image with podman. Linux CI only: `just it-podman`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(cmd: &mut Command) -> String {
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        cmd,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
#[ignore = "needs podman, skopeo, lib/, a hypervisor and network; run with `just it-podman`"]
fn podman_runs_built_image() {
    let bin = PathBuf::from(std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN"));
    let out = tempfile::tempdir().unwrap();
    let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build/alpine");
    run(Command::new(&bin)
        .args(["build", "-t", "demo", "-o"])
        .arg(out.path())
        .arg(&context)
        .env("SANDCASTLE_ROOT", bin.parent().unwrap().join("store")));
    let image = "localhost/sandcastle-it:latest";
    run(Command::new("skopeo")
        .arg("copy")
        .arg(format!("oci:{}:demo", out.path().display()))
        .arg(format!("containers-storage:{image}")));
    assert_eq!(
        run(Command::new("podman").args(["run", "--rm", image])),
        "hello world\n"
    );
    assert_eq!(
        run(Command::new("podman").args(["run", "--rm", image, "id -u && pwd"])),
        "1234\n/app\n"
    );
    run(Command::new("podman").args(["rmi", image]));
}
