#![cfg(feature = "gui")]

//! Pretty GUI rendering for tool execution.
//!
//! Provides a more user-friendly and human-readable GUI experience
//! during the tool calling process,
//! parallel to the terminal-based `pretty` module.

use eframe::egui;
use serde_json::Value;

use crate::tools_fuzzy::{
    build_full_fuzzy_pattern, build_full_skip_blank_pattern, build_space_fuzzy_pattern,
    build_tab_fuzzy_pattern, build_tab_skip_blank_pattern,
};

// egui color constants -- roughly match ANSI codes used in the `pretty` module.
pub const C_GREEN: egui::Color32 = egui::Color32::from_rgb(150, 170, 100);
pub const C_MAGENTA: egui::Color32 = egui::Color32::from_rgb(170, 120, 170);
pub const C_GRAY: egui::Color32 = egui::Color32::from_rgb(130, 130, 130);
pub const C_RED: egui::Color32 = egui::Color32::from_rgb(220, 80, 80);
pub const C_CYAN: egui::Color32 = egui::Color32::from_rgb(130, 178, 169);
const C_YELLOW: egui::Color32 = egui::Color32::from_rgb(200, 180, 80);
const BG_GREEN: egui::Color32 = egui::Color32::from_rgb(80, 150, 95);
const BG_RED: egui::Color32 = egui::Color32::from_rgb(190, 85, 85);
const HDR_RED: egui::Color32 = egui::Color32::from_rgb(218, 75, 80);
const HDR_GREEN: egui::Color32 = egui::Color32::from_rgb(45, 180, 103);

fn truncate_str(s: &str, limit: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= limit * 2 + 3 {
        return s.to_string();
    }
    let first: String = s.chars().take(limit).collect();
    let last: String = s.chars().skip(char_count - limit).collect();
    format!("{}...{}", first, last)
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

/// Render a diff preview into an egui `Ui`.
fn show_diff_preview(
    ui: &mut egui::Ui,
    path: &str,
    start_line: usize,
    diff: &[DiffLine],
    match_type: Option<&str>,
) {
    let match_label = match match_type {
        Some("exact_match") => "[exact] Perfect match.",
        Some("space_fuzzy_match") => "[space_fuzzy] Space count mismatch",
        Some("tab_fuzzy_match") => "[tab_fuzzy] Tab/Space mismatch",
        Some("full_fuzzy_match") => "[fuzzy] Line break/Structure mismatch",
        Some("tab_skip_blank_match") => "[tab_skip_blank] Tab/Space mismatch, blank lines ignored",
        Some("full_skip_blank_match") => "[full_skip_blank] Major mismatch, blank lines ignored",
        _ => "",
    };
    let header = if match_label.is_empty() {
        format!("-- Code Preview: {} --", path)
    } else {
        format!("-- Code Preview: {} {} --", path, match_label)
    };
    ui.colored_label(C_GRAY, &header);

    let mut old_cur = start_line;
    let mut new_cur = start_line;
    let mut added = 0u32;
    let mut removed = 0u32;

    let font_id = egui::FontId::monospace(ui.style().text_styles[&egui::TextStyle::Monospace].size);
    ui.style_mut().spacing.item_spacing.y = 0.0;
    for d in diff {
        match d {
            DiffLine::Context(c) => {
                ui.colored_label(C_GRAY, format!(" {:<5} {:<5}   {}", old_cur, new_cur, c));
                old_cur += 1;
                new_cur += 1;
            }
            DiffLine::Removed(c) => {
                removed += 1;
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                let full_w = ui.available_width();
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::hover());
                // Full-width background
                ui.painter().rect_filled(rect, 0.0, BG_RED);

                // Line number with HDR_RED - left-aligned, 11-char zone matching CLI
                let ln = format!(" {:<5}     ", old_cur);
                let ln_galley =
                    ui.painter()
                        .layout_no_wrap(ln, font_id.clone(), egui::Color32::WHITE);
                let ln_rect = egui::Rect::from_min_size(rect.left_top(), ln_galley.size());
                ui.painter().rect_filled(ln_rect, 0.0, HDR_RED);
                ui.painter()
                    .galley(rect.left_top(), ln_galley, egui::Color32::WHITE);

                // Content text (white, on BG_RED background)
                let content = format!("- {}", c);
                ui.painter().text(
                    rect.left_top() + egui::vec2(ln_rect.width() + 4.0, 0.0),
                    egui::Align2::LEFT_TOP,
                    content,
                    font_id.clone(),
                    egui::Color32::WHITE,
                );
                old_cur += 1;
            }
            DiffLine::Added(c) => {
                added += 1;
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                let full_w = ui.available_width();
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(full_w, row_h), egui::Sense::hover());
                // Full-width background
                ui.painter().rect_filled(rect, 0.0, BG_GREEN);

                // Line number with HDR_GREEN - left-aligned, 11-char zone matching CLI
                let ln = format!("     {:<5} ", new_cur);
                let ln_galley =
                    ui.painter()
                        .layout_no_wrap(ln, font_id.clone(), egui::Color32::WHITE);
                let ln_rect = egui::Rect::from_min_size(rect.left_top(), ln_galley.size());
                ui.painter().rect_filled(ln_rect, 0.0, HDR_GREEN);
                ui.painter()
                    .galley(rect.left_top(), ln_galley, egui::Color32::WHITE);

                // Content text (white, on BG_GREEN background)
                let content = format!("+ {}", c);
                ui.painter().text(
                    rect.left_top() + egui::vec2(ln_rect.width() + 4.0, 0.0),
                    egui::Align2::LEFT_TOP,
                    content,
                    font_id.clone(),
                    egui::Color32::WHITE,
                );
                new_cur += 1;
            }
        }
    }

    ui.colored_label(C_GRAY, format!("[+{}, -{}]", added, removed));
}

/// Compute a diff preview for `str_replace_editor` arguments.
/// Silent on error -- returns `None` instead of printing diagnostics.
fn compute_str_replace_diff(args: &Value) -> Option<(String, usize, Vec<DiffLine>, String)> {
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
        let visible_start = ctx_before.saturating_add(start_pos);
        let line_num = visible_start + 1;
        return Some((path, line_num, result, "exact_match".to_string()));
    }

    // Step 2: Space-fuzzy match -- only horizontal space count variation
    let space_fuzzy_pattern = build_space_fuzzy_pattern(old_s);
    if let Ok(space_re) = regex::Regex::new(&space_fuzzy_pattern)
        && let Some(res) = try_fuzzy_diff(
            &path,
            &content,
            &space_re,
            old_s,
            new_s,
            &file_lines,
            "space_fuzzy_match",
        )
    {
        return Some(res);
    }

    // Step 3: Tab-fuzzy match -- tabs in old_s become [ \t]*
    let tab_fuzzy_pattern = build_tab_fuzzy_pattern(old_s);
    if let Ok(tab_re) = regex::Regex::new(&tab_fuzzy_pattern)
        && let Some(res) = try_fuzzy_diff(
            &path,
            &content,
            &tab_re,
            old_s,
            new_s,
            &file_lines,
            "tab_fuzzy_match",
        )
    {
        return Some(res);
    }

    // Step 3.5: Tab-fuzzy + blank-line tolerant
    let tab_skip_blank_pattern = build_tab_skip_blank_pattern(old_s);
    if !tab_skip_blank_pattern.is_empty()
        && let Ok(tab_skip_re) = regex::Regex::new(&tab_skip_blank_pattern)
        && let Some(res) = try_fuzzy_diff(
            &path,
            &content,
            &tab_skip_re,
            old_s,
            new_s,
            &file_lines,
            "tab_skip_blank_match",
        )
    {
        return Some(res);
    }

    // Step 4: Full fuzzy match -- all whitespace fully flexible
    let full_pattern = build_full_fuzzy_pattern(old_s);
    let re = match regex::Regex::new(&full_pattern) {
        Ok(r) => r,
        Err(_) => return None, // silent on error
    };

    if let Some(res) = try_fuzzy_diff(
        &path,
        &content,
        &re,
        old_s,
        new_s,
        &file_lines,
        "full_fuzzy_match",
    ) {
        return Some(res);
    }

    // Step 4.5: Full-fuzzy + blank-line tolerant
    let full_skip_blank_pattern = build_full_skip_blank_pattern(old_s);
    if !full_skip_blank_pattern.is_empty()
        && let Ok(full_skip_re) = regex::Regex::new(&full_skip_blank_pattern)
        && let Some(res) = try_fuzzy_diff(
            &path,
            &content,
            &full_skip_re,
            old_s,
            new_s,
            &file_lines,
            "full_skip_blank_match",
        )
    {
        return Some(res);
    }

    None // silent on error
}

/// Try to fuzzy-match `old_s` in `content` using the given regex,
/// and return a diff preview tuple if a unique match is found.
fn try_fuzzy_diff(
    file_path: &str,
    content: &str,
    re: &regex::Regex,
    _old_s: &str,
    new_s: &str,
    file_lines: &[&str],
    match_type_str: &str,
) -> Option<(String, usize, Vec<DiffLine>, String)> {
    let matches: Vec<_> = re.find_iter(content).collect();
    if matches.is_empty() || matches.len() > 1 {
        return None;
    }

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

    let start_line = ctx_before + 1;
    Some((
        file_path.to_string(),
        start_line,
        result,
        match_type_str.to_string(),
    ))
}

/// Compute the (start_line, end_line) range where `new_string` appears in the file.
fn compute_replace_lines(path: &str, args: &Value) -> Option<(u64, u64)> {
    let new_s = args.get("new_string")?.as_str()?;
    let content = std::fs::read_to_string(path).ok()?;
    let pos = content.find(new_s)?;
    let start_line = content[..pos].chars().filter(|c| *c == '\n').count() as u64 + 1;
    let new_lines = new_s.chars().filter(|c| *c == '\n').count() as u64;
    let end_line = start_line + new_lines;
    Some((start_line, end_line))
}

/// Render a pretty command preview for the given tool name and arguments.
pub fn gui_pretty_command(ui: &mut egui::Ui, name: &str, args: &Value) {
    match name {
        "read_file" => {
            let path = match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return,
            };
            ui.colored_label(C_YELLOW, format!("-- Read: {}", path));
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
            show_diff_preview(ui, &path, 1, &diff, None);
        }
        "str_replace_editor" => {
            if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(args) {
                show_diff_preview(ui, &path, start_line, &diff, Some(&match_type));
            } else {
                // Fallback: compute diff from old/new strings directly
                let obj = match args.as_object() {
                    Some(o) => o,
                    None => return,
                };
                let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                let old_s = obj.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
                let new_s = obj.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                if old_s == new_s {
                    return;
                }
                let old_lines: Vec<&str> = old_s.lines().collect();
                let new_lines: Vec<&str> = new_s.lines().collect();
                let diff = group_diff(&compute_diff(&old_lines, &new_lines));
                show_diff_preview(ui, path, 1, &diff, None);
            }
        }
        "execute_bash" => {
            let cmd = match args.get("command").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => return,
            };
            ui.colored_label(C_YELLOW, format!("-- Command: {}", cmd));
        }
        "grep_search" => {
            let query = match args.get("query").and_then(|v| v.as_str()) {
                Some(q) => q.to_string(),
                None => return,
            };
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                ui.colored_label(C_YELLOW, format!("-- Grep: {} (in {})", query, path));
            } else {
                ui.colored_label(C_YELLOW, format!("-- Grep: {}", query));
            }
        }
        "list_directory" => {
            let path = match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return,
            };
            ui.colored_label(C_YELLOW, format!("-- List: {}", path));
        }
        "fetch_web" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => return,
            };
            ui.colored_label(C_YELLOW, format!("-- Fetch: {}", url));
        }
        _ => {}
    }
}

/// Render a pretty result display for the given tool execution result.
pub fn gui_pretty_result(ui: &mut egui::Ui, name: &str, result: &Value, args_json: Option<&Value>) {
    let obj = match result.as_object() {
        Some(o) => o,
        None => {
            ui.colored_label(C_GRAY, format!("Result: {}", result));
            return;
        }
    };

    match name {
        "read_file" => {
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let total = obj.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
            let start = obj.get("start").and_then(|v| v.as_u64()).unwrap_or(0);
            let end = obj.get("end").and_then(|v| v.as_u64()).unwrap_or(0);
            let unit = obj.get("unit").and_then(|v| v.as_str()).unwrap_or("lines");
            let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = content.len() as u64;
            let trimmed = content.trim();
            let first = truncate_str(trimmed.split('\n').next().unwrap_or(""), 20);
            let last = {
                let lines: Vec<&str> = trimmed.lines().collect();
                truncate_str(lines.last().unwrap_or(&"").trim_end_matches('\n'), 20)
            };
            let (prefix, total_label) = if unit == "pages" {
                ("P", "pages")
            } else {
                ("L", "lines")
            };
            ui.colored_label(
                C_GRAY,
                format!(
                    "[{} bytes, {}{}-{}{} (file total: {} {}) ({})] {} ... {}",
                    bytes, prefix, start, prefix, end, total, total_label, path, first, last
                ),
            );
        }
        "write_file" => {
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let bytes = obj
                .get("bytes_written")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            ui.colored_label(C_GRAY, format!("[{} bytes ({})]", bytes, path));
        }
        "str_replace_editor" => {
            let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");

            let match_type = obj.get("match_type").and_then(|v| v.as_str());

            let line_range = if let Some(aj) = args_json {
                compute_replace_lines(path, aj)
            } else {
                None
            };

            let match_label = match match_type {
                Some("exact_match") => "[exact] Perfect match.".to_string(),
                Some("space_fuzzy_match") => "[space_fuzzy] Space-only fuzzy match.".to_string(),
                Some("tab_fuzzy_match") => {
                    "[tab_fuzzy] Tab characters detected in indents.".to_string()
                }
                Some("full_fuzzy_match") => {
                    "[fuzzy] Major mismatch in line breaks or structure.".to_string()
                }
                Some("tab_skip_blank_match") => {
                    "[tab_skip_blank] Tab/Space mismatch, blank lines ignored.".to_string()
                }
                Some("full_skip_blank_match") => {
                    "[full_skip_blank] Major mismatch, blank lines ignored.".to_string()
                }
                _ => String::new(),
            };

            match line_range {
                Some((start_l, end_l)) => {
                    ui.colored_label(
                        C_GRAY,
                        format!("[L{}-L{}: {} ({})]", start_l, end_l, match_label, path),
                    );
                }
                None => {
                    ui.colored_label(C_GRAY, format!("[: {} ({})]", match_label, path));
                }
            }

            // Post-execution diff preview
            if let Some(aj) = args_json
                && let Some((diff_path, diff_start, diff, diff_match_type)) =
                    compute_str_replace_diff(aj)
            {
                show_diff_preview(ui, &diff_path, diff_start, &diff, Some(&diff_match_type));
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
                    ui.label(format!(" - {}:{}:{}", path, line, text));
                }
            }
            ui.colored_label(
                C_GRAY,
                format!("[{} match{}]", total, if total != 1 { "es" } else { "" }),
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
                    ui.label(format!(" - {}{}", name, suffix));
                }
            }
            let count = entries.map(|e| e.len()).unwrap_or(0);
            ui.colored_label(
                C_GRAY,
                format!("[{} item{}]", count, if count != 1 { "s" } else { "" }),
            );
        }
        "execute_bash" => {
            let exit = obj.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(0);
            let stdout = obj.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = obj.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            if !stdout.is_empty() {
                ui.colored_label(C_GRAY, "stdout:");
                for line in stdout.lines() {
                    ui.label(line);
                }
            }
            if !stderr.is_empty() {
                ui.colored_label(C_GRAY, "stderr:");
                for line in stderr.lines() {
                    ui.label(line);
                }
            }
            if exit == 0 {
                ui.colored_label(C_GREEN, format!("[exit {}]", exit));
            } else {
                ui.colored_label(C_RED, format!("[exit {}]", exit));
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
            ui.colored_label(
                C_GRAY,
                format!("[{} bytes ({})] {} ... {}", bytes, url, first, last),
            );
        }
        _ => {
            ui.colored_label(C_GRAY, format!("Result: {}", result));
        }
    }
}
