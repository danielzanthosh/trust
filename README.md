# TRUST

[![Hackatime](https://hackatime.hackclub.com/api/v1/badge/U0A4759P0Q0/danielzanthosh/trust)](https://hackati.me/danielzanthosh)

TRUST is a terminal AI assistant built in Rust. It combines a TUI chat interface, streaming model responses, persistent named chats, sandboxed file access, permission-gated command execution, and optional browser control through Kimi WebBridge.

## Features

- TUI chat interface powered by `ratatui` and `crossterm`
- Streaming assistant responses
- Named chat history with local commands
- OpenAI-compatible and Anthropic-compatible API support
- Custom API base URL support
- Sandboxed runtime directories for tool actions
- PowerShell command execution with approval prompts
- Destructive command blocking and command logs
- Optional browser/page control through Kimi WebBridge
- Styled terminal rendering for headings, bullets, inline code, bold, and italic text

## Install and run

```bash
git clone https://github.com/danielzanthosh/trust.git
cd trust
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
SANDBOX_COMMAND_TIMEOUT_MS=10000
```

`BASE_URL` can point to an OpenAI-compatible, OpenAI Responses-compatible, or Anthropic-compatible API. TRUST detects the provider shape and builds the request accordingly.

For Codex-style OAuth with OpenAI, run Codex login first and omit `API_KEY` while using an OpenAI `BASE_URL`. TRUST will read `~/.codex/auth.json` and use `tokens.access_token`. You can override the auth file with `CODEX_AUTH_FILE`, or set `AUTH_MODE=codex` to require Codex OAuth fallback.

TRUST also supports a persistent multi-model registry in `config/models.json` (ignored by Git because it can contain API keys). Each model can use a different base URL, model name, API key/auth mode, and priority. The active model is tried first; if it fails, TRUST falls back to the rest in ascending priority order.

## Local commands

| Command | Description |
| --- | --- |
| `/credits` | Show project credits |
| `/model` | List configured models and fallback priority |
| `/model <name>` | Switch the active model |
| `/config` | Show model configuration help |
| `/config codex` | Start Codex OAuth if needed, show the login link/code, then import available Codex models |
| `/config codex [name] [model=gpt-5] [priority=0]` | Manually add/update one Codex OAuth model |
| `/config model <name> base_url=<url> model=<model> [api_key=<key>|auth=codex] [priority=<n>] [active=true]` | Add/update a model config |
| `/exit`, `/quit` | Exit the app |
| `/list` | List saved chats |
| `/clear` | Clear the current chat |
| `/chat <name>` | Switch to a named chat |
| `/delete <name>` | Delete a saved chat |

## Runtime safety

TRUST can ask the runtime to execute tools, but the runtime enforces safety rules.

- File tools are restricted to `sandbox/workspace`, `sandbox/outputs`, and `sandbox/temp`.
- Shell commands run from `sandbox/workspace` with stdin disabled.
- Risky commands require approval.
- Destructive commands are blocked by default.
- Destructive commands are never saved as always-allowed commands.
- Command decisions and results are logged under `permissions/`.

The sandbox workspace starts empty by default. If you want TRUST to seed the sandbox with a copy of the project on startup, set:

```env
SEED_SANDBOX_WORKSPACE=true
```

## Browser control

For opening apps or URLs, TRUST uses PowerShell through `run_command`, for example `Start-Process chrome`.

For reading, clicking, typing, filling forms, or interacting with webpage contents, TRUST uses Kimi WebBridge when available. WebBridge must be installed and connected separately, and TRUST talks to the local daemon at:

```text
http://127.0.0.1:10086
```

TRUST may help navigate or fill harmless sample data, but it should not submit payments, place orders, or enter real credentials.

## Development

Useful checks:

```bash
cargo fmt --check
cargo clippy
cargo test
```

Current note: the crate builds and the test harness runs, but more real Rust unit tests are still needed.

## Tech stack

- Rust
- Tokio
- Reqwest
- Serde / serde_json
- dotenvy
- ratatui
- crossterm

## Status

Active development. Expect rough edges while the TUI, runtime tools, and browser-control workflows evolve.

## License

MIT

## Author

Daniel Santhosh
