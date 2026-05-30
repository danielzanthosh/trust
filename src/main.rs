// TRUST

// Terminal Runtime for Unified Smart Tasks

// Sandboxed terminal AI runtime with a ratatui/crossterm TUI, streaming chat, tool execution, and permission-gated command execution.

mod app;

mod history;

mod model;

mod permissions;

mod prompt;

mod runtime;

mod tui;

mod types;

use crossterm::event::{self, Event as CEvent};

use dotenvy::dotenv;

use std::io;

use std::time::Duration;

use tokio::sync::mpsc;

use app::App;

use runtime::events::RuntimeEvent;
use runtime::sandbox::ensure_sandbox_ready;
use types::SandboxConfig;

use tui::{Tui, handle_key_event, handle_mouse_event};

#[tokio::main]

async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let sandbox = SandboxConfig::load()
        .and_then(|config| {
            ensure_sandbox_ready(&config)?;

            Ok(config)
        })
        .map_err(io::Error::other)?;

    let mut tui = Tui::new()?;

    let mut app = App::new(sandbox);

    app.status = "Ready · /chat <name> · /list · /clear · /delete <name> · /credits · Ctrl+C quit"
        .to_string();

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<RuntimeEvent>();

    while !app.should_quit {
        while let Ok(event) = event_rx.try_recv() {
            app.handle_runtime_event(event);
        }

        tui.draw(&app)?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                CEvent::Key(key) => handle_key_event(&mut app, key, &event_tx),

                CEvent::Mouse(mouse) => handle_mouse_event(&mut app, mouse),

                _ => {}
            }
        }
    }

    Ok(())
}
