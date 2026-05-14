use std::time::Duration;

use crate::reducer::AgentEvent;

use super::AgentRuntime;

#[derive(Debug, Clone)]
pub struct ScriptedAgentEvent {
    pub delay: Duration,
    pub event: AgentEvent,
}

#[derive(Debug, Default)]
pub struct FakeAgentRuntime {
    next_turn: usize,
}

impl FakeAgentRuntime {
    fn scripted_turn(&mut self, project_path: &str, prompt: String) -> Vec<ScriptedAgentEvent> {
        self.next_turn += 1;
        let turn_id = format!("fake-turn-{}", self.next_turn);
        let assistant_item_id = format!("assistant-{}", self.next_turn);
        let plan_item_id = format!("plan-{}", self.next_turn);
        let command_item_id = format!("command-{}", self.next_turn);
        let diff_item_id = format!("diff-{}", self.next_turn);
        let approval_id = format!("approval-{}", self.next_turn);
        let cwd = project_path.to_string();

        vec![
            event(0, AgentEvent::RuntimeReady { cwd: cwd.clone() }),
            event(
                0,
                AgentEvent::ThreadStarted {
                    id: "fake-thread".to_string(),
                    title: "GPUI desktop spike".to_string(),
                },
            ),
            event(0, AgentEvent::UserMessageSubmitted { prompt }),
            event(
                80,
                AgentEvent::TurnStarted {
                    id: turn_id.clone(),
                },
            ),
            event(
                140,
                AgentEvent::PlanDelta {
                    item_id: plan_item_id.clone(),
                    delta: "1. Create the separate macOS-first GPUI repo.\n".to_string(),
                },
            ),
            event(
                120,
                AgentEvent::PlanDelta {
                    item_id: plan_item_id,
                    delta: "2. Stream fake agent events through an app-owned reducer.\n3. Keep the transcript virtualized for large sessions.".to_string(),
                },
            ),
            event(
                160,
                AgentEvent::ReasoningDelta {
                    item_id: format!("reasoning-{}", self.next_turn),
                    delta: "Keeping this repo separate lets Codex and GPUI clones remain read-only references.".to_string(),
                },
            ),
            event(
                180,
                AgentEvent::AgentMessageDelta {
                    item_id: assistant_item_id.clone(),
                    delta: "This is now a standalone desktop-app repository. ".to_string(),
                },
            ),
            event(
                140,
                AgentEvent::AgentMessageDelta {
                    item_id: assistant_item_id.clone(),
                    delta: "The UI starts with a dense daily-driver shape: threads left, transcript center, context and approvals right. ".to_string(),
                },
            ),
            event(
                140,
                AgentEvent::AgentMessageDelta {
                    item_id: assistant_item_id,
                    delta: format!(
                        "The selected project is `{cwd}`. The next backend should spawn or connect to Codex app-server without modifying the Codex checkout."
                    ),
                },
            ),
            event(
                180,
                AgentEvent::CommandStarted {
                    item_id: command_item_id.clone(),
                    command: "cargo test".to_string(),
                    cwd: cwd.clone(),
                },
            ),
            event(
                120,
                AgentEvent::CommandOutputDelta {
                    item_id: command_item_id.clone(),
                    delta: "running reducer tests\n".to_string(),
                },
            ),
            event(
                100,
                AgentEvent::CommandOutputDelta {
                    item_id: command_item_id.clone(),
                    delta: "test result: ok. state transitions are deterministic\n".to_string(),
                },
            ),
            event(
                80,
                AgentEvent::CommandCompleted {
                    item_id: command_item_id,
                    exit_code: 0,
                },
            ),
            event(
                160,
                AgentEvent::DiffUpdated {
                    item_id: diff_item_id,
                    path: "src/ui/root.rs".to_string(),
                    summary: "+ root window\n+ virtualized transcript\n+ inline tool output".to_string(),
                },
            ),
            event(
                180,
                AgentEvent::ApprovalRequested {
                    id: approval_id.clone(),
                    title: "Run local workspace check".to_string(),
                    detail: "Fake approval showing where command and file-change requests will land.".to_string(),
                    action: "Approve once".to_string(),
                },
            ),
            event(
                600,
                AgentEvent::ApprovalResolved {
                    id: approval_id,
                    approved: true,
                },
            ),
            event(140, AgentEvent::TurnCompleted { id: turn_id }),
        ]
    }
}

impl AgentRuntime for FakeAgentRuntime {
    fn start_turn(&mut self, project_path: &str, prompt: String) -> Vec<ScriptedAgentEvent> {
        self.scripted_turn(project_path, prompt)
    }

    fn interrupt(&mut self) -> AgentEvent {
        AgentEvent::Disconnected {
            message: "Fake stream interrupted by user".to_string(),
        }
    }
}

fn event(delay_ms: u64, event: AgentEvent) -> ScriptedAgentEvent {
    ScriptedAgentEvent {
        delay: Duration::from_millis(delay_ms),
        event,
    }
}
