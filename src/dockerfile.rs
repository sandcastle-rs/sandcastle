//! Reads a Dockerfile and checks it against what sandcastle can build
//! today, so unsupported instructions fail before anything is pulled.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sandcastle_dockerfile::{Instruction, Node, parse};

/// One instruction after `FROM`.
#[derive(Debug, Clone)]
pub struct Step {
    pub instruction: Instruction,
    /// The instruction as written, continuations joined (history, messages).
    pub text: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub base: String,
    pub escape: char,
    pub steps: Vec<Step>,
}

pub fn load(path: &Path) -> Result<Recipe> {
    let src = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    check(&src).with_context(|| path.display().to_string())
}

pub fn check(src: &str) -> Result<Recipe> {
    let dockerfile = parse(src)?;
    let mut nodes = dockerfile.nodes.iter();
    let first = nodes.next().context("the Dockerfile has no instructions")?;
    let line = first.start_line;
    let base = match Instruction::try_from(first)? {
        Instruction::From { stage: Some(_), .. } => {
            bail!("line {line}: multi-stage builds (FROM … AS) are not supported yet")
        }
        Instruction::From { flags, .. } if !flags.is_empty() => {
            bail!("line {line}: FROM --{} is not supported yet", flags[0].name)
        }
        Instruction::From { image, .. } if image.eq_ignore_ascii_case("scratch") => {
            bail!("line {line}: FROM scratch is not supported yet")
        }
        Instruction::From { image, .. } => image,
        _ => bail!("line {line}: the first instruction must be FROM"),
    };
    let steps = nodes
        .map(|node| {
            let instruction = Instruction::try_from(node)?;
            supported(&instruction, node)?;
            Ok(Step {
                instruction,
                text: node.original.clone(),
                line: node.start_line,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Recipe {
        base,
        escape: dockerfile.escape,
        steps,
    })
}

fn supported(instruction: &Instruction, node: &Node) -> Result<()> {
    let line = node.start_line;
    match instruction {
        Instruction::From { .. } => {
            bail!("line {line}: multi-stage builds (a second FROM) are not supported yet")
        }
        Instruction::Run { flags, .. } | Instruction::Copy { flags, .. } => match flags.first() {
            Some(flag) => bail!(
                "line {line}: {} --{} is not supported yet",
                node.cmd.to_ascii_uppercase(),
                flag.name
            ),
            None => Ok(()),
        },
        Instruction::Other(other) => {
            bail!(
                "line {line}: {} is not supported yet",
                other.cmd.to_ascii_uppercase()
            )
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> String {
        format!("{:#}", check(src).unwrap_err())
    }

    #[test]
    fn accepts_every_v1_instruction_in_any_case() {
        let recipe = check(
            "# syntax=docker/dockerfile:1\nFROM alpine:3.20\nenv A=1\nWORKDIR /app\nCOPY a b/\nrun echo hi\nUSER 1000\nLABEL x=y\nEXPOSE 80\nCMD [\"sh\"]\nENTRYPOINT [\"/bin/sh\",\"-c\"]\n",
        )
        .unwrap();
        assert_eq!(recipe.base, "alpine:3.20");
        assert_eq!(recipe.escape, '\\');
        assert_eq!(recipe.steps.len(), 9);
        assert_eq!(recipe.steps[3].text, "run echo hi");
        assert_eq!(recipe.steps[3].line, 6);
    }

    #[test]
    fn rejects_out_of_scope_instructions_with_lines() {
        assert_eq!(err("FROM a\nARG X\n"), "line 2: ARG is not supported yet");
        assert_eq!(
            err("FROM a\nonbuild RUN x\n"),
            "line 2: ONBUILD is not supported yet"
        );
        assert_eq!(
            err("FROM a\nRUN --mount=type=cache,target=/x true\n"),
            "line 2: RUN --mount is not supported yet"
        );
        assert_eq!(
            err("FROM a\nCOPY --from=b x y\n"),
            "line 2: COPY --from is not supported yet"
        );
        assert_eq!(
            err("FROM a AS b\n"),
            "line 1: multi-stage builds (FROM … AS) are not supported yet"
        );
        assert_eq!(
            err("FROM --platform=linux/amd64 a\n"),
            "line 1: FROM --platform is not supported yet"
        );
        assert_eq!(
            err("FROM a\nFROM b\n"),
            "line 2: multi-stage builds (a second FROM) are not supported yet"
        );
        assert_eq!(
            err("FROM scratch\n"),
            "line 1: FROM scratch is not supported yet"
        );
    }

    #[test]
    fn first_instruction_must_be_from() {
        assert_eq!(
            err("ENV A=1\nFROM a\n"),
            "line 1: the first instruction must be FROM"
        );
        assert_eq!(
            err("# only a comment\n"),
            "the Dockerfile has no instructions"
        );
    }

    #[test]
    fn parse_and_instruction_errors_keep_their_line() {
        assert!(
            err("FROM a\nENV\n").starts_with("line 2:"),
            "{}",
            err("FROM a\nENV\n")
        );
        assert!(
            err("FROM a\nBOGUS x\n").starts_with("line 2:"),
            "{}",
            err("FROM a\nBOGUS x\n")
        );
    }
}
