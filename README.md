# Always Goofy Things

A lightweight, experimental agentic CLI application for AI-assisted software development. It interacts with LLMs to reason about tasks, execute system tools safely with user confirmation, and provide streaming responses with visible "thinking" processes.

> [!CAUTION]
> **Experimental Implementation**: This is a demo-level prototype. 
> **Security Risk**: Local shell execution (bash) can lead to catastrophic system damage or security breaches if the AI generates malicious commands. 
> **Billing Alert**: Automated agent loops can rapidly consume tokens, leading to unexpected API costs. Use with extreme caution.

## Features
- **Tool-Augmented Iteration**: Automatically calls tools for file I/O, searching, bash execution, and web fetching.
- **Safety First**: All tool executions require explicit user approval (`y/N`).
- **Streaming Thinking**: Displays the model's reasoning process in a subtle reddish tint.
- **Open Standards**: Supports Ollama and OpenAI-compatible APIs.

## Requirements
- **Rust**: Latest stable version (Cargo).
- **Backend**: Ollama or any OpenAI-compatible API.
- **Tools**: `bash`, `grep`, and `network access` for web fetching.

## Environment Variables
| Variable | Description | Default |
| :--- | :--- | :--- |
| `LLM_URL` | Endpoint for the Chat API. | `http://localhost:11434/api/chat` |
| `LLM_MODEL` | The LLM model name to use. | `gemma4:12b` |
| `LLM_API_KEY` | (Optional) API key for authentication. | (None) |
| `TRUNCATE_MODE` | UI mode for API request truncation (0-3). | `2` |

### Truncation Modes (`TRUNCATE_MODE`)
- `0`: Full request display (Verbose).
- `1`: Incremental display (New messages only).
- `2`: Metadata only (Content-Length).
- `3`: Silent (No request logs).

## Usage
### Default Execution (Local Ollama + gemma4:12b)
```bash
cargo run
```

### Full Options Example (Excluding API Key)
```bash
LLM_URL="http://localhost:11434/api/chat" \
LLM_MODEL="gemma4:12b" \
TRUNCATE_MODE=1 \
cargo run
```

## Example Prompts

- "Who are you and what tools can you use?"
- "Create a 'Hello World' Python script named `test.py`."
- "Locate the Python script in the workspace and translate its messages to Japanese."
- "Summarize the 'Anyhow' crate documentation from this URL: https://docs.rs/anyhow/latest/anyhow/."