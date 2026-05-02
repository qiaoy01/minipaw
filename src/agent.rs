use crate::llm::LlmClient;
use crate::memory::MemoryStore;
use crate::orchestration::{
    AgentLoop, Gateway, GatewayTarget, MapReduceReport, PipelineReport, PipelineStage, QueuedTask,
    TaskQueue, WorkerAgent,
};
use crate::planner::Planner;
use crate::tools::ToolRunner;
use crate::types::{
    AgentPattern, Heartbeat, Plan, StepStatus, Task, TaskId, TaskStatus, ToolResult, WorkerId,
};

const MEMORY_INDEX_LIMIT: usize = 12;
const MEMORY_DETAIL_LIMIT: usize = 4;
const MEMORY_DETAIL_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub task_id: crate::types::TaskId,
    pub status: TaskStatus,
    pub output: String,
    pub pattern: AgentPattern,
}

pub struct AgentOrchestrator {
    memory: Box<dyn MemoryStore>,
    planner: Planner,
    tools: ToolRunner,
    gateway: Gateway,
    queue: TaskQueue,
    workers: Vec<WorkerAgent>,
    loop_state: AgentLoop,
}

impl AgentOrchestrator {
    pub fn new(memory: Box<dyn MemoryStore>, tools: ToolRunner) -> Self {
        Self {
            memory,
            planner: Planner,
            tools,
            gateway: Gateway::default(),
            queue: TaskQueue::default(),
            workers: vec![
                WorkerAgent::new(WorkerId(1), "local-reader"),
                WorkerAgent::new(WorkerId(2), "local-operator"),
            ],
            loop_state: AgentLoop::default(),
        }
    }

    pub fn memory(&self) -> &dyn MemoryStore {
        self.memory.as_ref()
    }

    pub fn memory_mut(&mut self) -> &mut dyn MemoryStore {
        self.memory.as_mut()
    }

    pub fn heartbeat(&self) -> &Heartbeat {
        self.loop_state.heartbeat()
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn enqueue_task(&mut self, input: &str) -> TaskId {
        self.enqueue_task_with_dependencies(input, Vec::new(), AgentPattern::CoordinatorWorker)
    }

    pub fn enqueue_task_with_dependencies(
        &mut self,
        input: &str,
        dependencies: Vec<TaskId>,
        pattern: AgentPattern,
    ) -> TaskId {
        let task = self.memory.create_task(input);
        self.memory.append_message(task.id, "user", input);
        self.queue.push(QueuedTask {
            id: task.id,
            input: input.to_owned(),
            dependencies,
            status: TaskStatus::New,
            pattern,
        });
        task.id
    }

    pub fn tick(&mut self, llm: &mut dyn LlmClient) -> Option<AgentOutcome> {
        self.loop_state.tick_started();
        let Some(queued) = self.queue.next_ready() else {
            self.loop_state.tick_finished(None, TaskStatus::Waiting);
            return None;
        };

        let outcome = self.run_queued_task(queued, llm);
        self.queue.mark_finished(outcome.task_id, outcome.status);
        self.loop_state
            .tick_finished(Some(outcome.task_id), outcome.status);
        Some(outcome)
    }

    pub fn run_task(&mut self, input: &str, llm: &mut dyn LlmClient) -> AgentOutcome {
        let task = self.memory.create_task(input);
        self.memory.append_message(task.id, "user", input);
        let memory = self.memory.progressive_memory(
            input,
            MEMORY_INDEX_LIMIT,
            MEMORY_DETAIL_LIMIT,
            MEMORY_DETAIL_BYTES,
        );
        let pattern = self.planner.select_pattern_with_memory(input, &memory, llm);
        self.execute_task(task, input, pattern, llm)
    }

    pub fn run_pipeline(
        &mut self,
        input: &str,
        stages: &[PipelineStage],
        llm: &mut dyn LlmClient,
    ) -> PipelineReport {
        let task = self.memory.create_task(input);
        self.memory.update_task_status(task.id, TaskStatus::Running);
        self.memory.append_message(task.id, "user", input);

        let mut current = input.to_owned();
        for stage in stages {
            let memory = self.memory.progressive_memory(
                &current,
                MEMORY_INDEX_LIMIT,
                MEMORY_DETAIL_LIMIT,
                MEMORY_DETAIL_BYTES,
            );
            let stage_prompt = format!(
                "Pipeline stage '{}'. Use progressive memory index first, then selected details.\n{}\nInput:\n{}\nInstruction:\n{}",
                stage.name,
                memory.render(),
                current,
                stage.input
            );
            current = llm.next_step(&stage_prompt);
            self.memory.append_message(task.id, &stage.name, &current);
        }

        self.memory.append_message(task.id, "assistant", &current);
        self.memory.update_task_status(task.id, TaskStatus::Done);
        PipelineReport {
            task_id: task.id,
            stages_run: stages.len(),
            output: current,
        }
    }

    pub fn run_map_reduce(
        &mut self,
        input: &str,
        items: &[String],
        llm: &mut dyn LlmClient,
    ) -> MapReduceReport {
        let task = self.memory.create_task(input);
        self.memory.update_task_status(task.id, TaskStatus::Running);
        self.memory.append_message(task.id, "user", input);

        let mut mapped = Vec::new();
        for item in items {
            let memory = self.memory.progressive_memory(
                item,
                MEMORY_INDEX_LIMIT,
                MEMORY_DETAIL_LIMIT,
                MEMORY_DETAIL_BYTES,
            );
            let prompt = format!(
                "Map this item for task '{}'. Use progressive memory only if relevant.\n{}\nItem:\n{}",
                input,
                memory.render(),
                item
            );
            let output = llm.next_step(&prompt);
            self.memory.append_message(task.id, "map-worker", &output);
            mapped.push(output);
        }

        let memory = self.memory.progressive_memory(
            input,
            MEMORY_INDEX_LIMIT,
            MEMORY_DETAIL_LIMIT,
            MEMORY_DETAIL_BYTES,
        );
        let reduce_prompt = format!(
            "Reduce these {} mapped results into one concise answer. Use progressive memory for continuity.\n{}\nMapped results:\n{}",
            mapped.len(),
            memory.render(),
            mapped.join("\n---\n")
        );
        let output = llm.next_step(&reduce_prompt);
        self.memory.append_message(task.id, "assistant", &output);
        self.memory.update_task_status(task.id, TaskStatus::Done);

        MapReduceReport {
            task_id: task.id,
            mapped: mapped.len(),
            output,
        }
    }

    fn run_queued_task(&mut self, queued: QueuedTask, llm: &mut dyn LlmClient) -> AgentOutcome {
        let task = self.memory.get_task(queued.id).unwrap_or_else(|| Task {
            id: queued.id,
            title: queued.input.clone(),
            status: queued.status,
            created_at: 0,
            updated_at: 0,
        });

        let route = self.gateway.route(&queued.input, &self.workers);
        let memory = self.memory.progressive_memory(
            &queued.input,
            MEMORY_INDEX_LIMIT,
            MEMORY_DETAIL_LIMIT,
            MEMORY_DETAIL_BYTES,
        );
        let selected = self
            .planner
            .select_pattern_with_memory(&queued.input, &memory, llm);
        let pattern = match route {
            GatewayTarget::Pipeline => AgentPattern::Pipeline,
            GatewayTarget::MapReduce => AgentPattern::MapReduce,
            GatewayTarget::Coordinator => AgentPattern::HubAndSpoke,
            GatewayTarget::Worker(worker_id) => {
                self.mark_worker_busy(worker_id, true);
                selected
            }
        };

        let outcome = self.execute_task(task, &queued.input, pattern, llm);
        if let GatewayTarget::Worker(worker_id) = route {
            self.mark_worker_finished(worker_id);
        }
        outcome
    }

    fn execute_task(
        &mut self,
        task: Task,
        input: &str,
        pattern: AgentPattern,
        llm: &mut dyn LlmClient,
    ) -> AgentOutcome {
        self.memory.update_task_status(task.id, TaskStatus::Running);

        let memory = self.memory.progressive_memory(
            input,
            MEMORY_INDEX_LIMIT,
            MEMORY_DETAIL_LIMIT,
            MEMORY_DETAIL_BYTES,
        );
        let mut plan = self
            .planner
            .plan_with_context(&task, input, pattern, &memory, llm);
        self.execute_plan(task.id, &mut plan, pattern)
    }

    fn execute_plan(
        &mut self,
        task_id: TaskId,
        plan: &mut Plan,
        pattern: AgentPattern,
    ) -> AgentOutcome {
        let mut final_output = String::new();
        let mut final_status = TaskStatus::Done;

        for step in &mut plan.steps {
            let result = self.tools.run_step(&step.kind);
            self.memory.append_tool_result(task_id, &result);
            final_output = result.output.clone();
            if result.ok {
                step.status = StepStatus::Done;
            } else {
                step.status = failed_status(&result);
                final_status = if step.status == StepStatus::Denied {
                    TaskStatus::Waiting
                } else {
                    TaskStatus::Failed
                };
                break;
            }
        }

        self.memory
            .append_message(task_id, "assistant", &final_output);
        self.memory.update_task_status(task_id, final_status);

        AgentOutcome {
            task_id,
            status: final_status,
            output: final_output,
            pattern,
        }
    }

    fn mark_worker_busy(&mut self, worker_id: WorkerId, busy: bool) {
        if let Some(worker) = self
            .workers
            .iter_mut()
            .find(|worker| worker.id == worker_id)
        {
            worker.busy = busy;
        }
    }

    fn mark_worker_finished(&mut self, worker_id: WorkerId) {
        if let Some(worker) = self
            .workers
            .iter_mut()
            .find(|worker| worker.id == worker_id)
        {
            worker.busy = false;
            worker.completed += 1;
        }
    }
}

fn failed_status(result: &ToolResult) -> StepStatus {
    if result.output.contains("denied") {
        StepStatus::Denied
    } else {
        StepStatus::Failed
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::llm::{LlmClient, OfflineLlm};
    use crate::memory::InMemoryStore;
    use crate::tools::ToolPolicy;

    fn test_agent() -> AgentOrchestrator {
        AgentOrchestrator::new(
            Box::new(InMemoryStore::new(8)),
            ToolRunner::new(ToolPolicy {
                workspace: PathBuf::from("."),
                max_file_bytes: 1024,
                max_output_bytes: 4096,
                timeout: Duration::from_secs(1),
                allow_exec: false,
                allowed_exec: BTreeSet::new(),
            }),
        )
    }

    struct ScriptedLlm {
        responses: std::collections::VecDeque<String>,
    }

    impl ScriptedLlm {
        fn new(responses: &[&str]) -> Self {
            Self {
                responses: responses.iter().map(|value| value.to_string()).collect(),
            }
        }
    }

    impl LlmClient for ScriptedLlm {
        fn next_step(&mut self, _prompt: &str) -> String {
            self.responses
                .pop_front()
                .unwrap_or_else(|| "direct".into())
        }
    }

    #[test]
    fn tick_runs_queued_task_and_updates_heartbeat() {
        let mut agent = test_agent();
        let mut llm = OfflineLlm;
        let task_id = agent.enqueue_task("/ls src");

        let outcome = agent.tick(&mut llm).unwrap();

        assert_eq!(outcome.task_id, task_id);
        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(agent.heartbeat().tick, 1);
        assert_eq!(agent.heartbeat().last_task, Some(task_id));
    }

    #[test]
    fn dependent_task_waits_until_parent_done() {
        let mut agent = test_agent();
        let mut llm = OfflineLlm;
        let parent = agent.enqueue_task("/ls src");
        let child = agent.enqueue_task_with_dependencies(
            "/ls .",
            vec![parent],
            AgentPattern::CoordinatorWorker,
        );

        assert_eq!(agent.tick(&mut llm).unwrap().task_id, parent);
        assert_eq!(agent.tick(&mut llm).unwrap().task_id, child);
    }

    #[test]
    fn run_task_uses_llm_pattern_selection() {
        let mut agent = test_agent();
        let mut llm = ScriptedLlm::new(&["map-reduce", "selected pattern answer"]);

        let outcome = agent.run_task("compare alpha beta gamma", &mut llm);

        assert_eq!(outcome.pattern, AgentPattern::MapReduce);
        assert_eq!(outcome.output, "selected pattern answer");
    }

    #[test]
    fn tick_uses_llm_pattern_selection_for_queued_task() {
        let mut agent = test_agent();
        let mut llm = ScriptedLlm::new(&["pipeline", "pipeline answer"]);
        let task_id = agent.enqueue_task("turn draft into final answer");

        let outcome = agent.tick(&mut llm).unwrap();

        assert_eq!(outcome.task_id, task_id);
        assert_eq!(outcome.pattern, AgentPattern::Pipeline);
        assert_eq!(outcome.output, "pipeline answer");
    }
}
