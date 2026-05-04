use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::types::{PlanStepKind, ToolResult};

#[derive(Debug, Clone)]
pub struct ToolPolicy {
    pub workspace: PathBuf,
    pub max_file_bytes: usize,
    pub max_output_bytes: usize,
    pub timeout: Duration,
    pub allow_exec: bool,
    /// Explicit user allowlist. Empty means allow all (when allow_exec=true).
    pub allowed_exec: BTreeSet<String>,
    /// Programs added by skills — always allowed, never affect the "empty=all" logic.
    pub skill_exec: BTreeSet<String>,
}

impl ToolPolicy {
    pub fn allows_program(&self, program: &str) -> bool {
        if !self.allow_exec {
            return false;
        }
        if self.skill_exec.contains(program) {
            return true;
        }
        self.allowed_exec.is_empty() || self.allowed_exec.contains(program)
    }
}

#[derive(Debug, Clone)]
pub struct ToolRunner {
    policy: ToolPolicy,
}

impl ToolRunner {
    pub fn new(policy: ToolPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &ToolPolicy {
        &self.policy
    }

    pub fn run_step(&self, step: &PlanStepKind) -> ToolResult {
        match step {
            PlanStepKind::Answer(text) => ToolResult {
                name: "answer".to_owned(),
                ok: true,
                output: text.clone(),
            },
            PlanStepKind::ReadFile(path) => self.read_file(path),
            PlanStepKind::ListDir(path) => self.list_dir(path),
            PlanStepKind::Exec { program, args } => self.exec(program, args),
        }
    }

    fn read_file(&self, requested: &str) -> ToolResult {
        let path = workspace_path(&self.policy.workspace, requested);
        let output = match read_limited(&path, self.policy.max_file_bytes) {
            Ok(text) => text,
            Err(err) => format!("read failed: {err}"),
        };
        ToolResult {
            name: "fs.read".to_owned(),
            ok: !output.starts_with("read failed:"),
            output,
        }
    }

    fn list_dir(&self, requested: &str) -> ToolResult {
        let path = workspace_path(&self.policy.workspace, requested);
        let output = match list_dir(&path, self.policy.max_output_bytes) {
            Ok(text) => text,
            Err(err) => format!("list failed: {err}"),
        };
        ToolResult {
            name: "fs.list".to_owned(),
            ok: !output.starts_with("list failed:"),
            output,
        }
    }

    fn exec(&self, program: &str, args: &[String]) -> ToolResult {
        if !self.policy.allows_program(program) {
            return ToolResult {
                name: "exec.run".to_owned(),
                ok: false,
                output: format!("exec denied: {program} is not allowlisted"),
            };
        }

        let mut child = match Command::new(program)
            .args(args)
            .current_dir(&self.policy.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                return ToolResult {
                    name: "exec.run".to_owned(),
                    ok: false,
                    output: format!("exec failed: {err}"),
                };
            }
        };

        let deadline = Instant::now() + self.policy.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    return ToolResult {
                        name: "exec.run".to_owned(),
                        ok: false,
                        output: "exec timed out".to_owned(),
                    };
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(err) => {
                    let _ = child.kill();
                    return ToolResult {
                        name: "exec.run".to_owned(),
                        ok: false,
                        output: format!("exec wait failed: {err}"),
                    };
                }
            }
        }

        match child.wait_with_output() {
            Ok(output) => {
                let mut text = String::new();
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                if !output.stderr.is_empty() {
                    text.push_str("\nstderr:\n");
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                ToolResult {
                    name: "exec.run".to_owned(),
                    ok: output.status.success(),
                    output: cap_text(&text, self.policy.max_output_bytes),
                }
            }
            Err(err) => ToolResult {
                name: "exec.run".to_owned(),
                ok: false,
                output: format!("exec output failed: {err}"),
            },
        }
    }
}

fn workspace_path(workspace: &Path, requested: &str) -> PathBuf {
    let path = Path::new(requested);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

fn read_limited(path: &Path, max_bytes: usize) -> io::Result<String> {
    let data = fs::read(path)?;
    if data.len() > max_bytes {
        return Ok(format!(
            "file is {} bytes; limit is {} bytes",
            data.len(),
            max_bytes
        ));
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}

fn list_dir(path: &Path, max_bytes: usize) -> io::Result<String> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let suffix = if kind.is_dir() { "/" } else { "" };
        names.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
    }
    names.sort_unstable();
    Ok(cap_text(&names.join("\n"), max_bytes))
}

fn cap_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }

    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated to {} bytes]", &text[..end], max_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_exec_by_default() {
        let runner = ToolRunner::new(ToolPolicy {
            workspace: PathBuf::from("."),
            max_file_bytes: 64,
            max_output_bytes: 64,
            timeout: Duration::from_secs(1),
            allow_exec: false,
            allowed_exec: BTreeSet::new(),
            skill_exec: BTreeSet::new(),
        });

        let result = runner.run_step(&PlanStepKind::Exec {
            program: "true".to_owned(),
            args: Vec::new(),
        });

        assert!(!result.ok);
        assert!(result.output.contains("denied"));
    }
}
