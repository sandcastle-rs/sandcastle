//! Parse stage recipes and validate the instructions understood by the planner.

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
pub struct StageRecipe {
    pub name: Option<String>,
    pub base: String,
    pub text: String,
    pub line: usize,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub escape: char,
    pub stages: Vec<StageRecipe>,
}

pub fn load(path: &Path) -> Result<Recipe> {
    let src = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let recipe = check(&src).with_context(|| path.display().to_string())?;
    single_stage(&recipe).with_context(|| path.display().to_string())?;
    Ok(recipe)
}

pub fn check(src: &str) -> Result<Recipe> {
    let dockerfile = parse(src)?;
    let mut stages: Vec<StageRecipe> = Vec::new();
    for node in &dockerfile.nodes {
        let instruction = Instruction::try_from(node)?;
        let line = node.start_line;
        if let Instruction::From {
            image,
            stage,
            flags,
        } = instruction
        {
            if let Some(flag) = flags.first() {
                bail!("line {line}: FROM --{} is not supported yet", flag.name);
            }
            stages.push(StageRecipe {
                name: stage.map(|name| name.to_ascii_lowercase()),
                base: image,
                text: node.original.clone(),
                line,
                steps: Vec::new(),
            });
        } else {
            let stage = stages
                .last_mut()
                .with_context(|| format!("line {line}: the first instruction must be FROM"))?;
            supported(&instruction, node)?;
            stage.steps.push(Step {
                instruction,
                text: node.original.clone(),
                line,
            });
        }
    }
    Ok(Recipe {
        escape: dockerfile.escape,
        stages,
    })
}

/// Keep execution limited to the existing builder until stage jobs are wired in.
fn single_stage(recipe: &Recipe) -> Result<()> {
    for (index, stage) in recipe.stages.iter().enumerate() {
        let line = stage.line;
        if index > 0 {
            bail!("line {line}: multi-stage builds (a second FROM) are not supported yet");
        }
        if stage.name.is_some() {
            bail!("line {line}: multi-stage builds (FROM … AS) are not supported yet");
        }
        if stage.base.eq_ignore_ascii_case("scratch") {
            bail!("line {line}: FROM scratch is not supported yet");
        }
        for step in &stage.steps {
            if let Instruction::Copy { flags, .. } = &step.instruction
                && flags.iter().any(|flag| flag.name == "from")
            {
                bail!("line {}: COPY --from is not supported yet", step.line);
            }
        }
    }
    Ok(())
}

fn supported(instruction: &Instruction, node: &Node) -> Result<()> {
    let line = node.start_line;
    match instruction {
        Instruction::From { .. } => {
            bail!("line {line}: multi-stage builds (a second FROM) are not supported yet")
        }
        Instruction::Run { flags, .. } | Instruction::Copy { flags, .. } => match flags
            .iter()
            .find(|flag| !matches!(instruction, Instruction::Copy { .. }) || flag.name != "from")
        {
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
        let result = check(src).and_then(|recipe| single_stage(&recipe));
        format!("{:#}", result.unwrap_err())
    }

    #[test]
    fn accepts_every_v1_instruction_in_any_case() {
        let recipe = check(
            "# syntax=docker/dockerfile:1\nFROM alpine:3.20\nenv A=1\nWORKDIR /app\nCOPY a b/\nrun echo hi\nUSER 1000\nLABEL x=y\nEXPOSE 80\nCMD [\"sh\"]\nENTRYPOINT [\"/bin/sh\",\"-c\"]\n",
        )
        .unwrap();
        assert_eq!(recipe.stages[0].base, "alpine:3.20");
        assert_eq!(recipe.escape, '\\');
        assert_eq!(recipe.stages[0].steps.len(), 9);
        assert_eq!(recipe.stages[0].steps[3].text, "run echo hi");
        assert_eq!(recipe.stages[0].steps[3].line, 6);
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
            "line 1: file with no instructions"
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
