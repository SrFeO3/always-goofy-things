//! Attached file support for `@file` references.
//!
//! Parses `@path1, @path2, ...` prefixes from the beginning of user input,
//! validates file existence, checks sizes, and reads file contents.
//! File classification and encoding is delegated to [`crate::file`].
//!
//! ## Attach modes
//!
//! | Prefix | Mode | Behaviour |
//! |--------|------|-----------|
//! | `@`    | Raw  | Send file as-is (PDF -> base64, images -> data URL, text -> UTF-8) |
//! | `@@`   | Text | Force text extraction (PDF -> Markdown via pdf_oxide) |
//!
//! Provider support for raw `@` mode:
//! - Anthropic: PDF supported, images supported.
//! - OpenAI, Ollama: PDF not supported (stripped with warning), images supported.

use std::path::Path;

use regex::Regex;

use crate::file::FileType;

/// Size threshold for large-file confirmation (1 MiB).
pub const OVERLOADED_BYTES: u64 = 1_048_576;

/// Controls how attached files are read and sent to the LLM.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AttachMode {
    /// `@file` - send file contents as-is (raw bytes, base64 for binary).
    Raw,
    /// `@@file` - force text extraction (e.g. PDF -> Markdown via pdf_oxide).
    TextExtraction,
}

/// Information about an attached file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachedFile {
    pub path: String,
    #[serde(skip)]
    pub content: String,
    pub attach_type: FileType,
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

/// Parse `@file` or `@@file` prefixes from the beginning of user input.
///
/// Returns `(query, paths, mode)`. If no `@` prefix is found, returns
/// `(input, vec![], Raw)`.
pub fn parse_attached_files(input: &str) -> (String, Vec<String>, AttachMode) {
    let trimmed = input.trim();

    // Detect @@ prefix -> text extraction mode
    let (effective_input, mode) = if trimmed.starts_with("@@") {
        (&trimmed[1..], AttachMode::TextExtraction)
    } else if trimmed.starts_with('@') {
        (trimmed, AttachMode::Raw)
    } else {
        return (input.to_string(), vec![], AttachMode::Raw);
    };

    // Pattern: `@path1, @path2` followed optionally by query text.
    // Group1 = the entire file-list segment, Group2 = the rest (query).
    // Use [\\s\\S]* to match across newlines (multi-line query after @file).
    let re = Regex::new(r"^((?:@[^,\s]+\s*,\s*)*@[^,\s]+)\s*([\s\S]*)")
        .expect("invalid attached-file regex");

    if let Some(caps) = re.captures(effective_input) {
        let files_part = caps.get(1).unwrap().as_str();
        let rest = caps.get(2).map_or("", |m| m.as_str());

        let paths: Vec<String> = files_part
            .split(',')
            .map(|s| s.trim().trim_start_matches('@').to_string())
            .collect();

        (rest.to_string(), paths, mode)
    } else {
        // Starts with @ but no valid file pattern - treat as normal query, keep mode.
        (input.to_string(), vec![], mode)
    }
}

/// Validate that every path exists relative to the current working directory.
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

/// Read all files, converting non-text formats as needed.
///
/// When `mode` is [`AttachMode::TextExtraction`], PDF files are converted to
/// Markdown text via pdf_oxide instead of being sent as raw documents.
pub fn read_attached_files(
    paths: &[String],
    mode: AttachMode,
) -> Result<Vec<AttachedFile>, String> {
    let mut files = Vec::with_capacity(paths.len());
    for p in paths {
        let mut file_type = crate::file::classify_file(p);

        // @@ mode: override PDF from Document to Text (triggers pdf_oxide extraction)
        if mode == AttachMode::TextExtraction && matches!(file_type, FileType::Document { .. }) {
            file_type = FileType::Text;
        }

        let content = match &file_type {
            FileType::Text => {
                if p.to_lowercase().ends_with(".pdf") {
                    let text = crate::file_pdf::extract_text_from_pdf(p)?;
                    let _ = crate::file_pdf::save_converted_text(p, &text);
                    text
                } else {
                    std::fs::read_to_string(p)
                        .map_err(|e| format!("Failed to read '{}': {}", p, e))?
                }
            }
            FileType::Document { .. } => {
                let bytes =
                    std::fs::read(p).map_err(|e| format!("Failed to read '{}': {}", p, e))?;
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)
            }
            FileType::Image { .. } => crate::file::convert_image_to_data_url(p)?,
            FileType::Audio { .. } => {
                let (_format, b64) = crate::file::convert_audio_to_base64(p)?;
                b64
            }
        };
        files.push(AttachedFile {
            path: p.clone(),
            content,
            attach_type: file_type,
        });
    }
    Ok(files)
}

#[cfg(test)]
#[path = "tests/attach_test.rs"]
mod tests;
