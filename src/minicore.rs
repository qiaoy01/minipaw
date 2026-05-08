use crate::adjustments::{
    apply_training, parse_directive, write_proposal, AdjustmentDirective,
};
use crate::advisor::{compare, DivergenceRecord, DivergenceVerdict};
use crate::channels::AgentHandler;
use crate::llm::{ChatMessage, LlmClient};
use crate::memory::MemoryStore;
use crate::planner::classify_message;
use crate::prompts::PromptStore;
use crate::skills::SkillRegistry;
use crate::tools::ToolRunner;
use crate::types::{AdvisorMode, AgentChoice, MessageClass, TaskId, TaskStatus};

const MAX_SESSION_STEPS: usize = 16;
const CONTEXT_MAX_BYTES: usize = 6144;
const MEMORY_INDEX_LIMIT: usize = 12;
const MEMORY_DETAIL_LIMIT: usize = 4;
const MEMORY_DETAIL_BYTES: usize = 512;

// Embedded at compile time so it is always available regardless of install path.
const SOUL: &str = include_str!("../SOUL.md");
const MAX_RULE_APPENDS_PER_CLASS: usize = 24;

pub struct IncomingMessage {
    pub text: String,
    pub context_id: Option<String>,
    pub source: String,
}

pub struct SessionReport {
    pub task_id: TaskId,
    pub class: MessageClass,
    pub output: String,
    pub steps: usize,
}

pub struct MiniCore {
    pub(crate) memory: Box<dyn MemoryStore>,
    pub(crate) llm: Box<dyn LlmClient>,
    advisor: Option<Box<dyn LlmClient>>,
    advisor_mode: AdvisorMode,
    routing: std::collections::BTreeMap<MessageClass, AgentChoice>,
    tools: ToolRunner,
    skills: SkillRegistry,
    prompts: PromptStore,
    agents: Vec<Box<dyn AgentHandler>>,
    os_info: String,
}

impl MiniCore {
    pub fn new(
        memory: Box<dyn MemoryStore>,
        llm: Box<dyn LlmClient>,
        tools: ToolRunner,
        skills: SkillRegistry,
        prompts: PromptStore,
    ) -> Self {
        Self {
            memory,
            llm,
            advisor: None,
            advisor_mode: AdvisorMode::Work,
            routing: std::collections::BTreeMap::new(),
            os_info: detect_os_info(),
            tools,
            skills,
            prompts,
            agents: Vec::new(),
        }
    }

    /// Attach a remote advisor LLM and configure routing. When the advisor is
    /// unset, MiniCore behaves exactly as before (primary handles everything).
    pub fn set_advisor(
        &mut self,
        advisor: Box<dyn LlmClient>,
        mode: AdvisorMode,
        routing: std::collections::BTreeMap<MessageClass, AgentChoice>,
    ) {
        self.advisor = Some(advisor);
        self.advisor_mode = mode;
        self.routing = routing;
    }

    pub fn advisor_mode(&self) -> AdvisorMode {
        self.advisor_mode
    }

    pub fn has_advisor(&self) -> bool {
        self.advisor.is_some()
    }

    pub fn register(&mut self, agent: Box<dyn AgentHandler>) {
        self.agents.push(agent);
    }

    pub fn memory(&self) -> &dyn MemoryStore {
        self.memory.as_ref()
    }

    pub fn memory_mut(&mut self) -> &mut dyn MemoryStore {
        self.memory.as_mut()
    }

    pub fn agents(&self) -> impl Iterator<Item = &str> {
        self.agents.iter().map(|a| a.name())
    }

    pub fn run_exec(&self, command: &str) -> String {
        self.dispatch_exec(command)
    }

    pub fn process(&mut self, msg: IncomingMessage) -> SessionReport {
        if msg.text.trim() == "/new" {
            if let Some(ref cid) = msg.context_id {
                self.memory.delete_fact(&format!("ctx:{cid}"));
            }
            let task = self.memory.create_task("/new");
            self.memory.append_message(task.id, "user", "/new");
            self.memory
                .append_message(task.id, "assistant", "New conversation started.");
            self.memory.update_task_status(task.id, TaskStatus::Done);
            return SessionReport {
                task_id: task.id,
                class: MessageClass::MiniHow,
                output: "New conversation started.".to_owned(),
                steps: 0,
            };
        }

        let (prior_task_id, context) = self.assemble_context(&msg);

        // Classification always runs through the primary so the advisor is not
        // billed for the routing decision itself.
        let class = classify_message(&context, self.llm.as_mut());

        // Skill-override: if a registered executable skill matches the input
        // and the classifier chose miniwhat (no tool access), upgrade to minihow
        // so the EXEC/DONE loop can invoke it. This compensates for small models
        // that conflate "query a fact" with "run a tool to get a live value".
        let class = if class == MessageClass::MiniWhat
            && self.skills.match_for_input(&msg.text).is_some()
        {
            println!("classify class=minihow (skill-override)");
            MessageClass::MiniHow
        } else {
            class
        };

        let task_id = if let Some(tid) = prior_task_id {
            tid
        } else {
            let task = self.memory.create_task(&msg.text);
            if let Some(ref cid) = msg.context_id {
                self.memory
                    .set_fact(&format!("ctx:{cid}"), &task.id.0.to_string());
            }
            task.id
        };

        let executor = self.choose_executor(class);
        println!(
            "minicore task={task_id} class={class} source={} resumed={} mode={} executor={}",
            msg.source,
            prior_task_id.is_some(),
            self.advisor_mode,
            executor,
        );
        self.memory.append_message(task_id, "user", &msg.text);
        self.memory.update_task_status(task_id, TaskStatus::Running);

        let (output, steps) = self.run_with_executor(task_id, &msg.text, &context, class, executor);

        if self.should_shadow(executor) {
            self.run_shadow(task_id, &context, class, executor, &output);
        }

        let final_status = if output.starts_with("error:") {
            TaskStatus::Failed
        } else {
            TaskStatus::Done
        };
        self.memory.update_task_status(task_id, final_status);
        self.memory.append_message(task_id, "assistant", &output);

        SessionReport {
            task_id,
            class,
            output,
            steps,
        }
    }

    fn choose_executor(&self, class: MessageClass) -> AgentChoice {
        if self.advisor.is_none() {
            return AgentChoice::Primary;
        }
        match self.advisor_mode {
            // In training the advisor is the trusted teacher; primary is shadowed.
            AdvisorMode::Training => AgentChoice::Advisor,
            AdvisorMode::Trial | AdvisorMode::Work => self
                .routing
                .get(&class)
                .copied()
                .unwrap_or(AgentChoice::Primary),
        }
    }

    fn should_shadow(&self, executor: AgentChoice) -> bool {
        if self.advisor.is_none() {
            return false;
        }
        match self.advisor_mode {
            AdvisorMode::Training | AdvisorMode::Trial => true,
            // Work mode: route deterministically, do not shadow.
            AdvisorMode::Work => {
                let _ = executor;
                false
            }
        }
    }

    fn run_with_executor(
        &mut self,
        task_id: TaskId,
        input: &str,
        context: &str,
        class: MessageClass,
        executor: AgentChoice,
    ) -> (String, usize) {
        match executor {
            AgentChoice::Primary => {
                let mut llm = std::mem::replace(&mut self.llm, Box::new(crate::llm::OfflineLlm));
                let result = self.run_session(task_id, input, context, class, llm.as_mut());
                self.llm = llm;
                result
            }
            AgentChoice::Advisor => {
                let Some(mut llm) = self.advisor.take() else {
                    return self.run_with_executor(task_id, input, context, class, AgentChoice::Primary);
                };
                let result = self.run_session(task_id, input, context, class, llm.as_mut());
                self.advisor = Some(llm);
                result
            }
        }
    }

    fn run_shadow(
        &mut self,
        task_id: TaskId,
        context: &str,
        class: MessageClass,
        executor: AgentChoice,
        primary_output: &str,
    ) {
        let shadow_choice = match executor {
            AgentChoice::Primary => AgentChoice::Advisor,
            AgentChoice::Advisor => AgentChoice::Primary,
        };
        let shadow_output = self.run_shadow_query(context, shadow_choice);
        let Some(shadow) = shadow_output else {
            return;
        };

        // Order outputs as (primary, advisor) regardless of which one served.
        let (primary_text, advisor_text) = match executor {
            AgentChoice::Primary => (primary_output.to_owned(), shadow),
            AgentChoice::Advisor => (shadow, primary_output.to_owned()),
        };
        let record = compare(class, executor, &primary_text, &advisor_text);
        self.record_divergence(task_id, &record);

        if record.verdict == DivergenceVerdict::Divergent {
            self.run_adjustment(task_id, context, class, &primary_text, &advisor_text);
        }
    }

    fn run_adjustment(
        &mut self,
        task_id: TaskId,
        context: &str,
        class: MessageClass,
        primary_output: &str,
        advisor_output: &str,
    ) {
        if self.advisor.is_none() {
            return;
        }
        if self.advisor_mode == AdvisorMode::Work {
            return;
        }
        let current_prompt = self.prompts.read_class(class);
        let meta = self.prompts.render_adjust_meta(&[
            ("class", &class.to_string()),
            ("task", context),
            ("primary_output", primary_output),
            ("advisor_output", advisor_output),
            ("current_prompt", &current_prompt),
        ]);
        let Some(advisor) = self.advisor.as_mut() else {
            return;
        };
        let response = advisor.chat(
            "You are minipaw's offline coach.",
            &[ChatMessage::user(meta)],
        );
        let Some(directive) = parse_directive(class, &response) else {
            println!(
                "advisor adjust task={task_id} class={class} parsed=none raw={:?}",
                cap(&response, 160)
            );
            return;
        };
        if matches!(directive, AdjustmentDirective::NoChange) {
            println!("advisor adjust task={task_id} class={class} kind=no-change");
            return;
        }
        if let AdjustmentDirective::RuleAppend { class, .. } = &directive {
            // Cap how many auto-appended rules can accumulate per class so an
            // overzealous advisor cannot bloat the system prompt indefinitely.
            let existing = self.prompts.read_class(*class);
            let count = existing
                .lines()
                .filter(|line| {
                    let trimmed = line.trim_start();
                    trimmed
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit())
                        && trimmed.contains(". ")
                })
                .count();
            if count >= MAX_RULE_APPENDS_PER_CLASS {
                println!(
                    "advisor adjust task={task_id} class={class} skipped: rule cap reached"
                );
                return;
            }
        }
        let workspace = self.prompts.workspace().to_owned();
        match self.advisor_mode {
            AdvisorMode::Training => match apply_training(&workspace, &self.prompts, &directive) {
                Ok(outcome) => {
                    println!(
                        "advisor adjust task={task_id} mode=training applied: {outcome}"
                    );
                    let body = format!("[advisor adjust applied] {}", directive.summary());
                    self.memory.append_message(task_id, "advisor-adjust", &body);
                    if matches!(directive, AdjustmentDirective::SkillNew { .. }) {
                        // Re-load skills so the new file is visible to subsequent tasks.
                        self.skills =
                            SkillRegistry::load(&workspace.join("skills"));
                    }
                }
                Err(err) => eprintln!("advisor adjust apply failed: {err}"),
            },
            AdvisorMode::Trial => match write_proposal(&workspace, task_id, &directive) {
                Ok(path) => {
                    println!(
                        "advisor adjust task={task_id} mode=trial proposal={}",
                        path.display()
                    );
                    let body = format!(
                        "[advisor proposal] {} → {}",
                        directive.summary(),
                        path.display()
                    );
                    self.memory.append_message(task_id, "advisor-adjust", &body);
                }
                Err(err) => eprintln!("advisor adjust proposal failed: {err}"),
            },
            AdvisorMode::Work => {}
        }
    }

    fn run_shadow_query(&mut self, context: &str, target: AgentChoice) -> Option<String> {
        let system = "You are a shadow advisor. Provide your best single-turn answer to the task below; do not request tools or further input.";
        let messages = [ChatMessage::user(context.to_owned())];
        let response = match target {
            AgentChoice::Primary => self.llm.chat(system, &messages),
            AgentChoice::Advisor => self.advisor.as_mut()?.chat(system, &messages),
        };
        let trimmed = response.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    }

    fn record_divergence(&mut self, task_id: TaskId, record: &DivergenceRecord) {
        let body = record.render();
        println!(
            "advisor divergence task={task_id} class={} verdict={:?} similarity={:.2}",
            record.class, record.verdict, record.similarity
        );
        self.memory.append_message(task_id, "advisor-shadow", &body);
    }

    fn assemble_context(&self, msg: &IncomingMessage) -> (Option<TaskId>, String) {
        let Some(ref cid) = msg.context_id else {
            return (None, msg.text.clone());
        };

        let Some(raw_id) = self.memory.get_fact(&format!("ctx:{cid}")) else {
            return (None, msg.text.clone());
        };

        let Ok(id_num) = raw_id.parse::<u64>() else {
            return (None, msg.text.clone());
        };

        let task_id = TaskId(id_num);
        let history = self.memory.task_context(task_id, CONTEXT_MAX_BYTES);
        if history.is_empty() {
            return (Some(task_id), msg.text.clone());
        }

        let context = format!(
            "Conversation history:\n{history}\n\nNew message:\n{}",
            msg.text
        );
        (Some(task_id), context)
    }

    fn run_session(
        &mut self,
        task_id: TaskId,
        input: &str,
        context: &str,
        class: MessageClass,
        llm: &mut dyn LlmClient,
    ) -> (String, usize) {
        match class {
            MessageClass::MiniHow => self.run_minihow(task_id, input, context, llm),
            MessageClass::MiniWhy => self.run_miniwhy(task_id, context, llm),
            MessageClass::MiniWhat => self.run_miniwhat(task_id, context, llm),
        }
    }

    // MiniHow: execution task — proper multi-turn conversation with EXEC/DONE directives.
    fn run_minihow(
        &mut self,
        task_id: TaskId,
        input: &str,
        context: &str,
        llm: &mut dyn LlmClient,
    ) -> (String, usize) {
        let workspace = self.tools.policy().workspace.display().to_string();
        let system = self.prompts.render(
            MessageClass::MiniHow,
            &[
                ("soul", SOUL),
                ("os", &self.os_info),
                ("workspace", &workspace),
            ],
        );

        // The first user message carries session context (prior turns) if present,
        // otherwise just the current input.
        let first_user = if context != input {
            context.to_owned()
        } else {
            input.to_owned()
        };

        let mut messages = vec![ChatMessage::user(first_user)];
        let mut steps = 0;
        let mut last_cmd: Option<String> = None;
        let mut last_result: Option<String> = None;

        for _ in 0..MAX_SESSION_STEPS {
            steps += 1;
            let response = llm.chat(&system, &messages);

            // Scan the first few lines for a directive so preamble text before
            // the command doesn't prevent it from being found.
            let directive = response.lines().take(4).find_map(|line| {
                let t = line.trim();
                if t.starts_with("DONE:") || t.starts_with("EXEC:") {
                    Some(t.to_owned())
                } else {
                    None
                }
            });

            if let Some(ref d) = directive {
                if d.starts_with("DONE:") {
                    // Capture everything after DONE: including subsequent lines,
                    // since the LLM often puts the full answer on multiple lines.
                    let output = match response.find("DONE:") {
                        Some(pos) => response[pos + "DONE:".len()..].trim().to_owned(),
                        None => d["DONE:".len()..].trim().to_owned(),
                    };
                    println!("minihow done task={task_id} steps={steps}");
                    messages.push(ChatMessage::assistant(response));
                    self.memory.append_message(task_id, "advisor", &output);
                    return (output, steps);
                }

                if let Some(cmd) = d.strip_prefix("EXEC:") {
                    let cmd = cmd.trim();

                    // Loop detection: same command repeated — return last result.
                    if last_cmd.as_deref() == Some(cmd) {
                        if let Some(result) = last_result {
                            let output = result.trim().to_owned();
                            self.memory.append_message(task_id, "advisor", &output);
                            return (output, steps);
                        }
                    }

                    let result = self.dispatch_exec(cmd);
                    println!(
                        "minihow step={steps} exec={cmd:?} ok={} out={:?}",
                        !result.starts_with("error:"),
                        &result[..result.len().min(120)]
                    );

                    self.memory.append_message(
                        task_id,
                        "exec",
                        &format!("EXEC: {cmd}\nResult: {result}"),
                    );

                    messages.push(ChatMessage::assistant(response));
                    messages.push(ChatMessage::user(format!("Result of `{cmd}`:\n{result}")));

                    last_cmd = Some(cmd.to_owned());
                    last_result = Some(result);
                    continue;
                }
            }

            // No directive found — free-form response, treat as final answer.
            let output = response.trim().to_owned();
            println!("minihow free-form task={task_id} steps={steps}");
            messages.push(ChatMessage::assistant(response));
            self.memory.append_message(task_id, "advisor", &output);
            return (output, steps);
        }

        eprintln!("minihow step-limit task={task_id}");
        let output = format!("Session reached step limit ({MAX_SESSION_STEPS}).");
        (output, steps)
    }

    // MiniWhy: analysis task — LLM reasons, may request memory data via DATA: directive.
    fn run_miniwhy(
        &mut self,
        task_id: TaskId,
        context: &str,
        llm: &mut dyn LlmClient,
    ) -> (String, usize) {
        let system = self
            .prompts
            .render(MessageClass::MiniWhy, &[("soul", SOUL)]);

        let mut messages = vec![ChatMessage::user(context.to_owned())];
        let mut steps = 0;

        for _ in 0..MAX_SESSION_STEPS {
            steps += 1;
            println!("miniwhy step={steps} task={task_id}");
            let response = llm.chat(&system, &messages);
            let first_line = response.lines().next().unwrap_or("").trim().to_owned();

            if let Some(query) = first_line.strip_prefix("DATA:") {
                let query = query.trim();
                // Guard: reject shell-like content in DATA: — the LLM sometimes
                // tries to embed exec directives here. Treat those responses as
                // final answers instead of memory fetches.
                if !data_query_is_safe(query) {
                    println!("miniwhy data-rejected task={task_id} query={query:?}");
                    let output = response.trim().to_owned();
                    messages.push(ChatMessage::assistant(response));
                    self.memory.append_message(task_id, "advisor", &output);
                    return (output, steps);
                }
                let memory = self.memory.progressive_memory(
                    query,
                    MEMORY_INDEX_LIMIT,
                    MEMORY_DETAIL_LIMIT,
                    MEMORY_DETAIL_BYTES,
                );
                let data = memory.render();
                println!("miniwhy data-fetch task={task_id} query={query:?} bytes={}", data.len());
                self.memory.append_message(task_id, "data-fetch", &data);

                messages.push(ChatMessage::assistant(response));
                messages.push(ChatMessage::user(format!("Data for '{query}':\n{data}")));
                continue;
            }

            let output = response.trim().to_owned();
            println!("miniwhy done task={task_id} steps={steps}");
            messages.push(ChatMessage::assistant(response));
            self.memory.append_message(task_id, "advisor", &output);
            return (output, steps);
        }

        eprintln!("miniwhy step-limit task={task_id}");
        let output = format!("Analysis reached step limit ({MAX_SESSION_STEPS}).");
        (output, steps)
    }

    // MiniWhat: query task — single LLM call with SOUL + memory context in the system prompt.
    fn run_miniwhat(
        &mut self,
        task_id: TaskId,
        context: &str,
        llm: &mut dyn LlmClient,
    ) -> (String, usize) {
        println!("miniwhat task={task_id}");
        let memory = self.memory.progressive_memory(
            context,
            MEMORY_INDEX_LIMIT,
            MEMORY_DETAIL_LIMIT,
            MEMORY_DETAIL_BYTES,
        );
        let system = self.prompts.render(
            MessageClass::MiniWhat,
            &[("soul", SOUL), ("memory", &memory.render())],
        );

        let response = llm.chat(&system, &[ChatMessage::user(context.to_owned())]);
        let output = response.trim().to_owned();
        self.memory.append_message(task_id, "advisor", &output);
        (output, 1)
    }

    fn dispatch_exec(&self, command: &str) -> String {
        if let Some((name, rest)) = command.split_once(':') {
            let name = name.trim();
            let is_agent_name = !name.is_empty()
                && !name.contains(' ')
                && !name.chars().all(|c| c.is_ascii_digit() || c == '-');
            if is_agent_name {
                for agent in &self.agents {
                    if agent.name() == name {
                        return match agent.execute(rest.trim()) {
                            Ok(out) => out,
                            Err(err) => format!("error: {err}"),
                        };
                    }
                }
                return format!("error: no agent named '{name}'");
            }
        }
        for agent in &self.agents {
            if agent.name() == "exec" {
                return match agent.execute(command) {
                    Ok(out) => out,
                    Err(err) => format!("error: {err}"),
                };
            }
        }
        "error: no exec agent registered".to_owned()
    }
}

/// Return true when a DATA: query looks like a plain memory/search string.
/// Rejects shell-like content so the LLM cannot misuse DATA: as an exec path.
fn data_query_is_safe(query: &str) -> bool {
    !query.contains('|')
        && !query.contains(';')
        && !query.contains("&&")
        && !query.contains("$(")
        && !query.contains("exec.")
        && !query.starts_with('/')
        && !query.contains(">>")
        && !query.contains('<')
}

fn cap(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.replace('\n', "↵");
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].replace('\n', "↵"))
}

fn detect_os_info() -> String {
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        let pretty = content
            .lines()
            .find(|l| l.starts_with("PRETTY_NAME="))
            .and_then(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| v.trim_matches('"'));
        let id = content
            .lines()
            .find(|l| l.starts_with("ID="))
            .and_then(|l| l.strip_prefix("ID="))
            .map(|v| v.trim_matches('"'));
        if let Some(name) = pretty {
            let pkg = match id.unwrap_or("") {
                "debian" | "ubuntu" | "raspbian" => "apt",
                "fedora" | "rhel" | "centos" => "dnf/yum",
                "arch" => "pacman",
                "alpine" => "apk",
                _ => "the system package manager",
            };
            return format!("{name} (package manager: {pkg})");
        }
    }
    if let Ok(out) = std::process::Command::new("uname").args(["-s", "-r", "-m"]).output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            return s.trim().to_owned();
        }
    }
    "Unix".to_owned()
}
