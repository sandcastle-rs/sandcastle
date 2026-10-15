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

/// Builds the fixture `name` and loads it into podman as `image`. The
/// returned directory holds the layout.
fn build_and_load(name: &str, image: &str) -> tempfile::TempDir {
    let bin = PathBuf::from(std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN"));
    let out = tempfile::tempdir().unwrap();
    let context = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/build")
        .join(name);
    run(Command::new(&bin)
        .args(["build", "-t", "demo", "-o"])
        .arg(out.path())
        .arg(&context)
        .env("SANDCASTLE_ROOT", bin.parent().unwrap().join("store")));
    run(Command::new("skopeo")
        .arg("copy")
        .arg(format!("oci:{}:demo", out.path().display()))
        .arg(format!("containers-storage:{image}")));
    out
}

#[test]
#[ignore = "needs podman, skopeo, lib/, a hypervisor and network; run with `just it-podman`"]
fn podman_runs_built_image() {
    let image = "localhost/sandcastle-it:latest";
    let _out = build_and_load("alpine", image);
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

#[test]
#[ignore = "needs podman, skopeo, lib/, a hypervisor and network; run with `just it-podman`"]
fn podman_runs_a_static_binary_copied_into_scratch() {
    let image = "localhost/sandcastle-it-multi:latest";
    let _out = build_and_load("multi-stage", image);
    assert_eq!(
        run(Command::new("podman").args(["run", "--rm", image])),
        "built\n"
    );
    assert_eq!(
        run(Command::new("podman").args(["run", "--rm", image, "cat", "/late.txt"])),
        "late\n"
    );
    run(Command::new("podman").args(["rmi", image]));
}
