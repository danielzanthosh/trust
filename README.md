# TRUST

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
```

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
