//! End-to-end `sandcastle build` tests. Run with `just it` (needs lib/, a
//! hypervisor and network). They inspect only the OCI layout on disk.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::read::GzDecoder;
use oci_spec::image::{ImageConfiguration, ImageIndex, ImageManifest};
use sha2::{Digest, Sha256};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/build")
        .join(name)
}

/// Runs `sandcastle build` with a store shared by all build tests, so base
/// images are pulled once per `just it` run.
fn build(context: &Path, dockerfile: Option<&Path>, out: &Path) -> Output {
    build_with(context, dockerfile, out, &[])
}

fn build_with(context: &Path, dockerfile: Option<&Path>, out: &Path, extra: &[&str]) -> Output {
    let bin = PathBuf::from(std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN"));
    let store = bin.parent().unwrap().join("store");
    let mut cmd = Command::new(&bin);
    cmd.args(["build", "-t", "demo", "-o"]).arg(out);
    if let Some(f) = dockerfile {
        cmd.arg("-f").arg(f);
    }
    let output = cmd
        .args(extra)
        .arg(context)
        .env("SANDCASTLE_ROOT", store)
        .output()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    output
}

struct Layout {
    dir: PathBuf,
    manifest: ImageManifest,
    config: ImageConfiguration,
}

impl Layout {
    fn open(dir: &Path) -> Self {
        let index: ImageIndex =
            serde_json::from_slice(&std::fs::read(dir.join("index.json")).unwrap()).unwrap();
        let desc = &index.manifests()[0];
        assert_eq!(
            desc.annotations().as_ref().unwrap()["org.opencontainers.image.ref.name"],
            "demo"
        );
        let manifest: ImageManifest =
            serde_json::from_slice(&blob(dir, desc.digest().digest())).unwrap();
        let config: ImageConfiguration =
            serde_json::from_slice(&blob(dir, manifest.config().digest().digest())).unwrap();
        Layout {
            dir: dir.to_path_buf(),
            manifest,
            config,
        }
    }

    /// Entries of layer `i`: path (trailing `/` trimmed) → (entry type, uid, contents or link target, mode).
    fn layer(&self, i: usize) -> BTreeMap<String, (tar::EntryType, u64, Vec<u8>, u32)> {
        let gz = blob(&self.dir, self.manifest.layers()[i].digest().digest());
        let mut tar_bytes = Vec::new();
        GzDecoder::new(&gz[..]).read_to_end(&mut tar_bytes).unwrap();
        let mut map = BTreeMap::new();
        for entry in tar::Archive::new(&tar_bytes[..]).entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_owned();
            let (kind, uid) = (entry.header().entry_type(), entry.header().uid().unwrap());
            let mode = entry.header().mode().unwrap();
            let body = match entry.link_name().unwrap() {
                Some(t) => t.to_string_lossy().into_owned().into_bytes(),
                None => {
                    let mut b = Vec::new();
                    entry.read_to_end(&mut b).unwrap();
                    b
                }
            };
            map.insert(path, (kind, uid, body, mode));
        }
        map
    }
}

fn blob(dir: &Path, hex: &str) -> Vec<u8> {
    std::fs::read(dir.join("blobs/sha256").join(hex)).unwrap()
}

fn hex_digest(bytes: &[u8]) -> String {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn alpine_build_produces_expected_image() {
    let out = tempfile::tempdir().unwrap();
    let output = build(&fixture("alpine"), None, out.path());
    assert!(output.status.success());
    let layout = Layout::open(out.path());
    let config = layout.config.config().as_ref().unwrap();

    assert_eq!(config.user().as_deref(), Some("builder"));
    assert_eq!(config.working_dir().as_deref(), Some("/app"));
    let env = config.env().as_ref().unwrap();
    assert!(env.contains(&"GREETING=hello world".to_string()), "{env:?}");
    assert!(
        env.iter().any(|e| e.starts_with("PATH=")),
        "base PATH kept: {env:?}"
    );
    assert_eq!(config.labels().as_ref().unwrap()["org.example.demo"], "yes");
    let mut ports = config.exposed_ports().clone().unwrap();
    ports.sort();
    assert_eq!(ports, ["53/udp", "8080/tcp"]);
    assert_eq!(
        config.entrypoint().as_deref(),
        Some(&["/bin/sh".to_string(), "-c".to_string()][..])
    );
    assert_eq!(
        config.cmd().as_deref(),
        Some(&["cat /app/greeting".to_string()][..])
    );

    // Every layer digest and diff_id recomputed from the bytes on disk.
    let diff_ids = layout.config.rootfs().diff_ids();
    assert_eq!(diff_ids.len(), layout.manifest.layers().len());
    for (desc, diff_id) in layout.manifest.layers().iter().zip(diff_ids) {
        let gz = blob(out.path(), desc.digest().digest());
        assert_eq!(hex_digest(&gz), desc.digest().to_string());
        let mut raw = Vec::new();
        GzDecoder::new(&gz[..]).read_to_end(&mut raw).unwrap();
        assert_eq!(&hex_digest(&raw), diff_id);
    }

    // 3 COPY + 3 RUN layers on top of the base; metadata steps add none.
    let base = layout.manifest.layers().len() - 6;
    let history = layout.config.history().as_ref().unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|h| !h.empty_layer().unwrap_or(false))
            .count(),
        base + 6
    );

    let hello = layout.layer(base);
    let hello_file = &hello["app/hello.txt"];
    assert_eq!(
        (hello_file.0, hello_file.1, &hello_file.2[..]),
        (tar::EntryType::Regular, 0, &b"hello from the context\n"[..])
    );
    let tree = layout.layer(base + 1);
    assert_eq!(tree["app/data/a.txt"].2, b"a\n");
    assert_eq!(tree["app/data/sub/b.txt"].2, b"b\n");
    assert_eq!(
        tree["app/data/run.sh"].3 & 0o777,
        0o755,
        "COPY keeps the executable bit"
    );
    let confs = layout.layer(base + 2);
    assert!(confs.contains_key("etc/demo/one.conf") && confs.contains_key("etc/demo/two.conf"));
    let run = layout.layer(base + 3);
    assert!(run.contains_key("etc/.wh.motd"), "{:?}", run.keys());
    assert_eq!(run["app/greeting"].2, b"hello world\n");
    let link = &run["app/link"];
    assert_eq!(
        (link.0, link.1, &link.2[..]),
        (tar::EntryType::Symlink, 0, &b"greeting"[..])
    );
    assert!(layout.layer(base + 4).contains_key("sbin/tini"));
    assert_eq!(layout.layer(base + 5)["tmp/uid"].2, b"1234\n");
    for i in base..base + 6 {
        let entries = layout.layer(i);
        for stub in [
            "etc/resolv.conf",
            "etc/hosts",
            "etc/hostname",
            "dev",
            "proc",
            "sys",
        ] {
            assert!(
                !entries
                    .keys()
                    .any(|k| k == stub || k.starts_with(&format!("{stub}/"))),
                "{stub} in layer {i}"
            );
        }
    }
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn debian_build_resolves_users_and_groups() {
    let out = tempfile::tempdir().unwrap();
    let output = build(&fixture("debian"), None, out.path());
    assert!(output.status.success());
    let layout = Layout::open(out.path());
    let last = layout.layer(layout.manifest.layers().len() - 1);
    assert_eq!(
        last["tmp/id"].2,
        b"uid=1500(app) gid=1500(app) groups=1500(app),2000(grp)\n"
    );
    assert_eq!(last["tmp/pwd"].2, b"/home/app\n");
    assert_eq!(last["tmp/home"].2, b"/home/app\n");
    let first = layout.layer(layout.manifest.layers().len() - 2);
    assert!(first.contains_key("etc/.wh.debian_version"));
}

/// Builds `dockerfile` with `ctx` as the context.
fn build_dockerfile(ctx: &Path, dockerfile: &str, extra: &[&str]) -> Output {
    let path = ctx.join("Dockerfile");
    std::fs::write(&path, dockerfile).unwrap();
    build_with(ctx, Some(&path), &ctx.join("out"), extra)
}

fn failing_build(body: &str) -> String {
    let ctx = tempfile::tempdir().unwrap();
    let output = build_dockerfile(
        ctx.path(),
        &format!("FROM mirror.gcr.io/library/alpine:3.20\n{body}"),
        &[],
    );
    assert!(!output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn failing_steps_report_their_exit_code() {
    let stderr = failing_build("RUN exit 3\n");
    assert!(
        stderr.contains("step 2/2 RUN exit 3: exited with 3"),
        "{stderr}"
    );
    assert!(failing_build("RUN definitely-not-a-command\n").contains("exited with 127"));
    assert!(failing_build("RUN [\"/no/such/binary\"]\n").contains("exited with 127"));
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn unknown_user_fails_step() {
    let stderr = failing_build("USER nosuchuser\nRUN true\n");
    assert!(
        stderr.contains("unable to find user nosuchuser"),
        "{stderr}"
    );
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn timings_and_trace_show_guest_phases() {
    let ctx = tempfile::tempdir().unwrap();
    let dockerfile = ctx.path().join("Dockerfile");
    std::fs::write(
        &dockerfile,
        "FROM mirror.gcr.io/library/alpine:3.20\nRUN echo one >/one && echo two\n",
    )
    .unwrap();
    let trace = ctx.path().join("trace.json");
    let output = build_with(
        ctx.path(),
        Some(&dockerfile),
        &ctx.path().join("out"),
        &["--timings", "--trace", trace.to_str().unwrap()],
    );
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[2/2] done in "), "{stderr}");
    for phase in [
        "kernel boot",
        "vmm",
        "(spawn ",
        "· load ",
        "· create ",
        "· teardown ",
        "command",
        "commit",
        "ingest",
    ] {
        assert!(stderr.contains(phase), "{phase} missing: {stderr}");
    }
    assert!(stderr.contains("Steps by duration:"), "{stderr}");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
    let names: Vec<String> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    for n in [
        "build",
        "pull",
        "vm",
        "kernel boot",
        "command",
        "cmd: echo two",
        "ingest",
        "write layout",
    ] {
        assert!(names.iter().any(|x| x == n), "{n} missing: {names:?}");
    }
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn failing_chain_names_its_command() {
    let stderr = failing_build("RUN true && printf '\\033[2J' >/dev/null && false && true\n");
    assert!(
        stderr.contains("exited with 1; last command started: false ("),
        "{stderr}"
    );
    assert!(stderr.contains("s into the step)"), "{stderr}");
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn failing_command_with_an_escape_is_shown_escaped() {
    // The shell traces `test` with the raw ESC from `$e` in its arguments.
    let stderr = failing_build("RUN e=$(printf '\\033[2J') && test \"$e\" = x\n");
    assert!(
        stderr.contains("exited with 1; last command started: test "),
        "{stderr}"
    );
    assert!(stderr.contains("\\x1b[2J"), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "raw escape in output");
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn bash_as_bin_sh_is_traced() {
    let ctx = tempfile::tempdir().unwrap();
    let output = build_dockerfile(
        ctx.path(),
        "FROM mirror.gcr.io/library/bash:5\n\
         RUN ln -sf /usr/local/bin/bash /bin/sh\n\
         RUN true && false && true\n",
        &[],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exited with 1; last command started: false ("),
        "{stderr}"
    );
    assert!(
        !stderr.lines().any(|l| l.trim_end() == "+ true"),
        "trace lines leaked into the log"
    );
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn nested_shell_traces_stay_in_the_log() {
    let ctx = tempfile::tempdir().unwrap();
    let output = build_dockerfile(
        ctx.path(),
        "FROM mirror.gcr.io/library/alpine:3.20\nRUN sh -c 'set -x; echo nested'\n",
        &[],
    );
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.lines().any(|l| l.trim_end() == "+ echo nested"),
        "{stderr}"
    );
}

#[test]
#[ignore = "needs bundled libkrun, a hypervisor and network; run with `just it`"]
fn failed_build_still_reports_timings_and_trace() {
    let ctx = tempfile::tempdir().unwrap();
    let trace = ctx.path().join("trace.json");
    let output = build_dockerfile(
        ctx.path(),
        "FROM mirror.gcr.io/library/alpine:3.20\nRUN true\nRUN true && false\n",
        &["--timings", "--trace", trace.to_str().unwrap()],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = stderr
        .split("Steps by duration:")
        .nth(1)
        .unwrap_or_else(|| panic!("no summary: {stderr}"));
    for step in [
        "s  step 1/3 FROM ",
        "s  step 2/3 RUN true",
        "s  step 3/3 RUN true && false",
    ] {
        assert!(summary.contains(step), "{step} missing: {stderr}");
    }
    let failing = stderr.split("[3/3] RUN true && false").nth(1).unwrap();
    assert!(failing.contains("· command "), "{stderr}");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    for n in ["command", "cmd: false"] {
        assert!(names.contains(&n), "{n} missing: {names:?}");
    }
}
