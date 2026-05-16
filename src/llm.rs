use std::time::Duration;

use crate::config::LlmConfig;
use crate::http;

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".to_owned(), content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".to_owned(), content: content.into() }
    }
}

pub trait LlmClient {
    /// Multi-turn chat with an explicit system prompt and message history.
    fn chat(&mut self, system: &str, messages: &[ChatMessage]) -> String;

    /// Single-turn convenience: wraps `chat` with one user message.
    fn next_step(&mut self, prompt: &str) -> String {
        self.chat(
            "You are minipaw, a small-footprint AI agent.",
            &[ChatMessage::user(prompt)],
        )
    }
}

#[derive(Debug, Default)]
pub struct OfflineLlm;

impl LlmClient for OfflineLlm {
    fn chat(&mut self, _system: &str, messages: &[ChatMessage]) -> String {
        let last = messages.last().map(|m| m.content.as_str()).unwrap_or("");
        format!(
            "offline planner noted: {last}\nNo LLM provider is configured in this minimal build."
        )
    }
}

#[derive(Debug, Clone)]
pub struct LlamaCppClient {
    endpoint: HttpEndpoint,
    model: String,
    api_key: Option<String>,
    thinking: bool,
    timeout: Duration,
}

impl LlamaCppClient {
    pub fn from_config(config: &LlmConfig) -> Result<Self, String> {
        match config.provider.as_str() {
            "llamacpp" | "deepseek" | "openai" => {}
            other => return Err(format!("unsupported provider: {other}")),
        }
        let mut endpoint = HttpEndpoint::parse(&config.url)?;
        if endpoint.base_path.is_empty() {
            endpoint.base_path = "/v1".to_owned();
        }
        Ok(Self {
            endpoint,
            model: config.model.clone(),
            api_key: config
                .api_key
                .clone()
                .filter(|key| !key.trim().is_empty()),
            thinking: config.thinking,
            timeout: Duration::from_secs(300),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn complete_chat(&self, system: &str, messages: &[ChatMessage]) -> Result<String, String> {
        let (temperature, max_tokens, thinking_flag) = if self.thinking {
            ("1.0", 16384, "true")
        } else {
            ("0.2", 32768, "false")
        };

        // Build the messages JSON array.
        let mut msg_array = String::from("[");
        let mut first = true;
        if !system.is_empty() {
            msg_array.push_str(&format!(
                "{{\"role\":\"system\",\"content\":\"{}\"}}",
                json_escape(system)
            ));
            first = false;
        }
        for msg in messages {
            if !first {
                msg_array.push(',');
            }
            msg_array.push_str(&format!(
                "{{\"role\":\"{}\",\"content\":\"{}\"}}",
                json_escape(&msg.role),
                json_escape(&msg.content)
            ));
            first = false;
        }
        msg_array.push(']');

        let body = format!(
            "{{\"model\":\"{}\",\"messages\":{msg_array},\"max_tokens\":{max_tokens},\"temperature\":{temperature},\"chat_template_kwargs\":{{\"enable_thinking\":{thinking_flag}}}}}",
            json_escape(&self.model)
        );

        let last_content = messages.last().map(|m| m.content.as_str()).unwrap_or("");
        let preview = last_content.replace('\n', "↵");
        println!(
            "llm >> ({} msgs, {} chars) {}",
            messages.len() + usize::from(!system.is_empty()),
            last_content.len(),
            char_boundary_truncate(&preview, 300)
        );

        let response = self.post("/chat/completions", &body)?;
        let resp_preview = response.replace('\n', "↵");
        println!(
            "llm << raw ({} chars) {}",
            response.len(),
            char_boundary_truncate(&resp_preview, 500)
        );

        let result = extract_chat_content(&response)
            .map(|text| strip_think_blocks(text.trim()))
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| "llm response did not contain completion text".to_owned());

        if let Ok(ref text) = result {
            let content_preview = text.replace('\n', "↵");
            println!("llm << content: {content_preview}");
        }
        result
    }

    fn post(&self, path: &str, body: &str) -> Result<String, String> {
        let full_path = join_path(&self.endpoint.base_path, path);
        let auth_header = self
            .api_key
            .as_ref()
            .map(|key| format!("Bearer {key}"));
        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        if let Some(value) = auth_header.as_deref() {
            headers.push(("Authorization", value));
        }
        http::request(http::Request {
            method: "POST",
            scheme: &self.endpoint.scheme,
            host: &self.endpoint.host,
            port: self.endpoint.port,
            path: &full_path,
            headers: &headers,
            body: body.as_bytes(),
            timeout: self.timeout,
        })
    }
}

impl LlmClient for LlamaCppClient {
    fn chat(&mut self, system: &str, messages: &[ChatMessage]) -> String {
        self.complete_chat(system, messages).unwrap_or_else(|err| {
            eprintln!("llm error: {err}");
            format!("llm request failed: {err}")
        })
    }
}

#[derive(Debug, Clone)]
struct HttpEndpoint {
    scheme: String,
    host: String,
    port: u16,
    base_path: String,
}

impl HttpEndpoint {
    fn parse(url: &str) -> Result<Self, String> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| "llm url must include scheme".to_owned())?;
        let (authority, base_path) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = authority
            .rsplit_once(':')
            .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
            .unwrap_or((authority, if scheme == "https" { 443 } else { 80 }));

        if host.is_empty() {
            return Err("llm url host is empty".to_owned());
        }

        Ok(Self {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
            base_path: if base_path.is_empty() {
                String::new()
            } else {
                format!("/{base_path}")
            },
        })
    }
}

fn join_path(base: &str, path: &str) -> String {
    if base.is_empty() {
        path.to_owned()
    } else {
        format!("{}{}", base.trim_end_matches('/'), path)
    }
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
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

fn strip_think_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find("<think>") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_start = &rest[start + "<think>".len()..];
        let Some(end) = after_start.find("</think>") else {
            break;
        };
        rest = &after_start[end + "</think>".len()..];
    }
    out.trim().to_owned()
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

fn extract_chat_content(text: &str) -> Option<String> {
    if let Some(message_pos) = text.find("\"message\"") {
        if let Some(content) = extract_json_string(&text[message_pos..], "content") {
            if !content.trim().is_empty() {
                return Some(content);
            }
        }
    }
    extract_json_string(text, "content").filter(|content| !content.trim().is_empty())
}

fn char_boundary_truncate(s: &str, max_bytes: usize) -> &str {
    let end = s.len().min(max_bytes);
    let end = (0..=end).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
    &s[..end]
}

#[cfg(test)]
fn dechunk(body: &str) -> Result<String, String> {
    let mut rest = body;
    let mut out = String::new();
    loop {
        let Some((raw_size, after_size)) = rest.split_once("\r\n") else {
            return Err("invalid chunked response".to_owned());
        };
        let size_text = raw_size.split(';').next().unwrap_or(raw_size).trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|err| format!("invalid chunk size: {err}"))?;
        if size == 0 {
            return Ok(out);
        }
        if after_size.len() < size + 2 {
            return Err("truncated chunked response".to_owned());
        }
        out.push_str(&after_size[..size]);
        rest = &after_size[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint() {
        let endpoint = HttpEndpoint::parse("http://192.0.2.1:14416/v1").unwrap();
        assert_eq!(endpoint.host, "192.0.2.1");
        assert_eq!(endpoint.port, 14416);
        assert_eq!(
            join_path(&endpoint.base_path, "/completions"),
            "/v1/completions"
        );
    }

    #[test]
    fn extracts_completion_text() {
        let text = r#"{"choices":[{"text":" minipaw-ok\n"}]}"#;
        assert_eq!(extract_json_string(text, "text").unwrap(), " minipaw-ok\n");
    }

    #[test]
    fn strips_think_blocks() {
        assert_eq!(strip_think_blocks("<think>hidden</think>\nvisible"), "visible");
    }

    #[test]
    fn extracts_chat_content() {
        let text = r#"{"choices":[{"message":{"role":"assistant","content":"clean-ok","reasoning_content":"hidden"}}]}"#;
        assert_eq!(extract_chat_content(text).unwrap(), "clean-ok");
    }

    #[test]
    fn decodes_chunked_body() {
        assert_eq!(
            dechunk("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").unwrap(),
            "hello world"
        );
    }

    #[test]
    fn offline_llm_uses_last_message_content() {
        let mut llm = OfflineLlm;
        let msgs = vec![ChatMessage::user("hello")];
        let resp = llm.chat("sys", &msgs);
        assert!(resp.contains("hello"));
    }

    #[test]
    #[ignore]
    fn deepseek_smoke_via_rustls() {
        let api_key = std::env::var("MINIPAW_DEEPSEEK_KEY")
            .expect("set MINIPAW_DEEPSEEK_KEY to run this network probe");
        let cfg = LlmConfig {
            provider: "deepseek".into(),
            url: "https://api.deepseek.com".into(),
            model: "deepseek-v4-flash".into(),
            thinking: false,
            api_key: Some(api_key),
        };
        let mut client = LlamaCppClient::from_config(&cfg).expect("from_config");
        let reply = client.chat("Reply with the single word PONG.", &[ChatMessage::user("ping")]);
        eprintln!("deepseek reply: {reply}");
        assert!(!reply.trim().is_empty(), "empty reply");
        assert!(!reply.starts_with("llm request failed"), "request failed: {reply}");
    }
}
