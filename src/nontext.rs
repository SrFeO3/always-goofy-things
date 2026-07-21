//! Media conversion for non-text file attachments.
//!
//! Converts PDF, image, and audio files into text or Base64
//! representations suitable for LLM API content blocks.

use std::sync::LazyLock;

use base64::Engine as _;
use pdfium_render::prelude::*;

// ---------------------------------------------------------------------------
// PDF → text (via Pdfium)
// ---------------------------------------------------------------------------

static PDFIUM: LazyLock<Result<Pdfium, String>> = LazyLock::new(|| {
    let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
        .or_else(|_| Pdfium::bind_to_system_library())
        .map_err(|e| {
            format!(
                "Cannot load Pdfium library: {}. \
                 Download libpdfium from https://github.com/bblanchon/pdfium-binaries/releases \
                 and place it in the same directory as the executable, or install it system-wide.",
                e
            )
        })?;
    Ok(Pdfium::new(bindings))
});

/// Extract text content from a PDF file.
///
/// Returns text page by page with `--- Page N ---` separators,
/// preserving reading order, multi-column layouts, and CJK text.
pub fn extract_text_from_pdf(path: &str) -> Result<String, String> {
    let pdfium = PDFIUM
        .as_ref()
        .map_err(|e| format!("Pdfium error: {}", e))?;

    let document = pdfium
        .load_pdf_from_file(path, None)
        .map_err(|e| format!("Failed to load PDF '{}': {}", path, e))?;

    let mut result = String::new();
    for (i, page) in document.pages().iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let _ = result.push_str(&format!("--- Page {} ---\n", i + 1));

        match page.text() {
            Ok(page_text) => {
                let text = page_text.to_string();
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
