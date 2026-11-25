Major LLM API Compatibility

Reference for extending an OpenAI Chat / Ollama base implementation with Anthropic's dialect, the next-generation OpenAI Responses API, and Google's native Gemini API.

# API Overview

## API Differences

| Item | OpenAI API (Chat Completions) | OpenAI API (Responses) | Ollama API (Chat API) | Anthropic API (Messages API) | Gemini API (native) |
|---|---|---|---|---|---|
| Official API Name | OpenAI Chat Completions API | OpenAI Responses API | Ollama Chat API | Anthropic Messages API | Gemini API (generateContent: legacy / Interactions: current) |
| Main Endpoint | /v1/chat/completions | /v1/responses | /api/chat | /v1/messages | POST /v1beta/models/{model}:generateContent (:streamGenerateContent for stream) / POST /v1beta/interactions |
| Base URL (Official Example) | https://api.openai.com/v1 | https://api.openai.com/v1 | http://localhost:11434 | https://api.anthropic.com/v1 | https://generativelanguage.googleapis.com/v1beta |
| Root Request Fields | messages, model, stream, etc. | input, instructions, model, max_output_tokens, stream, tools, etc. | messages, model, stream, etc. | messages, model, max_tokens, etc. | contents, system_instruction, generationConfig, tools (generateContent) / model, input, generation_config, tools (Interactions) |
| Core Input/Context | Array of messages (role/content) | input (string/items) + instructions | Array of messages | Array of messages (root system allowed) | contents[] {role: user\|model, parts[]} / input string\|steps[] |
| State / Chaining | Stateless (manual history management) | Stateless (resend items) or stateful via previous_response_id / conversation | Stateless (manual history management) | Stateless (manual history management) | Stateless resend (Interactions also: previous_interaction_id) |
| Token Limit Parameter | `max_completion_tokens` (legacy: `max_tokens`) | `max_output_tokens` (optional; includes reasoning tokens) | Not required | max_tokens is required | `maxOutputTokens` / `max_output_tokens` (includes thought tokens) |
| System Prompt | Set as role: "system" inside messages array (newer o1+/GPT-5 models: role: "developer" preferred) | Set as root-level `instructions` (string, or system/developer input items) | Set as role: "system" inside messages array | Set as root-level `system` (string, or array of text blocks with cache_control) | Top-level `system_instruction` (object in generateContent, string in Interactions) |
| Streaming Response Format | SSE with data: {...} events, terminated by data: [DONE] | Event-based SSE (event:/data: pairs, data.type discriminates events; no [DONE]; ends with response.completed) | Line-delimited JSON (NDJSON) | Event-based SSE with events such as message_start and content_block_delta | SSE `data:` chunks (generateContent) / event-based SSE steps (Interactions, ends with `done` / `[DONE]`) |
| Tool Call Request Format | `assistant` message with `tool_calls`; `function.arguments` is a JSON string | `output` item with `type: "function_call"` (`call_id`, `name`, `arguments`) | `assistant` message with `tool_calls`; `function.arguments` is a JSON object | `assistant` message `content` array with a `type: "tool_use"` block; `input` is a JSON object | `functionCall {name, args, id}` part / `function_call {name, arguments, id}` step |
| Tool Call Arguments Type | **JSON String** (escaped string) | **JSON String** (escaped string) | **JSON Object** (raw JSON) | **JSON Object** (raw JSON) | **JSON Object** (raw JSON; Interactions streams `arguments_delta` chunks) |
| Tool Result Format | Return as a `role: "tool"` message with `tool_call_id` and string `content` | Append `type: "function_call_output"` input item (`call_id`, string `output`) to the next request `input` | Return as a `role: "tool"` message with string `content` | Return as a `role: "user"` message with a `type: "tool_result"` block containing `tool_use_id` and string `content` | `functionResponse {name, id, response: object}` part / `function_result {name, call_id, result[]}` step |
| Tool Result Content Type | String containing JSON text (or plain text) | String containing JSON text (or plain text) | String containing JSON text (or plain text) | String containing JSON text (or plain text) | Object (generateContent) / content blocks (Interactions) |
| Structured Output Format | `response_format: { type: "json_schema", json_schema: {...} }`<br>Output `content` contains JSON text | `text: { format: { type: "json_schema", name, schema, strict } }`<br>Output `content[].text` (`type: "output_text"`) contains JSON text | `format: "json"` (or JSON Schema)<br>Output `content` contains JSON text | `output_config: { format: { type: "json_schema", schema: ... } }`<br>Output `content[].text` contains JSON text | `responseMimeType: "application/json"` + `responseJsonSchema` (generateContent) / `response_format` (Interactions)<br>Output text contains JSON text |
| HTTP Headers | Content-Type: application/json, Authorization: Bearer ... | Content-Type: application/json, Authorization: Bearer ... | Content-Type: application/json | Content-Type: application/json, x-api-key, anthropic-version: 2023-06-01 | Content-Type: application/json, `x-goog-api-key` (`Api-Revision` on Interactions) |

### Key Notes

The following implementation details differ significantly across providers and deserve special attention:
- Structured Outputs & JSON Parsing (See dedicated section below)
- Reasoning & Thinking: Handling & Retention (See dedicated section below)
- Gemini has two envelopes for the same concepts: legacy generateContent (`contents`/`parts`) and current Interactions (`input`/`steps`). Do not mix them in one request.
- Gateway note: Gemini via `.../v1beta/openai/` or OpenCode Go uses the OpenAI Chat shape (no native mapping); only direct `generativelanguage.googleapis.com` needs the Gemini column.

## This App's Specific Choices

Actual requests differ from the ideal shapes above as follows:

1. **Client-held history on every provider, including Responses**: every request rebuilds the full conversation from client-side session history (never API-held `previous_response_id` / `conversation` / `store`).
2. **Reasoning is never resent on the official OpenAI APIs**. Streamed reasoning is display-only, and Responses `reasoning` item resends are not implemented (unverified with o-series + tools on the live API).
3. **Structured Outputs and reasoning-control parameters are never sent** on any provider. JSON is steered via prompting and tools.

## Provider Adoption History

Wire formats were adopted in this order: a shared OpenAI Chat / Ollama base, then Anthropic's gratuitously dissimilar dialect, then the next-generation OpenAI Responses API. Google's native Gemini API is surveyed below but not yet implemented (Gemini via OpenAI-compatible gateways already works through the OpenAI shape).

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
4. **Surveyed, not yet implemented: Gemini (native)** - `system_instruction` instead of `role: "system"`; `contents[]` (`user`/`model` + `parts`) in legacy generateContent, `steps[]` (`user_input`/`function_call`/`function_result`/`thought`) in current Interactions; tools are `functionCall`/`functionResponse` objects matched by `id`. (Gemini via OpenAI-compatible gateways needs no new mapping and already works.)

# LLM Request

The headers are basically identical, such as Content-Type: application/json. Anthropic (`x-api-key` + `anthropic-version`) and direct Gemini (`x-goog-api-key`) require specific custom headers.

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

## Gemini API (native)

Direct endpoint: `POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` (stream: `...:streamGenerateContent?alt=sse`); Interactions: `POST .../v1beta/interactions` (`?alt=sse` + `"stream": true`).

```request body
{
  "system_instruction": { "parts": [{ "text": "You are a professional programmer." }] },
  "contents": [{ "role": "user", "parts": [{ "text": "Hello!" }] }],
  "generationConfig": { "maxOutputTokens": 1024 }
}
```

Interactions differs: `model` moves into the body, `input` replaces `contents` (string or `steps[]`), `system_instruction` is a plain string, `generation_config` replaces `generationConfig`. Roles are only `user`/`model` (generateContent) or step types (Interactions); there is no `role: "system"`.

## Implementation

- Standardized Fields: Consistently place max token right after model (JSON key order is arbitrary outside of the messages array).
- Anthropic Headers: Add specific headers: x-api-key for authentication and anthropic-version.
- Gemini Headers: direct calls use `x-goog-api-key` (Interactions adds `Api-Revision`); compat paths and Vertex use `Authorization: Bearer`. Streaming is endpoint-selected (`:streamGenerateContent?alt=sse` vs `/interactions?alt=sse` + `"stream": true`).

# Max Output Token

The maximum number of tokens to generate in a single response.
Recommended for Code Generation: 2048 to 4096 tokens. (Code consumes a high number of tokens due to indentations and symbols, making a larger allocation essential).

# LLM Response (Stream)

- Ollama: Pure newline-delimited JSON (NDJSON format) without any prefixes like data: or event:.
- OpenAI (Chat Completions): Server-Sent Events (SSE) format where each content line starts with `data: {...}`, terminated by a final `data: [DONE]` line.
- OpenAI (Responses API): Event-based SSE. Each event is an `event:` line plus a `data:` line whose JSON `type` identifies the event (e.g. `response.output_text.delta` for text chunks); the stream ends with `response.completed` - there is no `data: [DONE]`.
- Anthropic: Server-Sent Events (SSE) format where event: lines and data: lines alternate.
- Gemini (generateContent): SSE `data: {GenerateContentResponse}` chunks (same shape as the sync response); no `event:` lines; stream close = end.
- Gemini (Interactions): event-based SSE `event:` + `data:` pairs (`interaction.created` / `step.start` / `step.delta` / `step.stop` / `interaction.completed`); ends with `event: done` `data: [DONE]`.

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

## Gemini API (native)

```
data: {"candidates": [{"content": {"parts": [{"text": "hello"}], "role": "model"}}]}
```

generateContent streams `GenerateContentResponse` objects as bare `data:` chunks (no `event:` lines; close = end). Interactions streams typed steps instead: `interaction.created` / `step.start` / `step.delta` (`text` / `thought_summary`+`thought_signature` / `arguments_delta`) / `step.stop` / `interaction.completed` (`completed` vs `requires_action`), ending with `event: done` / `data: [DONE]`. Usage: `usageMetadata` (generateContent) / `interaction.usage` (Interactions); both count thought tokens.

## Implementation

Anthropic, the OpenAI Responses API, and Gemini Interactions require event-based parsing. Gemini generateContent streaming is plain `data:` JSON (no `event:` lines).

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

### Gemini (generateContent) stream
- Each `data:` line is a full `GenerateContentResponse`; concatenate `parts[].text` (`thought: true` parts are display-only but must be resent). Tool calls arrive as `functionCall` parts; map results by `id`.

### Gemini (Interactions) stream
- Branch on `data.event_type`; accumulate `arguments_delta` per `index` and `JSON.parse` after `step.stop`; keep `thought_signature` verbatim for stateless resends.

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

## Gemini Tool Definition Format (native; both envelopes)

```json
{
  "name": "read_file",
  "description": "Read the contents of a file...",
  "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
}
```

- generateContent wraps it as `tools: [{functionDeclarations: [...]}]`; mode via `toolConfig.functionCallingConfig` (`AUTO` / `ANY` / `NONE` / `VALIDATED`).
- Interactions wraps it as `tools: [{type: "function", ...}]`; mode via `generation_config.tool_choice`. Built-ins (`google_search`, `url_context`, `code_execution`, ...) can mix in.

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

## Gemini API (native)

Request: `functionCall {name, args: object, id}` (generateContent part) / `function_call {name, arguments: object, id}` (Interactions step). Result: `role: "user", parts: [{functionResponse: {name, id, response: object}}]` / `{type: "function_result", name, call_id, result: [{type: "text", text}]}`. Match by `id`; resend model content/steps verbatim (signatures included) plus the result.

# Automatic Provider Detection

```rust
#[derive(Debug, PartialEq)]
enum LlmProvider {
    Anthropic,
    Ollama,
    OpenAi,
    OpenAiResponses,
    OpenAiCompatible,
    Google, // native Gemini (generateContent + Interactions share detection; branch by endpoint path)
}

fn detect_provider(url: &str) -> LlmProvider {
    let url_lower = url.to_lowercase();

    if url_lower.contains("api.anthropic.com") || url_lower.contains("/v1/messages") {
        return LlmProvider::Anthropic;
    }

    if url_lower.contains("/api/chat") || url_lower.contains(":11434") {
        return LlmProvider::Ollama;
    }

    if url_lower.contains("/v1/responses") {
        return LlmProvider::OpenAiResponses;
    }

    // Native Gemini (AI Studio + Vertex). Keep before the generic OpenAI fallthrough
    // because Gemini also ships an OpenAI-compat path that must NOT match here.
    if (url_lower.contains("generativelanguage.googleapis.com")
        && !url_lower.contains("/v1beta/openai/"))
        || url_lower.contains(":generatecontent")
        || url_lower.contains(":streamgeneratecontent")
        || url_lower.contains("/interactions")
        || url_lower.contains("-aiplatform.googleapis.com")
    {
        return LlmProvider::Google;
    }

    if url_lower.contains("api.openai.com") {
        return LlmProvider::OpenAi;
    }

    // Covers generic gateways AND Gemini/OpenCode-Go via OpenAI shape:
    // `.../v1beta/openai/...`, `https://opencode.ai/inference/go/...`.
    LlmProvider::OpenAiCompatible
}
```

Notes:
- `.../v1beta/openai/...` and `opencode.ai/inference/go/...` intentionally fall through to `OpenAi`/`OpenAiCompatible` (OpenAI Chat shape + `Authorization: Bearer` + `x-opencode-session` extra). Only direct `generativelanguage.googleapis.com` (non-`/openai/`) and Vertex `aiplatform.googleapis.com` become `Google`.

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

## Gemini API (native)
- Tool Call Arguments: **JSON Object** in both envelopes (`args` / `arguments`; Interactions streams them as `arguments_delta` chunks — accumulate, then parse).
- Structured Outputs: JSON text in `parts[].text` (generateContent: `responseMimeType: "application/json"` + `responseJsonSchema`) / `output_text` (Interactions: `response_format`). Concatenate stream chunks, then `JSON.parse()`.

## Tool Result Submission (API-Specific Rules)
- OpenAI & Anthropic: When returning execution results back to the model, the payload submitted within the content field must be formatted as a string containing JSON text (or plain text).
- Ollama (Native `/api/chat`): `content` accepts a plain string, including JSON text. The model can reliably consume stringified JSON.
- Gemini (generateContent): `functionResponse.response` is a **JSON object** (`{"output": ..., "error": ...}`, any keys you choose; multimodal via sibling `parts`). Do not stringify it into a string field.
- Gemini (Interactions): `function_result.result` is an **array of content blocks**, normally `[{type: "text", text: json_string}]`. Stringify your JSON into that inner `text` field (multimodal blocks allowed on Gemini 3+).

## Implementation

| API                      | Tool Call Arguments                   | Structured Outputs                                       | Tool Results                 |
| ------------------------ | ------------------------------------- | -------------------------------------------------------- | ---------------------------- |
| **OpenAI (Chat / Responses)** | JSON string -> `JSON.parse()` required | JSON text in `content` / `output_text` -> `JSON.parse()` required | Return JSON text as a string |
| **Ollama (`/api/chat`)** | JSON object -> no parsing required     | JSON text in `message.content` -> `JSON.parse()` required | Return JSON text as a string |
| **Anthropic**            | JSON object -> no parsing required     | JSON text in `content[].text` -> `JSON.parse()` required  | Return JSON text as a string |
| **Gemini (native)** | JSON object -> no parsing required (Interactions stream: accumulate `arguments_delta`) | JSON text in `parts[].text` / `output_text` -> `JSON.parse()` required | Object (`functionResponse.response`) / text in `function_result.result[].text` |

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

## Gemini (native; both envelopes)
- Thinking arrives with its signature: generateContent `parts[]` with `thought: true` + `thoughtSignature`; Interactions dedicated `thought` steps `{signature, summary[]}`. Resend everything verbatim (a missing/altered signature with tools is HTTP 400); never merge signed parts.
- Control: `thinkingLevel` (`minimal` / `low` / `medium` / `high` / `xhigh`, model-dependent) or `thinkingBudget` (2.5 only); display via `includeThoughts` / `thinking_summaries`. Token limits cover thoughts + output.

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

### Client-held vs API-held state

LLMs are stateless; only the history location differs. This app keeps history on the client (full `Session` resend every turn) and avoids API-held state (`previous_response_id` / `store` / `previous_interaction_id`), which gateways don't reliably support. A future native Gemini implementation does the same, including display-only `thought` signatures.
