# minipaw

`minipaw` is a tiny Rust command line agent for constrained Debian systems. The
default build uses only the Rust standard library. Native C, C++, and assembly
libraries are allowed where they provide a real footprint or performance benefit.
Runtime components in slower languages are out of scope.

## Goals

- Small memory footprint.
- Low CPU overhead.
- Predictable local behavior.
- Release builds optimized for binary size.
- No Python, JavaScript, JVM, Ruby, or other slower-language runtime dependency.
- A simple core that can later host a small local model, rules engine, or remote
  inference adapter behind explicit feature flags.

## Build

```sh
cargo build --release
```

The release profile enables `lto`, `opt-level = "z"`, `panic = "abort"`, and
symbol stripping to reduce the final binary size.

## Install

```sh
./install.sh
```

The installer uses `$HOME/.minipaw` by default. It builds the release binary,
copies it to `$HOME/.minipaw/minipaw`, seeds `SOUL.md`, `skills/`,
`minipaw.json`, `memory/minipaw.sqlite3`, and `workspace/`, adds the install
directory to `PATH` in `~/.bashrc`, sources that file for the installer process,
and then runs `minipaw onboarding` to configure the model and channel.

Override the install location with:

```sh
MINIPAW_HOME=/path/to/minipaw ./install.sh
```

Uninstall while keeping user data:

```sh
minipaw uninstall --keep-data
```

Remove minipaw-managed data as well:

```sh
minipaw uninstall --remove-user-data
```

Completely remove the install folder:

```sh
minipaw uninstall --purge
```

## Run

```sh
cargo run --release
```

Commands:

```text
minipaw run                         start interactive agent
minipaw telegram run                poll Telegram and answer paired chats
minipaw task new <text>             create and run a task
minipaw task list                   list tasks for this process
minipaw task show <id>              inspect a task
minipaw memory get <key>            read a fact
minipaw memory set <key> <value>    write a fact
minipaw gateway run                 run foreground gateway
minipaw gateway simulate            wait for simulated channel/agent messages
minipaw onboarding                  configure model and channel
minipaw uninstall                   remove minipaw install
minipaw config check                validate config
minipaw config telegram set --token <token> --chats <ids>
minipaw config telegram pair <chat-id>
minipaw config telegram unpair <chat-id>
minipaw config telegram show
```

Interactive task commands:

```text
/help                              show help
/ls [path]                         list a directory
/read <path>                       read a capped file
/exec <program> [args...]          run an allowlisted command
/enqueue <task>                    queue a task for the agent loop
/tick                              run one heartbeat loop tick
/heartbeat                         show loop state
/pipeline input | step | step      run a supervised pipeline
/mapreduce goal | item | item      run map-reduce supervision
/quit                              exit
```

## Footprint Notes

The current implementation avoids async runtimes, JSON libraries, HTTP clients,
and allocator-heavy data structures. History is bounded with `VecDeque`, file
reads are capped, and all command handling is synchronous.

## Current Implementation

Implemented modules:

- `agent`: `AgentOrchestrator` runs task creation, planning, tool execution, and
  memory updates.
- `orchestration`: coordinator-worker routing, hub-and-spoke gateway, heartbeat
  loop state, task queue dependency resolution, pipeline supervision, and
  map-reduce reports.
- `planner`: maps simple local intents to typed plan steps and falls back to an
  offline LLM client.
- `llm`: small `LlmClient` trait with an offline provider for no-network builds.
- `tools`: capped `fs.read`, capped `fs.list`, and guarded `exec.run`.
- `memory`: bounded in-memory store plus feature-gated native SQLite backend,
  with progressive index/detail loading for LLM context.
- `channels`: Telegram admission boundary that validates allowlisted chat IDs.
- `cli`: dependency-free management CLI with manual argument parsing.

OpenClaw comparison:

- OpenClaw has a full gateway server with WebSocket sessions, channel plugins,
  session subscriptions, session-message broadcasts, delivery contexts, and
  inter-session agent-message provenance.
- minipaw keeps the same rough boundary in a smaller form: Telegram admission
  validates paired chats, the orchestration gateway routes local work, and
  `gateway simulate` reads channel/agent events from stdin and emits
  `session.message`-style log events.
- This repo does not currently include a `sea/hermes-agent` checkout to compare
  directly.

Simulated gateway:

```sh
minipaw gateway run
minipaw gateway simulate
```

`gateway run` reads `minipaw.json` from the default workspace
(`MINIPAW_WORKSPACE`, then `MINIPAW_HOME`, then `$HOME/.minipaw`) and runs in
the foreground. It polls Telegram when `telegram.bot_token` is configured and
also reads stdin gateway events unless `--no-stdin` is set. It does not install
or manage a background service.

Input format:

```text
telegram <chat-id> <message>
agent <session-key> <message>
/quit
```

Example:

```sh
printf 'telegram 123456 hello\nagent agent:main:telegram:chat:123456 follow up\n' | minipaw gateway simulate
```

Environment configuration:

```text
MINIPAW_HOME=$HOME/.minipaw
MINIPAW_WORKSPACE=$MINIPAW_HOME
MINIPAW_ALLOW_EXEC=1
MINIPAW_EXEC_ALLOWLIST=git,ls,uname
MINIPAW_TELEGRAM_TOKEN=<bot-token>
MINIPAW_TELEGRAM_CHATS=123456,789012
MINIPAW_SQLITE_PATH=$MINIPAW_HOME/memory/minipaw.sqlite3
```

Telegram bot configuration:

```sh
minipaw config telegram set --token '123456:bot-token' --chats '111111,222222'
minipaw config telegram pair 333333
minipaw config telegram unpair 333333
minipaw config telegram show
```

This writes `telegram.bot_token` and `telegram.allowed_chats` into
`minipaw.json` while preserving the existing LLM model configuration. Tokens are
masked when displayed.

When a Telegram chat is not paired, the Telegram channel must only reply with
the chat ID and this command. It must not add the chat automatically:

```text
minipaw config telegram pair <chat-id>
```

Only someone with shell access to the minipaw machine can complete pairing.

`exec.run` remains disabled unless both `MINIPAW_ALLOW_EXEC=1` and the command
name appears in `MINIPAW_EXEC_ALLOWLIST`.

Feature flags:

```toml
[features]
default = []
sqlite = []
telegram = []
llm-http = []
tools-exec = []
compact-memory = []
```

The `sqlite` feature uses direct `libsqlite3` FFI and needs Debian's SQLite
development package at build time:

```sh
sudo apt install -y libsqlite3-dev
cargo test --features sqlite
```

## Orchestration Patterns

`minipaw` implements the orchestration patterns as small synchronous primitives
instead of a resident async framework.

Progressive memory disclosure:

- The orchestrator never dumps full memory into the LLM prompt.
- It first asks memory for a compact scored index of relevant tasks, messages,
  tool results, and facts.
- It then loads details only for the highest-scoring index entries.
- Detail text is capped by byte budget before it reaches the model.
- Pattern selection, planning, pipeline stages, map steps, and reduce steps all
  receive the same compact memory bundle.
- This keeps context windows small while preserving continuity across long task
  chains.

Pattern selection:

- The planner first applies tiny local overrides for explicit commands such as
  `/ls`, `/read`, `/exec`, `/pipeline`, and `/mapreduce`.
- For normal natural-language tasks, the planner asks the configured LLM to
  choose exactly one pattern.
- The selected pattern is parsed into a typed `AgentPattern` enum.
- If the model returns noisy or malformed text, the planner falls back to small
  local heuristics.
- The orchestrator validates routing through the gateway before execution.

Coordinator lead + worker sub-agents:

- The coordinator owns memory, planning, tool policy, and final task status.
- Worker agents are lightweight slots with `WorkerId`, name, busy flag, and
  completion count.
- The gateway assigns normal queued tasks to the first idle worker.
- The worker prompt is constrained to one assigned task and a concise result.

Hub-and-spoke with gateway:

- `Gateway` receives task text and routes by prefix.
- `/pipeline` routes to pipeline supervision.
- `/map` and `/reduce` route to map-reduce supervision.
- `/exec` stays with the coordinator because local command execution is risky.
- Other tasks route to idle workers, then fall back to the coordinator.

Heartbeat agent loop:

Each `tick` does one bounded unit of work:

```text
heartbeat -> dequeue ready task -> gateway route -> plan -> tool execute -> memory update
```

The loop records:

- Tick count.
- Last task.
- Last status.
- Queue depth through the CLI.

Task queue + dependency resolution:

- Tasks can be enqueued with dependencies.
- A task is ready only when every dependency is `done`.
- If a dependency fails, dependent work will not run.
- The queue executes one ready task per tick to keep CPU use predictable.

Pipeline supervisor:

- Pipeline stages run in order.
- Each stage receives the previous stage output plus its stage instruction.
- Stage outputs are written to memory.
- The final stage output becomes the task answer.

Map-reduce supervisor:

- Map phase calls the LLM once per bounded input item.
- Each mapped output is stored in memory.
- Reduce phase combines mapped outputs into one concise answer.
- This is intended for small item lists on embedded systems, not unbounded data
  processing.

## Development Plan

The project should grow as a small, modular Rust agent. Native C, C++, and
assembly libraries are acceptable for embedded storage, TLS, inference kernels,
and platform integration when they reduce footprint or CPU cost. The default
build should stay minimal, with larger integrations enabled by Cargo features
only when they are needed.

### Design Principles

- Keep the core agent Rust-first and dependency-light.
- Allow C, C++, and assembly libraries for performance-critical or system-level
  components.
- Do not add runtime services written in Python, JavaScript, JVM languages,
  Ruby, or similar slower environments.
- Prefer synchronous code unless a feature clearly needs async IO.
- Put every external integration behind a narrow trait.
- Keep memory bounded by configuration, not by best effort.
- Make all local side effects explicit, auditable, and deny-by-default.
- Support small-device builds with feature flags such as `telegram`, `sqlite`,
  `llm-http`, and `tools-exec`.

### Target Architecture

```text
Telegram / CLI
      |
      v
Agent Orchestrator
      |
      +-- Planner <-> LLM Client
      |
      +-- Tool Runner -> local tools, exec, filesystem
      |
      +-- Memory Store -> SQLite or compact fallback
```

Core modules:

- `agent`: request lifecycle, task state, safety policy, and response shaping.
- `planner`: turns user intent into small steps, discusses uncertain steps with
  the LLM, and asks the orchestrator to run approved actions.
- `llm`: small trait for chat/completion providers; concrete adapters stay
  feature-gated.
- `tools`: local tool registry with strict input schemas and execution limits.
- `memory`: conversation history, task records, tool logs, summaries, and
  key-value facts.
- `channels`: user-facing transports such as CLI and Telegram.
- `config`: compact config loading from env vars and a small TOML file.

### Phase 1: Core Agent Runtime

- Replace the current demo response path with an `AgentOrchestrator`.
- Add `Task`, `Plan`, `PlanStep`, `ToolCall`, and `ToolResult` structs.
- Keep all queues in memory first, with fixed-size limits.
- Add a policy layer that decides whether a step is allowed, denied, or requires
  operator confirmation.
- Add structured logs using a small internal format before adding external
  logging crates.

Exit criteria:

- The CLI can submit a task.
- The orchestrator can create a plan.
- The planner can execute simple local read-only steps.
- Unit tests cover plan state transitions and policy decisions.

### Phase 2: Management CLI

Add a management CLI for operating the agent locally:

```text
minipaw run                  start interactive agent
minipaw task new "..."       create a task
minipaw task list            list active and recent tasks
minipaw task show <id>       inspect a task, plan, and tool results
minipaw memory get <key>     read memory
minipaw memory set <key>     write memory
minipaw config check         validate config
```

Implementation notes:

- Start with manual argument parsing to avoid a CLI dependency.
- Add a feature-gated CLI crate only if manual parsing becomes hard to maintain.
- Keep command output plain text by default, with optional line-delimited JSON
  later for automation.

Exit criteria:

- Management jobs can run without the Telegram channel.
- Local task inspection does not require network access.
- CLI commands work in release builds on the target device.

### Phase 3: Tool Runner

Add a tool runner that can execute local tools safely.

Initial tools:

- `fs.read`: read a capped-size text file.
- `fs.list`: list a directory.
- `exec.run`: run an allowlisted command on the local machine.

Safety requirements:

- `exec.run` is disabled by default.
- Commands require allowlist entries in config.
- Each command has timeout, max output bytes, environment, and working directory
  limits.
- Tool results are stored in memory with command, exit code, duration, stdout
  preview, and stderr preview.
- Destructive commands require explicit operator confirmation.

Exit criteria:

- The planner can request a tool call.
- The orchestrator validates and runs approved tools.
- Tool output is capped and cannot grow memory without bound.

### Phase 4: SQLite Memory

Add embedded memory with a small schema:

```text
tasks(id, status, created_at, updated_at, title)
plans(id, task_id, status, created_at)
plan_steps(id, plan_id, idx, status, kind, input, output_ref)
messages(id, task_id, role, body, created_at)
tool_runs(id, task_id, tool, input, exit_code, output, created_at)
facts(key, value, updated_at)
```

Implementation approach:

SQLite is acceptable because it is a compact native C library with strong
embedded-device behavior. Keep it feature-gated so devices that do not need
durable memory can avoid the dependency. The planned approach is:

- Default build: in-memory store only.
- `sqlite` feature: SQLite-backed durable memory.
- `compact-memory` feature: optional lightweight fallback if SQLite is too large
  for a target device.

Exit criteria:

- Agent state survives process restarts with the `sqlite` feature enabled.
- Database writes are small, explicit transactions.
- Schema migrations are simple Rust functions, not a large migration framework.

### Phase 5: LLM Planner

Add an LLM client trait:

```text
trait LlmClient {
    fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmError>;
}
```

Planner responsibilities:

- Summarize current task state.
- Ask the LLM for the next small step.
- Convert model output into a typed plan step.
- Reject malformed or unsafe tool requests.
- Stop when the task is complete, blocked, or waiting for confirmation.

Implementation notes:

- Keep provider-specific HTTP clients behind feature flags.
- Prefer compact prompts and bounded context windows.
- Store summaries in memory instead of replaying full history.
- Add a no-network mock LLM for tests and offline development.

Exit criteria:

- Planner can complete a multi-step task through mock LLM responses.
- Bad LLM output cannot directly execute a local tool.
- Context size is capped by config.

### Phase 6: Telegram Channel

Add a Telegram channel as an optional feature.

Responsibilities:

- Poll or receive Telegram updates.
- Map each chat message to an agent task or task continuation.
- Send concise status updates and final answers.
- Restrict allowed chat IDs from config.
- Provide operator confirmation buttons or commands for risky tool calls.

Implementation notes:

- Start with long polling because it is simpler for small hardware.
- Keep webhook support optional for deployments that already have HTTPS.
- Store Telegram update IDs in memory to avoid duplicate processing.
- Avoid downloading large files by default.

Exit criteria:

- The agent can receive a Telegram message and create a task.
- Only configured chat IDs can control the agent.
- Telegram can approve or deny pending tool calls.

### Phase 7: Small Hardware Hardening

- Add release-size checks in CI or a local script.
- Add config limits for max task count, max history rows, max tool output, and
  max planning turns.
- Add graceful shutdown that flushes memory and active task state.
- Add a single-process mode with no background workers.
- Add optional metrics printed through the management CLI, not a resident server.

Exit criteria:

- Release binary size is tracked.
- Idle CPU usage is near zero.
- Memory usage remains bounded during repeated tasks.
- All optional integrations can be disabled at compile time.

### Suggested Feature Flags

```toml
[features]
default = []
telegram = []
sqlite = []
compact-memory = []
llm-http = []
tools-exec = []
```

The default binary should remain a tiny local CLI. Production builds can opt into
only the features needed for a specific device.
