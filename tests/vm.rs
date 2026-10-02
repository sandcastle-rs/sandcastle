//! VM integration tests. Run with `just it` (needs lib/ and a hypervisor).

use std::path::PathBuf;

use sandcastle::install::Install;
use sandcastle::store::Store;
use sandcastle::vm::run_job;
use sandcastle_proto::Job;

fn sandcastle_bin() -> PathBuf {
    std::env::var_os("SANDCASTLE_BIN")
        .expect("SANDCASTLE_BIN must point at the signed sandcastle binary")
        .into()
}

/// A store under a directory with a space, as on many macOS setups.
fn store() -> (tempfile::TempDir, PathBuf, Install, Store) {
    let exe = sandcastle_bin();
    let install = Install::locate(&exe).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("store root"), &install).unwrap();
    (dir, exe, install, store)
}

#[test]
#[ignore = "needs bundled libkrun and a hypervisor; run with `just it`"]
fn probe_mounts_store_and_supports_overlay_lowerdir_plus() {
    let (_dir, exe, install, store) = store();
    let status = run_job(&exe, &store, &install, &Job::Probe { exit_code: 0 }).unwrap();
    assert_eq!(status.exit_code, 0);
    let probe = status.probe.expect("probe report");
    assert!(!probe.kernel_release.is_empty());
    assert!(
        probe.overlay_lowerdir_plus,
        "guest kernel {} lacks overlay lowerdir+",
        probe.kernel_release
    );
}

#[test]
#[ignore = "needs bundled libkrun and a hypervisor; run with `just it`"]
fn guest_exit_127_comes_from_status_file() {
    let (_dir, exe, install, store) = store();
    let status = run_job(&exe, &store, &install, &Job::Probe { exit_code: 127 }).unwrap();
    assert_eq!(status.exit_code, 127);
    assert!(
        status.probe.is_some(),
        "status file must be present, not inferred from the exit code"
    );
}
