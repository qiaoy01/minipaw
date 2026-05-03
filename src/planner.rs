use crate::llm::LlmClient;
use crate::memory::ProgressiveMemory;
use crate::skills::{Skill, SkillRegistry};
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
        self.plan_with_context(
            task,
            &task.title,
            AgentPattern::Direct,
            &ProgressiveMemory::default(),
            &SkillRegistry::default(),
            llm,
        )
    }

    pub fn plan_with_pattern(
        &self,
        task: &Task,
        input: &str,
        pattern: AgentPattern,
        llm: &mut dyn LlmClient,
    ) -> Plan {
        self.plan_with_context(
            task,
            input,
            pattern,
            &ProgressiveMemory::default(),
            &SkillRegistry::default(),
            llm,
        )
    }

    pub fn plan_with_context(
        &self,
        task: &Task,
        input: &str,
        pattern: AgentPattern,
        memory: &ProgressiveMemory,
        skills: &SkillRegistry,
        llm: &mut dyn LlmClient,
    ) -> Plan {
        let text = input.trim();
        let kind = parse_local_intent(text).unwrap_or_else(|| {
            let prompt = planner_prompt(pattern, text, memory, skills);
            let response = llm.next_step(&prompt);
            // Let the LLM invoke a skill or fall back to a plain answer.
            parse_skill_call(text, &response, skills).unwrap_or(PlanStepKind::Answer(response))
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

/// Parse a `SKILL: <name>` directive from the first few lines of an LLM
/// response.  On a match, look up the skill's exec command and return the
/// corresponding plan step.  Returns `None` when no directive is present or
/// the named skill has no exec command.
fn parse_skill_call(input: &str, response: &str, registry: &SkillRegistry) -> Option<PlanStepKind> {
    for line in response.lines().take(4) {
        let trimmed = line.trim();
        if !trimmed.to_ascii_lowercase().starts_with("skill:") {
            continue;
        }
        let name = trimmed["skill:".len()..].trim();
        let skill = registry.find(name)?;
        if !skill_allowed_for_input(input, skill) {
            return None;
        }
        let exec = skill.exec.as_deref()?;
        let mut parts = exec.split_whitespace();
        let program = parts.next()?.to_owned();
        let args = parts.map(str::to_owned).collect();
        return Some(PlanStepKind::Exec { program, args });
    }
    None
}

fn skill_allowed_for_input(input: &str, skill: &Skill) -> bool {
    let input_terms = meaningful_terms(input);
    if input_terms.is_empty() {
        return false;
    }
    let mut skill_text = String::new();
    skill_text.push_str(&skill.name);
    skill_text.push(' ');
    skill_text.push_str(&skill.description);
    let skill_terms = meaningful_terms(&skill_text);
    input_terms.iter().any(|term| skill_terms.contains(term))
}

fn meaningful_terms(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|term| term.len() >= 3)
        .filter(|term| !is_stop_word(term))
        .map(str::to_ascii_lowercase)
        .collect()
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

fn planner_prompt(
    pattern: AgentPattern,
    input: &str,
    memory: &ProgressiveMemory,
    skills: &SkillRegistry,
) -> String {
    let memory_text = memory.render();
    let skills_text = skills.index_text();
    let skills_section = if skills_text.is_empty() {
        String::new()
    } else {
        format!(
            "\n{skills_text}\
             If a skill matches what is needed, respond with exactly:\n\
             SKILL: <skill-name>\n\
             Otherwise answer directly.\n"
        )
    };
    match pattern {
        AgentPattern::Direct => {
            format!("{memory_text}{skills_section}\nTask:\n{input}")
        }
        AgentPattern::CoordinatorWorker => format!(
            "You are a worker sub-agent. Use progressive memory only as needed.\n\
             {memory_text}{skills_section}\nComplete this assigned task concisely:\n{input}"
        ),
        AgentPattern::HubAndSpoke => format!(
            "You are behind a gateway in a hub-and-spoke agent. Use memory index first, details second.\n\
             {memory_text}{skills_section}\nTask:\n{input}"
        ),
        AgentPattern::Pipeline => {
            format!("Produce the next pipeline stage output.\n{memory_text}{skills_section}\nTask:\n{input}")
        }
        AgentPattern::MapReduce => {
            format!("Produce a map-reduce friendly answer.\n{memory_text}{skills_section}\nTask:\n{input}")
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
        _ => parse_natural_action(input),
    }
}

/// Map natural-language action phrases to tool plan steps.
///
/// Recognises:
///   list/ls [path]          → ListDir
///   read/cat <path>         → ReadFile
///   run/exec/execute <prog> → Exec
///
/// Strips articles and common prepositions when searching for the path/program
/// token so that "list the files in src" and "read the file src/main.rs" both
/// resolve correctly.  Falls back to None for everything else so the LLM path
/// handles knowledge questions.
fn parse_natural_action(input: &str) -> Option<PlanStepKind> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    let verb = tokens[0].to_ascii_lowercase();
    match verb.as_str() {
        "list" | "ls" => {
            let path = first_path_token(&tokens[1..])
                .map(str::to_owned)
                .unwrap_or_else(|| ".".to_owned());
            Some(PlanStepKind::ListDir(path))
        }
        "read" | "cat" => {
            // Require an explicit path marker (/ or file extension) so that
            // "read about X" or "read notes" fall through to the LLM instead
            // of being mistaken for file reads.
            let path = tokens[1..]
                .iter()
                .find(|t| looks_like_file_path(t))?
                .to_string();
            Some(PlanStepKind::ReadFile(path))
        }
        "run" | "exec" | "execute" => {
            let program = tokens.get(1)?.to_string();
            let args = tokens[2..].iter().map(|t| t.to_string()).collect();
            Some(PlanStepKind::Exec { program, args })
        }
        _ => None,
    }
}

/// Return the first token in `tokens` that looks like a file or directory path,
/// skipping common English stop words (articles, prepositions, etc.).
fn first_path_token<'a>(tokens: &[&'a str]) -> Option<&'a str> {
    for &token in tokens {
        if is_path_like(token) {
            return Some(token);
        }
    }
    None
}

/// Strict check: requires a directory separator or a filename extension so that
/// plain words ("about", "notes", "sqlite") are never mistaken for file paths.
fn looks_like_file_path(token: &str) -> bool {
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    token
        .rfind('.')
        .map(|pos| pos > 0 && pos < token.len() - 1)
        .unwrap_or(false)
}

fn is_path_like(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // Contains a directory separator — definitely a path.
    if token.contains('/') || token.contains('\\') {
        return true;
    }
    // Has an internal dot that looks like a file extension (e.g. "README.md").
    if let Some(pos) = token.rfind('.') {
        if pos > 0 && pos < token.len() - 1 {
            return true;
        }
    }
    // Simple alphanumeric/dash/underscore identifier that is not a stop word.
    token
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
        && !is_stop_word(token)
}

fn is_stop_word(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "the" | "a" | "an" | "all" | "file" | "files" | "dir" | "directory"
            | "folder" | "folders" | "in" | "of" | "for" | "at" | "to" | "from"
            | "about" | "with" | "on" | "by" | "as" | "into" | "through"
            | "please" | "me" | "my" | "here" | "there" | "this" | "that"
            | "it" | "them" | "they" | "him" | "her" | "us" | "you" | "we"
            | "and" | "or" | "is" | "are" | "was" | "were" | "be" | "been"
            | "have" | "has" | "had" | "do" | "does" | "did" | "will" | "would"
            | "can" | "could" | "should" | "may" | "might" | "must" | "shall"
    )
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
        "gateway run                 run foreground gateway",
        "gateway simulate            wait for simulated channel/agent messages",
        "onboarding                  configure model and channel",
        "uninstall                   remove minipaw install",
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
    fn parses_natural_list_action() {
        assert_eq!(
            parse_natural_action("list src"),
            Some(PlanStepKind::ListDir("src".into()))
        );
        assert_eq!(
            parse_natural_action("ls ."),
            Some(PlanStepKind::ListDir(".".into()))
        );
        assert_eq!(
            parse_natural_action("list the files in src"),
            Some(PlanStepKind::ListDir("src".into()))
        );
        // "list" with no path falls back to current directory.
        assert_eq!(
            parse_natural_action("list"),
            Some(PlanStepKind::ListDir(".".into()))
        );
    }

    #[test]
    fn parses_natural_read_action() {
        assert_eq!(
            parse_natural_action("read src/main.rs"),
            Some(PlanStepKind::ReadFile("src/main.rs".into()))
        );
        assert_eq!(
            parse_natural_action("read the file README.md"),
            Some(PlanStepKind::ReadFile("README.md".into()))
        );
        // No path-like token → returns None so LLM handles it.
        assert_eq!(parse_natural_action("read about sqlite"), None);
    }

    #[test]
    fn parses_natural_exec_action() {
        assert_eq!(
            parse_natural_action("run git status"),
            Some(PlanStepKind::Exec {
                program: "git".into(),
                args: vec!["status".into()]
            })
        );
        assert_eq!(
            parse_natural_action("exec ls"),
            Some(PlanStepKind::Exec {
                program: "ls".into(),
                args: vec![]
            })
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

    #[test]
    fn gates_skill_calls_by_generic_term_overlap() {
        let skill = Skill {
            name: "current-time".into(),
            description: "Get the current date and time on the local machine".into(),
            exec: Some("date".into()),
        };

        assert!(!skill_allowed_for_input("add 7 again", &skill));
        assert!(skill_allowed_for_input("what time is it", &skill));
    }
}
