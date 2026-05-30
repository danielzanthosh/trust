// TRUST
// Terminal Runtime for Unified Smart Tasks
// Sandboxed terminal AI runtime with a ratatui/crossterm TUI, streaming chat, tool execution, and permission-gated command execution.

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        size as terminal_size,
    },
};
use dotenvy::dotenv;
use futures_util::StreamExt;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

const MAX_AGENT_STEPS: usize = 5;
const DEFAULT_SANDBOX_DIR: &str = "sandbox/workspace";
const DEFAULT_SANDBOX_COMMAND_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_TOKENS: u64 = 1024;
const TOOL_RESULT_PREFIX: &str = "Tool result from TRUST runtime:";
const WEBBRIDGE_COMMAND_URL: &str = "http://127.0.0.1:10086/command";
const MODEL_STOP_SEQUENCES: [&str; 4] = [
    "Tool result from TRUST runtime:",
    "\nTool result from TRUST runtime:",
    "Runtime output:",
    "Tool Execution Success:",
];

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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct ToolArgs {
    path: Option<String>,
    content: Option<String>,
    url: Option<String>,
    app: Option<String>,
    command: Option<String>,
    action: Option<String>,
    selector: Option<String>,
    value: Option<String>,
    session: Option<String>,
    new_tab: Option<bool>,
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
enum CommandShell {
    PowerShell,
}

impl CommandShell {
    fn label(self) -> &'static str {
        match self {
            CommandShell::PowerShell => "PowerShell",
        }
    }

    fn program(self) -> &'static str {
        match self {
            CommandShell::PowerShell if cfg!(windows) => "powershell.exe",
            CommandShell::PowerShell => "pwsh",
        }
    }

    fn args(self, command: &str) -> Vec<String> {
        match self {
            CommandShell::PowerShell => vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
        }
    }
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

#[derive(Serialize)]
struct CommandLogEntry {
    timestamp_unix_ms: u128,
    shell: String,
    command: String,
    permission_choice: String,
    result: String,
    blocked: bool,
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
    transcript_scroll: u16,
    auto_scroll: bool,
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
            transcript_scroll: u16::MAX,
            auto_scroll: true,
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
        self.scroll_to_bottom();
    }

    fn scroll_to_bottom(&mut self) {
        self.transcript_scroll = u16::MAX;
        self.auto_scroll = true;
    }

    fn push_visible_message(&mut self, role: UiMessageRole, content: String) {
        if content.trim().is_empty() {
            return;
        }

        self.visible_messages.push(UiMessage { role, content });
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    fn add_info_message(&mut self, content: impl Into<String>) {
        self.push_visible_message(UiMessageRole::Info, content.into());
    }

    fn add_user_message(&mut self, content: String) {
        self.push_visible_message(UiMessageRole::User, content);
    }

    fn add_tool_message(&mut self, content: String) {
        self.push_visible_message(UiMessageRole::Tool, content);
    }

    fn add_assistant_message(&mut self, content: String) {
        self.push_visible_message(UiMessageRole::Assistant, content);
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Status(status) => self.status = status,
            RuntimeEvent::StartAssistant => {
                self.draft_assistant.clear();
                self.busy = true;
            }
            RuntimeEvent::AssistantChunk(chunk) => {
                self.draft_assistant.push_str(&chunk);
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
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

fn seed_sandbox_workspace() -> bool {
    env::var("SEED_SANDBOX_WORKSPACE")
        .map(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )
        })
        .unwrap_or(false)
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

    if seed_sandbox_workspace() {
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

fn command_log_path() -> PathBuf {
    permissions_dir().join("command_log.jsonl")
}

fn current_timestamp_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn append_command_log(
    shell: CommandShell,
    command: &str,
    permission_choice: &str,
    result: &str,
    blocked: bool,
) {
    let dir = permissions_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }

    let entry = CommandLogEntry {
        timestamp_unix_ms: current_timestamp_unix_ms(),
        shell: shell.label().to_string(),
        command: command.to_string(),
        permission_choice: permission_choice.to_string(),
        result: result.to_string(),
        blocked,
    };

    let Ok(mut line) = serde_json::to_string(&entry) else {
        return;
    };
    line.push('\n');

    let path = command_log_path();
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
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

fn command_contains_any(normalized_command: &str, patterns: &[&str]) -> bool {
    patterns
        .iter()
        .any(|pattern| normalized_command.contains(pattern))
}

fn command_uses_destructive_verb(normalized_command: &str) -> bool {
    command_contains_any(
        normalized_command,
        &[
            "remove-item",
            " rm ",
            "rm -",
            "del ",
            "erase ",
            "rmdir",
            "rd /",
            "clear-disk",
            "format",
            "diskpart",
            "bcdedit",
            "reg delete",
            "reg add",
            "set-mppreference",
            "disable-",
            "stop-service",
            "sc delete",
            "takeown",
            "icacls",
        ],
    )
}

fn command_targets_protected_path(normalized_command: &str, project_root: &Path) -> bool {
    let project_root_text = project_root
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();

    command_contains_any(
        normalized_command,
        &[
            "c:\\windows",
            "c:/windows",
            "\\windows\\system32",
            "/windows/system32",
            "system32",
            "boot.ini",
            "bootmgr",
            "bcd",
            "ntldr",
            "sam",
            "security",
            "software",
            "system32\\config",
            "system32/config",
            "hkey_local_machine",
            "hklm",
            "hkey_classes_root",
            "hkcr",
            "%userprofile%",
            "$env:userprofile",
            "c:\\users\\",
            "c:/users/",
        ],
    ) || (!project_root_text.is_empty() && normalized_command.contains(&project_root_text))
}

fn command_targets_entire_drive(normalized_command: &str) -> bool {
    command_contains_any(
        normalized_command,
        &[
            " c:\\ -recurse",
            " c:/ -recurse",
            " c:\\* -recurse",
            " c:/* -recurse",
            " c:\\ -force",
            " c:/ -force",
            " c:\\* -force",
            " c:/* -force",
            " remove-item / -recurse",
            " rm / -rf",
            " rm -rf /",
        ],
    )
}

fn destructive_command_reason(command: &str, project_root: &Path) -> Option<String> {
    let normalized = format!(" {} ", command.trim().to_lowercase());

    if normalized.contains("powershell -enc") || normalized.contains("powershell.exe -enc") {
        return Some("encoded PowerShell can hide destructive actions".to_string());
    }

    if normalized.contains("curl") && normalized.contains("| powershell") {
        return Some("downloaded script is piped directly into PowerShell".to_string());
    }

    if normalized.contains("irm") && normalized.contains("| iex") {
        return Some("downloaded script is executed with Invoke-Expression".to_string());
    }

    if command_contains_any(
        &normalized,
        &[
            "format ",
            "format-volume",
            "clear-disk",
            "initialize-disk",
            "remove-partition",
            "diskpart",
            "bcdedit",
        ],
    ) {
        return Some("may wipe drives or modify boot configuration".to_string());
    }

    if command_contains_any(
        &normalized,
        &[
            "disable-windowsdefender",
            "disable-realtimemonitoring",
            "set-mppreference -disablerealtimemonitoring $true",
            "stop-service windefend",
            "sc stop windefend",
        ],
    ) {
        return Some("may disable security tools".to_string());
    }

    if command_uses_destructive_verb(&normalized) && command_targets_entire_drive(&normalized) {
        return Some("targets an entire drive with a destructive operation".to_string());
    }

    if command_uses_destructive_verb(&normalized)
        && command_targets_protected_path(&normalized, project_root)
    {
        return Some(
            "may damage system files, boot files, registry hives, or user profile data".to_string(),
        );
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

fn command_approval_signature(command: &str) -> Option<String> {
    let lower = command.trim().to_lowercase();
    let first_line = lower.lines().find(|line| !line.trim().is_empty())?.trim();
    let separators = [' ', '\t', '|', ';', '&', '\r', '\n'];
    let first_token = first_line
        .split(separators)
        .find(|token| !token.trim().is_empty())?
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '(' | ')'));

    if first_token.is_empty() || first_token.starts_with('$') {
        return None;
    }

    Some(format!("signature:powershell:{}", first_token))
}

fn command_is_preapproved(command: &str, allowed_commands: &BTreeSet<String>) -> bool {
    let normalized = normalize_command(command);

    allowed_commands.contains(&normalized)
        || command_approval_signature(command)
            .as_ref()
            .is_some_and(|signature| allowed_commands.contains(signature))
}

fn classify_command_risk(command: &str, project_root: &Path) -> Result<CommandRisk, String> {
    if blocked_command_reason(command).is_some() {
        return Ok(CommandRisk::Blocked);
    }

    if destructive_command_reason(command, project_root).is_some() {
        return Ok(CommandRisk::Destructive);
    }

    let allowed_commands = load_allowed_commands()?;

    if command_is_preapproved(command, &allowed_commands) {
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

    if let Some(signature) = command_approval_signature(command) {
        allowed_commands.insert(signature);
    }

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

fn execute_powershell_with_timeout(sandbox: &SandboxConfig, command: &str) -> String {
    let shell = CommandShell::PowerShell;
    let display_name = command.trim();
    let args = shell.args(command);

    let mut child = match Command::new(shell.program())
        .args(&args)
        .current_dir(&sandbox.workspace)
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
                                format!(
                                    "Executed PowerShell command:\n{}\nStatus: success",
                                    display_name
                                )
                            } else {
                                format!(
                                    "Executed PowerShell command:\n{}\nStatus: success\n[stdout]\n{}",
                                    display_name, stdout
                                )
                            }
                        } else if stdout.is_empty() && stderr.is_empty() {
                            format!(
                                "PowerShell command failed:\n{}\nStatus: {}",
                                display_name, output.status
                            )
                        } else if stdout.is_empty() {
                            format!(
                                "PowerShell command failed:\n{}\n[stderr]\n{}",
                                display_name, stderr
                            )
                        } else if stderr.is_empty() {
                            format!(
                                "PowerShell command failed:\n{}\n[stdout]\n{}",
                                display_name, stdout
                            )
                        } else {
                            format!(
                                "PowerShell command failed:\n{}\n[stdout]\n{}\n\n[stderr]\n{}",
                                display_name, stdout, stderr
                            )
                        }
                    }
                    Err(e) => format!(
                        "PowerShell command failed:\n{}\n[stderr]\nFailed to collect output: {}",
                        display_name, e
                    ),
                };
            }
            Ok(None) => {
                if start.elapsed() >= Duration::from_millis(sandbox.command_timeout_ms) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return format!(
                        "PowerShell command timed out:\n{}\nTimeout: {} ms",
                        display_name, sandbox.command_timeout_ms
                    );
                }

                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                return format!(
                    "PowerShell command failed:\n{}\n[stderr]\nFailed while waiting on command: {}",
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
    let shell = CommandShell::PowerShell;
    let normalized = normalize_command(command);
    if normalized.is_empty() {
        let result = "Blocked command: empty command".to_string();
        append_command_log(shell, &normalized, "blocked", &result, true);
        return result;
    }

    let project_root = match env::current_dir() {
        Ok(path) => path,
        Err(e) => {
            let result = format!("Blocked command: failed to resolve project root: {}", e);
            append_command_log(shell, &normalized, "blocked", &result, true);
            return result;
        }
    };

    match classify_command_risk(&normalized, &project_root) {
        Ok(CommandRisk::Blocked) => {
            let reason = blocked_command_reason(&normalized)
                .unwrap_or_else(|| "blocked by runtime policy".to_string());
            let result = format!("Blocked command: {} ({})", normalized, reason);
            append_command_log(shell, &normalized, "blocked", &result, true);
            result
        }
        Ok(CommandRisk::Safe) => {
            let result = execute_powershell_with_timeout(sandbox, &normalized);
            append_command_log(shell, &normalized, "preapproved", &result, false);
            result
        }
        Ok(CommandRisk::NeedsApproval) => {
            let choice = request_permission(
                event_tx,
                "PowerShell command requested",
                &normalized,
                "NeedsApproval",
                true,
            )
            .await;

            match choice {
                PermissionChoice::AllowOnce => {
                    let result = execute_powershell_with_timeout(sandbox, &normalized);
                    append_command_log(shell, &normalized, "allow_once", &result, false);
                    result
                }
                PermissionChoice::AllowAlways => {
                    let result = match persist_allowed_command(&normalized, &project_root) {
                        Ok(()) => execute_powershell_with_timeout(sandbox, &normalized),
                        Err(error) => {
                            format!("Blocked command: failed to persist approval ({})", error)
                        }
                    };
                    let blocked = result.starts_with("Blocked command:");
                    append_command_log(shell, &normalized, "allow_always", &result, blocked);
                    result
                }
                PermissionChoice::Decline => {
                    let result = format!("User declined command: {}", normalized);
                    append_command_log(shell, &normalized, "decline", &result, true);
                    result
                }
            }
        }
        Ok(CommandRisk::Destructive) => {
            let reason = destructive_command_reason(&normalized, &project_root)
                .unwrap_or_else(|| "matched destructive runtime policy".to_string());

            if !allow_destructive_actions() {
                let result = format!(
                    "This command was blocked because it may damage system files.\nReason: {}\nSafer alternative: narrow the command to a non-system path, preview affected items with Get-ChildItem, or ask for a read-only diagnostic command first.\nCommand:\n{}",
                    reason, normalized
                );
                append_command_log(shell, &normalized, "blocked", &result, true);
                return result;
            }

            let choice = request_permission(
                event_tx,
                "Destructive PowerShell command requested",
                &normalized,
                "Destructive",
                false,
            )
            .await;

            match choice {
                PermissionChoice::AllowOnce => {
                    let result = execute_powershell_with_timeout(sandbox, &normalized);
                    append_command_log(
                        shell,
                        &normalized,
                        "allow_once_destructive",
                        &result,
                        false,
                    );
                    result
                }
                PermissionChoice::AllowAlways | PermissionChoice::Decline => {
                    let result = format!("User declined command: {}", normalized);
                    append_command_log(shell, &normalized, "decline_destructive", &result, true);
                    result
                }
            }
        }
        Err(error) => {
            let result = format!(
                "Blocked command: failed to classify command risk ({})",
                error
            );
            append_command_log(shell, &normalized, "blocked", &result, true);
            result
        }
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

fn max_tokens() -> u64 {
    env::var("MAX_TOKENS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

fn build_request_body(model: &str, history: &[Message], provider: Provider) -> serde_json::Value {
    let max_tokens = max_tokens();

    match provider {
        Provider::OpenAiCompatible => json!({
            "model": model,
            "messages": history,
            "stream": true,
            "max_tokens": max_tokens,
            "stop": MODEL_STOP_SEQUENCES
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
                "max_tokens": max_tokens,
                "stop_sequences": MODEL_STOP_SEQUENCES
            })
        }
    }
}

fn extract_stream_text(data: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(_) => return String::new(),
    };

    // OpenAI-compatible chat completions streaming
    if let Some(content) = parsed["choices"][0]["delta"]["content"].as_str() {
        return content.to_string();
    }

    // Some OpenAI-compatible providers stream plain text here.
    if let Some(content) = parsed["choices"][0]["text"].as_str() {
        return content.to_string();
    }

    // Anthropic Messages API streaming: content_block_delta -> delta.text
    if let Some(content) = parsed["delta"]["text"].as_str() {
        return content.to_string();
    }

    // A few local/proxy APIs return a direct text/content field.
    if let Some(content) = parsed["text"].as_str() {
        return content.to_string();
    }

    if let Some(content) = parsed["content"].as_str() {
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
                    "API error status: {}\n\nAPI error body:\n{}\n\nRequest URL:\n{}\n\nDetected provider: {:?}",
                    status, error_body, url, provider
                ));
            }

            let mut stream = res.bytes_stream();
            let mut full_message = String::new();
            let mut sse_buffer = String::new();
            let _ = event_tx.send(RuntimeEvent::StartAssistant);

            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => {
                        sse_buffer.push_str(&String::from_utf8_lossy(&chunk));

                        while let Some(newline_index) = sse_buffer.find('\n') {
                            let mut line = sse_buffer[..newline_index].to_string();
                            sse_buffer.drain(..=newline_index);

                            if line.ends_with('\r') {
                                line.pop();
                            }

                            let Some(data) = line.strip_prefix("data:") else {
                                continue;
                            };

                            let data = data.trim();
                            if data == "[DONE]" {
                                break;
                            }

                            let content = extract_stream_text(data);
                            if !content.is_empty() {
                                full_message.push_str(&content);
                                let _ = event_tx.send(RuntimeEvent::AssistantChunk(content));
                            }
                        }
                    }
                    Err(error) => {
                        return Err(format!("Stream error: {}", error));
                    }
                }
            }

            // Some providers send a final data line without a trailing newline.
            if let Some(data) = sse_buffer.trim().strip_prefix("data:") {
                let data = data.trim();
                if data != "[DONE]" {
                    let content = extract_stream_text(data);
                    if !content.is_empty() {
                        full_message.push_str(&content);
                        let _ = event_tx.send(RuntimeEvent::AssistantChunk(content));
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

fn strip_json_code_fence(response: &str) -> &str {
    let trimmed = response.trim();
    let Some(after_opening) = trimmed.strip_prefix("```") else {
        return trimmed;
    };

    let after_language = after_opening
        .strip_prefix("json")
        .or_else(|| after_opening.strip_prefix("JSON"))
        .unwrap_or(after_opening)
        .trim_start();

    after_language
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or(trimmed)
}

fn parse_tool_call_response(response: &str) -> Result<ToolCall, serde_json::Error> {
    let candidate = strip_json_code_fence(response);

    if let Ok(tool_call) = serde_json::from_str::<ToolCall>(candidate) {
        return Ok(tool_call);
    }

    let value = serde_json::from_str::<serde_json::Value>(candidate)?;
    let tool = value
        .get("tool")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();

    if tool.is_empty() {
        return serde_json::from_str::<ToolCall>(candidate);
    }

    let args = if let Some(args) = value.get("args") {
        serde_json::from_value::<ToolArgs>(args.clone()).unwrap_or_default()
    } else {
        ToolArgs {
            path: value
                .get("path")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            content: value
                .get("content")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            url: value
                .get("url")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            app: value
                .get("app")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            command: value
                .get("command")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            action: value
                .get("action")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            selector: value
                .get("selector")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            value: value
                .get("value")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            session: value
                .get("session")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            new_tab: value.get("new_tab").and_then(|value| value.as_bool()),
        }
    };

    Ok(ToolCall {
        r#type: value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("tool_call")
            .to_string(),
        tool,
        args,
    })
}

fn latest_user_runtime_action_request(history: &[Message]) -> Option<&str> {
    history
        .iter()
        .rev()
        .filter(|message| {
            message.role == "user" && !message.content.starts_with(TOOL_RESULT_PREFIX)
        })
        .map(|message| message.content.as_str())
        .find(|content| user_requested_runtime_action(content))
}

fn user_requested_runtime_action(input: &str) -> bool {
    let normalized = input.to_lowercase();
    [
        "open ",
        "launch ",
        "start ",
        "go to",
        "navigate",
        "visit",
        "website",
        "browser",
        "chrome",
        "click",
        "fill",
        "type ",
        "scroll",
        "find ",
        "search",
        "look for",
        "you have to do it",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

fn response_looks_like_tool_avoidance(response: &str) -> bool {
    let normalized = response.to_lowercase();
    [
        "i can't directly",
        "i cannot directly",
        "i don't have access",
        "i can guide",
        "i'll help you",
        "i will help you",
        "i'll open",
        "i will open",
        "should i",
        "would you like me",
        "let me initiate",
        "let me start",
        "here's how",
        "here is how",
        "follow these steps",
        "manually",
        "you can open",
        "you can click",
        "please click",
        "tool result from trust runtime",
        "the command executed successfully",
        "initiated chrome",
        "none required for this interaction",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn extract_url_like(input: &str) -> Option<String> {
    input
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|ch: char| {
                matches!(
                    ch,
                    ',' | '.' | ';' | ':' | '!' | '?' | '"' | '\'' | '(' | ')' | '[' | ']'
                )
            })
        })
        .find_map(|word| {
            let lower = word.to_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                Some(word.to_string())
            } else if lower.contains('.') && !lower.contains('@') {
                Some(format!("https://{}", word))
            } else {
                None
            }
        })
}

fn inferred_website_url(input: &str) -> Option<String> {
    if let Some(url) = extract_url_like(input) {
        return Some(url);
    }

    let normalized = input.to_lowercase();
    if normalized.contains("apple")
        && (normalized.contains("website") || normalized.contains("site"))
    {
        return Some("https://www.apple.com".to_string());
    }
    if normalized.contains("google")
        && (normalized.contains("website") || normalized.contains("site"))
    {
        return Some("https://www.google.com".to_string());
    }
    if normalized.contains("youtube")
        && (normalized.contains("website") || normalized.contains("site"))
    {
        return Some("https://www.youtube.com".to_string());
    }

    None
}

fn inferred_tool_call_for_request(user_request: &str) -> Option<ToolCall> {
    let normalized = user_request.to_lowercase();

    let command = if normalized.contains("chrome") {
        Some(match extract_url_like(user_request) {
            Some(url) => format!("Start-Process chrome {}", shell_single_quote(&url)),
            None => "Start-Process chrome".to_string(),
        })
    } else if normalized.contains("notepad") {
        Some("Start-Process notepad".to_string())
    } else if normalized.contains("calculator") || normalized.contains("calc") {
        Some("Start-Process calc".to_string())
    } else if normalized.contains("edge") {
        Some(match extract_url_like(user_request) {
            Some(url) => format!("Start-Process msedge {}", shell_single_quote(&url)),
            None => "Start-Process msedge".to_string(),
        })
    } else {
        None
    };

    if let Some(command) = command {
        return Some(ToolCall {
            r#type: "tool_call".to_string(),
            tool: "run_command".to_string(),
            args: ToolArgs {
                command: Some(command),
                ..ToolArgs::default()
            },
        });
    }

    if let Some(url) = inferred_website_url(user_request) {
        return Some(ToolCall {
            r#type: "tool_call".to_string(),
            tool: "kimi_webbridge".to_string(),
            args: ToolArgs {
                action: Some("navigate".to_string()),
                url: Some(url),
                new_tab: Some(true),
                session: Some("trust".to_string()),
                ..ToolArgs::default()
            },
        });
    }

    None
}

fn request_mentions_installed_app(user_request: &str) -> bool {
    let normalized = user_request.to_lowercase();
    [
        "chrome",
        "notepad",
        "calculator",
        "calc",
        "edge",
        "file explorer",
        "explorer",
        "terminal",
        "powershell",
    ]
    .iter()
    .any(|app| normalized.contains(app))
}

fn tool_call_matches_request(user_request: &str, tool_call: &ToolCall) -> bool {
    if request_mentions_installed_app(user_request) {
        return tool_call.tool == "run_command";
    }

    true
}

fn tool_reprompt_message(user_request: &str) -> String {
    format!(
        "Runtime correction: The user requested a real computer/browser action, but your last response did not correctly call the required tool. Do not give a tutorial or say you cannot do it. Use the available runtime tools now. For opening installed apps like Chrome or Notepad use run_command with PowerShell Start-Process. For navigating, reading, scrolling, clicking, or filling webpages use kimi_webbridge. If Kimi WebBridge is unavailable, call it anyway so the runtime can return the installation/setup message. User request: {}",
        user_request
    )
}

fn normalize_assistant_response(response: &str) -> String {
    let trimmed = response.trim();

    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return response.to_string();
    };

    if parsed.get("type").and_then(|value| value.as_str()) == Some("tool_call") {
        return response.to_string();
    }

    for key in [
        "response",
        "message",
        "content",
        "text",
        "answer",
        "tool_result_from_TRUST_runtime",
        "tool_result",
        "result",
    ] {
        if let Some(value) = parsed.get(key).and_then(|value| value.as_str()) {
            return value.to_string();
        }
    }

    response.to_string()
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
            "Blocked command: {}. Allowed sandbox commands: cargo check, cargo fmt --check, cargo clippy",
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
        PermissionChoice::AllowAlways => {
            format!(
                "Allow always is not available for sandboxed commands: {}",
                allowed_command.display
            )
        }
        PermissionChoice::Decline => format!("User declined command: {}", allowed_command.display),
    }
}

fn webbridge_unavailable_message(error: &str) -> String {
    format!(
        "Kimi WebBridge is not available or not connected. Install the Kimi WebBridge browser extension, install/start the device daemon, then retry. Check it with `~/.kimi-webbridge/bin/kimi-webbridge status`. Details: {}",
        error
    )
}

async fn send_webbridge_command(
    action: &str,
    args: serde_json::Value,
    session: Option<String>,
) -> String {
    let client = Client::new();
    let mut body = json!({
        "action": action,
        "args": args,
    });

    if let Some(session) = session.filter(|session| !session.trim().is_empty()) {
        body["session"] = json!(session);
    }

    let response = client.post(WEBBRIDGE_COMMAND_URL).json(&body).send().await;

    let response = match response {
        Ok(response) => response,
        Err(error) => return webbridge_unavailable_message(&error.to_string()),
    };

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read WebBridge response>".to_string());

    if !status.is_success() {
        return webbridge_unavailable_message(&format!("HTTP {}: {}", status, text));
    }

    format!("Kimi WebBridge {} result:\n{}", action, text)
}

async fn run_webbridge_tool(tool_call: ToolCall) -> String {
    let session = tool_call.args.session.or_else(|| Some("trust".to_string()));
    let action = tool_call
        .args
        .action
        .unwrap_or_else(|| "navigate".to_string())
        .trim()
        .to_lowercase();

    match action.as_str() {
        "navigate" | "open" => {
            let Some(url) = tool_call.args.url else {
                return "Missing url for Kimi WebBridge navigate".to_string();
            };
            send_webbridge_command(
                "navigate",
                json!({
                    "url": url,
                    "newTab": tool_call.args.new_tab.unwrap_or(true),
                    "group_title": "TRUST"
                }),
                session,
            )
            .await
        }
        "snapshot" | "read" => send_webbridge_command("snapshot", json!({}), session).await,
        "click" => {
            let Some(selector) = tool_call.args.selector else {
                return "Missing selector for Kimi WebBridge click".to_string();
            };
            send_webbridge_command("click", json!({ "selector": selector }), session).await
        }
        "fill" | "type" => {
            let Some(selector) = tool_call.args.selector else {
                return "Missing selector for Kimi WebBridge fill".to_string();
            };
            let Some(value) = tool_call.args.value.or(tool_call.args.content) else {
                return "Missing value for Kimi WebBridge fill".to_string();
            };
            send_webbridge_command(
                "fill",
                json!({
                    "selector": selector,
                    "value": value
                }),
                session,
            )
            .await
        }
        "evaluate" => {
            let Some(code) = tool_call.args.content else {
                return "Missing content code for Kimi WebBridge evaluate".to_string();
            };
            send_webbridge_command("evaluate", json!({ "code": code }), session).await
        }
        "tabs" | "list_tabs" => send_webbridge_command("list_tabs", json!({}), session).await,
        "close_tab" => send_webbridge_command("close_tab", json!({}), session).await,
        other => format!(
            "Unknown Kimi WebBridge action: {}. Supported actions: navigate, snapshot, click, fill, evaluate, list_tabs, close_tab",
            other
        ),
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

            if let Some(parent) = resolved_path.parent()
                && let Err(e) = fs::create_dir_all(parent)
            {
                return format!("Failed to create parent directory for {}: {}", path, e);
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
        "kimi_webbridge" | "web_bridge" | "browser_control" => run_webbridge_tool(tool_call).await,
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

        if let Ok(tool_call) = parse_tool_call_response(&full_message) {
            if let Some(user_request) = latest_user_runtime_action_request(&history)
                && !tool_call_matches_request(user_request, &tool_call)
                && step + 1 < MAX_AGENT_STEPS
            {
                let _ = event_tx.send(RuntimeEvent::DiscardAssistantDraft);
                let _ = event_tx.send(RuntimeEvent::Status(
                    "Correcting model: wrong tool selected...".to_string(),
                ));
                history.push(Message {
                    role: "user".to_string(),
                    content: tool_reprompt_message(user_request),
                });
                continue;
            }

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

        if let Some(user_request) = latest_user_runtime_action_request(&history)
            && response_looks_like_tool_avoidance(&full_message)
            && step + 1 < MAX_AGENT_STEPS
        {
            let _ = event_tx.send(RuntimeEvent::DiscardAssistantDraft);

            if let Some(tool_call) = inferred_tool_call_for_request(user_request) {
                let tool_call_content = serde_json::to_string(&tool_call)
                    .unwrap_or_else(|_| "<failed to serialize inferred tool call>".to_string());
                history.push(Message {
                    role: "assistant".to_string(),
                    content: tool_call_content,
                });

                let _ = event_tx.send(RuntimeEvent::Status(format!(
                    "Runtime enforcing tool: {}",
                    tool_call.tool
                )));
                let tool_result = run_tool(tool_call, &sandbox, &event_tx).await;
                let _ = event_tx.send(RuntimeEvent::ToolResult(tool_result.clone()));

                history.push(Message {
                    role: "user".to_string(),
                    content: format!("{}{}", TOOL_RESULT_PREFIX, tool_result),
                });
                continue;
            }

            let _ = event_tx.send(RuntimeEvent::Status(
                "Correcting model: tool action required...".to_string(),
            ));
            history.push(Message {
                role: "user".to_string(),
                content: tool_reprompt_message(user_request),
            });
            continue;
        }

        let normalized_message = normalize_assistant_response(&full_message);
        let final_message = if response_claims_destructive_action(&normalized_message) {
            "Blocked destructive command: assistant claimed execution without a runtime tool result"
                .to_string()
        } else {
            normalized_message
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
You are the planner: decide which tool or command should be used for each user request.
Use tools only when the user clearly asks for an action on the computer, files, apps, browser, web pages, or sandbox.
For normal conversation, answer normally in plain text and do not call tools. Do not wrap normal replies in JSON.
Do not use raw Markdown formatting for normal replies: avoid ### headings, **bold markers**, and long numbered instruction dumps. The TUI adds styling.
If the user asks you to open, launch, navigate to, go to, search on, click, fill, or interact with an app/website/computer, that is not normal conversation: you MUST call an appropriate tool instead of giving instructions.
You may use multiple tools across multiple steps in a single turn.
When using a tool, reply ONLY with one JSON object and no prose, no markdown fences, no explanation, and no predicted result.
After emitting a tool-call JSON object, STOP. The runtime will execute the tool and call you again with a message that starts with "Tool result from TRUST runtime:".
Use that real runtime result to decide your next step or produce a final answer.
When producing a final answer after a real tool result, write normal human-readable text. Do not wrap the final answer in JSON.
Never write fake tool results such as "Tool result from TRUST runtime", "Runtime output", "Tool Execution Success", or "None required" yourself. Only the runtime provides tool results.

You operate with controlled autonomy inside a sandbox.
File tools resolve only inside the sandbox directories: workspace/, outputs/, and temp/.
If a path does not start with one of those prefixes, it is treated as relative to workspace/.

Example write_file tool call:
{
  "type": "tool_call",
  "tool": "write_file",
  "args": {
    "path": "outputs/example.md",
    "content": "Hello world"
  }
}

Example run_command tool call:
{
  "type": "tool_call",
  "tool": "run_command",
  "args": {
    "command": "Get-ChildItem"
  }
}

Example open Chrome app tool call:
{
  "type": "tool_call",
  "tool": "run_command",
  "args": {
    "command": "Start-Process chrome"
  }
}

Example open URL in Chrome tool call:
{
  "type": "tool_call",
  "tool": "run_command",
  "args": {
    "command": "Start-Process chrome 'https://example.com'"
  }
}

Example delayed PowerShell action:
{
  "type": "tool_call",
  "tool": "run_command",
  "args": {
    "command": "$processes = @()\nfor ($i = 0; $i -lt 5; $i++) {\n    $processes += Start-Process cmd -PassThru\n    Start-Sleep -Seconds 1\n}\nStart-Sleep -Seconds 5\nforeach ($p in $processes) {\n    if (!$p.HasExited) {\n        Stop-Process -Id $p.Id -Force\n    }\n}"
  }
}

Example Kimi WebBridge navigate tool call for "go to the Apple website":
{
  "type": "tool_call",
  "tool": "kimi_webbridge",
  "args": {
    "action": "navigate",
    "url": "https://www.apple.com",
    "new_tab": true,
    "session": "trust"
  }
}

Example Kimi WebBridge navigate tool call for "go to https://www.youtube.com":
{
  "type": "tool_call",
  "tool": "kimi_webbridge",
  "args": {
    "action": "navigate",
    "url": "https://www.youtube.com",
    "new_tab": true,
    "session": "trust"
  }
}

Example Kimi WebBridge page read tool call:
{
  "type": "tool_call",
  "tool": "kimi_webbridge",
  "args": {
    "action": "snapshot",
    "session": "trust"
  }
}

Example Kimi WebBridge click/fill tool calls:
{
  "type": "tool_call",
  "tool": "kimi_webbridge",
  "args": {
    "action": "click",
    "selector": "@e1",
    "session": "trust"
  }
}
{
  "type": "tool_call",
  "tool": "kimi_webbridge",
  "args": {
    "action": "fill",
    "selector": "@e2",
    "value": "search text",
    "session": "trust"
  }
}

Allowed tools:
- write_file: reads and writes only inside sandboxed workspace/, outputs/, or temp/
- read_file: only reads inside sandboxed workspace/, outputs/, or temp/
- list_directory: only lists inside sandboxed workspace/, outputs/, or temp/
- kimi_webbridge: full browser control through Kimi WebBridge. Actions: navigate, snapshot, click, fill, evaluate, list_tabs, close_tab. If WebBridge is not installed or connected, the runtime asks the user to install/start it first.
- run_sandboxed_command: runs allowlisted project commands only inside sandbox/workspace after user approval. Allowed commands: cargo check, cargo fmt --check, cargo clippy
- run_command: runs PowerShell by default on Windows from sandbox/workspace. Use it for normal PowerShell commands, opening apps, creating files/folders, reading files, running scripts, process management, networking/debug commands, delayed/sequenced actions, and launching multiple terminals/apps. The runtime decides whether approval is required and whether the command is blocked.

Command planning rules:
- Understand the user's intent, generate PowerShell, rely on the runtime's destructive-command detector, request execution through run_command, then explain the result.
- If the user asks to open, launch, or start an installed app, browser, Chrome, Edge, terminal, file explorer, or desktop program, immediately call run_command with PowerShell Start-Process. Do not ask for confirmation for non-destructive app launches. Do not answer with instructions. Do not use Kimi WebBridge for launching apps.
- If the user asks to open, go to, visit, or navigate to a website/URL/page, immediately call Kimi WebBridge navigate. Preserve the exact requested URL when present. For named common sites, use the canonical URL, e.g. Apple website -> https://www.apple.com. Do not assume WebBridge is unavailable; call the tool first. If WebBridge is unavailable, report the setup message returned by the runtime.
- If the user gives a multi-step browser task like "open Chrome, go to a site, click something, fill a form", do it as tool steps: use run_command only to launch Chrome if explicitly requested, then use Kimi WebBridge navigate/snapshot/click/fill for page interaction. Do not respond with a written tutorial.
- If the user asks to search the web from the browser, call run_command with a browser URL for the search query.
- Use Kimi WebBridge only when the user asks to inspect, read, click, type, fill, search inside, configure, buy, check out, or otherwise interact with webpage contents after a page is open.
- You may fill harmless sample data when explicitly asked, but do not enter real personal/payment credentials. Do not complete purchases, submit payment, or place orders.
- For delayed or scheduled tasks, generate a complete PowerShell script with variables, loops, Start-Sleep, process tracking with -PassThru where possible, and cleanup after completion.
- For multi-step app/process tasks, prefer tracking process objects instead of broad process kills. Use Stop-Process only for processes you started when possible.
- If permission is required, the runtime will show the generated command/script before execution.

Safety rules:
- Do not claim you cannot interact with the computer or browser when an allowed tool can do the task.
- If the user asks to open or launch Chrome, a browser, or any installed app, use run_command with Start-Process.
- If the user asks to open, go to, visit, or navigate to a specific website or URL, use Kimi WebBridge navigate and preserve the exact requested URL.
- If the user asks to read, click, type, search inside, inspect, configure, buy, check out, or interact with a webpage, use kimi_webbridge. If the tool result says Kimi WebBridge is not available, tell the user to install the browser extension and start the device daemon first.
- Do not access private data such as .env files unless the user explicitly asks and the runtime allows it.
- You may request run_command when needed, but the runtime decides whether it executes.
- Destructive commands targeting system folders, Windows/System32, boot files, registry hives, entire drives, security tools, or user profiles may be blocked unless ALLOW_DESTRUCTIVE_ACTIONS=true and the user explicitly approves them.
- If the runtime blocks a command, briefly explain that the runtime blocked it and suggest a safer alternative.
- Never claim a command ran unless the runtime returns a real tool result confirming it ran.
- Never pretend to save files, read files, list directories, open apps, browse websites, click elements, fill fields, or run commands. Use a tool when available.
- If a tool is useful, prefer actually using it over just describing what would happen.
- If a requested action needs a tool but the tool is unavailable or fails, say that clearly and stop; do not replace the action with a tutorial.
- Only use JSON when calling tools.
"#
    .to_string()
}

fn ensure_system_message(mut history: Vec<Message>) -> Vec<Message> {
    let prompt = system_prompt();

    if let Some(system_message) = history.iter_mut().find(|message| message.role == "system") {
        system_message.content = prompt;
    } else {
        history.insert(
            0,
            Message {
                role: "system".to_string(),
                content: prompt,
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
    parse_tool_call_response(content).is_ok()
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

fn spinner() -> &'static str {
    let frame = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_millis() / 140) % 8)
        .unwrap_or(0);

    match frame {
        0 => "⠋",
        1 => "⠙",
        2 => "⠹",
        3 => "⠸",
        4 => "⠼",
        5 => "⠴",
        6 => "⠦",
        _ => "⠧",
    }
}

fn styled_inline_spans(content: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = content;

    while !rest.is_empty() {
        if let Some(after_marker) = rest.strip_prefix("**")
            && let Some(end) = after_marker.find("**")
        {
            spans.push(Span::styled(
                after_marker[..end].to_string(),
                base_style.add_modifier(Modifier::BOLD),
            ));
            rest = &after_marker[end + 2..];
            continue;
        }

        if let Some(after_marker) = rest.strip_prefix('*')
            && let Some(end) = after_marker.find('*')
        {
            spans.push(Span::styled(
                after_marker[..end].to_string(),
                base_style.add_modifier(Modifier::ITALIC),
            ));
            rest = &after_marker[end + 1..];
            continue;
        }

        if let Some(after_marker) = rest.strip_prefix('`')
            && let Some(end) = after_marker.find('`')
        {
            spans.push(Span::styled(
                after_marker[..end].to_string(),
                Style::default().fg(Color::LightMagenta),
            ));
            rest = &after_marker[end + 1..];
            continue;
        }

        let next_marker = rest
            .char_indices()
            .skip(1)
            .find_map(|(index, ch)| matches!(ch, '*' | '`').then_some(index))
            .unwrap_or(rest.len());
        spans.push(Span::styled(rest[..next_marker].to_string(), base_style));
        rest = &rest[next_marker..];
    }

    spans
}

fn styled_message_lines(label: &str, color: Color, content: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let label_style = Style::default().fg(color).add_modifier(Modifier::BOLD);

    for (index, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim_start();
        let mut line_spans = Vec::new();

        if index == 0 {
            line_spans.push(Span::styled(format!("{}: ", label), label_style));
        } else {
            line_spans.push(Span::raw("  "));
        }

        if let Some(heading) = trimmed.strip_prefix("### ") {
            line_spans.push(Span::styled(
                heading.to_string(),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if let Some(heading) = trimmed.strip_prefix("## ") {
            line_spans.push(Span::styled(
                heading.to_string(),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if let Some(heading) = trimmed.strip_prefix("# ") {
            line_spans.push(Span::styled(
                heading.to_string(),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            line_spans.push(Span::styled("• ", Style::default().fg(Color::LightCyan)));
            line_spans.extend(styled_inline_spans(item, Style::default()));
        } else {
            line_spans.extend(styled_inline_spans(raw_line, Style::default()));
        }

        lines.push(Line::from(line_spans));
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!("{}: ", label),
            label_style,
        )]));
    }

    lines
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

        lines.extend(styled_message_lines(label, color, &message.content));
        lines.push(Line::raw(""));
    }

    if !app.draft_assistant.is_empty() {
        lines.extend(styled_message_lines(
            "TRUST",
            Color::Green,
            &app.draft_assistant,
        ));
    } else if app.busy {
        lines.push(Line::from(vec![
            Span::styled(
                "TRUST: ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} thinking…", spinner()),
                Style::default().fg(Color::LightGreen),
            ),
        ]));
    }

    Text::from(lines)
}

fn wrapped_line_count(content: &str, viewport_width: u16) -> usize {
    let width = usize::from(viewport_width.max(1));

    content
        .lines()
        .map(|line| {
            let chars = line.chars().count();
            chars.saturating_sub(1) / width + 1
        })
        .sum::<usize>()
        .max(1)
}

fn transcript_line_count(app: &App, viewport_width: u16) -> usize {
    let message_lines = app
        .visible_messages
        .iter()
        .map(|message| {
            let label_width = match message.role {
                UiMessageRole::User => 5,
                UiMessageRole::Assistant => 7,
                UiMessageRole::Tool => 6,
                UiMessageRole::Info => 6,
            };
            let effective_width = viewport_width.saturating_sub(label_width).max(1);

            wrapped_line_count(&message.content, effective_width) + 1
        })
        .sum::<usize>();

    let draft_lines = if app.draft_assistant.trim().is_empty() {
        usize::from(app.busy)
    } else {
        wrapped_line_count(
            &app.draft_assistant,
            viewport_width.saturating_sub(7).max(1),
        )
    };

    message_lines + draft_lines
}

fn resolved_transcript_scroll(app: &App, viewport_height: u16, viewport_width: u16) -> u16 {
    let content_lines =
        transcript_line_count(app, viewport_width).min(usize::from(u16::MAX)) as u16;
    let max_scroll = content_lines.saturating_sub(viewport_height);

    if app.auto_scroll || app.transcript_scroll == u16::MAX {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    }
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

    let transcript_area = vertical[0];
    let transcript_inner_height = transcript_area.height.saturating_sub(2);
    let transcript_inner_width = transcript_area.width.saturating_sub(2);
    let transcript_scroll =
        resolved_transcript_scroll(app, transcript_inner_height, transcript_inner_width);
    let transcript_title = if app.busy {
        format!(
            "Conversation · {} TRUST working · wheel/↑/↓ scroll · End newest · {} messages",
            spinner(),
            app.visible_messages.len()
        )
    } else {
        format!(
            "Conversation · wheel/↑/↓ scroll · PgUp/PgDn jump · End newest · {} messages",
            app.visible_messages.len()
        )
    };
    let transcript = Paragraph::new(build_transcript(app))
        .block(
            Block::default()
                .title(transcript_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .scroll((transcript_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, transcript_area);

    let mut scrollbar_state =
        ScrollbarState::new(transcript_line_count(app, transcript_inner_width))
            .position(transcript_scroll as usize)
            .viewport_content_length(transcript_inner_height as usize);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .begin_symbol(Some("▲"))
        .end_symbol(Some("▼"));
    frame.render_stateful_widget(scrollbar, transcript_area, &mut scrollbar_state);

    let status_style = if app.busy {
        Style::default().fg(Color::LightYellow)
    } else {
        Style::default().fg(Color::Gray)
    };
    let status = Paragraph::new(app.status.clone())
        .style(status_style)
        .block(
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

    let input_title = if app.busy {
        "Input · waiting for TRUST"
    } else {
        "Input · Enter send · Tab chat · Ctrl+C quit"
    };
    let input = Paragraph::new(app.input.clone())
        .style(Style::default().fg(Color::LightCyan))
        .block(
            Block::default()
                .title(input_title)
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
            Line::from(Span::styled(
                pending.title.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Risk: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(pending.risk_label.clone()),
            ]),
            Line::raw(""),
            Line::from(Span::styled(
                "Command:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ];

        for command_line in pending.command.lines().take(8) {
            lines.push(Line::raw(command_line.to_string()));
        }

        if pending.command.lines().count() > 8 {
            lines.push(Line::raw("..."));
        }

        lines.push(Line::raw(""));
        lines.push(Line::raw("[A] Allow once"));

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

    app.scroll_to_bottom();
    app.add_user_message(input.clone());
    app.model_history.push(Message {
        role: "user".to_string(),
        content: input.clone(),
    });
    save_history(&app.current_chat, &app.model_history);

    app.input.clear();
    app.busy = true;
    app.draft_assistant.clear();

    let current_chat = app.current_chat.clone();
    let history = app.model_history.clone();
    let runtime_tx = event_tx.clone();
    let sandbox = app.sandbox.clone();

    app.status = "Planning tool actions...".to_string();

    tokio::spawn(async move {
        process_turn(current_chat, history, sandbox, runtime_tx).await;
    });
}

fn transcript_viewport_size() -> Option<(u16, u16)> {
    let (terminal_width, terminal_height) = terminal_size().ok()?;
    let sidebar_width = 24;
    let transcript_border = 2;
    let bottom_panels_height = 7;

    Some((
        terminal_width
            .saturating_sub(sidebar_width)
            .saturating_sub(transcript_border),
        terminal_height
            .saturating_sub(bottom_panels_height)
            .saturating_sub(transcript_border),
    ))
}

fn transcript_max_scroll(app: &App) -> Option<u16> {
    let (width, height) = transcript_viewport_size()?;
    let content_lines = transcript_line_count(app, width).min(usize::from(u16::MAX)) as u16;

    Some(content_lines.saturating_sub(height))
}

fn scroll_up_visible(app: &mut App, amount: u16) {
    let current_scroll = if app.auto_scroll || app.transcript_scroll == u16::MAX {
        transcript_max_scroll(app).unwrap_or(0)
    } else {
        app.transcript_scroll
    };

    app.transcript_scroll = current_scroll.saturating_sub(amount);
    app.auto_scroll = false;
}

fn scroll_down_visible(app: &mut App, amount: u16) {
    let max_scroll = transcript_max_scroll(app).unwrap_or(0);
    let current_scroll = if app.auto_scroll || app.transcript_scroll == u16::MAX {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    };
    let next_scroll = current_scroll.saturating_add(amount).min(max_scroll);

    if next_scroll >= max_scroll {
        app.scroll_to_bottom();
    } else {
        app.transcript_scroll = next_scroll;
        app.auto_scroll = false;
    }
}

fn handle_mouse_event(app: &mut App, mouse: MouseEvent) {
    let Ok((terminal_width, terminal_height)) = terminal_size() else {
        return;
    };

    let sidebar_width = 24;
    let bottom_panels_height = 7;
    let in_transcript = mouse.column >= sidebar_width
        && mouse.column < terminal_width
        && mouse.row < terminal_height.saturating_sub(bottom_panels_height);

    if !in_transcript {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => scroll_up_visible(app, 2),
        MouseEventKind::ScrollDown => scroll_down_visible(app, 2),
        _ => {}
    }
}

fn handle_key_event(app: &mut App, key: KeyEvent, event_tx: &mpsc::UnboundedSender<RuntimeEvent>) {
    if key.kind != KeyEventKind::Press {
        return;
    }

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
            let command = pending.command.clone();
            let _ = pending.responder.send(choice);
            app.status = format!("Resolved approval for: {}", command);
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
        KeyCode::Up => scroll_up_visible(app, 1),
        KeyCode::Down => scroll_down_visible(app, 1),
        KeyCode::PageUp => scroll_up_visible(app, 8),
        KeyCode::PageDown => scroll_down_visible(app, 8),
        KeyCode::Home => {
            app.transcript_scroll = 0;
            app.auto_scroll = false;
        }
        KeyCode::End => app.scroll_to_bottom(),
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
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let sandbox = SandboxConfig::load()
        .and_then(|config| {
            ensure_sandbox_ready(&config)?;
            Ok(config)
        })
        .map_err(io::Error::other)?;

    let mut tui = Tui::new()?;
    let mut app = App::new(sandbox);
    app.status = "Ready · /chat <name> · /list · /clear · /delete <name> · /credits · Ctrl+C quit"
        .to_string();

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<RuntimeEvent>();

    while !app.should_quit {
        while let Ok(event) = event_rx.try_recv() {
            app.handle_runtime_event(event);
        }

        tui.draw(&app)?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                CEvent::Key(key) => handle_key_event(&mut app, key, &event_tx),
                CEvent::Mouse(mouse) => handle_mouse_event(&mut app, mouse),
                _ => {}
            }
        }
    }

    Ok(())
}
