//! Attached file support for `@file` references.
//!
//! Parses `@path1, @path2, ...` prefixes from the beginning of user input,
//! validates file existence, checks sizes, and reads file contents.
//! Non-text files (PDF, image, audio) are automatically converted.

use std::path::Path;

use regex::Regex;

/// Size threshold for large-file confirmation (1 MiB).
pub const OVERLOADED_BYTES: u64 = 1_048_576;

/// How an attached file should be represented in the LLM request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AttachType {
    /// Plain text content (includes PDF-to-text).
    Text,
    /// Image encoded as a data URL; carries the MIME type.
    Image { mime: String },
    /// Audio encoded as raw Base64; carries the format name.
    Audio { format: String },
}

/// Information about an attached file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachedFile {
    pub path: String,
    #[serde(skip)]
    pub content: String,
    pub attach_type: AttachType,
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
    // Use [\\s\\S]* to match across newlines (multi-line query after @file).
    let re = Regex::new(r"^((?:@[^,\s]+\s*,\s*)*@[^,\s]+)\s*([\s\S]*)")
        .expect("invalid attached-file regex");

    if let Some(caps) = re.captures(trimmed) {
        let files_part = caps.get(1).unwrap().as_str();
        let rest = caps.get(2).map_or("", |m| m.as_str());

        let paths: Vec<String> = files_part
            .split(',')
            .map(|s| s.trim().trim_start_matches('@').to_string())
            .collect();

        (rest.to_string(), paths)
    } else {
        // Input starts with '@' but doesn't match - treat as normal query.
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

/// Classify a file by extension.
pub fn classify_file(path: &str) -> AttachType {
    let lower = path.to_lowercase();
    if lower.ends_with(".pdf") {
        return AttachType::Text;
    }
    if lower.ends_with(".png") {
        return AttachType::Image {
            mime: "image/png".to_string(),
        };
    }
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        return AttachType::Image {
            mime: "image/jpeg".to_string(),
        };
    }
    if lower.ends_with(".gif") {
        return AttachType::Image {
            mime: "image/gif".to_string(),
        };
    }
    if lower.ends_with(".webp") {
        return AttachType::Image {
            mime: "image/webp".to_string(),
        };
    }
    if lower.ends_with(".wav") {
        return AttachType::Audio {
            format: "wav".to_string(),
        };
    }
    if lower.ends_with(".mp3") {
        return AttachType::Audio {
            format: "mp3".to_string(),
        };
    }
    AttachType::Text
}

/// Read all files, converting non-text formats as needed.
///
/// Returns `AttachedFile` entries in the order specified.
pub fn read_attached_files(paths: &[String]) -> Result<Vec<AttachedFile>, String> {
    let mut files = Vec::with_capacity(paths.len());
    for p in paths {
        let attach_type = classify_file(p);
        let content = match &attach_type {
            AttachType::Text => {
                if p.to_lowercase().ends_with(".pdf") {
                    let text = crate::nontext::extract_text_from_pdf(p)?;
                    let _ = crate::nontext::save_converted_text(p, &text);
                    text
                } else {
                    std::fs::read_to_string(p)
                        .map_err(|e| format!("Failed to read '{}': {}", p, e))?
                }
            }
            AttachType::Image { .. } => crate::nontext::convert_image_to_data_url(p)?,
            AttachType::Audio { .. } => {
                let (_format, b64) = crate::nontext::convert_audio_to_base64(p)?;
                b64
            }
        };
        files.push(AttachedFile {
            path: p.clone(),
            content,
            attach_type,
        });
    }
    Ok(files)
}

#[cfg(test)]
#[path = "tests/attach_test.rs"]
mod tests;
