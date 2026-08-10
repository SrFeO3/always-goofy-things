# Always-Goofy-Things

To demonstrate the core mechanics of iterative LLM function-calling, this lightweight, experimental CLI application showcases the workflow in the context of AI-assisted software development. It interacts with LLMs to reason about tasks, executes system tools via user confirmation paired with an unsafe reflex mode, and streams the "thinking" process.

> [!CAUTION]
> **Experimental Implementation**: AI unpredictability and bugs may cause unexpected behavior.
> **Security Risk**: File, shell, and network access enabled. Flawed AI commands may cause system damage, data loss, or data exfiltration.
> **Billing Alert**: AI reasoning loops or oversized contexts can rapidly spike API costs. Monitor closely.

## Features
- **Tool-Augmented Iteration**: Automatically calls tools for file I/O, search, bash execution, and web fetching.
- **Safety Guards & Unsafe Reflex**: Balances explicit user approval (y/N) and experimental deterministic "unsafe reflex" mode that dangerously auto-resolves safety checkpoints.
- **Open Standards**: Supports Ollama, OpenAI-compatible, and Anthropic-compatible APIs with streaming reasoning.

## Requirements
- **Rust**: Latest stable version (Cargo).
- **Backend**: Ollama, OpenAI-compatible, or Anthropic-compatible API.
- **Tools**: `bash`, `grep`, and internet connectivity (for web fetching).

## Configuration

Configuration can be set via environment variables or command-line flags (flags take precedence).
| CLI Flag | Env Var | Description | Default |
| :--- | :--- | :--- | :--- |
| `-w, --working-dir <DIR>` | `WORKING_DIR` | Directory where AI tools operate. | `.` |
| `-u, --llm-url <URL>` | `LLM_URL` | LLM Chat API endpoint. | `http://localhost:11434/api/chat` |
| `-P, --llm-provider <PROVIDER>` | `LLM_PROVIDER` | LLM API provider (auto-detected from URL if not specified). | (auto) |
| `-m, --llm-model <MODEL>` | `LLM_MODEL` | LLM model name to use. | `gemma4:12b` |
| `-k, --llm-api-key <KEY>` | `LLM_API_KEY` | API key for authentication. | (none) |
| `-r, --llm-rpm <NUM>` | `LLM_RPM` | Maximum requests per minute for the LLM API. | `0` (unlimited) |
| `-T, --max-output-tokens <NUM>` | `MAX_OUTPUT_TOKENS` | Maximum output tokens per LLM request. | `16384` |
| `-E, --max-reasoning-empty-responses <NUM>` | `MAX_REASONING_EMPTY_RESPONSES` | Stop after N consecutive empty LLM responses in the reasoning loop (`0` = unlimited). | `2` |
| `--max-reasoning-turns <NUM>` | `MAX_REASONING_TURNS` | Max LLM calls per user message (`0` = unlimited). In batch mode, exceeding it exits with error. | `30` |
| `-R, --tool-result-format <FORMAT>` | `TOOL_RESULT_FORMAT` | How tool results are structured when sent to the LLM. | `json_string` |
| `-v, --verbose-level <LEVEL>` | `VERBOSE_LEVEL` | LLM API traffic verbosity (`0`-`4`). | `1` |
| `-p, --pretty-level <LEVEL>` | `PRETTY_LEVEL` | UI decoration level (`0`-`1`). | `1` |
| `-s, --session-label <LABEL>` | `SESSION_LABEL` | Label for session persistence files (enables running multiple sessions). | `default` |
| `-q, --query <QUERY>` | (none) | Run in batch mode: execute once and exit, printing the final answer to stdout. | (interactive) |
| `-o, --output <FILE>` | `OUTPUT_FILE` | Write each turn's final LLM response to a file. | (none) |
| `-t, --todo <MODE>` | `TODO_MODE` | Todo-based Plan-and-Execute mode. `0`=ReAct (default), `1`=Static plan, `2`=AI-driven dynamic replanning. | `0` |
| `--unsafe-reflex` | `UNSAFE_REFLEX_MODE` | Bypasses manual confirmation for tool-execution safety checkpoints. | false |

> [!NOTE]
> **Renamed**: `MAX_EMPTY_RETRY` / `--max-empty-retry` was replaced by `MAX_REASONING_EMPTY_RESPONSES` / `--max-reasoning-empty-responses`. New semantics: `N` = stop after N consecutive empty responses (`1` = stop at the first empty response, `2` = retry once, `0` = unlimited). The previous default `1` (retry once) now corresponds to `2`. The old env var and flag names are **removed** and will not be recognized.

### LLM Provider (`LLM_PROVIDER`)

Controls which provider-specific API format is used. If not set, the provider is auto-detected from LLM_URL.
- `openai` - OpenAI-compatible API format. Endpoint: `/v1/chat/completions`
- `ollama` - Ollama API format. Endpoint: `/api/chat`
- `anthropic` - Anthropic-compatible API format. Endpoint: `/v1/messages` (gratuitously dissimilar).

### Tool Result Format (`TOOL_RESULT_FORMAT`)

Controls how tool results are structured when sent back to the LLM.
- `json_string` (default): escaped JSON string.
- `text`: plain text.
- `json_structured`: JSON object.

### Verbosity Levels (`VERBOSE_LEVEL`)

Controls how much LLM API traffic is displayed on the terminal.
- `0`: Silent - no conversation content is shown
- `1`: Metadata - only summary information (content length) is displayed
- `2`: Incremental - only newly appended messages are shown
- `3`: Full - the entire conversation is printed in detail, including raw tool call delta SSE lines
- `4`: Raw - same as Level 3, plus every raw SSE line from the response stream

### Pretty Levels (`PRETTY_LEVEL`)

Controls the visual styling and decorations applied to the terminal output.
- `0`: Plain - no colors or visual decorations
- `1`: Standard - colored text with structured sections and separators

### Todo Mode (`TODO_MODE`)

Plan-and-Execute execution for complex multi-step tasks. Reads `./todo.md`, resets LLM context between steps, and carries state forward via the file.

- `0` (default): Standard ReAct loop. Single-turn tasks.
- `1`: Static sequential execution from a user-prepared plan. Known step-by-step workflows.
- `2`: Dynamic AI-driven replanning. The AI rewrites `./todo.md` as it works. Exploratory / research tasks.

See [docs/todo-mode.md](docs/todo-mode.md) for sample `./todo.md` files and quick-start guides.

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
> In batch mode, large files are attached without confirmation and tools that usually prompt `y/N` are automatically denied.

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
- "@src/main.rs, @Cargo.toml Explain the structure of these files."
- "@diagram.png Describe this image."

**Complex tasks:**
- "Find the Python script in the workspace and translate its output messages into Shakespearean English."
- "Summarize the 'Anyhow' crate documentation from this URL: https://docs.rs/anyhow/latest/anyhow/."
- "@@spec.pdf Fix this broken http server based on the specification."

**Long complex tasks - too large for a single LLM context. Use todo mode:**

Requires a `./todo.md` plan file instead of a typed query, and runs immediately.

Example tasks:
- "Refactor this entire legacy codebase, writing unit tests for every module."
- "Crawl the docs of a web framework and generate a migration guide for v2 to v3."
- "Design a DB schema, write backend APIs, build frontend, and verify end-to-end."

See [docs/todo-mode.md](docs/todo-mode.md) for how to use.
