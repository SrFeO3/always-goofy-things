Three Major LLM API Compatibility

Reference for extending existing OpenAI/Ollama implementations with Anthropic support.

# API Overview

## API Differences

| Item | OpenAI API (Chat Completions) | Ollama API (Chat API) | Anthropic API (Messages API) |
|---|---|---|---|
| Official API Name | OpenAI Chat Completions API | Ollama Chat API | Anthropic Messages API |
| Main Endpoint | /v1/chat/completions | /api/chat | /v1/messages |
| Base URL (Official Example) | https://api.openai.com/v1 | http://localhost:11434 | https://api.anthropic.com/v1 |
| Root Request Fields | messages, model, stream, etc. | messages, model, stream, etc. | messages, model, max_tokens, etc. |
| Token Limit Parameter | `max_completion_tokens` (legacy: `max_tokens`) | Not required | max_tokens is required |
| System Prompt | Set as role: "system" inside messages array | Set as role: "system" inside messages array | Set as root-level system: "..." string |
| Streaming Response Format | SSE with data: {...} events | Line-delimited JSON (NDJSON) | Event-based SSE with events such as message_start and content_block_delta |
| Tool Call Request Format | `assistant` message with `tool_calls`; `function.arguments` is a JSON string | `assistant` message with `tool_calls`; `function.arguments` is a JSON object | `assistant` message `content` array with a `type: "tool_use"` block; `input` is a JSON object |
| Tool Call Arguments Type | **JSON String** (escaped string) | **JSON Object** (raw JSON) | **JSON Object** (raw JSON) |
| Tool Result Format | Return as a `role: "tool"` message with `tool_call_id` and string `content` | Return as a `role: "tool"` message with string `content` | Return as a `role: "user"` message with a `type: "tool_result"` block containing `tool_use_id` and string `content` |
| Tool Result Content Type | String containing JSON text (or plain text) | String containing JSON text (or plain text) | String containing JSON text (or plain text) |
| Structured Output Format | `response_format: { type: "json_schema", json_schema: {...} }`<br>Output `content` contains JSON text | `format: "json"` (or JSON Schema)<br>Output `content` contains JSON text | `output_config: { format: { type: "json_schema", json_schema: ... } }`<br>Output `content[].text` contains JSON text |
| HTTP Headers | Content-Type: application/json, Authorization: Bearer ... | Content-Type: application/json | Content-Type: application/json, x-api-key, anthropic-version |

### Key Notes

The following implementation details differ significantly across providers and deserve special attention:
- Structured Outputs & JSON Parsing (See dedicated section below)
- Reasoning & Thinking: Handling & Retention (See dedicated section below)

## Common Behaviors

- Streaming is controlled by the root-level `"stream": true/false` flag.
- HTTP requests and responses use `Content-Type: application/json`.

## 4 Key Implementation Considerations for Anthropic API (Messages API)

When adding Anthropic API (Claude-compatible) support to an existing OpenAI/Ollama-compatible implementation, note the following differences.

1. Different system prompt hierarchy
   - Unlike OpenAI and Ollama, Anthropic does not accept role: "system" inside the messages array.
   - The system prompt must be specified as a top-level `system` property.

2. max_tokens is mandatory
   - OpenAI allows omission or automatic handling of token limits.
   - Anthropic requires an explicit integer `max_tokens` value; otherwise, the request returns HTTP 400.

3. Streaming parsing is more complex
   - OpenAI streams only require extracting data from `data: {...}` events.
   - Anthropic includes event types such as `message_start` and `content_block_delta` before the data payload, requiring event-based parsing logic.

4. Tool calling structure differs
  - A) LLM → Tool: Tool call request returned by LLM
    - OpenAI/Ollama: `assistant.tool_calls`
    - Anthropic: `assistant.content[]` with `type: "tool_use"`
  - B) Tool → LLM: Tool result response returned to LLM
    - OpenAI/Ollama: Message with `role: "tool"`
    - Anthropic: `user.content[]` with `type: "tool_result"`

# LLM Request

The headers are basically identical, such as Content-Type: application/json. Only Anthropic requires specific custom headers.

## OpenAI

```request body
{
  "model": "gpt-4o",
  "max_completion_tokens": 1024,
  "stream": true
  "messages": [
    { "role": "system", "content": "You are a professional programmer." },
    { "role": "user", "content": "Hello!" }
  ]
}
```

Note: > Use max_completion_tokens to specify the maximum token count. Although max_tokens was used traditionally, only the newer field is implemented here.

## Ollama

```request body
{
  "model": "llama3",
  "messages": [
    { "role": "system", "content": "You are a professional programmer." },
    { "role": "user", "content": "Hello!" }
  ],
  "options": {
    "num_predict": 1024
  },
  "stream": true
}
```

## Anthropic API

```request body
{
  "model": "claude-3-5-sonnet-20241022",
  "max_tokens": 1024,
  "stream": true
  "system": "You are a professional programmer.",
  "messages": [
    { "role": "user", "content": "Hello!" }
  ]
}
```

Custom HTTP Headers:
```
x-api-key: YOUR_API_KEY
anthropic-version: 2023-11-01
```

## Implementation

- Standardized Fields: Consistently place max token right after model (JSON key order is arbitrary outside of the messages array).
- Anthropic Headers: Add specific headers: x-api-key for authentication and anthropic-version.

# Max Output Token

The maximum number of tokens to generate in a single response.
Recommended for Code Generation: 2048 to 4096 tokens. (Code consumes a high number of tokens due to indentations and symbols, making a larger allocation essential).

# LLM Response (Stream)

- Ollama: Pure newline-delimited JSON (NDJSON format) without any prefixes like data: or event:.
- OpenAI: Server-Sent Events (SSE) format where each line starts with data: {...} only.
- Anthropic: Server-Sent Events (SSE) format where event: lines and data: lines alternate.

## OpenAI

```
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}
```

## Ollama

```
{"model":"llama3","created_at":"2026-07-07T16:53:00Z","message":{"role":"assistant","content":"hello"},"done":false}
```

## Anthropic API

```
event: content_block_delta
data: {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}}
```

## Implementation

Only Anthropic requires event-based parsing:
1. Read the `event:` line to identify the event type.
2. Parse the following `data:` line as JSON.

### Text output
- When `delta.type == "text_delta"`: Extract generated text from `delta.text`.

### Tool call output
- When `content_block_start` detects `type: "tool_use"`:
  - Handle subsequent `content_block_delta` events.
  - When `delta.type == "input_json_delta"`:
    - Append `delta.partial_json` fragments to a String buffer.
    - After all fragments are received, parse the complete JSON object.
    - Example result:
      ```json
      { "path": "src/main.rs" }
      ```
### Required metadata events
- `message_start`:　Store `data.usage.input_tokens` for token statistics.
- `content_block_start`:　Use the `type` field to identify the output mode (`text`, `tool_use`, or `thinking`).
- `message_delta`:　Store `data.usage.output_tokens` for token statistics.

# Tool Calling (Request)

## Anthropic Tool Definition Format

Anthropic uses a different JSON schema for tool definitions.
Convert tool definitions to the Anthropic format during initialization.

```json
{
   "model": "claude-sonnet-4-20250514",
   "max_tokens": 4096,
   "system": "You are a professional programmer.",
   "messages": [...],
   "tools": [
      {
         "name": "read_file",
         "description": "Read the contents of a file...",
         "input_schema": {
            "type": "object",
            "properties": {
               "path": { "type": "string" },
               "start_line": { "type": "integer" },
               "end_line": { "type": "integer" }
            },
            "required": ["path"]
         }
      }
   ]
}
```

# Tool Calling (Response)

## OpenAI / Ollama

Tool results are returned as a message with `role: "tool"`.
```
{ "role": "tool", "tool_call_id": "call_abc", "content": "..." }
```

```Rust
choices[0].delta.tool_calls: [
    { "id": "call_abc", "function": { "name": "read_file", "arguments": "..."} }
]
```

## Anthropic API

Tool results are returned inside the content array of a role: "user" message.
(role: "assistant" is not used.)

```
{
   "role": "user",
   "content": [
      {
         "type": "tool_result",
         "tool_use_id": "toolu_abc",
         "content": "{\"stdout\": \"...\", \"stderr\": \"\", \"exit_code\": 0}"
      }
   ]
}
```

# Automatic Provider Detection

```rust
#[derive(Debug, PartialEq)]
enum LlmProvider {
    Anthropic,
    Ollama,
    OpenAi,
    OpenAiCompatible,
}

fn detect_provider(url: &str) -> LlmProvider {
    let url_lower = url.to_lowercase();

    if url_lower.contains("api.anthropic.com") || url_lower.contains("/v1/messages") {
        return LlmProvider::Anthropic;
    }

    if url_lower.contains("/api/chat") || url_lower.contains(":11434") {
        return LlmProvider::Ollama;
    }

    if url_lower.contains("api.openai.com") {
        return LlmProvider::OpenAi;
    }

    LlmProvider::OpenAiCompatible
}
```

# Structured Outputs & JSON Parsing

## OpenAI API (Chat Completions)
- Tool Call Arguments: When the model invokes a function, the `function.arguments` field returns an escaped **JSON String** (e.g., `"{ \"location\": \"Tokyo\" }"`). Your application must explicitly parse it (e.g., via `JSON.parse()`) before consumption.
- Structured Outputs: Even when configuring `response_format` for JSON/JSON Schema, the resulting message `content` is returned as JSON text in the `content` string.

## Ollama API (Chat API)
- Tool Call Arguments: If using the OpenAI-compatible endpoint (`/v1/chat/completions`), it behaves like OpenAI (escaped JSON string). However, when using the native `/api/chat` endpoint, `tool_calls[].function.arguments` is delivered directly as a pre-parsed **JSON Object**.
- Structured Outputs: When specifying `format: "json"` (or a JSON Schema) via `/api/chat`, the response `message.content` is returned as JSON text in the content string.

## Anthropic API (Messages API)
- Tool Call Arguments: The `input` field within the `tool_use` content block is provided directly as a structured, unescaped **JSON Object** (no manual parsing required).
- Structured Outputs: Anthropic supports native structured outputs via the `output_config.format` parameter. Similar to OpenAI, when using this mode, the guaranteed-valid JSON is returned as JSON text in the content string inside the message's `content[].text` field.

## Tool Result Submission (API-Specific Rules)
- OpenAI & Anthropic: When returning execution results back to the model, the payload submitted within the content field must be formatted as a string containing JSON text (or plain text).
- Ollama (Native `/api/chat`): `content` accepts a plain string, including JSON text. The model can reliably consume stringified JSON.

## Implementation

| API                      | Tool Call Arguments                   | Structured Outputs                                       | Tool Results                 |
| ------------------------ | ------------------------------------- | -------------------------------------------------------- | ---------------------------- |
| **OpenAI**               | JSON string -> `JSON.parse()` required | JSON text in `content` -> `JSON.parse()` required         | Return JSON text as a string |
| **Ollama (`/api/chat`)** | JSON object -> no parsing required     | JSON text in `message.content` -> `JSON.parse()` required | Return JSON text as a string |
| **Anthropic**            | JSON object -> no parsing required     | JSON text in `content[].text` -> `JSON.parse()` required  | Return JSON text as a string |

# Reasoning & Thinking: Handling & Retention

## OpenAI (o1, o3-mini, ...)
- receive: `choices[].message.reasoning_content` on OpenAI-compatible reasoning models
- send back: `reasoning_content` in `role: "assistant"`
- on stream: `delta.reasoning_content`

## Ollama (DeepSeek-R1, Qwen3.5 Reasoning, ...)
- receive: `message.thinking` (`thinking` field completely separated with `content`)
- send back: `thinking` in `role: "assistant"`

## Anthropic(Claude 3.7 Sonnet, ...)
- receive: `type: "thinking"` and `type: "text"` blocks in `content[]` array
- send back: the entire `content[]` array (including both thinking and text objects) in `role: "assistant"`

## reference

### DeepSeek Official API (DeepSeek-R1, ...)
- receive: `choices[].message.reasoning_content`
- send back: do not send back, except in tool responses

### DeepSeek via Third-party / Ollama Stream (DeepSeek-R1, ...)
- receive: inside `<think>...</think>` tags in a single `content` string
- send back: do not send back (strip `<think>` tags and the inner text from content), except in tool responses

## Implementation

```Rust
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    Thinking {
            thinking: String,
            signature: Option<String>
        },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
enum MessageContent {
    String(String),
    Array(Vec<ContentBlock>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AssistantMessage {
    // OpenAI / DeepSeek
    reasoning_content: Option<String>,
    // Ollama
    thinking: Option<String>,
    // Anthropic (string, or block array)
    content: Option<MessageContent>,
}

fn extract_reasoning(message: &AssistantMessage) -> String {
    // OpenAI / DeepSeek
    if let Some(ref r) = message.reasoning_content {
        return r.clone();
    }

    // Ollama
    if let Some(ref t) = message.thinking {
        return t.clone();
    }

    // 2. Anthropic
    if let Some(MessageContent::Array(ref blocks)) = message.content {
        for block in blocks {
            if let ContentBlock::Thinking { ref thinking } = block {
                return thinking.clone();
            }
        }
    }

    String::new()
}
```
