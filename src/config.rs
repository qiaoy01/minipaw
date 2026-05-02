use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub provider: String,
    pub url: String,
    pub model: String,
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
}

impl AgentConfig {
    pub fn constrained(workspace: PathBuf) -> Self {
        Self {
            workspace,
            history_limit: 16,
            max_file_bytes: 16 * 1024,
            max_tool_output_bytes: 24 * 1024,
            tool_timeout: Duration::from_secs(10),
            allow_exec: false,
            allowed_exec: BTreeSet::new(),
            telegram_token: None,
            telegram_allowed_chats: BTreeSet::new(),
            primary_agent: None,
        }
    }

    pub fn from_env(workspace: PathBuf) -> Self {
        let mut config = Self::constrained(workspace);
        let file_config = read_file_config(&config.workspace);
        config.allow_exec = env_bool("MINIPAW_ALLOW_EXEC");
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
        config
    }
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
        })
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
    }
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
    let mut out = String::from("{\n");
    if let Some(primary) = &config.primary_agent {
        out.push_str("  \"agents\": {\n");
        out.push_str("    \"primary\": {\n");
        out.push_str(&format!(
            "      \"provider\": \"{}\",\n",
            json_escape(&primary.provider)
        ));
        out.push_str(&format!(
            "      \"url\": \"{}\",\n",
            json_escape(&primary.url)
        ));
        out.push_str(&format!(
            "      \"model\": \"{}\"\n",
            json_escape(&primary.model)
        ));
        out.push_str("    }\n");
        out.push_str("  }");
    }

    if let Some(telegram) = &config.telegram {
        if config.primary_agent.is_some() {
            out.push_str(",\n");
        }
        out.push_str("  \"telegram\": {\n");
        out.push_str(&format!(
            "    \"bot_token\": \"{}\",\n",
            json_escape(&telegram.token)
        ));
        out.push_str("    \"allowed_chats\": [");
        for (index, chat) in telegram.allowed_chats.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&chat.to_string());
        }
        out.push_str("]\n");
        out.push_str("  }");
    }

    if config.primary_agent.is_none() && config.telegram.is_none() {
        out.push('\n');
    } else {
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

fn env_llm_config() -> Option<LlmConfig> {
    Some(LlmConfig {
        provider: env::var("MINIPAW_LLM_PROVIDER").ok()?,
        url: env::var("MINIPAW_LLM_URL").ok()?,
        model: env::var("MINIPAW_LLM_MODEL").ok()?,
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
            }),
            telegram: Some(TelegramBotConfig {
                token: "123:abc".into(),
                allowed_chats: BTreeSet::from([10]),
            }),
        };

        let rendered = render_file_config(&config);

        assert!(rendered.contains("\"primary\""));
        assert!(rendered.contains("\"telegram\""));
        assert!(rendered.contains("\"allowed_chats\": [10]"));
    }
}
