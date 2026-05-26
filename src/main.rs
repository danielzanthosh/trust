// AI Assistant through the Terminal
// OpenAI, Anthropic API Compatible
// Custom Base URL, and API Key
// Works with Hackclub AI
// Fast, Secure, Memory Safety
// Features: Chat History, Infinite Memory

use colored::Colorize;
use dotenvy::dotenv;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::fs;
use std::io::{self, Write};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Message {
    role: String,
    content: String,
}

async fn handle_input(input: &str, current_chat: &str, history: &mut Vec<Message>) {
    let api_key = env::var("API_KEY").unwrap_or_default();
    let base_url = env::var("BASE_URL").unwrap_or_default();
    let model = env::var("MODEL").unwrap_or_default();

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

    let url = format!("{}/v1/chat/completions", base_url);

    let response = client
        .post(url)
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

            history.push(Message {
                role: "assistant".to_string(),
                content: full_message,
            });

            save_history(current_chat, history);
        }

        Err(e) => {
            eprintln!("Request Error: {}", e);
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
    credits();

    println!("Commands: /list, /chat <name>, /delete <name>, /clear, /exit\n");

    let mut current_chat = "default".to_string();
    let mut history = load_history(&current_chat);

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
