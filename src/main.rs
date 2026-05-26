// AI Assistant through the Terminal
// OpenAI, Anthropic API Compatible
// Custom Base URL, and API Key
// Works with Hackclub AI
// Fast, Secure, Memory Safety
// Features: Chat History, Infinite Memory

use colored::Colorize;
use dotenvy::dotenv;
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

#[tokio::main]
async fn main() {
    dotenv().ok();
    println!("{}", "Welcome to TRUST!".bright_blue());
    println!("{}", "This is a dev version".bright_black());
    println!(
        "{}",
        "Please check the README for instructions.".bright_black()
    );

    let mut current_chat = "default".to_string();
    let mut history: Vec<Message> = load_history(&current_chat);

    loop {
        print!("Enter your message: ");
        io::stdout().flush().unwrap();
        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
        let input = input.trim();
        if input == "/exit" {
            println!("{}", "Goodbye!".blue());
            break;
        } else if input == "/credits" {
            credits();
        } else {
            handle_input(input, &mut history).await;
        }
    }
}

async fn handle_input(input: &str, history: &mut Vec<Message>) {
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
        "messages": history
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

            let json: serde_json::Value = res.json().await.unwrap_or_default();

            // This is for Debugging
            if !status.is_success() {
                println!("API Error Status: {}", status);
                println!("API Error Body: {:#}", json);
                return;
            }

            let ai_message = json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("No message found");

            println!("{}", ai_message.bright_green());

            history.push(Message {
                role: "assistant".to_string(),
                content: ai_message.to_string(),
            });

            save_history(history);
        }

        Err(e) => {
            eprintln!("Request Error: {}", e);
        }
    }
}

fn save_history(history: &Vec<Message>) {
    fs::create_dir_all("memory").unwrap();

    let json = serde_json::to_string(history).unwrap();

    fs::write("memory/history.json", json).unwrap();
}

fn load_history() -> Vec<Message> {
    let data = fs::read_to_string("memory/history.json");
    match data {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| Vec::new()),
        Err(_) => Vec::new(),
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
