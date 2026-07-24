use super::*;

#[test]
fn test_parse_png_data_url() {
    let url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let (mime, data) = parse_data_url(url).expect("should parse");
    assert_eq!(mime, "image/png");
    assert!(data.starts_with("iVBORw0KGgo"));
}

#[test]
fn test_parse_jpeg_data_url() {
    let url = "data:image/jpeg;base64,/9j/4AAQSkZJRg==";
    let (mime, data) = parse_data_url(url).expect("should parse");
    assert_eq!(mime, "image/jpeg");
    assert_eq!(data, "/9j/4AAQSkZJRg==");
}

#[test]
fn test_parse_data_url_no_base64() {
    // Missing ;base64 -> should return None
    assert!(parse_data_url("data:image/png,iVBOR").is_none());
}

#[test]
fn test_parse_data_url_no_prefix() {
    assert!(parse_data_url("https://example.com/image.png").is_none());
}

#[test]
fn test_parse_data_url_no_comma() {
    assert!(parse_data_url("data:image/png;base64").is_none());
}
