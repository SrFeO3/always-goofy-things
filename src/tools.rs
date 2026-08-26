//! Tool implementations for the LLM assistant.
//!
//! Implements capabilities such as shell command execution, fuzzy text replacement,
//! and URL fetching.
//!
//! # Safety Warning
//!
//! The following tools perform direct operations on the local system
//! and network, such as file modification, command execution, and internet access.
//! Use only in a secure environment to prevent unintended data loss or security breaches.
//!
//! # Available Tools
//!
//! - `read_file`: Read a text or binary file's content. start/end select a
//!   1-based range (lines for text, pages for PDF).
//! - `write_file`: Create a new file or overwrite an existing one with full content.
//! - `str_replace_editor`: Replace specific text blocks in a file for code modification.
//! - `grep_search`: Search for text patterns across files in the workspace.
//! - `list_directory`: List the contents of a directory to explore the project structure.
//! - `execute_bash`: Run terminal commands to perform development tasks.
//! - `fetch_web`: Fetch and extract text content from a specified URL.

use std::fs;
#[cfg(not(feature = "gui"))]
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
#[cfg(feature = "gui")]
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use clap::ValueEnum;
use regex::Regex;
use reqwest::redirect;
use serde_json::json;
#[cfg(feature = "gui")]
use tokio::sync::{mpsc, oneshot};

use crate::file::{self, FileType};
use crate::reflex::auto_confirm;
#[cfg(not(feature = "gui"))]
use crate::startup::{C_CYAN, RESET};
use crate::tools_calc;
use crate::tools_data;
use crate::tools_fuzzy::{
    build_full_fuzzy_pattern, build_full_skip_blank_pattern, build_space_fuzzy_pattern,
    build_tab_fuzzy_pattern, build_tab_skip_blank_pattern,
};

pub const ALLOW_COMMAND_LIST: &[&str] = &[
    "^ls",
    "^cat",
    "^echo",
    "^grep",
    "^touch",
    "^which",
    "^head",
    "^tail",
    "^file",
    "^find",
    "^diff",
    "^rg",
    "^cargo build",
    "^cargo check",
    "^cargo clean",
    "^cargo fmt",
    "^cargo init",
    "^cargo test",
    "^cargo --version$",
    "^cargo version",
    "^cargo tree",
    "^cargo doc",
    "^rustdoc",
    "^rustc --version$",
    "^git status",
    "^git diff",
    "^git log",
    "^git show",
    "^git branch$",
];

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB

static ALLOW_COMMAND_LIST_RE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    ALLOW_COMMAND_LIST
        .iter()
        .map(|&p| Regex::new(p).unwrap())
        .collect()
});

static ABSOLUTE_PATH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|[\s=])/").unwrap());

static PATH_TRAVERSAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[\s=])\.\.($|[\s/])|/\.\.($|[\s/])").unwrap());

/// Canonicalized workspace root, registered once at startup (CLI / GUI).
static WORKSPACE_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn set_workspace_root(root: PathBuf) {
    let _ = WORKSPACE_ROOT.set(root);
}

pub(crate) fn workspace_root() -> &'static PathBuf {
    WORKSPACE_ROOT
        .get()
        .expect("workspace root not set: set_workspace_root must be called at startup")
}

/// Canonical names of all AI tools. Used by `--only-tools`, tool definition
/// filtering, and startup display so the list stays in one place.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "snake_case")]
pub enum ToolName {
    ListDirectory,
    ReadFile,
    WriteFile,
    StrReplaceEditor,
    GrepSearch,
    ExecuteBash,
    FetchWeb,
    DataSearch,
    DataSchema,
    Calc,
}

impl ToolName {
    /// The tool name as it appears in tool definitions and LLM calls.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolName::ListDirectory => "list_directory",
            ToolName::ReadFile => "read_file",
            ToolName::WriteFile => "write_file",
            ToolName::StrReplaceEditor => "str_replace_editor",
            ToolName::GrepSearch => "grep_search",
            ToolName::ExecuteBash => "execute_bash",
            ToolName::FetchWeb => "fetch_web",
            ToolName::DataSearch => "data_search",
            ToolName::DataSchema => "data_schema",
            ToolName::Calc => "calc",
        }
    }
}

/// Records how a tool execution was approved (or denied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRunDecisionKind {
    UserConfirm,
    UserCancel,
    AutoConfirm,
    SystemError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRunDecision {
    pub proceed: bool,
    pub kind: ToolRunDecisionKind,
    pub reason: Option<String>,
}

/// GUI: channel pair for tool interactions (user-confirm prompts and
/// auto-confirm notices). Initialised lazily on first access; no manual
/// setup required.
#[cfg(feature = "gui")]
pub(crate) static TOOL_INTERACT_CH: LazyLock<(
    mpsc::UnboundedSender<ToolInteractMsg>,
    Mutex<mpsc::UnboundedReceiver<ToolInteractMsg>>,
)> = LazyLock::new(|| {
    let (tx, rx) = mpsc::unbounded_channel();
    (tx, Mutex::new(rx))
});

/// Tool execution notice shared by both interaction variants.
#[cfg(feature = "gui")]
pub(crate) struct ToolNotice {
    pub name: String,
    pub args: serde_json::Value,
    pub reason: Option<String>,
}

/// A single tool interaction event flowing from core to GUI.
/// `Prompt` blocks until the user decides; `Notice` is fire-and-forget.
#[cfg(feature = "gui")]
pub(crate) enum ToolInteractMsg {
    Prompt {
        notice: ToolNotice,
        reply: oneshot::Sender<ToolRunDecision>,
    },
    Notice(ToolNotice),
}

/// Tool definitions sent to the LLM API.
/// Order matters: `compat::infer_tool_name_from_args` picks the first
/// highest-scoring match, so list more generic tools earlier.
/// Tools for which `is_enabled` returns false are omitted entirely, so the
/// LLM never learns about (and normally never calls) disabled tools.
pub fn get_tool_definitions(
    db_type: Option<&str>,
    is_enabled: impl Fn(&str) -> bool,
) -> Vec<serde_json::Value> {
    let mut tools = vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_directory",
                "description": "List files and directories in a given directory (non-recursive). Use this tool to explore the project structure before reading or editing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the workspace root. Do not start with '/' or '../'." },
                    },
                    "required": ["path"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file, including text, images, audio, and PDFs. start/end form a 1-based inclusive range: line numbers for text files, page numbers for PDF files. Omit start/end to read the whole file. Use this tool before editing files or investigating code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the workspace root. Do not start with '/' or '../'." },
                        "start": { "type": "integer", "description": "Optional 1-based range start: line number for text files, page number for PDF files." },
                        "end": { "type": "integer", "description": "Optional 1-based range end (inclusive): line number for text files, page number for PDF files." }
                    },
                    "required": ["path"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create a new file or completely replace an existing file. The content must represent the entire final file. Do not provide partial edits.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the workspace root. Do not start with '/' or '../'." },
                        "content": { "type": "string", "description": "The full content to write to the file." }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "str_replace_editor",
                "description": "Edit an existing file by replacing one exact string with another. Prefer this tool over rewriting entire files with write_file. The old_string must match the file contents exactly, including whitespace and newlines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the workspace root. Do not start with '/' or '../'." },
                        "old_string": { "type": "string", "description": "The exact string block to be replaced. Must match the target file content perfectly, including all whitespaces and newlines." },
                        "new_string": { "type": "string", "description": "The new string block to insert." }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "grep_search",
                "description": "Search for text patterns across files in the workspace. Use this tool to locate functions, classes, symbols, or error messages before reading or editing files. This does NOT search for filenames.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Text pattern to search for." },
                        "path": { "type": "string", "description": "Directory path relative to the workspace root. If omitted, searches the entire workspace." }
                    },
                    "required": ["query"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "execute_bash",
                "description": "Execute a shell command in a non-interactive bash environment. Use this tool to run tests, build projects, and execute development commands.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute. Examples: 'ls -la', 'git status', 'cargo build'." }
                    },
                    "required": ["command"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "fetch_web",
                "description": "Fetch the textual content of a web page and return it in an LLM-friendly format. Use this tool to read documentation, API references, articles, and other web resources.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The URL of the web page to fetch (http/https only)." }
                    },
                    "required": ["url"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "calc",
                "description": "Deterministic, side-effect-free expression evaluator for arithmetic, percentages, rates, byte/unit conversion, epoch (ns/s/ms) to UTC datetime conversion, decoding (base64/hex/url/json), and string normalization. Accepts a LIST of expressions in a single call and returns one result per expression in the same order. Use this for ALL computations and conversions: never compute or convert values by mental arithmetic or timezone guesswork, and never repeat a value from a tool result with modified numbers. When several calculations are needed, pass them all in this one call instead of calling calc repeatedly.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "expressions": {
                            "type": "array",
                            "description": "All expressions to evaluate in this batch, in the order the results should be returned. Pass every needed calculation in this single call (up to 50).",
                            "items": {
                                "type": "string",
                                "description": "One expression to evaluate. Operators: + - * / % with parentheses; numbers are IEEE 754 doubles (integer literals are kept exact). Functions: percent(part, whole), rate(count, seconds), round(value, digits), sum(...), avg(...), bytes_to_human(bytes), bytes_unit(bytes, unit), epoch_ns_to_utc(ns), epoch_s_to_utc(seconds), epoch_ms_to_utc(ms), utc_to_epoch(iso), duration_between(from, to), tz_convert(datetime, zone), base64_encode(s), base64_decode(s), hex_encode(s), hex_decode(s), url_encode(s), url_decode(s), json_get(json, path), normalize(s). Times are expressed as 'YYYY-MM-DD HH:MM:SS UTC' or with a fixed offset like '... +09:00' (tz_convert accepts UTC and fixed offsets only). Examples: \"1425 * 32\", \"(100 + 50) / 3\", \"percent(17121, 17267)\", \"round(percent(17121, 17267), 2)\", \"epoch_ns_to_utc(1472057364542756000)\", \"bytes_to_human(71176704)\", \"base64_decode('aHR0cHM6Ly8=')\". Each result element carries a calc_id; when quoting a computed value in artifacts or reports, cite its calc_id as [C-XXXX]."
                            }
                        }
                    },
                    "required": ["expressions"]
                }
            }
        }),
    ];

    // Conditionally append data tools when db_type is configured
    if let Some(dt) = db_type {
        if let Ok(def) = tools_data::build_data_search_def(dt) {
            tools.push(def);
        }
        tools.push(tools_data::build_data_schema_def());
    }

    // Hide disabled tools from the LLM.
    tools.retain(|def| {
        def.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .is_some_and(&is_enabled)
    });
    tools
}

pub async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    db_ctx: Option<&tools_data::DbContext>,
    calc_ledger: Option<&tools_calc::CalcLedger>,
    is_enabled: impl Fn(&str) -> bool,
) -> Result<serde_json::Value> {
    // Refuse disabled tools even if the LLM calls them anyway (defense in depth).
    if !is_enabled(name) {
        return Err(anyhow!(
            "[TOOL_DISABLED] Tool '{}' is disabled in this configuration.",
            name
        ));
    }

    // Path security check for tools that take 'path'
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        validate_path(path)?;
    }

    match name {
        "read_file" => execute_read_file(args),
        "write_file" => execute_write_file(args),
        "str_replace_editor" => execute_str_replace(args),
        "grep_search" => execute_grep_search(args).await,
        "list_directory" => execute_list_directory(args),
        "execute_bash" => execute_bash(args).await,
        "fetch_web" => execute_fetch_web(args).await,
        "data_search" => {
            let ctx = db_ctx.ok_or_else(|| {
                anyhow::anyhow!(
                    "[DB_CONFIG_ERROR] --db-url is required when --db-type is set. Provide the database HTTP endpoint URL."
                )
            })?;
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("[DB_INTERNAL_ERROR] Missing 'query' parameter."))?;
            tools_data::execute_data_search(ctx, query).await
        }
        "data_schema" => {
            let ctx = db_ctx.ok_or_else(|| {
                anyhow::anyhow!(
                    "[DB_CONFIG_ERROR] --db-url is required when --db-type is set. Provide the database HTTP endpoint URL."
                )
            })?;
            let table = args.get("table").and_then(|v| v.as_str());
            tools_data::execute_data_schema(ctx, table).await
        }
        "calc" => Ok(tools_calc::execute_calc(args, calc_ledger)),
        _ => Err(anyhow::anyhow!("[INVALID_TOOL] Unknown tool: {}", name)),
    }
}

/// Validate a path stays inside the workspace: lexical fast reject, then
/// canonicalize (resolving symlinks; parent for not-yet-existing paths) and
/// check against the workspace root.
fn validate_path(path: &str) -> Result<()> {
    let mut depth: i32 = 0;
    for component in std::path::Path::new(path).components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(anyhow!(
                    "[SECURITY_VIOLATION] Absolute paths are forbidden."
                ));
            }
            std::path::Component::ParentDir => {
                depth -= 1;
            }
            std::path::Component::Normal(_) => {
                depth += 1;
            }
            std::path::Component::CurDir => {}
        }
        if depth < 0 {
            return Err(anyhow!(
                "[SECURITY_VIOLATION] Directory traversal outside workspace is forbidden."
            ));
        }
    }

    let p = Path::new(path);
    let resolved = match fs::canonicalize(p) {
        Ok(r) => r,
        Err(_) => {
            let Some(parent) = p.parent().filter(|pp| !pp.as_os_str().is_empty()) else {
                return Ok(()); // bare filename, under CWD
            };
            match fs::canonicalize(parent) {
                Ok(parent_resolved) => parent_resolved.join(p.file_name().unwrap_or_default()),
                Err(_) => return Ok(()), // no existing ancestor: lexically safe
            }
        }
    };

    // Both sides are canonicalized, so on Windows the casing matches as well.
    if !resolved.starts_with(workspace_root()) {
        return Err(anyhow!(
            "[SECURITY_VIOLATION] Path resolves outside the workspace: '{}'",
            path
        ));
    }
    Ok(())
}

/// Confirms tool execution (via user interaction or auto-rules).
/// This function encapsulates the approval logic for tool execution.
/// Returns `ToolRunDecision` - whether to proceed and how it was decided.
/// I/O errors are handled internally and returned as `SystemError`.
pub async fn confirm_execute_tool(
    name: &str,
    args: &serde_json::Value,
    unsafe_reflex: bool,
    db_unsafe_reflex: bool,
    batch: bool,
    is_enabled: impl Fn(&str) -> bool,
) -> ToolRunDecision {
    // Disabled tools are rejected before any prompt or auto-confirm:
    // no user interaction, just a system error result fed back to the LLM.
    if !is_enabled(name) {
        return ToolRunDecision {
            proceed: false,
            kind: ToolRunDecisionKind::SystemError,
            reason: Some(format!(
                "[TOOL_DISABLED] Tool '{}' is disabled in this configuration.",
                name
            )),
        };
    }

    // Auto-confirm data tools when --db-unsafe-reflex is set.
    // Data tools (data_search, data_schema) are read-only queries against
    // external databases -- inherently safe to auto-execute.
    let is_data_tool = matches!(name, "data_search" | "data_schema");
    let effective_unsafe = unsafe_reflex || (is_data_tool && db_unsafe_reflex);

    if effective_unsafe
        && let (proceed, reason) = auto_confirm(name, args)
        && proceed
    {
        #[cfg(feature = "gui")]
        let _ = TOOL_INTERACT_CH.0.send(ToolInteractMsg::Notice(ToolNotice {
            name: name.to_string(),
            args: args.clone(),
            reason: reason.clone(),
        }));
        return ToolRunDecision {
            proceed: true,
            kind: ToolRunDecisionKind::AutoConfirm,
            reason,
        };
    }

    // In batch mode, deny any tool that wasn't auto-confirmed (no stdin available).
    if batch {
        return ToolRunDecision {
            proceed: false,
            kind: ToolRunDecisionKind::SystemError,
            reason: Some("Skipped: please try simpler and safer operations.".to_string()),
        };
    }

    // -- CLI: read from stdin --
    #[cfg(not(feature = "gui"))]
    {
        print!(
            "      {}Execute this tool ({})? (y/N) {}",
            C_CYAN, name, RESET
        );
        if io::stdout().flush().is_err() {
            return ToolRunDecision {
                proceed: false,
                kind: ToolRunDecisionKind::SystemError,
                reason: Some("Failed to flush stdout".to_string()),
            };
        }

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            return ToolRunDecision {
                proceed: false,
                kind: ToolRunDecisionKind::SystemError,
                reason: Some("Failed to read stdin".to_string()),
            };
        }

        if input.trim().eq_ignore_ascii_case("y") {
            ToolRunDecision {
                proceed: true,
                kind: ToolRunDecisionKind::UserConfirm,
                reason: None,
            }
        } else {
            ToolRunDecision {
                proceed: false,
                kind: ToolRunDecisionKind::UserCancel,
                reason: None,
            }
        }
    }

    // -- GUI: mpsc + oneshot to main thread --
    #[cfg(feature = "gui")]
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = TOOL_INTERACT_CH.0.send(ToolInteractMsg::Prompt {
            notice: ToolNotice {
                name: name.to_string(),
                args: args.clone(),
                reason: None,
            },
            reply: reply_tx,
        });
        reply_rx.await.unwrap_or(ToolRunDecision {
            proceed: false,
            kind: ToolRunDecisionKind::UserCancel,
            reason: None,
        })
    }
}

fn execute_read_file(args: &serde_json::Value) -> Result<serde_json::Value> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] path is required"))?;

    let file_type = file::classify_file(path);

    match file_type {
        FileType::Text => {
            let content = fs::read_to_string(path)
                .map_err(|e| anyhow!("[FILE_READ_FAILED] Could not read '{}': {}", path, e))?;
            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();

            // start/end are 1-based inclusive; invalid ranges are rejected
            // (never clamped) so the LLM can correct them instead of getting
            // a silently widened or empty slice.
            let start = args["start"].as_u64().map(|v| v as usize).unwrap_or(1);
            let end = args["end"]
                .as_u64()
                .map(|v| v as usize)
                .unwrap_or(total_lines);
            if start < 1 || start > end || start > total_lines || end > total_lines {
                return Err(anyhow!(
                    "[INVALID_ARGUMENTS] Invalid line range: start/end must satisfy 1 <= start <= end <= total (total lines: {})",
                    total_lines
                ));
            }

            let sliced_content = lines[(start - 1)..end].join("\n");
            let truncated = start > 1 || end < total_lines;

            Ok(json!({
                "path": path,
                "start": start,
                "end": end,
                "total": total_lines,
                "unit": "lines",
                "content": sliced_content,
                "truncated": truncated
            }))
        }
        FileType::Image { mime } => {
            reject_range(args)?;
            let data_url = file::convert_image_to_data_url(path)
                .map_err(|e| anyhow!("[FILE_READ_FAILED] {}", e))?;
            Ok(json!({
                "path": path,
                "content_type": "image",
                "mime": mime,
                "content": data_url
            }))
        }
        FileType::Audio { format } => {
            reject_range(args)?;
            let (_format, b64) = file::convert_audio_to_base64(path)
                .map_err(|e| anyhow!("[FILE_READ_FAILED] {}", e))?;
            let mime = audio_format_to_mime(&format);
            Ok(json!({
                "path": path,
                "content_type": "audio",
                "mime": mime,
                "content": b64
            }))
        }
        FileType::Document { mime } if mime == "application/pdf" => {
            let total = crate::file_pdf::pdf_page_count(path)
                .map_err(|e| anyhow!("[FILE_READ_FAILED] {}", e))?;
            let start = args["start"].as_u64().map(|v| v as usize).unwrap_or(1);
            let end = args["end"].as_u64().map(|v| v as usize).unwrap_or(total);
            if start < 1 || start > end || start > total || end > total {
                return Err(anyhow!(
                    "[INVALID_ARGUMENTS] Invalid page range: start/end must satisfy 1 <= start <= end <= total (total pages: {})",
                    total
                ));
            }
            let result = crate::file_pdf::extract_text_from_pdf(path, Some((start, end)))
                .map_err(|e| anyhow!("[FILE_READ_FAILED] {}", e))?;
            Ok(json!({
                "path": path,
                "start": result.start,
                "end": result.end,
                "total": result.page_count,
                "unit": "pages",
                "content": result.text,
                "truncated": result.start > 1 || result.end < result.page_count
            }))
        }
        FileType::Document { mime } => {
            reject_range(args)?;
            let bytes = fs::read(path)
                .map_err(|e| anyhow!("[FILE_READ_FAILED] Could not read '{}': {}", path, e))?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(json!({
                "path": path,
                "content_type": "pdf",
                "mime": mime,
                "content": b64
            }))
        }
    }
}

/// Reject start/end for file types without a range concept: images, audio,
/// and non-PDF documents are always read in full.
fn reject_range(args: &serde_json::Value) -> Result<(), anyhow::Error> {
    if args.get("start").is_some() || args.get("end").is_some() {
        return Err(anyhow!(
            "[INVALID_ARGUMENTS] start/end only apply to text and PDF files; this file is read in full"
        ));
    }
    Ok(())
}

fn audio_format_to_mime(format: &str) -> &str {
    match format {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn execute_write_file(args: &serde_json::Value) -> Result<serde_json::Value> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] path is required"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] content is required"))?;

    validate_path(path)?;

    if content.len() as u64 > MAX_FILE_SIZE {
        return Err(anyhow!("[FILE_TOO_LARGE] File content exceeds 10MB limit"));
    }
    let bytes = atomic_write_with_dir(path, content)
        .map_err(|e| anyhow!("[FILE_WRITE_FAILED] '{}': {}", path, e))?;

    Ok(json!({
        "path": path,
        "bytes_written": bytes
    }))
}

fn atomic_write_with_dir(path: &str, content: &str) -> Result<usize> {
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = format!(
        "{}.tmp.{}",
        path,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    if let Err(e) = fs::write(&tmp_path, content) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    Ok(content.len())
}

/// Build a concise mismatch report showing why fuzzy match was needed.
/// For whitespace-only diffs, reports per-line indentation shortages/excesses.
fn build_fuzzy_mismatch_report(provided: &str, actual: &str) -> serde_json::Value {
    // --- Whitespace-only diff: analyze per-line indentation & spacing ---
    let p_lines: Vec<&str> = provided.lines().collect();
    let a_lines: Vec<&str> = actual.lines().collect();

    let mut line_issues: Vec<serde_json::Value> = Vec::new();
    let max_lines = p_lines.len().max(a_lines.len());

    for i in 0..max_lines {
        let p_line = p_lines.get(i).unwrap_or(&"");
        let a_line = a_lines.get(i).unwrap_or(&"");

        if p_line == a_line {
            continue; // identical line, skip
        }

        let line_num = i + 1;

        // Compare leading whitespace (indentation)
        let p_leading = p_line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .count();
        let a_leading = a_line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .count();

        // Compare trailing whitespace
        let p_trailing = p_line
            .chars()
            .rev()
            .take_while(|c| *c == ' ' || *c == '\t')
            .count();
        let a_trailing = a_line
            .chars()
            .rev()
            .take_while(|c| *c == ' ' || *c == '\t')
            .count();

        // Compare internal spacing: count whitespace runs between non-whitespace tokens
        // Strip leading/trailing first, then split to get only internal gaps
        let p_trimmed = p_line.trim_matches(|c| c == ' ' || c == '\t');
        let a_trimmed = a_line.trim_matches(|c| c == ' ' || c == '\t');
        let p_internal_ws: Vec<usize> = p_trimmed
            .split(|c: char| !c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().count())
            .collect();
        let a_internal_ws: Vec<usize> = a_trimmed
            .split(|c: char| !c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().count())
            .collect();
        let mut line_map: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

        if p_leading != a_leading {
            line_map.insert(
                format!("{}", line_num),
                json!({ "expected_lead": a_leading, "your_lead": p_leading }),
            );
        }

        if p_trailing != a_trailing {
            let entry = line_map.entry(format!("{}", line_num)).or_insert(json!({}));
            entry
                .as_object_mut()
                .unwrap()
                .insert("expected_trail".to_string(), json!(a_trailing));
            entry
                .as_object_mut()
                .unwrap()
                .insert("your_trail".to_string(), json!(p_trailing));
        }

        if p_internal_ws != a_internal_ws {
            // Report each differing internal gap
            let min_w = p_internal_ws.len().min(a_internal_ws.len());
            for j in 0..min_w {
                if p_internal_ws[j] != a_internal_ws[j] {
                    let gap_key = format!("internal_gap_{}", j + 1);
                    let entry = line_map.entry(format!("{}", line_num)).or_insert(json!({}));
                    entry
                        .as_object_mut()
                        .unwrap()
                        .insert(format!("expected_{}", gap_key), json!(a_internal_ws[j]));
                    entry
                        .as_object_mut()
                        .unwrap()
                        .insert(format!("your_{}", gap_key), json!(p_internal_ws[j]));
                }
            }
            if p_internal_ws.len() != a_internal_ws.len() {
                let entry = line_map.entry(format!("{}", line_num)).or_insert(json!({}));
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("expected_gap_count".to_string(), json!(a_internal_ws.len()));
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("your_gap_count".to_string(), json!(p_internal_ws.len()));
            }
        }

        if line_map.is_empty() {
            // Lines differ only in newline style or tabs-vs-spaces
            line_map.insert(
                format!("{}", line_num),
                json!({ "note": "unspecified whitespace difference" }),
            );
        }

        for (line_key, data) in line_map {
            line_issues.push(json!({
                "line": line_key,
                "numerical_diff": data,
            }));
        }
    }

    // Detect extra/missing lines (newline mismatch)
    let p_line_count = p_lines.len();
    let a_line_count = a_lines.len();
    let mut newline_issues: Vec<String> = Vec::new();
    if p_line_count != a_line_count {
        let diff = a_line_count as i32 - p_line_count as i32;
        if diff > 0 {
            newline_issues.push(format!(
                "missing {} line(s): provided {} line(s) but file has {} line(s)",
                diff, p_line_count, a_line_count
            ));
        } else {
            newline_issues.push(format!(
                "extra {} line(s): provided {} line(s) but file has {} line(s)",
                -diff, p_line_count, a_line_count
            ));
        }
    } // Build combined issues     // Flatten all issues into a single array
    let mut all_issues: Vec<serde_json::Value> = Vec::new();
    all_issues.extend(line_issues);
    for issue in &newline_issues {
        all_issues.push(json!({
            "line": "extra_line",
            "issues": [issue],
        }));
    }

    json!({
         "kind": "invalid_whitespace_or_indentation",
         "total_lines_compared": max_lines,
         "line_issues": all_issues,
         "hint": "CRITICAL: Fuzzy match applied due to wrong spaces/indentation. Next time, you MUST run `read_file` first and copy the target lines character-for-character to ensure a 100% exact match.",
    })
}

fn execute_str_replace(args: &serde_json::Value) -> Result<serde_json::Value> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] path is required"))?;
    let old_str = args["old_string"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] old_string is required"))?;
    let new_str = args["new_string"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] new_string is required"))?;

    validate_path(path)?;

    let metadata = fs::metadata(path)
        .map_err(|e| anyhow!("[FILE_READ_FAILED] Could not stat '{}': {}", path, e))?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(anyhow!("[FILE_TOO_LARGE] File exceeds 10MB limit"));
    }

    let content = fs::read_to_string(path)
        .map_err(|e| anyhow!("[FILE_READ_FAILED] Could not read '{}': {}", path, e))?;

    // --- Step 1: Try exact match first (single occurrence) ---
    if content.matches(old_str).count() == 1 {
        let new_content = content.replace(old_str, new_str);
        atomic_write_with_dir(path, &new_content)
            .map_err(|e| anyhow!("[FILE_WRITE_FAILED] '{}': {}", path, e))?;
        return Ok(json!({
            "path": path,
            "occurrences_replaced": 1,
            "match_type": "Perfect match."
        }));
    }

    // --- Step 2: Space-fuzzy match (horizontal whitespace only, no tabs/newlines) ---
    let space_fuzzy_pattern = build_space_fuzzy_pattern(old_str);
    if let Ok(re) = Regex::new(&space_fuzzy_pattern) {
        match try_fuzzy_replace(&content, &re, old_str, new_str, path, "space_fuzzy_match") {
            Ok(res) => return Ok(res),
            Err(e) if e.to_string().contains("AMBIGUOUS_MATCH") => return Err(e),
            _ => {} // Continue to next stage if NO_MATCH
        }
    }

    // --- Step 3: Tab-fuzzy match (horizontal whitespace: spaces + tabs) ---
    let tab_fuzzy_pattern = build_tab_fuzzy_pattern(old_str);
    if let Ok(re) = Regex::new(&tab_fuzzy_pattern) {
        match try_fuzzy_replace(&content, &re, old_str, new_str, path, "tab_fuzzy_match") {
            Ok(res) => return Ok(res),
            Err(e) if e.to_string().contains("AMBIGUOUS_MATCH") => return Err(e),
            _ => {} // Continue to next stage if NO_MATCH
        }
    }

    // --- Step 3.5: Tab-fuzzy + blank-line tolerant (space/tab-only blank lines ignored) ---
    let tab_skip_blank_pattern = build_tab_skip_blank_pattern(old_str);
    if !tab_skip_blank_pattern.is_empty()
        && let Ok(re) = Regex::new(&tab_skip_blank_pattern)
    {
        match try_fuzzy_replace(
            &content,
            &re,
            old_str,
            new_str,
            path,
            "tab_skip_blank_match",
        ) {
            Ok(res) => return Ok(res),
            Err(e) if e.to_string().contains("AMBIGUOUS_MATCH") => return Err(e),
            _ => {}
        }
    }

    // --- Step 4: Full fuzzy match (all whitespace incl. line breaks + \r\n / \n differences) ---
    let full_pattern = build_full_fuzzy_pattern(old_str);
    if let Ok(re) = Regex::new(&full_pattern) {
        match try_fuzzy_replace(&content, &re, old_str, new_str, path, "full_fuzzy_match") {
            Ok(res) => return Ok(res),
            Err(e) if e.to_string().contains("AMBIGUOUS_MATCH") => return Err(e),
            _ => {} // Final stage, let it fall through to the final NO_MATCH error
        }
    }

    // --- Step 4.5: Full-fuzzy + blank-line tolerant (space/tab-only blank lines ignored) ---
    let full_skip_blank_pattern = build_full_skip_blank_pattern(old_str);
    if !full_skip_blank_pattern.is_empty()
        && let Ok(re) = Regex::new(&full_skip_blank_pattern)
    {
        match try_fuzzy_replace(
            &content,
            &re,
            old_str,
            new_str,
            path,
            "full_skip_blank_match",
        ) {
            Ok(res) => return Ok(res),
            Err(e) if e.to_string().contains("AMBIGUOUS_MATCH") => return Err(e),
            _ => {}
        }
    }

    // All fallbacks were exhausted - old_string truly isn't in file.
    Err(anyhow!(
        "[NO_MATCH] old_string not found in '{}' after trying exact / space-fuzzy / tab-fuzzy / tab-blank-skip / full-fuzzy / full-blank-skip stages",
        path,
    ))
}

/// Shared helper for fuzzy replace attempts (used by tab_fuzzy and full_fuzzy steps).
fn try_fuzzy_replace(
    content: &str,
    re: &Regex,
    old_str: &str,
    new_str: &str,
    path: &str,
    match_type: &str,
) -> Result<serde_json::Value> {
    let matches: Vec<_> = re.find_iter(content).collect();

    if matches.is_empty() {
        return Err(anyhow!("[NO_MATCH] old_string not found in '{}'", path));
    }
    if matches.len() > 1 {
        return Err(anyhow!(
            "[AMBIGUOUS_MATCH] Multiple matches found ({}). Be more specific.",
            matches.len()
        ));
    }
    let actual_matched = matches[0].as_str();
    let new_content = re
        .replace(content, |_caps: &regex::Captures| new_str.to_string())
        .to_string();
    atomic_write_with_dir(path, &new_content)
        .map_err(|e| anyhow!("[FILE_WRITE_FAILED] '{}': {}", path, e))?;

    // Build a mismatch report
    let mismatch_report = build_fuzzy_mismatch_report(old_str, actual_matched);

    let match_result = match match_type {
        "space_fuzzy_match" => "Space count mismatch: matched by allowing flexible space runs.",
        "tab_fuzzy_match" => {
            "Tab/Space mismatch: matched by treating tabs and spaces as equivalent."
        }
        "tab_skip_blank_match" => {
            "Tab/Space mismatch (blank-line tolerant): blank lines ignored for matching."
        }
        "full_fuzzy_match" => {
            "Line break/Structure mismatch: matched by ignoring all whitespace and newlines."
        }
        "full_skip_blank_match" => {
            "Full fuzzy (blank-line tolerant): whitespace flexible, blank lines ignored."
        }
        _ => "Fuzzy match applied.",
    };

    Ok(json!({
         "path": path,
         "occurrences_replaced": 1,
         "match_type": match_result,
         "fuzzy_match_detail": mismatch_report
    }))
}

fn execute_list_directory(args: &serde_json::Value) -> Result<serde_json::Value> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] path is required"))?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|e| {
        anyhow!(
            "[DIRECTORY_READ_FAILED] Could not read directory '{}': {}",
            path,
            e
        )
    })? {
        let entry = entry.map_err(|e| {
            anyhow!(
                "[DIRECTORY_READ_FAILED] Error reading entry in '{}': {}",
                path,
                e
            )
        })?;
        let file_type = entry
            .file_type()
            .map_err(|e| anyhow!("[DIRECTORY_READ_FAILED] Error getting file type: {}", e))?;
        let metadata = entry
            .metadata()
            .map_err(|e| anyhow!("[DIRECTORY_READ_FAILED] Error getting metadata: {}", e))?;
        entries.push(json!({
           "name": entry.file_name().to_string_lossy(),
           "type": if file_type.is_dir() { "directory" } else { "file" },
           "size": metadata.len()
        }));
    }
    Ok(json!({ "path": path, "entries": entries }))
}

async fn execute_grep_search(args: &serde_json::Value) -> Result<serde_json::Value> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] query is required"))?;
    let search_path = args["path"].as_str().unwrap_or(".");

    let output =
        crate::tools_process::run_captured("grep", &["-rnE", query, search_path], "GREP").await?;
    // Exit code 1 means "no matches" (not an error); output is parsed below.

    let mut matches = Vec::new();

    for line in output.stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() == 3
            && let Ok(line_num) = parts[1].parse::<usize>()
        {
            matches.push(json!({
                "path": parts[0],
                "line": line_num,
                "text": parts[2]
            }));
        }
    }

    let total_matches = matches.len();
    let mut result = serde_json::Map::new();
    result.insert("matches".to_string(), json!(matches));
    result.insert("total_matches".to_string(), json!(total_matches));
    result.insert("truncated".to_string(), json!(output.stdout_truncated));
    if output.stdout_truncated {
        // Exact byte count discarded by the cap (lower bound on drain abort).
        result.insert(
            "output_bytes_omitted".to_string(),
            json!(output.stdout_omitted),
        );
    }
    Ok(json!(result))
}

async fn execute_bash(args: &serde_json::Value) -> Result<serde_json::Value> {
    let command = args["command"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] command is required"))?;
    let cmd_trim = command.trim();

    // Whitelist verification using pre-compiled regexes
    let is_allowed = ALLOW_COMMAND_LIST_RE.iter().any(|re| re.is_match(cmd_trim));

    if !is_allowed {
        return Err(anyhow!(
            "[BASH_NOT_WHITELISTED] Command not in whitelist: {}",
            cmd_trim
        ));
    }

    // Robust check for absolute paths and directory traversal
    if ABSOLUTE_PATH_RE.is_match(cmd_trim) || PATH_TRAVERSAL_RE.is_match(cmd_trim) {
        return Err(anyhow!(
            "[SECURITY_VIOLATION] Absolute paths or directory traversal detected in bash command."
        ));
    }

    // Basic check for interactive commands
    if ["nano", "vim", "top", "ssh"]
        .iter()
        .any(|&c| cmd_trim.contains(c))
    {
        return Err(anyhow!(
            "[BASH_INTERACTIVE] Interactive commands are not allowed."
        ));
    }

    let output = crate::tools_process::run_captured("bash", &["-c", cmd_trim], "BASH").await?;
    let stdout_raw = output.stdout;
    let stderr_raw = output.stderr;
    let exit_code = output.exit_code;
    let signal = output.signal;

    // Optimized Truncation: Keep the end of output as per spec. When the
    // capture layer cut the stream (cap overflow, or a background grandchild
    // held the pipe open) but the visible tail is short, say so instead of
    // looking like a clean EOF.
    let stdout = if stdout_raw.len() > 4096 {
        format!(
            "[... Output truncated ...]\n{}",
            &stdout_raw[stdout_raw.len() - 4000..]
        )
    } else if output.stdout_truncated {
        format!("[... Output truncated ...]\n{}", stdout_raw)
    } else {
        stdout_raw
    };
    let stderr = if stderr_raw.len() > 4096 {
        format!(
            "[... stderr truncated ...]\n{}",
            &stderr_raw[stderr_raw.len() - 4000..]
        )
    } else if output.stderr_truncated {
        format!("[... stderr truncated ...]\n{}", stderr_raw)
    } else {
        stderr_raw
    };

    Ok(json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
        "signal": signal,
    }))
}

async fn execute_fetch_web(args: &serde_json::Value) -> Result<serde_json::Value> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] url is required"))?;
    validate_url_deep(url).await?;

    // Re-validate every redirect hop instead of following blindly.
    // Policy::none() is not used because docs.rs crate links redirect to
    // the latest version (https://docs.rs/<crate> -> /<crate>/latest/<crate>/).
    let redirect_policy = redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.stop(); // hop cap
        }
        let target = attempt.url();
        if let Some(prev) = attempt.previous().last()
            && prev.scheme() == "https"
            && target.scheme() == "http"
        {
            return attempt.stop(); // no https -> http downgrade
        }
        match validate_url_sync(target) {
            Ok(()) => attempt.follow(),
            Err(_) => attempt.stop(),
        }
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(redirect_policy)
        .build()
        .map_err(|e| {
            anyhow!(
                "[NETWORK_REQUEST_FAILED] Failed to build HTTP client: {}",
                e
            )
        })?;

    let res = client.get(url).send().await.map_err(|e| {
        anyhow!(
            "[NETWORK_REQUEST_FAILED] Failed to send request to '{}': {}",
            url,
            e
        )
    })?;
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .to_string();
    let body = res.text().await.map_err(|e| {
        anyhow!(
            "[NETWORK_REQUEST_FAILED] Failed to read response from '{}': {}",
            url,
            e
        )
    })?;

    let clean_text = strip_html_tags(&body);
    let truncated_content = if clean_text.len() > 20480 {
        format!("{}... [Output truncated]", &clean_text[..20000])
    } else {
        clean_text
    };

    Ok(json!({
        "url": url,
        "title": "Web Page Content", // Placeholder as full HTML parsing is heavy
        "content": truncated_content,
        "content_type": content_type
    }))
}

/// Scheme and host-level SSRF checks without DNS. Used at entry and for each
/// redirect hop.
fn validate_url_sync(url: &reqwest::Url) -> Result<()> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(anyhow!(
            "[INVALID_URL] Invalid scheme '{}'. Only http/https allowed.",
            url.scheme()
        ));
    }
    // Use the typed host (url::Host), not host_str(): the url crate stores
    // IPv6 hosts in bracketed canonical form ("[::ffff:7f00:1]"), which
    // IpAddr cannot parse and which is_ip_literal_form misses.
    let host = url
        .host()
        .ok_or_else(|| anyhow!("[INVALID_URL] Missing host: {}", url))?;
    match host {
        url::Host::Ipv4(v4) => reject_blocked_ip(&IpAddr::V4(v4))?,
        url::Host::Ipv6(v6) => reject_blocked_ip(&IpAddr::V6(v6))?,
        url::Host::Domain(domain) => {
            // Decimal / octal / hex / short IP notations that IpAddr cannot parse
            // (e.g. 2130706433, 0177.0.0.1, 0x7f.0.0.1, 127.1) all resolve to
            // loopback or private addresses; reject them outright.
            if is_ip_literal_form(domain) {
                return Err(anyhow!(
                    "[SECURITY_VIOLATION] IP literal host is not allowed: {}",
                    domain
                ));
            }
        }
    }
    Ok(())
}

/// validate_url_sync plus a DNS pre-resolution check (DNS rebinding mitigation;
/// a full fix requires a network namespace / proxy).
async fn validate_url_deep(url: &str) -> Result<()> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| anyhow!("[INVALID_URL] Malformed URL: {}", url))?;
    validate_url_sync(&parsed)?;

    // Literal hosts (IPv4, IPv6, and mapped IPv6) were fully checked by
    // validate_url_sync; only domain names need a DNS pre-resolution check.
    let url::Host::Domain(domain) = parsed
        .host()
        .ok_or_else(|| anyhow!("[INVALID_URL] Missing host: {}", url))?
    else {
        return Ok(());
    };
    let port = parsed
        .port()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    let addrs = tokio::net::lookup_host((domain, port)).await.map_err(|e| {
        anyhow!(
            "[NETWORK_REQUEST_FAILED] DNS resolution failed for '{}': {}",
            domain,
            e
        )
    })?;
    for addr in addrs {
        if is_blocked_ip(&addr.ip()) {
            return Err(anyhow!(
                "[SECURITY_VIOLATION] '{}' resolves to a private/local address.",
                domain
            ));
        }
    }
    Ok(())
}

fn reject_blocked_ip(ip: &IpAddr) -> Result<()> {
    if is_blocked_ip(ip) {
        return Err(anyhow!(
            "[SECURITY_VIOLATION] Access to private/local network is forbidden."
        ));
    }
    Ok(())
}

fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped IPv6 (::ffff:127.0.0.1 etc.) must be checked as IPv4:
            // it is neither loopback nor ULA in v6 terms but connects to the
            // mapped IPv4 address.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified();
            }
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// True when the host looks like an IP literal in an unusual notation that
/// `IpAddr` cannot parse (decimal / octal / hex / short form). Note that this
/// also matches short all-hex hostnames (rare, and rejected by design).
fn is_ip_literal_form(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == 'x' || c == 'X')
        })
}

fn strip_html_tags(html: &str) -> String {
    // 1. Remove non-content blocks entirely
    let html = Regex::new(r"(?is)<script.*?>.*?</script>")
        .unwrap()
        .replace_all(html, "");
    let html = Regex::new(r"(?is)<style.*?>.*?</style>")
        .unwrap()
        .replace_all(&html, "");
    let html = Regex::new(r"(?is)<head.*?>.*?</head>")
        .unwrap()
        .replace_all(&html, "");
    let html = Regex::new(r"(?is)<nav.*?>.*?</nav>")
        .unwrap()
        .replace_all(&html, "");
    let html = Regex::new(r"(?is)<footer.*?>.*?</footer>")
        .unwrap()
        .replace_all(&html, "");

    // 2. Convert links to Markdown: [text](url)
    // Using a simple regex to capture href and inner text
    let html = Regex::new(r#"(?i)<a\s+[^>]*href=["']([^"']*)["'][^>]*>(.*?)</a>"#)
        .unwrap()
        .replace_all(&html, "[$2]($1)");

    // 3. Convert structural blocks to newlines to preserve readability
    let html = Regex::new(r"(?i)<(p|div|br|h[1-6]|li|tr)[^>]*>")
        .unwrap()
        .replace_all(&html, "\n");

    // 4. Strip all remaining tags
    let text = Regex::new(r"<[^>]*>").unwrap().replace_all(&html, "");

    // 5. Decode basic entities and normalize whitespace
    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    let space_re = Regex::new(r"\n\s*\n+").unwrap();
    let text = space_re.replace_all(&text, "\n\n");

    text.trim().to_string()
}

#[cfg(test)]
#[path = "tests/tools_test.rs"]
mod tests;
