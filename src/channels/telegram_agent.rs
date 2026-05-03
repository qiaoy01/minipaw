use crate::channels::AgentHandler;
use crate::channels::telegram::send_message;

pub struct TelegramAgent {
    token: String,
}

impl TelegramAgent {
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

impl AgentHandler for TelegramAgent {
    fn name(&self) -> &str {
        "telegram"
    }

    // command format: "<chat_id>:<message>"
    fn execute(&self, command: &str) -> Result<String, String> {
        let (chat_id_str, message) = command
            .split_once(':')
            .ok_or_else(|| "telegram agent: expected <chat_id>:<message>".to_owned())?;
        let chat_id: i64 = chat_id_str
            .trim()
            .parse()
            .map_err(|_| "telegram agent: invalid chat_id".to_owned())?;
        send_message(&self.token, chat_id, message.trim()).map(|()| "sent".to_owned())
    }
}
