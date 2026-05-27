// TRUST
// Terminal Runtime for Unified Smart Tasks
//
// A sandboxed terminal AI runtime written in Rust.
//
// Features:
// - Multi-provider AI support
// - Streaming chat responses
// - Persistent multi-chat memory
// - Autonomous agent loop
// - Structired tool caling
// - Sandboxed file system access
// - Safe command execution
// - OpenAI & Anthropic compatibility
//
// Providers:
// Openrouter • Groq • NVIDIA • Cerebras • Anthropic • HCAI
//
// Designed for speed, safety, and controlled autonomy.
//
// Build with Rust by Daniel Santhosh
// GitHub Repo: https://github.com/danielzanthosh/trust

use colored::*;
use dotenvy::dotenv;
use futures_util::StreamExt;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MAX_AGENT_STEPS: usize = 5;
const DEFAULT_SANDBOX_DIR: &str = "sandbox/workspace";
const DEFAULT_SANDBOX_COMMAND_TIMEOUT_MS: u64 = 30_000;

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

fn read_approval_input(prompt: &str) -> bool {
    print!("{}", prompt.bright_yellow());
    io::stdout().flush().unwrap();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn read_permission_choice(command: &str, risk: CommandRisk) -> PermissionChoice {
    match risk {
        CommandRisk::NeedsApproval => {
            println!(
                "\n{} {}",
                "Command requested:".bright_yellow().bold(),
                command
            );
            println!("{} {}", "Risk:".bright_yellow().bold(), "NeedsApproval");
            println!("\nOptions:");
            println!("[A] Allow once");
            println!("[L] Allow always");
            println!("[D] Decline");
            print!("{}", "Choose A, L, or D: ".bright_yellow());
        }
        CommandRisk::Destructive => {
            println!(
                "\n{} {}",
                "Destructive command requested:".bright_red().bold(),
                command
            );
            println!("{} {}", "Risk:".bright_red().bold(), "Destructive");
            println!("\nOptions:");
            println!("[A] Allow once");
            println!("[D] Decline");
            print!("{}", "Choose A or D: ".bright_yellow());
        }
        _ => return PermissionChoice::Decline,
    }

    io::stdout().flush().unwrap();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return PermissionChoice::Decline;
    }

    match (risk, input.trim().to_lowercase().as_str()) {
        (_, "a") | (_, "allow once") => PermissionChoice::AllowOnce,
        (CommandRisk::NeedsApproval, "l") | (CommandRisk::NeedsApproval, "allow always") => {
            PermissionChoice::AllowAlways
        }
        _ => PermissionChoice::Decline,
    }
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
            "Failed to prepare sandbox target directory {}: {}",
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
        Err(e) => return format!("Failed to start command {}: {}", display_name, e),
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
                    Err(e) => format!("Failed to collect output from {}: {}", display_name, e),
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
            Err(e) => return format!("Failed while waiting on command {}: {}", display_name, e),
        }
    }
}

fn run_command(sandbox: &SandboxConfig, command: &str) -> String {
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
            match read_permission_choice(&normalized, CommandRisk::NeedsApproval) {
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

            match read_permission_choice(&normalized, CommandRisk::Destructive) {
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
                        "user" => "user",
                        "tool" => "assistant",
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

async fn request_model_reply(history: &[Message]) -> Result<String, ()> {
    let api_key = env::var("API_KEY").unwrap_or_default();
    let base_url = env::var("BASE_URL").unwrap_or_default();
    let model = env::var("MODEL").unwrap_or_default();

    if api_key.trim().is_empty() {
        eprintln!("Missing API_KEY. Add it to your .env file or environment variables.");
        return Err(());
    }

    if base_url.trim().is_empty() {
        eprintln!("Missing BASE_URL. Example: BASE_URL=https://api.openai.com");
        return Err(());
    }

    if model.trim().is_empty() {
        eprintln!("Missing MODEL. Example: MODEL=gpt-4o-mini");
        return Err(());
    }

    let provider = detect_provider(&base_url);
    let url = build_request_url(&base_url, provider);
    let client = Client::new();
    let body = build_request_body(&model, history, provider);

    let headers = match build_headers(&api_key, provider) {
        Ok(headers) => headers,
        Err(error) => {
            eprintln!("Header Error: {}", error);
            return Err(());
        }
    };

    let response = client.post(&url).headers(headers).json(&body).send().await;

    match response {
        Ok(res) => {
            let status = res.status();

            if !status.is_success() {
                let error_body = res
                    .text()
                    .await
                    .unwrap_or_else(|_| "<failed to read error response body>".to_string());
                println!("API Error Status: {}", status);
                println!("API Error Body: {}", error_body);
                println!("Request URL: {}", url);
                println!("Detected Provider: {:?}", provider);
                return Err(());
            }

            let mut stream = res.bytes_stream();
            let mut full_message = String::new();

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

                                print!("{}", content.bright_green());
                                io::stdout().flush().unwrap();
                                full_message.push_str(&content);
                            }
                        }
                    }
                    Err(e) => {
                        println!("Stream Error: {}", e);
                    }
                }
            }

            println!();
            Ok(full_message)
        }
        Err(e) => {
            eprintln!("Request Error: {}", e);
            if e.is_builder() {
                eprintln!(
                    "This usually means BASE_URL is not a valid absolute URL. Current request URL: {}",
                    url
                );
            }
            Err(())
        }
    }
}

async fn handle_input(
    input: &str,
    current_chat: &str,
    history: &mut Vec<Message>,
    sandbox: &SandboxConfig,
) {
    history.push(Message {
        role: "user".to_string(),
        content: input.to_string(),
    });

    for step in 0..MAX_AGENT_STEPS {
        let full_message = match request_model_reply(history).await {
            Ok(message) => message,
            Err(()) => return,
        };

        if response_claims_destructive_action(&full_message) {
            let response = "Blocked destructive command: assistant claimed execution without a runtime tool result".to_string();
            println!("{}", response.bright_green());

            history.push(Message {
                role: "assistant".to_string(),
                content: response,
            });

            save_history(current_chat, history);
            return;
        }

        history.push(Message {
            role: "assistant".to_string(),
            content: full_message.clone(),
        });

        if let Ok(tool_call) = serde_json::from_str::<ToolCall>(&full_message) {
            let tool_result = run_tool(tool_call, sandbox);

            println!("{}", tool_result.bright_magenta());

            history.push(Message {
                role: "user".to_string(),
                content: format!("Tool result from TRUST runtime: {}", tool_result),
            });

            if step + 1 < MAX_AGENT_STEPS {
                continue;
            }

            println!(
                "{}",
                "Stopped after reaching the max autonomous tool steps for one turn."
                    .bright_yellow()
            );
        }

        save_history(current_chat, history);
        return;
    }
}

fn chat_path(chat_name: &str) -> String {
    format!("memory/{}.json", chat_name)
}

fn save_history(chat_name: &str, history: &Vec<Message>) {
    fs::create_dir_all("memory").unwrap();

    let json = serde_json::to_string_pretty(history).unwrap();

    fs::write(chat_path(chat_name), json).unwrap();
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
                        content: format!("Tool result from TRUST runtime: {}", message.content),
                    }
                } else {
                    message
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn list_chats() {
    let paths = fs::read_dir("memory");

    match paths {
        Ok(entries) => {
            println!("\nSaved Chats:\n");

            for entry in entries {
                let entry = entry.unwrap();
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                let chat_name = file_name.replace(".json", "");

                println!("- {}", chat_name.bright_cyan());
            }

            println!();
        }

        Err(_) => {
            println!("No chats found.");
        }
    }
}

fn delete_chat(chat_name: &str) {
    let path = chat_path(chat_name);

    let result = fs::remove_file(path);

    match result {
        Ok(_) => {
            println!("Deleted chat: {}", chat_name.bright_red());
        }

        Err(_) => {
            println!("Chat not found: {}", chat_name.bright_red());
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

fn run_sandboxed_command(config: &SandboxConfig, command: &str) -> String {
    let Some(allowed_command) = allowed_sandbox_command(command) else {
        return format!(
            "Blocked command: {}. Allowed sandbox commands: cargo check, cargo test, cargo fmt --check, cargo clippy",
            command
        );
    };

    let approval_prompt = format!(
        "\nApprove sandboxed command? {}\n  cwd: {}\n  timeout: {} ms\nType y/yes to allow: ",
        allowed_command.display,
        display_path(&config.workspace),
        config.command_timeout_ms
    );

    if !read_approval_input(&approval_prompt) {
        return format!("User denied sandboxed command: {}", allowed_command.display);
    }

    execute_command_with_timeout(
        config,
        allowed_command.program,
        allowed_command.args,
        allowed_command.display,
    )
}

fn run_tool(tool_call: ToolCall, sandbox: &SandboxConfig) -> String {
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

            run_sandboxed_command(sandbox, &command)
        }

        "run_command" => {
            let Some(command) = tool_call.args.command else {
                return "Missing command for run_command".to_string();
            };

            run_command(sandbox, &command)
        }

        _ => format!("Unknown tool: {}", tool_call.tool),
    }
}
fn credits() {
    println!("\n{}", "━".repeat(60).bright_black());
    println!("🤖 {}", "Terminal AI Assistant".bold().bright_cyan());
    println!(
        "{}\n",
        "Fast, Secure, and Memory Safe".italic().bright_black()
    );

    println!(
        "{}   {}",
        "Developer:".bright_yellow().bold(),
        "Daniel Santhosh".green()
    );
    println!(
        "{} {}",
        "Powered by:".bright_magenta().bold(),
        "Rust 🦀".bright_red()
    );
    println!(
        "{}  {}",
        "Repository:".bright_yellow().bold(),
        "https://github.com/danielzanthosh/trust"
            .underline()
            .bright_blue()
    );
    println!("{}\n", "━".repeat(60).bright_black());
}

#[tokio::main]
async fn main() {
    dotenv().ok();

    let sandbox = match SandboxConfig::load().and_then(|config| {
        ensure_sandbox_ready(&config)?;
        Ok(config)
    }) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Sandbox Error: {}", error);
            return;
        }
    };

    intro(&sandbox);

    let mut current_chat = "default".to_string();
    let mut history = load_history(&current_chat);

    history.push(Message {
        role: "system".to_string(),
        content: r#"
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

    Example read_file tool call:

    {
      "type": "tool_call",
      "tool": "read_file",
      "args": {
        "path": "outputs/test.md"
      }
    }

    Example list_directory tool call:

    {
      "type": "tool_call",
      "tool": "list_directory",
      "args": {
        "path": "outputs"
      }
    }

    Example open_app tool call:

    {
      "type": "tool_call",
      "tool": "open_app",
      "args": {
        "app": "chrome"
      }
    }

    Example open_app with URL:

    {
      "type": "tool_call",
      "tool": "open_app",
      "args": {
        "app": "chrome",
        "url": "https://www.google.com"
      }
    }

    Example sandboxed command:

    {
      "type": "tool_call",
      "tool": "run_sandboxed_command",
      "args": {
        "command": "cargo check"
      }
    }

    Example broader command request:

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
        .to_string(),
    });

    loop {
        print!("{} > ", current_chat.bright_cyan());
        io::stdout().flush().unwrap();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            eprintln!("Failed to read input");
            continue;
        }

        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "/exit" || input == "/quit" {
            break;
        }

        if input == "/list" {
            list_chats();
            continue;
        }

        if input == "/credits" {
            credits();
            continue;
        }

        if input == "/clear" {
            history.clear();
            save_history(&current_chat, &history);
            println!("Cleared chat: {}", current_chat.bright_red());
            continue;
        }

        if let Some(chat_name) = input.strip_prefix("/chat ") {
            let chat_name = chat_name.trim();

            if chat_name.is_empty() {
                println!("Usage: /chat <name>");
                continue;
            }

            current_chat = chat_name.to_string();
            history = load_history(&current_chat);
            println!("Switched to chat: {}", current_chat.bright_cyan());
            continue;
        }

        if let Some(chat_name) = input.strip_prefix("/delete ") {
            let chat_name = chat_name.trim();

            if chat_name.is_empty() {
                println!("Usage: /delete <name>");
                continue;
            }

            delete_chat(chat_name);

            if chat_name == current_chat {
                history.clear();
            }

            continue;
        }

        handle_input(input, &current_chat, &mut history, &sandbox).await;
    }
}

//    .------..------..------..------..------.
//    |T.--. ||R.--. ||U.--. ||S.--. ||T.--. |
//    | :/\: || :(): || (\/) || :/\: || :/\: |
//    | (__) || ()() || :\/: || :\/: || (__) |
//    | '--'T|| '--'R|| '--'U|| '--'S|| '--'T|
//    `------'`------'`------'`------'`------'

fn intro(sandbox: &SandboxConfig) {
    println!("{}", ".------..------..------..------..------.".red());
    println!(
        "{}",
        "|T.--. ||R.--. ||U.--. ||S.--. ||T.--. |".bright_red()
    );
    println!("{}", "| :/\\: || :(): || (\\/) || :/\\: || :/\\: |".red());
    println!("{}", "| (__) || ()() || :\\/: || :\\/: || (__) |".red());
    println!(
        "{}",
        "| '--'T|| '--'R|| '--'U|| '--'S|| '--'T|".bright_red()
    );
    println!("{}", "`------'`------'`------'`------'`------'".red());

    println!(
        "{}",
        "Commands: /list, /chat <name>, /delete <name>, /credits, /clear, /exit".bright_red()
    );
    println!(
        "{} {}",
        "Sandbox workspace:".bright_yellow(),
        display_path(&sandbox.workspace).bright_cyan()
    );
    println!(
        "{} {}\n",
        "Sandbox outputs:".bright_yellow(),
        display_path(&sandbox.outputs).bright_cyan()
    );
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
