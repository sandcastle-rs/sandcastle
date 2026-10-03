//! VM integration tests. Run with `just it` (needs lib/ and a hypervisor).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use sandcastle::image::ConfigState;
use sandcastle::install::Install;
use sandcastle::registry;
use sandcastle::store::Store;
use sandcastle::vm::{Resources, Vm};
use sandcastle_proto::{CopyJob, EVENTS_FILE, Event, Job, LAYER_FILE, LowerLayer, RunJob, Status};

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
        let path = entry
            .path()
            .unwrap()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_owned();
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

/// Files in the COPY context; well above the 1024 soft fd limit forced below.
const MANY_FILES: usize = 3000;

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn copy_job_with_more_files_than_the_soft_fd_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    // libkrun's virtio-fs server keeps a host fd per guest inode; force the
    // common default soft limit so the test means the same on every machine.
    let limit = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(1024),
            maximum: limit.maximum,
        },
    )
    .unwrap();

    let (dir, exe, install, store) = store();
    let ctx = dir.path().join("ctx");
    for i in 0..MANY_FILES {
        let sub = ctx.join(format!("d{:02}", i % 30));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(format!("f{i:04}")), i.to_string()).unwrap();
    }
    let job = Job::Copy(CopyJob {
        lower: busybox(&store),
        sources: vec![".".into()],
        dest: "/data/".into(),
        workdir: "/".into(),
    });
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let finished = vm.run(&job, Some(&ctx)).unwrap();
    setrlimit(Resource::Nofile, limit).unwrap();
    assert_eq!(finished.status.exit_code, 0);
    let entries = layer_entries(&finished.out_dir());
    let files = entries
        .values()
        .filter(|(kind, ..)| *kind == tar::EntryType::Regular)
        .count();
    assert_eq!(files, MANY_FILES);
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

fn run_job(lower: Vec<LowerLayer>, script: &str, user: &str) -> Job {
    Job::Run(RunJob {
        lower,
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: vec!["PATH=/usr/sbin:/usr/bin:/sbin:/bin".into()],
        user: user.into(),
        workdir: "/work".into(),
        resolv_conf: "nameserver 8.8.8.8\n".into(),
        shell_form: false,
    })
}

/// Runs `job` and, if it made a layer, returns the lower stack with it on top.
fn run_step(
    vm: &Vm,
    store: &Store,
    lower: &[LowerLayer],
    job: &Job,
) -> (sandcastle::vm::Finished, Vec<LowerLayer>) {
    let finished = vm.run(job, None).unwrap();
    let mut stack = lower.to_vec();
    if let Some(diff_id) = &finished.status.layer {
        // Ingest the layer so later jobs can find it as a blob too.
        let blobs = store.blobs();
        let mut writer = sandcastle::image::LayerWriter::new(&blobs).unwrap();
        std::io::copy(
            &mut std::fs::File::open(finished.out_dir().join(LAYER_FILE)).unwrap(),
            &mut writer,
        )
        .unwrap();
        let layer = writer.finish().unwrap();
        assert_eq!(layer.diff_id.to_string(), *diff_id);
        stack.push(LowerLayer {
            diff_id: diff_id.clone(),
            blob: layer.descriptor.digest().to_string(),
            media_type: sandcastle_proto::LAYER_TAR_GZIP.into(),
        });
    }
    (finished, stack)
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_job_commits_changes_and_whiteouts() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let job = run_job(
        base.clone(),
        "echo hi > out.txt && rm /etc/group && ls /dev/null /proc/self >/dev/null",
        "",
    );
    let (finished, _) = run_step(&vm, &store, &base, &job);
    assert_eq!(finished.status.exit_code, 0);
    let entries = layer_entries(&finished.out_dir());
    assert_eq!(entries["work/out.txt"].2, b"hi\n");
    assert!(
        entries.contains_key("etc/.wh.group"),
        "{:?}",
        entries.keys()
    );
    for stub in ["etc/resolv.conf", "etc/hosts", "dev", "proc", "sys"] {
        assert!(
            !entries
                .keys()
                .any(|k| k == stub || k.starts_with(&format!("{stub}/"))),
            "{stub} leaked into the layer"
        );
    }
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_job_exit_codes_and_no_op_steps() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let status = vm
        .run(&run_job(base.clone(), "exit 3", ""), None)
        .unwrap()
        .status
        .clone();
    assert_eq!((status.exit_code, status.layer), (3, None));
    let mut job = run_job(base.clone(), "", "");
    if let Job::Run(r) = &mut job {
        r.argv = vec!["/no/such/binary".into()];
    }
    assert_eq!(vm.run(&job, None).unwrap().status.exit_code, 127);
    let mut noop = run_job(base, "true", "");
    if let Job::Run(r) = &mut noop {
        r.workdir = "/".into();
    }
    let status = vm.run(&noop, None).unwrap().status.clone();
    assert_eq!((status.exit_code, status.layer), (0, None));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_job_drops_to_user() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let job = run_job(
        base.clone(),
        "id -u > /tmp/uid; echo $HOME > /tmp/home",
        "65534",
    );
    let (finished, _) = run_step(&vm, &store, &base, &job);
    let entries = layer_entries(&finished.out_dir());
    assert_eq!(entries["tmp/uid"].2, b"65534\n");
    assert_eq!(entries["tmp/home"].2, b"/home\n");
    let err = vm.run(&run_job(base, "true", "ghost"), None).err().unwrap();
    assert!(
        format!("{err:#}").contains("unable to find user ghost"),
        "{err:#}"
    );
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_kills_leftover_processes() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let status = vm
        .run(
            &run_job(base, "sleep 1000 & echo started > /started", ""),
            None,
        )
        .unwrap()
        .status
        .clone();
    assert_eq!(status.exit_code, 0);
    assert!(status.layer.is_some());
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn opaque_dir_hides_lower_contents() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let (_, stack) = run_step(
        &vm,
        &store,
        &base,
        &run_job(base.clone(), "mkdir /d && touch /d/a /d/b", ""),
    );
    let (finished, stack) = run_step(
        &vm,
        &store,
        &stack,
        &run_job(stack.clone(), "rm -rf /d && mkdir /d && touch /d/c", ""),
    );
    assert!(layer_entries(&finished.out_dir()).contains_key("d/.wh..wh..opq"));
    let (finished, _) = run_step(
        &vm,
        &store,
        &stack,
        &run_job(stack.clone(), "ls /d > /listing", ""),
    );
    assert_eq!(layer_entries(&finished.out_dir())["listing"].2, b"c\n");
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_survives_resolv_conf_symlink() {
    // A RUN cannot replace /etc/resolv.conf (it is a bind mount during the
    // step), so the dangling link comes from a COPY layer, as from a base image.
    let (dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(ctx.join("root/etc")).unwrap();
    std::os::unix::fs::symlink("/nonexistent", ctx.join("root/etc/resolv.conf")).unwrap();
    let copy = Job::Copy(CopyJob {
        lower: base.clone(),
        sources: vec!["root".into()],
        dest: "/".into(),
        workdir: "/".into(),
    });
    let finished = vm.run(&copy, Some(&ctx)).unwrap();
    let mut stack = base;
    let diff_id = finished.status.layer.clone().unwrap();
    let blobs = store.blobs();
    let mut writer = sandcastle::image::LayerWriter::new(&blobs).unwrap();
    std::io::copy(
        &mut std::fs::File::open(finished.out_dir().join(LAYER_FILE)).unwrap(),
        &mut writer,
    )
    .unwrap();
    let layer = writer.finish().unwrap();
    stack.push(LowerLayer {
        diff_id,
        blob: layer.descriptor.digest().to_string(),
        media_type: sandcastle_proto::LAYER_TAR_GZIP.into(),
    });
    let status = vm
        .run(&run_job(stack, "echo ok > /ok", ""), None)
        .unwrap()
        .status
        .clone();
    assert_eq!(status.exit_code, 0);
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_creating_a_reserved_whiteout_name_fails_the_step() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let Err(err) = vm.run(&run_job(base, "touch /.wh.foo", ""), None) else {
        panic!("the step should fail");
    };
    assert!(
        format!("{err:#}").contains("names starting with .wh. are reserved for whiteouts"),
        "{err:#}"
    );
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn run_cannot_reach_the_store_disk() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let base = busybox(&store);
    let script = "mknod /tmp/vda b 254 0 2>/dev/null || echo blocked > /m1; \
                  mkdir -p /mnt; mount -t tmpfs none /mnt 2>/dev/null || echo blocked > /m2; \
                  touch /f && chown 65534 /f && echo ok > /m3; \
                  grep -c ' /store' /proc/self/mountinfo > /m4; \
                  test ! -e /store/layers && echo ok > /m6; \
                  echo x > /proc/sys/kernel/core_pattern 2>/dev/null || echo blocked > /m5";
    let (finished, _) = run_step(&vm, &store, &base, &run_job(base.clone(), script, ""));
    assert_eq!(finished.status.exit_code, 0);
    let entries = layer_entries(&finished.out_dir());
    for marker in ["m1", "m2", "m3", "m4", "m5", "m6"] {
        assert!(
            entries.contains_key(marker),
            "{marker} missing: {:?}",
            entries.keys()
        );
    }
    assert_eq!(entries["m1"].2, b"blocked\n");
    assert_eq!(entries["m2"].2, b"blocked\n");
    assert_eq!(
        entries["m4"].2, b"0\n",
        "the store is still mounted in the step"
    );
    assert_eq!(entries["m5"].2, b"blocked\n");
}

fn shell_job(lower: Vec<LowerLayer>, script: &str) -> Job {
    let Job::Run(mut run) = run_job(lower, script, "") else {
        unreachable!()
    };
    run.shell_form = true;
    Job::Run(run)
}

fn events(out_dir: &std::path::Path) -> Vec<Event> {
    std::fs::read_to_string(out_dir.join(EVENTS_FILE))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn cmds(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Cmd { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn traced_chain_reports_the_failing_command_last() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let finished = vm
        .run(
            &shell_job(
                busybox(&store),
                "printf 'a\\rb\\n' >&2 && true && false && true",
            ),
            None,
        )
        .unwrap();
    assert_eq!(finished.status.exit_code, 1);
    let ev = events(&finished.out_dir());
    assert_eq!(cmds(&ev).last().map(String::as_str), Some("false"));
    let phases: Vec<&str> = ev
        .iter()
        .filter_map(|e| match e {
            Event::Phase { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    for p in ["store mount", "overlay", "command", "unmount"] {
        assert!(phases.contains(&p), "{p} missing from {phases:?}");
    }
    assert!(matches!(ev.first(), Some(Event::Boot { .. })));
    assert!(matches!(ev.last(), Some(Event::End { .. })));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn forged_markers_and_set_plus_x() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let job = shell_job(
        busybox(&store),
        "echo '+sc-000000000000> forged' >&2 && true && set +x && false",
    );
    let finished = vm.run(&job, None).unwrap();
    let c = cmds(&events(&finished.out_dir()));
    assert!(
        !c.iter()
            .any(|t| t.contains("forged") && !t.starts_with("echo")),
        "{c:?}"
    );
    assert_eq!(c.last().map(String::as_str), Some("set +x"));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn exec_form_and_copy_jobs_still_record_phases() {
    let (dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let finished = vm.run(&run_job(busybox(&store), "true", ""), None).unwrap();
    assert!(
        cmds(&events(&finished.out_dir())).is_empty(),
        "exec-style job is not traced"
    );
    let ctx = dir.path().join("ctx");
    std::fs::create_dir_all(&ctx).unwrap();
    std::fs::write(ctx.join("a"), "a").unwrap();
    let copy = Job::Copy(CopyJob {
        lower: busybox(&store),
        sources: vec!["a".into()],
        dest: "/a".into(),
        workdir: "/".into(),
    });
    let finished = vm.run(&copy, Some(&ctx)).unwrap();
    let ev = events(&finished.out_dir());
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Phase { name, .. } if name == "copy"))
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Phase { name, .. } if name == "commit"))
    );
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn traced_run_finishes_with_a_leftover_background_process() {
    let (_dir, exe, install, store) = store();
    let vm = Vm {
        exe: &exe,
        install: &install,
        store: &store,
        resources: Resources::default(),
    };
    let job = shell_job(busybox(&store), "sleep 1000 & echo started > /started");
    let finished = vm.run(&job, None).unwrap();
    assert_eq!(finished.status.exit_code, 0);
    assert!(finished.status.layer.is_some());
}
