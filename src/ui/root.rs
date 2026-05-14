use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use crate::model::{AppModel, RuntimeStatus, ThreadSummary, TimelineItem, TimelineKind};
use crate::reducer::{reduce, seed_long_transcript};
use crate::runtime::AgentRuntime;
use crate::runtime::app_server::{
    AppServerProcess, AppServerWireEvent, JsonRpcRequestBuilder, approval_event_id,
    approval_response_result, notification_to_agent_event, server_request_to_agent_event,
};
use crate::runtime::fake::FakeAgentRuntime;
use crate::ui::text_input::TextInput;
use gpui::{
    Context, Div, ElementId, Entity, Hsla, IntoElement, PathPromptOptions, Render, ScrollStrategy,
    Stateful, UniformListScrollHandle, Window, div, prelude::*, px, rgb, uniform_list,
};

const ROW_HEIGHT: f32 = 92.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingRequest {
    Initialize,
    ThreadStart,
    TurnInterrupt,
    TurnStart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingServerApproval {
    method: String,
    params: serde_json::Value,
    request_id: serde_json::Value,
}

pub struct RootView {
    app_server: Option<AppServerProcess>,
    composer_input: Entity<TextInput>,
    live_thread_id: Option<String>,
    live_turn_id: Option<String>,
    model: AppModel,
    pending_requests: HashMap<u64, PendingRequest>,
    pending_server_approvals: HashMap<String, PendingServerApproval>,
    project_input: Entity<TextInput>,
    request_builder: JsonRpcRequestBuilder,
    runtime: FakeAgentRuntime,
    scroll_handle: UniformListScrollHandle,
    stream_token: usize,
}

impl RootView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut model = AppModel::default();
        if let Ok(cwd) = std::env::current_dir() {
            model.cwd = cwd.display().to_string();
        }
        model.status_message = "Phase 0 fake runtime ready".to_string();
        let project_path = model.cwd.clone();

        Self {
            app_server: None,
            composer_input: cx.new(|cx| TextInput::new("Message Codex for this project...", cx)),
            live_thread_id: None,
            live_turn_id: None,
            model,
            pending_requests: HashMap::new(),
            pending_server_approvals: HashMap::new(),
            project_input: cx.new(|cx| {
                let mut input = TextInput::new("Project folder path", cx);
                input.set_content(project_path, cx);
                input
            }),
            request_builder: JsonRpcRequestBuilder::default(),
            runtime: FakeAgentRuntime::default(),
            scroll_handle: UniformListScrollHandle::new(),
            stream_token: 0,
        }
    }

    fn start_fake_turn(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.model.runtime_status,
            RuntimeStatus::Running | RuntimeStatus::WaitingForApproval
        ) {
            return;
        }

        let prompt = self
            .composer_input
            .update(cx, |input, cx| input.take_content(cx));
        if prompt.trim().is_empty() {
            self.model.status_message = "Type a message before sending".to_string();
            cx.notify();
            return;
        }

        if self.app_server.is_some() {
            self.start_live_turn(prompt, cx);
            return;
        }

        self.stream_fake_turn(prompt, cx);
    }

    fn stream_fake_turn(&mut self, prompt: String, cx: &mut Context<Self>) {
        self.stream_token += 1;
        let token = self.stream_token;
        let script = self.runtime.start_turn(&self.model.cwd, prompt);

        cx.spawn(async move |this, cx| {
            for step in script {
                cx.background_spawn(async move {
                    std::thread::sleep(step.delay);
                })
                .await;

                let event = step.event;
                this.update(cx, move |this, cx| {
                    if this.stream_token == token {
                        reduce(&mut this.model, event);
                        this.scroll_to_bottom();
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn interrupt_fake_turn(&mut self, cx: &mut Context<Self>) {
        self.stream_token += 1;
        if self.app_server.is_some() {
            self.interrupt_live_turn();
            cx.notify();
            return;
        }

        self.model.pending_approvals.clear();
        self.model.runtime_status = RuntimeStatus::Idle;
        self.model.status_message = match self.runtime.interrupt() {
            crate::reducer::AgentEvent::Disconnected { message } => message,
            _ => "Fake stream interrupted".to_string(),
        };
        self.model.turn_status = "Interrupted".to_string();
        cx.notify();
    }

    fn connect_codex(&mut self, cx: &mut Context<Self>) {
        if self.app_server.is_some() {
            self.model.status_message = "Codex app-server is already connected".to_string();
            cx.notify();
            return;
        }

        match AppServerProcess::spawn_default() {
            Ok(process) => {
                self.app_server = Some(process);
                self.pending_requests.clear();
                self.pending_server_approvals.clear();
                self.live_thread_id = None;
                self.live_turn_id = None;
                self.model.status_message = "Starting Codex app-server".to_string();

                let initialize = self.request_builder.initialize();
                self.send_app_server_request(initialize, PendingRequest::Initialize);
                self.start_app_server_poll(cx);
            }
            Err(error) => {
                self.model.status_message = format!("Could not start `codex app-server`: {error}");
            }
        }
        cx.notify();
    }

    fn start_app_server_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_spawn(async {
                    std::thread::sleep(Duration::from_millis(50));
                })
                .await;

                let keep_polling = this
                    .update(cx, |this, cx| {
                        this.drain_app_server_events(cx);
                        this.app_server.is_some()
                    })
                    .unwrap_or(false);

                if !keep_polling {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_live_turn(&mut self, prompt: String, cx: &mut Context<Self>) {
        let Some(thread_id) = self.live_thread_id.clone() else {
            self.model.status_message =
                "Codex is still starting; try again in a moment".to_string();
            cx.notify();
            return;
        };

        let request = self
            .request_builder
            .turn_start(&thread_id, &self.model.cwd, &prompt);
        self.send_app_server_request(request, PendingRequest::TurnStart);
        self.model.status_message = "Sent prompt to Codex app-server".to_string();
        self.model.runtime_status = RuntimeStatus::Running;
        cx.notify();
    }

    fn interrupt_live_turn(&mut self) {
        let (Some(thread_id), Some(turn_id)) =
            (self.live_thread_id.clone(), self.live_turn_id.clone())
        else {
            self.model.status_message = "No live Codex turn to interrupt".to_string();
            return;
        };

        let request = self.request_builder.turn_interrupt(&thread_id, &turn_id);
        self.send_app_server_request(request, PendingRequest::TurnInterrupt);
        self.model.status_message = "Interrupt requested".to_string();
    }

    fn send_app_server_request(&mut self, request: serde_json::Value, pending: PendingRequest) {
        let Some(request_id) = request.get("id").and_then(|id| id.as_u64()) else {
            self.model.status_message = "Internal error: app-server request missing id".to_string();
            return;
        };

        let Some(app_server) = self.app_server.as_ref() else {
            self.model.status_message = "Codex app-server is not connected".to_string();
            return;
        };

        match app_server.send(request) {
            Ok(()) => {
                self.pending_requests.insert(request_id, pending);
            }
            Err(error) => {
                self.model.status_message = format!("Could not send app-server request: {error}");
            }
        }
    }

    fn send_app_server_notification(&mut self, notification: serde_json::Value) {
        let Some(app_server) = self.app_server.as_ref() else {
            self.model.status_message = "Codex app-server is not connected".to_string();
            return;
        };

        if let Err(error) = app_server.send(notification) {
            self.model.status_message = format!("Could not send app-server notification: {error}");
        }
    }

    fn drain_app_server_events(&mut self, cx: &mut Context<Self>) {
        loop {
            let event = self
                .app_server
                .as_ref()
                .and_then(AppServerProcess::try_recv);
            let Some(event) = event else {
                break;
            };
            self.handle_app_server_event(event, cx);
        }
    }

    fn handle_app_server_event(&mut self, event: AppServerWireEvent, cx: &mut Context<Self>) {
        match event {
            AppServerWireEvent::Response(response) => self.handle_app_server_response(response),
            AppServerWireEvent::ServerRequest(request) => self.handle_app_server_request(request),
            AppServerWireEvent::Notification(notification) => {
                if let Some(agent_event) = notification_to_agent_event(&notification) {
                    if let crate::reducer::AgentEvent::TurnStarted { id } = &agent_event {
                        self.live_turn_id = Some(id.clone());
                    }
                    if matches!(
                        agent_event,
                        crate::reducer::AgentEvent::TurnCompleted { .. }
                    ) {
                        self.live_turn_id = None;
                    }
                    reduce(&mut self.model, agent_event);
                    self.scroll_to_bottom();
                }
            }
            AppServerWireEvent::MalformedLine(line) => {
                self.model.status_message = format!("Malformed app-server line: {line}");
            }
            AppServerWireEvent::IoError(error) => {
                self.model.status_message = format!("App-server IO error: {error}");
                self.app_server = None;
                self.pending_server_approvals.clear();
            }
        }
        cx.notify();
    }

    fn handle_app_server_request(&mut self, request: serde_json::Value) {
        let method = request
            .get("method")
            .and_then(|method| method.as_str())
            .unwrap_or("unknown")
            .to_string();

        if let Some(agent_event) = server_request_to_agent_event(&request) {
            let Some(approval_id) = approval_event_id(&request) else {
                self.send_app_server_error_response(
                    request
                        .get("id")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    -32600,
                    "Approval request was missing a usable id",
                );
                return;
            };
            let request_id = request
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let params = request
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            self.pending_server_approvals.insert(
                approval_id,
                PendingServerApproval {
                    method,
                    params,
                    request_id,
                },
            );
            reduce(&mut self.model, agent_event);
            self.scroll_to_bottom();
            return;
        }

        self.send_app_server_error_response(
            request
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            -32601,
            &format!("Unsupported app-server request: {method}"),
        );
        self.model.status_message = format!("Unsupported app-server request: {method}");
    }

    fn handle_app_server_response(&mut self, response: serde_json::Value) {
        let request_id = response.get("id").and_then(|id| id.as_u64());
        let pending = request_id.and_then(|id| self.pending_requests.remove(&id));

        if let Some(error) = response.get("error") {
            self.model.status_message = format!("App-server error: {error}");
            return;
        }

        let result = response.get("result").unwrap_or(&serde_json::Value::Null);
        match pending {
            Some(PendingRequest::Initialize) => {
                self.model.status_message = "Codex app-server initialized".to_string();
                self.send_app_server_notification(self.request_builder.initialized_notification());
                let request = self.request_builder.thread_start(&self.model.cwd);
                self.send_app_server_request(request, PendingRequest::ThreadStart);
            }
            Some(PendingRequest::ThreadStart) => {
                if let Some(thread) = result.get("thread") {
                    let thread_id = string_at(thread, "id").unwrap_or_else(|| "thread".to_string());
                    self.live_thread_id = Some(thread_id.clone());
                    reduce(
                        &mut self.model,
                        crate::reducer::AgentEvent::ThreadStarted {
                            id: thread_id,
                            title: string_at(thread, "preview")
                                .filter(|preview| !preview.is_empty())
                                .unwrap_or_else(|| "Codex thread".to_string()),
                        },
                    );
                }
                self.model.status_message = "Codex app-server ready".to_string();
            }
            Some(PendingRequest::TurnStart) => {
                if let Some(turn) = result.get("turn") {
                    self.live_turn_id = string_at(turn, "id");
                }
                self.model.status_message = "Codex turn started".to_string();
            }
            Some(PendingRequest::TurnInterrupt) => {
                self.model.status_message = "Codex interrupt accepted".to_string();
            }
            None => {
                self.model.status_message = "Received app-server response".to_string();
            }
        }
    }

    fn resolve_approval(&mut self, approval_id: String, approved: bool, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending_server_approvals.remove(&approval_id) {
            let Some(result) = approval_response_result(&pending.method, &pending.params, approved)
            else {
                self.model.status_message =
                    format!("Cannot resolve unsupported approval: {}", pending.method);
                cx.notify();
                return;
            };

            let response = serde_json::json!({
                "id": pending.request_id.clone(),
                "result": result
            });

            if self.send_app_server_response(response) {
                reduce(
                    &mut self.model,
                    crate::reducer::AgentEvent::ApprovalResolved {
                        id: approval_id,
                        approved,
                    },
                );
                self.model.status_message = if approved {
                    "Approval sent to Codex".to_string()
                } else {
                    "Denial sent to Codex".to_string()
                };
            } else {
                self.pending_server_approvals.insert(approval_id, pending);
            }
            cx.notify();
            return;
        }

        reduce(
            &mut self.model,
            crate::reducer::AgentEvent::ApprovalResolved {
                id: approval_id,
                approved,
            },
        );
        cx.notify();
    }

    fn send_app_server_response(&mut self, response: serde_json::Value) -> bool {
        let Some(app_server) = self.app_server.as_ref() else {
            self.model.status_message = "Codex app-server is not connected".to_string();
            return false;
        };

        match app_server.send(response) {
            Ok(()) => true,
            Err(error) => {
                self.model.status_message = format!("Could not send app-server response: {error}");
                false
            }
        }
    }

    fn send_app_server_error_response(
        &mut self,
        request_id: serde_json::Value,
        code: i64,
        message: &str,
    ) {
        let response = serde_json::json!({
            "id": request_id,
            "error": {
                "code": code,
                "message": message
            }
        });
        let _ = self.send_app_server_response(response);
    }

    fn seed_long_transcript(&mut self, cx: &mut Context<Self>) {
        self.stream_token += 1;
        seed_long_transcript(&mut self.model, 10_000);
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn choose_project(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            directories: true,
            files: false,
            multiple: false,
            prompt: Some("Choose Codex project".into()),
        });

        cx.spawn(async move |this, cx| {
            let selected = receiver.await.ok().and_then(Result::ok).flatten();
            if let Some(path) = selected.and_then(|paths| paths.into_iter().next()) {
                this.update(cx, |this, cx| {
                    this.set_project_path(path, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn set_project_from_input(&mut self, cx: &mut Context<Self>) {
        let path = self.project_input.read(cx).content();
        self.set_project_path(PathBuf::from(path), cx);
    }

    fn use_current_dir(&mut self, cx: &mut Context<Self>) {
        match std::env::current_dir() {
            Ok(path) => self.set_project_path(path, cx),
            Err(error) => {
                self.model.status_message = format!("Could not read current directory: {error}");
                cx.notify();
            }
        }
    }

    fn set_project_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match path.canonicalize() {
            Ok(path) if path.is_dir() => {
                let path = path.display().to_string();
                self.model.cwd = path.clone();
                self.model.status_message = "Project selected".to_string();
                self.project_input.update(cx, |input, cx| {
                    input.set_content(path, cx);
                });
            }
            Ok(path) => {
                self.model.status_message = format!("Not a directory: {}", path.display());
            }
            Err(error) => {
                self.model.status_message = format!("Project path is invalid: {error}");
            }
        }
        cx.notify();
    }

    fn scroll_to_bottom(&self) {
        if !self.model.timeline.is_empty() {
            self.scroll_handle.scroll_to_item(
                self.model.timeline.len().saturating_sub(1),
                ScrollStrategy::Bottom,
            );
        }
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::default();
        let is_running = matches!(
            self.model.runtime_status,
            RuntimeStatus::Running | RuntimeStatus::WaitingForApproval
        );

        div()
            .id("codex-gpui-root")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.text)
            .child(self.top_bar(&theme))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.sidebar(&theme, cx))
                    .child(self.main_pane(&theme, cx, is_running))
                    .child(self.context_panel(&theme, cx)),
            )
            .child(self.status_bar(&theme))
    }
}

impl RootView {
    fn top_bar(&self, theme: &Theme) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(48.0))
            .px_4()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Codex GPUI Desktop"),
                    )
                    .child(status_pill(
                        "macOS first",
                        theme.accent,
                        theme.accent_soft,
                        theme.background,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_pill(
                        self.model.runtime_status.label(),
                        status_color(self.model.runtime_status),
                        theme.surface_alt,
                        theme.text,
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted)
                            .child(self.model.account_label.clone()),
                    ),
            )
    }

    fn sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let threads = if self.model.threads.is_empty() {
            vec![empty_thread_row(theme)]
        } else {
            self.model
                .threads
                .iter()
                .map(|thread| thread_row(thread, self.model.active_thread.as_deref(), theme))
                .collect()
        };

        div()
            .w(px(248.0))
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(section_header("Workspaces", theme))
            .child(
                div().px_3().pb_3().child(
                    div()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_alt)
                        .p_3()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("codex-gpui-desktop"),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_xs()
                                .line_clamp(2)
                                .text_color(theme.muted)
                                .child(self.model.cwd.clone()),
                        )
                        .child(div().mt_3().child(self.project_input.clone()))
                        .child(
                            div()
                                .mt_2()
                                .flex()
                                .gap_2()
                                .child(
                                    secondary_button("choose-project", "Choose", theme).on_click(
                                        cx.listener(|this, _, _, cx| this.choose_project(cx)),
                                    ),
                                )
                                .child(secondary_button("set-project", "Set", theme).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.set_project_from_input(cx);
                                    }),
                                )),
                        )
                        .child(
                            div().mt_2().child(
                                secondary_button("use-current-dir", "Use cwd", theme).on_click(
                                    cx.listener(|this, _, _, cx| this.use_current_dir(cx)),
                                ),
                            ),
                        ),
                ),
            )
            .child(section_header("Threads", theme))
            .child(
                div()
                    .id("thread-list-scroll")
                    .flex_1()
                    .overflow_scroll()
                    .px_2()
                    .children(threads),
            )
    }

    fn main_pane(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
        is_running: bool,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .bg(theme.background)
            .child(self.transcript_header(theme, cx, is_running))
            .child(self.transcript(theme, cx))
            .child(self.composer(theme, cx, is_running))
    }

    fn transcript_header(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
        is_running: bool,
    ) -> impl IntoElement {
        let active_thread = self
            .model
            .active_thread
            .clone()
            .unwrap_or_else(|| "No active thread".to_string());

        div()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .h(px(54.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(active_thread),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted)
                            .child(format!("{} timeline items", self.model.timeline.len())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("start-fake-turn", "Stream fake turn", is_running, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.start_fake_turn(cx))),
                    )
                    .child(
                        secondary_button("seed-long-transcript", "Seed 10k", theme).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.seed_long_transcript(cx);
                            }),
                        ),
                    )
                    .child(
                        secondary_button("interrupt-turn", "Interrupt", theme)
                            .when(!is_running, |button| button.opacity(0.45))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.interrupt_fake_turn(cx);
                            })),
                    ),
            )
    }

    fn transcript(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.model.timeline.is_empty() {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(theme.muted)
                .child("Start a fake turn to stream the first agent timeline.")
                .into_any_element();
        }

        div()
            .flex_1()
            .overflow_hidden()
            .child(
                uniform_list(
                    "codex-gpui-transcript",
                    self.model.timeline.len(),
                    cx.processor(|this, range: Range<usize>, _window, _cx| {
                        range
                            .map(|ix| timeline_row(ix, &this.model.timeline[ix], &Theme::default()))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll_handle)
                .h_full(),
            )
            .into_any_element()
    }

    fn composer(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
        is_running: bool,
    ) -> impl IntoElement {
        div()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .p_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .p_3()
                    .child(div().flex_1().child(self.composer_input.clone()))
                    .child(
                        button("composer-send", "Send", is_running, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.start_fake_turn(cx))),
                    ),
            )
    }

    fn context_panel(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let backend = if self.app_server.is_some() {
            "codex app-server"
        } else {
            "fake runtime"
        };

        div()
            .w(px(300.0))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(section_header("Plan", theme))
            .child(context_block(
                theme,
                "Current turn",
                self.model.turn_status.clone(),
            ))
            .child(section_header("Approvals", theme))
            .child(div().px_3().pb_4().children(self.approval_rows(theme, cx)))
            .child(section_header("Changed files", theme))
            .child(context_block(
                theme,
                "desktop repo",
                "Model, reducer, fake runtime, and root GPUI view".to_string(),
            ))
            .child(section_header("Runtime", theme))
            .child(context_block(
                theme,
                "Backend",
                format!("{backend}; fake runtime remains available before connecting"),
            ))
            .child(
                div().mx_3().child(
                    secondary_button("connect-codex", "Connect Codex", theme)
                        .on_click(cx.listener(|this, _, _, cx| this.connect_codex(cx))),
                ),
            )
    }

    fn approval_rows(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        if self.model.pending_approvals.is_empty() {
            return vec![
                div()
                    .id("approval-empty")
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted)
                    .child("No pending approvals"),
            ];
        }

        self.model
            .pending_approvals
            .iter()
            .map(|approval| {
                let approve_id = approval.id.clone();
                let deny_id = approval.id.clone();
                let element_suffix = element_id_suffix(&approval.id);

                div()
                    .id(format!("approval-row-{element_suffix}"))
                    .rounded_md()
                    .border_1()
                    .border_color(theme.reasoning)
                    .bg(theme.background)
                    .p_3()
                    .mb_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(approval.title.clone()),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .line_clamp(4)
                            .text_color(theme.muted)
                            .child(approval.detail.clone()),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(theme.reasoning)
                            .child(approval.action.clone()),
                    )
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .gap_2()
                            .child(
                                button(
                                    format!("approval-approve-{element_suffix}"),
                                    "Approve",
                                    false,
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.resolve_approval(approve_id.clone(), true, cx);
                                    },
                                )),
                            )
                            .child(
                                secondary_button(
                                    format!("approval-deny-{element_suffix}"),
                                    "Deny",
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.resolve_approval(deny_id.clone(), false, cx);
                                    },
                                )),
                            ),
                    )
            })
            .collect()
    }

    fn status_bar(&self, theme: &Theme) -> impl IntoElement {
        div()
            .h(px(30.0))
            .px_3()
            .flex()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.surface_alt)
            .text_xs()
            .text_color(theme.muted)
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(format!("model {}", self.model.active_model))
                    .child(format!("sandbox {}", self.model.sandbox_label))
                    .child(format!("cwd {}", self.model.cwd)),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(self.model.status_message.clone())
                    .child(format!("skipped {}", self.model.skipped_events)),
            )
    }
}

#[derive(Clone, Copy)]
struct Theme {
    accent: Hsla,
    accent_soft: Hsla,
    background: Hsla,
    border: Hsla,
    command: Hsla,
    diff: Hsla,
    muted: Hsla,
    plan: Hsla,
    reasoning: Hsla,
    surface: Hsla,
    surface_alt: Hsla,
    text: Hsla,
    user: Hsla,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: rgb(0x2563eb).into(),
            accent_soft: rgb(0xdbeafe).into(),
            background: rgb(0xf7f7f4).into(),
            border: rgb(0xd7d7ce).into(),
            command: rgb(0x475569).into(),
            diff: rgb(0x0f766e).into(),
            muted: rgb(0x6b7280).into(),
            plan: rgb(0x7c3aed).into(),
            reasoning: rgb(0xb45309).into(),
            surface: rgb(0xffffff).into(),
            surface_alt: rgb(0xefefea).into(),
            text: rgb(0x171717).into(),
            user: rgb(0x047857).into(),
        }
    }
}

fn section_header(label: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .px_3()
        .pt_3()
        .pb_2()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.muted)
        .child(label)
}

fn empty_thread_row(theme: &Theme) -> Stateful<Div> {
    div()
        .id("empty-thread-row")
        .rounded_md()
        .px_2()
        .py_2()
        .text_sm()
        .text_color(theme.muted)
        .child("No threads yet")
}

fn thread_row(thread: &ThreadSummary, active_thread: Option<&str>, theme: &Theme) -> Stateful<Div> {
    let is_active = active_thread == Some(thread.id.as_str());

    div()
        .id(format!("thread-row-{}", thread.id))
        .rounded_md()
        .p_2()
        .mb_1()
        .bg(if is_active {
            theme.accent_soft
        } else {
            theme.surface
        })
        .border_1()
        .border_color(if is_active {
            theme.accent
        } else {
            theme.border
        })
        .child(
            div()
                .text_sm()
                .line_clamp(1)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(thread.title.clone()),
        )
        .child(
            div()
                .mt_1()
                .text_xs()
                .line_clamp(1)
                .text_color(theme.muted)
                .child(thread.subtitle.clone()),
        )
        .child(
            div()
                .mt_2()
                .text_xs()
                .text_color(theme.muted)
                .child(thread.status.clone()),
        )
}

fn timeline_row(ix: usize, item: &TimelineItem, theme: &Theme) -> Stateful<Div> {
    let color = kind_color(item.kind, theme);

    div()
        .id(format!("timeline-row-{}", item.id))
        .h(px(ROW_HEIGHT))
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(if ix % 2 == 0 {
            theme.background
        } else {
            theme.surface
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(status_pill(
                    item.kind.label(),
                    color,
                    color.opacity(0.12),
                    color,
                ))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .line_clamp(1)
                        .child(item.title.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted)
                        .child(item.status.label()),
                ),
        )
        .child(
            div()
                .mt_1()
                .text_sm()
                .line_clamp(2)
                .text_color(theme.text)
                .child(item.body.clone()),
        )
        .child(
            div()
                .mt_1()
                .text_xs()
                .line_clamp(1)
                .text_color(theme.muted)
                .child(item.meta.clone()),
        )
}

fn kind_color(kind: TimelineKind, theme: &Theme) -> Hsla {
    match kind {
        TimelineKind::Assistant => theme.accent,
        TimelineKind::Command => theme.command,
        TimelineKind::Diff => theme.diff,
        TimelineKind::Plan => theme.plan,
        TimelineKind::Reasoning => theme.reasoning,
        TimelineKind::System => theme.muted,
        TimelineKind::User => theme.user,
    }
}

fn status_color(status: RuntimeStatus) -> Hsla {
    match status {
        RuntimeStatus::Disconnected => rgb(0xb91c1c).into(),
        RuntimeStatus::Idle => rgb(0x047857).into(),
        RuntimeStatus::Running => rgb(0x2563eb).into(),
        RuntimeStatus::WaitingForApproval => rgb(0xb45309).into(),
    }
}

fn string_at(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToString::to_string)
}

fn status_pill(
    label: impl Into<String>,
    color: Hsla,
    background: Hsla,
    text: Hsla,
) -> impl IntoElement {
    div()
        .rounded_sm()
        .px_2()
        .py_1()
        .bg(background)
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(text)
        .border_1()
        .border_color(color.opacity(0.25))
        .child(label.into())
}

fn context_block(theme: &Theme, title: &'static str, body: String) -> impl IntoElement {
    div()
        .mx_3()
        .mb_3()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .p_3()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(
            div()
                .mt_1()
                .text_xs()
                .line_clamp(3)
                .text_color(theme.muted)
                .child(body),
        )
}

fn button(
    id: impl Into<ElementId>,
    label: &'static str,
    disabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .rounded_md()
        .px_3()
        .py_1p5()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.surface)
        .bg(theme.accent)
        .when(disabled, |button| button.opacity(0.45).cursor_not_allowed())
        .when(!disabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x1d4ed8)))
                .active(|style| style.opacity(0.85))
        })
        .child(label)
}

fn secondary_button(id: impl Into<ElementId>, label: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .rounded_md()
        .px_3()
        .py_1p5()
        .text_sm()
        .text_color(theme.text)
        .bg(theme.surface_alt)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|style| style.bg(rgb(0xe4e4de)))
        .child(label)
}

fn element_id_suffix(value: &str) -> String {
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
