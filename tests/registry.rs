//! Registry integration tests: need network and skopeo. Run with `just it-registry`.

use std::path::Path;
use std::process::{Command, Output};

const IMAGE: &str = "mirror.gcr.io/library/busybox:1.36";

fn sandcastle(store: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sandcastle"))
        .args(args)
        .env("SANDCASTLE_ROOT", store)
        .output()
        .unwrap()
}

fn skopeo(args: &[&str]) -> Output {
    Command::new("skopeo")
        .args(args)
        .output()
        .expect("skopeo not found: install it (brew install skopeo / apt-get install skopeo)")
}

fn assert_ok(what: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "needs network and skopeo; run with `just it-registry`"]
fn pulled_layout_is_readable_by_skopeo() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("busybox layout");
    let out_arg = out.to_str().unwrap();
    assert_ok(
        "pull",
        &sandcastle(
            &dir.path().join("store"),
            &["pull", IMAGE, "-o", out_arg, "--tag", "test"],
        ),
    );

    let layout = format!("oci:{out_arg}:test");
    let inspect = skopeo(&["inspect", &layout]);
    assert_ok("skopeo inspect", &inspect);
    let info: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(info["Os"], "linux");
    // skopeo copy reads and digest-checks every blob of the layout.
    let copy = format!("dir:{}", dir.path().join("copy").display());
    assert_ok("skopeo copy", &skopeo(&["copy", &layout, &copy]));
}

#[test]
#[ignore = "needs network and skopeo; run with `just it-registry`"]
fn second_pull_reuses_stored_layers() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let out = dir.path().join("out");
    let out_arg = out.to_str().unwrap();
    assert_ok(
        "first pull",
        &sandcastle(&store, &["pull", IMAGE, "-o", out_arg]),
    );
    let second = sandcastle(&store, &["pull", IMAGE, "-o", out_arg]);
    assert_ok("second pull", &second);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already present") && !stderr.contains("downloaded"),
        "{stderr}"
    );
    assert_ok(
        "skopeo inspect",
        &skopeo(&["inspect", &format!("oci:{out_arg}:1.36")]),
    );
}

#[test]
#[ignore = "needs network; run with `just it-registry`"]
fn missing_tag_fails_naming_the_reference() {
    let dir = tempfile::tempdir().unwrap();
    let reference = "mirror.gcr.io/library/busybox:sandcastle-no-such-tag";
    let output = sandcastle(
        &dir.path().join("store"),
        &[
            "pull",
            reference,
            "-o",
            dir.path().join("out").to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(reference));
}
