use crate::model::{
    AppModel, ApprovalRequest, ProjectSummary, RuntimeStatus, ThreadSummary, TimelineItem,
    TimelineKind, TimelineStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    RuntimeReady {
        cwd: String,
    },
    ThreadStarted {
        id: String,
        title: String,
    },
    UserMessageSubmitted {
        prompt: String,
    },
    TurnStarted {
        id: String,
    },
    AgentMessageDelta {
        item_id: String,
        delta: String,
    },
    PlanDelta {
        item_id: String,
        delta: String,
    },
    ReasoningDelta {
        item_id: String,
        delta: String,
    },
    CommandStarted {
        item_id: String,
        command: String,
        cwd: String,
    },
    CommandOutputDelta {
        item_id: String,
        delta: String,
    },
    CommandCompleted {
        item_id: String,
        exit_code: i32,
    },
    DiffUpdated {
        item_id: String,
        path: String,
        summary: String,
    },
    ApprovalRequested {
        id: String,
        title: String,
        detail: String,
        action: String,
    },
    ApprovalResolved {
        id: String,
        approved: bool,
    },
    TurnCompleted {
        id: String,
    },
    Lagged {
        skipped: usize,
    },
    Disconnected {
        message: String,
    },
}

pub fn reduce(model: &mut AppModel, event: AgentEvent) {
    match event {
        AgentEvent::RuntimeReady { cwd } => {
            model.cwd = cwd;
            upsert_project(model);
            model.runtime_status = RuntimeStatus::Idle;
            model.status_message = "Runtime ready".to_string();
        }
        AgentEvent::ThreadStarted { id, title } => {
            upsert_project(model);
            upsert_thread(model, id.clone(), title);
            model.active_thread = Some(id);
        }
        AgentEvent::UserMessageSubmitted { prompt } => {
            let id = model.next_timeline_id("user");
            push_item(
                model,
                id,
                TimelineKind::User,
                "Prompt".to_string(),
                prompt,
                "submitted".to_string(),
                TimelineStatus::Complete,
            );
        }
        AgentEvent::TurnStarted { id } => {
            model.runtime_status = RuntimeStatus::Running;
            model.status_message = "Streaming fake agent events".to_string();
            model.turn_status = format!("Turn {id} running");
        }
        AgentEvent::AgentMessageDelta { item_id, delta } => {
            append_to_item(
                model,
                item_id,
                TimelineKind::Assistant,
                "Assistant response".to_string(),
                "streaming".to_string(),
                delta,
            );
        }
        AgentEvent::PlanDelta { item_id, delta } => {
            append_to_item(
                model,
                item_id,
                TimelineKind::Plan,
                "Plan".to_string(),
                "updated".to_string(),
                delta,
            );
        }
        AgentEvent::ReasoningDelta { item_id, delta } => {
            append_to_item(
                model,
                item_id,
                TimelineKind::Reasoning,
                "Reasoning summary".to_string(),
                "streaming".to_string(),
                delta,
            );
        }
        AgentEvent::CommandStarted {
            item_id,
            command,
            cwd,
        } => {
            push_item(
                model,
                item_id,
                TimelineKind::Command,
                command,
                String::new(),
                cwd,
                TimelineStatus::Running,
            );
        }
        AgentEvent::CommandOutputDelta { item_id, delta } => {
            append_to_item(
                model,
                item_id,
                TimelineKind::Command,
                "Command".to_string(),
                "streaming".to_string(),
                delta,
            );
        }
        AgentEvent::CommandCompleted { item_id, exit_code } => {
            if let Some(item) = model.timeline.iter_mut().find(|item| item.id == item_id) {
                item.meta = format!("exit code {exit_code}");
                item.status = TimelineStatus::Complete;
            }
        }
        AgentEvent::DiffUpdated {
            item_id,
            path,
            summary,
        } => {
            push_item(
                model,
                item_id,
                TimelineKind::Diff,
                path,
                summary,
                "working tree".to_string(),
                TimelineStatus::Complete,
            );
        }
        AgentEvent::ApprovalRequested {
            id,
            title,
            detail,
            action,
        } => {
            model.runtime_status = RuntimeStatus::WaitingForApproval;
            model.status_message = "Approval pending".to_string();
            model.pending_approvals.push(ApprovalRequest {
                action,
                detail,
                id: id.clone(),
                title: title.clone(),
            });
            push_item(
                model,
                id,
                TimelineKind::System,
                "Approval requested".to_string(),
                title,
                "pending".to_string(),
                TimelineStatus::Pending,
            );
        }
        AgentEvent::ApprovalResolved { id, approved } => {
            model.pending_approvals.retain(|approval| approval.id != id);
            if let Some(item) = model.timeline.iter_mut().find(|item| item.id == id) {
                item.meta = if approved { "approved" } else { "denied" }.to_string();
                item.status = TimelineStatus::Complete;
            }
            model.runtime_status = RuntimeStatus::Running;
            model.status_message = "Approval resolved".to_string();
        }
        AgentEvent::TurnCompleted { id } => {
            model.runtime_status = RuntimeStatus::Idle;
            model.status_message = "Fake turn complete".to_string();
            model.turn_status = format!("Turn {id} complete");
            if let Some(active_thread) = &model.active_thread {
                if let Some(thread) = model
                    .threads
                    .iter_mut()
                    .find(|thread| thread.id == *active_thread)
                {
                    thread.status = "Idle".to_string();
                }
            }
        }
        AgentEvent::Lagged { skipped } => {
            model.skipped_events = model.skipped_events.saturating_add(skipped);
            model.status_message = format!("Skipped {skipped} best-effort events");
        }
        AgentEvent::Disconnected { message } => {
            model.runtime_status = RuntimeStatus::Disconnected;
            model.status_message = message;
        }
    }
}

fn upsert_thread(model: &mut AppModel, id: String, title: String) {
    if let Some(thread) = model.threads.iter_mut().find(|thread| thread.id == id) {
        thread.status = model.runtime_status.label().to_string();
        thread.title = title;
        return;
    }

    model.threads.insert(
        0,
        ThreadSummary {
            id,
            status: model.runtime_status.label().to_string(),
            subtitle: model.cwd.clone(),
            title,
        },
    );
}

pub fn upsert_project(model: &mut AppModel) {
    let path = model.cwd.clone();
    if path == "No workspace selected" {
        return;
    }

    let title = path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path.as_str())
        .to_string();
    let id = element_safe_id(&path);
    model.active_project = Some(id.clone());

    if let Some(project) = model.projects.iter_mut().find(|project| project.id == id) {
        project.path = path;
        project.title = title;
        return;
    }

    model.projects.insert(0, ProjectSummary { id, path, title });
}

fn append_to_item(
    model: &mut AppModel,
    item_id: String,
    kind: TimelineKind,
    title: String,
    meta: String,
    delta: String,
) {
    if let Some(item) = model.timeline.iter_mut().find(|item| item.id == item_id) {
        item.body.push_str(&delta);
        item.status = TimelineStatus::Running;
        return;
    }

    push_item(
        model,
        item_id,
        kind,
        title,
        delta,
        meta,
        TimelineStatus::Running,
    );
}

fn element_safe_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn push_item(
    model: &mut AppModel,
    id: String,
    kind: TimelineKind,
    title: String,
    body: String,
    meta: String,
    status: TimelineStatus,
) {
    model.timeline.push(TimelineItem {
        body,
        id,
        kind,
        meta,
        status,
        title,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_deltas_append_to_existing_item() {
        let mut model = AppModel::default();

        reduce(
            &mut model,
            AgentEvent::AgentMessageDelta {
                item_id: "assistant-1".to_string(),
                delta: "hello".to_string(),
            },
        );
        reduce(
            &mut model,
            AgentEvent::AgentMessageDelta {
                item_id: "assistant-1".to_string(),
                delta: " world".to_string(),
            },
        );

        assert_eq!(model.timeline.len(), 1);
        assert_eq!(model.timeline[0].body, "hello world");
        assert_eq!(model.timeline[0].status, TimelineStatus::Running);
    }

    #[test]
    fn approval_request_is_first_class_state() {
        let mut model = AppModel::default();

        reduce(
            &mut model,
            AgentEvent::ApprovalRequested {
                id: "approval-1".to_string(),
                title: "Run cargo check".to_string(),
                detail: "Needs workspace write access".to_string(),
                action: "Approve once".to_string(),
            },
        );

        assert_eq!(model.runtime_status, RuntimeStatus::WaitingForApproval);
        assert_eq!(model.pending_approvals.len(), 1);
        assert_eq!(model.timeline[0].status, TimelineStatus::Pending);

        reduce(
            &mut model,
            AgentEvent::ApprovalResolved {
                id: "approval-1".to_string(),
                approved: true,
            },
        );

        assert!(model.pending_approvals.is_empty());
        assert_eq!(model.timeline[0].meta, "approved");
    }
}
