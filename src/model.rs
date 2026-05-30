use futures_util::StreamExt;

use reqwest::Client;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};

use serde_json::json;

use std::env;

use tokio::sync::mpsc;

use crate::types::*;

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

pub(crate) async fn request_model_reply(
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
