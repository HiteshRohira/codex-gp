use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use crate::model::{
    AppModel, ProjectSummary, RuntimeStatus, ThreadSummary, TimelineItem, TimelineKind,
};
use crate::reducer::{reduce, upsert_project};
use crate::runtime::AgentRuntime;
use crate::runtime::app_server::{
    AppServerProcess, AppServerWireEvent, JsonRpcRequestBuilder, approval_event_id,
    notification_to_agent_event, server_request_to_agent_event,
};
use crate::runtime::fake::FakeAgentRuntime;
use crate::ui::text_input::TextInput;
use gpui::{
    Context, Div, ElementId, Entity, Hsla, IntoElement, PathPromptOptions, Render, ScrollStrategy,
    Stateful, UniformListScrollHandle, Window, div, prelude::*, px, rgb, uniform_list,
};

const ROW_HEIGHT: f32 = 172.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingRequest {
    Initialize,
    ThreadStart,
    TurnInterrupt,
    TurnStart,
}

pub struct RootView {
    app_server: Option<AppServerProcess>,
    composer_input: Entity<TextInput>,
    live_thread_id: Option<String>,
    live_turn_id: Option<String>,
    model: AppModel,
    pending_requests: HashMap<u64, PendingRequest>,
    queued_live_prompt: Option<String>,
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
        upsert_project(&mut model);
        model.status_message = "Phase 0 fake runtime ready".to_string();

        Self {
            app_server: None,
            composer_input: cx.new(|cx| TextInput::new("Message Codex for this project...", cx)),
            live_thread_id: None,
            live_turn_id: None,
            model,
            pending_requests: HashMap::new(),
            queued_live_prompt: None,
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
            self.queued_live_prompt = Some(prompt);
            if !self.has_pending_thread_start() {
                let request = self.request_builder.thread_start(&self.model.cwd);
                self.send_app_server_request(request, PendingRequest::ThreadStart);
            }
            self.model.status_message = "Creating Codex thread before sending".to_string();
            cx.notify();
            return;
        };

        reduce(
            &mut self.model,
            crate::reducer::AgentEvent::UserMessageSubmitted {
                prompt: prompt.clone(),
            },
        );
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

    fn has_pending_thread_start(&self) -> bool {
        self.pending_requests
            .values()
            .any(|pending| matches!(pending, PendingRequest::ThreadStart))
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
            AppServerWireEvent::Response(response) => self.handle_app_server_response(response, cx),
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
            let Some(_approval_id) = approval_event_id(&request) else {
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

    fn handle_app_server_response(&mut self, response: serde_json::Value, cx: &mut Context<Self>) {
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
                if let Some(prompt) = self.queued_live_prompt.take() {
                    self.start_live_turn(prompt, cx);
                    return;
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

    fn set_project_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match path.canonicalize() {
            Ok(path) if path.is_dir() => {
                let path = path.display().to_string();
                self.model.cwd = path.clone();
                upsert_project(&mut self.model);
                self.model.status_message = "Project selected".to_string();
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

    fn new_chat(&mut self, cx: &mut Context<Self>) {
        self.stream_token += 1;
        self.model.timeline.clear();
        self.model.pending_approvals.clear();
        self.model.active_thread = None;
        self.live_thread_id = None;
        self.live_turn_id = None;
        self.queued_live_prompt = None;
        self.model.runtime_status = RuntimeStatus::Idle;
        if self.app_server.is_some() {
            let request = self.request_builder.thread_start(&self.model.cwd);
            self.send_app_server_request(request, PendingRequest::ThreadStart);
            self.model.status_message = "Started a new Codex thread".to_string();
        } else {
            self.model.status_message = "New local chat ready".to_string();
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
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.sidebar(&theme, cx))
                    .child(self.main_pane(&theme, cx, is_running)),
            )
            .child(self.status_bar(&theme))
    }
}

impl RootView {
    fn sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let projects = if self.model.projects.is_empty() {
            vec![empty_project_row(theme)]
        } else {
            self.model
                .projects
                .iter()
                .map(|project| {
                    let path = project.path.clone();
                    project_row(project, self.model.active_project.as_deref(), theme).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.set_project_path(PathBuf::from(path.clone()), cx);
                        }),
                    )
                })
                .collect()
        };

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
            .w(px(264.0))
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(
                div()
                    .h(px(58.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div().flex().items_center().gap_2().child(
                            div()
                                .text_lg()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("Codex"),
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                compact_button("new-chat", "New", theme)
                                    .on_click(cx.listener(|this, _, _, cx| this.new_chat(cx))),
                            )
                            .child(
                                compact_button("choose-project-top", "Open", theme).on_click(
                                    cx.listener(|this, _, _, cx| this.choose_project(cx)),
                                ),
                            ),
                    ),
            )
            .child(
                div().px_3().pb_3().child(
                    div()
                        .h(px(34.0))
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface_alt)
                        .px_3()
                        .flex()
                        .items_center()
                        .text_sm()
                        .text_color(theme.muted)
                        .child("Search"),
                ),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("PROJECTS"),
                    )
                    .child(
                        compact_button("add-project-inline", "Add", theme)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_project(cx))),
                    ),
            )
            .child(div().px_2().children(projects))
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("THREADS"),
                    )
                    .child(
                        compact_button("add-thread-inline", "New", theme)
                            .on_click(cx.listener(|this, _, _, cx| this.new_chat(cx))),
                    ),
            )
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
            .h(px(58.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div().flex().items_center().gap_2().child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(active_thread),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .when(self.app_server.is_none(), |controls| {
                        controls.child(
                            secondary_button("connect-codex", "Connect Codex", theme)
                                .on_click(cx.listener(|this, _, _, cx| this.connect_codex(cx))),
                        )
                    })
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
        let show_thinking = self.model.runtime_status == RuntimeStatus::Running;
        if self.model.timeline.is_empty() && !show_thinking {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(theme.muted)
                .child("Send a message to start a thread.")
                .into_any_element();
        }

        div()
            .flex_1()
            .overflow_hidden()
            .child(
                uniform_list(
                    "codex-gpui-transcript",
                    self.model.timeline.len() + usize::from(show_thinking),
                    cx.processor(|this, range: Range<usize>, _window, _cx| {
                        range
                            .map(|ix| {
                                if ix < this.model.timeline.len() {
                                    timeline_row(ix, &this.model.timeline[ix], &Theme::default())
                                } else {
                                    thinking_row(&Theme::default())
                                }
                            })
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
            .bg(theme.background)
            .px_4()
            .pb_5()
            .pt_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if self.app_server.is_some() {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .bg(theme.surface)
                    .p_3()
                    .shadow_sm()
                    .child(
                        compact_button("composer-add-project", "Attach", theme)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_project(cx))),
                    )
                    .child(div().flex_1().child(self.composer_input.clone()))
                    .child(
                        button("composer-send", "Send", is_running, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.start_fake_turn(cx))),
                    ),
            )
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
            accent: rgb(0x3b82f6).into(),
            background: rgb(0x0f1013).into(),
            border: rgb(0x292b31).into(),
            command: rgb(0xa1a1aa).into(),
            diff: rgb(0x34d399).into(),
            muted: rgb(0x8a8f98).into(),
            plan: rgb(0xa78bfa).into(),
            reasoning: rgb(0xf59e0b).into(),
            surface: rgb(0x18191d).into(),
            surface_alt: rgb(0x202126).into(),
            text: rgb(0xe8e8ea).into(),
            user: rgb(0x2a2c32).into(),
        }
    }
}

fn empty_project_row(theme: &Theme) -> Stateful<Div> {
    div()
        .id("empty-project-row")
        .rounded_md()
        .px_2()
        .py_2()
        .text_sm()
        .text_color(theme.muted)
        .child("No projects yet")
}

fn project_row(
    project: &ProjectSummary,
    active_project: Option<&str>,
    theme: &Theme,
) -> Stateful<Div> {
    let is_active = active_project == Some(project.id.as_str());

    div()
        .id(format!("project-row-{}", project.id))
        .rounded_md()
        .px_2()
        .py_1p5()
        .mb_1()
        .bg(if is_active {
            theme.surface_alt
        } else {
            theme.surface
        })
        .cursor_pointer()
        .child(
            div().flex().items_center().gap_2().child(
                div()
                    .text_sm()
                    .line_clamp(1)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(project.title.clone()),
            ),
        )
        .child(
            div()
                .ml_2()
                .text_xs()
                .line_clamp(1)
                .text_color(theme.muted)
                .child(project.path.clone()),
        )
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
        .px_3()
        .py_2()
        .mb_1()
        .bg(if is_active {
            theme.surface_alt
        } else {
            theme.surface
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
                .mt_0p5()
                .text_xs()
                .line_clamp(1)
                .text_color(theme.muted)
                .child(thread.status.clone()),
        )
}

fn timeline_row(_ix: usize, item: &TimelineItem, theme: &Theme) -> Stateful<Div> {
    let color = kind_color(item.kind, theme);
    let is_user = matches!(item.kind, TimelineKind::User);
    let is_tool = matches!(
        item.kind,
        TimelineKind::Command
            | TimelineKind::Diff
            | TimelineKind::Plan
            | TimelineKind::Reasoning
            | TimelineKind::System
    );

    div()
        .id(format!("timeline-row-{}", item.id))
        .h(px(ROW_HEIGHT))
        .px_5()
        .py_3()
        .bg(theme.background)
        .flex()
        .justify_end()
        .when(!is_user, |row| row.justify_start())
        .child(
            div()
                .w(if is_user { px(520.0) } else { px(820.0) })
                .rounded_md()
                .px_4()
                .py_3()
                .border_1()
                .border_color(if is_tool {
                    theme.border
                } else {
                    theme.background
                })
                .bg(if is_user {
                    theme.user
                } else if is_tool {
                    theme.surface
                } else {
                    theme.background
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .when(!is_tool, |meta| meta.hidden())
                        .child(status_pill(
                            item.kind.label(),
                            color,
                            color.opacity(0.10),
                            color,
                        ))
                        .child(
                            div()
                                .text_xs()
                                .line_clamp(1)
                                .text_color(theme.muted)
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
                        .mt(if is_tool { px(8.0) } else { px(0.0) })
                        .children(markdown_blocks(&item.body, theme, is_tool)),
                )
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .line_clamp(1)
                        .text_color(theme.muted)
                        .when(item.meta.is_empty() || !is_tool, |meta| meta.hidden())
                        .child(item.meta.clone()),
                ),
        )
}

fn thinking_row(theme: &Theme) -> Stateful<Div> {
    div()
        .id("timeline-thinking")
        .h(px(ROW_HEIGHT))
        .px_5()
        .py_3()
        .bg(theme.background)
        .flex()
        .justify_start()
        .child(
            div()
                .w(px(820.0))
                .rounded_md()
                .px_4()
                .py_3()
                .border_1()
                .border_color(theme.border)
                .bg(theme.surface)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .text_color(theme.muted)
                        .child("Thinking..."),
                ),
        )
}

fn markdown_blocks(markdown: &str, theme: &Theme, compact: bool) -> Vec<Div> {
    let mut blocks = Vec::new();
    let mut paragraph = Vec::new();
    let mut code = Vec::new();
    let mut in_code = false;

    for line in markdown.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            flush_paragraph(&mut blocks, &mut paragraph, theme);
            if in_code {
                blocks.push(code_block(&code.join("\n"), theme));
                code.clear();
            }
            in_code = !in_code;
            continue;
        }

        if in_code {
            code.push(trimmed.to_string());
            continue;
        }

        let trimmed_start = trimmed.trim_start();
        if trimmed_start.is_empty() {
            flush_paragraph(&mut blocks, &mut paragraph, theme);
            continue;
        }

        if let Some(heading) = trimmed_start.strip_prefix("### ") {
            flush_paragraph(&mut blocks, &mut paragraph, theme);
            blocks.push(markdown_line(clean_inline_markdown(heading), theme, true));
        } else if let Some(heading) = trimmed_start.strip_prefix("## ") {
            flush_paragraph(&mut blocks, &mut paragraph, theme);
            blocks.push(markdown_line(clean_inline_markdown(heading), theme, true));
        } else if let Some(heading) = trimmed_start.strip_prefix("# ") {
            flush_paragraph(&mut blocks, &mut paragraph, theme);
            blocks.push(markdown_line(clean_inline_markdown(heading), theme, true));
        } else if let Some(item) = bullet_text(trimmed_start) {
            flush_paragraph(&mut blocks, &mut paragraph, theme);
            blocks.push(markdown_line(
                format!("- {}", clean_inline_markdown(item)),
                theme,
                false,
            ));
        } else {
            paragraph.push(trimmed_start.to_string());
        }
    }

    if in_code {
        blocks.push(code_block(&code.join("\n"), theme));
    }
    flush_paragraph(&mut blocks, &mut paragraph, theme);

    if blocks.is_empty() {
        blocks.push(markdown_line(String::new(), theme, false));
    }
    if compact && blocks.len() > 3 {
        blocks.truncate(3);
    }
    blocks
}

fn flush_paragraph(blocks: &mut Vec<Div>, paragraph: &mut Vec<String>, theme: &Theme) {
    if paragraph.is_empty() {
        return;
    }
    blocks.push(markdown_line(
        clean_inline_markdown(&paragraph.join(" ")),
        theme,
        false,
    ));
    paragraph.clear();
}

fn markdown_line(text: String, theme: &Theme, heading: bool) -> Div {
    div()
        .mb_1()
        .text_sm()
        .line_height(px(22.0))
        .line_clamp(if heading { 1 } else { 3 })
        .text_color(theme.text)
        .when(heading, |line| line.font_weight(gpui::FontWeight::SEMIBOLD))
        .child(text)
}

fn code_block(code: &str, theme: &Theme) -> Div {
    div()
        .mb_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.surface_alt)
        .p_3()
        .text_xs()
        .font_family("Lilex")
        .line_height(px(18.0))
        .line_clamp(5)
        .text_color(theme.text)
        .child(code.to_string())
}

fn bullet_text(line: &str) -> Option<&str> {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| {
            let (number, rest) = line.split_once(". ")?;
            number.chars().all(|ch| ch.is_ascii_digit()).then_some(rest)
        })
}

fn clean_inline_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' | '*' | '_' => {}
            '[' => {
                let mut label = String::new();
                for next in chars.by_ref() {
                    if next == ']' {
                        break;
                    }
                    label.push(next);
                }
                if chars.peek() == Some(&'(') {
                    let _ = chars.next();
                    let mut target = String::new();
                    for next in chars.by_ref() {
                        if next == ')' {
                            break;
                        }
                        target.push(next);
                    }
                    out.push_str(&label);
                    if !target.is_empty() {
                        out.push(' ');
                        out.push_str(&target);
                    }
                } else {
                    out.push('[');
                    out.push_str(&label);
                }
            }
            _ => out.push(ch),
        }
    }
    out
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
        .text_color(theme.text)
        .bg(theme.accent)
        .when(disabled, |button| button.opacity(0.45).cursor_not_allowed())
        .when(!disabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x2563eb)))
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
        .hover(|style| style.bg(rgb(0x2a2c32)))
        .child(label)
}

fn compact_button(id: impl Into<ElementId>, label: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .rounded_md()
        .px_2()
        .py_1()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .bg(theme.surface_alt)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|style| style.bg(rgb(0x2a2c32)))
        .child(label)
}
