pub mod app_server;
pub mod fake;

use crate::reducer::AgentEvent;

use self::fake::ScriptedAgentEvent;

pub trait AgentRuntime {
    fn start_turn(&mut self, project_path: &str, prompt: String) -> Vec<ScriptedAgentEvent>;
    fn interrupt(&mut self) -> AgentEvent;
}
