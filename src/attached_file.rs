//! Attached file support for `@file` references.
//!
//! Parses `@path1, @path2, ...` prefixes from the beginning of user input,
//! validates file existence, checks sizes, and reads file contents.

use std::path::Path;

use regex::Regex;

/// Size threshold for large-file confirmation (1 MiB).
pub const OVERLOADED_BYTES: u64 = 1_048_576;

/// Information about an attached file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachedFile {
    pub path: String,
    pub content: String,
}

/// Format a file size in human-readable form, e.g. `"24.3 MB (25481220 bytes)"`.
pub fn format_file_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["bytes", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0usize;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} bytes", bytes)
    } else {
        format!("{:.1} {} ({} bytes)", size, UNITS[unit_idx], bytes)
    }
}

/// Parse the leading `@file, @file, ...` prefix from `input`.
///
/// Returns `(clean_query, Vec<raw_path>)` where `clean_query` is the remainder
/// after stripping the `@file` prefix, and `raw_paths` are the file paths as
/// written by the user (relative to the working directory).
///
/// If no `@file` prefix is found, returns `(input, vec![])`.
pub fn parse_attached_files(input: &str) -> (String, Vec<String>) {
    let trimmed = input.trim();
    if !trimmed.starts_with('@') {
        return (input.to_string(), vec![]);
    }

    // Pattern: `@path1, @path2` followed optionally by query text.
    // Group1 = the entire file-list segment, Group2 = the rest (query).
    let re =
        Regex::new(r"^((?:@[^,\s]+\s*,\s*)*@[^,\s]+)\s*(.*)").expect("invalid attached-file regex");

    if let Some(caps) = re.captures(trimmed) {
        let files_part = caps.get(1).unwrap().as_str();
        let rest = caps.get(2).map_or("", |m| m.as_str());

        let paths: Vec<String> = files_part
            .split(',')
            .map(|s| s.trim().trim_start_matches('@').to_string())
            .collect();

        (rest.to_string(), paths)
    } else {
        // Input starts with '@' but doesn't match – treat as normal query.
        (input.to_string(), vec![])
    }
}

/// Validate that every path exists relative to the current working directory.
///
/// Returns `Ok(())` if all exist, or `Err` with a list of missing paths.
pub fn validate_files(paths: &[String]) -> Result<(), Vec<String>> {
    let missing: Vec<String> = paths
        .iter()
        .filter(|p| !Path::new(p).exists())
        .cloned()
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

/// Check which files exceed the given size threshold.
///
/// Returns `Vec<(path, size_in_bytes)>` for files over `max_bytes`.
pub fn check_oversized_files(paths: &[String], max_bytes: u64) -> Vec<(String, u64)> {
    paths
        .iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(p).ok()?;
            let len = meta.len();
            if len > max_bytes {
                Some((p.clone(), len))
            } else {
                None
            }
        })
        .collect()
}

/// Read the full text content of every file in `paths`.
///
/// Returns a list of `AttachedFile` entries.
/// Files are read in the order they were specified.
pub fn read_attached_files(paths: &[String]) -> Result<Vec<AttachedFile>, String> {
    let mut files = Vec::with_capacity(paths.len());
    for p in paths {
        let content =
            std::fs::read_to_string(p).map_err(|e| format!("Failed to read '{}': {}", p, e))?;
        files.push(AttachedFile {
            path: p.clone(),
            content,
        });
    }
    Ok(files)
}

#[cfg(test)]
#[path = "tests/attached_file_test.rs"]
mod tests;
