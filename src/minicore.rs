use crate::channels::AgentHandler;
use crate::llm::LlmClient;
use crate::memory::MemoryStore;
use crate::planner::classify_message;
use crate::skills::SkillRegistry;
use crate::tools::ToolRunner;
use crate::types::{MessageClass, TaskId, TaskStatus};

const MAX_SESSION_STEPS: usize = 8;
const CONTEXT_MAX_BYTES: usize = 2048;
const MEMORY_INDEX_LIMIT: usize = 12;
const MEMORY_DETAIL_LIMIT: usize = 4;
const MEMORY_DETAIL_BYTES: usize = 512;

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
    _tools: ToolRunner,
    _skills: SkillRegistry,
    agents: Vec<Box<dyn AgentHandler>>,
}

impl MiniCore {
    pub fn new(
        memory: Box<dyn MemoryStore>,
        llm: Box<dyn LlmClient>,
        tools: ToolRunner,
        skills: SkillRegistry,
    ) -> Self {
        Self {
            memory,
            llm,
            _tools: tools,
            _skills: skills,
            agents: Vec::new(),
        }
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

    /// Run an exec command directly via the registered exec agent (bypass advisor).
    pub fn run_exec(&self, command: &str) -> String {
        self.dispatch_exec(command)
    }

    pub fn process(&mut self, msg: IncomingMessage) -> SessionReport {
        let (prior_task_id, context) = self.assemble_context(&msg);

        let class = classify_message(&context, self.llm.as_mut());

        let task = self.memory.create_task(&msg.text);
        self.memory.append_message(task.id, "user", &msg.text);

        // Bind context_id → task_id for first message in a session;
        // subsequent messages with the same context_id will find this mapping.
        if let Some(ref cid) = msg.context_id {
            if prior_task_id.is_none() {
                self.memory
                    .set_fact(&format!("ctx:{cid}"), &task.id.0.to_string());
            }
        }

        self.memory.update_task_status(task.id, TaskStatus::Running);
        let (output, steps) = self.run_session(task.id, &msg.text, &context, class);
        let final_status = if output.starts_with("error:") {
            TaskStatus::Failed
        } else {
            TaskStatus::Done
        };
        self.memory.update_task_status(task.id, final_status);
        self.memory.append_message(task.id, "assistant", &output);

        SessionReport {
            task_id: task.id,
            class,
            output,
            steps,
        }
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
    ) -> (String, usize) {
        match class {
            MessageClass::MiniHow => self.run_minihow(task_id, input, context),
            MessageClass::MiniWhy => self.run_miniwhy(task_id, context),
            MessageClass::MiniWhat => self.run_miniwhat(task_id, context),
        }
    }

    // MiniHow: execution task — LLM advises, agents execute, loop until DONE.
    fn run_minihow(&mut self, task_id: TaskId, input: &str, context: &str) -> (String, usize) {
        let mut accumulated = context.to_owned();
        let mut steps = 0;

        for _ in 0..MAX_SESSION_STEPS {
            steps += 1;
            let agent_names: Vec<&str> = self.agents.iter().map(|a| a.name()).collect();
            let prompt = format!(
                "You are an execution advisor. Registered agents: {}.\n\
                 Goal: {input}\nContext:\n{accumulated}\n\n\
                 Respond with exactly one directive on the first line:\n\
                 EXEC: <command>          — run via exec agent\n\
                 EXEC: telegram:<id>:<msg> — send via telegram agent\n\
                 DONE: <final report>     — task complete",
                agent_names.join(", ")
            );
            let response = self.llm.next_step(&prompt);
            let first_line = response.lines().next().unwrap_or("").trim();

            if let Some(rest) = first_line.strip_prefix("DONE:") {
                let output = rest.trim().to_owned();
                self.memory.append_message(task_id, "advisor", &output);
                return (output, steps);
            }

            if let Some(cmd) = first_line.strip_prefix("EXEC:") {
                let cmd = cmd.trim();
                let result = self.dispatch_exec(cmd);
                let record = format!("EXEC: {cmd}\nResult: {result}");
                self.memory.append_message(task_id, "exec", &record);
                accumulated.push_str(&format!("\n{record}"));
                continue;
            }

            // Free-form response: treat as final answer.
            let output = response.trim().to_owned();
            self.memory.append_message(task_id, "advisor", &output);
            return (output, steps);
        }

        let output = format!("Session reached step limit ({MAX_SESSION_STEPS}).");
        (output, steps)
    }

    // MiniWhy: analysis task — LLM reasons, may request memory data.
    fn run_miniwhy(&mut self, task_id: TaskId, context: &str) -> (String, usize) {
        let mut accumulated = context.to_owned();
        let mut steps = 0;

        for _ in 0..MAX_SESSION_STEPS {
            steps += 1;
            let prompt = format!(
                "You are an analysis advisor. Analyze the context and provide insights.\n\
                 To fetch more data, respond with DATA: <query> on the first line.\n\
                 Otherwise provide your analysis directly.\n\nContext:\n{accumulated}"
            );
            let response = self.llm.next_step(&prompt);
            let first_line = response.lines().next().unwrap_or("").trim();

            if let Some(query) = first_line.strip_prefix("DATA:") {
                let query = query.trim();
                let memory =
                    self.memory
                        .progressive_memory(query, MEMORY_INDEX_LIMIT, MEMORY_DETAIL_LIMIT, MEMORY_DETAIL_BYTES);
                let data = memory.render();
                self.memory.append_message(task_id, "data-fetch", &data);
                accumulated.push_str(&format!("\nData for '{query}':\n{data}"));
                continue;
            }

            let output = response.trim().to_owned();
            self.memory.append_message(task_id, "advisor", &output);
            return (output, steps);
        }

        let output = format!("Analysis reached step limit ({MAX_SESSION_STEPS}).");
        (output, steps)
    }

    // MiniWhat: query task — single LLM call with memory context.
    fn run_miniwhat(&mut self, task_id: TaskId, context: &str) -> (String, usize) {
        let memory = self.memory.progressive_memory(
            context,
            MEMORY_INDEX_LIMIT,
            MEMORY_DETAIL_LIMIT,
            MEMORY_DETAIL_BYTES,
        );
        let prompt = format!(
            "You are a query advisor. Answer the question concisely.\n\
             {}\nQuestion:\n{context}",
            memory.render()
        );
        let response = self.llm.next_step(&prompt);
        let output = response.trim().to_owned();
        self.memory.append_message(task_id, "advisor", &output);
        (output, 1)
    }

    fn dispatch_exec(&self, command: &str) -> String {
        // "agent_name:rest" routes to a specific agent; bare command defaults to exec.
        if let Some((name, rest)) = command.split_once(':') {
            let name = name.trim();
            // Exclude numeric prefixes (e.g. Telegram chat IDs like "12345:message")
            // to avoid misrouting those to a non-existent "12345" agent.
            if !name.chars().all(|c| c.is_ascii_digit() || c == '-') {
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
