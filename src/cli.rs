use std::env;
use std::io::{self, BufRead, Write};

use crate::agent::{AgentOrchestrator, AgentOutcome};
use crate::channels::telegram::{
    classify_message_kind, get_updates, normalize_action_text, send_message, MessageKind,
    TelegramAdmission, TelegramChannel, TelegramConfig,
};
use crate::config::{
    pair_telegram_chat, read_file_config, unpair_telegram_chat, write_telegram_config, AgentConfig,
};
use crate::llm::{LlamaCppClient, LlmClient, OfflineLlm};
use crate::memory::{InMemoryStore, MemoryStore};
use crate::orchestration::PipelineStage;
use crate::planner::help_text;
use crate::skills::SkillRegistry;
use crate::tools::{ToolPolicy, ToolRunner};
use crate::types::{TaskId, TaskStatus};

pub fn run_from_env() -> io::Result<i32> {
    let workspace = env::current_dir()?;
    let config = AgentConfig::from_env(workspace);
    let args: Vec<String> = env::args().skip(1).collect();
    let mut app = App::new(config);
    app.run(&args)
}

struct App {
    agent: AgentOrchestrator,
    llm: Box<dyn LlmClient>,
    workspace: std::path::PathBuf,
}

impl App {
    fn new(config: AgentConfig) -> Self {
        let skills = SkillRegistry::load(&config.workspace.join("skills"));
        if !skills.is_empty() {
            println!(
                "skills loaded: {}",
                skills
                    .skills()
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        // Skills are operator-curated; auto-trust their exec programs so they
        // don't require MINIPAW_ALLOW_EXEC in the environment.
        let mut allow_exec = config.allow_exec;
        let mut allowed_exec = config.allowed_exec.clone();
        if !skills.is_empty() {
            allow_exec = true;
            for prog in skills.exec_programs() {
                allowed_exec.insert(prog.to_owned());
            }
        }
        let policy = ToolPolicy {
            workspace: config.workspace.clone(),
            max_file_bytes: config.max_file_bytes,
            max_output_bytes: config.max_tool_output_bytes,
            timeout: config.tool_timeout,
            allow_exec,
            allowed_exec,
        };
        let tools = ToolRunner::new(policy);
        let memory = open_memory(&config);
        Self {
            agent: AgentOrchestrator::new(memory, tools, skills),
            llm: open_llm(&config),
            workspace: config.workspace.clone(),
        }
    }

    fn run(&mut self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            None | Some("run") => self.repl(),
            Some("task") => self.task(&args[1..]),
            Some("memory") => self.memory(&args[1..]),
            Some("config") => self.config(&args[1..]),
            Some("telegram") => self.telegram(&args[1..]),
            Some("help") | Some("--help") | Some("-h") => {
                println!("{}", help_text());
                Ok(0)
            }
            Some(other) => {
                eprintln!("unknown command: {other}");
                eprintln!("{}", help_text());
                Ok(2)
            }
        }
    }

    fn repl(&mut self) -> io::Result<i32> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        writeln!(
            stdout,
            "minipaw ready. Type /help for commands, /quit to exit."
        )?;
        write!(stdout, "> ")?;
        stdout.flush()?;

        for line in stdin.lock().lines() {
            let line = line?;
            let input = line.trim();
            if input == "/quit" || input == "/exit" {
                break;
            }
            if input == "/help" || input == "help" {
                writeln!(stdout, "{}", help_text())?;
            } else if let Some(task) = input.strip_prefix("/enqueue ") {
                let task_id = self.agent.enqueue_task(task);
                writeln!(stdout, "queued {task_id}")?;
            } else if input == "/tick" {
                match self.agent.tick(self.llm.as_mut()) {
                    Some(outcome) => writeln!(
                        stdout,
                        "{} [{} {}]\n{}",
                        outcome.task_id, outcome.status, outcome.pattern, outcome.output
                    )?,
                    None => writeln!(stdout, "idle")?,
                }
            } else if input == "/heartbeat" {
                let heartbeat = self.agent.heartbeat();
                writeln!(
                    stdout,
                    "tick={} last_task={} last_status={} queue={}",
                    heartbeat.tick,
                    heartbeat
                        .last_task
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_owned()),
                    heartbeat.last_status,
                    self.agent.queue_len()
                )?;
            } else if let Some(rest) = input.strip_prefix("/pipeline ") {
                let (initial, stages) = parse_pipeline(rest);
                let report = self
                    .agent
                    .run_pipeline(&initial, &stages, self.llm.as_mut());
                writeln!(
                    stdout,
                    "{} [pipeline stages={}]\n{}",
                    report.task_id, report.stages_run, report.output
                )?;
            } else if let Some(rest) = input.strip_prefix("/mapreduce ") {
                let (goal, items) = parse_map_reduce(rest);
                let report = self.agent.run_map_reduce(&goal, &items, self.llm.as_mut());
                writeln!(
                    stdout,
                    "{} [map-reduce mapped={}]\n{}",
                    report.task_id, report.mapped, report.output
                )?;
            } else {
                let outcome = self.agent.run_task(input, self.llm.as_mut());
                writeln!(
                    stdout,
                    "{} [{} {}]\n{}",
                    outcome.task_id, outcome.status, outcome.pattern, outcome.output
                )?;
            }
            write!(stdout, "> ")?;
            stdout.flush()?;
        }
        Ok(0)
    }

    fn task(&mut self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("new") => {
                let input = args[1..].join(" ");
                if input.trim().is_empty() {
                    eprintln!("task new requires text");
                    return Ok(2);
                }
                let outcome = self.agent.run_task(&input, self.llm.as_mut());
                println!(
                    "{} [{} {}]\n{}",
                    outcome.task_id, outcome.status, outcome.pattern, outcome.output
                );
                Ok(0)
            }
            Some("list") => {
                for task in self.agent.memory().list_tasks() {
                    println!("{} [{}] {}", task.id, task.status, task.title);
                }
                Ok(0)
            }
            Some("show") => {
                let Some(id) = args.get(1).and_then(|value| parse_task_id(value)) else {
                    eprintln!("task show requires a task id like t1");
                    return Ok(2);
                };
                match self.agent.memory().get_task(id) {
                    Some(task) => println!(
                        "{} [{}] created={} updated={}\n{}",
                        task.id, task.status, task.created_at, task.updated_at, task.title
                    ),
                    None => println!("task not found: {id}"),
                }
                Ok(0)
            }
            _ => {
                eprintln!("usage: minipaw task new <text> | task list | task show <id>");
                Ok(2)
            }
        }
    }

    fn memory(&mut self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("get") => {
                let Some(key) = args.get(1) else {
                    eprintln!("memory get requires a key");
                    return Ok(2);
                };
                if let Some(value) = self.agent.memory().get_fact(key) {
                    println!("{value}");
                }
                Ok(0)
            }
            Some("set") => {
                let Some(key) = args.get(1) else {
                    eprintln!("memory set requires a key and value");
                    return Ok(2);
                };
                let value = args[2..].join(" ");
                self.agent.memory_mut().set_fact(key, &value);
                println!("ok");
                Ok(0)
            }
            _ => {
                eprintln!("usage: minipaw memory get <key> | memory set <key> <value>");
                Ok(2)
            }
        }
    }

    fn config(&self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("check") => {
                println!("config ok");
                Ok(0)
            }
            Some("telegram") => self.config_telegram(&args[1..]),
            _ => {
                eprintln!(
                    "usage: minipaw config check | config telegram set --token <token> --chats <ids> | config telegram pair <chat-id> | config telegram unpair <chat-id> | config telegram show"
                );
                Ok(2)
            }
        }
    }

    fn config_telegram(&self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("set") => {
                let mut token = None;
                let mut chats = None;
                let mut index = 1;
                while index < args.len() {
                    match args[index].as_str() {
                        "--token" => {
                            token = args.get(index + 1).cloned();
                            index += 2;
                        }
                        "--chats" => {
                            chats = args.get(index + 1).cloned();
                            index += 2;
                        }
                        unknown => {
                            eprintln!("unknown telegram config option: {unknown}");
                            return Ok(2);
                        }
                    }
                }

                let Some(token) = token else {
                    eprintln!("telegram set requires --token <bot-token>");
                    return Ok(2);
                };
                let Some(chats) = chats else {
                    eprintln!("telegram set requires --chats <chat-id[,chat-id...]>");
                    return Ok(2);
                };
                let parsed_chats = parse_chat_ids(&chats);
                if parsed_chats.is_empty() {
                    eprintln!("--chats must contain at least one numeric chat id");
                    return Ok(2);
                }

                write_telegram_config(&self.workspace, &token, &parsed_chats)?;
                println!(
                    "telegram configured: token={} chats={}",
                    mask_token(&token),
                    join_chat_ids(&parsed_chats)
                );
                Ok(0)
            }
            Some("show") => {
                let config = read_file_config(&self.workspace);
                if let Some(telegram) = config.telegram {
                    println!("token={}", mask_token(&telegram.token));
                    println!("chats={}", join_chat_ids(&telegram.allowed_chats));
                } else {
                    println!("telegram not configured");
                }
                Ok(0)
            }
            Some("pair") => {
                let Some(chat_id) = args.get(1).and_then(|value| value.parse::<i64>().ok()) else {
                    eprintln!("telegram pair requires a numeric chat id");
                    return Ok(2);
                };
                match pair_telegram_chat(&self.workspace, chat_id) {
                    Ok(true) => println!("telegram chat paired: {chat_id}"),
                    Ok(false) => println!("telegram chat already paired: {chat_id}"),
                    Err(err) => {
                        eprintln!("telegram pair failed: {err}");
                        return Ok(2);
                    }
                }
                Ok(0)
            }
            Some("unpair") => {
                let Some(chat_id) = args.get(1).and_then(|value| value.parse::<i64>().ok()) else {
                    eprintln!("telegram unpair requires a numeric chat id");
                    return Ok(2);
                };
                match unpair_telegram_chat(&self.workspace, chat_id) {
                    Ok(true) => println!("telegram chat unpaired: {chat_id}"),
                    Ok(false) => println!("telegram chat was not paired: {chat_id}"),
                    Err(err) => {
                        eprintln!("telegram unpair failed: {err}");
                        return Ok(2);
                    }
                }
                Ok(0)
            }
            _ => {
                eprintln!(
                    "usage: minipaw config telegram set --token <token> --chats <ids> | config telegram pair <chat-id> | config telegram unpair <chat-id> | config telegram show"
                );
                Ok(2)
            }
        }
    }

    fn telegram(&mut self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("run") => self.telegram_run(),
            _ => {
                eprintln!("usage: minipaw telegram run");
                Ok(2)
            }
        }
    }

    fn telegram_run(&mut self) -> io::Result<i32> {
        let config = AgentConfig::from_env(self.workspace.clone());
        let Some(token) = config.telegram_token else {
            eprintln!("telegram token is not configured");
            return Ok(2);
        };
        let channel = TelegramChannel::new(TelegramConfig {
            token: token.clone(),
            allowed_chats: config.telegram_allowed_chats.clone(),
        });

        println!(
            "telegram runner started; allowed_chats={}",
            join_chat_ids(&config.telegram_allowed_chats)
        );
        let mut offset = None;
        loop {
            match get_updates(&token, offset, 25) {
                Ok(messages) => {
                    if messages.is_empty() {
                        println!("telegram poll: no updates");
                        continue;
                    }
                    for message in messages {
                        offset = Some(message.update_id + 1);
                        let chat_id = message.chat_id;
                        println!(
                            "telegram update={} chat={} bytes={}",
                            message.update_id,
                            chat_id,
                            message.text.len()
                        );
                        match channel.admit_message(message) {
                            TelegramAdmission::Accepted(text) => {
                                let kind = classify_message_kind(&text);
                                // Normalize action text: strip polite prefixes so the
                                // planner receives a clean imperative ("list src" not
                                // "can you please list src").
                                let task_text = match kind {
                                    MessageKind::Action => {
                                        normalize_action_text(&text).to_owned()
                                    }
                                    MessageKind::Knowledge => text.clone(),
                                };
                                println!(
                                    "telegram kind={} task={:?}",
                                    match kind {
                                        MessageKind::Action => "action",
                                        MessageKind::Knowledge => "knowledge",
                                    },
                                    task_text
                                );
                                let outcome =
                                    self.agent.run_task(&task_text, self.llm.as_mut());
                                let reply = match kind {
                                    MessageKind::Action => telegram_action_reply(&outcome),
                                    MessageKind::Knowledge => {
                                        telegram_user_reply(&outcome.output)
                                    }
                                };
                                match send_message(&token, chat_id, &reply) {
                                    Ok(()) => println!("telegram reply sent"),
                                    Err(err) => eprintln!("telegram send failed: {err}"),
                                }
                            }
                            TelegramAdmission::PairingRequired(text) => {
                                let chat_id = pairing_chat_id(&text).unwrap_or(0);
                                if chat_id != 0 {
                                    match send_message(&token, chat_id, &text) {
                                        Ok(()) => {
                                            println!(
                                                "telegram pairing instructions sent: {chat_id}"
                                            )
                                        }
                                        Err(err) => eprintln!("telegram send failed: {err}"),
                                    }
                                }
                            }
                        }
                    }
                }
                Err(err) => eprintln!("telegram poll failed: {err}"),
            }
        }
    }
}

fn pairing_chat_id(text: &str) -> Option<i64> {
    text.lines()
        .find_map(|line| line.strip_prefix("chat_id="))
        .and_then(|raw| raw.parse::<i64>().ok())
}

fn telegram_action_reply(outcome: &AgentOutcome) -> String {
    let label = match outcome.status {
        TaskStatus::Done => "Done",
        TaskStatus::Failed => "Failed",
        TaskStatus::Waiting => "Awaiting approval",
        _ => "In progress",
    };
    let body = outcome.output.trim();
    let mut reply = if body.is_empty() {
        format!("{label}: (no output)")
    } else {
        format!("{label}:\n{body}")
    };
    const TELEGRAM_REPLY_LIMIT: usize = 3500;
    if reply.len() > TELEGRAM_REPLY_LIMIT {
        let mut end = TELEGRAM_REPLY_LIMIT;
        while !reply.is_char_boundary(end) {
            end -= 1;
        }
        reply.truncate(end);
        reply.push_str("\n[truncated]");
    }
    reply
}

fn telegram_user_reply(output: &str) -> String {
    let mut lines = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("context:")
            || lower.starts_with("memory index:")
            || lower.starts_with("selected memory details:")
            || lower.starts_with("task:")
            || lower.starts_with("system:")
            || lower.starts_with("user:")
            || lower.starts_with("assistant:")
            || lower.starts_with("you are an ai assistant")
            || lower.starts_with("your task")
            || lower.starts_with("you must")
        {
            continue;
        }
        lines.push(line);
    }

    let mut reply = lines.join("\n").trim().to_owned();
    if reply.is_empty() {
        reply = "I could not produce a clean reply for that message.".to_owned();
    }
    const TELEGRAM_REPLY_LIMIT: usize = 3500;
    if reply.len() > TELEGRAM_REPLY_LIMIT {
        let mut end = TELEGRAM_REPLY_LIMIT;
        while !reply.is_char_boundary(end) {
            end -= 1;
        }
        reply.truncate(end);
        reply.push_str("\n[truncated]");
    }
    reply
}

fn parse_pipeline(rest: &str) -> (String, Vec<PipelineStage>) {
    let mut parts = rest
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty());
    let initial = parts.next().unwrap_or_default().to_owned();
    let stages = parts
        .enumerate()
        .map(|(index, input)| PipelineStage {
            name: format!("stage-{}", index + 1),
            input: input.to_owned(),
        })
        .collect();
    (initial, stages)
}

fn parse_map_reduce(rest: &str) -> (String, Vec<String>) {
    let mut parts = rest
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty());
    let goal = parts.next().unwrap_or_default().to_owned();
    let items = parts.map(str::to_owned).collect();
    (goal, items)
}

fn parse_task_id(value: &str) -> Option<TaskId> {
    value
        .strip_prefix('t')
        .unwrap_or(value)
        .parse::<u64>()
        .ok()
        .map(TaskId)
}

fn parse_chat_ids(value: &str) -> std::collections::BTreeSet<i64> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

fn join_chat_ids(chats: &std::collections::BTreeSet<i64>) -> String {
    chats
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn mask_token(token: &str) -> String {
    let Some((prefix, suffix)) = token.split_once(':') else {
        return "***".to_owned();
    };
    let tail = suffix.chars().rev().take(4).collect::<Vec<_>>();
    let visible_tail = tail.into_iter().rev().collect::<String>();
    format!("{prefix}:***{visible_tail}")
}

fn open_memory(config: &AgentConfig) -> Box<dyn MemoryStore> {
    #[cfg(feature = "sqlite")]
    {
        let path = std::env::var("MINIPAW_SQLITE_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| config.workspace.join("minipaw.sqlite3"));
        match crate::memory::sqlite::SqliteStore::open(&path) {
            Ok(store) => return Box::new(store),
            Err(err) => eprintln!("sqlite unavailable, using in-memory store: {err}"),
        }
    }

    Box::new(InMemoryStore::new(config.history_limit))
}

fn open_llm(config: &AgentConfig) -> Box<dyn LlmClient> {
    if let Some(llm) = &config.primary_agent {
        match LlamaCppClient::from_config(llm) {
            Ok(client) => return Box::new(client),
            Err(err) => eprintln!("llm config unavailable, using offline provider: {err}"),
        }
    }

    Box::new(OfflineLlm)
}
