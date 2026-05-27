// AI Assistant through the Terminal
// OpenAI, Anthropic API Compatible
// Custom Base URL, and API Key
// Works with Hackclub AI
// Fast, Secure, Memory Safety
// Features: Chat History, Infinite Memory

use colored::*;
use dotenvy::dotenv;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::Command;

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

async fn handle_input(input: &str, current_chat: &str, history: &mut Vec<Message>) {
    let api_key = env::var("API_KEY").unwrap_or_default();
    let base_url = env::var("BASE_URL").unwrap_or_default();
    let model = env::var("MODEL").unwrap_or_default();

    if api_key.trim().is_empty() {
        eprintln!("Missing API_KEY. Add it to your .env file or environment variables.");
        return;
    }

    if base_url.trim().is_empty() {
        eprintln!("Missing BASE_URL. Example: BASE_URL=https://api.openai.com");
        return;
    }

    if model.trim().is_empty() {
        eprintln!("Missing MODEL. Example: MODEL=gpt-4o-mini");
        return;
    }

    let base_url = base_url.trim().trim_end_matches('/');
    let url =
        if base_url.ends_with("/v1/chat/completions") || base_url.ends_with("/chat/completions") {
            base_url.to_string()
        } else {
            format!("{}/v1/chat/completions", base_url)
        };

    history.push(Message {
        role: "user".to_string(),
        content: input.to_string(),
    });

    let client = Client::new();

    let body = json!({
        "model": model,
        "messages": history,
        "stream": true
    });

    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await;

    match response {
        Ok(res) => {
            let status = res.status();

            if !status.is_success() {
                println!("API Error Status: {}", status);
                return;
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

                                let parsed: serde_json::Value =
                                    serde_json::from_str(&data).unwrap_or_default();

                                let content = parsed["choices"][0]["delta"]["content"]
                                    .as_str()
                                    .unwrap_or("");

                                print!("{}", content.bright_green());

                                io::stdout().flush().unwrap();

                                full_message.push_str(content);
                            }
                        }
                    }

                    Err(e) => {
                        println!("Stream Error: {}", e);
                    }
                }
            }

            println!();

            if let Ok(tool_call) = serde_json::from_str::<ToolCall>(&full_message) {
                let tool_result = run_tool(tool_call);

                println!("{}", tool_result.bright_magenta());

                history.push(Message {
                    role: "tool".to_string(),
                    content: tool_result,
                });
            }

            history.push(Message {
                role: "assistant".to_string(),
                content: full_message,
            });

            save_history(current_chat, history);
        }

        Err(e) => {
            eprintln!("Request Error: {}", e);
            if e.is_builder() {
                eprintln!(
                    "This usually means BASE_URL is not a valid absolute URL. Current request URL: {}",
                    url
                );
            }
        }
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
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| Vec::new()),
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

fn run_safe_command(command: &str) -> String {
    let normalized_command = command.trim().to_lowercase();

    let args: &[&str] = match normalized_command.as_str() {
        "date" => &["/C", "date", "/T"],
        "time" => &["/C", "time", "/T"],
        "whoami" => &["/C", "whoami"],
        "hostname" => &["/C", "hostname"],
        "version" | "ver" => &["/C", "ver"],
        "list_outputs" | "dir outputs" => &["/C", "dir", "outputs"],
        "list_memory" | "dir memory" => &["/C", "dir", "memory"],
        _ => {
            return format!(
                "Blocked command: {}. Allowed safe commands: date, time, whoami, hostname, ver, list_outputs, list_memory",
                command
            );
        }
    };

    match Command::new("cmd").args(args).output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

            if output.status.success() {
                if stdout.is_empty() {
                    "Command completed successfully with no output".to_string()
                } else {
                    stdout
                }
            } else if stderr.is_empty() {
                format!("Command failed with status: {}", output.status)
            } else {
                format!("Command failed: {}", stderr)
            }
        }
        Err(e) => format!("Failed to run safe command: {}", e),
    }
}

fn run_tool(tool_call: ToolCall) -> String {
    match tool_call.tool.as_str() {
        "write_file" => {
            let Some(path) = tool_call.args.path else {
                return "Missing path for write_file".to_string();
            };

            if !path.starts_with("outputs/") {
                return "Blocked unsafe path".to_string();
            }

            let content = tool_call.args.content.unwrap_or_default();

            if let Err(e) = fs::create_dir_all("outputs") {
                return format!("Failed to create outputs directory: {}", e);
            }

            match fs::write(&path, content) {
                Ok(_) => format!("Saved file: {}", path),
                Err(e) => format!("Failed to save file {}: {}", path, e),
            }
        }

        "open_chrome" => open_app("chrome", tool_call.args.url),

        "open_app" => {
            let Some(app) = tool_call.args.app else {
                return "Missing app for open_app".to_string();
            };

            open_app(&app, tool_call.args.url)
        }

        "run_safe_command" => {
            let Some(command) = tool_call.args.command else {
                return "Missing command for run_safe_command".to_string();
            };

            run_safe_command(&command)
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
    intro();

    let mut current_chat = "default".to_string();
    let mut history = load_history(&current_chat);

    history.push(Message {
        role: "system".to_string(),
        content: r#"
    You are TRUST.

    You are a helpful terminal AI with subtle, safe agentic control.
    You can answer normally, or use tools when the user asks you to do something supported.
    When using a tool, reply ONLY with JSON and no extra text.

    Example write_file tool call:

    {
      "type": "tool_call",
      "tool": "write_file",
      "args": {
        "path": "outputs/test.md",
        "content": "Hello world"
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

    Example safe command:

    {
      "type": "tool_call",
      "tool": "run_safe_command",
      "args": {
        "command": "date"
      }
    }

    Allowed tools:
    - write_file: only writes inside outputs/
    - open_app: opens allowed apps only. Allowed apps: chrome, edge, firefox, notepad, calculator, explorer, paint, wordpad, vscode
    - open_chrome: alias for opening Chrome
    - run_safe_command: runs allowlisted read-only commands only. Allowed commands: date, time, whoami, hostname, ver, list_outputs, list_memory

    Safety rules:
    - Do not claim you cannot interact with the computer when an allowed tool can do the task.
    - Do not perform destructive actions.
    - Do not delete files, modify system settings, run arbitrary shell commands, install software, download files, or access private data.
    - If the user asks for an unsupported or destructive command, explain that it is blocked for safety and offer a safe alternative.
    - Never pretend to save files, open apps, or run commands. Use a tool when available.
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

        handle_input(input, &current_chat, &mut history).await;
    }
}

//    .------..------..------..------..------.
//    |T.--. ||R.--. ||U.--. ||S.--. ||T.--. |
//    | :/\: || :(): || (\/) || :/\: || :/\: |
//    | (__) || ()() || :\/: || :\/: || (__) |
//    | '--'T|| '--'R|| '--'U|| '--'S|| '--'T|
//    `------'`------'`------'`------'`------'

fn intro() {
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
        "Commands: /list, /chat <name>, /delete <name>, /credits, /clear, /exit\n".bright_red()
    );
}
