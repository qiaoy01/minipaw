use crate::channels::AgentHandler;
use crate::llm::{ChatMessage, LlmClient};
use crate::memory::MemoryStore;
use crate::planner::classify_message;
use crate::skills::SkillRegistry;
use crate::tools::ToolRunner;
use crate::types::{MessageClass, TaskId, TaskStatus};

const MAX_SESSION_STEPS: usize = 16;
const CONTEXT_MAX_BYTES: usize = 6144;
const MEMORY_INDEX_LIMIT: usize = 12;
const MEMORY_DETAIL_LIMIT: usize = 4;
const MEMORY_DETAIL_BYTES: usize = 512;

// Embedded at compile time so it is always available regardless of install path.
const SOUL: &str = include_str!("../SOUL.md");

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
    skills: SkillRegistry,
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
            skills,
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

        println!(
            "minicore task={task_id} class={class} source={} resumed={}",
            msg.source,
            prior_task_id.is_some()
        );
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

    // MiniHow: execution task — proper multi-turn conversation with EXEC/DONE directives.
    fn run_minihow(&mut self, task_id: TaskId, input: &str, context: &str) -> (String, usize) {
        let workspace = self.tools.policy().workspace.display().to_string();
        let system = format!(
            "{SOUL}\n\n\
             OS: {}\nWorkspace: {workspace}\n\n\
             Your response MUST start with one of these directives on the very first line — \
             no explanation, no preamble, no reasoning before it:\n\
             EXEC: <shell command>   — run a shell command or calculation\n\
             DONE: <final answer>    — task complete, report result\n\n\
             Rules:\n\
             1. NEVER compute arithmetic yourself — use EXEC: python3 -c \"print(expr)\" for all math.\n\
             2. When reading a file, use EXEC: cat <path> or python3 -c \"print(int(open('<path>').read().strip()) + N)\".\n\
             3. ALWAYS cast file contents to int before arithmetic: int(open(path).read().strip()).\n\
             4. Do NOT issue DONE until EVERY requested step has been executed and its result appears in the conversation.\n\
             5. If EXEC is denied, try an alternative. Only give up when no alternative exists.\n\
             6. Put EXEC: or DONE: on line 1. Never write text before the directive.\n\
             7. Every EXEC: command MUST fit on a single line — no newlines inside the command. \
             Chain Python statements with semicolons: python3 -c \"stmt1; stmt2; stmt3\". \
             NEVER use heredocs or multi-line python3 -c strings — only the first line is read.\n\
             8. When a computation involves multiple distinct quantities, print each one \
             on its own labeled line before printing the combined result, e.g.: \
             python3 -c \"h=16; ts=1234; print(f'hour={{h}} ts={{ts}} total={{ts+h}}')\". \
             This makes every intermediate value traceable.\n\
             9. In DONE, only cite numbers that explicitly appeared in prior EXEC output. \
             Do not reconstruct arithmetic from memory — re-read the EXEC results above.",
            self.os_info
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
            let response = self.llm.chat(&system, &messages);

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
                    let mut preview_end = result.len().min(120);
                    while preview_end > 0 && !result.is_char_boundary(preview_end) {
                        preview_end -= 1;
                    }
                    println!(
                        "minihow step={steps} exec={cmd:?} ok={} out={:?}",
                        !result.starts_with("error:"),
                        &result[..preview_end]
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
    fn run_miniwhy(&mut self, task_id: TaskId, context: &str) -> (String, usize) {
        let system = format!(
            "{SOUL}\n\n\
             You are an analysis advisor. Analyze the context and provide insights.\n\
             To fetch more data, respond with DATA: <query> on the first line.\n\
             Otherwise provide your analysis directly."
        );

        let mut messages = vec![ChatMessage::user(context.to_owned())];
        let mut steps = 0;

        for _ in 0..MAX_SESSION_STEPS {
            steps += 1;
            println!("miniwhy step={steps} task={task_id}");
            let response = self.llm.chat(&system, &messages);
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
    fn run_miniwhat(&mut self, task_id: TaskId, context: &str) -> (String, usize) {
        println!("miniwhat task={task_id}");
        let memory = self.memory.progressive_memory(
            context,
            MEMORY_INDEX_LIMIT,
            MEMORY_DETAIL_LIMIT,
            MEMORY_DETAIL_BYTES,
        );
        let system = format!(
            "{SOUL}\n\n\
             You are a query advisor. Answer the question concisely.\n\
             If your answer may be incomplete, outdated, or based on uncertain \
             knowledge, say so explicitly at the start of your response.\n\
             {}",
            memory.render()
        );

        let response = self.llm.chat(&system, &[ChatMessage::user(context.to_owned())]);
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
