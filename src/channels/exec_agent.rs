use crate::channels::AgentHandler;
use crate::tools::{ToolPolicy, ToolRunner};
use crate::types::PlanStepKind;

pub struct ExecAgent {
    runner: ToolRunner,
}

impl ExecAgent {
    pub fn new(policy: ToolPolicy) -> Self {
        Self {
            runner: ToolRunner::new(policy),
        }
    }
}

impl AgentHandler for ExecAgent {
    fn name(&self) -> &str {
        "exec"
    }

    fn execute(&self, command: &str) -> Result<String, String> {
        let step = parse_exec_command(command);
        let result = self.runner.run_step(&step);
        if result.ok {
            Ok(result.output)
        } else {
            Err(result.output)
        }
    }
}

fn parse_exec_command(command: &str) -> PlanStepKind {
    let trimmed = command.trim();
    let mut parts = trimmed.split_whitespace();
    let Some(verb) = parts.next() else {
        return PlanStepKind::Answer("(empty command)".to_owned());
    };
    match verb {
        "read" | "cat" => {
            let path = parts.collect::<Vec<_>>().join(" ");
            if path.is_empty() {
                PlanStepKind::Answer("read requires a path".to_owned())
            } else {
                PlanStepKind::ReadFile(path)
            }
        }
        "ls" | "list" => {
            let path = parts.collect::<Vec<_>>().join(" ");
            PlanStepKind::ListDir(if path.is_empty() {
                ".".to_owned()
            } else {
                path
            })
        }
        _ => {
            let program = verb.to_owned();
            let args = parts.map(str::to_owned).collect();
            PlanStepKind::Exec { program, args }
        }
    }
}
