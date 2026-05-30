use reqwest::Client;

use serde_json::json;

use std::fs;

use tokio::sync::mpsc;

use crate::history::*;

use crate::model::*;

use crate::runtime::sandbox::*;

use crate::types::*;

pub(crate) fn strip_json_code_fence(response: &str) -> &str {
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

pub(crate) fn parse_tool_call_response(response: &str) -> Result<ToolCall, serde_json::Error> {
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

pub(crate) fn latest_user_runtime_action_request(history: &[Message]) -> Option<&str> {
    history
        .iter()
        .rev()
        .filter(|message| {
            message.role == "user" && !message.content.starts_with(TOOL_RESULT_PREFIX)
        })
        .map(|message| message.content.as_str())
        .find(|content| user_requested_runtime_action(content))
}

pub(crate) fn user_requested_runtime_action(input: &str) -> bool {
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

pub(crate) fn response_looks_like_tool_avoidance(response: &str) -> bool {
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
        "i've opened",
        "i have opened",
        "i've navigated",
        "i have navigated",
        "i navigated",
        "i've gone to",
        "i have gone to",
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
        "opened chrome for you",
        "opened the website",
        "navigated to the",
        "none required for this interaction",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn extract_url_like(input: &str) -> Option<String> {
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

pub(crate) fn inferred_website_url(input: &str) -> Option<String> {
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

pub(crate) fn friendly_tool_summary(tool_call: &ToolCall, tool_result: &str) -> String {
    if tool_result.starts_with("User declined") {
        return "Command declined.".to_string();
    }

    if tool_result.starts_with("Blocked command:")
        || tool_result.starts_with("This command was blocked")
        || tool_result.starts_with("Kimi WebBridge is not available")
        || tool_result.contains("failed")
    {
        return tool_result.to_string();
    }

    match tool_call.tool.as_str() {
        "run_command" => {
            let command = tool_call
                .args
                .command
                .as_deref()
                .unwrap_or_default()
                .to_lowercase();

            if command.contains("chrome") {
                "I've opened Chrome for you.".to_string()
            } else if command.contains("notepad") {
                "I've opened Notepad for you.".to_string()
            } else if command.contains("calc") {
                "I've opened Calculator for you.".to_string()
            } else if command.contains("msedge") {
                "I've opened Edge for you.".to_string()
            } else {
                "Done. The command ran successfully.".to_string()
            }
        }

        "kimi_webbridge" => match tool_call.args.action.as_deref().unwrap_or("navigate") {
            "navigate" | "open" => "I've opened the page for you.".to_string(),

            "click" => "I've clicked it for you.".to_string(),

            "fill" | "type" => "I've filled that in for you.".to_string(),

            "snapshot" | "read" => "I've read the page for you.".to_string(),

            _ => "Done.".to_string(),
        },

        _ => "Done.".to_string(),
    }
}

pub(crate) fn inferred_tool_call_for_request(user_request: &str) -> Option<ToolCall> {
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

pub(crate) fn request_mentions_installed_app(user_request: &str) -> bool {
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

pub(crate) fn tool_call_matches_request(user_request: &str, tool_call: &ToolCall) -> bool {
    if request_mentions_installed_app(user_request) {
        return tool_call.tool == "run_command";
    }

    true
}

pub(crate) fn tool_reprompt_message(user_request: &str) -> String {
    format!(
        "Runtime correction: The user requested a real computer/browser action, but your last response did not correctly call the required tool. Do not give a tutorial or say you cannot do it. Use the available runtime tools now. For opening installed apps like Chrome or Notepad use run_command with PowerShell Start-Process. For navigating, reading, scrolling, clicking, or filling webpages use kimi_webbridge. If Kimi WebBridge is unavailable, call it anyway so the runtime can return the installation/setup message. User request: {}",
        user_request
    )
}

pub(crate) fn normalize_assistant_response(response: &str) -> String {
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

pub(crate) fn response_claims_destructive_action(response: &str) -> bool {
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

pub(crate) fn allowed_sandbox_command(command: &str) -> Option<AllowedCommand> {
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

pub(crate) async fn run_sandboxed_command(
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

pub(crate) fn webbridge_unavailable_message(error: &str) -> String {
    format!(
        "Kimi WebBridge is not available or not connected. Install the Kimi WebBridge browser extension, install/start the device daemon, then retry. Check it with `~/.kimi-webbridge/bin/kimi-webbridge status`. Details: {}",
        error
    )
}

pub(crate) async fn send_webbridge_command(
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

pub(crate) async fn run_webbridge_tool(tool_call: ToolCall) -> String {
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

pub(crate) fn list_runtime_directory(config: &SandboxConfig, requested_path: &str) -> String {
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

pub(crate) fn read_runtime_file(config: &SandboxConfig, requested_path: &str) -> String {
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

pub(crate) async fn run_tool(
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

pub(crate) async fn process_turn(
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

                let tool_result = run_tool(tool_call.clone(), &sandbox, &event_tx).await;

                let _ = event_tx.send(RuntimeEvent::ToolResult(tool_result.clone()));

                history.push(Message {
                    role: "user".to_string(),

                    content: format!("{}{}", TOOL_RESULT_PREFIX, tool_result.clone()),
                });

                let summary = friendly_tool_summary(&tool_call, &tool_result);

                history.push(Message {
                    role: "assistant".to_string(),

                    content: summary.clone(),
                });

                let _ = event_tx.send(RuntimeEvent::CommitAssistant(summary));

                break;
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

pub(crate) fn ensure_system_message(mut history: Vec<Message>) -> Vec<Message> {
    let prompt = crate::prompt::system_prompt();

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
