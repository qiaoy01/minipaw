use std::collections::BTreeSet;
use std::env;
use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::channels::exec_agent::ExecAgent;
use crate::channels::telegram::{
    get_updates, send_message, TelegramAdmission, TelegramChannel, TelegramConfig, TelegramMessage,
};
use crate::channels::telegram_agent::TelegramAgent;
use crate::adjustments::{
    apply_proposal, find_proposal, list_proposals, reject_proposal, ProposalEntry,
};
use crate::config::{
    clear_advisor, default_workspace, pair_telegram_chat, read_file_config, unpair_telegram_chat,
    write_advisor_agent, write_advisor_mode, write_advisor_route, write_primary_config,
    write_telegram_config, AgentConfig,
};
use crate::llm::{LlamaCppClient, LlmClient, OfflineLlm};
use crate::prompts::PromptStore;
use crate::types::{AdvisorMode, AgentChoice, MessageClass};
use crate::memory::{InMemoryStore, MemoryStore};
use crate::minicore::{IncomingMessage, MiniCore, SessionReport};
use crate::planner::help_text;
use crate::skills::SkillRegistry;
use crate::tools::{ToolPolicy, ToolRunner};

pub fn run_from_env() -> io::Result<i32> {
    let args: Vec<String> = env::args().skip(1).collect();
    let workspace = env::var("MINIPAW_WORKSPACE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| default_workspace());
    if matches!(args.first().map(String::as_str), Some("uninstall")) {
        return uninstall_from_env(&workspace, &args[1..]);
    }

    std::fs::create_dir_all(&workspace)?;
    let config = AgentConfig::from_env(workspace);
    let mut app = App::new(config);
    app.run(&args)
}

struct App {
    core: MiniCore,
    workspace: std::path::PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UninstallMode {
    KeepData,
    RemoveUserData,
    Purge,
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
        let mut skill_exec = BTreeSet::new();
        if !skills.is_empty() {
            allow_exec = true;
            for prog in skills.exec_programs() {
                skill_exec.insert(prog.to_owned());
            }
        }

        let policy = ToolPolicy {
            workspace: config.workspace.clone(),
            max_file_bytes: config.max_file_bytes,
            max_output_bytes: config.max_tool_output_bytes,
            timeout: config.tool_timeout,
            allow_exec,
            allowed_exec: config.allowed_exec.clone(),
            skill_exec,
        };

        let tools = ToolRunner::new(policy.clone());
        let memory = open_memory(&config);
        let llm = open_llm(&config);
        let prompts = PromptStore::install(&config.workspace).unwrap_or_else(|err| {
            eprintln!("prompt install failed, falling back to embedded defaults: {err}");
            // Even on failure return a store rooted at workspace; render() falls
            // back to compile-time defaults when the file is missing.
            PromptStore::install(&config.workspace).unwrap_or_else(|_| {
                PromptStore::install(std::path::Path::new("."))
                    .expect("fallback prompt install must succeed")
            })
        });

        let mut core = MiniCore::new(memory, llm, tools, skills, prompts);

        if let Some(advisor) = &config.advisor {
            match LlamaCppClient::from_config(&advisor.agent) {
                Ok(client) => {
                    println!(
                        "advisor configured: provider={} model={} mode={}",
                        advisor.agent.provider, advisor.agent.model, advisor.mode
                    );
                    core.set_advisor(
                        Box::new(client),
                        advisor.mode,
                        advisor.routing.clone(),
                    );
                }
                Err(err) => eprintln!("advisor disabled: {err}"),
            }
        }

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
            Some("gateway") => self.gateway(&args[1..]),
            Some("onboarding") | Some("onboard") => self.onboarding(),
            Some("uninstall") => uninstall_from_env(&self.workspace, &args[1..]),
            Some("telegram") => self.telegram(&args[1..]),
            Some("help") | Some("--help") | Some("-h") => {
                println!("{}", help_text());
                Ok(0)
            }
            Some("--version") | Some("-V") | Some("version") => {
                println!(
                    "minipaw {} ({})",
                    env!("CARGO_PKG_VERSION"),
                    env!("MINIPAW_GIT_HASH")
                );
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
            Some("set") => self.config_set(&args[1..]),
            Some("show") => self.config_show(),
            Some("telegram") => self.config_telegram(&args[1..]),
            Some("advisor") => self.config_advisor(&args[1..]),
            _ => {
                eprintln!(
                    "usage: minipaw config check | config show | config set [--provider p] [--url u] [--model m] [--api-key k] | config telegram ... | config advisor ..."
                );
                Ok(2)
            }
        }
    }

    fn config_show(&self) -> io::Result<i32> {
        let file_config = read_file_config(&self.workspace);
        if let Some(llm) = &file_config.primary_agent {
            println!("provider={}", llm.provider);
            println!("url={}", llm.url);
            println!("model={}", llm.model);
            if llm.api_key.as_ref().is_some_and(|k| !k.is_empty()) {
                println!("api_key=<set>");
            }
        } else {
            println!("model: not configured");
        }
        if let Some(advisor) = &file_config.advisor {
            println!("advisor.provider={}", advisor.agent.provider);
            println!("advisor.url={}", advisor.agent.url);
            println!("advisor.model={}", advisor.agent.model);
            if advisor.agent.api_key.as_ref().is_some_and(|k| !k.is_empty()) {
                println!("advisor.api_key=<set>");
            }
            println!("advisor.mode={}", advisor.mode);
            for class in [
                MessageClass::MiniHow,
                MessageClass::MiniWhy,
                MessageClass::MiniWhat,
            ] {
                let choice = advisor
                    .routing
                    .get(&class)
                    .copied()
                    .unwrap_or(AgentChoice::Primary);
                println!("advisor.routing.{class}={choice}");
            }
        }
        if let Some(telegram) = &file_config.telegram {
            println!("telegram.token={}", mask_token(&telegram.token));
            println!("telegram.chats={}", join_chat_ids(&telegram.allowed_chats));
        }
        Ok(0)
    }

    fn config_advisor(&self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("set") => self.config_advisor_set(&args[1..]),
            Some("show") => self.config_advisor_show(),
            Some("mode") => self.config_advisor_mode(&args[1..]),
            Some("route") => self.config_advisor_route(&args[1..]),
            Some("clear") => self.config_advisor_clear(),
            Some("proposals") => self.config_advisor_proposals(&args[1..]),
            _ => {
                eprintln!(
                    "usage: minipaw config advisor set --provider p --url u --model m [--api-key k]\n\
                     \x20      config advisor mode <training|trial|working>\n\
                     \x20      config advisor route <minihow|miniwhy|miniwhat> <primary|advisor>\n\
                     \x20      config advisor proposals list | show <id> | apply <id> | reject <id>\n\
                     \x20      config advisor show\n\
                     \x20      config advisor clear"
                );
                Ok(2)
            }
        }
    }

    fn config_advisor_proposals(&self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("list") | None => {
                let proposals = list_proposals(&self.workspace);
                if proposals.is_empty() {
                    println!("no advisor proposals pending");
                    return Ok(0);
                }
                for p in proposals {
                    println!(
                        "{} kind={} {}",
                        p.id,
                        p.kind,
                        proposal_summary(&p)
                    );
                }
                Ok(0)
            }
            Some("show") => {
                let Some(id) = args.get(1) else {
                    eprintln!("usage: minipaw config advisor proposals show <id>");
                    return Ok(2);
                };
                let Some(proposal) = find_proposal(&self.workspace, id) else {
                    eprintln!("proposal not found: {id}");
                    return Ok(2);
                };
                println!("id: {}", proposal.id);
                println!("kind: {}", proposal.kind);
                if let Some(class) = proposal.class {
                    println!("class: {class}");
                }
                if let Some(task) = proposal.task_id {
                    println!("task: {task}");
                }
                println!("created_at: {}", proposal.created_at);
                println!("---");
                println!("{}", proposal.body);
                Ok(0)
            }
            Some("apply") => {
                let Some(id) = args.get(1) else {
                    eprintln!("usage: minipaw config advisor proposals apply <id>");
                    return Ok(2);
                };
                let Some(proposal) = find_proposal(&self.workspace, id) else {
                    eprintln!("proposal not found: {id}");
                    return Ok(2);
                };
                let prompts = match PromptStore::install(&self.workspace) {
                    Ok(p) => p,
                    Err(err) => {
                        eprintln!("prompt store unavailable: {err}");
                        return Ok(2);
                    }
                };
                match apply_proposal(&self.workspace, &prompts, &proposal) {
                    Ok(outcome) => {
                        println!("applied {id}: {outcome}");
                        Ok(0)
                    }
                    Err(err) => {
                        eprintln!("apply failed: {err}");
                        Ok(2)
                    }
                }
            }
            Some("reject") => {
                let Some(id) = args.get(1) else {
                    eprintln!("usage: minipaw config advisor proposals reject <id>");
                    return Ok(2);
                };
                let Some(proposal) = find_proposal(&self.workspace, id) else {
                    eprintln!("proposal not found: {id}");
                    return Ok(2);
                };
                match reject_proposal(&proposal) {
                    Ok(()) => {
                        println!("rejected {id}");
                        Ok(0)
                    }
                    Err(err) => {
                        eprintln!("reject failed: {err}");
                        Ok(2)
                    }
                }
            }
            Some(other) => {
                eprintln!("unknown proposals subcommand: {other}");
                Ok(2)
            }
        }
    }

    fn config_advisor_set(&self, args: &[String]) -> io::Result<i32> {
        let file_config = read_file_config(&self.workspace);
        let current = file_config.advisor.as_ref().map(|a| &a.agent);
        let mut provider = current
            .map(|c| c.provider.clone())
            .unwrap_or_else(|| "deepseek".to_owned());
        let mut url = current
            .map(|c| c.url.clone())
            .unwrap_or_else(|| "https://api.deepseek.com/v1".to_owned());
        let mut model = current
            .map(|c| c.model.clone())
            .unwrap_or_else(|| "deepseek-chat".to_owned());
        let mut api_key: Option<String> = current.and_then(|c| c.api_key.clone());
        let mut clear_key = false;

        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--provider" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--provider requires a value");
                        return Ok(2);
                    };
                    provider = val.clone();
                    index += 2;
                }
                "--url" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--url requires a value");
                        return Ok(2);
                    };
                    url = val.clone();
                    index += 2;
                }
                "--model" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--model requires a value");
                        return Ok(2);
                    };
                    model = val.clone();
                    index += 2;
                }
                "--api-key" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--api-key requires a value");
                        return Ok(2);
                    };
                    if val.is_empty() {
                        clear_key = true;
                    } else {
                        api_key = Some(val.clone());
                    }
                    index += 2;
                }
                unknown => {
                    eprintln!("unknown advisor set option: {unknown}");
                    return Ok(2);
                }
            }
        }

        if clear_key {
            api_key = None;
        }

        write_advisor_agent(&self.workspace, &provider, &url, &model, api_key.as_deref())?;
        println!("advisor.provider={provider}");
        println!("advisor.url={url}");
        println!("advisor.model={model}");
        if api_key.is_some() {
            println!("advisor.api_key=<set>");
        }
        Ok(0)
    }

    fn config_advisor_show(&self) -> io::Result<i32> {
        let file_config = read_file_config(&self.workspace);
        let Some(advisor) = file_config.advisor else {
            println!("advisor: not configured");
            return Ok(0);
        };
        println!("advisor.provider={}", advisor.agent.provider);
        println!("advisor.url={}", advisor.agent.url);
        println!("advisor.model={}", advisor.agent.model);
        if advisor.agent.api_key.as_ref().is_some_and(|k| !k.is_empty()) {
            println!("advisor.api_key=<set>");
        }
        println!("advisor.mode={}", advisor.mode);
        for class in [
            MessageClass::MiniHow,
            MessageClass::MiniWhy,
            MessageClass::MiniWhat,
        ] {
            let choice = advisor
                .routing
                .get(&class)
                .copied()
                .unwrap_or(AgentChoice::Primary);
            println!("advisor.routing.{class}={choice}");
        }
        Ok(0)
    }

    fn config_advisor_mode(&self, args: &[String]) -> io::Result<i32> {
        let Some(raw) = args.first() else {
            eprintln!("usage: minipaw config advisor mode <training|trial|working>");
            return Ok(2);
        };
        let Some(mode) = AdvisorMode::parse(raw) else {
            eprintln!("unknown advisor mode: {raw} (expected training|trial|working)");
            return Ok(2);
        };
        match write_advisor_mode(&self.workspace, mode) {
            Ok(()) => {
                println!("advisor.mode={mode}");
                Ok(0)
            }
            Err(err) => {
                eprintln!("advisor mode update failed: {err}");
                Ok(2)
            }
        }
    }

    fn config_advisor_route(&self, args: &[String]) -> io::Result<i32> {
        let (Some(class_raw), Some(choice_raw)) = (args.first(), args.get(1)) else {
            eprintln!(
                "usage: minipaw config advisor route <minihow|miniwhy|miniwhat> <primary|advisor>"
            );
            return Ok(2);
        };
        let Some(class) = MessageClass::parse(class_raw) else {
            eprintln!("unknown message class: {class_raw}");
            return Ok(2);
        };
        let Some(choice) = AgentChoice::parse(choice_raw) else {
            eprintln!("unknown agent choice: {choice_raw}");
            return Ok(2);
        };
        match write_advisor_route(&self.workspace, class, choice) {
            Ok(()) => {
                println!("advisor.routing.{class}={choice}");
                Ok(0)
            }
            Err(err) => {
                eprintln!("advisor route update failed: {err}");
                Ok(2)
            }
        }
    }

    fn config_advisor_clear(&self) -> io::Result<i32> {
        clear_advisor(&self.workspace)?;
        println!("advisor cleared");
        Ok(0)
    }

    fn config_set(&self, args: &[String]) -> io::Result<i32> {
        let file_config = read_file_config(&self.workspace);
        let current = file_config.primary_agent.as_ref();
        let mut provider =
            current.map(|c| c.provider.clone()).unwrap_or_else(|| "llamacpp".to_owned());
        let mut url = current
            .map(|c| c.url.clone())
            .unwrap_or_else(|| "http://127.0.0.1:8080/v1".to_owned());
        let mut model =
            current.map(|c| c.model.clone()).unwrap_or_else(|| "local-model".to_owned());
        let mut api_key: Option<String> = current.and_then(|c| c.api_key.clone());
        let mut clear_key = false;

        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--provider" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--provider requires a value");
                        return Ok(2);
                    };
                    provider = val.clone();
                    index += 2;
                }
                "--url" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--url requires a value");
                        return Ok(2);
                    };
                    url = val.clone();
                    index += 2;
                }
                "--model" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--model requires a value");
                        return Ok(2);
                    };
                    model = val.clone();
                    index += 2;
                }
                "--api-key" => {
                    let Some(val) = args.get(index + 1) else {
                        eprintln!("--api-key requires a value");
                        return Ok(2);
                    };
                    if val.is_empty() {
                        clear_key = true;
                    } else {
                        api_key = Some(val.clone());
                    }
                    index += 2;
                }
                "--help" | "-h" => {
                    eprintln!(
                        "usage: minipaw config set [--provider p] [--url u] [--model m] [--api-key k]\n\
                         Updates model config in minipaw.json; unspecified fields keep their current values.\n\
                         Use --api-key \"\" to remove the api key."
                    );
                    return Ok(0);
                }
                unknown => {
                    eprintln!("unknown config set option: {unknown}");
                    return Ok(2);
                }
            }
        }

        if clear_key {
            api_key = None;
        }

        write_primary_config(&self.workspace, &provider, &url, &model, api_key.as_deref())?;
        println!("provider={provider}");
        println!("url={url}");
        println!("model={model}");
        if api_key.is_some() {
            println!("api_key=<set>");
        }
        Ok(0)
    }

    fn gateway(&mut self, args: &[String]) -> io::Result<i32> {
        match args.first().map(String::as_str) {
            Some("run") => self.gateway_run(&args[1..]),
            Some("simulate") => self.gateway_simulate(&args[1..]),
            _ => {
                eprintln!("usage: minipaw gateway run [--once] [--no-stdin] [--poll-timeout <secs>] | gateway simulate [--once]");
                Ok(2)
            }
        }
    }

    fn gateway_run(&mut self, args: &[String]) -> io::Result<i32> {
        let mut once = false;
        let mut read_stdin = true;
        let mut poll_timeout_secs = 5u64;
        let mut index = 0usize;
        while index < args.len() {
            match args[index].as_str() {
                "--once" => {
                    once = true;
                    index += 1;
                }
                "--no-stdin" => {
                    read_stdin = false;
                    index += 1;
                }
                "--poll-timeout" => {
                    let Some(value) = args.get(index + 1) else {
                        eprintln!("--poll-timeout requires seconds");
                        return Ok(2);
                    };
                    poll_timeout_secs = value.parse::<u64>().unwrap_or(5).clamp(1, 25);
                    index += 2;
                }
                "--help" | "-h" => {
                    print_gateway_run_help();
                    return Ok(0);
                }
                unknown => {
                    eprintln!("unknown gateway run option: {unknown}");
                    print_gateway_run_help();
                    return Ok(2);
                }
            }
        }

        let config = AgentConfig::from_env(self.workspace.clone());
        let config_path = self.workspace.join("minipaw.json");
        let telegram_token = match config.telegram_token.clone() {
            Some(token) => match validate_telegram_token(&token) {
                Ok(()) => Some(token),
                Err(reason) => {
                    eprintln!("telegram token is invalid: {reason}");
                    eprintln!(
                        "fix it with: minipaw config telegram set --token <bot-token> --chats <chat-id[,chat-id...]>"
                    );
                    return Ok(2);
                }
            },
            None => None,
        };
        let telegram_configured = telegram_token.is_some();
        let channel = TelegramChannel::new(TelegramConfig {
            token: telegram_token.clone().unwrap_or_default(),
            allowed_chats: config.telegram_allowed_chats.clone(),
        });
        let stdin_rx = if read_stdin {
            Some(spawn_gateway_stdin_reader())
        } else {
            None
        };

        println!("minipaw gateway running");
        println!("workspace={}", self.workspace.display());
        println!("config={}", config_path.display());
        println!(
            "model={}",
            config
                .primary_agent
                .as_ref()
                .map(|llm| format!("{}:{}", llm.provider, llm.model))
                .unwrap_or_else(|| "offline".to_owned())
        );
        println!(
            "telegram={} allowed_chats={}",
            if telegram_configured { "configured" } else { "not-configured" },
            join_chat_ids(&config.telegram_allowed_chats)
        );
        if read_stdin {
            println!("stdin: agent <session-key> <message> | telegram <chat-id> <message> | /quit");
        }

        if !telegram_configured && !read_stdin {
            eprintln!("gateway has no inputs: configure telegram or omit --no-stdin");
            return Ok(2);
        }

        let mut offset = None;
        loop {
            if let Some(rx) = &stdin_rx {
                loop {
                    match rx.try_recv() {
                        Ok(line) => {
                            if self.handle_gateway_stdin_line(&channel, &line)? {
                                return Ok(0);
                            }
                            if once {
                                return Ok(0);
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }

            if let Some(token) = telegram_token.as_deref() {
                match get_updates(token, offset, poll_timeout_secs) {
                    Ok(messages) => {
                        for message in messages {
                            offset = Some(message.update_id + 1);
                            let chat_id = message.chat_id;
                            println!(
                                "gateway recv channel=telegram update={} chat={} bytes={}",
                                message.update_id,
                                chat_id,
                                message.text.len()
                            );
                            self.handle_gateway_telegram_message(
                                &channel,
                                chat_id,
                                &message.text,
                                Some(token),
                            )?;
                            if once {
                                return Ok(0);
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("gateway telegram poll failed: {err}");
                        if is_telegram_not_found_error(&err) {
                            eprintln!(
                                "telegram returned 404; the configured bot token is not recognized by Telegram"
                            );
                            eprintln!(
                                "fix it with: minipaw config telegram set --token <bot-token> --chats <chat-id[,chat-id...]>"
                            );
                            return Ok(2);
                        }
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    fn gateway_simulate(&mut self, args: &[String]) -> io::Result<i32> {
        let mut once = false;
        for arg in args {
            match arg.as_str() {
                "--once" => once = true,
                "--help" | "-h" => {
                    print_gateway_simulate_help();
                    return Ok(0);
                }
                unknown => {
                    eprintln!("unknown gateway simulate option: {unknown}");
                    print_gateway_simulate_help();
                    return Ok(2);
                }
            }
        }

        let config = AgentConfig::from_env(self.workspace.clone());
        let channel = TelegramChannel::new(TelegramConfig {
            token: config.telegram_token.unwrap_or_default(),
            allowed_chats: config.telegram_allowed_chats,
        });

        println!("minipaw simulated gateway");
        println!("workspace={}", self.workspace.display());
        println!("input: telegram <chat-id> <message>");
        println!("input: agent <session-key> <message>");
        println!("input: /quit");

        let stdin = io::stdin();
        let mut processed = 0usize;
        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "/quit" || trimmed == "/exit" {
                break;
            }

            match parse_simulated_gateway_event(trimmed) {
                Some(SimulatedGatewayEvent::Telegram { chat_id, text }) => {
                    self.handle_gateway_telegram_message(&channel, chat_id, &text, None)?;
                    processed += 1;
                }
                Some(SimulatedGatewayEvent::Agent { session_key, text }) => {
                    self.handle_simulated_agent_message(&session_key, &text)?;
                    processed += 1;
                }
                None => {
                    eprintln!("ignored malformed gateway event: {trimmed}");
                    eprintln!("expected: telegram <chat-id> <message> | agent <session-key> <message>");
                }
            }

            if once && processed > 0 {
                break;
            }
        }

        Ok(0)
    }

    fn handle_gateway_stdin_line(
        &mut self,
        channel: &TelegramChannel,
        line: &str,
    ) -> io::Result<bool> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(false);
        }
        if trimmed == "/quit" || trimmed == "/exit" {
            println!("gateway stopping");
            return Ok(true);
        }
        match parse_simulated_gateway_event(trimmed) {
            Some(SimulatedGatewayEvent::Telegram { chat_id, text }) => {
                self.handle_gateway_telegram_message(channel, chat_id, &text, None)?;
            }
            Some(SimulatedGatewayEvent::Agent { session_key, text }) => {
                self.handle_simulated_agent_message(&session_key, &text)?;
            }
            None => {
                eprintln!("ignored malformed gateway event: {trimmed}");
                eprintln!("expected: telegram <chat-id> <message> | agent <session-key> <message>");
            }
        }
        Ok(false)
    }

    fn handle_gateway_telegram_message(
        &mut self,
        channel: &TelegramChannel,
        chat_id: i64,
        text: &str,
        telegram_token: Option<&str>,
    ) -> io::Result<()> {
        let message = TelegramMessage {
            update_id: 0,
            chat_id,
            text: text.to_owned(),
        };
        let session_key = format!("agent:main:telegram:chat:{chat_id}");
        print_gateway_event(
            "session.message",
            &session_key,
            "telegram",
            &format!("telegram:{chat_id}"),
            "user",
            text,
        );

        match channel.admit_message(message) {
            TelegramAdmission::Accepted(text) => {
                println!(
                    "gateway route channel=telegram task={}",
                    quote_compact(&text)
                );
                let msg = IncomingMessage {
                    text,
                    context_id: Some(chat_id.to_string()),
                    source: "telegram".to_owned(),
                };
                let report = self.core.process(msg);
                let reply = format_session_report(&report);
                println!("gateway task [{}] steps={}", report.class, report.steps);
                print_gateway_event(
                    "session.message",
                    &session_key,
                    "telegram",
                    &format!("telegram:{chat_id}"),
                    "assistant",
                    &reply,
                );
                if let Some(token) = telegram_token {
                    match send_message(token, chat_id, &reply) {
                        Ok(()) => println!("gateway deliver channel=telegram to=telegram:{chat_id} sent=true"),
                        Err(err) => eprintln!("gateway telegram send failed: {err}"),
                    }
                } else {
                    println!("gateway deliver channel=telegram to=telegram:{chat_id} sent=false");
                }
            }
            TelegramAdmission::PairingRequired(text) => {
                print_gateway_event(
                    "session.message",
                    &session_key,
                    "telegram",
                    &format!("telegram:{chat_id}"),
                    "assistant",
                    &text,
                );
                if let Some(token) = telegram_token {
                    match send_message(token, chat_id, &text) {
                        Ok(()) => println!("gateway deliver channel=telegram to=telegram:{chat_id} pairing=required sent=true"),
                        Err(err) => eprintln!("gateway telegram pairing send failed: {err}"),
                    }
                } else {
                    println!("gateway deliver channel=telegram to=telegram:{chat_id} pairing=required sent=false");
                }
            }
        }
        Ok(())
    }

    fn handle_simulated_agent_message(
        &mut self,
        session_key: &str,
        text: &str,
    ) -> io::Result<()> {
        let routed = format!(
            "[Inter-session message] sourceSessionKey={} isUser=false\n{}",
            session_key, text
        );
        print_gateway_event(
            "session.message",
            session_key,
            "agent",
            session_key,
            "user",
            text,
        );
        println!(
            "gateway route channel=agent kind=inter-session task={}",
            quote_compact(&routed)
        );
        let msg = IncomingMessage {
            text: routed,
            context_id: Some(session_key.to_owned()),
            source: "agent".to_owned(),
        };
        let report = self.core.process(msg);
        print_gateway_event(
            "session.message",
            session_key,
            "agent",
            session_key,
            "assistant",
            &report.output,
        );
        Ok(())
    }

    fn onboarding(&self) -> io::Result<i32> {
        std::fs::create_dir_all(&self.workspace)?;
        println!("minipaw onboarding");
        println!("workspace={}", self.workspace.display());

        let config = read_file_config(&self.workspace);
        let current = config.primary_agent.as_ref();
        let default_provider = current
            .map(|llm| llm.provider.as_str())
            .unwrap_or("llamacpp");

        let provider = prompt_default("model provider (llamacpp/deepseek)", default_provider)?;
        let provider_str = provider.trim();
        let url_default = current
            .map(|llm| llm.url.as_str())
            .unwrap_or(if provider_str == "deepseek" {
                "https://api.deepseek.com/v1"
            } else {
                "http://127.0.0.1:8080/v1"
            });
        let model_default = current
            .map(|llm| llm.model.as_str())
            .unwrap_or(if provider_str == "deepseek" { "deepseek-chat" } else { "local-model" });
        let url = prompt_default("model url", url_default)?;
        let model = prompt_default("model name", model_default)?;
        let api_key = if matches!(provider_str, "deepseek" | "openai") {
            let key_default = current.and_then(|llm| llm.api_key.as_deref()).unwrap_or("");
            let key = prompt_default("api key", key_default)?;
            if key.trim().is_empty() { None } else { Some(key.trim().to_owned()) }
        } else {
            current.and_then(|llm| llm.api_key.clone())
        };
        write_primary_config(&self.workspace, &provider, &url, &model, api_key.as_deref())?;
        println!("model configured: provider={provider} model={model}");

        let channel = prompt_default("channel (none/telegram)", "none")?;
        match channel.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "local" | "cli" => {
                println!("channel configured: local CLI only");
            }
            "telegram" => {
                let previous = read_file_config(&self.workspace).telegram;
                let token_default = previous
                    .as_ref()
                    .map(|telegram| telegram.token.as_str())
                    .unwrap_or("");
                let chats_default = previous
                    .as_ref()
                    .map(|telegram| join_chat_ids(&telegram.allowed_chats))
                    .unwrap_or_default();
                let token = prompt_default("telegram bot token", token_default)?;
                let chats = prompt_default("telegram chat ids", &chats_default)?;
                let parsed_chats = parse_chat_ids(&chats);
                if token.trim().is_empty() || parsed_chats.is_empty() {
                    eprintln!("telegram setup skipped: token and at least one numeric chat id are required");
                } else {
                    write_telegram_config(&self.workspace, token.trim(), &parsed_chats)?;
                    println!(
                        "telegram configured: token={} chats={}",
                        mask_token(token.trim()),
                        join_chat_ids(&parsed_chats)
                    );
                }
            }
            other => {
                eprintln!("unknown channel: {other}");
                eprintln!("supported channels: none, telegram");
                return Ok(2);
            }
        }

        Ok(0)
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
        if let Err(reason) = validate_telegram_token(&token) {
            eprintln!("telegram token is invalid: {reason}");
            eprintln!(
                "fix it with: minipaw config telegram set --token <bot-token> --chats <chat-id[,chat-id...]>"
            );
            return Ok(2);
        }
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
                Err(err) => {
                    eprintln!("telegram poll failed: {err}");
                    if is_telegram_not_found_error(&err) {
                        eprintln!(
                            "telegram returned 404; the configured bot token is not recognized by Telegram"
                        );
                        return Ok(2);
                    }
                }
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

fn validate_telegram_token(token: &str) -> Result<(), &'static str> {
    let Some((bot_id, secret)) = token.split_once(':') else {
        return Err("expected <numeric-bot-id>:<token-secret>");
    };
    if bot_id.is_empty() || !bot_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("bot id before ':' must be numeric");
    }
    if secret.len() < 20 {
        return Err("token secret is too short");
    }
    if !secret
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err("token secret contains invalid characters");
    }
    Ok(())
}

fn is_telegram_not_found_error(error: &str) -> bool {
    error.contains(" 404")
        || error.contains("error: 404")
        || error.contains("returned error: 404")
        || error.contains("\"error_code\":404")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SimulatedGatewayEvent {
    Telegram { chat_id: i64, text: String },
    Agent { session_key: String, text: String },
}

fn parse_simulated_gateway_event(line: &str) -> Option<SimulatedGatewayEvent> {
    let (kind, rest) = line.split_once(' ')?;
    match kind {
        "telegram" | "tg" => {
            let (chat_id, text) = rest.trim().split_once(' ')?;
            Some(SimulatedGatewayEvent::Telegram {
                chat_id: chat_id.parse::<i64>().ok()?,
                text: text.trim().to_owned(),
            })
        }
        "agent" => {
            let (session_key, text) = rest.trim().split_once(' ')?;
            Some(SimulatedGatewayEvent::Agent {
                session_key: session_key.trim().to_owned(),
                text: text.trim().to_owned(),
            })
        }
        _ => None,
    }
}

fn print_gateway_simulate_help() {
    eprintln!(
        "usage: minipaw gateway simulate [--once]\n\
         Reads simulated gateway events from stdin:\n\
         telegram <chat-id> <message>\n\
         agent <session-key> <message>\n\
         /quit"
    );
}

fn print_gateway_run_help() {
    eprintln!(
        "usage: minipaw gateway run [--once] [--no-stdin] [--poll-timeout <secs>]\n\
         Runs the foreground gateway using minipaw.json from the default workspace.\n\
         Inputs:\n\
         - Telegram long polling when telegram.bot_token is configured\n\
         - stdin events unless --no-stdin is set:\n\
           telegram <chat-id> <message>\n\
           agent <session-key> <message>\n\
           /quit"
    );
}

fn spawn_gateway_stdin_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn print_gateway_event(
    event: &str,
    session_key: &str,
    channel: &str,
    to: &str,
    role: &str,
    text: &str,
) {
    let text = cap_gateway_text(text, 1200);
    println!(
        "event={} sessionKey={} channel={} to={} role={} text={}",
        event,
        quote_compact(session_key),
        quote_compact(channel),
        quote_compact(to),
        quote_compact(role),
        quote_compact(&text)
    );
}

fn cap_gateway_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}

fn quote_compact(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn parse_task_id(value: &str) -> Option<crate::types::TaskId> {
    value
        .strip_prefix('t')
        .unwrap_or(value)
        .parse::<u64>()
        .ok()
        .map(crate::types::TaskId)
}

fn proposal_summary(p: &ProposalEntry) -> String {
    let body = p.body.trim();
    let snippet = body.lines().next().unwrap_or("").trim();
    let mut head = String::new();
    if let Some(class) = p.class {
        head.push_str(&format!("[{class}] "));
    }
    if snippet.len() > 100 {
        let mut end = 100;
        while !snippet.is_char_boundary(end) {
            end -= 1;
        }
        head.push_str(&snippet[..end]);
        head.push('…');
    } else {
        head.push_str(snippet);
    }
    head
}

fn parse_chat_ids(value: &str) -> std::collections::BTreeSet<i64> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

fn uninstall_from_env(workspace: &std::path::Path, args: &[String]) -> io::Result<i32> {
    let mut mode = None;
    let mut assume_yes = false;

    for arg in args {
        match arg.as_str() {
            "--keep-data" => mode = Some(UninstallMode::KeepData),
            "--remove-user-data" | "--remove-data" => mode = Some(UninstallMode::RemoveUserData),
            "--purge" | "--remove-minipaw-folder" | "--remove-folder" => {
                mode = Some(UninstallMode::Purge)
            }
            "--yes" | "-y" => assume_yes = true,
            "--help" | "-h" => {
                print_uninstall_help();
                return Ok(0);
            }
            unknown => {
                eprintln!("unknown uninstall option: {unknown}");
                print_uninstall_help();
                return Ok(2);
            }
        }
    }

    let mode = match mode {
        Some(mode) => mode,
        None if assume_yes => UninstallMode::KeepData,
        None => prompt_uninstall_mode()?,
    };

    let install_dir = workspace;
    if install_dir.as_os_str().is_empty() || install_dir == std::path::Path::new("/") {
        eprintln!("refusing to uninstall from unsafe path: {}", install_dir.display());
        return Ok(2);
    }

    println!("minipaw uninstall");
    println!("install_dir={}", install_dir.display());
    println!(
        "mode={}",
        match mode {
            UninstallMode::KeepData => "keep-data",
            UninstallMode::RemoveUserData => "remove-user-data",
            UninstallMode::Purge => "purge",
        }
    );

    if !assume_yes && !confirm("continue?")? {
        println!("uninstall cancelled");
        return Ok(0);
    }

    remove_bashrc_block()?;

    match mode {
        UninstallMode::KeepData => {
            remove_file_if_exists(&install_dir.join("minipaw"))?;
            println!("removed minipaw binary and shell profile block; kept user data");
        }
        UninstallMode::RemoveUserData => {
            remove_file_if_exists(&install_dir.join("minipaw"))?;
            remove_file_if_exists(&install_dir.join("SOUL.md"))?;
            remove_file_if_exists(&install_dir.join("minipaw.json"))?;
            remove_dir_if_exists(&install_dir.join("skills"))?;
            remove_dir_if_exists(&install_dir.join("memory"))?;
            remove_dir_if_exists(&install_dir.join("workspace"))?;
            println!("removed minipaw binary, managed files, and user data");
        }
        UninstallMode::Purge => {
            remove_dir_if_exists(install_dir)?;
            println!("removed minipaw folder");
        }
    }

    Ok(0)
}

fn print_uninstall_help() {
    eprintln!(
        "usage: minipaw uninstall [--keep-data | --remove-user-data | --purge] [--yes]\n\
         --keep-data            remove binary and shell profile block; keep ~/.minipaw data\n\
         --remove-user-data     remove binary, config, memory, skills, and workspace\n\
         --purge                remove the entire minipaw folder\n\
         --yes                  do not prompt for confirmation"
    );
}

fn prompt_uninstall_mode() -> io::Result<UninstallMode> {
    println!("Choose uninstall mode:");
    println!("1. keep data: remove binary and shell profile block only");
    println!("2. remove user data: remove config, memory, skills, and workspace");
    println!("3. purge: remove the entire minipaw folder");

    loop {
        let answer = prompt_default("mode (1/2/3)", "1")?;
        match answer.trim() {
            "1" | "keep" | "keep-data" => return Ok(UninstallMode::KeepData),
            "2" | "remove" | "remove-data" | "remove-user-data" => {
                return Ok(UninstallMode::RemoveUserData)
            }
            "3" | "purge" | "remove-folder" | "remove-minipaw-folder" => {
                return Ok(UninstallMode::Purge)
            }
            _ => eprintln!("enter 1, 2, or 3"),
        }
    }
}

fn confirm(prompt: &str) -> io::Result<bool> {
    let answer = prompt_default(prompt, "no")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn remove_bashrc_block() -> io::Result<()> {
    let Some(home) = env::var_os("HOME") else {
        return Ok(());
    };
    let bashrc = std::path::PathBuf::from(home).join(".bashrc");
    let Ok(text) = std::fs::read_to_string(&bashrc) else {
        return Ok(());
    };
    let updated = remove_managed_block(&text);
    if updated != text {
        std::fs::write(&bashrc, updated)?;
        println!("removed minipaw block from {}", bashrc.display());
    }
    Ok(())
}

fn remove_managed_block(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.lines() {
        if line.trim() == "# minipaw managed block" {
            skipping = true;
            continue;
        }
        if skipping {
            if line.trim() == "# end minipaw managed block" {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn remove_file_if_exists(path: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn remove_dir_if_exists(path: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn prompt_default(label: &str, default: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    if default.is_empty() {
        write!(stdout, "{label}: ")?;
    } else {
        write!(stdout, "{label} [{default}]: ")?;
    }
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(default.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
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
            .unwrap_or_else(|_| config.workspace.join("memory").join("minipaw.sqlite3"));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simulated_telegram_event() {
        assert_eq!(
            parse_simulated_gateway_event("telegram 123 hello there"),
            Some(SimulatedGatewayEvent::Telegram {
                chat_id: 123,
                text: "hello there".to_owned(),
            })
        );
    }

    #[test]
    fn parses_simulated_agent_event() {
        assert_eq!(
            parse_simulated_gateway_event("agent agent:main:telegram:chat:123 forwarded note"),
            Some(SimulatedGatewayEvent::Agent {
                session_key: "agent:main:telegram:chat:123".to_owned(),
                text: "forwarded note".to_owned(),
            })
        );
    }

    #[test]
    fn caps_gateway_text_on_char_boundary() {
        let capped = cap_gateway_text("abcédef", 5);

        assert_eq!(capped, "abcé [truncated]");
    }

    #[test]
    fn rejects_malformed_telegram_token() {
        assert_eq!(
            validate_telegram_token("123:abc\",").unwrap_err(),
            "token secret is too short"
        );
        assert_eq!(
            validate_telegram_token("123:abcdefghijklmnopqrstuvwxyz\",").unwrap_err(),
            "token secret contains invalid characters"
        );
    }

    #[test]
    fn accepts_well_formed_telegram_token_shape() {
        assert!(validate_telegram_token("123456:abcdefghijklmnopqrstuvwxyz_ABC-123").is_ok());
    }
}
