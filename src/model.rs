use futures_util::StreamExt;

use reqwest::Client;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};

use serde_json::json;

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use tokio::sync::mpsc;

use crate::types::*;

pub(crate) fn model_config_path() -> PathBuf {
    PathBuf::from("config").join("models.json")
}

fn command_exists(path: &PathBuf) -> bool {
    path.exists() && path.is_file()
}

fn codex_cli_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("CODEX_BIN").map(PathBuf::from)
        && command_exists(&path)
    {
        return Some(path);
    }

    let executable_names: &[&str] = if cfg!(windows) {
        &["codex.cmd", "codex.exe", "codex.bat", "codex"]
    } else {
        &["codex"]
    };

    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            for name in executable_names {
                let candidate = dir.join(name);
                if command_exists(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }

    let mut candidates = Vec::new();

    if cfg!(windows) {
        if let Some(appdata) = env::var_os("APPDATA").map(PathBuf::from) {
            for name in executable_names {
                candidates.push(appdata.join("npm").join(name));
            }
        }
    }

    if let Some(home) = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    {
        for name in executable_names {
            candidates.push(home.join("AppData").join("Roaming").join("npm").join(name));
            candidates.push(home.join(".local").join("bin").join(name));
            candidates.push(home.join(".npm-global").join("bin").join(name));
        }
    }

    candidates.into_iter().find(command_exists)
}

fn codex_command() -> Result<Command, String> {
    let path = codex_cli_path().ok_or_else(|| {
        "Codex CLI was not found. Install Codex or set CODEX_BIN to the full path, e.g. C:\\Users\\<you>\\AppData\\Roaming\\npm\\codex.cmd".to_string()
    })?;

    let mut command = Command::new(path);
    command.env("NO_COLOR", "1").env("CLICOLOR", "0");
    Ok(command)
}

fn strip_ansi_codes(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            }
        }

        output.push(ch);
    }

    output
}

pub(crate) fn codex_device_code_path() -> PathBuf {
    PathBuf::from("config").join("codex_device_code.txt")
}

pub(crate) fn read_last_codex_device_code() -> Result<String, String> {
    fs::read_to_string(codex_device_code_path())
        .map(|code| code.trim().to_string())
        .map_err(|_| {
            "No Codex device code is currently stored. Run /config codex first.".to_string()
        })
}

fn save_last_codex_device_code(code: &str) {
    let path = codex_device_code_path();

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let _ = fs::write(path, code);
}

fn delete_last_codex_device_code() {
    let _ = fs::remove_file(codex_device_code_path());
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    if cfg!(windows) {
        let mut child = Command::new("cmd")
            .args(["/C", "clip"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Failed to start clipboard command: {}", error))?;

        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|error| format!("Failed to write to clipboard: {}", error))?;
        }

        let status = child
            .wait()
            .map_err(|error| format!("Failed waiting for clipboard command: {}", error))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!("Clipboard command exited with {}", status))
        }
    } else {
        Err("Clipboard auto-copy is currently implemented for Windows only.".to_string())
    }
}

fn codex_device_login_message(output: &[String]) -> String {
    let clean = strip_ansi_codes(&output.join("\n"));
    let url = clean
        .split_whitespace()
        .find(|part| part.starts_with("http://") || part.starts_with("https://"))
        .unwrap_or("https://auth.openai.com/codex/device");

    let code = clean
        .lines()
        .flat_map(|line| line.split_whitespace())
        .find_map(|part| {
            let normalized = part.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-');

            (normalized.contains('-')
                && normalized.len() >= 8
                && normalized
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '-'))
            .then(|| normalized.to_string())
        });

    let code_status = if let Some(code) = code.as_deref() {
        save_last_codex_device_code(code);

        match copy_to_clipboard(code) {
            Ok(()) => "Copied one-time code to clipboard.".to_string(),
            Err(error) => format!(
                "Could not copy one-time code automatically: {}\nType /config codex code to show it.",
                error
            ),
        }
    } else {
        "Could not detect the one-time code. Type /config codex code to show the last stored code if available.".to_string()
    };

    format!(
        "Codex OAuth login\n\nOpen this link:\n{}\n\n{}\nType /config codex code to show the code if clipboard paste fails.\n\nTRUST will import available Codex models after login completes.",
        url, code_status
    )
}

pub(crate) fn load_app_config() -> AppConfig {
    let path = model_config_path();

    fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<AppConfig>(&content).ok())
        .unwrap_or_else(|| AppConfig {
            active_model: None,
            models: Vec::new(),
        })
}

pub(crate) fn save_app_config(config: &AppConfig) -> Result<(), String> {
    let path = model_config_path();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create config directory {}: {}",
                parent.display(),
                error
            )
        })?;
    }

    let json = serde_json::to_string_pretty(config)
        .map_err(|error| format!("Failed to serialize model config: {}", error))?;

    fs::write(&path, json)
        .map_err(|error| format!("Failed to write model config {}: {}", path.display(), error))
}

fn legacy_env_model_config() -> Option<ModelConfig> {
    let base_url = env::var("BASE_URL").unwrap_or_default();
    let model = env::var("MODEL").unwrap_or_default();

    if base_url.trim().is_empty() || model.trim().is_empty() {
        return None;
    }

    let api_key = env::var("API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let auth_mode = env::var("AUTH_MODE")
        .ok()
        .filter(|value| !value.trim().is_empty());

    Some(ModelConfig {
        name: "env".to_string(),
        base_url,
        model,
        api_key,
        auth_mode,
        priority: u32::MAX,
    })
}

pub(crate) fn configured_models() -> Vec<ModelConfig> {
    let mut config = load_app_config();

    if config.models.is_empty()
        && let Some(env_model) = legacy_env_model_config()
    {
        config.models.push(env_model);
    }

    config.models
}

pub(crate) fn ordered_model_fallbacks() -> Vec<ModelConfig> {
    let config = load_app_config();
    let mut models = configured_models();

    if let Some(active) = config.active_model.as_deref()
        && let Some(index) = models.iter().position(|model| model.name == active)
    {
        let selected = models.remove(index);
        models.sort_by_key(|model| model.priority);
        models.insert(0, selected);
        return models;
    }

    models.sort_by_key(|model| model.priority);
    models
}

pub(crate) fn set_active_model(name: &str) -> Result<(), String> {
    let mut config = load_app_config();

    if !config.models.iter().any(|model| model.name == name) {
        return Err(format!("Unknown model config: {}", name));
    }

    config.active_model = Some(name.to_string());
    save_app_config(&config)
}

pub(crate) fn upsert_model_config(model: ModelConfig, make_active: bool) -> Result<(), String> {
    let mut config = load_app_config();

    if let Some(existing) = config
        .models
        .iter_mut()
        .find(|existing| existing.name == model.name)
    {
        *existing = model.clone();
    } else {
        config.models.push(model.clone());
    }

    if make_active || config.active_model.is_none() {
        config.active_model = Some(model.name);
    }

    save_app_config(&config)
}

pub(crate) fn describe_models() -> String {
    let config = load_app_config();
    let mut models = configured_models();
    models.sort_by_key(|model| model.priority);

    if models.is_empty() {
        return "No models configured. Use /config model <name> base_url=<url> model=<model> api_key=<key> priority=<n> or /config codex <name> model=<model>.".to_string();
    }

    models
        .into_iter()
        .map(|model| {
            let active = if config.active_model.as_deref() == Some(model.name.as_str()) {
                "*"
            } else {
                " "
            };
            let auth = model.auth_mode.as_deref().unwrap_or_else(|| {
                if model
                    .api_key
                    .as_deref()
                    .is_some_and(|key| !key.trim().is_empty())
                {
                    "api_key"
                } else {
                    "env/codex"
                }
            });

            format!(
                "{} {} · priority={} · model={} · base_url={} · auth={}",
                active, model.name, model.priority, model.model, model.base_url, auth
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn parse_key_values(input: &str) -> Vec<(String, String)> {
    input
        .split_whitespace()
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.trim().to_lowercase(), value.trim().to_string()))
        })
        .collect()
}

pub(crate) fn key_value<'a>(values: &'a [(String, String)], key: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.as_str())
}

pub(crate) fn parse_model_config_command(args: &str) -> Result<(ModelConfig, bool), String> {
    let mut parts = args.split_whitespace();
    let name = parts
        .next()
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "Usage: /config model <name> base_url=<url> model=<model> [api_key=<key>|auth=codex] [priority=<n>] [active=true]".to_string())?;

    let rest = parts.collect::<Vec<_>>().join(" ");
    let values = parse_key_values(&rest);

    let base_url = key_value(&values, "base_url")
        .or_else(|| key_value(&values, "url"))
        .ok_or_else(|| "Missing base_url=<url>".to_string())?
        .to_string();

    let model = key_value(&values, "model")
        .ok_or_else(|| "Missing model=<model>".to_string())?
        .to_string();

    let api_key = key_value(&values, "api_key")
        .or_else(|| key_value(&values, "key"))
        .filter(|value| *value != "-" && !value.eq_ignore_ascii_case("codex"))
        .map(str::to_string);

    let auth_mode = key_value(&values, "auth")
        .or_else(|| key_value(&values, "auth_mode"))
        .map(str::to_string)
        .or_else(|| {
            key_value(&values, "api_key")
                .filter(|value| value.eq_ignore_ascii_case("codex"))
                .map(|_| "codex".to_string())
        });

    let priority = key_value(&values, "priority")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(100);

    let make_active = key_value(&values, "active")
        .map(|value| matches!(value.to_lowercase().as_str(), "true" | "yes" | "1"))
        .unwrap_or(false);

    Ok((
        ModelConfig {
            name: name.to_string(),
            base_url,
            model,
            api_key,
            auth_mode,
            priority,
        },
        make_active,
    ))
}

fn codex_models_from_json(content: &str) -> Result<Vec<ModelConfig>, String> {
    let parsed: serde_json::Value = serde_json::from_str(content)
        .map_err(|error| format!("Failed to parse Codex model catalog: {}", error))?;

    let models = parsed
        .get("models")
        .and_then(|models| models.as_array())
        .ok_or_else(|| "Codex model catalog did not contain a models array.".to_string())?;

    let mut configs = Vec::new();

    for (index, model) in models.iter().enumerate() {
        let Some(slug) = model.get("slug").and_then(|value| value.as_str()) else {
            continue;
        };

        if model
            .get("supported_in_api")
            .and_then(|value| value.as_bool())
            == Some(false)
        {
            continue;
        }

        if model
            .get("visibility")
            .and_then(|value| value.as_str())
            .is_some_and(|visibility| visibility == "hidden")
        {
            continue;
        }

        let priority = model
            .get("priority")
            .and_then(|value| value.as_u64())
            .map(|priority| priority as u32)
            .unwrap_or(index as u32);

        configs.push(ModelConfig {
            name: format!("codex:{}", slug),
            base_url: "https://api.openai.com/v1/responses".to_string(),
            model: slug.to_string(),
            api_key: None,
            auth_mode: Some("codex".to_string()),
            priority,
        });
    }

    configs.sort_by_key(|model| model.priority);
    Ok(configs)
}

pub(crate) fn import_codex_models() -> Result<String, String> {
    let output = codex_command()?
        .args(["debug", "models"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Failed to run `codex debug models`: {}", error))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Err(format!(
            "`codex debug models` failed with {}: {}{}",
            output.status,
            stdout.trim(),
            stderr.trim()
        ));
    }

    delete_last_codex_device_code();

    let models = codex_models_from_json(&stdout)?;

    if models.is_empty() {
        return Err("Codex returned no API-supported models.".to_string());
    }

    let mut config = load_app_config();

    for model in &models {
        if let Some(existing) = config
            .models
            .iter_mut()
            .find(|existing| existing.name == model.name)
        {
            *existing = model.clone();
        } else {
            config.models.push(model.clone());
        }
    }

    if config.active_model.is_none() {
        config.active_model = Some(models[0].name.clone());
    }

    save_app_config(&config)?;

    Ok(format!(
        "Imported {} Codex OAuth models:\n{}",
        models.len(),
        models
            .iter()
            .map(|model| format!(
                "- {} ({}) priority={}",
                model.name, model.model, model.priority
            ))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

pub(crate) fn codex_login_status() -> Result<String, String> {
    let output = codex_command()?
        .args(["login", "status"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Failed to run `codex login status`: {}", error))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!("{}{}", stdout, stderr))
    }
}

pub(crate) fn start_codex_device_login(event_tx: mpsc::UnboundedSender<RuntimeEvent>) {
    thread::spawn(move || {
        let mut command = match codex_command() {
            Ok(command) => command,
            Err(error) => {
                let _ = event_tx.send(RuntimeEvent::Info(error));
                return;
            }
        };

        let mut child = match command
            .args(["login", "--device-auth"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = event_tx.send(RuntimeEvent::Info(format!(
                    "Failed to start Codex OAuth login: {}",
                    error
                )));
                return;
            }
        };

        let mut initial_output = Vec::new();

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);

            for line in reader.lines().map_while(Result::ok) {
                let clean_line = strip_ansi_codes(&line);
                initial_output.push(clean_line.clone());

                if clean_line.contains("Never share this code") || clean_line.contains("Logged in")
                {
                    let _ = event_tx.send(RuntimeEvent::Info(codex_device_login_message(
                        &initial_output,
                    )));
                }
            }
        }

        match child.wait() {
            Ok(status) if status.success() => match import_codex_models() {
                Ok(summary) => {
                    delete_last_codex_device_code();

                    let _ = event_tx.send(RuntimeEvent::Info(format!(
                        "Codex OAuth login completed.\n{}\n\nOne-time code was removed from local storage.",
                        summary
                    )));
                    let _ =
                        event_tx.send(RuntimeEvent::Status("Codex OAuth configured".to_string()));
                }
                Err(error) => {
                    let _ = event_tx.send(RuntimeEvent::Info(format!(
                        "Codex OAuth login completed, but model import failed: {}",
                        error
                    )));
                }
            },
            Ok(status) => {
                let _ = event_tx.send(RuntimeEvent::Info(format!(
                    "Codex OAuth login exited with status: {}",
                    status
                )));
            }
            Err(error) => {
                let _ = event_tx.send(RuntimeEvent::Info(format!(
                    "Failed waiting for Codex OAuth login: {}",
                    error
                )));
            }
        }
    });
}

pub(crate) fn parse_codex_config_command(args: &str) -> Result<(ModelConfig, bool), String> {
    let mut parts = args.split_whitespace();
    let first = parts.next().unwrap_or("codex");
    let (name, rest) = if first.contains('=') {
        ("codex".to_string(), args.to_string())
    } else {
        (first.to_string(), parts.collect::<Vec<_>>().join(" "))
    };

    let values = parse_key_values(&rest);
    let model = key_value(&values, "model").unwrap_or("gpt-5").to_string();
    let base_url = key_value(&values, "base_url")
        .or_else(|| key_value(&values, "url"))
        .unwrap_or("https://api.openai.com/v1/responses")
        .to_string();
    let priority = key_value(&values, "priority")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let make_active = key_value(&values, "active")
        .map(|value| matches!(value.to_lowercase().as_str(), "true" | "yes" | "1"))
        .unwrap_or(true);

    Ok((
        ModelConfig {
            name,
            base_url,
            model,
            api_key: None,
            auth_mode: Some("codex".to_string()),
            priority,
        },
        make_active,
    ))
}

pub(crate) fn detect_provider(base_url: &str) -> Provider {
    let normalized = base_url.trim().to_lowercase();

    if normalized.contains("anthropic") || normalized.ends_with("/v1/messages") {
        Provider::AnthropicCompatible
    } else if normalized.ends_with("/v1/responses") || normalized.ends_with("/responses") {
        Provider::OpenAiResponsesCompatible
    } else {
        Provider::OpenAiCompatible
    }
}

pub(crate) fn build_request_url(base_url: &str, provider: Provider) -> String {
    let normalized = base_url.trim().trim_end_matches('/');

    if normalized.ends_with("/v1/chat/completions")
        || normalized.ends_with("/chat/completions")
        || normalized.ends_with("/v1/responses")
        || normalized.ends_with("/responses")
        || normalized.ends_with("/v1/messages")
    {
        normalized.to_string()
    } else {
        match provider {
            Provider::OpenAiCompatible => format!("{}/v1/chat/completions", normalized),

            Provider::OpenAiResponsesCompatible => format!("{}/v1/responses", normalized),

            Provider::AnthropicCompatible => format!("{}/v1/messages", normalized),
        }
    }
}

pub(crate) fn build_headers(api_key: &str, provider: Provider) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    match provider {
        Provider::OpenAiCompatible | Provider::OpenAiResponsesCompatible => {
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

pub(crate) fn max_tokens() -> u64 {
    env::var("MAX_TOKENS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

pub(crate) fn build_request_body(
    model: &str,
    history: &[Message],
    provider: Provider,
) -> serde_json::Value {
    let max_tokens = max_tokens();

    match provider {
        Provider::OpenAiCompatible => json!({

            "model": model,

            "messages": history,

            "stream": true,

            "max_tokens": max_tokens,

            "stop": MODEL_STOP_SEQUENCES

        }),

        Provider::OpenAiResponsesCompatible => {
            let instructions = history
                .iter()
                .filter(|message| message.role == "system")
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");

            let input = history
                .iter()
                .filter(|message| message.role != "system")
                .map(|message| {
                    json!({
                        "role": message.role,
                        "content": message.content,
                    })
                })
                .collect::<Vec<_>>();

            json!({

                "model": model,

                "instructions": instructions,

                "input": input,

                "stream": true,

                "max_output_tokens": max_tokens,

                "truncation": "auto"

            })
        }

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

pub(crate) fn extract_stream_text(data: &str) -> String {
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

    // OpenAI Responses API streaming: response.output_text.delta -> delta

    if parsed["type"].as_str() == Some("response.output_text.delta") {
        if let Some(content) = parsed["delta"].as_str() {
            return content.to_string();
        }
    }

    if let Some(content) = parsed["output_text"].as_str() {
        return content.to_string();
    }

    if let Some(content) = parsed["response"]["output"][0]["content"][0]["text"].as_str() {
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

fn codex_auth_path() -> Option<PathBuf> {
    env::var_os("CODEX_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .map(|path| path.join("auth.json"))
        })
        .or_else(|| {
            env::var_os("HOME")
                .or_else(|| env::var_os("USERPROFILE"))
                .map(PathBuf::from)
                .map(|path| path.join(".codex").join("auth.json"))
        })
}

fn codex_oauth_token() -> Result<Option<String>, String> {
    let Some(path) = codex_auth_path() else {
        return Ok(None);
    };

    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path).map_err(|error| {
        format!(
            "Failed to read Codex auth file {}: {}",
            path.display(),
            error
        )
    })?;

    let parsed: serde_json::Value = serde_json::from_str(&content).map_err(|error| {
        format!(
            "Failed to parse Codex auth file {}: {}",
            path.display(),
            error
        )
    })?;

    Ok(parsed
        .get("tokens")
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(|token| token.as_str())
        .filter(|token| !token.trim().is_empty())
        .map(str::to_string))
}

fn resolve_api_key(config: &ModelConfig) -> Result<String, String> {
    if let Some(api_key) = config
        .api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(api_key.to_string());
    }

    let auth_mode = config
        .auth_mode
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();

    let env_api_key = env::var("API_KEY").unwrap_or_default();

    if !env_api_key.trim().is_empty() && auth_mode != "codex" {
        return Ok(env_api_key);
    }
    let openai_base = config.base_url.to_lowercase().contains("openai.com");

    if (auth_mode == "codex" || openai_base)
        && let Some(token) = codex_oauth_token()?
    {
        return Ok(token);
    }

    if auth_mode == "codex" || openai_base {
        Err(format!(
            "Model '{}' needs Codex OAuth, but no token was found at ~/.codex/auth.json. Run Codex login first or set api_key=... .",
            config.name
        ))
    } else {
        Err(format!(
            "Model '{}' is missing an API key. Configure api_key=... or set API_KEY.",
            config.name
        ))
    }
}

async fn request_model_reply_with_config(
    config: &ModelConfig,
    history: &[Message],
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> Result<String, String> {
    if config.base_url.trim().is_empty() {
        return Err(format!("Model '{}' is missing base_url.", config.name));
    }

    if config.model.trim().is_empty() {
        return Err(format!("Model '{}' is missing model.", config.name));
    }

    let api_key = resolve_api_key(config)?;
    let provider = detect_provider(&config.base_url);
    let url = build_request_url(&config.base_url, provider);
    let client = Client::new();
    let body = build_request_body(&config.model, history, provider);
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
                    "Model '{}' API error status: {}\n\nAPI error body:\n{}\n\nRequest URL:\n{}\n\nDetected provider: {:?}",
                    config.name, status, error_body, url, provider
                ));
            }

            let mut stream = res.bytes_stream();
            let mut full_message = String::new();
            let mut sse_buffer = String::new();

            let _ = event_tx.send(RuntimeEvent::Status(format!(
                "Streaming from model: {}",
                config.name
            )));
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
                        return Err(format!("Model '{}' stream error: {}", config.name, error));
                    }
                }
            }

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
                    "Model '{}' request error: {}\nThis usually means BASE_URL is not a valid absolute URL. Current request URL: {}",
                    config.name, error, url
                ))
            } else {
                Err(format!("Model '{}' request error: {}", config.name, error))
            }
        }
    }
}

pub(crate) async fn request_model_reply(
    history: &[Message],
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) -> Result<String, String> {
    let models = ordered_model_fallbacks();

    if models.is_empty() {
        return Err("No models configured. Use /config model <name> base_url=<url> model=<model> api_key=<key> or /config codex <name> model=<model>.".to_string());
    }

    let mut errors = Vec::new();

    for config in models {
        let _ = event_tx.send(RuntimeEvent::Status(format!(
            "Trying model: {}",
            config.name
        )));

        match request_model_reply_with_config(&config, history, event_tx).await {
            Ok(message) => return Ok(message),
            Err(error) => errors.push(error),
        }
    }

    Err(format!(
        "All configured models failed:\n{}",
        errors.join("\n\n---\n")
    ))
}
