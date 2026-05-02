use std::collections::{BTreeSet, VecDeque};

use crate::types::{AgentPattern, Heartbeat, TaskId, TaskStatus, WorkerId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayTarget {
    Coordinator,
    Worker(WorkerId),
    Pipeline,
    MapReduce,
}

#[derive(Debug, Clone)]
pub struct Gateway {
    routes: Vec<(String, GatewayTarget)>,
}

impl Default for Gateway {
    fn default() -> Self {
        Self {
            routes: vec![
                ("/pipeline".to_owned(), GatewayTarget::Pipeline),
                ("/map".to_owned(), GatewayTarget::MapReduce),
                ("/reduce".to_owned(), GatewayTarget::MapReduce),
                ("/exec".to_owned(), GatewayTarget::Coordinator),
            ],
        }
    }
}

impl Gateway {
    pub fn route(&self, input: &str, workers: &[WorkerAgent]) -> GatewayTarget {
        for (prefix, target) in &self.routes {
            if input.trim_start().starts_with(prefix) {
                return target.clone();
            }
        }

        workers
            .iter()
            .find(|worker| !worker.busy)
            .map(|worker| GatewayTarget::Worker(worker.id))
            .unwrap_or(GatewayTarget::Coordinator)
    }
}

#[derive(Debug, Clone)]
pub struct WorkerAgent {
    pub id: WorkerId,
    pub name: String,
    pub busy: bool,
    pub completed: u64,
}

impl WorkerAgent {
    pub fn new(id: WorkerId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            busy: false,
            completed: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueuedTask {
    pub id: TaskId,
    pub input: String,
    pub dependencies: Vec<TaskId>,
    pub status: TaskStatus,
    pub pattern: AgentPattern,
}

#[derive(Debug, Default)]
pub struct TaskQueue {
    pending: VecDeque<QueuedTask>,
    completed: BTreeSet<TaskId>,
    failed: BTreeSet<TaskId>,
}

impl TaskQueue {
    pub fn push(&mut self, task: QueuedTask) {
        self.pending.push_back(task);
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn mark_finished(&mut self, id: TaskId, status: TaskStatus) {
        match status {
            TaskStatus::Done => {
                self.completed.insert(id);
            }
            TaskStatus::Failed => {
                self.failed.insert(id);
            }
            _ => {}
        }
    }

    pub fn next_ready(&mut self) -> Option<QueuedTask> {
        let ready_index = self.pending.iter().position(|task| {
            task.dependencies
                .iter()
                .all(|dependency| self.completed.contains(dependency))
                && !task
                    .dependencies
                    .iter()
                    .any(|dependency| self.failed.contains(dependency))
        })?;
        self.pending.remove(ready_index)
    }
}

#[derive(Debug, Clone)]
pub struct PipelineStage {
    pub name: String,
    pub input: String,
}

#[derive(Debug, Clone)]
pub struct PipelineReport {
    pub task_id: TaskId,
    pub stages_run: usize,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct MapReduceReport {
    pub task_id: TaskId,
    pub mapped: usize,
    pub output: String,
}

#[derive(Debug, Default)]
pub struct AgentLoop {
    heartbeat: Heartbeat,
}

impl AgentLoop {
    pub fn tick_started(&mut self) {
        self.heartbeat.tick += 1;
    }

    pub fn tick_finished(&mut self, task_id: Option<TaskId>, status: TaskStatus) {
        self.heartbeat.last_task = task_id;
        self.heartbeat.last_status = status;
    }

    pub fn heartbeat(&self) -> &Heartbeat {
        &self.heartbeat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_resolution_waits_for_parent() {
        let mut queue = TaskQueue::default();
        queue.push(QueuedTask {
            id: TaskId(2),
            input: "child".into(),
            dependencies: vec![TaskId(1)],
            status: TaskStatus::New,
            pattern: AgentPattern::CoordinatorWorker,
        });
        assert!(queue.next_ready().is_none());
        queue.mark_finished(TaskId(1), TaskStatus::Done);
        assert_eq!(queue.next_ready().unwrap().id, TaskId(2));
    }
}
