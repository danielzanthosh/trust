# TRUST

[![Hackatime](https://hackatime.hackclub.com/api/v1/badge/U0A4759P0Q0/danielzanthosh/trust)](https://hackati.me/danielzanthosh)

A fast, lightweight AI assistant that runs directly inside the terminal.

Built with Rust 🦀 for speed, safety, and performance.
## Features

* OpenAI-compatible API support
* Anthropic-compatible API support
* Custom API base URL
* Chat history memory
* Terminal-based interface
* Fast async networking
* Colored terminal output
* Environment variable configuration
* Memory-safe architecture powered by Rust

## Planned Features

* Infinite memory system
* Streaming responses
* Markdown rendering
* Syntax highlighting
* TUI interface
* Multiple provider support
* Local AI support with Ollama
* Slash commands
* Persistent conversation storage

## Tech Stack

* Rust
* Tokio
* Reqwest
* Serde
* dotenvy
* colored

## Installation

Clone the repository:

```bash
git clone https://github.com/danielzanthosh/trust.git
cd trust
```

Install dependencies and run:

```bash
cargo run
```

## Configuration

Create a `.env` file in the project root:

```env
API_KEY=your_api_key_here
BASE_URL=https://ai.hackclub.com/proxy
MODEL=anthropic/claude-haiku-4.5
MAX_TOKENS=1024
ALLOW_DESTRUCTIVE_ACTIONS=false
```

`MAX_TOKENS` caps each model response. Set this explicitly if your API provider enforces spending limits or token budgets.

`ALLOW_DESTRUCTIVE_ACTIONS` controls whether destructive commands requested through `run_command` are always blocked or may be presented for explicit approval.
When set to `false`, destructive commands are blocked.
When set to `true`, destructive commands still never auto-run: the runtime will prompt the user and only execute them when the user chooses `Allow once`.

## Command Permissions

TRUST supports runtime-gated command execution through the `run_command` tool.

- Commands run inside `sandbox/workspace`
- `stdin` is disabled
- `stdout` and `stderr` are captured
- the timeout comes from `SANDBOX_COMMAND_TIMEOUT_MS`
- destructive commands are never persisted to `permissions/allowed_commands.json`
- non-destructive commands may be saved with `Allow always`

To enable destructive command approvals, add this to `.env`:

```env
ALLOW_DESTRUCTIVE_ACTIONS=true
```

Accepted truthy values are `true`, `1`, `yes`, and `on`.

## Commands

| Command    | Description          |
| ---------- | -------------------- |
| `/credits` | Show project credits |
| `/exit`    | Exit the application |

## Project Goals

TRUST aims to be:

* Fast
* Minimal
* Extensible
* Cross-platform
* Developer-friendly
* Memory-safe

The long-term goal is to create a fully featured AI assistant experience entirely inside the terminal.

## Why Rust?

Rust provides:

* Memory safety
* Excellent performance
* Strong type safety
* Great async support
* Cross-platform compatibility

Perfect for building reliable terminal applications and AI tooling.

## Current Status

🚧 Active Development

This project is currently in early development.

## License

MIT License

## Author

Daniel Santhosh
