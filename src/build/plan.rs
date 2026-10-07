//! Resolve stage dependencies into a deterministic execution order.

use crate::dockerfile::Recipe;
use std::collections::HashMap;

use anyhow::{Result, anyhow, ensure};
use sandcastle_dockerfile::Instruction;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Context,
    Stage(usize),
    Image(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    Scratch,
    Stage(usize),
    Image(String),
}

#[derive(Debug)]
pub struct PlannedStage {
    pub base: Base,
    /// Indexed by instruction; non-COPY instructions use Context.
    pub sources: Vec<Source>,
}

#[derive(Debug)]
pub struct BuildPlan {
    pub target: usize,
    pub order: Vec<usize>,
    pub stages: Vec<PlannedStage>,
}

/// Resolve all references, then traverse only dependencies of the target.
/// This performs no image pulls or other I/O.
pub fn resolve(recipe: &Recipe, target: Option<&str>) -> Result<BuildPlan> {
    ensure!(!recipe.stages.is_empty(), "the Dockerfile has no stages");
    let mut names = HashMap::new();
    for (index, stage) in recipe.stages.iter().enumerate() {
        if let Some(name) = &stage.name {
            ensure!(
                names.insert(name.to_ascii_lowercase(), index).is_none(),
                "line {}: duplicate stage name {name:?}",
                stage.line
            );
        }
    }
    let target = match target {
        Some(name) => *names
            .get(&name.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("target stage {name:?} could not be found"))?,
        None => recipe.stages.len() - 1,
    };
    let mut stages = Vec::with_capacity(recipe.stages.len());
    let mut graph = Vec::with_capacity(recipe.stages.len());
    for (index, stage) in recipe.stages.iter().enumerate() {
        let base = if stage.base.eq_ignore_ascii_case("scratch") {
            Base::Scratch
        } else if let Some(&parent) = names
            .get(&stage.base.to_ascii_lowercase())
            .filter(|&&parent| parent < index)
        {
            Base::Stage(parent)
        } else {
            Base::Image(stage.base.clone())
        };
        let mut dependencies = Vec::new();
        if let Base::Stage(parent) = base {
            dependencies.push(Dependency {
                stage: parent,
                line: stage.line,
            });
        }
        let mut sources = Vec::with_capacity(stage.steps.len());
        for step in &stage.steps {
            let source = match &step.instruction {
                Instruction::Copy { flags, .. } => {
                    match flags.iter().find(|flag| flag.name == "from") {
                        None => Source::Context,
                        Some(flag) => {
                            let value = &flag.value;
                            ensure!(
                                !value.is_empty() && !value.contains('$'),
                                "line {}: COPY --from must be a literal stage or image reference",
                                step.line
                            );
                            let digits = value.strip_prefix('-').unwrap_or(value);
                            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                                let source = value.parse::<usize>().map_err(|_| {
                                    anyhow!("line {}: invalid stage index {value}", step.line)
                                })?;
                                ensure!(
                                    source < recipe.stages.len(),
                                    "line {}: invalid stage index {value}",
                                    step.line
                                );
                                Source::Stage(source)
                            } else if let Some(&source) = names.get(&value.to_ascii_lowercase()) {
                                Source::Stage(source)
                            } else {
                                Source::Image(value.clone())
                            }
                        }
                    }
                }
                _ => Source::Context,
            };
            if let Source::Stage(source) = source {
                dependencies.push(Dependency {
                    stage: source,
                    line: step.line,
                });
            }
            sources.push(source);
        }
        stages.push(PlannedStage { base, sources });
        graph.push(dependencies);
    }
    let order = execution_order(&graph, target).map_err(|cycle| {
        let label = |index: usize| match &recipe.stages[index].name {
            Some(name) => format!("stage {index} ({name:?})"),
            None => format!("stage {index}"),
        };
        let edges = cycle
            .edges
            .iter()
            .map(|edge| {
                format!(
                    "{} --[line {}]--> {}",
                    label(edge.from),
                    edge.line,
                    label(edge.to)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        anyhow!("stage dependency cycle: {edges}")
    })?;
    Ok(BuildPlan {
        target,
        order,
        stages,
    })
}

#[derive(Debug, Clone, Copy)]
struct Dependency {
    stage: usize,
    line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CycleEdge {
    from: usize,
    to: usize,
    line: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct DependencyCycle {
    edges: Vec<CycleEdge>,
}

#[derive(Clone, Copy)]
enum Color {
    White,
    Gray,
    Black,
}

struct Frame {
    stage: usize,
    next_dependency: usize,
}

/// All indices must have been validated by the reference resolver.
fn execution_order(
    dependencies: &[Vec<Dependency>],
    target: usize,
) -> std::result::Result<Vec<usize>, DependencyCycle> {
    let mut colors = vec![Color::White; dependencies.len()];
    let mut stack = Vec::with_capacity(dependencies.len());
    let mut order = Vec::with_capacity(dependencies.len());
    let mut active_positions = vec![None; dependencies.len()];
    let mut path = Vec::with_capacity(dependencies.len());
    colors[target] = Color::Gray;
    active_positions[target] = Some(0);
    stack.push(Frame {
        stage: target,
        next_dependency: 0,
    });
    while let Some(frame) = stack.last_mut() {
        let stage = frame.stage;
        if let Some(dependency) = dependencies[stage].get(frame.next_dependency) {
            frame.next_dependency += 1;
            match colors[dependency.stage] {
                Color::White => {
                    colors[dependency.stage] = Color::Gray;
                    active_positions[dependency.stage] = Some(stack.len());
                    path.push(CycleEdge {
                        from: stage,
                        to: dependency.stage,
                        line: dependency.line,
                    });
                    stack.push(Frame {
                        stage: dependency.stage,
                        next_dependency: 0,
                    });
                }
                Color::Gray => {
                    let start = active_positions[dependency.stage]
                        .expect("gray stages are on the active path");
                    let mut edges = path[start..].to_vec();
                    edges.push(CycleEdge {
                        from: stage,
                        to: dependency.stage,
                        line: dependency.line,
                    });
                    return Err(DependencyCycle { edges });
                }
                Color::Black => {}
            }
        } else {
            stack.pop();
            path.pop();
            active_positions[stage] = None;
            colors[stage] = Color::Black;
            order.push(stage);
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dockerfile::check;

    fn dependency(stage: usize, line: usize) -> Dependency {
        Dependency { stage, line }
    }

    #[test]
    fn single_stage() {
        assert_eq!(execution_order(&[vec![]], 0).unwrap(), [0]);
    }

    #[test]
    fn chain() {
        let graph = [vec![], vec![dependency(0, 2)], vec![dependency(1, 3)]];
        assert_eq!(execution_order(&graph, 2).unwrap(), [0, 1, 2]);
    }

    #[test]
    fn diamond() {
        let graph = [
            vec![],
            vec![dependency(0, 2)],
            vec![dependency(0, 3)],
            vec![dependency(1, 4), dependency(2, 5)],
        ];
        assert_eq!(execution_order(&graph, 3).unwrap(), [0, 1, 2, 3]);
    }

    #[test]
    fn later_stage_dependency() {
        let graph = [vec![dependency(2, 2)], vec![], vec![]];
        assert_eq!(execution_order(&graph, 0).unwrap(), [2, 0]);
    }

    #[test]
    fn target_prunes_unrelated_stages() {
        let graph = [vec![], vec![dependency(0, 2)], vec![], vec![]];
        assert_eq!(execution_order(&graph, 1).unwrap(), [0, 1]);
    }

    #[test]
    fn repeated_edges() {
        let graph = [vec![], vec![dependency(0, 2), dependency(0, 3)]];
        assert_eq!(execution_order(&graph, 1).unwrap(), [0, 1]);
    }

    #[test]
    fn self_cycle() {
        let graph = [vec![dependency(0, 2)]];
        assert_eq!(
            execution_order(&graph, 0).unwrap_err().edges,
            [CycleEdge {
                from: 0,
                to: 0,
                line: 2
            }]
        );
    }

    #[test]
    fn two_stage_cycle() {
        let graph = [vec![dependency(1, 3)], vec![dependency(0, 7)]];
        assert_eq!(
            execution_order(&graph, 0).unwrap_err().edges,
            [
                CycleEdge {
                    from: 0,
                    to: 1,
                    line: 3
                },
                CycleEdge {
                    from: 1,
                    to: 0,
                    line: 7
                },
            ]
        );
    }

    #[test]
    fn cycle_excludes_acyclic_prefix() {
        let graph = [
            vec![dependency(1, 3)],
            vec![dependency(0, 7)],
            vec![],
            vec![dependency(0, 9)],
        ];
        assert_eq!(
            execution_order(&graph, 3).unwrap_err().edges,
            [
                CycleEdge {
                    from: 0,
                    to: 1,
                    line: 3
                },
                CycleEdge {
                    from: 1,
                    to: 0,
                    line: 7
                },
            ]
        );
    }

    #[test]
    fn unreachable_cycle_is_ignored() {
        let graph = [vec![dependency(1, 3)], vec![dependency(0, 7)], vec![]];
        assert_eq!(execution_order(&graph, 2).unwrap(), [2]);
    }

    #[test]
    fn deep_chain() {
        let mut graph = vec![vec![]];
        graph.extend((1..100_000).map(|stage| vec![dependency(stage - 1, stage + 1)]));
        let order = execution_order(&graph, 99_999).unwrap();
        assert_eq!(order.len(), 100_000);
        assert!(order.into_iter().eq(0..100_000));
    }

    #[test]
    fn single_stage_recipe() {
        let plan = resolve(&check("FROM alpine\nCOPY a /a\n").unwrap(), None).unwrap();
        assert_eq!(plan.target, 0);
        assert_eq!(plan.order, [0]);
        assert_eq!(plan.stages[0].base, Base::Image("alpine".into()));
        assert_eq!(plan.stages[0].sources, [Source::Context]);
    }

    #[test]
    fn resolves_bases_and_copy_sources_in_instruction_order() {
        let recipe = check("FROM alpine AS Base\nFROM base AS unused\nRUN false\nFROM BASE AS builder\nCOPY file /file\nFROM scratch AS final\nCOPY --from=builder /file /file\nCOPY --from=0 /etc/os-release /os\nCOPY --from=busybox:1.36 /bin/busybox /busybox\n").unwrap();
        let plan = resolve(&recipe, None).unwrap();
        assert_eq!(plan.target, 3);
        assert_eq!(plan.order, [0, 2, 3]);
        assert_eq!(plan.stages[2].base, Base::Stage(0));
        assert_eq!(plan.stages[3].base, Base::Scratch);
        assert_eq!(
            plan.stages[3].sources,
            [
                Source::Stage(2),
                Source::Stage(0),
                Source::Image("busybox:1.36".into())
            ]
        );
        assert_eq!(resolve(&recipe, Some("bUiLdEr")).unwrap().order, [0, 2]);
    }

    #[test]
    fn later_copy_stage_precedes_consumer() {
        let recipe = check(
            "FROM scratch AS consumer\nCOPY --from=producer /x /x\nFROM alpine AS producer\n",
        )
        .unwrap();
        assert_eq!(resolve(&recipe, Some("consumer")).unwrap().order, [1, 0]);
    }

    #[test]
    fn from_resolves_only_preceding_stages() {
        let recipe = check("FROM future AS early\nFROM alpine AS future\n").unwrap();
        let plan = resolve(&recipe, Some("early")).unwrap();
        assert_eq!(plan.stages[0].base, Base::Image("future".into()));
        assert_eq!(plan.order, [0]);
    }

    #[test]
    fn mixed_cycle_has_stage_names_and_instruction_lines() {
        let recipe = check(
            "FROM scratch AS consumer\nCOPY --from=builder /x /x\nFROM consumer AS builder\n",
        )
        .unwrap();
        let error = resolve(&recipe, None).unwrap_err().to_string();
        for expected in [
            "cycle", "consumer", "builder", "stage 0", "stage 1", "line 2", "line 3",
        ] {
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn unnamed_cycle_reports_stage_indices() {
        let recipe =
            check("FROM scratch\nCOPY --from=1 /x /x\nFROM scratch\nCOPY --from=0 /x /x\n")
                .unwrap();
        let error = resolve(&recipe, None).unwrap_err().to_string();
        for expected in ["stage 0", "stage 1", "line 2", "line 4"] {
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn independent_target_ignores_unreachable_cycle() {
        let recipe =
            check("FROM scratch AS a\nCOPY --from=b /x /x\nFROM a AS b\nFROM scratch AS target\n")
                .unwrap();
        assert_eq!(resolve(&recipe, None).unwrap().order, [2]);
    }

    #[test]
    fn validates_references_even_in_unreachable_stages() {
        for (src, target, expected) in [
            (
                "FROM scratch AS a\nFROM scratch AS A\n",
                None,
                "line 2: duplicate stage name",
            ),
            ("FROM scratch AS a\n", Some("missing"), "target stage"),
            (
                "FROM scratch\nCOPY --from=9 /x /x\nFROM scratch AS target\n",
                None,
                "line 2: invalid stage index",
            ),
            (
                "FROM scratch\nCOPY --from=-1 /x /x\n",
                None,
                "line 2: invalid stage index",
            ),
            (
                "FROM scratch\nCOPY --from=999999999999999999999999 /x /x\n",
                None,
                "line 2: invalid stage index",
            ),
            (
                "FROM scratch\nCOPY --from=$SOURCE /x /x\n",
                None,
                "line 2: COPY --from must be a literal",
            ),
        ] {
            let error = resolve(&check(src).unwrap(), target)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
    }
}
