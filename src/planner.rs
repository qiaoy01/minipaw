use crate::llm::LlmClient;
use crate::memory::ProgressiveMemory;
use crate::types::{AgentPattern, Plan, PlanStep, PlanStepKind, StepStatus, Task};

#[derive(Debug, Default, Clone)]
pub struct Planner;

impl Planner {
    pub fn select_pattern(&self, input: &str, llm: &mut dyn LlmClient) -> AgentPattern {
        self.select_pattern_with_memory(input, &ProgressiveMemory::default(), llm)
    }

    pub fn select_pattern_with_memory(
        &self,
        input: &str,
        memory: &ProgressiveMemory,
        llm: &mut dyn LlmClient,
    ) -> AgentPattern {
        if let Some(pattern) = local_pattern(input) {
            return pattern;
        }

        let prompt = pattern_selection_prompt(input, memory);
        let response = llm.next_step(&prompt);
        let selected = parse_pattern(&response).unwrap_or_else(|| heuristic_pattern(input));
        validate_pattern(input, selected)
    }

    pub fn plan(&self, task: &Task, llm: &mut dyn LlmClient) -> Plan {
        self.plan_with_pattern(task, &task.title, AgentPattern::Direct, llm)
    }

    pub fn plan_with_pattern(
        &self,
        task: &Task,
        input: &str,
        pattern: AgentPattern,
        llm: &mut dyn LlmClient,
    ) -> Plan {
        self.plan_with_context(task, input, pattern, &ProgressiveMemory::default(), llm)
    }

    pub fn plan_with_context(
        &self,
        task: &Task,
        input: &str,
        pattern: AgentPattern,
        memory: &ProgressiveMemory,
        llm: &mut dyn LlmClient,
    ) -> Plan {
        let text = input.trim();
        let kind = parse_local_intent(text).unwrap_or_else(|| {
            let prompt = planner_prompt(pattern, text, memory);
            let answer = llm.next_step(&prompt);
            PlanStepKind::Answer(answer)
        });

        Plan {
            task_id: task.id,
            steps: vec![PlanStep {
                index: 0,
                kind,
                status: StepStatus::Pending,
            }],
        }
    }
}

fn pattern_selection_prompt(input: &str, memory: &ProgressiveMemory) -> String {
    format!(
        "Choose exactly one orchestration pattern for this task.
Allowed values:
direct
coordinator-worker
hub-and-spoke
pipeline
map-reduce

Rules:
direct = simple answer or one local read/list operation
coordinator-worker = normal delegated work with a worker sub-agent
hub-and-spoke = risky local tool work or gateway-controlled execution
pipeline = ordered transform/refine stages
map-reduce = independent items that should be mapped then reduced

Reply with only the allowed value.
{}
Task:
{input}",
        memory.render()
    )
}

fn local_pattern(input: &str) -> Option<AgentPattern> {
    let trimmed = input.trim_start();
    if trimmed.starts_with("/pipeline") {
        Some(AgentPattern::Pipeline)
    } else if trimmed.starts_with("/map") || trimmed.starts_with("/reduce") {
        Some(AgentPattern::MapReduce)
    } else if trimmed.starts_with("/exec") {
        Some(AgentPattern::HubAndSpoke)
    } else if trimmed.starts_with("/read") || trimmed.starts_with("/ls") || trimmed == "/help" {
        Some(AgentPattern::Direct)
    } else {
        None
    }
}

fn heuristic_pattern(input: &str) -> AgentPattern {
    let lower = input.to_ascii_lowercase();
    let pipe_count = input.matches('|').count();

    if lower.contains("map reduce")
        || lower.contains("map-reduce")
        || (pipe_count >= 2
            && (lower.contains("summarize")
                || lower.contains("compare")
                || lower.contains("classify")))
    {
        AgentPattern::MapReduce
    } else if lower.contains("pipeline")
        || lower.contains("then")
        || lower.contains("step by step")
        || lower.contains("refine")
    {
        AgentPattern::Pipeline
    } else if lower.contains("run command")
        || lower.contains("execute")
        || lower.contains("install")
        || lower.contains("delete")
    {
        AgentPattern::HubAndSpoke
    } else if lower.contains("delegate")
        || lower.contains("worker")
        || lower.contains("investigate")
        || lower.contains("implement")
    {
        AgentPattern::CoordinatorWorker
    } else {
        AgentPattern::Direct
    }
}

fn validate_pattern(input: &str, selected: AgentPattern) -> AgentPattern {
    let lower = input.to_ascii_lowercase();
    let risky_local_action = lower.contains("run a local command")
        || lower.contains("run command")
        || lower.contains("execute")
        || lower.contains("install")
        || lower.contains("delete")
        || lower.contains("overwrite")
        || lower.contains("sudo");

    if risky_local_action && selected == AgentPattern::Direct {
        AgentPattern::HubAndSpoke
    } else {
        selected
    }
}

fn parse_pattern(response: &str) -> Option<AgentPattern> {
    for line in response.lines().take(8) {
        if let Some(pattern) = parse_pattern_line(line) {
            return Some(pattern);
        }
    }
    None
}

fn parse_pattern_line(line: &str) -> Option<AgentPattern> {
    let trimmed = line
        .trim()
        .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch == '`' || ch.is_ascii_punctuation())
        .trim()
        .to_ascii_lowercase();
    let value = trimmed
        .strip_prefix("pattern")
        .and_then(|rest| rest.trim().strip_prefix(':'))
        .map(str::trim)
        .unwrap_or(&trimmed);

    match value {
        "direct" => Some(AgentPattern::Direct),
        "coordinator-worker" | "coordinator_worker" | "coordinator worker" => {
            Some(AgentPattern::CoordinatorWorker)
        }
        "hub-and-spoke" | "hub_and_spoke" | "hub and spoke" => Some(AgentPattern::HubAndSpoke),
        "pipeline" => Some(AgentPattern::Pipeline),
        "map-reduce" | "map_reduce" | "map reduce" => Some(AgentPattern::MapReduce),
        _ => None,
    }
}

fn planner_prompt(pattern: AgentPattern, input: &str, memory: &ProgressiveMemory) -> String {
    let memory = memory.render();
    match pattern {
        AgentPattern::Direct => format!("{memory}\nTask:\n{input}"),
        AgentPattern::CoordinatorWorker => format!(
            "You are a worker sub-agent. Use progressive memory only as needed.\n{memory}\nComplete this assigned task concisely:\n{input}"
        ),
        AgentPattern::HubAndSpoke => format!(
            "You are behind a gateway in a hub-and-spoke agent. Use memory index first, details second. Produce the next safe response.\n{memory}\nTask:\n{input}"
        ),
        AgentPattern::Pipeline => {
            format!("Produce the next pipeline stage output.\n{memory}\nTask:\n{input}")
        }
        AgentPattern::MapReduce => {
            format!("Produce a map-reduce friendly answer.\n{memory}\nTask:\n{input}")
        }
    }
}

fn parse_local_intent(input: &str) -> Option<PlanStepKind> {
    let mut parts = input.split_whitespace();
    let head = parts.next()?;
    match head {
        "/read" => Some(PlanStepKind::ReadFile(parts.collect::<Vec<_>>().join(" "))),
        "/ls" => {
            let path = parts.collect::<Vec<_>>().join(" ");
            Some(PlanStepKind::ListDir(if path.is_empty() {
                ".".to_owned()
            } else {
                path
            }))
        }
        "/exec" => {
            let program = parts.next()?.to_owned();
            let args = parts.map(str::to_owned).collect();
            Some(PlanStepKind::Exec { program, args })
        }
        "/help" | "help" => Some(PlanStepKind::Answer(help_text())),
        _ => None,
    }
}

pub fn help_text() -> String {
    [
        "minipaw commands:",
        "run                         start interactive agent",
        "task new <text>             create and run a task",
        "telegram run                poll Telegram and answer paired chats",
        "task list                   list tasks",
        "memory get <key>            read memory",
        "memory set <key> <value>    write memory",
        "config check                validate config",
        "config telegram set ...     configure Telegram bot",
        "config telegram pair <id>   allow one Telegram chat",
        "config telegram unpair <id> remove one Telegram chat",
        "config telegram show        show Telegram bot config",
        "/ls [path]                  list directory",
        "/read <path>                read capped file",
        "/exec <program> [args...]   run allowlisted command",
        "/enqueue <task>             queue a task for heartbeat tick",
        "/tick                       run one loop tick",
        "/heartbeat                  show loop heartbeat",
        "/pipeline a | b | c         run pipeline stages",
        "/mapreduce goal | a | b     run map-reduce supervisor",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_intent() {
        assert_eq!(
            parse_local_intent("/ls src"),
            Some(PlanStepKind::ListDir("src".into()))
        );
    }

    #[test]
    fn parses_pattern_response() {
        assert_eq!(parse_pattern("map-reduce"), Some(AgentPattern::MapReduce));
        assert_eq!(
            parse_pattern("Pattern: hub-and-spoke"),
            Some(AgentPattern::HubAndSpoke)
        );
        assert_eq!(
            parse_pattern("Allowed values: direct, pipeline, map-reduce"),
            None
        );
    }

    #[test]
    fn validates_risky_direct_selection() {
        assert_eq!(
            validate_pattern(
                "run a local command to inspect the kernel",
                AgentPattern::Direct
            ),
            AgentPattern::HubAndSpoke
        );
    }
}
