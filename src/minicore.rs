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
    tools: ToolRunner,
    _skills: SkillRegistry,
    agents: Vec<Box<dyn AgentHandler>>,
    os_info: String,
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
            os_info: detect_os_info(),
            tools,
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

        let class = classify_message(&context, self.llm.as_mut());

        // Reuse the existing session task so all turns accumulate in one place;
        // create a new task only on the first message in a session.
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

        self.memory.append_message(task_id, "user", &msg.text);
        self.memory.update_task_status(task_id, TaskStatus::Running);
        let (output, steps) = self.run_session(task_id, &msg.text, &context, class);
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
        let mut last_exec: Option<String> = None;
        let mut last_result: Option<String> = None;

        for _ in 0..MAX_SESSION_STEPS {
            steps += 1;
            let workspace = self.tools.policy().workspace.display().to_string();
            let prompt = format!(
                "You are an execution advisor.\n\
                 OS: {}\n\
                 Workspace: {workspace}\n\
                 IMPORTANT RULES:\n\
                 1. NEVER compute arithmetic, logic, or any calculation yourself. \
                 Always delegate to a tool — use python3 -c for any math, no matter \
                 how simple (e.g. EXEC: python3 -c \"print(3+1)\").\n\
                 2. Use EXEC for all I/O: reading files, listing directories, \
                 network calls, running programs.\n\
                 3. Built-in commands always available without allowlisting: \
                 read <path>, ls [path].\n\
                 4. If a command returns \"exec denied\", try an alternative tool. \
                 Only give up if no alternative exists.\n\
                 5. Once the result you need already appears in the Context from a \
                 previous EXEC step, do NOT run that command again — issue DONE \
                 immediately with that result.\n\
                 6. When all needed data is in Context, respond with DONE.\n\
                 Goal: {input}\nContext:\n{accumulated}\n\n\
                 Respond with exactly one directive on the first line:\n\
                 EXEC: <shell command and args>  — run a shell command or calculation\n\
                 EXEC: telegram:<id>:<msg>       — send a telegram message\n\
                 DONE: <final answer>            — all done, report result to user",
                self.os_info
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

                // Loop detection: same command repeated — use last result as final answer.
                if last_exec.as_deref() == Some(cmd) {
                    if let Some(result) = last_result {
                        let output = result.trim().to_owned();
                        self.memory.append_message(task_id, "advisor", &output);
                        return (output, steps);
                    }
                }

                let result = self.dispatch_exec(cmd);
                println!("minihow step={steps} exec={cmd:?} ok={} out={:?}",
                    !result.starts_with("error:"),
                    &result[..result.len().min(120)]);
                let record = format!("EXEC: {cmd}\nResult: {result}");
                self.memory.append_message(task_id, "exec", &record);
                accumulated.push_str(&format!("\n{record}"));
                last_exec = Some(cmd.to_owned());
                last_result = Some(result);
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
        // Only treat as agent routing when the prefix is a single word (no spaces)
        // so that colons inside shell commands (e.g. python3 -c "x = 1: ...") are ignored.
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
    // Fallback: uname
    if let Ok(out) = std::process::Command::new("uname").args(["-s", "-r", "-m"]).output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            return s.trim().to_owned();
        }
    }
    "Unix".to_owned()
}
