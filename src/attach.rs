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
//! | `@@f:3-7` | Text | Text extraction of pages 3-7 only (1-based, inclusive; `@@f:3` = page 3) |
//!
//! Provider support for raw `@` mode:
//! - Anthropic: PDF supported, images supported.
//! - OpenAI: PDF supported, images supported.
//! - Ollama: PDF not supported (use @@ for text extraction), images supported.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::file::FileType;

/// A file reference parsed from `@`/`@@` prefixes, with an optional page range.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachedSpec {
    pub path: String,
    /// 1-based inclusive page range from `@@file.pdf:3-7` (None = all pages).
    pub page_range: Option<(usize, usize)>,
}

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
    /// 1-based inclusive page range requested via `@@file.pdf:3-7` (None = all pages).
    #[serde(default)]
    pub page_range: Option<(usize, usize)>,
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
/// Returns `(query, specs, mode)`. If no `@` prefix is found, returns
/// `(input, vec![], Raw)`. In [`AttachMode::TextExtraction`], a trailing
/// `:start-end` on a file name selects a 1-based inclusive page range
/// (`@@spec.pdf:3-7`, or `@@spec.pdf:3` for page 3 only).
pub fn parse_attached_files(input: &str) -> (String, Vec<AttachedSpec>, AttachMode) {
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
    // Use [\s\S]* to match across newlines (multi-line query after @file).
    static FILE_LIST_RE: OnceLock<Regex> = OnceLock::new();
    let re = FILE_LIST_RE.get_or_init(|| {
        Regex::new(r"^((?:@[^,\s]+\s*,\s*)*@[^,\s]+)\s*([\s\S]*)")
            .expect("invalid attached-file regex")
    });

    if let Some(caps) = re.captures(effective_input) {
        let files_part = caps.get(1).unwrap().as_str();
        let rest = caps.get(2).map_or("", |m| m.as_str());

        let specs: Vec<AttachedSpec> = files_part
            .split(',')
            .map(|s| {
                let token = s.trim().trim_start_matches('@');
                if mode == AttachMode::TextExtraction {
                    parse_file_spec(token)
                } else {
                    AttachedSpec {
                        path: token.to_string(),
                        page_range: None,
                    }
                }
            })
            .collect();

        (rest.to_string(), specs, mode)
    } else {
        // Starts with @ but no valid file pattern - treat as normal query, keep mode.
        (input.to_string(), vec![], mode)
    }
}

/// Split a trailing `:start-end` (or `:start`) page range off a file token.
///
/// Only a suffix of `:` followed by digits (and an optional `-digits` end) is
/// treated as a range; anything else (e.g. `notes:v2.md`) stays part of the
/// path. A lone `:N` means page N only. Page ranges are only recognized in
/// text-extraction mode, so `@file.pdf:3-7` keeps its literal path.
fn parse_file_spec(token: &str) -> AttachedSpec {
    // `(.+?)` is lazy so the trailing range wins over path greediness,
    // e.g. `a.pdf:3-7` splits into path `a.pdf` and range 3-7.
    static RANGE_RE: OnceLock<Regex> = OnceLock::new();
    let re = RANGE_RE
        .get_or_init(|| Regex::new(r"^(.+?):(\d+)(?:-(\d+))?$").expect("valid page-range regex"));

    if let Some(caps) = re.captures(token) {
        let path = caps.get(1).unwrap().as_str().to_string();
        // Any digit suffix is a range; zero, reversed and out-of-bounds ranges
        // are rejected later by the pdf extractor. Only numbers that cannot be
        // parsed as usize at all (overflow) keep the literal token as path.
        let start: usize = match caps.get(2).unwrap().as_str().parse() {
            Ok(n) => n,
            Err(_) => {
                return AttachedSpec {
                    path: token.to_string(),
                    page_range: None,
                };
            }
        };
        let end: usize = match caps.get(3).map_or(Ok(start), |m| m.as_str().parse()) {
            Ok(n) => n,
            Err(_) => {
                return AttachedSpec {
                    path: token.to_string(),
                    page_range: None,
                };
            }
        };
        return AttachedSpec {
            path,
            page_range: Some((start, end)),
        };
    }
    AttachedSpec {
        path: token.to_string(),
        page_range: None,
    }
}

/// Validate that every referenced path exists relative to the current working directory.
pub fn validate_files(specs: &[AttachedSpec]) -> Result<(), Vec<String>> {
    let missing: Vec<String> = specs
        .iter()
        .map(|s| &s.path)
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
pub fn check_oversized_files(specs: &[AttachedSpec], max_bytes: u64) -> Vec<(String, u64)> {
    specs
        .iter()
        .map(|s| &s.path)
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
/// Markdown text via pdf_oxide instead of being sent as raw documents; a
/// per-file `page_range` restricts the extraction to those pages.
pub fn read_attached_files(
    specs: &[AttachedSpec],
    mode: AttachMode,
) -> Result<Vec<AttachedFile>, String> {
    let mut files = Vec::with_capacity(specs.len());
    for spec in specs {
        let p = &spec.path;
        let mut file_type = crate::file::classify_file(p);

        // @@ mode: override PDF from Document to Text (triggers pdf_oxide extraction)
        if mode == AttachMode::TextExtraction && matches!(file_type, FileType::Document { .. }) {
            file_type = FileType::Text;
        }

        let content = match &file_type {
            FileType::Text => {
                if p.to_lowercase().ends_with(".pdf") {
                    let text = crate::file_pdf::extract_text_from_pdf(p, spec.page_range)?.text;
                    let _ = crate::file_pdf::save_converted_text(p, &text, spec.page_range);
                    text
                } else {
                    if spec.page_range.is_some() {
                        return Err(format!(
                            "Page range is only supported for PDF files: '{}'",
                            p
                        ));
                    }
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
            page_range: spec.page_range,
        });
    }
    Ok(files)
}

#[cfg(test)]
#[path = "tests/attach_test.rs"]
mod tests;
