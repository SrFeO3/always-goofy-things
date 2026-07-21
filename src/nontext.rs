//! Media conversion for non-text file attachments.
//!
//! Converts PDF, image, and audio files into text or Base64
//! representations suitable for LLM API content blocks.

use base64::Engine as _;
use pdf_oxide::PdfDocument;
use pdf_oxide::converters::ConversionOptions;

// ---------------------------------------------------------------------------
// PDF -> text (via pdf_oxide)
// ---------------------------------------------------------------------------

/// Extract text content from a PDF file.
///
/// Returns text page by page with `--- Page N ---` separators,
/// preserving reading order, multi-column layouts, and CJK text.
pub fn extract_text_from_pdf(path: &str) -> Result<String, String> {
    let doc =
        PdfDocument::open(path).map_err(|e| format!("Failed to open PDF '{}': {}", path, e))?;

    let page_count = doc.page_count().map_err(|e| format!("{}", e))?;

    let mut result = String::new();
    for i in 0..page_count {
        if i > 0 {
            result.push('\n');
        }
        let _ = result.push_str(&format!("--- converted-for-llm sheet {} ---\n", i + 1));

        match doc.to_plain_text(i, &ConversionOptions::default()) {
            Ok(text) => {
                if !text.is_empty() {
                    result.push_str(&text);
                    if !text.ends_with('\n') {
                        result.push('\n');
                    }
                }
            }
            Err(e) => {
                result.push_str(&format!("[Text extraction error: {}]\n", e));
            }
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Image → data URL (Base64)
// ---------------------------------------------------------------------------

/// Convert an image file to a `data:` URL string.
///
/// The returned string has the form `data:image/png;base64,iVBOR...`.
/// Supported formats: PNG, JPEG, GIF, WebP.
pub fn convert_image_to_data_url(path: &str) -> Result<String, String> {
    let mime = match detect_image_mime(path) {
        Some(m) => m,
        None => return Err(format!("Unsupported image format: '{}'", path)),
    };

    let bytes = std::fs::read(path).map_err(|e| format!("Failed to read '{}': {}", path, e))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{};base64,{}", mime, b64))
}

fn detect_image_mime(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".gif") {
        Some("image/gif")
    } else if lower.ends_with(".webp") {
        Some("image/webp")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Audio → raw Base64
// ---------------------------------------------------------------------------

/// Convert an audio file to a raw Base64 string (no prefix).
///
/// Supported formats: WAV, MP3.
pub fn convert_audio_to_base64(path: &str) -> Result<(String, String), String> {
    let format = match detect_audio_format(path) {
        Some(f) => f,
        None => return Err(format!("Unsupported audio format: '{}'", path)),
    };

    let bytes = std::fs::read(path).map_err(|e| format!("Failed to read '{}': {}", path, e))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok((format.to_string(), b64))
}

fn detect_audio_format(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    if lower.ends_with(".wav") {
        Some("wav")
    } else if lower.ends_with(".mp3") {
        Some("mp3")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Saving converted text to disk (PDF only)
// ---------------------------------------------------------------------------

/// Save extracted PDF text alongside the original file.
///
/// The output file is named `{orig}_converted_for_llm.txt`.
/// If that name is taken, appends `_1`, `_2`, etc. before `.txt`.
pub fn save_converted_text(orig_path: &str, text: &str) -> Result<String, String> {
    use std::path::Path;

    let base = format!("{}_converted_for_llm.txt", orig_path);

    let path = if Path::new(&base).exists() {
        let stem = format!("{}_converted_for_llm_", orig_path);
        (1..)
            .map(|n| format!("{}{}.txt", stem, n))
            .find(|p| !Path::new(p).exists())
            .unwrap()
    } else {
        base
    };

    std::fs::write(&path, text).map_err(|e| format!("Failed to write '{}': {}", path, e))?;

    println!(
        "{}[Saved] Converted text written to {}{}",
        crate::startup::C_DIM_GRAY,
        path,
        crate::startup::RESET
    );

    Ok(path)
}
