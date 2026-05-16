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

const DEFAULT_MAX_SESSION_STEPS: usize = 16;

fn max_session_steps() -> usize {
    std::env::var("MINIPAW_MAX_SESSION_STEPS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_SESSION_STEPS)
}
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
    /// Optional per-task subclass override. When set, minihow loads the
    /// per-subclass overlay (e.g. `minihow.transport.md`) in addition to
    /// the base prompt. Used by pawbench to tag each case with its category
    /// during evaluation. At interactive runtime this is normally None
    /// (classifier-driven subclass routing is Phase 2 work).
    pub subclass: Option<String>,
}

pub struct SessionReport {
    pub task_id: TaskId,
    pub class: MessageClass,
    pub output: String,
    pub steps: usize,
}

/// Single EXEC step record collected during a minihow session — used by the
/// rubric module to score training runs without parsing stdout.
#[derive(Debug, Clone)]
pub struct ExecRecord {
    pub cmd: String,
    pub ok: bool,
    pub result: String,
}

/// Result of running one task end-to-end. `output` is the model's final
/// answer (DONE: payload or last assistant turn), `steps` is the number of
/// LLM turns consumed, `execs` is every EXEC the model issued in order.
#[derive(Debug, Clone)]
pub struct SessionTrace {
    pub output: String,
    pub steps: usize,
    pub execs: Vec<ExecRecord>,
}

impl SessionTrace {
    pub fn empty(output: String, steps: usize) -> Self {
        Self {
            output,
            steps,
            execs: Vec::new(),
        }
    }
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

    pub fn prompts(&self) -> &PromptStore {
        &self.prompts
    }

    pub fn skills_index_text(&self) -> String {
        self.skills.index_text()
    }

    /// Reload the on-disk skill registry. Call after writing a new skill
    /// file during training so subsequent primary runs see it.
    pub fn reload_skills(&mut self) {
        self.skills = SkillRegistry::load(&self.prompts.workspace().join("skills"));
    }

    /// Run one task in evaluation mode (no memory writes, robot_state snapshotted
    /// so each call leaves the workspace state untouched). Used by the ReAct
    /// trainer to score primary or advisor on a single case repeatedly.
    /// `subclass` controls which per-subclass overlay is appended to the
    /// minihow system prompt.
    pub fn run_eval(
        &mut self,
        executor: AgentChoice,
        task: &str,
        subclass: Option<&str>,
    ) -> SessionTrace {
        let state_snap = snapshot_robot_state(self.prompts.workspace());
        let task_id = TaskId(0);
        let trace = match executor {
            AgentChoice::Primary => {
                let mut llm = std::mem::replace(&mut self.llm, Box::new(crate::llm::OfflineLlm));
                let t = self.run_session_with_subclass(
                    task_id,
                    task,
                    task,
                    MessageClass::MiniHow,
                    llm.as_mut(),
                    false,
                    subclass,
                );
                self.llm = llm;
                t
            }
            AgentChoice::Advisor => {
                let Some(mut llm) = self.advisor.take() else {
                    return self.run_eval(AgentChoice::Primary, task, subclass);
                };
                let t = self.run_session_with_subclass(
                    task_id,
                    task,
                    task,
                    MessageClass::MiniHow,
                    llm.as_mut(),
                    false,
                    subclass,
                );
                self.advisor = Some(llm);
                t
            }
        };
        restore_robot_state(self.prompts.workspace(), &state_snap);
        trace
    }

    /// Ask advisor for one directive given explicit inputs (used by ReAct
    /// loop). `prior_attempts` is a free-form text block that the
    /// adjust-meta template can show advisor so it varies its proposal
    /// across attempts. Returns None if no advisor configured or response
    /// is unparseable.
    pub fn ask_directive(
        &mut self,
        case_task: &str,
        primary_output: &str,
        advisor_output: &str,
        class: MessageClass,
        subclass: &str,
        prior_attempts: &str,
        rubric: &str,
    ) -> Option<AdjustmentDirective> {
        let current_main = self.prompts.read_class(class);
        let overlay = self
            .prompts
            .read_subclass(class, subclass)
            .unwrap_or_default();
        let current_prompt = if overlay.is_empty() {
            current_main
        } else {
            format!(
                "{}\n\n[per-subclass overlay {}]\n{}",
                current_main, subclass, overlay
            )
        };
        let available_skills = self.skills.index_text();
        let meta = self.prompts.render_adjust_meta(&[
            ("class", &class.to_string()),
            ("subclass", subclass),
            ("task", case_task),
            ("primary_output", primary_output),
            ("advisor_output", advisor_output),
            ("current_prompt", &current_prompt),
            ("available_skills", &available_skills),
            ("prior_attempts", prior_attempts),
            ("rubric", rubric),
        ]);
        let advisor = self.advisor.as_mut()?;
        let response = advisor.chat(
            "You are minipaw's offline coach.",
            &[ChatMessage::user(meta)],
        );
        parse_directive(class, &response)
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

        // If we will shadow, snapshot the workspace's transient state (robot_state/)
        // so the shadow can start from the same initial state the executor saw.
        let shadow_planned = self.should_shadow(executor);
        let initial_state = if shadow_planned {
            Some(snapshot_robot_state(self.prompts.workspace()))
        } else {
            None
        };

        let trace = self.run_with_executor(
            task_id,
            &msg.text,
            &context,
            class,
            executor,
            msg.subclass.as_deref(),
        );

        if let Some(initial) = initial_state {
            // Capture the executor's final state, run shadow against the pre-executor state,
            // then restore executor's state so it remains the "official" run.
            let exec_state = snapshot_robot_state(self.prompts.workspace());
            restore_robot_state(self.prompts.workspace(), &initial);
            self.run_shadow(task_id, &msg.text, &context, class, executor, &trace.output);
            restore_robot_state(self.prompts.workspace(), &exec_state);
        }

        let final_status = if trace.output.starts_with("error:") {
            TaskStatus::Failed
        } else {
            TaskStatus::Done
        };
        self.memory.update_task_status(task_id, final_status);
        self.memory.append_message(task_id, "assistant", &trace.output);

        SessionReport {
            task_id,
            class,
            output: trace.output,
            steps: trace.steps,
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
        subclass: Option<&str>,
    ) -> SessionTrace {
        match executor {
            AgentChoice::Primary => {
                let mut llm = std::mem::replace(&mut self.llm, Box::new(crate::llm::OfflineLlm));
                let result = self.run_session_with_subclass(
                    task_id,
                    input,
                    context,
                    class,
                    llm.as_mut(),
                    true,
                    subclass,
                );
                self.llm = llm;
                result
            }
            AgentChoice::Advisor => {
                let Some(mut llm) = self.advisor.take() else {
                    return self.run_with_executor(
                        task_id, input, context, class, AgentChoice::Primary, subclass,
                    );
                };
                let result = self.run_session_with_subclass(
                    task_id,
                    input,
                    context,
                    class,
                    llm.as_mut(),
                    true,
                    subclass,
                );
                self.advisor = Some(llm);
                result
            }
        }
    }

    fn run_shadow(
        &mut self,
        task_id: TaskId,
        input: &str,
        context: &str,
        class: MessageClass,
        executor: AgentChoice,
        primary_output: &str,
    ) {
        let shadow_choice = match executor {
            AgentChoice::Primary => AgentChoice::Advisor,
            AgentChoice::Advisor => AgentChoice::Primary,
        };
        // Shadow now runs the full session with tool access (record_memory=false so
        // the shadow's EXEC results don't pollute the executor's task transcript).
        // The caller has already restored robot_state so shadow starts from the
        // same initial state the executor saw.
        let shadow_output = match shadow_choice {
            AgentChoice::Primary => {
                let mut llm = std::mem::replace(&mut self.llm, Box::new(crate::llm::OfflineLlm));
                let trace = self.run_session(task_id, input, context, class, llm.as_mut(), false);
                self.llm = llm;
                trace.output
            }
            AgentChoice::Advisor => {
                let Some(mut llm) = self.advisor.take() else { return };
                let trace = self.run_session(task_id, input, context, class, llm.as_mut(), false);
                self.advisor = Some(llm);
                trace.output
            }
        };
        let trimmed = shadow_output.trim();
        if trimmed.is_empty() {
            return;
        }
        let shadow = trimmed.to_owned();

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
        let available_skills = self.skills.index_text();
        let meta = self.prompts.render_adjust_meta(&[
            ("class", &class.to_string()),
            ("subclass", "(not used in legacy advisor mode)"),
            ("task", context),
            ("primary_output", primary_output),
            ("advisor_output", advisor_output),
            ("current_prompt", &current_prompt),
            ("available_skills", &available_skills),
            ("prior_attempts", "(not used in legacy advisor mode)"),
            ("rubric", "(not used in legacy advisor mode — train command provides rubric)"),
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
            AdvisorMode::Training => match apply_training(&workspace, &self.prompts, &directive, None) {
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
        record_memory: bool,
    ) -> SessionTrace {
        self.run_session_with_subclass(task_id, input, context, class, llm, record_memory, None)
    }

    /// Same as `run_session` but renders the per-subclass overlay (if any)
    /// into the minihow system prompt. Used by the ReAct trainer so primary
    /// sees rules scoped to the current task subclass.
    fn run_session_with_subclass(
        &mut self,
        task_id: TaskId,
        input: &str,
        context: &str,
        class: MessageClass,
        llm: &mut dyn LlmClient,
        record_memory: bool,
        subclass: Option<&str>,
    ) -> SessionTrace {
        match class {
            MessageClass::MiniHow => {
                self.run_minihow(task_id, input, context, llm, record_memory, subclass)
            }
            MessageClass::MiniWhy => self.run_miniwhy(task_id, context, llm, record_memory),
            MessageClass::MiniWhat => self.run_miniwhat(task_id, context, llm, record_memory),
        }
    }

    // MiniHow: execution task — proper multi-turn conversation with EXEC/DONE directives.
    fn run_minihow(
        &mut self,
        task_id: TaskId,
        input: &str,
        context: &str,
        llm: &mut dyn LlmClient,
        record_memory: bool,
        subclass: Option<&str>,
    ) -> SessionTrace {
        let workspace = self.tools.policy().workspace.display().to_string();
        let skills_index = self.skills.index_text();
        let system = self.prompts.render_with_subclass(
            MessageClass::MiniHow,
            subclass,
            &[
                ("soul", SOUL),
                ("os", &self.os_info),
                ("workspace", &workspace),
                ("skills", &skills_index),
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
        let mut execs: Vec<ExecRecord> = Vec::new();
        let max_steps = max_session_steps();

        // Stdout label. Shadow runs use a different prefix so pawbench's
        // `^minihow step=` regex doesn't accidentally count shadow's tool
        // calls toward the executor's score (the executor is the only run
        // whose transcript should drive verdicts).
        let tag = if record_memory { "minihow" } else { "shadow" };

        for _ in 0..max_steps {
            steps += 1;
            let response = llm.chat(&system, &messages);

            // Collect every EXEC: / DONE: directive in order — scan the full
            // response so a model that preambles or batches multiple commands
            // in a single turn still has all its directives honored. An EXEC:
            // command is allowed to span multiple lines when its quoting is
            // unbalanced on the first line (e.g. `EXEC: python3 -c "` followed
            // by a multi-line script); the parser keeps appending lines until
            // the quote balances or another directive is encountered.
            let directives = collect_directives(&response);

            if directives.is_empty() {
                // Free-form response — treat as the final answer.
                let output = response.trim().to_owned();
                println!("{tag} free-form task={task_id} steps={steps}");
                messages.push(ChatMessage::assistant(response));
                if record_memory {
                    self.memory.append_message(task_id, "advisor", &output);
                }
                return SessionTrace { output, steps, execs };
            }

            // Execute every EXEC in order until the first DONE (which terminates).
            let mut combined = String::new();

            for d in &directives {
                if d.starts_with("DONE:") {
                    let output = match response.find("DONE:") {
                        Some(pos) => response[pos + "DONE:".len()..].trim().to_owned(),
                        None => d["DONE:".len()..].trim().to_owned(),
                    };
                    println!("{tag} done task={task_id} steps={steps}");
                    messages.push(ChatMessage::assistant(response.clone()));
                    if record_memory {
                        self.memory.append_message(task_id, "advisor", &output);
                    }
                    return SessionTrace { output, steps, execs };
                }

                let Some(cmd) = d.strip_prefix("EXEC:") else { continue };
                let cmd = cmd.trim();

                // Loop detection: identical to the most recent dispatched command.
                if last_cmd.as_deref() == Some(cmd) {
                    if let Some(result) = last_result.clone() {
                        if combined.is_empty() {
                            // Nothing executed this turn yet — return cached result.
                            let output = result.trim().to_owned();
                            if record_memory {
                                self.memory.append_message(task_id, "advisor", &output);
                            }
                            return SessionTrace { output, steps, execs };
                        }
                        // Otherwise fold the cached result into the combined feedback
                        // and stop executing further directives this turn so the LLM
                        // can re-plan with the accumulated context.
                        combined.push_str(&format!(
                            "Result of `{cmd}` (cached, loop suppressed):\n{}\n\n",
                            result
                        ));
                        break;
                    }
                }

                let result = self.dispatch_exec(cmd);
                let ok = !result.starts_with("error:");
                execs.push(ExecRecord {
                    cmd: cmd.to_owned(),
                    ok,
                    result: result.clone(),
                });
                if record_memory {
                    println!(
                        "{tag} step={steps} exec={cmd:?} ok={} out={:?}",
                        ok,
                        &result[..result.len().min(120)]
                    );
                } else {
                    // Shadow runs: omit out= to avoid substring-pollution into
                    // pawbench's must_in_exec scan. cmd is still useful for debug.
                    println!("{tag} step={steps} exec={cmd:?} ok={}", ok);
                }

                if record_memory {
                    self.memory.append_message(
                        task_id,
                        "exec",
                        &format!("EXEC: {cmd}\nResult: {result}"),
                    );
                }

                combined.push_str(&format!("Result of `{cmd}`:\n{result}\n\n"));

                last_cmd = Some(cmd.to_owned());
                last_result = Some(result);
            }

            if combined.is_empty() {
                // Directives were present but none was a real EXEC (e.g. malformed).
                let output = response.trim().to_owned();
                println!("{tag} no-exec task={task_id} steps={steps}");
                messages.push(ChatMessage::assistant(response));
                if record_memory {
                    self.memory.append_message(task_id, "advisor", &output);
                }
                return SessionTrace { output, steps, execs };
            }

            messages.push(ChatMessage::assistant(response));
            messages.push(ChatMessage::user(combined.trim_end().to_owned()));
        }

        eprintln!("{tag} step-limit task={task_id}");
        let output = format!("Session reached step limit ({max_steps}).");
        SessionTrace { output, steps, execs }
    }

    // MiniWhy: analysis task — LLM reasons, may request memory data via DATA: directive.
    fn run_miniwhy(
        &mut self,
        task_id: TaskId,
        context: &str,
        llm: &mut dyn LlmClient,
        record_memory: bool,
    ) -> SessionTrace {
        let system = self
            .prompts
            .render(MessageClass::MiniWhy, &[("soul", SOUL)]);

        let mut messages = vec![ChatMessage::user(context.to_owned())];
        let mut steps = 0;
        let max_steps = max_session_steps();

        for _ in 0..max_steps {
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
                    if record_memory {
                        self.memory.append_message(task_id, "advisor", &output);
                    }
                    return SessionTrace::empty(output, steps);
                }
                let memory = self.memory.progressive_memory(
                    query,
                    MEMORY_INDEX_LIMIT,
                    MEMORY_DETAIL_LIMIT,
                    MEMORY_DETAIL_BYTES,
                );
                let data = memory.render();
                println!("miniwhy data-fetch task={task_id} query={query:?} bytes={}", data.len());
                if record_memory {
                    self.memory.append_message(task_id, "data-fetch", &data);
                }

                messages.push(ChatMessage::assistant(response));
                messages.push(ChatMessage::user(format!("Data for '{query}':\n{data}")));
                continue;
            }

            let output = response.trim().to_owned();
            println!("miniwhy done task={task_id} steps={steps}");
            messages.push(ChatMessage::assistant(response));
            if record_memory {
                self.memory.append_message(task_id, "advisor", &output);
            }
            return SessionTrace::empty(output, steps);
        }

        eprintln!("miniwhy step-limit task={task_id}");
        let output = format!("Analysis reached step limit ({max_steps}).");
        SessionTrace::empty(output, steps)
    }

    // MiniWhat: query task — single LLM call with SOUL + memory context in the system prompt.
    fn run_miniwhat(
        &mut self,
        task_id: TaskId,
        context: &str,
        llm: &mut dyn LlmClient,
        record_memory: bool,
    ) -> SessionTrace {
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
        if record_memory {
            self.memory.append_message(task_id, "advisor", &output);
        }
        SessionTrace::empty(output, 1)
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
        let resolved = self.resolve_skill_invocation(command);
        let target = resolved.as_deref().unwrap_or(command);
        for agent in &self.agents {
            if agent.name() == "exec" {
                return match agent.execute(target) {
                    Ok(out) => out,
                    Err(err) => format!("error: {err}"),
                };
            }
        }
        "error: no exec agent registered".to_owned()
    }

    /// When the LLM emits `EXEC: <skill-name> [args...]` instead of the full
    /// exec command, swap the skill name for the registered exec line so the
    /// tool runner receives a real shell command. Falls back to the original
    /// string when the first token is not a known skill name.
    fn resolve_skill_invocation(&self, command: &str) -> Option<String> {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return None;
        }
        let (head, tail) = match trimmed.split_once(char::is_whitespace) {
            Some((h, t)) => (h, t),
            None => (trimmed, ""),
        };
        let skill = self.skills.find(head)?;
        let exec = skill.exec.as_deref()?;
        let result = if tail.is_empty() {
            exec.to_owned()
        } else {
            format!("{exec} {tail}")
        };
        println!("minicore skill-resolve from={head:?} to={result:?}");
        Some(result)
    }
}

/// In-memory snapshot of files under `<workspace>/robot_state/`. Used to
/// pin the executor's starting state so the shadow can rewind to it and
/// run with tool access without contaminating the executor's transcript.
/// Empty/None when the dir doesn't exist (typical for non-pawbench runs).
type RobotStateSnapshot = Vec<(String, Vec<u8>)>;

fn snapshot_robot_state(workspace: &std::path::Path) -> RobotStateSnapshot {
    let state_dir = workspace.join("robot_state");
    let Ok(entries) = std::fs::read_dir(&state_dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let (Some(name), Ok(bytes)) = (
                path.file_name().and_then(|n| n.to_str()),
                std::fs::read(&path),
            ) {
                files.push((name.to_owned(), bytes));
            }
        }
    }
    files
}

fn restore_robot_state(workspace: &std::path::Path, snapshot: &RobotStateSnapshot) {
    let state_dir = workspace.join("robot_state");
    if state_dir.exists() {
        let _ = std::fs::remove_dir_all(&state_dir);
    }
    if snapshot.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&state_dir);
    for (name, bytes) in snapshot {
        let _ = std::fs::write(state_dir.join(name), bytes);
    }
}

/// Collect EXEC: / DONE: directives from an LLM response in document order.
/// An EXEC: directive may span multiple lines when its first line has
/// unbalanced quotes — typical when the model writes
///
///     EXEC: python3 -c "
///     import sys
///     ...
///     "
///
/// The collector keeps appending subsequent lines (joined by '\n') until the
/// quoting balances or another directive line is reached. DONE: never spans
/// multiple lines (everything after DONE: is the final answer payload).
fn collect_directives(response: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lines: Vec<&str> = response.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with("DONE:") {
            out.push(trimmed.trim_end().to_owned());
            i += 1;
            continue;
        }
        if trimmed.starts_with("EXEC:") {
            // Re-take the substring without trimming the leading EXEC: so quote
            // counting starts from the actual command body.
            let mut buf = trimmed.to_owned();
            let body_start = "EXEC:".len();
            i += 1;
            while quote_balance(&buf[body_start..]) != 0 && i < lines.len() {
                let next_trimmed = lines[i].trim_start();
                if next_trimmed.starts_with("EXEC:") || next_trimmed.starts_with("DONE:") {
                    break;
                }
                buf.push('\n');
                buf.push_str(lines[i]);
                i += 1;
            }
            out.push(buf);
            continue;
        }
        i += 1;
    }
    out
}

/// Track unbalanced quote state ignoring backslash-escaped quote chars.
/// Returns 0 when balanced, nonzero when an open quote of some kind remains.
/// The exact nonzero value is opaque; callers should only check equality to 0.
fn quote_balance(text: &str) -> i32 {
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // Skip the next character (escape) only when inside a double quote
                // or a normal context. Single quotes don't process backslashes,
                // but inside python -c "..." they often do.
                if in_double {
                    let _ = chars.next();
                }
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }
    (in_single as i32) + (in_double as i32)
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
