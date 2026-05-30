use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        size as terminal_size,
    },
};

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};

use std::io::{self, Stdout};

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::runtime::tools::*;

use crate::app::*;

use crate::history::*;

use crate::model::*;

use crate::runtime::sandbox::*;

use crate::types::*;

pub(crate) fn spinner() -> &'static str {
    let frame = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_millis() / 140) % 8)
        .unwrap_or(0);

    match frame {
        0 => "⠋",

        1 => "⠙",

        2 => "⠹",

        3 => "⠸",

        4 => "⠼",

        5 => "⠴",

        6 => "⠦",

        _ => "⠧",
    }
}

pub(crate) fn styled_inline_spans(content: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();

    let mut rest = content;

    while !rest.is_empty() {
        if let Some(after_marker) = rest.strip_prefix("**")
            && let Some(end) = after_marker.find("**")
        {
            spans.push(Span::styled(
                after_marker[..end].to_string(),
                base_style.add_modifier(Modifier::BOLD),
            ));

            rest = &after_marker[end + 2..];

            continue;
        }

        if let Some(after_marker) = rest.strip_prefix('*')
            && let Some(end) = after_marker.find('*')
        {
            spans.push(Span::styled(
                after_marker[..end].to_string(),
                base_style.add_modifier(Modifier::ITALIC),
            ));

            rest = &after_marker[end + 1..];

            continue;
        }

        if let Some(after_marker) = rest.strip_prefix('`')
            && let Some(end) = after_marker.find('`')
        {
            spans.push(Span::styled(
                after_marker[..end].to_string(),
                Style::default().fg(Color::LightMagenta),
            ));

            rest = &after_marker[end + 1..];

            continue;
        }

        let next_marker = rest
            .char_indices()
            .skip(1)
            .find_map(|(index, ch)| matches!(ch, '*' | '`').then_some(index))
            .unwrap_or(rest.len());

        spans.push(Span::styled(rest[..next_marker].to_string(), base_style));

        rest = &rest[next_marker..];
    }

    spans
}

pub(crate) fn styled_message_lines(label: &str, color: Color, content: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let label_style = Style::default().fg(color).add_modifier(Modifier::BOLD);

    for (index, raw_line) in content.lines().enumerate() {
        let trimmed = raw_line.trim_start();

        let mut line_spans = Vec::new();

        if index == 0 {
            line_spans.push(Span::styled(format!("{}: ", label), label_style));
        } else {
            line_spans.push(Span::raw("  "));
        }

        if let Some(heading) = trimmed.strip_prefix("### ") {
            line_spans.push(Span::styled(
                heading.to_string(),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if let Some(heading) = trimmed.strip_prefix("## ") {
            line_spans.push(Span::styled(
                heading.to_string(),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if let Some(heading) = trimmed.strip_prefix("# ") {
            line_spans.push(Span::styled(
                heading.to_string(),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            ));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            line_spans.push(Span::styled("• ", Style::default().fg(Color::LightCyan)));

            line_spans.extend(styled_inline_spans(item, Style::default()));
        } else {
            line_spans.extend(styled_inline_spans(raw_line, Style::default()));
        }

        lines.push(Line::from(line_spans));
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!("{}: ", label),
            label_style,
        )]));
    }

    lines
}

pub(crate) fn build_transcript(app: &App) -> Text<'static> {
    let mut lines = Vec::new();

    for message in &app.visible_messages {
        let (label, color) = match message.role {
            UiMessageRole::User => ("You", Color::Cyan),

            UiMessageRole::Assistant => ("TRUST", Color::Green),

            UiMessageRole::Tool => ("Tool", Color::Magenta),

            UiMessageRole::Info => ("Info", Color::Yellow),
        };

        lines.extend(styled_message_lines(label, color, &message.content));

        lines.push(Line::raw(""));
    }

    if !app.draft_assistant.is_empty() {
        lines.extend(styled_message_lines(
            "Thinking",
            Color::DarkGray,
            &app.draft_assistant,
        ));
    } else if app.busy {
        lines.push(Line::from(vec![
            Span::styled(
                "Thinking: ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} thinking…", spinner()),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    Text::from(lines)
}

pub(crate) fn wrapped_line_count(content: &str, viewport_width: u16) -> usize {
    let width = usize::from(viewport_width.max(1));

    content
        .lines()
        .map(|line| {
            let chars = line.chars().count();

            chars.saturating_sub(1) / width + 1
        })
        .sum::<usize>()
        .max(1)
}

pub(crate) fn transcript_line_count(app: &App, viewport_width: u16) -> usize {
    let message_lines = app
        .visible_messages
        .iter()
        .map(|message| {
            let label_width = match message.role {
                UiMessageRole::User => 5,

                UiMessageRole::Assistant => 7,

                UiMessageRole::Tool => 6,

                UiMessageRole::Info => 6,
            };

            let effective_width = viewport_width.saturating_sub(label_width).max(1);

            wrapped_line_count(&message.content, effective_width) + 1
        })
        .sum::<usize>();

    let draft_lines = if app.draft_assistant.trim().is_empty() {
        usize::from(app.busy)
    } else {
        wrapped_line_count(
            &app.draft_assistant,
            viewport_width.saturating_sub(7).max(1),
        )
    };

    message_lines + draft_lines
}

pub(crate) fn resolved_transcript_scroll(
    app: &App,
    viewport_height: u16,
    viewport_width: u16,
) -> u16 {
    let content_lines =
        transcript_line_count(app, viewport_width).min(usize::from(u16::MAX)) as u16;

    let max_scroll = content_lines.saturating_sub(viewport_height);

    if app.auto_scroll || app.transcript_scroll == u16::MAX {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    }
}

pub(crate) fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let root = frame.area();

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(24), Constraint::Min(40)])
        .split(root);

    let sidebar_area = horizontal[0];

    let main_area = horizontal[1];

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(4),
        ])
        .split(main_area);

    let chats = list_chat_names();

    let items = chats
        .iter()
        .map(|chat| {
            let style = if chat == &app.current_chat {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };

            ListItem::new(chat.clone()).style(style)
        })
        .collect::<Vec<_>>();

    let chats_list = List::new(items).block(
        Block::default()
            .title("Chats")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Blue)),
    );

    frame.render_widget(chats_list, sidebar_area);

    let transcript_area = vertical[0];

    let transcript_inner_height = transcript_area.height.saturating_sub(2);

    let transcript_inner_width = transcript_area.width.saturating_sub(2);

    let transcript_scroll =
        resolved_transcript_scroll(app, transcript_inner_height, transcript_inner_width);

    let transcript_title = if app.busy {
        format!(
            "Conversation · {} TRUST working · wheel/↑/↓ scroll · End newest · {} messages",
            spinner(),
            app.visible_messages.len()
        )
    } else {
        format!(
            "Conversation · wheel/↑/↓ scroll · PgUp/PgDn jump · End newest · {} messages",
            app.visible_messages.len()
        )
    };

    let transcript = Paragraph::new(build_transcript(app))
        .block(
            Block::default()
                .title(transcript_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .scroll((transcript_scroll, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(transcript, transcript_area);

    let mut scrollbar_state =
        ScrollbarState::new(transcript_line_count(app, transcript_inner_width))
            .position(transcript_scroll as usize)
            .viewport_content_length(transcript_inner_height as usize);

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .begin_symbol(Some("▲"))
        .end_symbol(Some("▼"));

    frame.render_stateful_widget(scrollbar, transcript_area, &mut scrollbar_state);

    let status_style = if app.busy {
        Style::default().fg(Color::LightYellow)
    } else {
        Style::default().fg(Color::Gray)
    };

    let status = Paragraph::new(app.status.clone())
        .style(status_style)
        .block(
            Block::default()
                .title(format!(
                    "Status · chat={} · busy={} · sandbox={}",
                    app.current_chat,
                    if app.busy { "yes" } else { "no" },
                    display_path(&app.sandbox.workspace)
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );

    frame.render_widget(status, vertical[1]);

    let input_title = if app.busy {
        "Input · waiting for TRUST"
    } else {
        "Input · Enter send · Tab chat · Ctrl+C quit"
    };

    let input = Paragraph::new(app.input.clone())
        .style(Style::default().fg(Color::LightCyan))
        .block(
            Block::default()
                .title(input_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(input, vertical[2]);

    let cursor_x = vertical[2].x + 1 + app.input.chars().count() as u16;

    let cursor_y = vertical[2].y + 1;

    frame.set_cursor_position((cursor_x, cursor_y));

    if let Some(pending) = &app.pending_approval {
        let popup = centered_rect(70, if pending.allow_always { 40 } else { 35 }, root);

        frame.render_widget(Clear, popup);

        let mut lines = vec![
            Line::from(Span::styled(
                pending.title.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Risk: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(pending.risk_label.clone()),
            ]),
            Line::raw(""),
            Line::from(Span::styled(
                "Command:",
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ];

        for command_line in pending.command.lines().take(8) {
            lines.push(Line::raw(command_line.to_string()));
        }

        if pending.command.lines().count() > 8 {
            lines.push(Line::raw("..."));
        }

        lines.push(Line::raw(""));

        lines.push(Line::raw("[A] Allow once"));

        if pending.allow_always {
            lines.push(Line::raw("[L] Allow always"));
        }

        lines.push(Line::raw("[D] Decline"));

        let help = if pending.allow_always {
            "Choose A, L, or D"
        } else {
            "Choose A or D"
        };

        lines.push(Line::raw(""));

        lines.push(Line::from(Span::styled(
            help,
            Style::default().fg(Color::Yellow),
        )));

        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .title("Permission Required")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, popup);
    }
}

pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

pub(crate) fn credits_text() -> String {
    [
        "TRUST — Terminal Runtime for Unified Smart Tasks",
        "Developer: Daniel Santhosh",
        "Repository: https://github.com/danielzanthosh/trust",
    ]
    .join("\n")
}

pub(crate) fn handle_local_command(app: &mut App, input: &str) -> bool {
    if input == "/exit" || input == "/quit" {
        app.should_quit = true;

        return true;
    }

    if input == "/list" {
        let chats = list_chat_names();

        app.add_info_message(format!("Saved chats:\n{}", chats.join("\n")));

        app.status = "Listed chats".to_string();

        return true;
    }

    if input == "/credits" {
        app.add_info_message(credits_text());

        app.status = "Displayed credits".to_string();

        return true;
    }

    if input == "/model" {
        app.add_info_message(format!(
            "Configured models:\n{}\n\nUse /model <name> to switch.",
            describe_models()
        ));

        app.status = "Listed models".to_string();

        return true;
    }

    if let Some(model_name) = input.strip_prefix("/model ") {
        let model_name = model_name.trim();

        if model_name.is_empty() {
            app.add_info_message("Usage: /model <name>");
            return true;
        }

        match set_active_model(model_name) {
            Ok(()) => {
                app.status = format!("Switched model: {}", model_name);
                app.add_info_message(format!("Active model: {}", model_name));
            }
            Err(error) => {
                app.status = error.clone();
                app.add_info_message(format!(
                    "{}\n\nConfigured models:\n{}",
                    error,
                    describe_models()
                ));
            }
        }

        return true;
    }

    if input == "/config" || input == "/config model" {
        app.add_info_message([
            "Config commands:",
            "/config codex [name] [model=gpt-5] [base_url=https://api.openai.com/v1/responses] [priority=0] [active=true]",
            "/config model <name> base_url=<url> model=<model> [api_key=<key>|auth=codex] [priority=<n>] [active=true]",
            "/model lists configured models; /model <name> switches the active model.",
            "Lower priority numbers are tried earlier. The active model is tried first, then fallback uses priority order.",
        ].join("\n"));

        app.status = "Displayed config help".to_string();

        return true;
    }

    if input == "/config codex" || input.starts_with("/config codex ") {
        let args = input
            .strip_prefix("/config codex")
            .unwrap_or_default()
            .trim();

        match parse_codex_config_command(args) {
            Ok((model, make_active)) => {
                let name = model.name.clone();
                match upsert_model_config(model, make_active) {
                    Ok(()) => {
                        app.status = format!("Configured Codex model: {}", name);
                        app.add_info_message(format!(
                            "Configured Codex OAuth model: {}\n{}",
                            name,
                            describe_models()
                        ));
                    }
                    Err(error) => {
                        app.status = error.clone();
                        app.add_info_message(error);
                    }
                }
            }
            Err(error) => {
                app.status = error.clone();
                app.add_info_message(error);
            }
        }

        return true;
    }

    if let Some(args) = input.strip_prefix("/config model ") {
        match parse_model_config_command(args.trim()) {
            Ok((model, make_active)) => {
                let name = model.name.clone();
                match upsert_model_config(model, make_active) {
                    Ok(()) => {
                        app.status = format!("Configured model: {}", name);
                        app.add_info_message(format!(
                            "Configured model: {}\n{}",
                            name,
                            describe_models()
                        ));
                    }
                    Err(error) => {
                        app.status = error.clone();
                        app.add_info_message(error);
                    }
                }
            }
            Err(error) => {
                app.status = error.clone();
                app.add_info_message(error);
            }
        }

        return true;
    }

    if input == "/clear" {
        app.visible_messages.clear();

        app.model_history = ensure_system_message(Vec::new());

        save_history(&app.current_chat, &app.model_history);

        app.status = format!("Cleared chat: {}", app.current_chat);

        return true;
    }

    if let Some(chat_name) = input.strip_prefix("/chat ") {
        let chat_name = chat_name.trim();

        if chat_name.is_empty() {
            app.add_info_message("Usage: /chat <name>");

            return true;
        }

        app.set_chat(chat_name.to_string());

        return true;
    }

    if let Some(chat_name) = input.strip_prefix("/delete ") {
        let chat_name = chat_name.trim();

        if chat_name.is_empty() {
            app.add_info_message("Usage: /delete <name>");

            return true;
        }

        match delete_chat(chat_name) {
            Ok(()) => {
                app.status = format!("Deleted chat: {}", chat_name);

                if chat_name == app.current_chat {
                    app.set_chat("default".to_string());
                }
            }

            Err(error) => {
                app.status = error.clone();

                app.add_info_message(error);
            }
        }

        return true;
    }

    false
}

pub(crate) fn submit_current_input(app: &mut App, event_tx: &mpsc::UnboundedSender<RuntimeEvent>) {
    let input = app.input.trim().to_string();

    if input.is_empty() || app.busy {
        return;
    }

    if handle_local_command(app, &input) {
        app.input.clear();

        return;
    }

    app.scroll_to_bottom();

    app.add_user_message(input.clone());

    app.model_history.push(Message {
        role: "user".to_string(),

        content: input.clone(),
    });

    save_history(&app.current_chat, &app.model_history);

    app.input.clear();

    app.busy = true;

    app.draft_assistant.clear();

    let current_chat = app.current_chat.clone();

    let history = app.model_history.clone();

    let runtime_tx = event_tx.clone();

    let sandbox = app.sandbox.clone();

    app.status = "Planning tool actions...".to_string();

    tokio::spawn(async move {
        process_turn(current_chat, history, sandbox, runtime_tx).await;
    });
}

pub(crate) fn transcript_viewport_size() -> Option<(u16, u16)> {
    let (terminal_width, terminal_height) = terminal_size().ok()?;

    let sidebar_width = 24;

    let transcript_border = 2;

    let bottom_panels_height = 7;

    Some((
        terminal_width
            .saturating_sub(sidebar_width)
            .saturating_sub(transcript_border),
        terminal_height
            .saturating_sub(bottom_panels_height)
            .saturating_sub(transcript_border),
    ))
}

pub(crate) fn transcript_max_scroll(app: &App) -> Option<u16> {
    let (width, height) = transcript_viewport_size()?;

    let content_lines = transcript_line_count(app, width).min(usize::from(u16::MAX)) as u16;

    Some(content_lines.saturating_sub(height))
}

pub(crate) fn scroll_up_visible(app: &mut App, amount: u16) {
    let current_scroll = if app.auto_scroll || app.transcript_scroll == u16::MAX {
        transcript_max_scroll(app).unwrap_or(0)
    } else {
        app.transcript_scroll
    };

    app.transcript_scroll = current_scroll.saturating_sub(amount);

    app.auto_scroll = false;
}

pub(crate) fn scroll_down_visible(app: &mut App, amount: u16) {
    let max_scroll = transcript_max_scroll(app).unwrap_or(0);

    let current_scroll = if app.auto_scroll || app.transcript_scroll == u16::MAX {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    };

    let next_scroll = current_scroll.saturating_add(amount).min(max_scroll);

    if next_scroll >= max_scroll {
        app.scroll_to_bottom();
    } else {
        app.transcript_scroll = next_scroll;

        app.auto_scroll = false;
    }
}

pub(crate) fn handle_mouse_event(app: &mut App, mouse: MouseEvent) {
    let Ok((terminal_width, terminal_height)) = terminal_size() else {
        return;
    };

    let sidebar_width = 24;

    let bottom_panels_height = 7;

    let in_transcript = mouse.column >= sidebar_width
        && mouse.column < terminal_width
        && mouse.row < terminal_height.saturating_sub(bottom_panels_height);

    if !in_transcript {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => scroll_up_visible(app, 2),

        MouseEventKind::ScrollDown => scroll_down_visible(app, 2),

        _ => {}
    }
}

pub(crate) fn handle_key_event(
    app: &mut App,
    key: KeyEvent,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    if let Some(pending) = app.pending_approval.take() {
        let choice = match key.code {
            KeyCode::Char('a') | KeyCode::Char('A') => Some(PermissionChoice::AllowOnce),

            KeyCode::Char('l') | KeyCode::Char('L') if pending.allow_always => {
                Some(PermissionChoice::AllowAlways)
            }

            KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Esc => {
                Some(PermissionChoice::Decline)
            }

            _ => None,
        };

        if let Some(choice) = choice {
            let command = pending.command.clone();

            let _ = pending.responder.send(choice);

            app.status = format!("Resolved approval for: {}", command);
        } else {
            app.pending_approval = Some(pending);
        }

        return;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }

        KeyCode::Enter => submit_current_input(app, event_tx),

        KeyCode::Up => scroll_up_visible(app, 1),

        KeyCode::Down => scroll_down_visible(app, 1),

        KeyCode::PageUp => scroll_up_visible(app, 8),

        KeyCode::PageDown => scroll_down_visible(app, 8),

        KeyCode::Home => {
            app.transcript_scroll = 0;

            app.auto_scroll = false;
        }

        KeyCode::End => app.scroll_to_bottom(),

        KeyCode::Backspace => {
            app.input.pop();
        }

        KeyCode::Char(ch) => {
            app.input.push(ch);
        }

        KeyCode::Tab => {
            let chats = list_chat_names();

            if let Some(index) = chats.iter().position(|chat| chat == &app.current_chat) {
                let next = chats[(index + 1) % chats.len()].clone();

                app.set_chat(next);
            }
        }

        _ => {}
    }
}

pub(crate) struct Tui {
    pub(crate) terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    pub(crate) fn new() -> Result<Self, Box<dyn std::error::Error>> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();

        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

        let backend = CrosstermBackend::new(stdout);

        let terminal = Terminal::new(backend)?;

        Ok(Self { terminal })
    }

    pub(crate) fn draw(&mut self, app: &App) -> Result<(), Box<dyn std::error::Error>> {
        self.terminal.draw(|frame| draw_ui(frame, app))?;

        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();

        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        );

        let _ = self.terminal.show_cursor();
    }
}
