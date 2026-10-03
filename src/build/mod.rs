//! `sandcastle build`: pull the base, apply each instruction, run RUN and
//! COPY in microVMs, and write the image as an OCI layout.

pub mod config;
pub mod dns;
pub mod events;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use oci_spec::image::Descriptor;
use sandcastle_proto::{CopyJob, EVENTS_FILE, Job, LAYER_FILE, RunJob};

use crate::blobs::BlobStore;
use crate::dockerfile::{self, Step};
use crate::image::{self, ConfigState, Layer, LayerWriter};
use crate::install::{self, Install};
use crate::registry;
use crate::store::Store;
use crate::trace::{Span, Trace};
use crate::vm::{self, Finished, Resources, Vm};
use config::{Action, Stage};

/// Longest instruction text shown in progress and error messages.
const SHOWN_CHARS: usize = 60;

pub struct Options {
    pub dockerfile: PathBuf,
    pub context: PathBuf,
    pub output: PathBuf,
    pub tag: String,
    pub resources: Resources,
    /// Print each step's phases and a summary sorted by duration.
    pub timings: bool,
    /// Where to write a Chrome trace, also when the build fails.
    pub trace: Option<PathBuf>,
    /// Trace the commands of shell-form RUN with `set -x`.
    pub trace_run: bool,
}

/// Builds the image and returns its manifest descriptor in the layout.
pub fn build(exe: &Path, opts: &Options) -> Result<Descriptor> {
    let mut trace = Trace::new();
    let result = build_inner(exe, opts, &mut trace);
    let mut args = Vec::new();
    if let Err(e) = &result {
        args.push(("error".to_string(), format!("{e:#}")));
    }
    trace.push(Span {
        name: "build".into(),
        start: Duration::ZERO,
        dur: trace.origin().elapsed(),
        args,
    });
    if let Some(path) = &opts.trace
        && let Err(e) = std::fs::write(path, trace.to_chrome_json())
    {
        eprintln!(
            "sandcastle: warning: could not write trace {}: {e}",
            path.display()
        );
    }
    result
}

/// Records `f` as a span named `name`.
fn timed<T>(trace: &mut Trace, name: &str, f: impl FnOnce() -> T) -> T {
    let t0 = Instant::now();
    let out = f();
    trace.push(Span {
        name: name.into(),
        start: trace.at(t0),
        dur: t0.elapsed(),
        args: Vec::new(),
    });
    out
}

fn build_inner(exe: &Path, opts: &Options, trace: &mut Trace) -> Result<Descriptor> {
    // Unsupported instructions fail here, before any download or VM.
    let recipe = timed(trace, "parse", || dockerfile::load(&opts.dockerfile))?;
    let context = std::fs::canonicalize(&opts.context)
        .with_context(|| format!("build context {}", opts.context.display()))?;
    ensure!(
        context.is_dir(),
        "build context {} is not a directory",
        context.display()
    );
    let install = Install::locate(exe)?;
    #[cfg(target_os = "linux")]
    crate::doctor::check_kvm(Path::new("/dev/kvm"))?;
    let store = timed(trace, "store open", || {
        Store::open(&install::store_root()?, &install)
    })?;
    let blobs = store.blobs();

    let total = recipe.steps.len() + 1;
    eprintln!("[1/{total}] FROM {}", recipe.base);
    let t0 = Instant::now();
    let image = timed(trace, "pull", || registry::pull(&blobs, &recipe.base))?;
    let pulled = t0.elapsed();
    eprintln!("[1/{total}] done in {:.2}s", pulled.as_secs_f64());
    let mut step_times = vec![(
        format!("step 1/{total} FROM {}", shown(&recipe.base)),
        pulled,
    )];
    let mut stage = Stage::new(
        ConfigState::from_base(&image.config, image.layers)?,
        recipe.escape,
    );
    let vm = Vm {
        exe,
        install: &install,
        store: &store,
        resources: opts.resources,
    };
    let resolv_conf = dns::resolv_conf();
    let env = StepEnv {
        vm: &vm,
        blobs: &blobs,
        context: &context,
        resolv_conf: &resolv_conf,
        opts,
    };
    for (i, step) in recipe.steps.iter().enumerate() {
        let n = i + 2;
        let label = format!("step {n}/{total} {}", shown(&step.text));
        eprintln!("[{n}/{total}] {}", shown(&step.text));
        let t0 = Instant::now();
        let result = run_step(&env, trace, &mut stage, step, (n, total));
        let dur = t0.elapsed();
        trace.push(Span {
            name: label.clone(),
            start: trace.at(t0),
            dur,
            args: Vec::new(),
        });
        step_times.push((label.clone(), dur));
        if let Err(e) = result {
            if opts.timings {
                print_summary(step_times);
            }
            return Err(e.context(label));
        }
    }
    let descriptor = timed(trace, "write layout", || {
        image::write_layout(&blobs, &stage.state, &opts.output, &opts.tag)
    })?;
    if opts.timings {
        print_summary(step_times);
    }
    Ok(descriptor)
}

/// Prints the step durations, longest first.
fn print_summary(mut steps: Vec<(String, Duration)>) {
    steps.sort_by_key(|(_, dur)| std::cmp::Reverse(*dur));
    eprintln!("Steps by duration:");
    for (label, dur) in steps {
        eprintln!("  {:>8.2}s  {label}", dur.as_secs_f64());
    }
}

/// What every step needs besides the stage being built.
struct StepEnv<'a> {
    vm: &'a Vm<'a>,
    blobs: &'a BlobStore<'a>,
    context: &'a Path,
    resolv_conf: &'a str,
    opts: &'a Options,
}

fn run_step(
    env: &StepEnv<'_>,
    trace: &mut Trace,
    stage: &mut Stage,
    step: &Step,
    (n, total): (usize, usize),
) -> Result<()> {
    let step_start = trace.at(Instant::now());
    let (job, ctx, is_copy, shell_form) = match stage.apply(step)? {
        Action::Metadata => return stage.state.add_empty(&step.text),
        Action::Run { argv, shell_form } => (
            Job::Run(RunJob {
                lower: stage.state.lower_layers()?,
                argv,
                env: stage.run_env(),
                user: stage.user(),
                workdir: stage.workdir(),
                resolv_conf: env.resolv_conf.to_string(),
                shell_form: shell_form && env.opts.trace_run,
            }),
            None,
            false,
            shell_form,
        ),
        Action::Copy { sources, dest } => (
            Job::Copy(CopyJob {
                lower: stage.state.lower_layers()?,
                sources,
                dest,
                workdir: stage.workdir(),
            }),
            Some(env.context),
            true,
            false,
        ),
    };
    let finished = env.vm.run(&job, ctx)?;
    let vm_start = trace.at(finished.started);
    let vm_end = trace.at(finished.ended);
    let events = match events::read(&finished.out_dir().join(EVENTS_FILE)) {
        Ok(events) => events,
        Err(e) => {
            eprintln!("sandcastle: warning: step {n}/{total}: could not read guest events: {e:#}");
            None
        }
    };
    trace.push(Span {
        name: "vm".into(),
        start: vm_start,
        dur: vm_end.saturating_sub(vm_start),
        args: Vec::new(),
    });
    let timeline = vm_timeline(&finished, vm_start, vm_end);
    let guest = events
        .as_ref()
        .map(|ev| events::guest_spans(ev, &timeline))
        .unwrap_or_default();
    for span in &guest {
        trace.push(span.clone());
    }
    if let Some(ev) = events.as_ref().filter(|ev| ev.malformed > 0) {
        eprintln!(
            "sandcastle: warning: step {n}/{total}: skipped {} malformed guest event lines",
            ev.malformed
        );
    }

    let status = &finished.status;
    if status.exit_code != 0 {
        if env.opts.timings && events.is_some() {
            eprint!(
                "{}",
                phase_lines(&guest, Duration::ZERO, is_copy, shell_form)
            );
        }
        bail!(failure_message(
            status.exit_code,
            &guest,
            events.as_ref(),
            step_start
        ));
    }
    let mut ingested = Duration::ZERO;
    match &status.layer {
        None => stage.state.add_empty(&step.text)?,
        Some(diff_id) => {
            let t0 = Instant::now();
            let layer = ingest(env.blobs, &finished.out_dir().join(LAYER_FILE), diff_id)?;
            ingested = t0.elapsed();
            trace.push(Span {
                name: "ingest".into(),
                start: trace.at(t0),
                dur: ingested,
                args: Vec::new(),
            });
            stage.state.add_layer(layer, &step.text)?;
        }
    }
    eprintln!(
        "[{n}/{total}] done in {:.2}s",
        (trace.at(Instant::now()).saturating_sub(step_start)).as_secs_f64()
    );
    if env.opts.timings {
        eprint!("{}", phase_lines(&guest, ingested, is_copy, shell_form));
    }
    Ok(())
}

/// Why a step failed: its exit code and, for traced shell-form RUN, the
/// last command started and when, relative to `step_start`.
fn failure_message(
    exit_code: i32,
    guest: &[Span],
    events: Option<&events::GuestEvents>,
    step_start: Duration,
) -> String {
    let last = guest
        .iter()
        .rfind(|s| s.name.starts_with("cmd: "))
        .zip(events.and_then(|ev| ev.cmds.last()));
    match last {
        Some((span, cmd)) => format!(
            "exited with {exit_code}; last command started: {} ({:.1}s into the step)",
            events::display_safe(&cmd.text),
            span.start.saturating_sub(step_start).as_secs_f64()
        ),
        None => format!("exited with {exit_code}"),
    }
}

/// Formats one step's phase durations and, for traced shell-form RUN, its
/// slowest commands. `guest` holds guest-named spans; `ingested` is measured
/// on the host.
/// Places the child's wall-clock marks and the guest's `status.json` write
/// on the build timeline, relative to the VM process's spawn.
fn vm_timeline(finished: &Finished, vm_start: Duration, vm_end: Duration) -> events::VmTimeline {
    let at = |ns: u64| vm_start + Duration::from_nanos(ns.saturating_sub(finished.started_ns));
    events::VmTimeline {
        start: vm_start,
        end: vm_end,
        host: finished.marks.map(|m| events::HostMarks {
            main: at(m.main_ns),
            loaded: at(m.loaded_ns),
            configured: at(m.configured_ns),
            enter: at(m.enter_ns),
        }),
        helper_end: finished.helper_end_ns.map(at),
    }
}

/// Host-side VMM spans and their `--timings` labels, in order.
const VMM_PARTS: [(&str, &str); 4] = [
    ("process start", "spawn"),
    ("libkrun load", "load"),
    ("vm configure", "configure"),
    ("vm teardown", "teardown"),
];

fn phase_lines(guest: &[Span], ingested: Duration, is_copy: bool, shell_form: bool) -> String {
    let sum = |name: &str| -> f64 {
        guest
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.dur)
            .sum::<Duration>()
            .as_secs_f64()
    };
    let main = if is_copy { "copy" } else { "command" };
    // With the child's marks, VMM time is split into its parts.
    let parts = VMM_PARTS.map(|(name, label)| (label, sum(name)));
    let marked = guest
        .iter()
        .any(|s| VMM_PARTS.iter().any(|(n, _)| s.name == *n));
    let vmm = sum("vmm setup") + parts.iter().map(|(_, d)| d).sum::<f64>();
    let vmm = if marked {
        let detail: Vec<String> = parts.iter().map(|(l, d)| format!("{l} {d:.2}")).collect();
        format!("{vmm:.2}s ({})", detail.join(" · "))
    } else {
        format!("{vmm:.2}s")
    };
    // Measured only with the child's marks; see `events::guest_spans`.
    let early = if marked {
        format!("early boot {:.2}s · ", sum("early boot"))
    } else {
        String::new()
    };
    let mut out = format!(
        "  {early}kernel boot {:.2}s · vmm {vmm} · unpack {:.2}s · {main} {:.2}s · commit {:.2}s · ingest {:.2}s\n",
        sum("kernel boot"),
        sum("unpack"),
        sum(main),
        sum("commit"),
        ingested.as_secs_f64()
    );
    if shell_form {
        let mut cmds: Vec<&Span> = guest
            .iter()
            .filter(|s| s.name.starts_with("cmd: "))
            .collect();
        cmds.sort_by_key(|s| std::cmp::Reverse(s.dur));
        for s in cmds.iter().take(5) {
            out.push_str(&format!(
                "    {:.2}s  {}\n",
                s.dur.as_secs_f64(),
                events::display_safe(&s.name["cmd: ".len()..])
            ));
        }
    }
    out
}

/// Gzips the guest's layer tar into the blob store and checks that the
/// host and the guest agree on its diff_id.
fn ingest(blobs: &BlobStore<'_>, path: &Path, diff_id: &str) -> Result<Layer> {
    let mut file =
        vm::open_guest_file(path)?.context("the guest reported a layer but wrote no layer.tar")?;
    let mut writer = LayerWriter::new(blobs)?;
    io::copy(&mut file, &mut writer).context("compressing the layer")?;
    let layer = writer.finish()?;
    ensure!(
        layer.diff_id.to_string() == diff_id,
        "layer digest mismatch: the guest reported {diff_id}, the host computed {}",
        layer.diff_id
    );
    Ok(layer)
}

fn shown(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > SHOWN_CHARS {
        format!("{}…", line.chars().take(SHOWN_CHARS).collect::<String>())
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &str, ms: u64) -> Span {
        Span {
            name: name.into(),
            start: Duration::ZERO,
            dur: Duration::from_millis(ms),
            args: Vec::new(),
        }
    }

    #[test]
    fn missing_phases_print_zero_and_slowest_commands_lead() {
        let guest = [
            span("kernel boot", 1500),
            span("command", 3000),
            span("cmd: fast", 10),
            span("cmd: slow", 2000),
        ];
        let out = phase_lines(&guest, Duration::from_millis(250), false, true);
        assert_eq!(
            out,
            "  kernel boot 1.50s · vmm 0.00s · unpack 0.00s · command 3.00s · commit 0.00s · ingest 0.25s\n    2.00s  slow\n    0.01s  fast\n"
        );
    }

    #[test]
    fn vmm_figure_lists_its_parts_when_marked() {
        let guest = [
            span("process start", 10),
            span("libkrun load", 40),
            span("vm configure", 5),
            span("early boot", 170),
            span("kernel boot", 120),
            span("vm teardown", 15),
            span("command", 3000),
        ];
        let out = phase_lines(&guest, Duration::ZERO, false, false);
        assert!(
            out.starts_with(
                "  early boot 0.17s · kernel boot 0.12s · vmm 0.07s (spawn 0.01 · load 0.04 · configure 0.01 · teardown 0.01)"
            ),
            "{out}"
        );
    }

    /// Guest events of a step whose VM ran from 100ms to 300ms on the host
    /// clock, with `cmds` as `(text, start_us)`.
    fn step_events(cmds: &[(&str, u64)]) -> (Vec<Span>, events::GuestEvents) {
        let mut input = String::from(
            "{\"type\":\"phase\",\"name\":\"command\",\"start_us\":1000,\"dur_us\":9000}\n",
        );
        for (text, start_us) in cmds {
            let cmd = sandcastle_proto::Event::Cmd {
                text: (*text).into(),
                start_us: *start_us,
            };
            input.push_str(&serde_json::to_string(&cmd).unwrap());
            input.push('\n');
        }
        input.push_str("{\"type\":\"end\",\"at_us\":12000}\n");
        let ev = events::parse(input.as_bytes());
        let guest = events::guest_spans(
            &ev,
            &events::VmTimeline::new(Duration::from_millis(100), Duration::from_millis(300)),
        );
        (guest, ev)
    }

    #[test]
    fn failure_names_the_last_command_and_its_offset() {
        let (guest, ev) = step_events(&[("true", 2000), ("false", 4000)]);
        // helper start = 300ms - 12ms; "false" starts 4ms later, 192ms
        // after a step that started at 100ms.
        assert_eq!(
            failure_message(1, &guest, Some(&ev), Duration::from_millis(100)),
            "exited with 1; last command started: false (0.2s into the step)"
        );
    }

    #[test]
    fn failure_without_commands_gives_the_exit_code() {
        // Exec-form and untraced RUN, and COPY, record no cmds.
        let (guest, ev) = step_events(&[]);
        assert_eq!(
            failure_message(3, &guest, Some(&ev), Duration::ZERO),
            "exited with 3"
        );
        assert_eq!(
            failure_message(127, &[], None, Duration::ZERO),
            "exited with 127"
        );
    }

    #[test]
    fn failure_escapes_the_command_text() {
        let (guest, ev) = step_events(&[("test \u{1b}[2J = x", 2000)]);
        let msg = failure_message(1, &guest, Some(&ev), Duration::ZERO);
        assert!(!msg.contains('\u{1b}'), "{msg:?}");
        assert!(
            msg.contains("last command started: test \\x1b[2J = x ("),
            "{msg}"
        );
    }

    #[test]
    fn copy_steps_show_copy_and_no_commands() {
        let guest = [span("copy", 40), span("cmd: x", 5)];
        let out = phase_lines(&guest, Duration::ZERO, true, false);
        assert!(
            out.contains("copy 0.04s") && !out.contains("command"),
            "{out}"
        );
        assert_eq!(out.lines().count(), 1);
    }
}
