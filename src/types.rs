use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub u64);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerId(pub u8);

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "w{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    New,
    Running,
    Waiting,
    Done,
    Failed,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::New => f.write_str("new"),
            Self::Running => f.write_str("running"),
            Self::Waiting => f.write_str("waiting"),
            Self::Done => f.write_str("done"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessageClass {
    MiniHow,
    MiniWhy,
    MiniWhat,
}

impl fmt::Display for MessageClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MiniHow => f.write_str("minihow"),
            Self::MiniWhy => f.write_str("miniwhy"),
            Self::MiniWhat => f.write_str("miniwhat"),
        }
    }
}

impl MessageClass {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minihow" | "how" => Some(Self::MiniHow),
            "miniwhy" | "why" => Some(Self::MiniWhy),
            "miniwhat" | "what" => Some(Self::MiniWhat),
            _ => None,
        }
    }
}

/// Advisor lifecycle stage. Determines whether the local primary LLM, the
/// remote advisor LLM, or both are exercised on each request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisorMode {
    /// Send every request to both models; advisor's answer is authoritative.
    Training,
    /// Route per message class; shadow the unselected model for divergence data.
    Trial,
    /// Route per message class; do not shadow.
    Work,
}

impl fmt::Display for AdvisorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Training => f.write_str("training"),
            Self::Trial => f.write_str("trial"),
            Self::Work => f.write_str("work"),
        }
    }
}

impl AdvisorMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "training" | "train" => Some(Self::Training),
            "trial" => Some(Self::Trial),
            "work" | "working" => Some(Self::Work),
            _ => None,
        }
    }
}

/// Which configured LLM should answer a given request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentChoice {
    Primary,
    Advisor,
}

impl fmt::Display for AgentChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Primary => f.write_str("primary"),
            Self::Advisor => f.write_str("advisor"),
        }
    }
}

impl AgentChoice {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "primary" | "small" | "local" => Some(Self::Primary),
            "advisor" | "large" | "remote" => Some(Self::Advisor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPattern {
    Direct,
    CoordinatorWorker,
    HubAndSpoke,
    Pipeline,
    MapReduce,
}

impl fmt::Display for AgentPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => f.write_str("direct"),
            Self::CoordinatorWorker => f.write_str("coordinator-worker"),
            Self::HubAndSpoke => f.write_str("hub-and-spoke"),
            Self::Pipeline => f.write_str("pipeline"),
            Self::MapReduce => f.write_str("map-reduce"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStepKind {
    Answer(String),
    ReadFile(String),
    ListDir(String),
    Exec { program: String, args: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Done,
    Denied,
    Failed,
}

#[derive(Debug, Clone)]
pub struct PlanStep {
    pub index: usize,
    pub kind: PlanStepKind,
    pub status: StepStatus,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub task_id: TaskId,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    pub input: String,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub name: String,
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct Heartbeat {
    pub tick: u64,
    pub last_task: Option<TaskId>,
    pub last_status: TaskStatus,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            tick: 0,
            last_task: None,
            last_status: TaskStatus::New,
        }
    }
}

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
