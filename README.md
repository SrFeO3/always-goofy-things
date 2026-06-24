# Always-Goofy-Things

To demonstrate the core mechanics of iterative LLM function-calling, this lightweight, experimental CLI application showcases the workflow in the context of AI-assisted software development. It interacts with LLMs to reason about tasks, execute system tools safely with user confirmation, and provide streaming responses with visible "thinking" processes.

> [!CAUTION]
> **Experimental Implementation**: Prototype for demonstration purposes. AI unpredictability and application bugs may cause unexpected behavior.
> **Security Risk**: Local shell execution and network accessibility are enabled. Flawed AI commands or software failure can cause system damage, data loss, or unauthorized data exfiltration.
> **Billing Alert**: AI reasoning loops, oversized contexts, or software control failures can rapidly spike API costs. Monitor usage closely.

## Features
- **Tool-Augmented Iteration**: Automatically calls tools for file I/O, search, bash execution, and web fetching.
- **Safety Guards**: Includes experimental allowlist verification and explicit user approval (y/N).
- **Streaming "Thinking"**: Displays the model's reasoning process in a subtle reddish tint.
- **Open Standards**: Supports Ollama and OpenAI-compatible APIs.

## Requirements
- **Rust**: Latest stable version (Cargo).
- **Backend**: Ollama or any OpenAI-compatible API.
- **Tools**: `bash`, `grep`, and internet connectivity (for web fetching).

## Configuration

Configuration can be set via environment variables or command-line flags (flags take precedence).
| CLI Flag | Env Var | Description | Default |
| :--- | :--- | :--- | :--- |
| `-w, --working-dir <DIR>` | `WORKING_DIR` | Directory where AI tools operate. | `.` |
| `-u, --llm-url <URL>` | `LLM_URL` | Endpoint for the Chat API. | `http://localhost:11434/api/chat` |
| `-m, --llm-model <MODEL>` | `LLM_MODEL` | The LLM model name to use. | `gemma4:12b` |
| `-k, --llm-api-key <KEY>` | `LLM_API_KEY` | API key for authentication. | (none) |
| `-v, --verbose-level <LEVEL>` | `VERBOSE_LEVEL` | LLM conversation display verbosity (`0`-`3`). | `1` |
| `-p, --pretty-level <LEVEL>` | `PRETTY_LEVEL` | UI decoration level (`0`-`1`). | `1` |


### Verbosity Levels (`VERBOSE_LEVEL`)
Controls how much of the LLM conversation is displayed on the terminal.
- `0`: Silent - no conversation content is shown
- `1`: Metadata - only summary information (content length) is displayed
- `2`: Incremental - only newly appended messages are shown
- `3`: Full - the entire conversation is printed in detail

### Pretty Levels (`PRETTY_LEVEL`)
Controls the visual styling and decorations applied to the terminal output.
- `0`: Plain - no colors or visual decorations
- `1`: Standard - colored text with structured sections and separators

> **Note:** Command-line options always take precedence over environment variables.

## Usage
### Default Execution (Local Ollama + gemma4:12b)
```bash
cargo run
```

### Full Options Example (CLI flags, excluding llm-api-key)
```bash
cargo run \
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
cargo run -v 0 --llm-model gemma-4-31b-it
```

## Example User Queries

- "Who are you and what tools can you use?"
- "Create a Python script named `test.py` that prints 'Hello, World!'."
- "Find the Python script in the workspace and translate its output messages into Japanese."
- "Summarize the 'Anyhow' crate documentation from this URL: https://docs.rs/anyhow/latest/anyhow/."
- "Analyze the hyper documentation (https://docs.rs/hyper/latest/hyper/) and create a minimal HTTP server project in Rust."
- "Fix this broken http server."
