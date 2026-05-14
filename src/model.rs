#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppModel {
    pub account_label: String,
    pub active_model: String,
    pub active_thread: Option<String>,
    pub cwd: String,
    pub pending_approvals: Vec<ApprovalRequest>,
    pub runtime_status: RuntimeStatus,
    pub sandbox_label: String,
    pub skipped_events: usize,
    pub status_message: String,
    pub threads: Vec<ThreadSummary>,
    pub timeline: Vec<TimelineItem>,
    pub turn_status: String,
    pub(crate) next_timeline_id: usize,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            account_label: "Local account".to_string(),
            active_model: "gpt-5.4".to_string(),
            active_thread: None,
            cwd: "No workspace selected".to_string(),
            pending_approvals: Vec::new(),
            runtime_status: RuntimeStatus::Idle,
            sandbox_label: "workspace-write".to_string(),
            skipped_events: 0,
            status_message: "Fake runtime ready".to_string(),
            threads: Vec::new(),
            timeline: Vec::new(),
            turn_status: "Idle".to_string(),
            next_timeline_id: 0,
        }
    }
}

impl AppModel {
    pub fn next_timeline_id(&mut self, prefix: &str) -> String {
        self.next_timeline_id += 1;
        format!("{prefix}-{}", self.next_timeline_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: String,
    pub status: String,
    pub subtitle: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub body: String,
    pub id: String,
    pub kind: TimelineKind,
    pub meta: String,
    pub status: TimelineStatus,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineKind {
    Assistant,
    Command,
    Diff,
    Plan,
    Reasoning,
    System,
    User,
}

impl TimelineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Assistant => "Assistant",
            Self::Command => "Command",
            Self::Diff => "Diff",
            Self::Plan => "Plan",
            Self::Reasoning => "Reasoning",
            Self::System => "System",
            Self::User => "User",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineStatus {
    Complete,
    Pending,
    Running,
}

impl TimelineStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Pending => "pending",
            Self::Running => "running",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub action: String,
    pub detail: String,
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    Disconnected,
    Idle,
    Running,
    WaitingForApproval,
}

impl RuntimeStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::WaitingForApproval => "Needs approval",
        }
    }
}
