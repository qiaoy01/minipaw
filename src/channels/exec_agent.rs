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
    let parts = shell_split(command.trim());
    let mut iter = parts.iter().map(String::as_str);
    let Some(verb) = iter.next() else {
        return PlanStepKind::Answer("(empty command)".to_owned());
    };
    match verb {
        "read" | "cat" => {
            let path = iter.collect::<Vec<_>>().join(" ");
            if path.is_empty() {
                PlanStepKind::Answer("read requires a path".to_owned())
            } else {
                PlanStepKind::ReadFile(path)
            }
        }
        "ls" | "list" => {
            let path = iter.collect::<Vec<_>>().join(" ");
            PlanStepKind::ListDir(if path.is_empty() {
                ".".to_owned()
            } else {
                path
            })
        }
        _ => {
            let program = verb.to_owned();
            let args = iter.map(str::to_owned).collect();
            PlanStepKind::Exec { program, args }
        }
    }
}

/// Split a command string into tokens respecting single and double quotes.
fn shell_split(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                match next {
                                    'n' => current.push('\n'),
                                    't' => current.push('\t'),
                                    'r' => current.push('\r'),
                                    other => current.push(other),
                                }
                            }
                        }
                        _ => current.push(c),
                    }
                }
            }
            '\'' => {
                while let Some(c) = chars.next() {
                    match c {
                        '\'' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                match next {
                                    'n' => current.push('\n'),
                                    't' => current.push('\t'),
                                    'r' => current.push('\r'),
                                    other => current.push(other),
                                }
                            }
                        }
                        _ => current.push(c),
                    }
                }
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}
