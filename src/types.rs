use serde::{Deserialize, Serialize};

use std::env;

use std::path::PathBuf;

use tokio::sync::oneshot;

use crate::runtime::sandbox::*;

pub(crate) const MAX_AGENT_STEPS: usize = 5;

pub(crate) const DEFAULT_SANDBOX_DIR: &str = "sandbox/workspace";

pub(crate) const DEFAULT_SANDBOX_COMMAND_TIMEOUT_MS: u64 = 30_000;

pub(crate) const DEFAULT_MAX_TOKENS: u64 = 1024;

pub(crate) const TOOL_RESULT_PREFIX: &str = "Tool result from TRUST runtime:";

pub(crate) const WEBBRIDGE_COMMAND_URL: &str = "http://127.0.0.1:10086/command";

pub(crate) const MODEL_STOP_SEQUENCES: [&str; 4] = [
    "Tool result from TRUST runtime:",
    "\nTool result from TRUST runtime:",
    "Runtime output:",
    "Tool Execution Success:",
];

#[derive(Serialize, Deserialize, Clone, Debug)]

pub(crate) struct Message {
    pub(crate) role: String,

    pub(crate) content: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]

pub(crate) struct ToolCall {
    pub(crate) r#type: String,

    pub(crate) tool: String,

    pub(crate) args: ToolArgs,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]

pub(crate) struct ToolArgs {
    pub(crate) path: Option<String>,

    pub(crate) content: Option<String>,

    pub(crate) url: Option<String>,

    pub(crate) app: Option<String>,

    pub(crate) command: Option<String>,

    pub(crate) action: Option<String>,

    pub(crate) selector: Option<String>,

    pub(crate) value: Option<String>,

    pub(crate) session: Option<String>,

    pub(crate) new_tab: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]

pub(crate) enum Provider {
    OpenAiCompatible,

    AnthropicCompatible,
}

#[derive(Clone, Debug)]

pub(crate) struct SandboxConfig {
    pub(crate) root: PathBuf,

    pub(crate) workspace: PathBuf,

    pub(crate) outputs: PathBuf,

    pub(crate) temp: PathBuf,

    pub(crate) command_timeout_ms: u64,
}

impl SandboxConfig {
    pub(crate) fn load() -> Result<Self, String> {
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

pub(crate) struct AllowedCommand {
    pub(crate) display: &'static str,

    pub(crate) program: &'static str,

    pub(crate) args: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]

pub(crate) enum CommandRisk {
    Safe,

    NeedsApproval,

    Destructive,

    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]

pub(crate) enum CommandShell {
    PowerShell,
}

impl CommandShell {
    pub(crate) fn label(self) -> &'static str {
        match self {
            CommandShell::PowerShell => "PowerShell",
        }
    }

    pub(crate) fn program(self) -> &'static str {
        match self {
            CommandShell::PowerShell if cfg!(windows) => "powershell.exe",

            CommandShell::PowerShell => "pwsh",
        }
    }

    pub(crate) fn args(self, command: &str) -> Vec<String> {
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

pub(crate) enum PermissionChoice {
    AllowOnce,

    AllowAlways,

    Decline,
}

#[derive(Clone, Debug)]

pub(crate) enum UiMessageRole {
    User,

    Assistant,

    Tool,

    Info,
}

#[derive(Clone, Debug)]

pub(crate) struct UiMessage {
    pub(crate) role: UiMessageRole,

    pub(crate) content: String,
}

pub(crate) struct PendingApproval {
    pub(crate) title: String,

    pub(crate) command: String,

    pub(crate) risk_label: String,

    pub(crate) allow_always: bool,

    pub(crate) responder: oneshot::Sender<PermissionChoice>,
}

#[derive(Serialize)]

pub(crate) struct CommandLogEntry {
    pub(crate) timestamp_unix_ms: u128,

    pub(crate) shell: String,

    pub(crate) command: String,

    pub(crate) permission_choice: String,

    pub(crate) result: String,

    pub(crate) blocked: bool,
}

pub(crate) enum RuntimeEvent {
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
