use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::types::{AdvisorMode, AgentChoice, MessageClass};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub workspace: PathBuf,
    pub history_limit: usize,
    pub max_file_bytes: usize,
    pub max_tool_output_bytes: usize,
    pub tool_timeout: Duration,
    pub allow_exec: bool,
    pub allowed_exec: BTreeSet<String>,
    pub telegram_token: Option<String>,
    pub telegram_allowed_chats: BTreeSet<i64>,
    pub primary_agent: Option<LlmConfig>,
    pub advisor: Option<AdvisorConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub provider: String,
    pub url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub thinking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisorConfig {
    pub mode: AdvisorMode,
    pub agent: LlmConfig,
    pub routing: BTreeMap<MessageClass, AgentChoice>,
}

impl AdvisorConfig {
    pub fn new(agent: LlmConfig) -> Self {
        Self {
            mode: AdvisorMode::Trial,
            agent,
            routing: BTreeMap::new(),
        }
    }

    /// Return the agent that should serve this message class, defaulting to
    /// `Primary` when no rule has been set.
    pub fn route_for(&self, class: MessageClass) -> AgentChoice {
        self.routing
            .get(&class)
            .copied()
            .unwrap_or(AgentChoice::Primary)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramBotConfig {
    pub token: String,
    pub allowed_chats: BTreeSet<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileConfig {
    pub primary_agent: Option<LlmConfig>,
    pub telegram: Option<TelegramBotConfig>,
    pub advisor: Option<AdvisorConfig>,
}

impl AgentConfig {
    pub fn constrained(workspace: PathBuf) -> Self {
        Self {
            workspace,
            history_limit: 16,
            max_file_bytes: 16 * 1024,
            max_tool_output_bytes: 24 * 1024,
            tool_timeout: Duration::from_secs(10),
            allow_exec: true,
            allowed_exec: BTreeSet::new(),
            telegram_token: None,
            telegram_allowed_chats: BTreeSet::new(),
            primary_agent: None,
            advisor: None,
        }
    }

    pub fn from_env(workspace: PathBuf) -> Self {
        let mut config = Self::constrained(workspace);
        let file_config = read_file_config(&config.workspace);
        if env::var("MINIPAW_ALLOW_EXEC").is_ok() {
            config.allow_exec = env_bool("MINIPAW_ALLOW_EXEC");
        }
        config.allowed_exec = env_list("MINIPAW_EXEC_ALLOWLIST").into_iter().collect();
        config.telegram_token = env::var("MINIPAW_TELEGRAM_TOKEN").ok().or_else(|| {
            file_config
                .telegram
                .as_ref()
                .map(|telegram| telegram.token.clone())
        });
        config.telegram_allowed_chats = env::var("MINIPAW_TELEGRAM_CHATS")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|part| part.trim().parse::<i64>().ok())
                    .collect()
            })
            .unwrap_or_else(|| {
                file_config
                    .telegram
                    .as_ref()
                    .map(|telegram| telegram.allowed_chats.clone())
                    .unwrap_or_default()
            });
        config.primary_agent = file_config.primary_agent.or_else(env_llm_config);
        config.advisor = file_config.advisor;
        config
    }
}

pub fn default_workspace() -> PathBuf {
    env::var("MINIPAW_HOME")
        .map(PathBuf::from)
        .or_else(|_| env::var("HOME").map(|home| PathBuf::from(home).join(".minipaw")))
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn read_file_config(workspace: &std::path::Path) -> FileConfig {
    let Some(text) = fs::read_to_string(workspace.join("minipaw.json")).ok() else {
        return FileConfig::default();
    };

    let primary_agent = extract_object(&text, "primary").and_then(|primary| {
        Some(LlmConfig {
            provider: extract_json_string(primary, "provider")?,
            url: extract_json_string(primary, "url")?,
            model: extract_json_string(primary, "model")?,
            api_key: extract_json_string(primary, "api_key"),
            thinking: extract_json_bool(primary, "thinking").unwrap_or(false),
        })
    });

    let advisor_agent = extract_object(&text, "advisor").and_then(|advisor| {
        Some(LlmConfig {
            provider: extract_json_string(advisor, "provider")?,
            url: extract_json_string(advisor, "url")?,
            model: extract_json_string(advisor, "model")?,
            api_key: extract_json_string(advisor, "api_key"),
            thinking: extract_json_bool(advisor, "thinking").unwrap_or(false),
        })
    });

    let advisor = advisor_agent.map(|agent| {
        let block = extract_object(&text, "advisor_mode");
        let mode = block
            .and_then(|inner| extract_json_string(inner, "mode"))
            .and_then(|raw| AdvisorMode::parse(&raw))
            .unwrap_or(AdvisorMode::Trial);
        let routing = block.map(parse_routing_block).unwrap_or_default();
        AdvisorConfig {
            mode,
            agent,
            routing,
        }
    });

    let telegram = extract_object(&text, "telegram").and_then(|telegram| {
        Some(TelegramBotConfig {
            token: extract_json_string(telegram, "bot_token")
                .or_else(|| extract_json_string(telegram, "token"))?,
            allowed_chats: extract_json_i64_array(telegram, "allowed_chats"),
        })
    });

    FileConfig {
        primary_agent,
        telegram,
        advisor,
    }
}

fn parse_routing_block(block: &str) -> BTreeMap<MessageClass, AgentChoice> {
    let mut routing = BTreeMap::new();
    let Some(routing_text) = extract_object(block, "routing") else {
        return routing;
    };
    for class in [
        MessageClass::MiniHow,
        MessageClass::MiniWhy,
        MessageClass::MiniWhat,
    ] {
        if let Some(raw) = extract_json_string(routing_text, &class.to_string()) {
            if let Some(choice) = AgentChoice::parse(&raw) {
                routing.insert(class, choice);
            }
        }
    }
    routing
}

pub fn write_primary_config(
    workspace: &std::path::Path,
    provider: &str,
    url: &str,
    model: &str,
    api_key: Option<&str>,
) -> std::io::Result<()> {
    let mut file_config = read_file_config(workspace);
    let thinking = file_config.primary_agent.as_ref().map(|c| c.thinking).unwrap_or(false);
    let api_key = file_config.primary_agent.as_ref().and_then(|c| c.api_key.clone());
    file_config.primary_agent = Some(LlmConfig {
        provider: provider.to_owned(),
        url: url.to_owned(),
        model: model.to_owned(),
        api_key: api_key.filter(|k| !k.is_empty()),
        thinking,
    });

    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )
}

pub fn write_telegram_config(
    workspace: &std::path::Path,
    token: &str,
    chats: &BTreeSet<i64>,
) -> std::io::Result<()> {
    let mut file_config = read_file_config(workspace);
    file_config.telegram = Some(TelegramBotConfig {
        token: token.to_owned(),
        allowed_chats: chats.clone(),
    });

    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )
}

pub fn pair_telegram_chat(workspace: &std::path::Path, chat_id: i64) -> std::io::Result<bool> {
    let mut file_config = read_file_config(workspace);
    let Some(telegram) = file_config.telegram.as_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "telegram bot token is not configured",
        ));
    };
    let inserted = telegram.allowed_chats.insert(chat_id);
    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )?;
    Ok(inserted)
}

pub fn write_advisor_agent(
    workspace: &std::path::Path,
    provider: &str,
    url: &str,
    model: &str,
    api_key: Option<&str>,
) -> std::io::Result<()> {
    let mut file_config = read_file_config(workspace);
    let agent = LlmConfig {
        provider: provider.to_owned(),
        url: url.to_owned(),
        model: model.to_owned(),
        api_key: api_key.filter(|k| !k.is_empty()).map(str::to_owned),
        thinking: false,
    };
    file_config.advisor = Some(match file_config.advisor.take() {
        Some(mut existing) => {
            existing.agent = agent;
            existing
        }
        None => AdvisorConfig::new(agent),
    });

    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )
}

pub fn write_advisor_mode(
    workspace: &std::path::Path,
    mode: AdvisorMode,
) -> std::io::Result<()> {
    let mut file_config = read_file_config(workspace);
    let Some(advisor) = file_config.advisor.as_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "advisor agent is not configured",
        ));
    };
    advisor.mode = mode;
    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )
}

pub fn write_advisor_route(
    workspace: &std::path::Path,
    class: MessageClass,
    choice: AgentChoice,
) -> std::io::Result<()> {
    let mut file_config = read_file_config(workspace);
    let Some(advisor) = file_config.advisor.as_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "advisor agent is not configured",
        ));
    };
    advisor.routing.insert(class, choice);
    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )
}

pub fn clear_advisor(workspace: &std::path::Path) -> std::io::Result<()> {
    let mut file_config = read_file_config(workspace);
    file_config.advisor = None;
    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )
}

pub fn unpair_telegram_chat(workspace: &std::path::Path, chat_id: i64) -> std::io::Result<bool> {
    let mut file_config = read_file_config(workspace);
    let Some(telegram) = file_config.telegram.as_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "telegram bot token is not configured",
        ));
    };
    let removed = telegram.allowed_chats.remove(&chat_id);
    fs::write(
        workspace.join("minipaw.json"),
        render_file_config(&file_config),
    )?;
    Ok(removed)
}

pub fn render_file_config(config: &FileConfig) -> String {
    let mut sections: Vec<String> = Vec::new();

    let advisor_agent = config.advisor.as_ref().map(|a| &a.agent);
    if config.primary_agent.is_some() || advisor_agent.is_some() {
        let mut block = String::from("  \"agents\": {\n");
        let mut entries = Vec::new();
        if let Some(primary) = &config.primary_agent {
            entries.push(render_named_agent("primary", primary, 4));
        }
        if let Some(agent) = advisor_agent {
            entries.push(render_named_agent("advisor", agent, 4));
        }
        block.push_str(&entries.join(",\n"));
        block.push('\n');
        block.push_str("  }");
        sections.push(block);
    }

    if let Some(advisor) = &config.advisor {
        let mut block = String::from("  \"advisor_mode\": {\n");
        block.push_str(&format!(
            "    \"mode\": \"{}\"",
            json_escape(&advisor.mode.to_string())
        ));
        if !advisor.routing.is_empty() {
            block.push_str(",\n    \"routing\": {\n");
            let routing_entries: Vec<String> = advisor
                .routing
                .iter()
                .map(|(class, choice)| {
                    format!(
                        "      \"{}\": \"{}\"",
                        json_escape(&class.to_string()),
                        json_escape(&choice.to_string())
                    )
                })
                .collect();
            block.push_str(&routing_entries.join(",\n"));
            block.push_str("\n    }\n");
        } else {
            block.push('\n');
        }
        block.push_str("  }");
        sections.push(block);
    }

    if let Some(telegram) = &config.telegram {
        let mut block = String::from("  \"telegram\": {\n");
        block.push_str(&format!(
            "    \"bot_token\": \"{}\",\n",
            json_escape(&telegram.token)
        ));
        block.push_str("    \"allowed_chats\": [");
        for (index, chat) in telegram.allowed_chats.iter().enumerate() {
            if index > 0 {
                block.push_str(", ");
            }
            block.push_str(&chat.to_string());
        }
        block.push_str("]\n");
        block.push_str("  }");
        sections.push(block);
    }

    let mut out = String::from("{\n");
    out.push_str(&sections.join(",\n"));
    if !sections.is_empty() {
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

fn render_named_agent(name: &str, llm: &LlmConfig, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let inner_pad = " ".repeat(indent + 2);
    let mut lines = vec![
        format!("\"provider\": \"{}\"", json_escape(&llm.provider)),
        format!("\"url\": \"{}\"", json_escape(&llm.url)),
        format!("\"model\": \"{}\"", json_escape(&llm.model)),
    ];
    if let Some(key) = llm.api_key.as_deref().filter(|k| !k.is_empty()) {
        lines.push(format!("\"api_key\": \"{}\"", json_escape(key)));
    }
    let body = lines
        .into_iter()
        .map(|line| format!("{inner_pad}{line}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{pad}\"{}\": {{\n{body}\n{pad}}}", json_escape(name))
}

fn env_llm_config() -> Option<LlmConfig> {
    Some(LlmConfig {
        provider: env::var("MINIPAW_LLM_PROVIDER").ok()?,
        url: env::var("MINIPAW_LLM_URL").ok()?,
        model: env::var("MINIPAW_LLM_MODEL").ok()?,
        api_key: env::var("MINIPAW_LLM_API_KEY").ok().filter(|k| !k.is_empty()),
        thinking: env::var("MINIPAW_LLM_THINKING").is_ok_and(|v| {
            matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES")
        }),
    })
}

fn extract_object<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let marker = format!("\"{key}\"");
    let key_pos = text.find(&marker)?;
    let open = text[key_pos..].find('{')? + key_pos;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in text[open..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if ch == '{' {
            depth += 1;
        } else if ch == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(&text[open..open + offset + 1]);
            }
        }
    }
    None
}

fn extract_json_bool(text: &str, key: &str) -> Option<bool> {
    let marker = format!("\"{key}\"");
    let key_pos = text.find(&marker)?;
    let after = text[key_pos + marker.len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    if after.starts_with("true") {
        Some(true)
    } else if after.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn extract_json_string(text: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    let key_pos = text.find(&marker)?;
    let colon = text[key_pos + marker.len()..].find(':')? + key_pos + marker.len();
    let mut chars = text[colon + 1..]
        .char_indices()
        .skip_while(|(_, ch)| ch.is_whitespace());
    let (start_offset, quote) = chars.next()?;
    if quote != '"' {
        return None;
    }

    let start = colon + 1 + start_offset + 1;
    let mut out = String::new();
    let mut escaped = false;
    for ch in text[start..].chars() {
        if escaped {
            out.push(match ch {
                '"' => '"',
                '\\' => '\\',
                '/' => '/',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn extract_json_i64_array(text: &str, key: &str) -> BTreeSet<i64> {
    let marker = format!("\"{key}\"");
    let Some(key_pos) = text.find(&marker) else {
        return BTreeSet::new();
    };
    let Some(colon_offset) = text[key_pos + marker.len()..].find(':') else {
        return BTreeSet::new();
    };
    let colon = key_pos + marker.len() + colon_offset;
    let Some(open_offset) = text[colon + 1..].find('[') else {
        return BTreeSet::new();
    };
    let open = colon + 1 + open_offset;
    let Some(close_offset) = text[open + 1..].find(']') else {
        return BTreeSet::new();
    };
    text[open + 1..open + 1 + close_offset]
        .split(',')
        .filter_map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn env_bool(name: &str) -> bool {
    matches!(
        env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

fn env_list(name: &str) -> Vec<String> {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_primary_config() {
        let text = r#"{"agents":{"primary":{"provider":"llamacpp","url":"http://host/v1","model":"qwen9b"}}}"#;
        let primary = extract_object(text, "primary").unwrap();
        assert_eq!(extract_json_string(primary, "model").unwrap(), "qwen9b");
    }

    #[test]
    fn extracts_telegram_config() {
        let text = r#"{"telegram":{"bot_token":"123:abc","allowed_chats":[10,-20]}}"#;
        let telegram = extract_object(text, "telegram").unwrap();
        assert_eq!(
            extract_json_i64_array(telegram, "allowed_chats"),
            BTreeSet::from([-20, 10])
        );
    }

    #[test]
    fn renders_telegram_without_dropping_primary_agent() {
        let config = FileConfig {
            primary_agent: Some(LlmConfig {
                provider: "llamacpp".into(),
                url: "http://host/v1".into(),
                model: "qwen9b".into(),
                api_key: None,
                thinking: false,
            }),
            telegram: Some(TelegramBotConfig {
                token: "123:abc".into(),
                allowed_chats: BTreeSet::from([10]),
            }),
            advisor: None,
        };

        let rendered = render_file_config(&config);

        assert!(rendered.contains("\"primary\""));
        assert!(rendered.contains("\"telegram\""));
        assert!(rendered.contains("\"allowed_chats\": [10]"));
    }

    #[test]
    fn renders_and_reparses_advisor_block() {
        use crate::types::{AdvisorMode, AgentChoice, MessageClass};
        let mut routing = BTreeMap::new();
        routing.insert(MessageClass::MiniWhy, AgentChoice::Advisor);
        routing.insert(MessageClass::MiniHow, AgentChoice::Primary);
        let config = FileConfig {
            primary_agent: Some(LlmConfig {
                provider: "llamacpp".into(),
                url: "http://host/v1".into(),
                model: "qwen9b".into(),
                api_key: None,
                thinking: false,
            }),
            telegram: None,
            advisor: Some(AdvisorConfig {
                mode: AdvisorMode::Trial,
                agent: LlmConfig {
                    provider: "deepseek".into(),
                    url: "https://api.deepseek.com/v1".into(),
                    model: "deepseek-chat".into(),
                    api_key: Some("sk-test".into()),
                    thinking: false,
                },
                routing,
            }),
        };

        let rendered = render_file_config(&config);
        assert!(rendered.contains("\"advisor\""));
        assert!(rendered.contains("\"deepseek-chat\""));
        assert!(rendered.contains("\"advisor_mode\""));
        assert!(rendered.contains("\"miniwhy\": \"advisor\""));

        // round-trip parse
        let advisor_block = extract_object(&rendered, "advisor_mode").unwrap();
        let parsed_routing = parse_routing_block(advisor_block);
        assert_eq!(
            parsed_routing.get(&MessageClass::MiniWhy),
            Some(&AgentChoice::Advisor)
        );
    }
}
