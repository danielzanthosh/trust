// AI Assistant through the Terminal
// OpenAI, Anthropic API Compatible
// Custom Base URL, and API Key
// Works with Hackclub AI
// Fast, Secure, Memory Safety
// Features: Chat History, Infinite Memory

use colored::Colorize;
use dotenvy::dotenv;
use reqwest::Client;
use serde_json::json;
use std::env;
use std::io::{self, Write};

#[tokio::main]
async fn main() {
    dotenv().ok();
    println!("{}", "Welcome to TRUST!".bright_blue());
    println!("{}", "This is a dev version".bright_black());
    println!(
        "{}",
        "Please check the README for instructions.".bright_black()
    );

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
            handle_input(input).await;
        }
    }
}

async fn handle_input(input: &str) {
    // println!("Sending to AI: {}", input);
    let api_key = env::var("API_KEY").unwrap_or_default();
    let base_url = env::var("BASE_URL").unwrap_or_default();
    let model = env::var("MODEL").unwrap_or_default();

    let client = Client::new();

    let body = json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": input
            }
        ]
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
            let json: serde_json::Value = res.json().await.unwrap_or_default();
            let ai_message = json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default();
            println!("{}", ai_message.bright_green());
        }
        Err(e) => {
            eprintln!("Error: {}", e);
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
        "Rust 🦀".bright_red() // Note: Use .bright_red() if your colored version doesn't have orange
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
