use super::*;
use crate::file::{FileType, classify_file};

fn spec(path: &str) -> AttachedSpec {
    AttachedSpec {
        path: path.to_string(),
        page_range: None,
    }
}

fn spec_range(path: &str, start: usize, end: usize) -> AttachedSpec {
    AttachedSpec {
        path: path.to_string(),
        page_range: Some((start, end)),
    }
}

// ----- no prefix: nothing is attached -----

#[test]
fn test_no_at_prefix() {
    let (query, specs, mode) = parse_attached_files("hello world");
    assert_eq!(query, "hello world");
    assert!(specs.is_empty());
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_at_in_middle_ignored() {
    // Only the beginning is parsed; `@` in the middle is left alone.
    let (query, specs, mode) = parse_attached_files("not a file @src/main.py");
    assert_eq!(query, "not a file @src/main.py");
    assert!(specs.is_empty());
    assert_eq!(mode, AttachMode::Raw);
}

// ----- @ (raw) mode: one file -----

#[test]
fn test_single_file() {
    let (query, specs, mode) = parse_attached_files("@file.txt hello");
    assert_eq!(query, "hello");
    assert_eq!(specs, vec![spec("file.txt")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_single_file_no_query() {
    let (query, specs, mode) = parse_attached_files("@file.txt");
    assert_eq!(query, "");
    assert_eq!(specs, vec![spec("file.txt")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_single_file_multiline_query() {
    let (query, specs, mode) = parse_attached_files("@file.txt hello\nworld\nthird line");
    assert_eq!(query, "hello\nworld\nthird line");
    assert_eq!(specs, vec![spec("file.txt")]);
    assert_eq!(mode, AttachMode::Raw);
}

// ----- @ (raw) mode: multiple files -----

#[test]
fn test_multiple_files() {
    let (query, specs, mode) = parse_attached_files("@a.txt, @b.txt, @c.txt  query here");
    assert_eq!(query, "query here");
    assert_eq!(specs, vec![spec("a.txt"), spec("b.txt"), spec("c.txt")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_multiple_files_no_query() {
    let (query, specs, mode) = parse_attached_files("@a.txt, @b.txt");
    assert_eq!(query, "");
    assert_eq!(specs, vec![spec("a.txt"), spec("b.txt")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_multiple_files_multiline_query() {
    let (query, specs, mode) = parse_attached_files("@a.txt, @b.txt  line1\nline2");
    assert_eq!(query, "line1\nline2");
    assert_eq!(specs, vec![spec("a.txt"), spec("b.txt")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_spaces_around_commas() {
    let (query, specs, mode) = parse_attached_files("@a.txt , @b.txt  , @c.txt   query");
    assert_eq!(query, "query");
    assert_eq!(specs, vec![spec("a.txt"), spec("b.txt"), spec("c.txt")]);
    assert_eq!(mode, AttachMode::Raw);
}

// ----- @@ (text extraction) mode: no page range -----

#[test]
fn test_double_at_single_file() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf この仕様書を要約して");
    assert_eq!(query, "この仕様書を要約して");
    assert_eq!(specs, vec![spec("spec.pdf")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_double_at_no_query() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf");
    assert_eq!(query, "");
    assert_eq!(specs, vec![spec("spec.pdf")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_double_at_single_file_multiline_query() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf 要約\n2行目\n3行目");
    assert_eq!(query, "要約\n2行目\n3行目");
    assert_eq!(specs, vec![spec("spec.pdf")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_double_at_multiple_files() {
    let (query, specs, mode) = parse_attached_files("@@a.pdf, @@b.pdf  query");
    // After stripping the first @, the input becomes "@a.pdf, @@b.pdf query".
    // The file-list regex treats "@@b.pdf" as one token; trim_start_matches('@')
    // then removes both leading @'s, so the second path is "b.pdf".
    assert_eq!(query, "query");
    assert_eq!(specs, vec![spec("a.pdf"), spec("b.pdf")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_double_at_multiple_files_multiline_query() {
    let (query, specs, mode) = parse_attached_files("@@a.pdf, @@b.pdf  line1\nline2");
    assert_eq!(query, "line1\nline2");
    assert_eq!(specs, vec![spec("a.pdf"), spec("b.pdf")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

// ----- @@ mode: page range `:N` (single page) -----

#[test]
fn test_page_range_single_page() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3 レビュー");
    assert_eq!(query, "レビュー");
    assert_eq!(specs, vec![spec_range("spec.pdf", 3, 3)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_single_page_no_query() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3");
    assert_eq!(query, "");
    assert_eq!(specs, vec![spec_range("spec.pdf", 3, 3)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_single_page_multiline_query() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3 レビュー\n2行目");
    assert_eq!(query, "レビュー\n2行目");
    assert_eq!(specs, vec![spec_range("spec.pdf", 3, 3)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

// ----- @@ mode: page range `:N-M` (span) -----

#[test]
fn test_page_range_span() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3-7 レビューして");
    assert_eq!(query, "レビューして");
    assert_eq!(specs, vec![spec_range("spec.pdf", 3, 7)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_span_no_query() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3-7");
    assert_eq!(query, "");
    assert_eq!(specs, vec![spec_range("spec.pdf", 3, 7)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_span_multiline_query() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3-7 レビュー\n2行目\n3行目");
    assert_eq!(query, "レビュー\n2行目\n3行目");
    assert_eq!(specs, vec![spec_range("spec.pdf", 3, 7)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

// ----- @@ mode: multiple files with per-file ranges -----

#[test]
fn test_page_range_multi_file() {
    let (query, specs, mode) = parse_attached_files("@@a.pdf:1-2, @b.pdf:3 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(
        specs,
        vec![spec_range("a.pdf", 1, 2), spec_range("b.pdf", 3, 3)]
    );
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_multi_file_multiline_query() {
    let (query, specs, mode) = parse_attached_files("@@a.pdf:1-2, @b.pdf:3 クエリ\n2行目");
    assert_eq!(query, "クエリ\n2行目");
    assert_eq!(
        specs,
        vec![spec_range("a.pdf", 1, 2), spec_range("b.pdf", 3, 3)]
    );
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_all_files() {
    let (query, specs, mode) = parse_attached_files("@@a.pdf:1-2, @@b.pdf:3-4 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(
        specs,
        vec![spec_range("a.pdf", 1, 2), spec_range("b.pdf", 3, 4)]
    );
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_mixed_files_no_query() {
    let (query, specs, mode) = parse_attached_files("@@a.pdf:1-2, @b.pdf");
    assert_eq!(query, "");
    assert_eq!(specs, vec![spec_range("a.pdf", 1, 2), spec("b.pdf")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

// ----- @@ mode: numbers are always a range, validation happens at read -----

#[test]
fn test_page_range_zero_start() {
    // `:0-5` parses as a range; the pdf extractor rejects it later.
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:0-5 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec_range("spec.pdf", 0, 5)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_zero_end() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3-0 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec_range("spec.pdf", 3, 0)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_reversed() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:7-3 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec_range("spec.pdf", 7, 3)]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

// ----- @@ mode: invalid suffixes stay part of the path -----

#[test]
fn test_page_range_open_end_is_path() {
    // `:3-` has no end digits, so the whole token is a path.
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3- クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec("spec.pdf:3-")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_end_only_is_path() {
    // `:-7` has no start digits; end-only ranges are not supported.
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:-7 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec("spec.pdf:-7")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_double_dash_is_path() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3-7-9 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec("spec.pdf:3-7-9")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_letters_is_path() {
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:abc クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec("spec.pdf:abc")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_trailing_text_is_path() {
    // `:3-7x` is not a valid range suffix, so the whole token is a path.
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:3-7x クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec("spec.pdf:3-7x")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_non_numeric_suffix_is_path() {
    // A colon not followed by digits stays part of the path.
    let (query, specs, mode) = parse_attached_files("@@notes:v2.md クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec("notes:v2.md")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_overflow_is_path() {
    // A digit suffix that overflows usize keeps the literal token as path.
    let (query, specs, mode) = parse_attached_files("@@spec.pdf:99999999999999999999 クエリ");
    assert_eq!(query, "クエリ");
    assert_eq!(specs, vec![spec("spec.pdf:99999999999999999999")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

#[test]
fn test_page_range_space_before_range_is_query() {
    // A range must be attached to the file token; with a space in between it
    // becomes part of the query.
    let (query, specs, mode) = parse_attached_files("@@spec.pdf :3-7 クエリ");
    assert_eq!(query, ":3-7 クエリ");
    assert_eq!(specs, vec![spec("spec.pdf")]);
    assert_eq!(mode, AttachMode::TextExtraction);
}

// ----- @ (raw) mode: ranges are never parsed, the literal token is the path -----

#[test]
fn test_page_range_raw_mode_keeps_literal_path() {
    let (query, specs, mode) = parse_attached_files("@spec.pdf:3-7 hello");
    assert_eq!(query, "hello");
    assert_eq!(specs, vec![spec("spec.pdf:3-7")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_page_range_raw_mode_single_keeps_literal_path() {
    let (query, specs, mode) = parse_attached_files("@spec.pdf:3 hello");
    assert_eq!(query, "hello");
    assert_eq!(specs, vec![spec("spec.pdf:3")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_page_range_raw_mode_no_query_keeps_literal_path() {
    let (query, specs, mode) = parse_attached_files("@spec.pdf:3-7");
    assert_eq!(query, "");
    assert_eq!(specs, vec![spec("spec.pdf:3-7")]);
    assert_eq!(mode, AttachMode::Raw);
}

#[test]
fn test_page_range_raw_mode_multiple_keeps_literal_paths() {
    let (query, specs, mode) = parse_attached_files("@a.pdf:1-2, @b.pdf:3 hello");
    assert_eq!(query, "hello");
    assert_eq!(specs, vec![spec("a.pdf:1-2"), spec("b.pdf:3")]);
    assert_eq!(mode, AttachMode::Raw);
}

// ----- read_attached_files with page ranges -----

#[test]
fn test_read_attached_files_pdf_page_range() {
    use pdf_oxide::api::Pdf;

    let path = std::env::temp_dir().join(format!("attach_pdf_range_{}.pdf", std::process::id()));
    let mut pdf =
        Pdf::from_markdown("# One\n\nBody one.\n\n## Two\n\nBody two.").expect("create pdf");
    pdf.save(&path).expect("save pdf");
    let path_str = path.to_string_lossy().to_string();

    // Range extraction: content is restricted and the range is echoed.
    let file_spec = spec_range(&path_str, 1, 1);
    let files = read_attached_files(&[file_spec], AttachMode::TextExtraction).expect("read");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].attach_type, FileType::Text);
    assert_eq!(files[0].page_range, Some((1, 1)));
    assert!(
        files[0].content.contains("extracted p.1"),
        "{}",
        files[0].content
    );

    // No range: full extraction without an extracted marker.
    let file_spec = spec(&path_str);
    let files = read_attached_files(&[file_spec], AttachMode::TextExtraction).expect("read");
    assert!(
        !files[0].content.contains("extracted:"),
        "{}",
        files[0].content
    );

    // Out-of-bounds range: the error propagates from the pdf extractor.
    let file_spec = spec_range(&path_str, 99, 100);
    let err = read_attached_files(&[file_spec], AttachMode::TextExtraction).unwrap_err();
    assert!(err.contains("exceeds"), "{}", err);

    // Reversed range: rejected by the pdf extractor.
    let file_spec = spec_range(&path_str, 7, 3);
    let err = read_attached_files(&[file_spec], AttachMode::TextExtraction).unwrap_err();
    assert!(err.contains("Invalid page range"), "{}", err);

    // Zero start: rejected by the pdf extractor.
    let file_spec = spec_range(&path_str, 0, 5);
    let err = read_attached_files(&[file_spec], AttachMode::TextExtraction).unwrap_err();
    assert!(err.contains("Invalid page range"), "{}", err);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_page_range_non_pdf_errors() {
    let dir = std::env::temp_dir().join(format!("attach_range_txt_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notes.txt");
    std::fs::write(&path, "hello").unwrap();
    let file_spec = spec_range(&path.to_string_lossy(), 1, 2);
    let err = read_attached_files(&[file_spec], AttachMode::TextExtraction).unwrap_err();
    assert!(err.contains("only supported for PDF files"), "{}", err);
    std::fs::remove_dir_all(&dir).ok();
}

// ----- classify_file tests -----

#[test]
fn test_classify_pdf_is_document() {
    let t = classify_file("spec.pdf");
    assert_eq!(
        t,
        FileType::Document {
            mime: "application/pdf".to_string()
        }
    );
}

#[test]
fn test_classify_png_is_image() {
    let t = classify_file("photo.png");
    assert_eq!(
        t,
        FileType::Image {
            mime: "image/png".to_string()
        }
    );
}

#[test]
fn test_classify_txt_is_text() {
    let t = classify_file("readme.txt");
    assert_eq!(t, FileType::Text);
}
