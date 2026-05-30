use crate::runtime::tools::*;

use crate::history::*;

use crate::types::*;

pub(crate) struct App {
    pub(crate) current_chat: String,

    pub(crate) model_history: Vec<Message>,

    pub(crate) visible_messages: Vec<UiMessage>,

    pub(crate) input: String,

    pub(crate) draft_assistant: String,

    pub(crate) status: String,

    pub(crate) transcript_scroll: u16,

    pub(crate) auto_scroll: bool,

    pub(crate) should_quit: bool,

    pub(crate) busy: bool,

    pub(crate) pending_approval: Option<PendingApproval>,

    pub(crate) sandbox: SandboxConfig,
}

impl App {
    pub(crate) fn new(sandbox: SandboxConfig) -> Self {
        let current_chat = "default".to_string();

        let model_history = ensure_system_message(load_history(&current_chat));

        let visible_messages = ui_messages_from_history(&model_history);

        Self {
            current_chat,

            model_history,

            visible_messages,

            input: String::new(),

            draft_assistant: String::new(),

            status: "Ready".to_string(),

            transcript_scroll: u16::MAX,

            auto_scroll: true,

            should_quit: false,

            busy: false,

            pending_approval: None,

            sandbox,
        }
    }

    pub(crate) fn set_chat(&mut self, chat_name: String) {
        self.current_chat = chat_name;

        self.model_history = ensure_system_message(load_history(&self.current_chat));

        self.visible_messages = ui_messages_from_history(&self.model_history);

        self.draft_assistant.clear();

        self.pending_approval = None;

        self.busy = false;

        self.status = format!("Switched to chat: {}", self.current_chat);

        self.scroll_to_bottom();
    }

    pub(crate) fn scroll_to_bottom(&mut self) {
        self.transcript_scroll = u16::MAX;

        self.auto_scroll = true;
    }

    pub(crate) fn push_visible_message(&mut self, role: UiMessageRole, content: String) {
        if content.trim().is_empty() {
            return;
        }

        self.visible_messages.push(UiMessage { role, content });

        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    pub(crate) fn add_info_message(&mut self, content: impl Into<String>) {
        self.push_visible_message(UiMessageRole::Info, content.into());
    }

    pub(crate) fn add_user_message(&mut self, content: String) {
        self.push_visible_message(UiMessageRole::User, content);
    }

    pub(crate) fn add_tool_message(&mut self, content: String) {
        self.push_visible_message(UiMessageRole::Tool, content);
    }

    pub(crate) fn add_assistant_message(&mut self, content: String) {
        self.push_visible_message(UiMessageRole::Assistant, content);
    }

    pub(crate) fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Status(status) => self.status = status,

            RuntimeEvent::StartAssistant => {
                self.draft_assistant.clear();

                self.busy = true;
            }

            RuntimeEvent::AssistantChunk(chunk) => {
                self.draft_assistant.push_str(&chunk);

                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }

            RuntimeEvent::CommitAssistant(message) => {
                self.draft_assistant.clear();

                self.add_assistant_message(message);
            }

            RuntimeEvent::DiscardAssistantDraft => self.draft_assistant.clear(),

            RuntimeEvent::ToolResult(result) => self.add_tool_message(result),

            RuntimeEvent::ApprovalRequested(pending) => {
                self.status = format!("Approval required for command: {}", pending.command);

                self.pending_approval = Some(pending);
            }

            RuntimeEvent::TurnFinished(history) => {
                self.model_history = history;

                self.busy = false;

                self.pending_approval = None;

                self.draft_assistant.clear();

                save_history(&self.current_chat, &self.model_history);

                self.status = format!("Ready · chat: {}", self.current_chat);
            }

            RuntimeEvent::Error(error) => {
                self.draft_assistant.clear();

                self.busy = false;

                self.pending_approval = None;

                self.status = error.clone();

                self.add_info_message(error);
            }
        }
    }
}
