//! Pretty UI
//! Provide a more user-friendly CLI during tool execution.
//!
//! - `read_file`: Read a file's content, optionally within a specific line range.
//!     - Success: File size, first 10 chars, and last 10 chars (excluding newlines) (1 line)
//!     - Error: Error reason (1 line)
//! - `write_file`: Create a new file or overwrite an existing one with full content.
//!     - Success: File size, first 10 chars, and last 10 chars (excluding newlines) (1 line)
//!     - Error: Error reason (1 line)
//! - `str_replace_editor`: Replace specific text blocks in a file for code modification.
//!     - Arguments view (existing UI): Shorten `old_string` and `new_string` for a compact CLI display.
//!     - Preview: Show a modern, single-pane red/green code diff for user confirmation (multi-line), on preview error, show full `old_string` and `new_string` with error message.
//!     - Success: Modification summary with changed line numbers (1 line)
//!     - Error: Error reason (1 line)
//! - `grep_search`: Search for text patterns across files in the workspace.
//!     - Success: Minimal, `grep`-like terminal output (multi-line)
//!     - Error: Error reason (1 line)
//! - `list_directory`: List the contents of a directory to explore the project structure.
//!     - Success: Minimal, `ls`-like terminal output (multi-line)
//!     - Error: Error reason (1 line)
//! - `execute_bash`: Run terminal commands to perform development tasks.
//!     - Success: Minimal terminal output (multi-line)
//!     - Error: Error reason (multi-line)
//! - `fetch_web`: Fetch and extract text content from a specified URL.
//!     - Success: Extracted size, first 10 chars, and last 10 chars (excluding newlines) (1 line)
//!      - Error: Error reason (multi-line)

use super::startup::{
    BG_GRAY, BG_GREEN, BG_RED, C_GRAY, C_GREEN, C_RED, EMPTY, ERASE_LINE, HDR_GREEN, HDR_RED, RESET,
};

fn truncate_str(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit * 2 + 3 {
        return s.to_string();
    }
    let first: String = s.chars().take(limit).collect();
    let last: String = s.chars().rev().take(limit).collect();
    format!("{}...{}", first, last)
}

/// Truncate any long string values in result JSON for display.
/// Walks the entire JSON tree and truncates every string that exceeds the limit.
/// All strings use head 10 chars + ... + tail 10 chars.
pub fn truncate_long_json(result: &str) -> String {
    let val: serde_json::Value = match serde_json::from_str(result) {
        Ok(v) => v,
        Err(_) => return result.to_string(),
    };

    fn walk(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => {
                let truncated = truncate_str(s, 10);
                if truncated != *s {
                    *s = truncated;
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr.iter_mut() {
                    walk(item);
                }
            }
            serde_json::Value::Object(map) => {
                for (_, v) in map.iter_mut() {
                    walk(v);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    let mut val = val;
    walk(&mut val);

    serde_json::to_string(&val).unwrap_or_else(|_| result.to_string())
}

// A single line of diff output.
#[derive(Debug, Clone, Copy)]
enum DiffLine<'a> {
    /// Unchanged context line
    Context(&'a str),
    /// Removed line (from old_string)
    Removed(&'a str),
    /// Added line (from new_string)
    Added(&'a str),
}

fn compute_diff<'a>(old_lines: &[&'a str], new_lines: &[&'a str]) -> Vec<DiffLine<'a>> {
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
            result.push(DiffLine::Context(old_lines[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            result.push(DiffLine::Added(new_lines[j - 1]));
            j -= 1;
        } else {
            result.push(DiffLine::Removed(old_lines[i - 1]));
            i -= 1;
        }
    }

    result.reverse();
    result
}

fn show_diff_preview(args_json: &str) {
    let args = match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(v) => v,
        Err(_) => return,
    };
    let obj = match args.as_object() {
        Some(o) => o,
        None => return,
    };

    let path = match obj.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return,
    };
    let old_s = match obj.get("old_string").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };
    let new_s = match obj.get("new_string").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return,
    };

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    if !content.contains(old_s) {
        println!("{}{} {} {}", C_RED, "old_string not found", path, RESET);
        println!("  old: {}{}{}", HDR_RED, old_s, RESET);
        println!("  new: {}{}{}", HDR_GREEN, new_s, RESET);
        return;
    }
    if old_s == new_s {
        return;
    }

    let file_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old_s.lines().collect();
    let new_lines: Vec<&str> = new_s.lines().collect();

    let mut found_line: Option<usize> = None;
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
            found_line = Some(i);
            break;
        }
    }

    let start = match found_line {
        Some(l) => l,
        None => {
            println!("{}{} {}", C_RED, "Could not match old_string in:", path);
            println!("  old: {}{}{}", HDR_RED, old_s, RESET);
            println!("  new: {}{}{}", HDR_GREEN, new_s, RESET);
            return;
        }
    };
    let end = start + old_lines.len();

    let diff = compute_diff(&old_lines, &new_lines);
    let grouped = group_diff(&diff);

    let ctx_before = ((start as i32).saturating_sub(2)).max(0) as usize;
    let ctx_after = (end + 3).min(file_lines.len());

    println!();
    println!("-- Diff Preview: {} --", path);

    let cur = ctx_before + 1;
    for l in file_lines.iter().take(start).skip(ctx_before) {
        println!("\x1b[90m{:4}{:4} \x1b[0m{}", cur, cur, l);
    }

    let mut old_cur = start + 1;
    let mut new_cur = start + 1;
    let mut added = 0;
    let mut removed = 0;

    for d in &grouped {
        match d {
            DiffLine::Context(c) => {
                println!(
                    " {}{:<4}{}{:<4} {}{} {}{}{}",
                    C_GRAY, old_cur, C_GRAY, new_cur, RESET, "  ", EMPTY, c, RESET
                );
                old_cur += 1;
                new_cur += 1;
            }
            DiffLine::Removed(c) => {
                removed += 1;
                println!(
                    " {}{:<4}{}{:<4} {}{} {}{}{}",
                    HDR_RED, old_cur, EMPTY, EMPTY, BG_RED, " -", ERASE_LINE, c, RESET
                );
                old_cur += 1;
            }
            DiffLine::Added(c) => {
                added += 1;
                println!(
                    " {}{:<4}{}{:<4} {}{} {}{}{}",
                    HDR_GREEN, EMPTY, EMPTY, new_cur, BG_GREEN, " -", ERASE_LINE, c, RESET
                );
                new_cur += 1;
            }
        }
    }

    for i in end..ctx_after {
        println!("{}{:4}{:4} \x1b[0m{}", C_GRAY, i + 1, i + 1, file_lines[i]);
    }

    println!("\n  {}+{} {}-{}\x1b[0m", C_GREEN, added, C_RED, removed);
}

fn group_diff<'a>(input: &[DiffLine<'a>]) -> Vec<DiffLine<'a>> {
    let mut result = Vec::new();
    let mut removed_buf: Vec<&DiffLine<'a>> = Vec::new();

    for item in input {
        match item {
            DiffLine::Removed(_) => removed_buf.push(item),
            _ => {
                if !removed_buf.is_empty() {
                    result.extend(removed_buf.drain(..).copied());
                }
                result.push(*item);
            }
        }
    }
    if !removed_buf.is_empty() {
        result.extend(removed_buf.into_iter().copied());
    }
    result
}

fn compute_replace_lines(path: &str, args_json: &str) -> Option<(u64, u64)> {
    let args = serde_json::from_str::<serde_json::Value>(args_json).ok()?;
    let new_s = args.get("new_string")?.as_str()?;
    let content = std::fs::read_to_string(path).ok()?;
    let pos = content.find(new_s)?;
    let start_line = content[..pos].chars().filter(|c| *c == '\n').count() as u64 + 1;
    let new_lines = new_s.chars().filter(|c| *c == '\n').count() as u64;
    let end_line = start_line + new_lines;
    Some((start_line, end_line))
}

pub fn pretty_print_result(name: &str, result_str: &str, args_json: Option<&str>) {
    let json = match serde_json::from_str::<serde_json::Value>(result_str) {
        Ok(v) => v,
        Err(_) => {
            println!("\x1b[90mResult:\x1b[0m {}", result_str);
            return;
        }
    };
    let obj = match json.as_object() {
        Some(o) => o,
        None => {
            println!("\x1b[90mResult:\x1b[0m {}", result_str);
            return;
        }
    };

    match name {
        "read_file" => {
            let error = obj.get("error").and_then(|v| v.as_str());
            if let Some(e) = error {
                println!("\x1b[91m✗ {}\x1b[0m", e);
                return;
            }
            let total = obj.get("total_lines").and_then(|v| v.as_u64()).unwrap_or(0);
            let start = obj.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0);
            let end = obj.get("end_line").and_then(|v| v.as_u64()).unwrap_or(0);
            let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = content.len() as u64;
            let trimmed = content.trim();
            let first = truncate_str(trimmed.split('\n').next().unwrap_or(""), 10);
            let last = {
                let lines: Vec<&str> = trimmed.lines().collect();
                truncate_str(
                    lines
                        .last()
                        .unwrap_or(&"")
                        .trim_end_matches(|c: char| c == '\n'),
                    10,
                )
            };
            println!(
                "\x1b[32m✓\x1b[0m \x1b[90mread_file:\x1b[0m {} bytes, L{}–L{} ({}) \"{}…{}\"",
                bytes, start, end, total, first, last
            );
        }
        "write_file" => {
            let success = obj
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !success {
                let error = obj
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                println!("\x1b[91m✗ {}\x1b[0m", error);
                return;
            }
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let bytes = obj
                .get("bytes_written")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            println!(
                "\x1b[32m✓\x1b[0m \x1b[90mwrite_file:\x1b[0m {} ({} bytes)",
                path, bytes
            );
        }
        "str_replace_editor" => {
            let success = obj
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !success {
                let error = obj
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                println!("\x1b[91m✗ {}\x1b[0m", error);
                return;
            }
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");

            // Try to compute changed line numbers from the file and args
            let line_range = if let Some(aj) = args_json {
                compute_replace_lines(path, aj)
            } else {
                None
            };

            match line_range {
                Some((start_l, end_l)) if start_l != end_l => {
                    println!(
                        "\x1b[32m✓\x1b[0m \x1b[90mstr_replace:\x1b[0m {} L{}–L{} replaced",
                        path, start_l, end_l
                    );
                }
                Some((l, _)) => {
                    println!(
                        "\x1b[32m✓\x1b[0m \x1b[90mstr_replace:\x1b[0m {} L{} replaced",
                        path, l
                    );
                }
                None => {
                    println!(
                        "\x1b[32m✓\x1b[0m \x1b[90mstr_replace:\x1b[0m {} replaced",
                        path
                    );
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
                    println!(
                        " {}{}:{}:{}{}{}",
                        BG_GRAY, path, line, text, ERASE_LINE, RESET
                    );
                }
            }
            println!(
                "\x1b[90m   ← {} match{}\x1b[0m",
                total,
                if total != 1 { "es" } else { "" }
            );
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
            println!(
                "\x1b[90m   ← {} item{}\x1b[0m",
                count,
                if count != 1 { "s" } else { "" }
            );
        }
        "execute_bash" => {
            let exit = obj.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
            let stdout = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            if !stdout.is_empty() {
                for line in stdout.lines() {
                    println!(" stderr: {}", line);
                }
                if !stdout.ends_with('\n') {
                    println!();
                }
            }
            if !stderr.is_empty() {
                println!(" stderr: {}", stderr);
                if !stderr.ends_with('\n') {
                    eprintln!();
                }
            }
            if exit != 0 {
                println!("\x1b[91m✗ exit code {}\x1b[0m", exit);
            } else {
                println!("\x1b[32m✓\x1b[0m \x1b[90mexit 0\x1b[0m");
            }
        }
        "fetch_web" => {
            let error = obj.get("error").and_then(|v| v.as_str());
            if let Some(e) = error {
                println!("\x1b[91m✗ {}\x1b[0m", e);
                return;
            }
            let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = content.len() as u64;
            let trimmed = content.trim();
            let first = truncate_str(trimmed.split('\n').next().unwrap_or(""), 10);
            let last = {
                let lines: Vec<&str> = trimmed.lines().collect();
                truncate_str(
                    lines
                        .last()
                        .unwrap_or(&"")
                        .trim_end_matches(|c: char| c == '\n'),
                    10,
                )
            };
            println!(
                "\x1b[32m✓\x1b[0m \x1b[90mfetch_web:\x1b[0m {} {} bytes \"{}…{}\"",
                url, bytes, first, last
            );
        }
        _ => {
            println!("\x1b[90mResult:\x1b[0m {}", result_str);
        }
    }
}

// Pretty-print command preview before execution.
pub fn pretty_print_command(name: &str, args_json: &str) {
    match name {
        "str_replace_editor" => show_diff_preview(args_json),
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

        let args_json = serde_json::json!({
           "new_string": "print(\"bonjour le monde\");",
           "old_string": "print(\"hello world\");",
           "path": temp_path.to_string_lossy()
        })
        .to_string();

        show_diff_preview(&args_json);
        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_show_diff_preview_how_are_you() {
        let temp_path = get_temp_path("how_are_you");
        create_test_file(&temp_path);

        let args_json = serde_json::json!({
           "new_string": "print(\"comment allez-vous ?\");",
           "old_string": "print(\"how are you?\");",
           "path": temp_path.to_string_lossy()
        })
        .to_string();

        show_diff_preview(&args_json);
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

        let args_json = serde_json::json!({
           "new_string": "print(\"bonjour\");\nprint(\"comment allez-vous ?\");\nprint(\"bar\");",
           "old_string": "print(\"hello world\");\nprint(\"how are you?\");\nprint(\"foo\");",
           "path": temp_path.to_string_lossy()
        })
        .to_string();

        show_diff_preview(&args_json);
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

        let args_json = serde_json::json!({
           "new_string": "ABCDEFG\nHIJKLMN\n12345\n67890\nOPQRSTU\nVWXYZ",
           "old_string": "abcdefg\nhijklmn\n12345\n67890\nopqrstu\nvwxyz",
           "path": temp_path.to_string_lossy()
        })
        .to_string();

        show_diff_preview(&args_json);
        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_show_diff_preview_not_found() {
        let temp_path = get_temp_path("not_found");
        create_test_file(&temp_path);

        let args_json = serde_json::json!({
           "new_string": "print(\"new stuff\");",
           "old_string": "print(\"this does not exist\");",
           "path": temp_path.to_string_lossy()
        })
        .to_string();

        show_diff_preview(&args_json);
        let _ = fs::remove_file(&temp_path);
    }
}
