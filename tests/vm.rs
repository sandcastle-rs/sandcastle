//! VM integration tests. Run with `just it` (needs lib/ and a hypervisor).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use sandcastle::image::ConfigState;
use sandcastle::install::Install;
use sandcastle::registry;
use sandcastle::store::Store;
use sandcastle::vm::{Resources, Vm};
use sandcastle_proto::{CopyJob, Job, LAYER_FILE, LowerLayer, Status};

fn sandcastle_bin() -> PathBuf {
    std::env::var_os("SANDCASTLE_BIN")
        .expect("SANDCASTLE_BIN must point at the signed sandcastle binary")
        .into()
}

fn run(exe: &std::path::Path, install: &Install, store: &Store, job: &Job) -> Status {
    let vm = Vm {
        exe,
        install,
        store,
        resources: Resources::default(),
    };
    vm.run(job, None).unwrap().status.clone()
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
    let status = run(&exe, &install, &store, &Job::Probe { exit_code: 0 });
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
    let status = run(&exe, &install, &store, &Job::Probe { exit_code: 127 });
    assert_eq!(status.exit_code, 127);
    assert!(
        status.probe.is_some(),
        "status file must be present, not inferred from the exit code"
    );
}

const BUSYBOX: &str = "mirror.gcr.io/library/busybox:1.36";

/// Pulls busybox into the store and returns its layer stack.
fn busybox(store: &Store) -> Vec<LowerLayer> {
    let image = registry::pull(&store.blobs(), BUSYBOX).unwrap();
    ConfigState::from_base(&image.config, image.layers)
        .unwrap()
        .lower_layers()
        .unwrap()
}

/// path → (entry type, uid, contents or link target) of the job's layer.tar.
fn layer_entries(out_dir: &std::path::Path) -> BTreeMap<String, (tar::EntryType, u64, Vec<u8>)> {
    let file = std::fs::File::open(out_dir.join(LAYER_FILE)).unwrap();
    let mut archive = tar::Archive::new(file);
    let mut map = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let kind = entry.header().entry_type();
        let uid = entry.header().uid().unwrap();
        let body = match entry.link_name().unwrap() {
            Some(target) => target.to_string_lossy().into_owned().into_bytes(),
            None => {
                let mut b = Vec::new();
                entry.read_to_end(&mut b).unwrap();
                b
            }
        };
        map.insert(path, (kind, uid, body));
    }
    map
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn copy_job_writes_context_files_as_root() {
    let (dir, exe, install, store) = store();
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(ctx.join("tree/sub")).unwrap();
    std::fs::write(ctx.join("hello.txt"), "hello\n").unwrap();
    std::fs::write(ctx.join("tree/sub/b.txt"), "b\n").unwrap();
    let job = Job::Copy(CopyJob {
        lower: busybox(&store),
        sources: vec!["hello.txt".into(), "tree".into()],
        dest: "/app/".into(),
        workdir: "/srv".into(),
    });
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let finished = vm.run(&job, Some(&ctx)).unwrap();
    assert_eq!(finished.status.exit_code, 0);
    let diff_id = finished.status.layer.clone().expect("a layer");
    let entries = layer_entries(&finished.out_dir());
    assert_eq!(
        entries["app/hello.txt"],
        (tar::EntryType::Regular, 0, b"hello\n".to_vec())
    );
    assert_eq!(entries["app/sub/b.txt"].2, b"b\n");
    assert!(
        entries.contains_key("srv"),
        "workdir is created: {:?}",
        entries.keys()
    );

    let bytes = std::fs::read(finished.out_dir().join(LAYER_FILE)).unwrap();
    assert_eq!(sandcastle::blobs::sha256(&bytes).to_string(), diff_id);
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn copy_job_missing_source_is_a_guest_error() {
    let (dir, exe, install, store) = store();
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(&ctx).unwrap();
    let job = Job::Copy(CopyJob {
        lower: busybox(&store),
        sources: vec!["nope.txt".into()],
        dest: "/x".into(),
        workdir: "/".into(),
    });
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let err = vm.run(&job, Some(&ctx)).err().unwrap();
    assert!(
        format!("{err:#}").contains("nope.txt: not found in the build context"),
        "{err:#}"
    );
}
