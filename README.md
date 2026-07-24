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
| `-P, --llm-provider <PROVIDER>` | `LLM_PROVIDER` | LLM API provider (auto-detected from URL if not specified). | (auto) |
| `-m, --llm-model <MODEL>` | `LLM_MODEL` | The LLM model name to use. | `gemma4:12b` |
| `-k, --llm-api-key <KEY>` | `LLM_API_KEY` | API key for authentication. | (none) |
| `-r, --llm-rpm <NUM>` | `LLM_RPM` | Maximum requests per minute for the LLM API. | `0` (unlimited) |
| `-T, --max-output-tokens <NUM>` | `MAX_OUTPUT_TOKENS` | Maximum output tokens per LLM request. | `16384` |
| `-E, --max-empty-retry <NUM>` | `MAX_EMPTY_RETRY` | Max retries when the LLM returns an empty response. | `1` |
| `--max-reasoning-turns <NUM>` | `MAX_REASONING_TURNS` | Safety cap on LLM calls per user message. In batch mode exceeding it causes an error exit; in interactive mode it returns control to the prompt. | `30` |
| `-R, --tool-result-format <FORMAT>` | `TOOL_RESULT_FORMAT` | How tool results are structured when sent to the LLM. | `json_string` |
| `-v, --verbose-level <LEVEL>` | `VERBOSE_LEVEL` | LLM conversation display verbosity (`0`-`4`). | `1` |
| `-p, --pretty-level <LEVEL>` | `PRETTY_LEVEL` | UI decoration level (`0`-`1`). | `1` |
| `-s, --session-label <LABEL>` | `SESSION_LABEL` | Label for session persistence files (enables running multiple sessions). | `default` |
| `-q, --query <QUERY>` | (none) | Run in batch mode: execute once and exit, printing the final answer to stdout. | (interactive) |
| `-o, --output <FILE>` | `OUTPUT_FILE` | Write the final answer to a file instead of stdout. Requires `-q`. | (none) |
| `--unsafe-reflex` | `UNSAFE_REFLEX_MODE` | Bypasses manual confirmation for certain safety checkpoints. | false |

> **Note:** Command-line options always take precedence over environment variables.

### LLM Provider (`LLM_PROVIDER`)

Controls which provider-specific API format is used. If not set, the provider is auto-detected from the URL.
- `openai` - OpenAI-compatible API format. Endpoint: `/v1/chat/completions`
- `ollama` - Ollama API format. Endpoint: `/api/chat`
- `anthropic` - Anthropic-compatible API format. Endpoint: `/v1/messages` (gratuitously dissimilar).

### Tool Result Format (`TOOL_RESULT_FORMAT`)

Controls how tool results are structured when sent back to the LLM.
- `json_string` (default): escaped JSON string.
- `text`: plain-text.
- `json_structured`: JSON object.

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

### Quick Start

Default execution with Local Ollama + gemma4:12b
```bash
cargo run
```

Alternatively, with a cloud provider
```bash
export LLM_URL="https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
 export LLM_API_KEY="..."
cargo run -- -m gemini-2.5-pro
```

Then type a query like "Who are you and what tools can you use?".

### Batch Mode (`-q`)

Run a single query non-interactively and exit. The final answer is written to stdout (or `-o <file>`). Progress and errors go to stderr.

```bash
cargo run -- -q "@src/main.rs Explain the architecture" -o result.txt
```

> [!WARNING]
> **No file-size guard**: large files attach without confirmation.
> **Manual-confirm tools are denied**: tools that normally prompt `y/N` are skipped.

### Attaching Files (`@`) and Text Extraction (`@@`)

Prepend `@file` paths to attach files, or `@@file` to force text extraction (useful for PDFs on providers without native document support). Paths are relative to the working directory. Multiple files: `@a.txt, @b.txt`.

| Prefix | Behaviour |
|--------|-----------|
| `@` | Send files (text, images, audio, PDF). Non-text is base64-encoded. |
| `@@` | Converts PDF to Markdown; saved as `{file}_converted_for_llm.txt`. |

Ollama requires `@@` for PDF (no native document support).

### Examples

**Basic queries:**
- "Who are you and what tools can you use?"
- "Create a Python script named `test.py` that prints 'Hello, World!'."

**File operations:**
- "@src/main.rs, @Cargo.toml Explain the structure of these files."
- "@diagram.png Describe this image."
- "@@spec.pdf Verify the integrity of this document."

**Complex tasks:**
- "Find the Python script in the workspace and translate its output messages into Shakespearean English."
- "Summarize the 'Anyhow' crate documentation from this URL: https://docs.rs/anyhow/latest/anyhow/."
- "Fix this broken http server."
