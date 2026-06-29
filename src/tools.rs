//! Implementation of executable tool capabilities for the LLM assistant.
//! Provides bash execution, fuzzy string replacement, and URL fetching.
//!
//! WARNING: The following tools perform direct operations on the local system
//! and network, such as file modification and command execution. Use only in
//! a secure environment to prevent unintended data loss or security breaches.
//!
//! Available Tools:
//! `read_file`: Read a file's content, optionally within a specific line range.
//! `write_file`: Create a new file or overwrite an existing one with full content.
//! `str_replace_editor`: Replace specific text blocks in a file for code modification.
//! `grep_search`: Search for text patterns across files in the workspace.
//! `list_directory`: List the contents of a directory to explore the project structure.
//! `execute_bash`: Run terminal commands to perform development tasks.
//! `fetch_web`: Fetch and extract text content from a specified URL.

use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Result, anyhow};
use regex::Regex;
use serde_json::json;
use tokio::process::Command as TokioCommand;

use super::reflex::auto_confirm;
use super::startup::{C_CYAN, RESET};

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

pub fn get_tool_definitions() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the contents of a file. Optionally specify a line range to read only part of the file. Use this tool before editing files or investigating code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the workspace root. Do not start with '/' or '../'." },
                        "start_line": { "type": "integer", "description": "Optional starting line number (1-based)." },
                        "end_line": { "type": "integer", "description": "Optional ending line number (inclusive)." }
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
    ]
}

pub async fn execute_tool(name: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
    // Path security check for tools that take 'path'
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        validate_path(path)?;
    }

    match name {
        "read_file" => execute_read_file(&args),
        "write_file" => execute_write_file(&args),
        "str_replace_editor" => execute_str_replace(&args),
        "grep_search" => execute_grep_search(&args),
        "list_directory" => execute_list_directory(&args),
        "execute_bash" => execute_bash(&args).await,
        "fetch_web" => execute_fetch_web(&args).await,
        _ => Err(anyhow!("[INVALID_TOOL] Unknown tool: {}", name)),
    }
}

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
    Ok(())
}

/// Confirms tool execution (via user interaction or auto-rules) and executes it.
/// This function encapsulates the approval logic for tool execution.
pub async fn confirm_and_execute_tool(
    name: &str,
    args: &serde_json::Value,
    unsafe_reflex: bool,
) -> Result<serde_json::Value> {
    let confirmed = if unsafe_reflex && auto_confirm(name, args).await {
        true
    } else {
        print!(
            "   {}Execute this tool ({})? (y/N): {}",
            C_CYAN, name, RESET
        );
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        input.trim().eq_ignore_ascii_case("y")
    };

    if confirmed {
        execute_tool(name, args).await
    } else {
        Ok(json!({"status": "denied", "message": "Tool execution skipped by user."}))
    }
}

fn execute_read_file(args: &serde_json::Value) -> Result<serde_json::Value> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] path is required"))?;
    let content = fs::read_to_string(path)
        .map_err(|e| anyhow!("[FILE_READ_FAILED] Could not read '{}': {}", path, e))?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let start = args["start_line"]
        .as_u64()
        .map(|v| (v as usize).saturating_sub(1))
        .unwrap_or(0);
    let end = args["end_line"]
        .as_u64()
        .map(|v| (v as usize).min(total_lines))
        .unwrap_or(total_lines);
    if start > end || start >= total_lines {
        return Err(anyhow!(
            "[INVALID_ARGUMENTS] Invalid line range (total lines: {})",
            total_lines
        ));
    }

    let sliced_content = lines[start..end].join("\n");

    let truncated = start > 0 || end < total_lines;

    Ok(json!({
        "path": path,
        "start_line": start + 1,
        "end_line": end,
        "total_lines": total_lines,
        "content": sliced_content,
        "truncated": truncated
    }))
}

fn execute_write_file(args: &serde_json::Value) -> Result<serde_json::Value> {
    let path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] path is required"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] content is required"))?;

    if let Err(e) = validate_path(path) {
        return Err(anyhow!("[OUTSIDE_WORKSPACE] {}", e));
    }

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
                "diff": data,
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
         "kind": "whitespace_only",
         "total_lines_compared": max_lines,
         "line_issues": all_issues,
         "hint": "Use read_file to see the exact whitespace. Match indentation, internal spaces, and trailing spaces precisely.",
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

    if let Err(e) = validate_path(path) {
        return Err(anyhow!("[OUTSIDE_WORKSPACE] {}", e));
    }

    let metadata = fs::metadata(path)
        .map_err(|e| anyhow!("[FILE_READ_FAILED] Could not stat '{}': {}", path, e))?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(anyhow!("[FILE_TOO_LARGE] File exceeds 10MB limit"));
    }

    let content = fs::read_to_string(path)
        .map_err(|e| anyhow!("[FILE_READ_FAILED] Could not read '{}': {}", path, e))?;

    // Try exact match first
    if content.matches(old_str).count() == 1 {
        let new_content = content.replace(old_str, new_str);
        atomic_write_with_dir(path, &new_content)
            .map_err(|e| anyhow!("[FILE_WRITE_FAILED] '{}': {}", path, e))?;
        return Ok(json!({
            "path": path,
            "occurrences_replaced": 1,
            "match_type": "exact"
        }));
    }

    // Fallback to fuzzy match by normalizing whitespace
    let escaped = regex::escape(old_str).replace(r" ", r"\s*");
    let re = Regex::new(&escaped)
        .map_err(|e| anyhow!("[INVALID_ARGUMENTS] Invalid regex pattern: {}", e))?;

    let matches: Vec<_> = re.find_iter(&content).collect();

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
        .replace(&content, |_caps: &regex::Captures| new_str.to_string())
        .to_string();
    atomic_write_with_dir(path, &new_content)
        .map_err(|e| anyhow!("[FILE_WRITE_FAILED] '{}': {}", path, e))?;

    // Build a short mismatch report for the LLM to learn from
    let mismatch_report = build_fuzzy_mismatch_report(old_str, actual_matched);

    Ok(json!({
        "path": path,
        "occurrences_replaced": 1,
        "match_type": "fuzzy",
        "fuzzy_mismatch": mismatch_report
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

fn execute_grep_search(args: &serde_json::Value) -> Result<serde_json::Value> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] query is required"))?;
    let search_path = args["path"].as_str().unwrap_or(".");
    let output = std::process::Command::new("grep")
        .arg("-rnE")
        .arg(query)
        .arg(search_path)
        .output()
        .map_err(|e| anyhow!("[GREP_EXECUTION_FAILED] grep command failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches = Vec::new();

    for line in stdout.lines() {
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

    Ok(json!({
        "matches": matches,
        "total_matches": matches.len(),
        "truncated": false
    }))
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

    let cmd_process = TokioCommand::new("bash").arg("-c").arg(cmd_trim).output();

    let output = match tokio::time::timeout(Duration::from_secs(30), cmd_process).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(anyhow!(
                "[BASH_EXECUTION_FAILED] Bash execution error: {}",
                e
            ));
        }
        Err(_) => {
            return Err(anyhow!(
                "[BASH_TIMED_OUT] Command timed out after 30 seconds."
            ));
        }
    };

    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(1);

    // Optimized Truncation: Keep the end of output as per spec
    let stdout = if stdout_raw.len() > 4096 {
        format!(
            "[... Output truncated ...]\n{}",
            &stdout_raw[stdout_raw.len() - 4000..]
        )
    } else {
        stdout_raw
    };

    Ok(json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code
    }))
}

async fn execute_fetch_web(args: &serde_json::Value) -> Result<serde_json::Value> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow!("[MISSING_PARAMETER] url is required"))?;
    validate_url(url)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
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

fn validate_url(url: &str) -> Result<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(anyhow!(
            "[INVALID_URL] Invalid scheme. Only http/https allowed."
        ));
    }
    let host_port = url.split('/').nth(2).unwrap_or("");
    let host = host_port.split(':').next().unwrap_or("");

    if host.to_lowercase() == "localhost" {
        return Err(anyhow!(
            "[SECURITY_VIOLATION] Access to localhost is forbidden."
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        let is_private = match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00, // Unique Local Address (fc00::/7)
        };
        if is_private {
            return Err(anyhow!(
                "[SECURITY_VIOLATION] Access to private network is forbidden."
            ));
        }
    }
    Ok(())
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
mod tests {
    use super::*;
    use std::fs;

    // Helper to generate a unique temporary path for testing (relative path)
    fn get_temp_path(name: &str) -> std::path::PathBuf {
        fs::create_dir_all("./tmp").ok();
        let mut path = std::path::Path::new("./tmp").to_path_buf();
        path.push(format!(
            "agt_test_{}_{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    #[test]
    fn test_read_file() {
        let path = get_temp_path("read");
        fs::write(&path, "line1\nline2\nline3\nline4").unwrap();
        let path_str = path.to_str().unwrap();

        // Test full file read
        let args = json!({
            "path": path_str,
        });
        let val = execute_read_file(&args).unwrap();
        assert_eq!(val["total_lines"], 4);
        assert!(val["content"].as_str().unwrap().contains("line4"));

        // Test specific line range read (lines 2-3)
        let args = json!({
            "path": path_str,
            "start_line":Some(2),
            "end_line":Some(3)
        });
        let val = execute_read_file(&args).unwrap();
        assert_eq!(val["content"], "line2\nline3");
        assert!(val["truncated"].as_bool().unwrap());

        fs::remove_file(path).ok();
    }

    #[test]
    fn test_write_file() {
        let path = get_temp_path("write");
        let path_str = path.to_str().unwrap();
        let content = "test content for write_file";
        let args = json!({
            "path": path_str,
            "content": content
        });

        let val = execute_write_file(&args).unwrap();
        assert_eq!(val["path"], path_str);
        assert_eq!(val["bytes_written"], content.len() as u64);

        let actual_content = fs::read_to_string(path_str).unwrap();
        assert_eq!(actual_content, content);

        fs::remove_file(path).ok();
    }

    #[test]
    fn test_str_replace_exact_and_fuzzy() {
        let path = get_temp_path("replace");
        fs::write(&path, "fn main() {\n    println!( \"hello\" );\n}").unwrap();
        let path_str = path.to_str().unwrap();

        // Match
        let _res1 = execute_str_replace(
            &json!({ "path": path_str, "old_string": "println!( \"hello\" );", "new_string": "println!(\"world\");" }),
        );

        let content = fs::read_to_string(path_str).unwrap();
        assert!(content.contains("println!(\"world\");"));

        // Fuzzy match
        let res2 = execute_str_replace(
            &json!({ "path": path_str, "old_string": "println! ( \"world\" ) ;", "new_string": "fixed();" }),
        );
        assert!(
            res2.as_ref().unwrap()["match_type"] == "fuzzy",
            "Fuzzy match failed: {}",
            res2.as_ref().unwrap()
        );

        let content = fs::read_to_string(path_str).unwrap();
        assert!(content.contains("fixed();"));

        fs::remove_file(path).ok();
    }

    #[test]
    fn test_fuzzy_mismatch_report_all_patterns() {
        // 1. whitespace_only - leading indentation diff
        let report = build_fuzzy_mismatch_report("foo bar", "  foo bar");
        println!("\n=== whitespace_only: leading indent ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());

        // 2. whitespace_only - trailing whitespace diff
        let report = build_fuzzy_mismatch_report("foo bar", "foo bar    ");
        println!("\n=== whitespace_only: trailing ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());

        // 3. whitespace_only - internal spacing diff
        let report = build_fuzzy_mismatch_report("foo  bar", "foo   bar");
        println!("\n=== whitespace_only: internal gap ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());

        // 4. whitespace_only - different internal gap count (per-gap values differ)
        let report = build_fuzzy_mismatch_report("foo  bar  baz", "foo bar baz");
        println!("\n=== whitespace_only: per-gap diff ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());

        // 5. whitespace_only - tab vs space (unspecified)
        let report = build_fuzzy_mismatch_report("foo\tbar", "foo bar");
        println!("\n=== whitespace_only: tab vs space (unspecified) ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());

        // 6. extra_line - different line counts (same tokens)
        let report = build_fuzzy_mismatch_report("foo bar\nbaz", "foo\nbar\nbaz");
        println!("\n=== whitespace_only: extra line ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    }

    #[test]
    fn test_fuzzy_vs_report_gap_analysis() {
        // Simulate the fuzzy matching regex to find mismatches
        // between what fuzzy CAN match vs what report CAN detect

        // CASE A: provided has tab, actual has spaces
        // Fuzzy regex: "foo\tbar" -> regex::escape -> "foo\tbar" (tab stays literal, NOT \s*)
        // So fuzzy would FAIL to match "foo  bar" (no mismatch report generated!)
        // This is a GAP: NO_REPORT because fuzzy itself fails
        let re = Regex::new(r"foo\tbar").unwrap();
        assert!(
            !re.is_match("foo  bar"),
            "Expected fuzzy match to FAIL (tab in provided, spaces in file)"
        );
        println!("\n=== CASE A: tab vs spaces -> fuzzy FAILs, NO report generated ===");

        // CASE B: provided has space, actual has tab
        // Fuzzy regex: "foo bar" -> regex::escape -> "foo" + "\s*" + "bar"
        // So fuzzy WOULD match "foo\tbar" (mismatch report IS generated)
        let re = Regex::new(r"foo\s*bar").unwrap();
        assert!(
            re.is_match("foo\tbar"),
            "Expected fuzzy match to SUCCEED (space in provided, tab in file)"
        );
        // What does the report say?
        let report = build_fuzzy_mismatch_report("foo bar", "foo\tbar");
        println!("\n=== CASE B: space vs tab -> fuzzy SUCCEEDS, report says ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        // split_whitespace splits both on space and tab -> tokens are ["foo", "bar"] == ["foo", "bar"]
        // So report says "whitespace_only" with "unspecified whitespace difference" (tab vs space)

        // CASE C: provided has newline, actual has spaces
        // Fuzzy regex: "foo\nbar" -> regex::escape -> "foo\nbar" (newline stays literal, NOT \s*)
        // So fuzzy would FAIL to match "foo  bar" (no mismatch report generated!)
        let re = Regex::new(r"foo\nbar").unwrap();
        assert!(
            !re.is_match("foo  bar"),
            "Expected fuzzy match to FAIL (newline in provided, spaces in file)"
        );
        println!("\n=== CASE C: newline vs spaces -> fuzzy FAILs, NO report generated ===");

        // CASE D: provided has space, actual has newline (cross-line match!)
        // Fuzzy regex: "foo bar" -> regex::escape -> "foo\s*bar"
        // \s* matches newline -> fuzzy WOULD match "foo\nbar"
        let re = Regex::new(r"foo\s*bar").unwrap();
        assert!(
            re.is_match("foo\nbar"),
            "Expected fuzzy match to SUCCEED (space in provided, newline in file)"
        );
        let report = build_fuzzy_mismatch_report("foo bar", "foo\nbar");
        println!("\n=== CASE D: space vs newline -> fuzzy SUCCEEDS (cross-line!), report says ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        // split_whitespace: ["foo", "bar"] == ["foo", "bar"] -> whitespace_only, but
        // provided has 1 line, actual has 2 lines -> extra_line + unspecified diff

        // CASE E: provided has extra internal whitespace run (double space vs single space)
        // Fuzzy regex: "foo  bar" -> regex::escape -> "foo  bar" -> replace space -> "foo\s*\s*bar"
        // (each space independently becomes \s*)
        // This still matches "foo bar" (single space), because \s* matches 0
        let re = Regex::new(r"foo\s*\s*bar").unwrap();
        assert!(
            re.is_match("foo bar"),
            "Expected fuzzy match to SUCCEED (double space in provided, single in file)"
        );
        let report = build_fuzzy_mismatch_report("foo  bar", "foo bar");
        println!("\n=== CASE E: double space vs single -> fuzzy SUCCEEDS, report says ===");
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        // split_whitespace: ["foo", "bar"] == ["foo", "bar"] -> whitespace_only
        // leading=0,0  trailing=0,0  internal_ws: provided="  " (2), actual=" " (1)

        // CASE F: provided has trailing newline, actual does not
        // Fuzzy regex: "foo bar\n" -> escape -> "foo bar\n" -> replace space -> "foo\s*bar\n"
        // The trailing \n is literal -> requires actual to end with newline
        let re = Regex::new(r"foo\s*bar\n").unwrap();
        assert!(
            !re.is_match("foo bar"),
            "Expected fuzzy match to FAIL (trailing newline in provided, none in file)"
        );
        println!("\n=== CASE F: trailing newline -> fuzzy FAILs, NO report generated ===");
    }

    #[test]
    fn test_list_directory() {
        // Test directory listing for the project root
        let res = execute_list_directory(&json!({ "path": "." })).unwrap();
        let entries: Vec<&str> = res["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(entries.iter().any(|n| *n == "src"));
        assert!(
            entries
                .iter()
                .any(|n| *n == "Cargo.toml" || *n == "Cargo.lock")
        );
    }

    #[test]
    fn test_grep_search() {
        // Search for a function definition within the project source
        let res =
            execute_grep_search(&json!({ "query": "pub fn get_tool_definitions", "path": "src" }))
                .unwrap();
        let matches: Vec<&str> = res["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(matches.iter().any(|p| p.contains("tools.rs")));
        assert!(res["total_matches"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn test_execute_bash_security() {
        // Test an allowed command from the whitelist
        let res = execute_bash(&json!({ "command": "echo 'test execution'" }))
            .await
            .unwrap();
        assert_eq!(res["exit_code"], 0);
        assert!(res["stdout"].as_str().unwrap().contains("test execution"));

        // Test a command blocked by the security whitelist
        let res = execute_bash(&json!({ "command": "rm -rf /tmp/some_non_existent_file" })).await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("BASH_NOT_WHITELISTED")
        );
    }

    #[tokio::test]
    async fn test_fetch_web_validation() {
        // Test invalid URL scheme
        let res = execute_fetch_web(&json!({ "url": "ftp://example.com" })).await;
        assert!(res.is_err());

        // Test private network access rejection
        let res = execute_fetch_web(&json!({ "url": "http://127.0.0.1/admin" })).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("forbidden"));
    }

    #[test]
    fn test_strip_html_tags() {
        let html =
            "<html><body><h1>Title</h1><p>Paragraph with <a href='#'>link</a>.</p></body></html>";
        let plain = strip_html_tags(html).replace("\n", ""); // remove newlines for testing
        // Verify links are converted to markdown
        assert!(plain.contains("[link](#)"));

        let complex = "<script>alert(1)</script>  <style>body{}</style>Text";
        assert_eq!(strip_html_tags(complex).trim(), "Text");
    }
}
