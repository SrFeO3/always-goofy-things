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

use super::startup::{C_CYAN, RESET};

pub const COMMAND_ALLOW_LIST: &[&str] = &[
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

static COMMAND_ALLOW_LIST_RE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    COMMAND_ALLOW_LIST
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
                "description": "List files and directories in a given directory. Use this tool to explore the project structure before reading or editing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the workspace root. Do not start with '/' or '../'." },
                        "recursive": { "type": "boolean", "description": "Whether to list subdirectories recursively." }
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

pub async fn execute_tool(name: &str, args_json: &str) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(args_json)?;

    // Path security check for tools that take 'path'
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        validate_path(path)?;
    }

    match name {
        "read_file" => {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow!("path missing"))?;
            let start = args["start_line"].as_u64().map(|v| v as usize);
            let end = args["end_line"].as_u64().map(|v| v as usize);
            execute_read_file(path, start, end)
        }
        "write_file" => {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow!("path missing"))?;
            let content = args["content"]
                .as_str()
                .ok_or_else(|| anyhow!("content missing"))?;

            if let Err(e) = validate_path(path) {
                return Ok(json!({
                    "success": false,
                    "path": path,
                    "error_code": "OUTSIDE_WORKSPACE",
                    "error": e.to_string()
                })
                .to_string());
            }
            Ok(execute_write_file(path, content))
        }
        "str_replace_editor" => {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow!("path missing"))?;
            let old_str = args["old_string"]
                .as_str()
                .ok_or_else(|| anyhow!("old_string missing"))?;
            let new_str = args["new_string"]
                .as_str()
                .ok_or_else(|| anyhow!("new_string missing"))?;
            Ok(execute_str_replace(path, old_str, new_str))
        }
        "grep_search" => {
            let query = args["query"]
                .as_str()
                .ok_or_else(|| anyhow!("query missing"))?;
            let path = args["path"].as_str();
            execute_grep_search(query, path)
        }
        "list_directory" => {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow!("path missing"))?;
            let recursive = args["recursive"].as_bool().unwrap_or(false);
            execute_list_directory(path, recursive)
        }
        "execute_bash" => {
            let command = args["command"]
                .as_str()
                .ok_or_else(|| anyhow!("command missing"))?;
            execute_bash(command).await
        }
        "fetch_web" => {
            let url = args["url"].as_str().ok_or_else(|| anyhow!("url missing"))?;
            execute_fetch_web(url).await
        }
        _ => Err(anyhow!("Unknown tool: {}", name)),
    }
}

fn validate_path(path: &str) -> Result<()> {
    let mut depth: i32 = 0;
    for component in std::path::Path::new(path).components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(anyhow!("Security violation: Absolute paths are forbidden."));
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
                "Security violation: Directory traversal outside workspace is forbidden."
            ));
        }
    }
    Ok(())
}

/// Prompts the user for confirmation and then executes the specified tool.
/// This function encapsulates the user interaction for tool execution approval.
pub async fn confirm_and_execute_tool(name: &str, args_json: &str) -> Result<String> {
    // Ask for user confirmation before execution
    print!("{}Execute this tool? (y/N): {}", C_CYAN, RESET);
    io::stdout().flush()?;
    let mut confirm = String::new();
    io::stdin().read_line(&mut confirm)?;

    let result = if confirm.trim().to_lowercase() == "y" {
        execute_tool(name, args_json).await
    } else {
        Ok("Execution denied by user.".to_string())
    };

    // Log success/failure of the tool execution
    match &result {
        Ok(_) => println!("✅ Tool executed successfully."),
        Err(e) => println!("❌ Tool execution failed: {}", e),
    };
    result
}

fn execute_read_file(
    path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<String> {
    let content = fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let start = start_line.unwrap_or(1).max(1) - 1;
    let end = end_line.unwrap_or(total_lines).min(total_lines);

    if start > end || start >= total_lines {
        return Ok(json!({ "path": path, "error": "Invalid line range" }).to_string());
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
    })
    .to_string())
}

fn execute_write_file(path: &str, content: &str) -> String {
    if content.len() as u64 > MAX_FILE_SIZE {
        return json!({
            "success": false,
            "path": path,
            "error_code": "FILE_TOO_LARGE",
            "error": "File content exceeds 10MB limit"
        })
        .to_string();
    }

    match atomic_write_with_dir(path, content) {
        Ok(bytes) => json!({
            "success": true,
            "path": path,
            "bytes_written": bytes
        })
        .to_string(),
        Err(e) => json!({
            "success": false,
            "path": path,
            "error_code": "WRITE_FAILED",
            "error": e.to_string()
        })
        .to_string(),
    }
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

fn execute_str_replace(path: &str, old_str: &str, new_str: &str) -> String {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return json!({ "success": false, "path": path, "error": e.to_string(), "error_code": "READ_FAILED" }).to_string(),
    };

    if metadata.len() > MAX_FILE_SIZE {
        return json!({ "success": false, "path": path, "error_code": "FILE_TOO_LARGE", "error": "File exceeds 10MB limit" }).to_string();
    }

    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return json!({ "success": false, "path": path, "error": e.to_string() }).to_string();
        }
    };

    // Try exact match first
    if content.matches(old_str).count() == 1 {
        let new_content = content.replace(old_str, new_str);
        match atomic_write_with_dir(path, &new_content) {
            Ok(_) => return json!({ "success": true, "path": path, "occurrences_replaced": 1, "match_type": "exact" }).to_string(),
            Err(e) => return json!({ "success": false, "path": path, "error": e.to_string(), "error_code": "WRITE_FAILED" }).to_string(),
        }
    }

    // Fallback to fuzzy match by normalizing whitespace
    let escaped = regex::escape(old_str).replace(r" ", r"\s*");
    let re = match Regex::new(&escaped) {
        Ok(r) => r,
        Err(_) => {
            return json!({ "success": false, "path": path, "error": "Invalid regex pattern" })
                .to_string();
        }
    };

    let matches: Vec<_> = re.find_iter(&content).collect();

    if matches.is_empty() {
        return json!({ "success": false, "path": path, "error": "old_string not found" })
            .to_string();
    }
    if matches.len() > 1 {
        return json!({ "success": false, "path": path, "error": "Multiple matches found. Be more specific." }).to_string();
    }

    let new_content = re
        .replace(&content, |_caps: &regex::Captures| new_str.to_string())
        .to_string();
    if let Err(e) = fs::write(path, new_content) {
        return json!({ "success": false, "path": path, "error": e.to_string() }).to_string();
    }
    json!({ "success": true, "path": path, "occurrences_replaced": 1, "match_type": "fuzzy" })
        .to_string()
}

fn execute_list_directory(path: &str, recursive: bool) -> Result<String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let metadata = entry.metadata()?;
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "type": if file_type.is_dir() { "directory" } else { "file" },
            "size": metadata.len()
        }));
    }
    Ok(json!({ "path": path, "entries": entries, "recursive": recursive }).to_string())
}

fn execute_grep_search(query: &str, path: Option<&str>) -> Result<String> {
    let search_path = path.unwrap_or(".");
    let output = std::process::Command::new("grep")
        .arg("-rnE")
        .arg(query)
        .arg(search_path)
        .output()?;

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
    })
    .to_string())
}

async fn execute_bash(command: &str) -> Result<String> {
    let cmd_trim = command.trim();

    // Whitelist verification using pre-compiled regexes
    let is_allowed = COMMAND_ALLOW_LIST_RE.iter().any(|re| re.is_match(cmd_trim));

    if !is_allowed {
        return Err(anyhow!("Command not in whitelist. Security rejection."));
    }

    // Robust check for absolute paths and directory traversal
    if ABSOLUTE_PATH_RE.is_match(cmd_trim) || PATH_TRAVERSAL_RE.is_match(cmd_trim) {
        return Err(anyhow!(
            "Security violation: Absolute paths or directory traversal detected."
        ));
    }

    // Basic check for interactive commands
    if ["nano", "vim", "top", "ssh"]
        .iter()
        .any(|&c| cmd_trim.contains(c))
    {
        return Err(anyhow!("Interactive commands are not allowed."));
    }

    let cmd_process = TokioCommand::new("bash").arg("-c").arg(cmd_trim).output();

    let output = match tokio::time::timeout(Duration::from_secs(30), cmd_process).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(anyhow!("Execution error: {}", e)),
        Err(_) => return Err(anyhow!("Command timed out after 30 seconds.")),
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
    })
    .to_string())
}

async fn execute_fetch_web(url: &str) -> Result<String> {
    validate_url(url)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let res = client.get(url).send().await?;
    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .to_string();
    let body = res.text().await?;

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
    })
    .to_string())
}

fn validate_url(url: &str) -> Result<()> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(anyhow!("Invalid scheme. Only http/https allowed."));
    }
    let host_port = url.split('/').nth(2).unwrap_or("");
    let host = host_port.split(':').next().unwrap_or("");

    if host.to_lowercase() == "localhost" {
        return Err(anyhow!("Access to localhost is forbidden."));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        let is_private = match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00, // Unique Local Address (fc00::/7)
        };
        if is_private {
            return Err(anyhow!("Access to private network is forbidden."));
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
    use std::env;
    use std::fs;

    // Helper to generate a unique temporary path for testing
    fn get_temp_path(name: &str) -> std::path::PathBuf {
        let mut path = env::temp_dir();
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
        let res = execute_read_file(path_str, None, None).unwrap();
        let val: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(val["total_lines"], 4);
        assert!(val["content"].as_str().unwrap().contains("line4"));

        // Test specific line range read (lines 2-3)
        let res = execute_read_file(path_str, Some(2), Some(3)).unwrap();
        let val: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(val["content"], "line2\nline3");
        assert!(val["truncated"].as_bool().unwrap());

        fs::remove_file(path).ok();
    }

    #[test]
    fn test_write_file() {
        let path = get_temp_path("write");
        let path_str = path.to_str().unwrap();
        let content = "test content for write_file";

        let res = execute_write_file(path_str, content);
        let val: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(val["path"], path_str);
        assert_eq!(val["success"], true);
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
        let res1 = execute_str_replace(path_str, "println!( \"hello\" );", "println!(\"world\");");
        assert!(res1.contains("\"success\":true"));

        let content = fs::read_to_string(path_str).unwrap();
        assert!(content.contains("println!(\"world\");"));

        // Fuzzy match
        let res2 = execute_str_replace(path_str, "println! ( \"world\" ) ;", "fixed();");
        assert!(
            res2.contains("\"success\":true"),
            "Fuzzy match failed: {}",
            res2
        );

        let content = fs::read_to_string(path_str).unwrap();
        assert!(content.contains("fixed();"));

        fs::remove_file(path).ok();
    }

    #[test]
    fn test_list_directory() {
        // Test directory listing for the project root
        let res = execute_list_directory(".", false).unwrap();
        assert!(res.contains("src"));
        assert!(res.contains("Cargo.toml") || res.contains("Cargo.lock"));
    }

    #[test]
    fn test_grep_search() {
        // Search for a function definition within the project source
        let res = execute_grep_search("pub fn get_tool_definitions", Some("src")).unwrap();
        assert!(res.contains("tools.rs"));
        assert!(res.contains("\"line\":"));
    }

    #[tokio::test]
    async fn test_execute_bash_security() {
        // Test an allowed command from the whitelist
        let res = execute_bash("echo 'test execution'").await.unwrap();
        assert!(res.contains("test execution"));
        assert!(res.contains("\"exit_code\":0"));

        // Test a command blocked by the security whitelist
        let res = execute_bash("rm -rf /tmp/some_non_existent_file").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Security rejection"));
    }

    #[tokio::test]
    async fn test_fetch_web_validation() {
        // Test invalid URL scheme
        let res = execute_fetch_web("ftp://example.com").await;
        assert!(res.is_err());

        // Test private network access rejection
        let res = execute_fetch_web("http://127.0.0.1/admin").await;
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
