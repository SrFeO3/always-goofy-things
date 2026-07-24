//! File classification and LLM content encoding.
//!
//! [`FileType`] maps extensions to content kinds. Conversion functions
//! encode file content into LLM-compatible representations (UTF-8,
//! base64 data URLs, raw base64).

use base64::Engine as _;

/// Classifies a file by its extension.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FileType {
    Text,
    Image { mime: String },
    Audio { format: String },
    Document { mime: String },
}

/// Classify a file by extension.
pub fn classify_file(path: &str) -> FileType {
    let lower = path.to_lowercase();
    if lower.ends_with(".pdf") {
        return FileType::Document {
            mime: "application/pdf".to_string(),
        };
    }
    if lower.ends_with(".png") {
        return FileType::Image {
            mime: "image/png".to_string(),
        };
    }
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        return FileType::Image {
            mime: "image/jpeg".to_string(),
        };
    }
    if lower.ends_with(".gif") {
        return FileType::Image {
            mime: "image/gif".to_string(),
        };
    }
    if lower.ends_with(".webp") {
        return FileType::Image {
            mime: "image/webp".to_string(),
        };
    }
    if lower.ends_with(".wav") {
        return FileType::Audio {
            format: "wav".to_string(),
        };
    }
    if lower.ends_with(".mp3") {
        return FileType::Audio {
            format: "mp3".to_string(),
        };
    }
    FileType::Text
}

// ---------------------------------------------------------------------------
// Image -> data URL (Base64)
// ---------------------------------------------------------------------------

/// Convert an image file to a `data:` URL.
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
// Audio -> raw Base64
// ---------------------------------------------------------------------------

/// Convert an audio file to raw Base64 (no prefix).
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
// Data URL parsing
// ---------------------------------------------------------------------------

/// Parse a `data:<mime>;base64,<data>` URL into its media-type and Base64 payload.
pub fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let stripped = url.strip_prefix("data:")?;
    let (mime_and_enc, data) = stripped.split_once(',')?;
    let mime = mime_and_enc.strip_suffix(";base64")?;
    Some((mime, data))
}

#[cfg(test)]
#[path = "tests/file_test.rs"]
mod tests;
