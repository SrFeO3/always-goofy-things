Major LLM API Compatibility

Reference for extending an OpenAI Chat / Ollama base implementation with Anthropic's dialect and the next-generation OpenAI Responses API.

# API Overview

## API Differences

| Item | OpenAI API (Chat Completions) | OpenAI API (Responses) | Ollama API (Chat API) | Anthropic API (Messages API) |
|---|---|---|---|---|
| Official API Name | OpenAI Chat Completions API | OpenAI Responses API | Ollama Chat API | Anthropic Messages API |
| Main Endpoint | /v1/chat/completions | /v1/responses | /api/chat | /v1/messages |
| Base URL (Official Example) | https://api.openai.com/v1 | https://api.openai.com/v1 | http://localhost:11434 | https://api.anthropic.com/v1 |
| Root Request Fields | messages, model, stream, etc. | input, instructions, model, max_output_tokens, stream, tools, etc. | messages, model, stream, etc. | messages, model, max_tokens, etc. |
| Core Input/Context | Array of messages (role/content) | input (string/items) + instructions | Array of messages | Array of messages (root system allowed) |
| State / Chaining | Stateless (manual history management) | Stateless (resend items) or stateful via previous_response_id / conversation | Stateless (manual history management) | Stateless (manual history management) |
| Token Limit Parameter | `max_completion_tokens` (legacy: `max_tokens`) | `max_output_tokens` (optional; includes reasoning tokens) | Not required | max_tokens is required |
| System Prompt | Set as role: "system" inside messages array (newer o1+/GPT-5 models: role: "developer" preferred) | Set as root-level `instructions` (string, or system/developer input items) | Set as role: "system" inside messages array | Set as root-level `system` (string, or array of text blocks with cache_control) |
| Streaming Response Format | SSE with data: {...} events, terminated by data: [DONE] | Event-based SSE (event:/data: pairs, data.type discriminates events; no [DONE]; ends with response.completed) | Line-delimited JSON (NDJSON) | Event-based SSE with events such as message_start and content_block_delta |
| Tool Call Request Format | `assistant` message with `tool_calls`; `function.arguments` is a JSON string | `output` item with `type: "function_call"` (`call_id`, `name`, `arguments`) | `assistant` message with `tool_calls`; `function.arguments` is a JSON object | `assistant` message `content` array with a `type: "tool_use"` block; `input` is a JSON object |
| Tool Call Arguments Type | **JSON String** (escaped string) | **JSON String** (escaped string) | **JSON Object** (raw JSON) | **JSON Object** (raw JSON) |
| Tool Result Format | Return as a `role: "tool"` message with `tool_call_id` and string `content` | Append `type: "function_call_output"` input item (`call_id`, string `output`) to the next request `input` | Return as a `role: "tool"` message with string `content` | Return as a `role: "user"` message with a `type: "tool_result"` block containing `tool_use_id` and string `content` |
| Tool Result Content Type | String containing JSON text (or plain text) | String containing JSON text (or plain text) | String containing JSON text (or plain text) | String containing JSON text (or plain text) |
| Structured Output Format | `response_format: { type: "json_schema", json_schema: {...} }`<br>Output `content` contains JSON text | `text: { format: { type: "json_schema", name, schema, strict } }`<br>Output `content[].text` (`type: "output_text"`) contains JSON text | `format: "json"` (or JSON Schema)<br>Output `content` contains JSON text | `output_config: { format: { type: "json_schema", schema: ... } }`<br>Output `content[].text` contains JSON text |
| HTTP Headers | Content-Type: application/json, Authorization: Bearer ... | Content-Type: application/json, Authorization: Bearer ... | Content-Type: application/json | Content-Type: application/json, x-api-key, anthropic-version: 2023-06-01 |

### Key Notes

The following implementation details differ significantly across providers and deserve special attention:
- Structured Outputs & JSON Parsing (See dedicated section below)
- Reasoning & Thinking: Handling & Retention (See dedicated section below)

## This App's Specific Choices

Actual requests differ from the ideal shapes above as follows:

1. **Stateless full resend on every provider, including Responses**: every request rebuilds the full conversation from session history (never `previous_response_id` / `conversation` / `store`).
2. **Reasoning is never resent on the official OpenAI APIs**. Streamed reasoning is display-only, and Responses `reasoning` item resends are not implemented (unverified with o-series + tools on the live API).
3. **Structured Outputs and reasoning-control parameters are never sent** on any provider. JSON is steered via prompting and tools.

## Provider Adoption History

Wire formats were adopted in this order: a shared OpenAI Chat / Ollama base, then Anthropic's gratuitously dissimilar dialect, and finally the next-generation OpenAI Responses API.

1. **Base: OpenAI Chat Completions / Ollama** - one shared shape: a `messages` array (`role: "system" / "user" / "assistant" / "tool"`), `assistant.tool_calls` for tool requests, `role: "tool"` messages for results; SSE `data:` events (Ollama: NDJSON) with `[DONE]` / `done: true`.
2. **Added: Anthropic (Messages API)** - a gratuitously dissimilar dialect:
   - System prompt moves to root-level `system`; there is no `role: "system"`.
   - `max_tokens` is mandatory (HTTP 400 without it).
   - Event-based SSE: `message_start` / `content_block_delta` / `message_stop`, etc.
   - Tools use `content[]` blocks: `tool_use` (request) and `tool_result` inside a `role: "user"` message (no `role: "tool"`). Parallel results must be merged into one user message.
3. **Added: OpenAI Responses API** - the next-generation official API:
   - No `messages` array; root-level `instructions` + typed `input` items.
   - `max_output_tokens` replaces `max_completion_tokens`.
   - Event-based SSE with `data.type` (`response.output_text.delta` / `response.output_item.done` / `response.completed` / `response.incomplete`); no `data: [DONE]`.
   - Tools are `function_call` / `function_call_output` input items (no `role: "tool"`).

# LLM Request

The headers are basically identical, such as Content-Type: application/json. Only Anthropic requires specific custom headers.

## OpenAI (Chat Completions)

```request body
{
  "model": "gpt-4o",
  "max_completion_tokens": 1024,
  "stream": true,
  "messages": [
    { "role": "system", "content": "You are a professional programmer." },
    { "role": "user", "content": "Hello!" }
  ]
}
```

Note: > Use max_completion_tokens to specify the maximum token count. Although max_tokens was used traditionally, only the newer field is implemented here. (Newer reasoning models additionally accept `reasoning_effort`.)

## OpenAI API (Responses)

```request body
{
  "model": "gpt-6-astra",
  "instructions": "You are a professional programmer.",
  "input": "Hello!",
  "max_output_tokens": 1024,
  "stream": true
}
```

Notes:
- System prompt: root-level `instructions` (equivalent to a system/developer message in Chat Completions). There is no `messages` field.
- Conversation: `input` is a string or an array of typed items (user/assistant messages, `function_call` / `function_call_output`, reasoning items, ...). Chat Completions-style `messages` map 1:1 onto `input` items.
- Multi-turn state: this app rebuilds the entire conversation as `input` items on every request (stateless resend). The API additionally supports server-side chaining via `previous_response_id` / `conversation`, but this app does not use them and does not set `store` (OpenAI's default server-side retention applies).
- Max output: `max_output_tokens` (optional; counts reasoning tokens too).

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
  "model": "claude-sonnet-5",
  "max_tokens": 1024,
  "stream": true,
  "system": "You are a professional programmer.",
  "messages": [
    { "role": "user", "content": "Hello!" }
  ]
}
```

Custom HTTP Headers:
```
x-api-key: YOUR_API_KEY
anthropic-version: 2023-06-01
```

Note: `temperature` / `top_p` / `top_k` are deprecated for models released after Claude Opus 4.6; values other than the defaults are rejected with HTTP 400 on those models.

## Implementation

- Standardized Fields: Consistently place max token right after model (JSON key order is arbitrary outside of the messages array).
- Anthropic Headers: Add specific headers: x-api-key for authentication and anthropic-version.

# Max Output Token

The maximum number of tokens to generate in a single response.
Recommended for Code Generation: 2048 to 4096 tokens. (Code consumes a high number of tokens due to indentations and symbols, making a larger allocation essential).

# LLM Response (Stream)

- Ollama: Pure newline-delimited JSON (NDJSON format) without any prefixes like data: or event:.
- OpenAI (Chat Completions): Server-Sent Events (SSE) format where each content line starts with `data: {...}`, terminated by a final `data: [DONE]` line.
- OpenAI (Responses API): Event-based SSE. Each event is an `event:` line plus a `data:` line whose JSON `type` identifies the event (e.g. `response.output_text.delta` for text chunks); the stream ends with `response.completed` - there is no `data: [DONE]`.
- Anthropic: Server-Sent Events (SSE) format where event: lines and data: lines alternate.

## OpenAI (Chat Completions)

```
data: {"id":"chatcmpl-123","object":"chat.completion.chunk","created":1677652288,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}
data: [DONE]
```

## OpenAI (Responses API)

```
event: response.output_text.delta
data: {"type": "response.output_text.delta", "delta": "hello", "item_id": "msg_123", "output_index": 0, "content_index": 0}

event: response.completed
data: {"type": "response.completed", "response": {"id": "resp_123", "status": "completed", "usage": {...}}}
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

Only Anthropic and the OpenAI Responses API require event-based parsing.

Anthropic:
1. Read the `event:` line to identify the event type (`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`, `ping`).
2. Parse the following `data:` line as JSON. Ignore `event: ping` heartbeat events.
3. Stop reading after `event: message_stop`.

OpenAI (Responses API) - parse the JSON in each `data:` line and branch on `data.type` (no `[DONE]` event exists):
1. Text output: `data.type == "response.output_text.delta"` -> append `data.delta`.
2. Tool call output: `data.type == "response.output_item.done"` with `item.type == "function_call"` -> parse the complete JSON string in `item.arguments`.
3. End: the Stream ends with `data.type == "response.completed"` (normal end; token usage is in `data.response.usage`, including `output_tokens_details.reasoning_tokens`) or `data.type == "response.incomplete"` (abnormal end - no usage; `data.response.incomplete_details.reason` says why). Both are treated as terminal events.

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
- `message_delta`:　Store `data.usage.output_tokens` (cumulative) for token statistics.

### Thinking (extended thinking) stream
- `content_block_start` with `type: "thinking"` begins a thinking block.
- `content_block_delta` with `delta.type == "thinking_delta"`: append `delta.thinking` fragments to the block buffer (this is NOT a `text_delta`).
- A `content_block_delta` with `delta.type == "signature_delta"` delivers `delta.signature` right before `content_block_stop`. Thinking blocks must be sent back unmodified with this signature (a modified block results in HTTP 400).
- When `thinking.display == "omitted"` (or thinking is safety-redacted), an opaque `type: "redacted_thinking"` block (`data` only) is returned instead; pass it back unchanged.
- Enable via `thinking: { "type": "enabled", "budget_tokens": N }` (N >= 1024, counts toward `max_tokens`) or `thinking: { "type": "adaptive" }`. Thinking token usage is also reported in `usage.output_tokens_details.thinking_tokens`.

# Tool Calling (Request)

## Anthropic Tool Definition Format

Anthropic uses a different JSON schema for tool definitions.
Convert tool definitions to the Anthropic format during initialization.

```json
{
   "model": "claude-sonnet-5",
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
- OpenAI: the message requires `tool_call_id` to reference the call:
  ```
  { "role": "tool", "tool_call_id": "call_abc", "content": "..." }
  ```
- Ollama native `/api/chat`: tool calls carry no `id`, so there is no `tool_call_id`; submit `{ "role": "tool", "content": "..." }` and, per the official API, optionally add `tool_name` (the name of the executed tool).

```Rust
choices[0].delta.tool_calls: [
    { "id": "call_abc", "function": { "name": "read_file", "arguments": "..."} }
]
```

## OpenAI API (Responses)

The model returns the call as an output item, and the result goes back as a new input item (`type: "function_call_output"`). There is no `role: "tool"` message.

```
{ "type": "function_call", "call_id": "call_abc", "name": "read_file", "arguments": "{\"path\": \"src/main.rs\"}" }

{ "type": "function_call_output", "call_id": "call_abc", "output": "{\"stdout\": \"...\", \"stderr\": \"\", \"exit_code\": 0}" }
```

Stateless usage: send the previous response's `output` items plus this `function_call_output` item as the next request's `input`. This app rebuilds the `input` items from its session history on every turn: assistant `tool_calls` become `function_call` items and `role: "tool"` results become `function_call_output` items.

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

## OpenAI API (Responses)
- Tool Call Arguments: The `arguments` field of a `function_call` output item is returned as an escaped **JSON String** (same as Chat Completions) - parse it explicitly before use.
- Structured Outputs: Set `text: { "format": { "type": "json_schema", "name": ..., "schema": ..., "strict": true } }`. The guaranteed-valid JSON is returned as JSON text inside the `type: "output_text"` block of `output[].message.content`.

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
| **OpenAI (Chat / Responses)** | JSON string -> `JSON.parse()` required | JSON text in `content` / `output_text` -> `JSON.parse()` required | Return JSON text as a string |
| **Ollama (`/api/chat`)** | JSON object -> no parsing required     | JSON text in `message.content` -> `JSON.parse()` required | Return JSON text as a string |
| **Anthropic**            | JSON object -> no parsing required     | JSON text in `content[].text` -> `JSON.parse()` required  | Return JSON text as a string |

# Reasoning & Thinking: Handling & Retention

## OpenAI (Official: o-series / GPT-5 family: Chat Completions / Responses API)
- receive (Chat Completions): raw reasoning text is **not** exposed by the official API (reasoning tokens are opaque). Only token counts are reported (e.g., `usage.completion_tokens_details.reasoning_tokens`), and there is no reasoning text field to retain or send back.
- receive (Responses API): reasoning is opaque here as well; it arrives as `type: "reasoning"` output items. Readable `summary` / `summary_text` blocks appear only when summaries are requested (`reasoning.summary`); otherwise the item carries an opaque `encrypted_content` (plus `usage.output_tokens_details.reasoning_tokens`).
- send back: nothing for Chat Completions - echo `content` / `tool_calls` as usual (no `reasoning_content` in official responses). For the Responses API, this app likewise sends nothing reasoning-related: it rebuilds `input` from its session history (assistant text, `function_call`, `function_call_output` items) and drops streamed `reasoning_content` on resend - the API only accepts its own opaque `reasoning` items, which this app never stores. `previous_response_id` / `conversation` chaining and `store: false` are not used; every request resends the full conversation.
- on stream: same as non-stream; request per-chunk token usage with `stream_options: { "include_usage": true }` if needed (Chat Completions).
- control: `reasoning_effort` (Chat Completions) / `reasoning: { effort, summary }` (Responses API). Reasoning **summaries** are only available in the Responses API (`reasoning.summary` output items), not in Chat Completions.
- Caution: OpenAI steers new development to the Responses API; some recent models (e.g., GPT-6 Astra) do not support function calling on Chat Completions.

## OpenAI-compatible third-party reasoning models (DeepSeek-R1 era, Qwen3, Kimi, ...)
- receive: `choices[].message.reasoning_content`
- send back: provider-dependent - DeepSeek official guidance: do not send back, except in tool responses (see reference below)
- on stream: `delta.reasoning_content`

## Ollama (thinking models: DeepSeek-R1, Qwen3, ...)
- receive: `message.thinking` on `/api/chat` (`thinking` field, separated from `content`)
- send back: `thinking` inside the `role: "assistant"` message
- control: request-level `think` parameter (`true` / `false`, or a level: `"low"`, `"medium"`, `"high"`, `"max"`)
- note: Ollama's OpenAI-compatible `/v1/chat/completions` endpoint also accepts `reasoning_effort` / `reasoning.effort` for thinking models

## Anthropic (Claude 4.5+ / Claude Sonnet 5, ...)
- receive: `type: "thinking"` and `type: "text"` blocks in the `content[]` array (`type: "redacted_thinking"` when thinking is redacted/omitted)
- send back: the entire `content[]` array (thinking and text objects, with `signature` intact) in `role: "assistant"` - required when tools are used with extended thinking
- enable: `thinking: { "type": "enabled", "budget_tokens": N }` (N >= 1024, counted toward `max_tokens`) or `thinking: { "type": "adaptive" }`; in streams, thinking arrives via `thinking_delta` / `signature_delta` events (see the streaming section above)

## reference

### DeepSeek Official API (current models: deepseek-v4-flash / deepseek-v4-pro)
- OpenAI-compatible base URL: `https://api.deepseek.com` (OpenAI Chat format)
- Anthropic-compatible base URL: `https://api.deepseek.com/anthropic` (Messages API format)
- Reasoning control (OpenAI format example): request params `thinking: { "type": "enabled" }` and `reasoning_effort`
- Note: legacy DeepSeek-R1-era docs exposed `reasoning_content` (see the OpenAI-compatible section above); the current quickstart centers on the deepseek-v4-* models with the new parameters, so verify against the model/endpoint actually used.

### DeepSeek-R1 via Third-party / Ollama Stream (legacy)
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
