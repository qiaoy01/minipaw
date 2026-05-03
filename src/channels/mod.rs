pub mod exec_agent;
pub mod telegram;
pub mod telegram_agent;

pub trait AgentHandler {
    fn name(&self) -> &str;
    fn execute(&self, command: &str) -> Result<String, String>;
}
