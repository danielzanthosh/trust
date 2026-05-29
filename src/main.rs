use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use dotenvy::dotenv;
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Stdout};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const MAX_AGENT_STEPS: usize = 5;
const DEFAULT_SANDBOX_DIR: &str = "sandbox/workspace";
const DEFAULT_SANDBOX_COMMAND_TIMEOUT_MS: u64 = 30_000;
const TOOL_RESULT_PREFIX: &str = "Tool result from TRUST runtime: ";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Message {
    role: String,
    content: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ToolCall {
    r#type: String,
    tool: String,
    args: ToolArgs,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ToolArgs {
    path: Option<String>,
    content: Option<String>,
    url: Option<String>,
    app: Option<String>,
    command: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    OpenAiCompatible,
    AnthropicCompatible,
}

#[derive(Clone, Debug)]
struct SandboxConfig {
    root: PathBuf,
    workspace: PathBuf,
    outputs: PathBuf,
    temp: PathBuf,
    command_timeout_ms: u64,
}

impl SandboxConfig {
    fn load() -> Result<Self, String> {
        let current_dir = env::current_dir()
            .map_err(|e| format!("Failed to resolve current working directory: {}", e))?;

        let sandbox_dir = env::var("SANDBOX_DIR")
            .unwrap_or_else(|_| DEFAULT_SANDBOX_DIR.to_string())
            .trim()
            .to_string();

        let workspace = absolute_path_from(&current_dir, &sandbox_dir);
        let root = workspace
            .parent()
            .unwrap_or(workspace.as_path())
            .to_path_buf();
        let outputs = root.join("outputs");
        let temp = root.join("temp");
        let command_timeout_ms = env::var("SANDBOX_COMMAND_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_SANDBOX_COMMAND_TIMEOUT_MS);

        Ok(Self {
            root,
            workspace,
            outputs,
            temp,
            command_timeout_ms,
        })
    }
}

#[derive(Clone, Copy)]
struct AllowedCommand {
    display: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandRisk {
    Safe,
    NeedsApproval,
    Destructive,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermissionChoice {
    AllowOnce,
    AllowAlways,
    Decline,
}

#[derive(Clone, Debug)]
enum UiMessageRole {
    User,
    Assistant,
    Tool,
    Info,
}

#[derive(Clone, Debug)]
struct UiMessage {
    role: UiMessageRole,
    content: String,
}

struct PendingApproval {
    title: String,
    command: String,
    risk_label: String,
    allow_always: bool,
    responder: oneshot::Sender<PermissionChoice>,
}

enum RuntimeEvent {
    Status(String),
    StartAssistant,
    AssistantChunk(String),
    CommitAssistant(String),
    DiscardAssistantDraft,
    ToolResult(String),
    ApprovalRequested(PendingApproval),
    TurnFinished(Vec<Message>),
    Error(String),
}

struct App {
    current_chat: String,
    model_history: Vec<Message>,
    visible_messages: Vec<UiMessage>,
    input: String,
    draft_assistant: String,
    status: String,
    should_quit: bool,
    busy: bool,
    pending_approval: Option<PendingApproval>,
    sandbox: SandboxConfig,
}

impl App {
    fn new(sandbox: SandboxConfig) -> Self {
        let current_chat = "default".to_string();
        let model_history = ensure_system_message(load_history(&current_chat));
        let visible_messages = ui_messages_from_history(&model_history);

        Self {
            current_chat,
            model_history,
            visible_messages,
            input: String::new(),
            draft_assistant: String::new(),
            status: "Ready".to_string(),
            should_quit: false,
            busy: false,
            pending_approval: None,
            sandbox,
        }
    }

    fn set_chat(&mut self, chat_name: String) {
        self.current_chat = chat_name;
        self.model_history = ensure_system_message(load_history(&self.current_chat));
        self.visible_messages = ui_messages_from_history(&self.model_history);
        self.draft_assistant.clear();
        self.pending_approval = None;
        self.busy = false;
        self.status = format!("Switched to chat: {}", self.current_chat);
    }

    fn add_info_message(&mut self, content: impl Into<String>) {
        self.visible_messages.push(UiMessage {
            role: UiMessageRole::Info,
            content: content.into(),
        });
    }

    fn add_user_message(&mut self, content: String) {
        self.visible_messages.push(UiMessage {
            role: UiMessageRole::User,
            content,
        });
    }

    fn add_tool_message(&mut self, content: String) {
        self.visible_messages.push(UiMessage {
            role: UiMessageRole::Tool,
            content,
        });
    }

    fn add_assistant_message(&mut self, content: String) {
        self.visible_messages.push(UiMessage {
            role: UiMessageRole::Assistant,
            content,
        });
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Status(status) => self.status = status,
            RuntimeEvent::StartAssistant => {
                self.draft_assistant.clear();
                self.busy = true;
            }
            RuntimeEvent::AssistantChunk(chunk) => self.draft_assistant.push_str(&chunk),
            RuntimeEvent::CommitAssistant(message) => {
                self.draft_assistant.clear();
                self.add_assistant_message(message);
            }
            RuntimeEvent::DiscardAssistantDraft => self.draft_assistant.clear(),
            RuntimeEvent::ToolResult(result) => self.add_tool_message(result),
            RuntimeEvent::ApprovalRequested(pending) => {
                self.status = format!("Approval required for command: {}", pending.command);
                self.pending_approval = Some(pending);
            }
            RuntimeEvent::TurnFinished(history) => {
                self.model_history = history;
                self.busy = false;
                self.pending_approval = None;
                self.draft_assistant.clear();
                save_history(&self.current_chat, &self.model_history);
                self.status = format!("Ready · chat: {}", self.current_chat);
            }
            RuntimeEvent::Error(error) => {
                self.draft_assistant.clear();
                self.busy = false;
                self.pending_approval = None;
                self.status = error.clone();
                self.add_info_message(error);
            }
        }
    }
}

fn absolute_path_from(base: &Path, path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);

    if candidate.is_absolute() {
        candidate
    } else {
        base.join(candidate)
    }
}

fn display_path(path: &Path) -> String {
    match env::current_dir() {
        Ok(current_dir) => match path.strip_prefix(&current_dir) {
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        },
        Err(_) => path.display().to_string(),
    }
}

fn validate_relative_path(path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(path.trim());

    if candidate.as_os_str().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    if candidate.is_absolute() {
        return Err("Blocked unsafe path: absolute paths are not allowed".to_string());
    }

    let mut cleaned = PathBuf::new();

    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("Blocked unsafe path: path traversal is not allowed".to_string());
            }
        }
    }

    if cleaned.as_os_str().is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    Ok(cleaned)
}

fn resolve_sandbox_path(config: &SandboxConfig, requested_path: &str) -> Result<PathBuf, String> {
    let normalized = requested_path.trim().replace('\\', "/");

    if normalized.is_empty() {
        return Err("Path cannot be empty".to_string());
    }

    let (base, suffix) = if normalized == "workspace" {
        (&config.workspace, "")
    } else if let Some(suffix) = normalized.strip_prefix("workspace/") {
        (&config.workspace, suffix)
    } else if normalized == "outputs" {
        (&config.outputs, "")
    } else if let Some(suffix) = normalized.strip_prefix("outputs/") {
        (&config.outputs, suffix)
    } else if normalized == "temp" {
        (&config.temp, "")
    } else if let Some(suffix) = normalized.strip_prefix("temp/") {
        (&config.temp, suffix)
    } else {
        (&config.workspace, normalized.as_str())
    };

    if suffix.is_empty() {
        return Ok(base.to_path_buf());
    }

    Ok(base.join(validate_relative_path(suffix)?))
}

fn should_skip_project_entry(relative_path: &Path) -> bool {
    let normalized = relative_path.to_string_lossy().replace('\\', "/");

    normalized == "sandbox"
        || normalized.starts_with("sandbox/")
        || normalized == "target"
        || normalized.starts_with("target/")
        || normalized == "memory"
        || normalized.starts_with("memory/")
        || normalized == ".git"
        || normalized.starts_with(".git/")
        || normalized == ".env"
}

fn copy_project_tree(source: &Path, destination: &Path, project_root: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| {
        format!(
            "Failed to create sandbox directory {}: {}",
            display_path(destination),
            e
        )
    })?;

    let entries = fs::read_dir(source).map_err(|e| {
        format!(
            "Failed to read project directory {}: {}",
            display_path(source),
            e
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read project entry: {}", e))?;
        let source_path = entry.path();
        let relative_path = source_path
            .strip_prefix(project_root)
            .map_err(|e| format!("Failed to compute relative path for sandbox copy: {}", e))?;

        if should_skip_project_entry(relative_path) {
            continue;
        }

        let destination_path = destination.join(relative_path);
        let file_type = entry
            .file_type()
            .map_err(|e| format!("Failed to inspect {}: {}", display_path(&source_path), e))?;

        if file_type.is_dir() {
            copy_project_tree(&source_path, destination, project_root)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "Failed to create sandbox parent directory {}: {}",
                        display_path(parent),
                        e
                    )
                })?;
            }

            fs::copy(&source_path, &destination_path).map_err(|e| {
                format!(
                    "Failed to copy {} into sandbox: {}",
                    display_path(&source_path),
                    e
                )
            })?;
        }
    }

    Ok(())
}

fn ensure_sandbox_ready(config: &SandboxConfig) -> Result<(), String> {
    fs::create_dir_all(&config.root).map_err(|e| {
        format!(
            "Failed to create sandbox root {}: {}",
            display_path(&config.root),
            e
        )
    })?;
    fs::create_dir_all(&config.workspace).map_err(|e| {
        format!(
            "Failed to create sandbox workspace {}: {}",
            display_path(&config.workspace),
            e
        )
    })?;
    fs::create_dir_all(&config.outputs).map_err(|e| {
        format!(
            "Failed to create sandbox outputs {}: {}",
            display_path(&config.outputs),
            e
        )
    })?;
    fs::create_dir_all(&config.temp).map_err(|e| {
        format!(
            "Failed to create sandbox temp {}: {}",
            display_path(&config.temp),
            e
        )
    })?;

    let workspace_is_empty = fs::read_dir(&config.workspace)
        .map_err(|e| {
            format!(
                "Failed to inspect sandbox workspace {}: {}",
                display_path(&config.workspace),
                e
            )
        })?
        .next()
        .is_none();

    if workspace_is_empty {
        let project_root = env::current_dir()
            .map_err(|e| format!("Failed to resolve current project directory: {}", e))?;
        copy_project_tree(&project_root, &config.workspace, &project_root)?;
    }

    Ok(())
}

fn permissions_dir() -> PathBuf {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("permissions")
}

fn allowed_commands_path() -> PathBuf {
    permissions_dir().join("allowed_commands.json")
}

fn load_allowed_commands() -> Result<BTreeSet<String>, String> {
    let path = allowed_commands_path();

    if !path.exists() {
        return Ok(BTreeSet::new());
    }

    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", display_path(&path), e))?;

    if content.trim().is_empty() {
        return Ok(BTreeSet::new());
    }

    let commands = serde_json::from_str::<Vec<String>>(&content)
        .map_err(|e| format!("Failed to parse {}: {}", display_path(&path), e))?;

    Ok(commands
        .into_iter()
        .map(|command| command.trim().to_string())
        .filter(|command| !command.is_empty())
        .collect())
}

fn save_allowed_commands(commands: &BTreeSet<String>) -> Result<(), String> {
    let dir = permissions_dir();
    fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "Failed to create permissions directory {}: {}",
            display_path(&dir),
            e
        )
    })?;

    let path = allowed_commands_path();
    let ordered = commands.iter().cloned().collect::<Vec<_>>();
    let content = serde_json::to_string_pretty(&ordered)
        .map_err(|e| format!("Failed to serialize allowed commands: {}", e))?;

    fs::write(&path, content).map_err(|e| format!("Failed to write {}: {}", display_path(&path), e))
}

fn normalize_command(command: &str) -> String {
    command.trim().to_string()
}

fn allow_destructive_actions() -> bool {
    env::var("ALLOW_DESTRUCTIVE_ACTIONS")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn command_targets_protected_path(normalized_command: &str, project_root: &Path) -> bool {
    let sensitive_targets = [
        "windows",
        "system32",
        "c:/users",
        "c:\\users",
        "%userprofile%",
        "$env:userprofile",
        ".env",
        ".git",
        "memory",
        "sandbox",
    ];

    let project_root_text = project_root
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();

    sensitive_targets
        .iter()
        .any(|target| normalized_command.contains(target))
        || normalized_command.contains(&project_root_text)
        || normalized_command.contains("/trust")
        || normalized_command.contains("\\trust")
}

fn destructive_command_reason(command: &str, project_root: &Path) -> Option<String> {
    let normalized = command.trim().to_lowercase();

    let destructive_patterns = [
        "shutdown",
        "restart",
        "logoff",
        "taskkill",
        "format",
        "diskpart",
        "bcdedit",
        "reg delete",
        "reg add",
        "takeown",
        "icacls",
        "del /s",
        "rmdir /s",
        "rd /s",
        "rm -rf",
        "remove-item -recurse",
        "powershell -enc",
    ];

    if let Some(pattern) = destructive_patterns
        .iter()
        .find(|pattern| normalized.contains(**pattern))
    {
        return Some(format!("matched destructive pattern: {}", pattern));
    }

    if normalized.contains("curl") && normalized.contains("| powershell") {
        return Some("matched destructive pattern: curl ... | powershell".to_string());
    }

    if normalized.contains("irm") && normalized.contains("| iex") {
        return Some("matched destructive pattern: irm ... | iex".to_string());
    }

    if command_targets_protected_path(&normalized, project_root) {
        return Some("targets a protected system or project path".to_string());
    }

    None
}

fn blocked_command_reason(command: &str) -> Option<String> {
    if command.trim().is_empty() {
        Some("empty command".to_string())
    } else {
        None
    }
}

fn classify_command_risk(command: &str, project_root: &Path) -> Result<CommandRisk, String> {
    if blocked_command_reason(command).is_some() {
        return Ok(CommandRisk::Blocked);
    }

    if destructive_command_reason(command, project_root).is_some() {
        return Ok(CommandRisk::Destructive);
    }

    let normalized = normalize_command(command);
    let allowed_commands = load_allowed_commands()?;

    if allowed_commands.contains(&normalized) {
        Ok(CommandRisk::Safe)
    } else {
        Ok(CommandRisk::NeedsApproval)
    }
}

fn persist_allowed_command(command: &str, project_root: &Path) -> Result<(), String> {
    if destructive_command_reason(command, project_root).is_some() {
        return Err("Destructive commands can never be saved to allowed_commands.json".to_string());
    }

    let normalized = normalize_command(command);
    let mut allowed_commands = load_allowed_commands()?;
    allowed_commands.insert(normalized);
    save_allowed_commands(&allowed_commands)
}

fn execute_command_with_timeout(
    sandbox: &SandboxConfig,
    program: &str,
    args: &[&str],
    display_name: &str,
) -> String {
    let cargo_target_dir = sandbox.temp.join("target");
    if let Err(e) = fs::create_dir_all(&cargo_target_dir) {
        return format!(
            "Command failed: {}\n[stderr]\nFailed to prepare sandbox target directory {}: {}",
            display_name,
            display_path(&cargo_target_dir),
            e
        );
    }

    let mut child = match Command::new(program)
        .args(args)
        .current_dir(&sandbox.workspace)
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return format!("Command failed: {}\n[stderr]\n{}", display_name, e),
    };

    let start = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return match child.wait_with_output() {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                        if output.status.success() {
                            if stdout.is_empty() {
                                format!("Executed command: {}\nStatus: success", display_name)
                            } else {
                                format!(
                                    "Executed command: {}\nStatus: success\n[stdout]\n{}",
                                    display_name, stdout
                                )
                            }
                        } else if stdout.is_empty() && stderr.is_empty() {
                            format!(
                                "Command failed: {}\nStatus: {}",
                                display_name, output.status
                            )
                        } else if stdout.is_empty() {
                            format!("Command failed: {}\n[stderr]\n{}", display_name, stderr)
                        } else if stderr.is_empty() {
                            format!("Command failed: {}\n[stdout]\n{}", display_name, stdout)
                        } else {
                            format!(
                                "Command failed: {}\n[stdout]\n{}\n\n[stderr]\n{}",
                                display_name, stdout, stderr
                            )
                        }
                    }
                    Err(e) => format!(
                        "Command failed: {}\n[stderr]\nFailed to collect output: {}",
                        display_name, e
                    ),
                };
            }
            Ok(None) => {
                if start.elapsed() >= Duration::from_millis(sandbox.command_timeout_ms) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return format!(
                        "Command timed out: {}\nTimeout: {} ms",
                        display_name, sandbox.command_timeout_ms
                    );
                }

                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return format!(
                    "Command failed: {}\n[stderr]\nFailed while waiting on command: {}",
                    display_name, e
                );
            }
        }
    }
}

async fn request_permission(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    title: &str,
    command: &str,
    risk_label: &str,
    allow_always: bool,
) -> PermissionChoice {
    let (response_tx, response_rx) = oneshot::channel();
    let pending = PendingApproval {
        title: title.to_string(),
        command: command.to_string(),
        risk_label: risk_label.to_string(),
        allow_always,
        responder: response_tx,
    };

    if event_tx
        .send(RuntimeEvent::ApprovalRequested(pending))
        .is_err()
    {
        return PermissionChoice::Decline;
    }

    response_rx.await.unwrap_or(PermissionChoice::Decline)
}

async fn run_command(
    sandbox: &SandboxConfig,
    command: &str,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> String {
    let normalized = normalize_command(command);
    if normalized.is_empty() {
        return "Blocked command: empty command".to_string();
    }

    let project_root = match env::current_dir() {
        Ok(path) => path,
        Err(e) => return format!("Blocked command: failed to resolve project root: {}", e),
    };

    match classify_command_risk(&normalized, &project_root) {
        Ok(CommandRisk::Blocked) => {
            let reason = blocked_command_reason(&normalized)
                .unwrap_or_else(|| "blocked by runtime policy".to_string());
            format!("Blocked command: {} ({})", normalized, reason)
        }
        Ok(CommandRisk::Safe) => {
            execute_command_with_timeout(sandbox, "cmd", &["/C", normalized.as_str()], &normalized)
        }
        Ok(CommandRisk::NeedsApproval) => {
            let choice = request_permission(
                event_tx,
                "Command requested",
                &normalized,
                "NeedsApproval",
                true,
            )
            .await;

            match choice {
                PermissionChoice::AllowOnce => execute_command_with_timeout(
                    sandbox,
                    "cmd",
                    &["/C", normalized.as_str()],
                    &normalized,
                ),
                PermissionChoice::AllowAlways => {
                    match persist_allowed_command(&normalized, &project_root) {
                        Ok(()) => execute_command_with_timeout(
                            sandbox,
                            "cmd",
                            &["/C", normalized.as_str()],
                            &normalized,
                        ),
                        Err(error) => {
                            format!("Blocked command: failed to persist approval ({})", error)
                        }
                    }
                }
                PermissionChoice::Decline => format!("User declined command: {}", normalized),
            }
        }
        Ok(CommandRisk::Destructive) => {
            let reason = destructive_command_reason(&normalized, &project_root)
                .unwrap_or_else(|| "matched destructive runtime policy".to_string());

            if !allow_destructive_actions() {
                return format!("Blocked destructive command: {} ({})", normalized, reason);
            }

            let choice = request_permission(
                event_tx,
                "Destructive command requested",
                &normalized,
                "Destructive",
                false,
            )
            .await;

            match choice {
                PermissionChoice::AllowOnce => execute_command_with_timeout(
                    sandbox,
                    "cmd",
                    &["/C", normalized.as_str()],
                    &normalized,
                ),
                PermissionChoice::AllowAlways | PermissionChoice::Decline => {
                    format!("User declined command: {}", normalized)
                }
            }
        }
        Err(error) => format!(
            "Blocked command: failed to classify command risk ({})",
            error
        ),
    }
}

fn detect_provider(base_url: &str) -> Provider {
    let normalized = base_url.trim().to_lowercase();

    if normalized.contains("anthropic") || normalized.ends_with("/v1/messages") {
        Provider::AnthropicCompatible
    } else {
        Provider::OpenAiCompatible
    }
}

fn build_request_url(base_url: &str, provider: Provider) -> String {
    let normalized = base_url.trim().trim_end_matches('/');

    if normalized.ends_with("/v1/chat/completions")
        || normalized.ends_with("/chat/completions")
        || normalized.ends_with("/v1/messages")
    {
        normalized.to_string()
    } else {
        match provider {
            Provider::OpenAiCompatible => format!("{}/v1/chat/completions", normalized),
            Provider::AnthropicCompatible => format!("{}/v1/messages", normalized),
        }
    }
}

fn build_headers(api_key: &str, provider: Provider) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    match provider {
        Provider::OpenAiCompatible => {
            let value = format!("Bearer {}", api_key);
            let header = HeaderValue::from_str(&value)
                .map_err(|e| format!("Invalid API key for Authorization header: {}", e))?;
            headers.insert(AUTHORIZATION, header);
        }
        Provider::AnthropicCompatible => {
            let api_key_header = HeaderValue::from_str(api_key)
                .map_err(|e| format!("Invalid API key for x-api-key header: {}", e))?;
            headers.insert("x-api-key", api_key_header);
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
    }

    Ok(headers)
}

fn build_request_body(model: &str, history: &[Message], provider: Provider) -> serde_json::Value {
    match provider {
        Provider::OpenAiCompatible => json!({
            "model": model,
            "messages": history,
            "stream": true
        }),
        Provider::AnthropicCompatible => {
            let system = history
                .iter()
                .filter(|message| message.role == "system")
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");

            let messages = history
                .iter()
                .filter(|message| message.role != "system")
                .map(|message| {
                    let role = match message.role.as_str() {
                        "assistant" => "assistant",
                        _ => "user",
                    };

                    json!({
                        "role": role,
                        "content": message.content,
                    })
                })
                .collect::<Vec<_>>();

            json!({
                "model": model,
                "system": system,
                "messages": messages,
                "stream": true,
                "max_tokens": 4096
            })
        }
    }
}

fn extract_stream_text(data: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(data).unwrap_or_default();

    if let Some(content) = parsed["choices"][0]["delta"]["content"].as_str() {
        return content.to_string();
    }

    if let Some(content) = parsed["delta"]["text"].as_str() {
        return content.to_string();
    }

    String::new()
}

async fn request_model_reply(
    history: &[Message],
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> Result<String, String> {
    let api_key = env::var("API_KEY").unwrap_or_default();
    let base_url = env::var("BASE_URL").unwrap_or_default();
    let model = env::var("MODEL").unwrap_or_default();

    if api_key.trim().is_empty() {
        return Err(
            "Missing API_KEY. Add it to your .env file or environment variables.".to_string(),
        );
    }

    if base_url.trim().is_empty() {
        return Err("Missing BASE_URL. Example: BASE_URL=https://api.openai.com".to_string());
    }

    if model.trim().is_empty() {
        return Err("Missing MODEL. Example: MODEL=gpt-4o-mini".to_string());
    }

    let provider = detect_provider(&base_url);
    let url = build_request_url(&base_url, provider);
    let client = Client::new();
    let body = build_request_body(&model, history, provider);
    let headers = build_headers(&api_key, provider)?;

    let response = client.post(&url).headers(headers).json(&body).send().await;

    match response {
        Ok(res) => {
            let status = res.status();

            if !status.is_success() {
                let error_body = res
                    .text()
                    .await
                    .unwrap_or_else(|_| "<failed to read error response body>".to_string());
                return Err(format!(
                    "API error status: {}\nAPI error body: {}\nRequest URL: {}\nDetected Provider: {:?}",
                    status, error_body, url, provider
                ));
            }

            let mut stream = res.bytes_stream();
            let mut full_message = String::new();
            let _ = event_tx.send(RuntimeEvent::StartAssistant);

            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => {
                        let text = String::from_utf8_lossy(&chunk);

                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let data = line.replace("data: ", "");

                                if data == "[DONE]" {
                                    break;
                                }

                                let content = extract_stream_text(&data);
                                if !content.is_empty() {
                                    full_message.push_str(&content);
                                    let _ = event_tx.send(RuntimeEvent::AssistantChunk(content));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        return Err(format!("Stream error: {}", error));
                    }
                }
            }

            Ok(full_message)
        }
        Err(error) => {
            if error.is_builder() {
                Err(format!(
                    "Request error: {}\nThis usually means BASE_URL is not a valid absolute URL. Current request URL: {}",
                    error, url
                ))
            } else {
                Err(format!("Request error: {}", error))
            }
        }
    }
}

fn response_claims_destructive_action(response: &str) -> bool {
    let normalized = response.trim().to_lowercase();

    let dangerous_claims = [
        "shutting down",
        "shutting the pc down",
        "powering off",
        "restarting now",
        "rebooting now",
        "formatting",
        "wiping",
        "deleting system32",
    ];

    dangerous_claims
        .iter()
        .any(|claim| normalized.contains(claim))
}

fn allowed_sandbox_command(command: &str) -> Option<AllowedCommand> {
    let normalized = command.trim().to_lowercase();

    match normalized.as_str() {
        "cargo check" => Some(AllowedCommand {
            display: "cargo check",
            program: "cargo",
            args: &["check"],
        }),
        "cargo test" => Some(AllowedCommand {
            display: "cargo test",
            program: "cargo",
            args: &["test"],
        }),
        "cargo fmt --check" => Some(AllowedCommand {
            display: "cargo fmt --check",
            program: "cargo",
            args: &["fmt", "--check"],
        }),
        "cargo clippy" => Some(AllowedCommand {
            display: "cargo clippy",
            program: "cargo",
            args: &["clippy"],
        }),
        _ => None,
    }
}

async fn run_sandboxed_command(
    config: &SandboxConfig,
    command: &str,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> String {
    let Some(allowed_command) = allowed_sandbox_command(command) else {
        return format!(
            "Blocked command: {}. Allowed sandbox commands: cargo check, cargo test, cargo fmt --check, cargo clippy",
            command
        );
    };

    let choice = request_permission(
        event_tx,
        "Sandboxed command requested",
        allowed_command.display,
        "NeedsApproval",
        false,
    )
    .await;

    match choice {
        PermissionChoice::AllowOnce => execute_command_with_timeout(
            config,
            allowed_command.program,
            allowed_command.args,
            allowed_command.display,
        ),
        PermissionChoice::AllowAlways | PermissionChoice::Decline => {
            format!("User declined command: {}", allowed_command.display)
        }
    }
}

fn open_app(app: &str, url: Option<String>) -> String {
    let normalized_app = app.trim().to_lowercase();

    let executable = match normalized_app.as_str() {
        "chrome" | "google chrome" => "chrome",
        "edge" | "microsoft edge" => "msedge",
        "firefox" => "firefox",
        "notepad" => "notepad",
        "calculator" | "calc" => "calc",
        "explorer" | "file explorer" => "explorer",
        "paint" | "mspaint" => "mspaint",
        "wordpad" => "write",
        "vscode" | "vs code" | "code" => "code",
        _ => {
            return format!(
                "Blocked app: {}. Allowed apps: chrome, edge, firefox, notepad, calculator, explorer, paint, wordpad, vscode",
                app
            );
        }
    };

    if let Some(url) = url {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return "Blocked unsafe URL. Only http:// and https:// URLs are allowed.".to_string();
        }

        match Command::new("cmd")
            .args(["/C", "start", "", executable, &url])
            .spawn()
        {
            Ok(_) => format!("Opened {} with URL: {}", app, url),
            Err(e) => format!("Failed to open {}: {}", app, e),
        }
    } else {
        match Command::new("cmd")
            .args(["/C", "start", "", executable])
            .spawn()
        {
            Ok(_) => format!("Opened {}", app),
            Err(e) => format!("Failed to open {}: {}", app, e),
        }
    }
}

fn list_runtime_directory(config: &SandboxConfig, requested_path: &str) -> String {
    let path = match resolve_sandbox_path(config, requested_path) {
        Ok(path) => path,
        Err(error) => return error,
    };

    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) => return format!("Failed to list directory {}: {}", requested_path, e),
    };

    let mut names = Vec::new();

    for entry in entries {
        match entry {
            Ok(entry) => {
                let name = entry.file_name().to_string_lossy().to_string();
                let suffix = if entry.path().is_dir() { "/" } else { "" };
                names.push(format!("{}{}", name, suffix));
            }
            Err(e) => {
                return format!(
                    "Failed to read directory entry in {}: {}",
                    requested_path, e
                );
            }
        }
    }

    names.sort();

    if names.is_empty() {
        format!("Directory is empty: {}", requested_path)
    } else {
        format!("{}\n{}", requested_path, names.join("\n"))
    }
}

fn read_runtime_file(config: &SandboxConfig, requested_path: &str) -> String {
    let path = match resolve_sandbox_path(config, requested_path) {
        Ok(path) => path,
        Err(error) => return error,
    };

    if path.is_dir() {
        return format!("{} is a directory, not a file", requested_path);
    }

    match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) => format!("Failed to read file {}: {}", requested_path, e),
    }
}

async fn run_tool(
    tool_call: ToolCall,
    sandbox: &SandboxConfig,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> String {
    match tool_call.tool.as_str() {
        "write_file" => {
            let Some(path) = tool_call.args.path else {
                return "Missing path for write_file".to_string();
            };

            let resolved_path = match resolve_sandbox_path(sandbox, &path) {
                Ok(path) => path,
                Err(error) => return error,
            };

            if let Some(parent) = resolved_path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return format!("Failed to create parent directory for {}: {}", path, e);
                }
            }

            let content = tool_call.args.content.unwrap_or_default();

            match fs::write(&resolved_path, content) {
                Ok(_) => format!("Saved file: {}", path),
                Err(e) => format!("Failed to save file {}: {}", path, e),
            }
        }
        "read_file" => {
            let Some(path) = tool_call.args.path else {
                return "Missing path for read_file".to_string();
            };

            read_runtime_file(sandbox, &path)
        }
        "list_directory" => {
            let Some(path) = tool_call.args.path else {
                return "Missing path for list_directory".to_string();
            };

            list_runtime_directory(sandbox, &path)
        }
        "open_chrome" => open_app("chrome", tool_call.args.url),
        "open_app" => {
            let Some(app) = tool_call.args.app else {
                return "Missing app for open_app".to_string();
            };

            open_app(&app, tool_call.args.url)
        }
        "run_sandboxed_command" => {
            let Some(command) = tool_call.args.command else {
                return "Missing command for run_sandboxed_command".to_string();
            };

            run_sandboxed_command(sandbox, &command, event_tx).await
        }
        "run_command" => {
            let Some(command) = tool_call.args.command else {
                return "Missing command for run_command".to_string();
            };

            run_command(sandbox, &command, event_tx).await
        }
        _ => format!("Unknown tool: {}", tool_call.tool),
    }
}

async fn process_turn(
    current_chat: String,
    mut history: Vec<Message>,
    sandbox: SandboxConfig,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
) {
    let _ = event_tx.send(RuntimeEvent::Status("Waiting for model...".to_string()));

    for step in 0..MAX_AGENT_STEPS {
        let full_message = match request_model_reply(&history, &event_tx).await {
            Ok(message) => message,
            Err(error) => {
                let _ = event_tx.send(RuntimeEvent::Error(error));
                return;
            }
        };

        if let Ok(tool_call) = serde_json::from_str::<ToolCall>(&full_message) {
            history.push(Message {
                role: "assistant".to_string(),
                content: full_message,
            });

            let _ = event_tx.send(RuntimeEvent::DiscardAssistantDraft);
            let _ = event_tx.send(RuntimeEvent::Status(format!(
                "Running tool: {}",
                tool_call.tool
            )));

            let tool_result = run_tool(tool_call, &sandbox, &event_tx).await;
            let _ = event_tx.send(RuntimeEvent::ToolResult(tool_result.clone()));

            history.push(Message {
                role: "user".to_string(),
                content: format!("{}{}", TOOL_RESULT_PREFIX, tool_result),
            });

            if step + 1 < MAX_AGENT_STEPS {
                continue;
            }

            let stop_message =
                "Stopped after reaching the max autonomous tool steps for one turn.".to_string();
            history.push(Message {
                role: "assistant".to_string(),
                content: stop_message.clone(),
            });
            let _ = event_tx.send(RuntimeEvent::CommitAssistant(stop_message));
            break;
        }

        let final_message = if response_claims_destructive_action(&full_message) {
            "Blocked destructive command: assistant claimed execution without a runtime tool result"
                .to_string()
        } else {
            full_message
        };

        history.push(Message {
            role: "assistant".to_string(),
            content: final_message.clone(),
        });
        let _ = event_tx.send(RuntimeEvent::CommitAssistant(final_message));
        break;
    }

    save_history(&current_chat, &history);
    let _ = event_tx.send(RuntimeEvent::TurnFinished(history));
}

fn system_prompt() -> String {
    r#"
You are TRUST.

You are a helpful terminal AI with safe agentic control.
Use tools only when the user clearly asks for an action on the computer, files, apps, or sandbox.
For normal conversation, answer normally and do not call tools.
You may use multiple tools across multiple steps in a single turn.
When using a tool, reply ONLY with JSON and no extra text.
After a tool runs, you will receive a follow-up message that starts with "Tool result from TRUST runtime:".
Use that result to decide your next step or produce a final answer.

You operate with controlled autonomy inside a sandbox.
File tools resolve only inside the sandbox directories: workspace/, outputs/, and temp/.
If a path does not start with one of those prefixes, it is treated as relative to workspace/.

Example write_file tool call:
{
  "type": "tool_call",
  "tool": "write_file",
  "args": {
    "path": "outputs/test.md",
    "content": "Hello world"
  }
}

Example run_command tool call:
{
  "type": "tool_call",
  "tool": "run_command",
  "args": {
    "command": "cargo check"
  }
}

Allowed tools:
- write_file: reads and writes only inside sandboxed workspace/, outputs/, or temp/
- read_file: only reads inside sandboxed workspace/, outputs/, or temp/
- list_directory: only lists inside sandboxed workspace/, outputs/, or temp/
- open_app: opens allowed apps only. Allowed apps: chrome, edge, firefox, notepad, calculator, explorer, paint, wordpad, vscode
- open_chrome: alias for opening Chrome
- run_sandboxed_command: runs allowlisted project commands only inside sandbox/workspace after user approval. Allowed commands: cargo check, cargo test, cargo fmt --check, cargo clippy
- run_command: may request broader shell commands inside sandbox/workspace. The runtime decides whether to execute the command, whether approval is required, and whether the command is blocked.

Safety rules:
- Do not claim you cannot interact with the computer when an allowed tool can do the task.
- Do not read or write outside the sandbox, modify system settings, install software, download files, or access private data such as .env files.
- You may request run_command when needed, but the runtime decides whether it executes.
- Destructive commands may be blocked unless ALLOW_DESTRUCTIVE_ACTIONS=true and the user explicitly approves them.
- If the runtime blocks a command, explain that the runtime blocked it.
- Never claim a command ran unless the runtime returns a tool result confirming it ran.
- Never pretend to save files, read files, list directories, open apps, or run commands. Use a tool when available.
- If a tool is useful, prefer actually using it over just describing what would happen.
- Only use JSON when calling tools.
"#
    .to_string()
}

fn ensure_system_message(mut history: Vec<Message>) -> Vec<Message> {
    let has_system = history.iter().any(|message| message.role == "system");
    if !has_system {
        history.insert(
            0,
            Message {
                role: "system".to_string(),
                content: system_prompt(),
            },
        );
    }
    history
}

fn chat_path(chat_name: &str) -> String {
    format!("memory/{}.json", chat_name)
}

fn save_history(chat_name: &str, history: &[Message]) {
    let _ = fs::create_dir_all("memory");
    if let Ok(json) = serde_json::to_string_pretty(history) {
        let _ = fs::write(chat_path(chat_name), json);
    }
}

fn load_history(chat_name: &str) -> Vec<Message> {
    let data = fs::read_to_string(chat_path(chat_name));

    match data {
        Ok(content) => serde_json::from_str::<Vec<Message>>(&content)
            .unwrap_or_else(|_| Vec::new())
            .into_iter()
            .map(|message| {
                if message.role == "tool" {
                    Message {
                        role: "user".to_string(),
                        content: format!("{}{}", TOOL_RESULT_PREFIX, message.content),
                    }
                } else {
                    message
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn list_chat_names() -> Vec<String> {
    let paths = match fs::read_dir("memory") {
        Ok(paths) => paths,
        Err(_) => return vec!["default".to_string()],
    };

    let mut names = paths
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            file_name.strip_suffix(".json").map(|name| name.to_string())
        })
        .collect::<Vec<_>>();

    names.sort();
    names.dedup();

    if names.is_empty() {
        names.push("default".to_string());
    }

    names
}

fn delete_chat(chat_name: &str) -> Result<(), String> {
    fs::remove_file(chat_path(chat_name)).map_err(|_| format!("Chat not found: {}", chat_name))
}

fn is_tool_call_json(content: &str) -> bool {
    serde_json::from_str::<ToolCall>(content).is_ok()
}

fn ui_messages_from_history(history: &[Message]) -> Vec<UiMessage> {
    history
        .iter()
        .filter_map(|message| match message.role.as_str() {
            "system" => None,
            "assistant" if is_tool_call_json(&message.content) => None,
            "assistant" => Some(UiMessage {
                role: UiMessageRole::Assistant,
                content: message.content.clone(),
            }),
            "user" => {
                if let Some(content) = message.content.strip_prefix(TOOL_RESULT_PREFIX) {
                    Some(UiMessage {
                        role: UiMessageRole::Tool,
                        content: content.to_string(),
                    })
                } else {
                    Some(UiMessage {
                        role: UiMessageRole::User,
                        content: message.content.clone(),
                    })
                }
            }
            _ => Some(UiMessage {
                role: UiMessageRole::Info,
                content: message.content.clone(),
            }),
        })
        .collect()
}

fn build_transcript(app: &App) -> Text<'static> {
    let mut lines = Vec::new();

    for message in &app.visible_messages {
        let (label, color) = match message.role {
            UiMessageRole::User => ("You", Color::Cyan),
            UiMessageRole::Assistant => ("TRUST", Color::Green),
            UiMessageRole::Tool => ("Tool", Color::Magenta),
            UiMessageRole::Info => ("Info", Color::Yellow),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{}: ", label),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(message.content.clone()),
        ]));
        lines.push(Line::raw(""));
    }

    if !app.draft_assistant.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "TRUST: ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(app.draft_assistant.clone()),
        ]));
    }

    Text::from(lines)
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let root = frame.area();
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(40)])
        .split(root);

    let sidebar_area = horizontal[0];
    let main_area = horizontal[1];

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(4),
        ])
        .split(main_area);

    let chats = list_chat_names();
    let items = chats
        .iter()
        .map(|chat| {
            let style = if chat == &app.current_chat {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(chat.clone()).style(style)
        })
        .collect::<Vec<_>>();

    let chats_list = List::new(items).block(
        Block::default()
            .title("Chats")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue)),
    );
    frame.render_widget(chats_list, sidebar_area);

    let transcript = Paragraph::new(build_transcript(app))
        .block(
            Block::default()
                .title("Conversation")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, vertical[0]);

    let status = Paragraph::new(app.status.clone()).block(
        Block::default()
            .title(format!(
                "Status · chat={} · busy={} · sandbox={}",
                app.current_chat,
                if app.busy { "yes" } else { "no" },
                display_path(&app.sandbox.workspace)
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(status, vertical[1]);

    let input = Paragraph::new(app.input.clone())
        .block(
            Block::default()
                .title("Input")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(input, vertical[2]);

    let cursor_x = vertical[2].x + 1 + app.input.chars().count() as u16;
    let cursor_y = vertical[2].y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));

    if let Some(pending) = &app.pending_approval {
        let popup = centered_rect(70, if pending.allow_always { 40 } else { 35 }, root);
        frame.render_widget(Clear, popup);

        let mut lines = vec![
            Line::from(vec![Span::styled(
                format!("{}: {}", pending.title, pending.command),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Risk: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(pending.risk_label.clone()),
            ]),
            Line::raw(""),
            Line::raw("[A] Allow once"),
        ];

        if pending.allow_always {
            lines.push(Line::raw("[L] Allow always"));
        }
        lines.push(Line::raw("[D] Decline"));

        let help = if pending.allow_always {
            "Choose A, L, or D"
        } else {
            "Choose A or D"
        };
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            help,
            Style::default().fg(Color::Yellow),
        )));

        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .title("Permission Required")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, popup);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn credits_text() -> String {
    [
        "TRUST — Terminal Runtime for Unified Smart Tasks",
        "Developer: Daniel Santhosh",
        "Repository: https://github.com/danielzanthosh/trust",
    ]
    .join("\n")
}

fn handle_local_command(app: &mut App, input: &str) -> bool {
    if input == "/exit" || input == "/quit" {
        app.should_quit = true;
        return true;
    }

    if input == "/list" {
        let chats = list_chat_names();
        app.add_info_message(format!("Saved chats:\n{}", chats.join("\n")));
        app.status = "Listed chats".to_string();
        return true;
    }

    if input == "/credits" {
        app.add_info_message(credits_text());
        app.status = "Displayed credits".to_string();
        return true;
    }

    if input == "/clear" {
        app.visible_messages.clear();
        app.model_history = ensure_system_message(Vec::new());
        save_history(&app.current_chat, &app.model_history);
        app.status = format!("Cleared chat: {}", app.current_chat);
        return true;
    }

    if let Some(chat_name) = input.strip_prefix("/chat ") {
        let chat_name = chat_name.trim();
        if chat_name.is_empty() {
            app.add_info_message("Usage: /chat <name>");
            return true;
        }
        app.set_chat(chat_name.to_string());
        return true;
    }

    if let Some(chat_name) = input.strip_prefix("/delete ") {
        let chat_name = chat_name.trim();
        if chat_name.is_empty() {
            app.add_info_message("Usage: /delete <name>");
            return true;
        }

        match delete_chat(chat_name) {
            Ok(()) => {
                app.status = format!("Deleted chat: {}", chat_name);
                if chat_name == app.current_chat {
                    app.set_chat("default".to_string());
                }
            }
            Err(error) => {
                app.status = error.clone();
                app.add_info_message(error);
            }
        }
        return true;
    }

    false
}

fn submit_current_input(app: &mut App, event_tx: &mpsc::UnboundedSender<RuntimeEvent>) {
    let input = app.input.trim().to_string();
    if input.is_empty() || app.busy {
        return;
    }

    if handle_local_command(app, &input) {
        app.input.clear();
        return;
    }

    app.add_user_message(input.clone());
    app.model_history.push(Message {
        role: "user".to_string(),
        content: input,
    });
    save_history(&app.current_chat, &app.model_history);

    app.input.clear();
    app.busy = true;
    app.status = "Submitting prompt...".to_string();
    app.draft_assistant.clear();

    let current_chat = app.current_chat.clone();
    let history = app.model_history.clone();
    let sandbox = app.sandbox.clone();
    let runtime_tx = event_tx.clone();

    tokio::spawn(async move {
        process_turn(current_chat, history, sandbox, runtime_tx).await;
    });
}

fn handle_key_event(app: &mut App, key: KeyEvent, event_tx: &mpsc::UnboundedSender<RuntimeEvent>) {
    if let Some(pending) = app.pending_approval.take() {
        let choice = match key.code {
            KeyCode::Char('a') | KeyCode::Char('A') => Some(PermissionChoice::AllowOnce),
            KeyCode::Char('l') | KeyCode::Char('L') if pending.allow_always => {
                Some(PermissionChoice::AllowAlways)
            }
            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Esc => {
                Some(PermissionChoice::Decline)
            }
            _ => None,
        };

        if let Some(choice) = choice {
            let _ = pending.responder.send(choice);
            app.status = format!("Resolved approval for: {}", pending.command);
        } else {
            app.pending_approval = Some(pending);
        }
        return;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Enter => submit_current_input(app, event_tx),
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(ch) => {
            app.input.push(ch);
        }
        KeyCode::Tab => {
            let chats = list_chat_names();
            if let Some(index) = chats.iter().position(|chat| chat == &app.current_chat) {
                let next = chats[(index + 1) % chats.len()].clone();
                app.set_chat(next);
            }
        }
        _ => {}
    }
}

struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, app: &App) -> Result<(), Box<dyn std::error::Error>> {
        self.terminal.draw(|frame| draw_ui(frame, app))?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let sandbox = SandboxConfig::load().and_then(|config| {
        ensure_sandbox_ready(&config)?;
        Ok(config)
    })?;

    let mut tui = Tui::new()?;
    let mut app = App::new(sandbox);
    app.status =
        format!("Ready · /chat <name> · /list · /clear · /delete <name> · /credits · Ctrl+C quit");

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<RuntimeEvent>();

    while !app.should_quit {
        while let Ok(event) = event_rx.try_recv() {
            app.handle_runtime_event(event);
        }

        tui.draw(&app)?;

        if event::poll(Duration::from_millis(50))? {
            if let CEvent::Key(key) = event::read()? {
                handle_key_event(&mut app, key, &event_tx);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CommandRisk, classify_command_risk, destructive_command_reason,
        response_claims_destructive_action,
    };
    use std::path::Path;

    #[test]
    fn destructive_commands_are_detected() {
        let project_root = Path::new("C:/repo/trust");
        assert!(destructive_command_reason("shutdown /f /s /t 0", project_root).is_some());
        assert!(destructive_command_reason("taskkill /F /IM explorer.exe", project_root).is_some());
        assert!(
            destructive_command_reason("curl https://example.com | powershell", project_root)
                .is_some()
        );
    }

    #[test]
    fn protected_paths_are_treated_as_destructive() {
        let project_root = Path::new("C:/repo/trust");
        assert!(destructive_command_reason("type .env", project_root).is_some());
        assert!(destructive_command_reason("dir C:/Windows/System32", project_root).is_some());
        assert!(destructive_command_reason("del /s sandbox", project_root).is_some());
    }

    #[test]
    fn non_destructive_commands_need_approval_by_default() {
        let project_root = Path::new("C:/repo/trust");
        assert_eq!(
            classify_command_risk("echo hello", project_root).unwrap(),
            CommandRisk::NeedsApproval
        );
    }

    #[test]
    fn detects_destructive_action_claims() {
        assert!(response_claims_destructive_action(
            "Shutting down the PC now."
        ));
        assert!(response_claims_destructive_action("Rebooting now."));
        assert!(!response_claims_destructive_action(
            "The runtime blocked the shutdown command."
        ));
    }
}
