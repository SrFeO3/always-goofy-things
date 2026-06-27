//! Pretty UI
//! Provide a more user-friendly CLI during tool execution.
//!
//! - `read_file`: Read a file's content, optionally within a specific line range.
//!     - Success: File size, first 10 chars, and last 10 chars (excluding newlines) (1 line)
//!     - Error: Error reason (1 line)
//! - `write_file`: Create a new file or overwrite an existing one with full content.
//!     - Arguments view (existing UI): Shorten `content` for a compact CLI display.
//!     - Preview: Show a modern, single-pane red/green code diff for user confirmation (multi-line)
//!     - Success: File size, first 10 chars, and last 10 chars (excluding newlines) (1 line)
//!     - Error: Error reason (1 line)
//! - `str_replace_editor`: Replace specific text blocks in a file for code modification.
//!     - Arguments view (existing UI): Shorten `old_string` and `new_string` for a compact CLI display.
//!     - Preview: Show a modern, single-pane red/green code diff for user confirmation (multi-line), on preview error, show full `old_string` and `new_string` with error message.
//!     - Success: Modification summary with changed line numbers and match type (1 line)
//!     - Error: Error reason (1 line)
//! - `grep_search`: Search for text patterns across files in the workspace.
//!     - Success: Minimal, `grep`-like terminal output (multi-line)
//!     - Error: Error reason (1 line)
//! - `list_directory`: List the contents of a directory to explore the project structure.
//!     - Success: Minimal, `ls`-like terminal output (multi-line)
//!     - Error: Error reason (1 line)
//! - `execute_bash`: Run terminal commands to perform development tasks.
//!       - Preview: Show the command to be executed (1 line)
//!      - Success: Minimal terminal output (multi-line)
//!      - Error: Error reason (multi-line)
//! - `fetch_web`: Fetch and extract text content from a specified URL.
//!     - Success: Extracted size, first 10 chars, and last 10 chars (excluding newlines) (1 line)
//!      - Error: Error reason (multi-line)

use serde_json::Value;

use super::startup::{
    BG_GREEN, BG_RED, C_GRAY, C_GREEN, C_RED, EMPTY, ERASE_LINE, HDR_GREEN, HDR_RED, RESET,
};

fn truncate_str(s: &str, limit: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= limit * 2 + 3 {
        return s.to_string();
    }
    let first: String = s.chars().take(limit).collect();
    let last: String = s.chars().skip(char_count - limit).collect();
    format!("{}...{}", first, last)
}

fn walk(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(truncate_str(s, 10)),
        Value::Array(arr) => {
            if arr.len() <= 2 {
                Value::Array(arr.iter().map(walk).collect())
            } else {
                let first = walk(&arr[0]);
                let last = walk(arr.last().unwrap());
                Value::Array(vec![first, Value::String("...".to_string()), last])
            }
        }
        Value::Object(map) => {
            let new_map = map
                .iter()
                .map(|(k, v)| (k.clone(), walk(v)))
                .collect::<serde_json::Map<String, Value>>();
            Value::Object(new_map)
        }
        Value::Null => Value::Null,
        Value::Bool(b) => Value::Bool(*b),
        Value::Number(n) => Value::Number(n.clone()),
    }
}

/// Truncate long string values and compress oversized arrays in result JSON for concise display.
/// Walks the entire JSON tree immutably (never mutates `original`), truncating strings over
/// 10 chars and capping arrays at max 3 elements (first, ..., last). Returns a new JSON string.
pub fn truncate(original: &Value) -> String {
    let truncated = walk(original);
    serde_json::to_string(&truncated).unwrap_or_else(|_| original.to_string())
}

// A single line of diff output.
#[derive(Debug, Clone)]
enum DiffLine {
    /// Unchanged context line
    Context(String),
    /// Removed line (from old_string)
    Removed(String),
    /// Added line (from new_string)
    Added(String),
}

fn compute_diff(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let m = old_lines.len();
    let n = new_lines.len();

    // Build LCS table
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if old_lines[i - 1] == new_lines[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to produce diff
    let mut result = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_lines[i - 1] == new_lines[j - 1] {
            result.push(DiffLine::Context(old_lines[i - 1].to_string()));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            result.push(DiffLine::Added(new_lines[j - 1].to_string()));
            j -= 1;
        } else {
            result.push(DiffLine::Removed(old_lines[i - 1].to_string()));
            i -= 1;
        }
    }

    result.reverse();
    result
}

fn show_diff_preview(path: &str, start_line: usize, diff: Vec<DiffLine>, match_type: Option<&str>) {
    println!();
    let match_label = match match_type {
        Some("exact") => format!("[{}exact{}]", C_GREEN, RESET),
        Some("fuzzy") => format!("[{}fuzzy{}]", C_RED, RESET),
        _ => String::new(),
    };
    if match_label.is_empty() {
        println!("-- Code Preview: {} --", path);
    } else {
        println!("-- Code Preview: {} {} --", path, match_label);
    }

    let mut old_cur = start_line;
    let mut new_cur = start_line;
    let mut added = 0;
    let mut removed = 0;

    for d in &diff {
        const ADD: &str = " +";
        const DEL: &str = " -";
        const NOP: &str = "  ";
        match d {
            DiffLine::Context(c) => {
                println!(
                    " {}{:<4}{}{:<4} {}{} {}{}{}",
                    C_GRAY, old_cur, C_GRAY, new_cur, RESET, NOP, EMPTY, c, RESET
                );
                old_cur += 1;
                new_cur += 1;
            }
            DiffLine::Removed(c) => {
                removed += 1;
                println!(
                    " {}{:<4}{}{:<4} {}{} {}{}{}",
                    HDR_RED, old_cur, EMPTY, EMPTY, BG_RED, DEL, ERASE_LINE, c, RESET
                );
                old_cur += 1;
            }
            DiffLine::Added(c) => {
                added += 1;
                println!(
                    " {}{:<4}{}{:<4} {}{} {}{}{}",
                    HDR_GREEN, EMPTY, EMPTY, new_cur, BG_GREEN, ADD, ERASE_LINE, c, RESET
                );
                new_cur += 1;
            }
        }
    }

    println!(
        "\n[{}+{}{}, {}-{}{}]",
        C_GREEN, added, RESET, C_RED, removed, RESET
    );
}

fn group_diff(input: &[DiffLine]) -> Vec<DiffLine> {
    let mut result = Vec::new();
    let mut removed_buf: Vec<&DiffLine> = Vec::new();

    for item in input {
        match item {
            DiffLine::Removed(_) => removed_buf.push(item),
            _ => {
                if !removed_buf.is_empty() {
                    result.extend(removed_buf.drain(..).cloned());
                }
                result.push(item.clone());
            }
        }
    }
    if !removed_buf.is_empty() {
        result.extend(removed_buf.into_iter().cloned());
    }
    result
}

fn compute_str_replace_diff(args: &Value) -> Option<(String, usize, Vec<DiffLine>, &str)> {
    let obj = args.as_object()?;
    let path = obj.get("path")?.as_str()?.to_string();
    let old_s = obj.get("old_string")?.as_str()?;
    let new_s = obj.get("new_string")?.as_str()?;

    if old_s == new_s {
        return None;
    }

    let content = std::fs::read_to_string(&path).ok()?;
    let file_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old_s.lines().collect();

    // Try exact line-by-line match first
    let mut start: Option<usize> = None;
    for i in 0..file_lines.len() {
        if i + old_lines.len() > file_lines.len() {
            break;
        }
        let mut matched = true;
        for (j, old_l) in old_lines.iter().enumerate() {
            if file_lines[i + j] != *old_l {
                matched = false;
                break;
            }
        }
        if matched {
            start = Some(i);
            break;
        }
    }

    if let Some(start_pos) = start {
        let end = start_pos + old_lines.len();
        let new_lines: Vec<&str> = new_s.lines().collect();
        let diff = group_diff(&compute_diff(&old_lines, &new_lines));

        let ctx_before = ((start_pos as i32).saturating_sub(2)).max(0) as usize;
        let ctx_after = (end + 3).min(file_lines.len());

        let mut result: Vec<DiffLine> = Vec::new();
        for l in file_lines.iter().take(start_pos).skip(ctx_before) {
            result.push(DiffLine::Context(l.to_string()));
        }
        result.extend(diff);
        for line in file_lines.iter().take(ctx_after).skip(end) {
            result.push(DiffLine::Context(line.to_string()));
        }

        let line_num = ctx_before + start_pos.saturating_sub(ctx_before) + 1;
        return Some((path, line_num, result, "exact"));
    }

    // Fallback: fuzzy match by treating spaces as \s*
    let escaped = regex::escape(old_s).replace(r" ", r"\s*");
    let re = match regex::Regex::new(&escaped) {
        Ok(r) => r,
        Err(_) => {
            println!("{}Could not match old_string in: {}{}", C_RED, path, RESET);
            println!("  old: {}{}{}", HDR_RED, old_s, RESET);
            println!("  new: {}{}{}", HDR_GREEN, new_s, RESET);
            return None;
        }
    };

    let matches: Vec<_> = re.find_iter(&content).collect();
    if matches.is_empty() {
        println!("{}Could not match old_string in: {}{}", C_RED, path, RESET);
        println!("  old: {}{}{}", HDR_RED, old_s, RESET);
        println!("  new: {}{}{}", HDR_GREEN, new_s, RESET);
        return None;
    }

    // Use the first match to find the line range
    let m = &matches[0];
    let matched_text = &content[m.start()..m.end()];
    let matched_lines: Vec<&str> = matched_text.lines().collect();
    let start_line_num = content[..m.start()].chars().filter(|c| *c == '\n').count();
    let end_line_num = start_line_num + matched_lines.len();

    let new_lines: Vec<&str> = new_s.lines().collect();
    let diff = group_diff(&compute_diff(matched_lines.as_slice(), &new_lines));

    let ctx_before = ((start_line_num as i32).saturating_sub(2)).max(0) as usize;
    let ctx_after = (end_line_num + 3).min(file_lines.len());

    let mut result: Vec<DiffLine> = Vec::new();
    for l in file_lines.iter().take(start_line_num).skip(ctx_before) {
        result.push(DiffLine::Context(l.to_string()));
    }
    result.extend(diff);
    for line in file_lines.iter().take(ctx_after).skip(end_line_num) {
        result.push(DiffLine::Context(line.to_string()));
    }

    let line_num = ctx_before + start_line_num.saturating_sub(ctx_before) + 1;
    Some((path, line_num, result, "fuzzy"))
}

fn compute_replace_lines(path: &str, args: &Value) -> Option<(u64, u64)> {
    let new_s = args.get("new_string")?.as_str()?;
    let content = std::fs::read_to_string(path).ok()?;
    let pos = content.find(new_s)?;
    let start_line = content[..pos].chars().filter(|c| *c == '\n').count() as u64 + 1;
    let new_lines = new_s.chars().filter(|c| *c == '\n').count() as u64;
    let end_line = start_line + new_lines;
    Some((start_line, end_line))
}

pub fn pretty_print_result(name: &str, result: &Value, args_json: Option<&Value>) {
    let obj = match result.as_object() {
        Some(o) => o,
        None => {
            println!("\x1b[90mResult:\x1b[0m {}", result);
            return;
        }
    };

    match name {
        "read_file" => {
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let total = obj.get("total_lines").and_then(|v| v.as_u64()).unwrap_or(0);
            let start = obj.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0);
            let end = obj.get("end_line").and_then(|v| v.as_u64()).unwrap_or(0);
            let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = content.len() as u64;
            let trimmed = content.trim();
            let first = truncate_str(trimmed.split('\n').next().unwrap_or(""), 20);
            let last = {
                let lines: Vec<&str> = trimmed.lines().collect();
                truncate_str(lines.last().unwrap_or(&"").trim_end_matches('\n'), 20)
            };
            println!(
                "[{} bytes, L{}-L{} (file total: {} lines) ({})] {} ... {}",
                bytes, start, end, total, path, first, last
            );
        }
        "write_file" => {
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let bytes = obj
                .get("bytes_written")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            println!("[{} bytes ({})]", bytes, path);
        }
        "str_replace_editor" => {
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");

            // Extract match_type from the result JSON
            let match_type = obj.get("match_type").and_then(|v| v.as_str());

            // Try to compute changed line numbers from the file and args
            let line_range = if let Some(aj) = args_json {
                compute_replace_lines(path, aj)
            } else {
                None
            };

            let match_label = match match_type {
                Some("exact") => format!("[{}exact{}]", C_GREEN, RESET),
                Some("fuzzy") => format!("[{}fuzzy{}]", C_RED, RESET),
                _ => String::new(),
            };

            match line_range {
                Some((start_l, end_l)) => {
                    println!("[L{}-L{}: {} ({})]", start_l, end_l, match_label, path);
                }
                None => {
                    println!("[: {} ({})]", match_label, path);
                }
            }
        }
        "grep_search" => {
            let matches_arr = obj.get("matches").and_then(|v| v.as_array());
            let total = obj
                .get("total_matches")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if let Some(m) = matches_arr {
                for m in m {
                    let path = m.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                    let line = m.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                    let text = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    println!(" - {}:{}:{}", path, line, text);
                }
            }
            println!("[{} match{}]", total, if total != 1 { "es" } else { "" });
        }
        "list_directory" => {
            let entries = obj.get("entries").and_then(|v| v.as_array());
            if let Some(entries) = entries {
                for e in entries {
                    let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let kind = e.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                    let suffix = match kind {
                        "directory" => "/",
                        "symlink" => "@",
                        _ => "",
                    };
                    println!(" - {}{}", name, suffix);
                }
            }
            let count = entries.map(|e| e.len()).unwrap_or(0);
            println!("[{} item{}]", count, if count != 1 { "s" } else { "" });
        }
        "execute_bash" => {
            let exit = obj.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
            let stdout = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            if !stdout.is_empty() {
                println!("{}stdout:{}", C_GRAY, RESET);
                for line in stdout.lines() {
                    println!("{}", line);
                }
                if !stdout.ends_with('\n') {
                    println!();
                }
            }
            if !stderr.is_empty() {
                println!("{}stderr:{}", C_GRAY, RESET);
                for line in stderr.lines() {
                    println!("{}", line);
                }
                if !stderr.ends_with('\n') {
                    println!();
                }
            }
            if exit == 0 {
                println!("[exit {}{}{}]", C_GREEN, exit, RESET);
            } else {
                println!("[exit {}{}{}]", C_RED, exit, RESET);
            }
        }
        "fetch_web" => {
            let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = content.len() as u64;
            let trimmed = content.trim();
            let first = truncate_str(trimmed.split('\n').next().unwrap_or(""), 20);
            let last = {
                let lines: Vec<&str> = trimmed.lines().collect();
                truncate_str(lines.last().unwrap_or(&"").trim_end_matches('\n'), 20)
            };

            println!("[{} bytes ({})] {} ... {}", bytes, url, first, last);
        }
        _ => {
            println!("\x1b[90mResult:\x1b[0m {}", result);
        }
    }
}

// Pretty-print command preview before execution.
pub fn pretty_print_command(name: &str, args: &Value) {
    match name {
        "read_file" => {
            let path = match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return,
            };
            println!("-- Read: {}", path);
        }
        "write_file" => {
            let obj = match args.as_object() {
                Some(o) => o,
                None => return,
            };
            let path = match obj.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return,
            };
            let content = match obj.get("content").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => return,
            };
            let diff: Vec<DiffLine> = content
                .lines()
                .map(|l| DiffLine::Added(l.to_string()))
                .collect();
            show_diff_preview(&path, 1, diff, None);
        }
        "str_replace_editor" => {
            if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(args) {
                show_diff_preview(&path, start_line, diff, Some(match_type));
            }
        }
        "execute_bash" => {
            let cmd = match args.get("command").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => return,
            };
            println!("-- Command: {}", cmd);
        }
        "grep_search" => {
            let query = match args.get("query").and_then(|v| v.as_str()) {
                Some(q) => q.to_string(),
                None => return,
            };
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                println!("-- Grep: {} (in {})", query, path);
            } else {
                println!("-- Grep: {}", query);
            }
        }
        "list_directory" => {
            let path = match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return,
            };
            println!("-- List: {}", path);
        }
        "fetch_web" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => return,
            };
            println!("-- Fetch: {}", url);
        }
        _ => {}
    }
}

#[cfg(test)]
mod diff_preview_tests {
    use super::*;
    use std::env;
    use std::fs;

    fn get_temp_path(name: &str) -> std::path::PathBuf {
        let mut path = env::temp_dir();
        path.push(format!(
            "agt_diff_test_{}_{}",
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    fn create_test_file(path: &std::path::Path) {
        let content = r#"#!/usr/bin/env python3

print("hello world");
print("how are you?");

for i in range(1, 3):
    print("hello times: ", i);
"#;
        fs::write(path, content).expect("Failed to create test file");
    }

    #[test]
    fn test_show_diff_preview_hello_world() {
        let temp_path = get_temp_path("hello_world");
        create_test_file(&temp_path);

        let args = serde_json::json!({
            "new_string": "print(\"bonjour le monde\");",
            "old_string": "print(\"hello world\");",
            "path": temp_path.to_string_lossy()
        });

        if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
            show_diff_preview(&path, start_line, diff, Some(&match_type));
        }
        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_show_diff_preview_how_are_you() {
        let temp_path = get_temp_path("how_are_you");
        create_test_file(&temp_path);

        let args = serde_json::json!({
            "new_string": "print(\"comment allez-vous ?\");",
            "old_string": "print(\"how are you?\");",
            "path": temp_path.to_string_lossy()
        });

        if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
            show_diff_preview(&path, start_line, diff, Some(&match_type));
        }
        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_show_diff_preview_multiline() {
        let temp_path = get_temp_path("multiline");
        fs::write(
            &temp_path,
            "#!/usr/bin/env python3

print(\"hello world\");
print(\"how are you?\");
print(\"foo\");

for i in range(1, 3):
 print(\"hello times: \", i);
",
        )
        .expect("Failed to create test file");

        let args = serde_json::json!({
            "new_string": "print(\"bonjour\");\nprint(\"comment allez-vous ?\");\nprint(\"bar\");",
            "old_string": "print(\"hello world\");\nprint(\"how are you?\");\nprint(\"foo\");",
            "path": temp_path.to_string_lossy()
        });

        if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
            show_diff_preview(&path, start_line, diff, Some(&match_type));
        }
        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_show_diff_preview_two_blocks() {
        let temp_path = get_temp_path("two_blocks");
        fs::write(
            &temp_path,
            "abcdefg
hijklmn
12345
67890
opqrstu
vwxyz
",
        )
        .expect("Failed to create test file");

        let args = serde_json::json!({
            "new_string": "ABCDEFG\nHIJKLMN\n12345\n67890\nOPQRSTU\nVWXYZ",
            "old_string": "abcdefg\nhijklmn\n12345\n67890\nopqrstu\nvwxyz",
            "path": temp_path.to_string_lossy()
        });

        if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
            show_diff_preview(&path, start_line, diff, Some(&match_type));
        }
        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_show_diff_preview_not_found() {
        let temp_path = get_temp_path("not_found");
        create_test_file(&temp_path);

        let args = serde_json::json!({
            "new_string": "print(\"new stuff\");",
            "old_string": "print(\"this does not exist\");",
            "path": temp_path.to_string_lossy()
        });
        if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
            show_diff_preview(&path, start_line, diff, Some(&match_type));
        }
        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_pretty_print_command_write_file() {
        let args = serde_json::json!({
            "path": "new_file.txt",
            "content": "hello\nworld\nfoo"
        });
        // Should not panic; renders all lines as Added (green) with line numbers starting at 1
        pretty_print_command("write_file", &args);
    }

    #[test]
    fn test_pretty_print_command_write_file_empty() {
        let args = serde_json::json!({
            "path": "empty.txt",
            "content": ""
        });
        // Should not panic even with empty content
        pretty_print_command("write_file", &args);
    }

    #[test]
    fn test_pretty_print_command_write_file_multiline() {
        let args = serde_json::json!({
            "path": "multi.py",
            "content": "#!/usr/bin/env python3\n\nprint(\"hello\")\nprint(\"world\")"
        });
        // Should render 4 lines (including blank line) as Added with line numbers 1–4
        pretty_print_command("write_file", &args);
    }

    #[test]
    fn test_pretty_print_result_write_file_success() {
        let result = serde_json::json!({
            "success": true,
            "path": "output.txt",
            "bytes_written": 128
        });
        // Should print a success summary line without panicking
        pretty_print_result("write_file", &result, None);
    }

    #[test]
    fn test_pretty_print_result_write_file_error() {
        let result = serde_json::json!({
            "success": false,
            "error": "Permission denied"
        });
        // Should print an error line without panicking
        pretty_print_result("write_file", &result, None);
    }
}
