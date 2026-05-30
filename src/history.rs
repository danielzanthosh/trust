use std::fs;

use crate::runtime::tools::*;

use crate::types::*;

pub(crate) fn chat_path(chat_name: &str) -> String {
    format!("memory/{}.json", chat_name)
}

pub(crate) fn save_history(chat_name: &str, history: &[Message]) {
    let _ = fs::create_dir_all("memory");

    if let Ok(json) = serde_json::to_string_pretty(history) {
        let _ = fs::write(chat_path(chat_name), json);
    }
}

pub(crate) fn load_history(chat_name: &str) -> Vec<Message> {
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

pub(crate) fn list_chat_names() -> Vec<String> {
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

pub(crate) fn delete_chat(chat_name: &str) -> Result<(), String> {
    fs::remove_file(chat_path(chat_name)).map_err(|_| format!("Chat not found: {}", chat_name))
}

pub(crate) fn is_tool_call_json(content: &str) -> bool {
    parse_tool_call_response(content).is_ok()
}

pub(crate) fn ui_messages_from_history(history: &[Message]) -> Vec<UiMessage> {
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
