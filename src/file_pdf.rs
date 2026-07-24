//! PDF text extraction via pdf_oxide.

use pdf_oxide::PdfDocument;
use pdf_oxide::converters::ConversionOptions;

/// Extract text content from a PDF file as Markdown.
pub fn extract_text_from_pdf(path: &str) -> Result<String, String> {
    let doc =
        PdfDocument::open(path).map_err(|e| format!("Failed to open PDF '{}': {}", path, e))?;

    let page_count = doc.page_count().map_err(|e| format!("{}", e))?;

    let mut result = String::new();
    for i in 0..page_count {
        if i > 0 {
            result.push('\n');
        }
        result.push_str(&format!("--- converted-for-llm sheet {} ---\n", i + 1));

        match doc.to_markdown(i, &ConversionOptions::default()) {
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

/// Save extracted PDF text alongside the original file.
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

    let size_str = crate::attach::format_file_size(text.len() as u64);
    println!(
        "{}[Converted] {} (Markdown, {}){}",
        crate::startup::C_DIM_GRAY,
        path,
        size_str,
        crate::startup::RESET
    );

    Ok(path)
}
