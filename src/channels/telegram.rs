use std::collections::BTreeSet;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Knowledge,
    Action,
}

/// Classify a Telegram message as a knowledge question or an action request.
///
/// Action indicators: slash commands, action verbs at the start ("list", "run",
/// "read <path>"), and indirect phrases ("can you list", "please run").
/// Everything else is treated as a knowledge question for the LLM to answer.
pub fn classify_message_kind(text: &str) -> MessageKind {
    let trimmed = text.trim();
    // Explicit slash commands (except /help) are always actions.
    if trimmed.starts_with('/') && !is_help_command(trimmed) {
        return MessageKind::Action;
    }
    let lower = trimmed.to_ascii_lowercase();
    // "list" / "ls" — always an action.
    if lower.starts_with("list ") || lower.starts_with("ls ") || lower == "list" || lower == "ls" {
        return MessageKind::Action;
    }
    // "run" / "exec" — always an action.
    if lower.starts_with("run ") || lower.starts_with("exec ") || lower.starts_with("execute ") {
        return MessageKind::Action;
    }
    // "read" / "cat" — only if followed by something that looks like a file path.
    if (lower.starts_with("read ") || lower.starts_with("cat ")) && text_contains_path(&lower) {
        return MessageKind::Action;
    }
    // Indirect imperative phrases.
    let action_phrases = [
        "please list",
        "please run",
        "please exec",
        "please read",
        "can you list",
        "can you run",
        "can you read",
        "could you list",
        "could you run",
        "could you read",
    ];
    for phrase in &action_phrases {
        if lower.contains(phrase) {
            return MessageKind::Action;
        }
    }
    MessageKind::Knowledge
}

/// Strip common polite prefixes ("can you", "please", etc.) from action text so
/// the planner receives a clean imperative like "list src" or "run git status".
pub fn normalize_action_text(text: &str) -> &str {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let prefixes = [
        "can you please ",
        "can you ",
        "could you please ",
        "could you ",
        "please ",
        "would you please ",
        "would you ",
    ];
    for prefix in &prefixes {
        if lower.starts_with(prefix) {
            return trimmed[prefix.len()..].trim_start();
        }
    }
    trimmed
}

fn is_help_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower == "/help" || lower.starts_with("/help ")
}

/// Return true when `text` contains a token that looks like a file/directory
/// path — has a directory separator or a non-leading dot (file extension).
fn text_contains_path(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        token.contains('/') || token.contains('\\') || {
            let dot = token.rfind('.');
            dot.map(|pos| pos > 0 && pos < token.len() - 1).unwrap_or(false)
        }
    })
}

#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub token: String,
    pub allowed_chats: BTreeSet<i64>,
}

impl TelegramConfig {
    pub fn validate_chat(&self, chat_id: i64) -> bool {
        self.allowed_chats.contains(&chat_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramMessage {
    pub update_id: i64,
    pub chat_id: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelegramAdmission {
    Accepted(String),
    PairingRequired(String),
}

#[derive(Debug, Clone)]
pub struct TelegramChannel {
    config: TelegramConfig,
}

impl TelegramChannel {
    pub fn new(config: TelegramConfig) -> Self {
        Self { config }
    }

    pub fn accept_message(&self, message: TelegramMessage) -> Result<String, String> {
        if !self.config.validate_chat(message.chat_id) {
            return Err("telegram chat is not allowlisted".to_owned());
        }
        Ok(message.text)
    }

    pub fn admit_message(&self, message: TelegramMessage) -> TelegramAdmission {
        if self.config.validate_chat(message.chat_id) {
            TelegramAdmission::Accepted(message.text)
        } else {
            TelegramAdmission::PairingRequired(pairing_text(message.chat_id))
        }
    }
}

pub fn pairing_text(chat_id: i64) -> String {
    format!(
        "This Telegram chat is not paired with minipaw.\nchat_id={chat_id}\nRun this on the minipaw machine:\nminipaw config telegram pair {chat_id}"
    )
}

pub fn get_updates(
    token: &str,
    offset: Option<i64>,
    timeout_secs: u64,
) -> Result<Vec<TelegramMessage>, String> {
    let mut url = format!("https://api.telegram.org/bot{token}/getUpdates?timeout={timeout_secs}");
    if let Some(offset) = offset {
        url.push_str("&offset=");
        url.push_str(&offset.to_string());
    }
    let output = Command::new("curl")
        .arg("--fail")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg((timeout_secs + 5).to_string())
        .arg(url)
        .output()
        .map_err(|err| format!("curl getUpdates failed: {err}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let body = String::from_utf8_lossy(&output.stdout);
    Ok(parse_updates(&body))
}

pub fn send_message(token: &str, chat_id: i64, text: &str) -> Result<(), String> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let output = Command::new("curl")
        .arg("--fail")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg("20")
        .arg("--data-urlencode")
        .arg(format!("chat_id={chat_id}"))
        .arg("--data-urlencode")
        .arg(format!("text={text}"))
        .arg(url)
        .output()
        .map_err(|err| format!("curl sendMessage failed: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

fn parse_updates(body: &str) -> Vec<TelegramMessage> {
    let mut messages = Vec::new();
    for chunk in body.split("\"update_id\":").skip(1) {
        let Some(update_id) = parse_i64_prefix(chunk) else {
            continue;
        };
        let Some(chat_id_pos) = chunk.find("\"chat\":{\"id\":") else {
            continue;
        };
        let chat_id_start = chat_id_pos + "\"chat\":{\"id\":".len();
        let Some(chat_id) = parse_i64_prefix(&chunk[chat_id_start..]) else {
            continue;
        };
        let Some(text_pos) = chunk.find("\"text\":\"") else {
            continue;
        };
        let text_start = text_pos + "\"text\":\"".len();
        let Some(text) = parse_json_string_tail(&chunk[text_start..]) else {
            continue;
        };
        messages.push(TelegramMessage {
            update_id,
            chat_id,
            text,
        });
    }
    messages
}

fn parse_i64_prefix(text: &str) -> Option<i64> {
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        if index == 0 && ch == '-' {
            end = ch.len_utf8();
            continue;
        }
        if ch.is_ascii_digit() {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    text[..end].parse::<i64>().ok()
}

fn parse_json_string_tail(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                    if hex.len() == 4 {
                        if let Ok(code) = u16::from_str_radix(&hex, 16) {
                            decode_utf16_codeunit(code, &mut chars, &mut out);
                        }
                    }
                }
                other => out.push(other),
            },
            ch => out.push(ch),
        }
    }
}

fn decode_utf16_codeunit(code: u16, chars: &mut impl Iterator<Item = char>, out: &mut String) {
    // High surrogate — consume the following \uXXXX low surrogate.
    if (0xD800..0xDC00).contains(&code) {
        // Expect \u immediately after.
        if chars.next() == Some('\\') && chars.next() == Some('u') {
            let hex2: String = (0..4).filter_map(|_| chars.next()).collect();
            if hex2.len() == 4 {
                if let Ok(low) = u16::from_str_radix(&hex2, 16) {
                    let codepoint =
                        0x10000u32 + ((code as u32 - 0xD800) << 10) + (low as u32 - 0xDC00);
                    if let Some(c) = char::from_u32(codepoint) {
                        out.push(c);
                        return;
                    }
                }
            }
        }
        // Malformed surrogate pair — skip.
        return;
    }
    if let Some(c) = char::from_u32(code as u32) {
        out.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_chat() {
        let channel = TelegramChannel::new(TelegramConfig {
            token: "token".to_owned(),
            allowed_chats: BTreeSet::from([7]),
        });

        assert!(channel
            .accept_message(TelegramMessage {
                update_id: 1,
                chat_id: 8,
                text: "hi".into(),
            })
            .is_err());
    }

    #[test]
    fn unknown_chat_gets_pairing_instructions() {
        let channel = TelegramChannel::new(TelegramConfig {
            token: "token".to_owned(),
            allowed_chats: BTreeSet::from([7]),
        });

        let admission = channel.admit_message(TelegramMessage {
            update_id: 1,
            chat_id: 8,
            text: "hi".into(),
        });

        match admission {
            TelegramAdmission::PairingRequired(text) => {
                assert!(text.contains("chat_id=8"));
                assert!(text.contains("minipaw config telegram pair 8"));
            }
            _ => panic!("expected pairing instructions"),
        }
    }

    #[test]
    fn classifies_knowledge_question() {
        assert_eq!(
            classify_message_kind("what time is it?"),
            MessageKind::Knowledge
        );
        assert_eq!(
            classify_message_kind("how does sqlite work?"),
            MessageKind::Knowledge
        );
        assert_eq!(
            classify_message_kind("read about sqlite"),
            MessageKind::Knowledge
        );
    }

    #[test]
    fn classifies_slash_command_as_action() {
        assert_eq!(classify_message_kind("/ls src"), MessageKind::Action);
        assert_eq!(classify_message_kind("/read README.md"), MessageKind::Action);
        assert_eq!(classify_message_kind("/exec git status"), MessageKind::Action);
        assert_eq!(classify_message_kind("/help"), MessageKind::Knowledge);
    }

    #[test]
    fn classifies_natural_language_action() {
        assert_eq!(classify_message_kind("list src"), MessageKind::Action);
        assert_eq!(classify_message_kind("ls ."), MessageKind::Action);
        assert_eq!(classify_message_kind("read src/main.rs"), MessageKind::Action);
        assert_eq!(classify_message_kind("read README.md"), MessageKind::Action);
        assert_eq!(classify_message_kind("run git status"), MessageKind::Action);
    }

    #[test]
    fn classifies_indirect_action_phrases() {
        assert_eq!(classify_message_kind("can you list src"), MessageKind::Action);
        assert_eq!(
            classify_message_kind("please run git status"),
            MessageKind::Action
        );
        assert_eq!(
            classify_message_kind("could you read src/main.rs"),
            MessageKind::Action
        );
    }

    #[test]
    fn normalizes_polite_prefixes() {
        assert_eq!(normalize_action_text("can you list src"), "list src");
        assert_eq!(
            normalize_action_text("please read src/main.rs"),
            "read src/main.rs"
        );
        assert_eq!(normalize_action_text("could you please run git status"), "run git status");
        assert_eq!(normalize_action_text("list src"), "list src");
    }

    #[test]
    fn parses_updates_response() {
        let body = r#"{"ok":true,"result":[{"update_id":7,"message":{"chat":{"id":42,"type":"private"},"text":"hello\nworld"}}]}"#;
        let updates = parse_updates(body);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 7);
        assert_eq!(updates[0].chat_id, 42);
        assert_eq!(updates[0].text, "hello\nworld");
    }

    #[test]
    fn decodes_unicode_escapes_in_telegram_text() {
        // 告 = 告, 诉 = 诉, 我 = 我, 等 = 等, 于 = 于, 多 = 多, 少 = 少
        let body = r#"{"ok":true,"result":[{"update_id":1,"message":{"chat":{"id":42,"type":"private"},"text":"告诉我8*17等于多少"}}]}"#;
        let updates = parse_updates(body);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].text, "告诉我8*17等于多少");
    }

    #[test]
    fn decodes_surrogate_pair_emoji() {
        // 😀 = U+1F600, encoded as surrogate pair 😀
        let body = r#"{"ok":true,"result":[{"update_id":1,"message":{"chat":{"id":42,"type":"private"},"text":"hi 😀"}}]}"#;
        let updates = parse_updates(body);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].text, "hi 😀");
    }
}
