# Always-Goofy-Things

To demonstrate the core mechanics of iterative LLM function-calling, this lightweight, experimental CLI application showcases the workflow in the context of AI-assisted software development. It interacts with LLMs to reason about tasks, executes system tools via user confirmation paired with an unsafe reflex mode, and streams the "thinking" process.

> [!CAUTION]
> **Experimental Implementation**: Prototype for demonstration purposes. AI unpredictability and application bugs may cause unexpected behavior.
> **Security Risk**: Local shell execution and network accessibility are enabled. Automating safety checkpoints via unsafe reflex mode, flawed AI commands, or software failure can cause system damage, data loss, or unauthorized data exfiltration.
> **Billing Alert**: AI reasoning loops, oversized contexts, or software control failures can rapidly spike API costs. Monitor usage closely.

## Features
- **Tool-Augmented Iteration**: Automatically calls tools for file I/O, search, bash execution, and web fetching.
- **Safety Guards & Unsafe Reflex**: Balances explicit user approval (y/N) and experimental deterministic "unsafe reflex" mode that automatically resolves safety checkpoints.
- **Streaming "Thinking"**: Displays the model's reasoning process in a subtle reddish tint.
- **Open Standards**: Supports Ollama, OpenAI-compatible, and Anthropic-compatible APIs.

## Requirements
- **Rust**: Latest stable version (Cargo).
- **Backend**: Ollama, any OpenAI-compatible API, or Anthropic-compatible API.
- **Tools**: `bash`, `grep`, and internet connectivity (for web fetching).

## Configuration

Configuration can be set via environment variables or command-line flags (flags take precedence).
| CLI Flag | Env Var | Description | Default |
| :--- | :--- | :--- | :--- |
| `-w, --working-dir <DIR>` | `WORKING_DIR` | Directory where AI tools operate. | `.` |
| `-u, --llm-url <URL>` | `LLM_URL` | Endpoint for the Chat API. | `http://localhost:11434/api/chat` |
| `-m, --llm-model <MODEL>` | `LLM_MODEL` | The LLM model name to use. | `gemma4:12b` |
| `-k, --llm-api-key <KEY>` | `LLM_API_KEY` | API key for authentication. | (none) |
| `-P, --llm-provider <PROVIDER>` | `LLM_PROVIDER` | LLM API provider (auto-detected from URL if not specified). | (auto) |
| `-R, --tool-result-format <FORMAT>` | `TOOL_RESULT_FORMAT` | How tool results are structured when sent to the LLM. | `json_string` |
| `-v, --verbose-level <LEVEL>` | `VERBOSE_LEVEL` | LLM conversation display verbosity (`0`-`4`). | `1` |
| `-p, --pretty-level <LEVEL>` | `PRETTY_LEVEL` | UI decoration level (`0`-`1`). | `1` |
| `-r, --llm-rpm <NUM>` | `LLM_RPM` | Maximum requests per minute for the LLM API. | `0` (unlimited) |
| `-s, --session-label <LABEL>` | `SESSION_LABEL` | Label for session persistence files (enables running multiple sessions). | `default` |
| `--unsafe-reflex` | `UNSAFE_REFLEX_MODE` | Bypasses manual confirmation for certain safety checkpoints. | false |

> **Note:** Command-line options always take precedence over environment variables.

### LLM Provider (`LLM_PROVIDER`)

Controls which provider-specific API format is used. If not set, the provider is auto-detected from the URL.

- `openai` - OpenAI API format. Also covers compatible backends (e.g., Google Gemini via OpenAI wrapper, or any OpenAI-proxy). Expects a standard `/v1/chat/completions` endpoint.
- `ollama` - Ollama API format. Uses the `/api/chat` endpoint with Ollama-specific request structure.
- `anthropic` - Anthropic-compatible API format. Uses `x-api-key` header and Anthropic-specific message format (content blocks, `tool_use`/`tool_result`).

### Tool Result Format (`TOOL_RESULT_FORMAT`)

Controls how tool execution results are structured when sent back to the LLM. This applies to all tools and all providers.

- `json_string` (default) - The full result JSON is serialized to a string. The LLM receives an escaped JSON string like `"{\"path\":\"...\",\"content\":\"...\"}"`.
- `text` - Each tool result is rendered as a concise plain-text string, discarding structured metadata. Examples:
  - `read_file` / `fetch_web` -> the content itself
  - `execute_bash` -> stdout (and stderr if exit_code != 0)
  - `write_file` -> `Written 1423 bytes to src/new_module.rs`
  - `str_replace_editor` -> `Replaced 1 occurrence in src/main.rs (Perfect match.)`
  - `grep_search` -> grep-style `path:line:text` lines
  - `list_directory` -> `name\ttype\tsize bytes` lines
- `json_structured` - The full result JSON is embedded as a proper JSON object, not an escaped string. Note: some providers may require tool message `content` to be a string.

### Verbosity Levels (`VERBOSE_LEVEL`)

Controls how much of the LLM conversation is displayed on the terminal.
- `0`: Silent - no conversation content is shown
- `1`: Metadata - only summary information (content length) is displayed
- `2`: Incremental - only newly appended messages are shown
- `3`: Full - the entire conversation is printed in detail, including raw tool call delta SSE lines
- `4`: Raw - same as Level 3, plus every raw SSE line from the response stream

### Pretty Levels (`PRETTY_LEVEL`)

Controls the visual styling and decorations applied to the terminal output.
- `0`: Plain - no colors or visual decorations
- `1`: Standard - colored text with structured sections and separators

## Usage

### Default Execution (Local Ollama + gemma4:12b)
```bash
cargo run
```

### Full Options Example (CLI flags, excluding llm-api-key)
```bash
cargo run -- \
    --working-dir ./work \
    --llm-url "http://localhost:11434/v1/chat/completions" \
    --llm-model "gemma4:12b" \
    --verbose-level 2 \
    --pretty-level 1
```

### Mixed Usage (Environment variables + short CLI flags, caution with cloud AI billing)
```bash
export LLM_URL="https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
 export LLM_API_KEY="......."
cargo run -- -v 0 --llm-model gemma-4-31b-it
```

## Example User Queries

### Basic Queries

- "Who are you and what tools can you use?"
- "Create a Python script named `test.py` that prints 'Hello, World!'."
- "Find the Python script in the workspace and translate its output messages into Japanese."
- "Summarize the 'Anyhow' crate documentation from this URL: https://docs.rs/anyhow/latest/anyhow/."
- "Analyze the hyper documentation (https://docs.rs/hyper/latest/hyper/) and create a minimal HTTP server project in Rust."
- "Fix this broken http server."

### Attaching Files (`@`)

Prepend `@file` paths at the beginning of your query to attach file contents to the LLM context. Paths are relative to the working directory. Multiple files are separated by commas. Files larger than 1 MiB prompt for confirmation before being attached.

Non-text files are automatically converted:
- **PDF** (`.pdf`) - text extracted page by page via Pdfium
- **Image** (`.png`, `.jpg`/`.jpeg`, `.gif`, `.webp`) - Base64 data URL
- **Audio** (`.wav`, `.mp3`) - raw Base64

- "@src/main.rs, @Cargo.toml Explain the structure of these files."
- "@README.md Summarize this file."
- "@guide.pdf Verify the integrity and consistency of this document."
