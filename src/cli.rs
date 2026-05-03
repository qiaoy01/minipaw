use std::env;
use std::io::{self, BufRead, Write};

use crate::channels::exec_agent::ExecAgent;
use crate::channels::telegram::{
    get_updates, send_message, TelegramAdmission, TelegramChannel, TelegramConfig,
};
use crate::channels::telegram_agent::TelegramAgent;
use crate::config::{
    pair_telegram_chat, read_file_config, unpair_telegram_chat, write_telegram_config, AgentConfig,
};
use crate::llm::{LlamaCppClient, LlmClient, OfflineLlm};
use crate::memory::{InMemoryStore, MemoryStore};
use crate::minicore::{IncomingMessage, MiniCore, SessionReport};
use crate::planner::help_text;
use crate::skills::SkillRegistry;
use crate::tools::{ToolPolicy, ToolRunner};

pub fn run_from_env() -> io::Result<i32> {
    let workspace = env::current_dir()?;
    let config = AgentConfig::from_env(workspace);
    let args: Vec<String> = env::args().skip(1).collect();
    let mut app = App::new(config);
    app.run(&args)
}

struct App {
    core: MiniCore,
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

        let tools = ToolRunner::new(policy.clone());
        let memory = open_memory(&config);
        let llm = open_llm(&config);

        let mut core = MiniCore::new(memory, llm, tools, skills);

        // Register built-in agents.
        core.register(Box::new(ExecAgent::new(policy)));
        if let Some(ref token) = config.telegram_token {
            core.register(Box::new(TelegramAgent::new(token.clone())));
        }

        println!(
            "minicore ready. agents: {}",
            core.agents().collect::<Vec<_>>().join(", ")
        );

        Self {
            core,
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
        writeln!(stdout, "minicore ready. Type /help for commands, /quit to exit.")?;
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
            } else if let Some(rest) = input.strip_prefix("/ls") {
                let cmd = format!("ls{}", rest);
                let out = self.core.run_exec(&cmd);
                writeln!(stdout, "{out}")?;
            } else if let Some(rest) = input.strip_prefix("/read ") {
                let out = self.core.run_exec(&format!("read {rest}"));
                writeln!(stdout, "{out}")?;
            } else if let Some(rest) = input.strip_prefix("/exec ") {
                let out = self.core.run_exec(rest);
                writeln!(stdout, "{out}")?;
            } else {
                let msg = IncomingMessage {
                    text: input.to_owned(),
                    context_id: None,
                    source: "cli".to_owned(),
                };
                let report = self.core.process(msg);
                writeln!(
                    stdout,
                    "{} [{}] steps={}\n{}",
                    report.task_id, report.class, report.steps, report.output
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
                let msg = IncomingMessage {
                    text: input,
                    context_id: None,
                    source: "cli".to_owned(),
                };
                let report = self.core.process(msg);
                println!(
                    "{} [{}] steps={}\n{}",
                    report.task_id, report.class, report.steps, report.output
                );
                Ok(0)
            }
            Some("list") => {
                for task in self.core.memory().list_tasks() {
                    println!("{} [{}] {}", task.id, task.status, task.title);
                }
                Ok(0)
            }
            Some("show") => {
                let Some(id) = args.get(1).and_then(|v| parse_task_id(v)) else {
                    eprintln!("task show requires a task id like t1");
                    return Ok(2);
                };
                match self.core.memory().get_task(id) {
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
                if let Some(value) = self.core.memory().get_fact(key) {
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
                self.core.memory_mut().set_fact(key, &value);
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
                let Some(chat_id) =
                    args.get(1).and_then(|v| v.parse::<i64>().ok())
                else {
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
                let Some(chat_id) =
                    args.get(1).and_then(|v| v.parse::<i64>().ok())
                else {
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
                                let msg = IncomingMessage {
                                    text,
                                    context_id: Some(chat_id.to_string()),
                                    source: "telegram".to_owned(),
                                };
                                let report = self.core.process(msg);
                                let reply = format_session_report(&report);
                                println!("telegram reply [{}] steps={}", report.class, report.steps);
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

fn format_session_report(report: &SessionReport) -> String {
    let label = match report.class {
        crate::types::MessageClass::MiniHow => {
            let status_word = if report.output.starts_with("error:") {
                "Failed"
            } else {
                "Done"
            };
            format!("{status_word}")
        }
        crate::types::MessageClass::MiniWhy => "Analysis".to_owned(),
        crate::types::MessageClass::MiniWhat => "Answer".to_owned(),
    };

    let body = report.output.trim();
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

fn pairing_chat_id(text: &str) -> Option<i64> {
    text.lines()
        .find_map(|line| line.strip_prefix("chat_id="))
        .and_then(|raw| raw.parse::<i64>().ok())
}

fn parse_task_id(value: &str) -> Option<crate::types::TaskId> {
    value
        .strip_prefix('t')
        .unwrap_or(value)
        .parse::<u64>()
        .ok()
        .map(crate::types::TaskId)
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
