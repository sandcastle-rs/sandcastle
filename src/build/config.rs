//! Applies Dockerfile instructions to the image config with Docker's
//! variable expansion rules, and turns RUN/COPY into job inputs.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use sandcastle_dockerfile::{Command, Instruction, expand};

use crate::dockerfile::Step;
use crate::image::ConfigState;

/// PATH for RUN when the image sets none (BuildKit's default).
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const PROTOCOLS: [&str; 3] = ["tcp", "udp", "sctp"];

/// The guest work a step needs, after expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Metadata,
    Run { argv: Vec<String> },
    Copy { sources: Vec<String>, dest: String },
}

pub struct Stage {
    pub state: ConfigState,
    escape: char,
    /// CMD was set in this Dockerfile, so ENTRYPOINT keeps it.
    cmd_set: bool,
}

impl Stage {
    pub fn new(state: ConfigState, escape: char) -> Self {
        Self {
            state,
            escape,
            cmd_set: false,
        }
    }

    pub fn apply(&mut self, step: &Step) -> Result<Action> {
        self.apply_inner(step)
            .with_context(|| format!("line {}", step.line))
    }

    fn apply_inner(&mut self, step: &Step) -> Result<Action> {
        let env = self.env_map();
        let escape = self.escape;
        let x = |word: &str| expand(word, &env, escape).map_err(anyhow::Error::from);
        match &step.instruction {
            Instruction::Env(pairs) => {
                for kv in pairs {
                    let (key, value) = (x(&kv.key)?, x(&kv.value)?);
                    set_env(&mut self.state.env, &key, &value);
                }
            }
            Instruction::Label(pairs) => {
                for kv in pairs {
                    let (key, value) = (x(&kv.key)?, x(&kv.value)?);
                    self.state.labels.insert(key, value);
                }
            }
            Instruction::Workdir(dir) => {
                let dir = x(dir)?;
                self.state.working_dir =
                    Some(join_workdir(self.state.working_dir.as_deref(), &dir));
            }
            Instruction::User(user) => self.state.user = Some(x(user)?),
            Instruction::Expose(ports) => {
                for port in ports {
                    self.state.exposed_ports.extend(parse_ports(&x(port)?)?);
                }
            }
            Instruction::Cmd(command) => {
                self.state.cmd = Some(argv(command));
                self.cmd_set = true;
            }
            Instruction::Entrypoint(command) => {
                self.state.entrypoint = Some(argv(command));
                if !self.cmd_set {
                    self.state.cmd = None;
                }
            }
            Instruction::Run { command, .. } => {
                return Ok(Action::Run {
                    argv: argv(command),
                });
            }
            Instruction::Copy { sources, dest, .. } => {
                let sources = sources.iter().map(|s| x(s)).collect::<Result<Vec<_>>>()?;
                let dest = x(dest)?;
                ensure!(
                    sources.len() == 1 || dest.ends_with('/'),
                    "When using COPY with more than one source file, the destination must be a directory and end with a /"
                );
                return Ok(Action::Copy { sources, dest });
            }
            Instruction::From { .. } | Instruction::Other(_) => {
                bail!("instruction is not supported here")
            }
        }
        Ok(Action::Metadata)
    }

    fn env_map(&self) -> HashMap<String, String> {
        self.state
            .env
            .iter()
            .filter_map(|e| e.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    pub fn run_env(&self) -> Vec<String> {
        let mut env = self.state.env.clone();
        if !env.iter().any(|e| e.starts_with("PATH=")) {
            env.push(format!("PATH={DEFAULT_PATH}"));
        }
        env
    }

    pub fn user(&self) -> String {
        self.state.user.clone().unwrap_or_default()
    }

    pub fn workdir(&self) -> String {
        self.state.working_dir.clone().unwrap_or_else(|| "/".into())
    }
}

fn argv(command: &Command) -> Vec<String> {
    match command {
        Command::Shell(cmd) => vec!["/bin/sh".into(), "-c".into(), cmd.clone()],
        Command::Exec(args) => args.clone(),
    }
}

fn set_env(env: &mut Vec<String>, key: &str, value: &str) {
    let entry = format!("{key}={value}");
    match env
        .iter_mut()
        .find(|e| e.split_once('=').is_some_and(|(k, _)| k == key))
    {
        Some(existing) => *existing = entry,
        None => env.push(entry),
    }
}

fn join_workdir(current: Option<&str>, dir: &str) -> String {
    if dir.starts_with('/') {
        clean(dir)
    } else {
        clean(&format!("{}/{dir}", current.unwrap_or("/")))
    }
}

/// Lexical clean of an absolute path, like Go's `path.Clean`.
fn clean(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

fn parse_ports(spec: &str) -> Result<Vec<String>> {
    let invalid = || anyhow::anyhow!("invalid port {spec}");
    let (ports, proto) = match spec.split_once('/') {
        Some((p, proto)) => (p, proto.to_ascii_lowercase()),
        None => (spec, "tcp".to_string()),
    };
    ensure!(PROTOCOLS.contains(&proto.as_str()), invalid());
    let (start, end) = match ports.split_once('-') {
        Some((a, b)) => (a, b),
        None => (ports, ports),
    };
    let start: u16 = start.parse().map_err(|_| invalid())?;
    let end: u16 = end.parse().map_err(|_| invalid())?;
    ensure!(start <= end, invalid());
    Ok((start..=end).map(|p| format!("{p}/{proto}")).collect())
}

#[cfg(test)]
mod tests {
    use oci_spec::image::{Arch, ImageConfigurationBuilder, Os, RootFsBuilder};

    use super::*;
    use crate::dockerfile::check;

    fn stage() -> Stage {
        let config = ImageConfigurationBuilder::default()
            .architecture(Arch::ARM64)
            .os(Os::Linux)
            .config(
                oci_spec::image::ConfigBuilder::default()
                    .env(vec!["PATH=/base/bin".to_string(), "HOME=/root".to_string()])
                    .cmd(vec!["sh".to_string()])
                    .build()
                    .unwrap(),
            )
            .rootfs(
                RootFsBuilder::default()
                    .typ("layers")
                    .diff_ids(Vec::<String>::new())
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();
        Stage::new(ConfigState::from_base(&config, vec![]).unwrap(), '\\')
    }

    /// Applies every step of `body` (after a FROM line) and returns the actions.
    fn apply(stage: &mut Stage, body: &str) -> Result<Vec<Action>> {
        let recipe = check(&format!("FROM base\n{body}")).unwrap();
        recipe.steps.iter().map(|s| stage.apply(s)).collect()
    }

    #[test]
    fn env_expands_against_the_env_before_the_instruction() {
        let mut s = stage();
        apply(&mut s, "ENV A=1\nENV A=2 B=$A PATH=$PATH:/x\n").unwrap();
        assert!(s.state.env.contains(&"A=2".to_string()));
        assert!(s.state.env.contains(&"B=1".to_string()));
        assert!(s.state.env.contains(&"PATH=/base/bin:/x".to_string()));
        assert_eq!(
            s.state.env.iter().filter(|e| e.starts_with("A=")).count(),
            1
        );
    }

    #[test]
    fn workdir_joins_and_cleans() {
        let mut s = stage();
        apply(&mut s, "ENV D=app\nWORKDIR /srv\nWORKDIR $D/../web/./\n").unwrap();
        assert_eq!(s.workdir(), "/srv/web");
        apply(&mut s, "WORKDIR /abs\n").unwrap();
        assert_eq!(s.workdir(), "/abs");
    }

    #[test]
    fn expose_normalises_ports_and_ranges() {
        let mut s = stage();
        apply(&mut s, "ENV P=8080\nEXPOSE $P 53/UDP 7000-7002/tcp\n").unwrap();
        let ports: Vec<&str> = s.state.exposed_ports.iter().map(String::as_str).collect();
        assert_eq!(
            ports,
            ["53/udp", "7000/tcp", "7001/tcp", "7002/tcp", "8080/tcp"]
        );
        let err = apply(&mut s, "EXPOSE 80/quic\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("line 2: invalid port 80/quic"),
            "{err:#}"
        );
    }

    #[test]
    fn cmd_entrypoint_and_run_argv() {
        let mut s = stage();
        let actions = apply(
            &mut s,
            "ENTRYPOINT [\"/entry\"]\nRUN echo $HOME\nRUN [\"a\", \"b\"]\n",
        )
        .unwrap();
        assert_eq!(s.state.cmd, None, "inherited CMD is cleared by ENTRYPOINT");
        assert_eq!(
            actions[1],
            Action::Run {
                argv: vec!["/bin/sh".into(), "-c".into(), "echo $HOME".into()]
            }
        );
        assert_eq!(
            actions[2],
            Action::Run {
                argv: vec!["a".into(), "b".into()]
            }
        );

        let mut s = stage();
        apply(&mut s, "CMD echo hi\nENTRYPOINT [\"/entry\"]\n").unwrap();
        assert_eq!(
            s.state.cmd,
            Some(vec!["/bin/sh".into(), "-c".into(), "echo hi".into()])
        );
    }

    #[test]
    fn copy_expands_and_checks_multi_source_dest() {
        let mut s = stage();
        let actions = apply(&mut s, "ENV F=a.txt\nCOPY $F /dst\n").unwrap();
        assert_eq!(
            actions[1],
            Action::Copy {
                sources: vec!["a.txt".into()],
                dest: "/dst".into()
            }
        );
        let err = apply(&mut s, "COPY a b /dst\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("must be a directory and end with a /"),
            "{err:#}"
        );
    }

    #[test]
    fn user_label_and_run_env() {
        let mut s = stage();
        apply(&mut s, "ENV U=app\nUSER $U:grp\nLABEL \"k $U\"=\"v $U\"\n").unwrap();
        assert_eq!(s.user(), "app:grp");
        assert_eq!(
            s.state.labels.get("k app").map(String::as_str),
            Some("v app")
        );
        let mut bare = stage();
        bare.state.env.retain(|e| !e.starts_with("PATH="));
        assert!(bare.run_env().contains(&format!("PATH={DEFAULT_PATH}")));
    }

    #[test]
    fn expansion_errors_name_the_line() {
        let mut s = stage();
        let err = apply(&mut s, "ENV A=1\nWORKDIR ${MISSING:?must be set}\n").unwrap_err();
        assert!(format!("{err:#}").starts_with("line 3:"), "{err:#}");
    }
}
