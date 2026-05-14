use std::ffi::OsStr;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;

use crate::reducer::AgentEvent;
use serde_json::{Value, json};

#[derive(Debug)]
pub enum AppServerWireEvent {
    Response(Value),
    ServerRequest(Value),
    Notification(Value),
    MalformedLine(String),
    IoError(String),
}

pub struct AppServerProcess {
    child: Child,
    event_rx: mpsc::Receiver<AppServerWireEvent>,
    stdin_tx: mpsc::Sender<Value>,
}

impl AppServerProcess {
    pub fn spawn_default() -> io::Result<Self> {
        Self::spawn("codex")
    }

    pub fn spawn(binary: impl AsRef<OsStr>) -> io::Result<Self> {
        let mut child = Command::new(binary)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("app-server stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("app-server stdout was not piped"))?;

        let (stdin_tx, stdin_rx) = mpsc::channel::<Value>();
        let (event_tx, event_rx) = mpsc::channel::<AppServerWireEvent>();

        let writer_event_tx = event_tx.clone();
        thread::spawn(move || {
            for message in stdin_rx {
                let line = match serde_json::to_string(&message) {
                    Ok(line) => line,
                    Err(error) => {
                        let _ =
                            writer_event_tx.send(AppServerWireEvent::IoError(error.to_string()));
                        continue;
                    }
                };
                if let Err(error) = writeln!(stdin, "{line}") {
                    let _ = writer_event_tx.send(AppServerWireEvent::IoError(error.to_string()));
                    break;
                }
            }
        });

        let reader_event_tx = event_tx;
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(value) => {
                            let _ = reader_event_tx.send(classify_wire_value(value, line));
                        }
                        Err(_) => {
                            let _ = reader_event_tx.send(AppServerWireEvent::MalformedLine(line));
                        }
                    },
                    Err(error) => {
                        let _ =
                            reader_event_tx.send(AppServerWireEvent::IoError(error.to_string()));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            event_rx,
            stdin_tx,
        })
    }

    pub fn send(&self, message: Value) -> io::Result<()> {
        self.stdin_tx
            .send(message)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "app-server stdin closed"))
    }

    pub fn try_recv(&self) -> Option<AppServerWireEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for AppServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

pub fn classify_wire_value(value: Value, raw_line: String) -> AppServerWireEvent {
    let has_id = value.get("id").is_some();
    let has_method = value.get("method").is_some();
    let has_result_or_error = value.get("result").is_some() || value.get("error").is_some();

    if has_id && has_method && !has_result_or_error {
        AppServerWireEvent::ServerRequest(value)
    } else if has_id && has_result_or_error {
        AppServerWireEvent::Response(value)
    } else if has_method {
        AppServerWireEvent::Notification(value)
    } else {
        AppServerWireEvent::MalformedLine(raw_line)
    }
}

#[derive(Debug, Clone)]
pub struct JsonRpcRequestBuilder {
    next_id: u64,
}

impl Default for JsonRpcRequestBuilder {
    fn default() -> Self {
        Self { next_id: 1 }
    }
}

impl JsonRpcRequestBuilder {
    pub fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "codex_gpui_desktop",
                    "title": "Codex GPUI Desktop",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )
    }

    pub fn initialized_notification(&self) -> Value {
        json!({ "method": "initialized" })
    }

    pub fn thread_start(&mut self, cwd: &str) -> Value {
        self.request(
            "thread/start",
            json!({
                "cwd": cwd,
                "sandbox": "workspaceWrite",
                "personality": "friendly",
                "serviceName": "codex_gpui_desktop",
                "sessionStartSource": "startup"
            }),
        )
    }

    pub fn turn_start(&mut self, thread_id: &str, cwd: &str, prompt: &str) -> Value {
        self.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "cwd": cwd,
                "input": [
                    {
                        "type": "text",
                        "text": prompt
                    }
                ]
            }),
        )
    }

    pub fn turn_interrupt(&mut self, thread_id: &str, turn_id: &str) -> Value {
        self.request(
            "turn/interrupt",
            json!({
                "threadId": thread_id,
                "turnId": turn_id
            }),
        )
    }

    fn request(&mut self, method: &'static str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        json!({
            "id": id,
            "method": method,
            "params": params
        })
    }
}

pub fn notification_to_agent_event(notification: &Value) -> Option<AgentEvent> {
    let method = notification.get("method")?.as_str()?;
    let params = notification.get("params").unwrap_or(&Value::Null);

    match method {
        "thread/started" => {
            let thread = params.get("thread")?;
            Some(AgentEvent::ThreadStarted {
                id: string_at(thread, "id").unwrap_or_else(|| "unknown-thread".to_string()),
                title: string_at(thread, "preview")
                    .filter(|preview| !preview.is_empty())
                    .unwrap_or_else(|| "Codex thread".to_string()),
            })
        }
        "turn/started" => Some(AgentEvent::TurnStarted {
            id: params
                .get("turn")
                .and_then(|turn| string_at(turn, "id"))
                .unwrap_or_else(|| "unknown-turn".to_string()),
        }),
        "turn/completed" => Some(AgentEvent::TurnCompleted {
            id: params
                .get("turn")
                .and_then(|turn| string_at(turn, "id"))
                .unwrap_or_else(|| "unknown-turn".to_string()),
        }),
        "item/agentMessage/delta" => Some(AgentEvent::AgentMessageDelta {
            item_id: string_at(params, "itemId").unwrap_or_else(|| "agent-message".to_string()),
            delta: string_at(params, "delta").unwrap_or_default(),
        }),
        "item/plan/delta" => Some(AgentEvent::PlanDelta {
            item_id: string_at(params, "itemId").unwrap_or_else(|| "plan".to_string()),
            delta: string_at(params, "delta").unwrap_or_default(),
        }),
        "item/reasoningText/delta" | "item/reasoningSummaryText/delta" => {
            Some(AgentEvent::ReasoningDelta {
                item_id: string_at(params, "itemId").unwrap_or_else(|| "reasoning".to_string()),
                delta: string_at(params, "delta").unwrap_or_default(),
            })
        }
        "item/commandExecution/outputDelta" => Some(AgentEvent::CommandOutputDelta {
            item_id: string_at(params, "itemId").unwrap_or_else(|| "command".to_string()),
            delta: string_at(params, "delta").unwrap_or_default(),
        }),
        "turn/diff/updated" => Some(AgentEvent::DiffUpdated {
            item_id: format!(
                "diff-{}",
                string_at(params, "turnId").unwrap_or_else(|| "turn".to_string())
            ),
            path: "turn diff".to_string(),
            summary: string_at(params, "diff").unwrap_or_default(),
        }),
        _ => None,
    }
}

pub fn server_request_to_agent_event(request: &Value) -> Option<AgentEvent> {
    let method = request.get("method")?.as_str()?;
    let params = request.get("params").unwrap_or(&Value::Null);
    let id = approval_event_id(request)?;

    match method {
        "item/commandExecution/requestApproval" => {
            let command = string_at(params, "command").unwrap_or_else(|| "unknown command".into());
            let cwd = string_at(params, "cwd")
                .map(|cwd| format!("cwd: {cwd}"))
                .unwrap_or_else(|| "cwd unavailable".to_string());
            let reason = string_at(params, "reason").unwrap_or_else(|| "No reason supplied".into());
            Some(AgentEvent::ApprovalRequested {
                id,
                title: "Run command".to_string(),
                detail: format!("{command}\n{cwd}\n{reason}"),
                action: "Approve once or deny".to_string(),
            })
        }
        "item/fileChange/requestApproval" => {
            let grant_root = string_at(params, "grantRoot")
                .map(|root| format!("Grant root: {root}"))
                .unwrap_or_else(|| "File change approval requested".to_string());
            let reason = string_at(params, "reason").unwrap_or_else(|| "No reason supplied".into());
            Some(AgentEvent::ApprovalRequested {
                id,
                title: "Apply file changes".to_string(),
                detail: format!("{grant_root}\n{reason}"),
                action: "Approve changes or deny".to_string(),
            })
        }
        "item/permissions/requestApproval" => {
            let cwd = string_at(params, "cwd")
                .map(|cwd| format!("cwd: {cwd}"))
                .unwrap_or_else(|| "cwd unavailable".to_string());
            let reason = string_at(params, "reason").unwrap_or_else(|| "No reason supplied".into());
            let permissions = describe_permissions(params.get("permissions"));
            Some(AgentEvent::ApprovalRequested {
                id,
                title: "Grant permissions".to_string(),
                detail: format!("{cwd}\n{reason}\n{permissions}"),
                action: "Grant for this turn or deny".to_string(),
            })
        }
        _ => None,
    }
}

pub fn approval_response_result(method: &str, params: &Value, approved: bool) -> Option<Value> {
    match method {
        "item/commandExecution/requestApproval" => Some(json!({
            "decision": if approved { "accept" } else { "decline" }
        })),
        "item/fileChange/requestApproval" => Some(json!({
            "decision": if approved { "accept" } else { "decline" }
        })),
        "item/permissions/requestApproval" => {
            let permissions = if approved {
                params
                    .get("permissions")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            } else {
                json!({})
            };
            Some(json!({
                "permissions": permissions,
                "scope": "turn"
            }))
        }
        _ => None,
    }
}

pub fn approval_event_id(request: &Value) -> Option<String> {
    let id = request.get("id")?;
    let id = match id {
        Value::Number(number) => format!("n-{number}"),
        Value::String(value) => format!("s-{}", hex_id(value)),
        _ => return None,
    };
    Some(format!("app-server-approval-{id}"))
}

fn describe_permissions(permissions: Option<&Value>) -> String {
    let Some(permissions) = permissions else {
        return "No permission details supplied".to_string();
    };

    let mut parts = Vec::new();
    if permissions.get("network").is_some() {
        parts.push("network".to_string());
    }
    if let Some(file_system) = permissions.get("fileSystem") {
        let mut file_parts = Vec::new();
        if file_system.get("read").is_some() {
            file_parts.push("read");
        }
        if file_system.get("write").is_some() {
            file_parts.push("write");
        }
        if file_parts.is_empty() {
            parts.push("file system".to_string());
        } else {
            parts.push(format!("file system {}", file_parts.join("/")));
        }
    }

    if parts.is_empty() {
        "Permission details are empty".to_string()
    } else {
        format!("Requested: {}", parts.join(", "))
    }
}

fn hex_id(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 2);
    for byte in value.as_bytes() {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_thread_start_for_selected_project() {
        let mut builder = JsonRpcRequestBuilder::default();

        let request = builder.thread_start("/tmp/project");

        assert_eq!(request["method"], "thread/start");
        assert_eq!(request["params"]["cwd"], "/tmp/project");
        assert_eq!(request["params"]["sandbox"], "workspaceWrite");
    }

    #[test]
    fn builds_turn_start_with_text_input() {
        let mut builder = JsonRpcRequestBuilder::default();

        let request = builder.turn_start("thr_123", "/tmp/project", "hello");

        assert_eq!(request["method"], "turn/start");
        assert_eq!(request["params"]["threadId"], "thr_123");
        assert_eq!(request["params"]["input"][0]["type"], "text");
        assert_eq!(request["params"]["input"][0]["text"], "hello");
    }

    #[test]
    fn builds_interrupt_for_active_turn() {
        let mut builder = JsonRpcRequestBuilder::default();

        let request = builder.turn_interrupt("thr_123", "turn_456");

        assert_eq!(request["method"], "turn/interrupt");
        assert_eq!(request["params"]["threadId"], "thr_123");
        assert_eq!(request["params"]["turnId"], "turn_456");
    }

    #[test]
    fn maps_agent_message_delta_notification() {
        let event = notification_to_agent_event(&json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thr_123",
                "turnId": "turn_456",
                "itemId": "item_789",
                "delta": "hello"
            }
        }));

        assert_eq!(
            event,
            Some(AgentEvent::AgentMessageDelta {
                item_id: "item_789".to_string(),
                delta: "hello".to_string()
            })
        );
    }

    #[test]
    fn maps_turn_completed_notification() {
        let event = notification_to_agent_event(&json!({
            "method": "turn/completed",
            "params": {
                "threadId": "thr_123",
                "turn": { "id": "turn_456" }
            }
        }));

        assert_eq!(
            event,
            Some(AgentEvent::TurnCompleted {
                id: "turn_456".to_string()
            })
        );
    }

    #[test]
    fn classifies_server_requests_before_responses() {
        let event = classify_wire_value(
            json!({
                "id": 7,
                "method": "item/commandExecution/requestApproval",
                "params": {}
            }),
            "{}".to_string(),
        );

        assert!(matches!(event, AppServerWireEvent::ServerRequest(_)));
    }

    #[test]
    fn maps_command_approval_request() {
        let event = server_request_to_agent_event(&json!({
            "id": 7,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "itemId": "item-1",
                "command": "cargo test",
                "cwd": "/tmp/project",
                "reason": "Needs to run tests"
            }
        }));

        assert_eq!(
            event,
            Some(AgentEvent::ApprovalRequested {
                id: "app-server-approval-n-7".to_string(),
                title: "Run command".to_string(),
                detail: "cargo test\ncwd: /tmp/project\nNeeds to run tests".to_string(),
                action: "Approve once or deny".to_string()
            })
        );
    }

    #[test]
    fn approval_event_id_handles_string_ids() {
        let id = approval_event_id(&json!({
            "id": "srv-1",
            "method": "item/fileChange/requestApproval",
            "params": {}
        }));

        assert_eq!(id, Some("app-server-approval-s-7372762d31".to_string()));
    }

    #[test]
    fn builds_permission_approval_response() {
        let result = approval_response_result(
            "item/permissions/requestApproval",
            &json!({
                "permissions": {
                    "network": { "enabled": true }
                }
            }),
            true,
        );

        assert_eq!(
            result,
            Some(json!({
                "permissions": {
                    "network": { "enabled": true }
                },
                "scope": "turn"
            }))
        );
    }
}
