//! Typed instructions, following the argument and flag rules of BuildKit's
//! `frontend/dockerfile/instructions/parse.go`.

use std::fmt;

use crate::parser::Node;

/// A command line for RUN, CMD or ENTRYPOINT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Shell form: run through the image's shell.
    Shell(String),
    /// Exec form (JSON array): argv as written.
    Exec(Vec<String>),
}

/// A builder flag, checked against the flags BuildKit accepts for the
/// instruction. `name` has no leading `--`; a bare boolean flag has value
/// `"true"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub name: String,
    pub value: String,
}

/// `key=value` from ENV or LABEL. Both sides are unexpanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

/// A Dockerfile instruction. Arguments are unexpanded: apply
/// [`crate::expand`] where Docker expands variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    From {
        image: String,
        stage: Option<String>,
        flags: Vec<Flag>,
    },
    Run {
        command: Command,
        flags: Vec<Flag>,
    },
    Copy {
        sources: Vec<String>,
        dest: String,
        flags: Vec<Flag>,
    },
    Env(Vec<KeyValue>),
    Label(Vec<KeyValue>),
    Workdir(String),
    User(String),
    /// Ports sorted as BuildKit sorts them.
    Expose(Vec<String>),
    Cmd(Command),
    Entrypoint(Command),
    /// A valid instruction without a typed form here (ADD, ARG, HEALTHCHECK,
    /// MAINTAINER, ONBUILD, SHELL, STOPSIGNAL, VOLUME).
    Other(Node),
}

/// Why a node is not a valid instruction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {kind}")]
pub struct InstructionError {
    line: usize,
    kind: InstructionErrorKind,
}

impl InstructionError {
    /// 1-based first line of the instruction.
    pub fn line(&self) -> usize {
        self.line
    }

    pub fn kind(&self) -> &InstructionErrorKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstructionErrorKind {
    UnknownInstruction(String),
    AtLeastOneArgument(&'static str),
    ExactlyOneArgument(&'static str),
    NoDestination(&'static str),
    FromArguments,
    InvalidStageName(String),
    BlankName(&'static str),
    UnknownFlag(String),
    DuplicateFlag(String),
    MissingFlagValue(String),
    NotBoolean { flag: String, value: String },
}

impl fmt::Display for InstructionErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownInstruction(cmd) => write!(f, "unknown instruction: {cmd}"),
            Self::AtLeastOneArgument(cmd) => write!(f, "{cmd} requires at least one argument"),
            Self::ExactlyOneArgument(cmd) => write!(f, "{cmd} requires exactly one argument"),
            Self::NoDestination(cmd) => write!(
                f,
                "{cmd} requires at least two arguments, but only one was provided. Destination could not be determined"
            ),
            Self::FromArguments => f.write_str("FROM requires either one or three arguments"),
            Self::InvalidStageName(name) => write!(
                f,
                "invalid name for build stage: {name:?}, name can't start with a number or contain symbols"
            ),
            Self::BlankName(cmd) => write!(f, "{cmd} names can not be blank"),
            Self::UnknownFlag(flag) => write!(f, "unknown flag: {flag}"),
            Self::DuplicateFlag(flag) => write!(f, "duplicate flag specified: {flag}"),
            Self::MissingFlagValue(flag) => write!(f, "missing a value on flag: {flag}"),
            Self::NotBoolean { flag, value } => {
                write!(f, "expecting boolean value for flag {flag}, not: {value}")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FlagKind {
    String,
    Bool,
    /// May repeat.
    Strings,
}

const FROM_FLAGS: &[(&str, FlagKind)] = &[("platform", FlagKind::String)];
const RUN_FLAGS: &[(&str, FlagKind)] = &[
    ("mount", FlagKind::Strings),
    ("network", FlagKind::String),
    ("security", FlagKind::String),
    ("device", FlagKind::Strings),
];
const COPY_FLAGS: &[(&str, FlagKind)] = &[
    ("chown", FlagKind::String),
    ("from", FlagKind::String),
    ("chmod", FlagKind::String),
    ("link", FlagKind::Bool),
    ("exclude", FlagKind::Strings),
    ("parents", FlagKind::Bool),
];

impl TryFrom<&Node> for Instruction {
    type Error = InstructionError;

    fn try_from(node: &Node) -> Result<Self, Self::Error> {
        typed(node).map_err(|kind| InstructionError {
            line: node.start_line,
            kind,
        })
    }
}

fn typed(node: &Node) -> Result<Instruction, InstructionErrorKind> {
    use InstructionErrorKind as E;
    let args = &node.args;
    Ok(match node.cmd.to_ascii_lowercase().as_str() {
        "from" => {
            let flags = parse_flags(&node.flags, FROM_FLAGS)?;
            let stage = match args.as_slice() {
                [_] => None,
                [_, as_kw, name] if as_kw.eq_ignore_ascii_case("as") => {
                    let stage = name.to_lowercase();
                    if !is_valid_stage_name(&stage) {
                        return Err(E::InvalidStageName(name.clone()));
                    }
                    Some(stage)
                }
                _ => return Err(E::FromArguments),
            };
            Instruction::From {
                image: args[0].clone(),
                stage,
                flags,
            }
        }
        "run" => Instruction::Run {
            flags: parse_flags(&node.flags, RUN_FLAGS)?,
            command: command(node),
        },
        "copy" => {
            let flags = parse_flags(&node.flags, COPY_FLAGS)?;
            let Some((dest, sources)) = args.split_last().filter(|(_, s)| !s.is_empty()) else {
                return Err(E::NoDestination("COPY"));
            };
            Instruction::Copy {
                sources: sources.to_vec(),
                dest: dest.clone(),
                flags,
            }
        }
        "env" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Env(key_values(args, "ENV")?)
        }
        "label" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Label(key_values(args, "LABEL")?)
        }
        "workdir" => Instruction::Workdir(exactly_one(node, "WORKDIR")?),
        "user" => Instruction::User(exactly_one(node, "USER")?),
        "expose" => {
            if args.is_empty() {
                return Err(E::AtLeastOneArgument("EXPOSE"));
            }
            parse_flags(&node.flags, &[])?;
            let mut ports = args.clone();
            ports.sort();
            Instruction::Expose(ports)
        }
        "cmd" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Cmd(command(node))
        }
        "entrypoint" => {
            parse_flags(&node.flags, &[])?;
            Instruction::Entrypoint(command(node))
        }
        "add" | "arg" | "healthcheck" | "maintainer" | "onbuild" | "shell" | "stopsignal"
        | "volume" => Instruction::Other(node.clone()),
        _ => return Err(E::UnknownInstruction(node.cmd.clone())),
    })
}

fn command(node: &Node) -> Command {
    if node.json {
        Command::Exec(node.args.clone())
    } else {
        Command::Shell(node.args.join(" "))
    }
}

fn exactly_one(node: &Node, cmd: &'static str) -> Result<String, InstructionErrorKind> {
    let [arg] = node.args.as_slice() else {
        return Err(InstructionErrorKind::ExactlyOneArgument(cmd));
    };
    parse_flags(&node.flags, &[])?;
    Ok(arg.clone())
}

/// `[key, value, sep]` triples from the parser.
fn key_values(args: &[String], cmd: &'static str) -> Result<Vec<KeyValue>, InstructionErrorKind> {
    if args.is_empty() {
        return Err(InstructionErrorKind::AtLeastOneArgument(cmd));
    }
    args.chunks(3)
        .map(|kv| match kv {
            [key, ..] if key.is_empty() => Err(InstructionErrorKind::BlankName(cmd)),
            [key, value, _] => Ok(KeyValue {
                key: key.clone(),
                value: value.clone(),
            }),
            _ => unreachable!("the parser emits complete key/value/separator triples"),
        })
        .collect()
}

/// BuildKit's `^[a-z][a-z0-9-_.]*$`.
fn is_valid_stage_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// BuildKit's `BFlags.Parse`.
fn parse_flags(
    raw: &[String],
    allowed: &[(&str, FlagKind)],
) -> Result<Vec<Flag>, InstructionErrorKind> {
    use InstructionErrorKind as E;
    let mut flags: Vec<Flag> = Vec::with_capacity(raw.len());
    for arg in raw {
        let (flag_name, value) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg.as_str(), None),
        };
        let name = flag_name.strip_prefix("--").unwrap_or(flag_name);
        let Some(&(_, kind)) = allowed.iter().find(|(n, _)| *n == name) else {
            return Err(E::UnknownFlag(flag_name.to_owned()));
        };
        if kind != FlagKind::Strings && flags.iter().any(|f| f.name == name) {
            return Err(E::DuplicateFlag(flag_name.to_owned()));
        }
        let value = match (kind, value) {
            (FlagKind::Bool, Some("")) | (FlagKind::String | FlagKind::Strings, None) => {
                return Err(E::MissingFlagValue(flag_name.to_owned()));
            }
            (FlagKind::Bool, None) => "true".to_owned(),
            (FlagKind::Bool, Some(v)) => match v.to_ascii_lowercase().as_str() {
                "true" => "true".to_owned(),
                "false" => "false".to_owned(),
                _ => {
                    return Err(E::NotBoolean {
                        flag: flag_name.to_owned(),
                        value: v.to_owned(),
                    });
                }
            },
            (_, Some(v)) => v.to_owned(),
        };
        flags.push(Flag {
            name: name.to_owned(),
            value,
        });
    }
    Ok(flags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn one(src: &str) -> Result<Instruction, InstructionError> {
        let df = parse(src).expect("parses");
        Instruction::try_from(df.nodes.last().expect("one node"))
    }

    fn flag(name: &str, value: &str) -> Flag {
        Flag {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn from_with_stage_and_platform() {
        assert_eq!(
            one("FROM --platform=linux/arm64 alpine:3.20 AS Build").unwrap(),
            Instruction::From {
                image: "alpine:3.20".into(),
                stage: Some("build".into()),
                flags: vec![flag("platform", "linux/arm64")]
            }
        );
        assert_eq!(
            one("FROM a b").unwrap_err().kind(),
            &InstructionErrorKind::FromArguments
        );
        assert_eq!(
            one("FROM a AS 1x").unwrap_err().kind(),
            &InstructionErrorKind::InvalidStageName("1x".into())
        );
    }

    #[test]
    fn run_cmd_entrypoint_shell_and_exec_forms() {
        assert_eq!(
            one("RUN apt-get update && echo \"hi\"").unwrap(),
            Instruction::Run {
                command: Command::Shell("apt-get update && echo \"hi\"".into()),
                flags: vec![]
            }
        );
        assert_eq!(
            one("CMD [\"a\", \"b c\"]").unwrap(),
            Instruction::Cmd(Command::Exec(vec!["a".into(), "b c".into()]))
        );
        assert_eq!(
            one("ENTRYPOINT []").unwrap(),
            Instruction::Entrypoint(Command::Exec(vec![]))
        );
        assert_eq!(
            one("CMD").unwrap(),
            Instruction::Cmd(Command::Shell(String::new()))
        );
    }

    #[test]
    fn copy_needs_a_destination_and_known_flags() {
        assert_eq!(
            one("COPY --link --chown=1:1 a b /dst/").unwrap(),
            Instruction::Copy {
                sources: vec!["a".into(), "b".into()],
                dest: "/dst/".into(),
                flags: vec![flag("link", "true"), flag("chown", "1:1")]
            }
        );
        assert_eq!(
            one("COPY [\"a b\", \"/c\"]").unwrap(),
            Instruction::Copy {
                sources: vec!["a b".into()],
                dest: "/c".into(),
                flags: vec![]
            }
        );
        assert_eq!(
            one("COPY a").unwrap_err().kind(),
            &InstructionErrorKind::NoDestination("COPY")
        );
        assert_eq!(
            one("COPY --mode=1 a b").unwrap_err().kind(),
            &InstructionErrorKind::UnknownFlag("--mode".into())
        );
        assert_eq!(
            one("COPY --from a b").unwrap_err().kind(),
            &InstructionErrorKind::MissingFlagValue("--from".into())
        );
        assert_eq!(
            one("COPY --link=maybe a b").unwrap_err().kind(),
            &InstructionErrorKind::NotBoolean {
                flag: "--link".into(),
                value: "maybe".into()
            }
        );
        assert_eq!(
            one("COPY --chown=1 --chown=2 a b").unwrap_err().kind(),
            &InstructionErrorKind::DuplicateFlag("--chown".into())
        );
    }

    #[test]
    fn env_and_label_pairs_stay_unexpanded() {
        assert_eq!(
            one("ENV A=\"x y\" B=$A").unwrap(),
            Instruction::Env(vec![
                KeyValue {
                    key: "A".into(),
                    value: "\"x y\"".into()
                },
                KeyValue {
                    key: "B".into(),
                    value: "$A".into()
                },
            ])
        );
        assert_eq!(
            one("LABEL k some value").unwrap(),
            Instruction::Label(vec![KeyValue {
                key: "k".into(),
                value: "some value".into()
            }])
        );
        assert_eq!(
            one("ENV =x").unwrap_err().kind(),
            &InstructionErrorKind::BlankName("ENV")
        );
        assert_eq!(
            one("ENV").unwrap_err().kind(),
            &InstructionErrorKind::AtLeastOneArgument("ENV")
        );
        assert_eq!(
            one("ENV --x=1 A=b").unwrap_err().kind(),
            &InstructionErrorKind::UnknownFlag("--x".into())
        );
    }

    #[test]
    fn single_argument_instructions() {
        assert_eq!(
            one("WORKDIR /a b").unwrap(),
            Instruction::Workdir("/a b".into())
        );
        assert_eq!(one("USER 0").unwrap(), Instruction::User("0".into()));
        assert_eq!(
            one("USER").unwrap_err().kind(),
            &InstructionErrorKind::ExactlyOneArgument("USER")
        );
        assert_eq!(
            one("EXPOSE 80 443/tcp 8080").unwrap(),
            Instruction::Expose(vec!["443/tcp".into(), "80".into(), "8080".into()])
        );
        assert_eq!(
            one("EXPOSE").unwrap_err().kind(),
            &InstructionErrorKind::AtLeastOneArgument("EXPOSE")
        );
    }

    #[test]
    fn other_and_unknown_instructions() {
        assert!(matches!(one("VOLUME /data").unwrap(), Instruction::Other(n) if n.cmd == "VOLUME"));
        assert!(matches!(
            one("onbuild run x").unwrap(),
            Instruction::Other(_)
        ));
        let err = one("FROM a\nFROOM b").unwrap_err();
        assert_eq!(
            err.kind(),
            &InstructionErrorKind::UnknownInstruction("FROOM".into())
        );
        assert_eq!(err.line(), 2);
    }
}
