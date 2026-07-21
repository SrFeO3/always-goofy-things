use super::*;

#[test]
fn test_no_at_prefix() {
    assert_eq!(
        parse_attached_files("hello world"),
        ("hello world".to_string(), vec![])
    );
}

#[test]
fn test_single_file() {
    let (query, paths) = parse_attached_files("@file.txt hello");
    assert_eq!(query, "hello");
    assert_eq!(paths, vec!["file.txt"]);
}

#[test]
fn test_single_file_no_query() {
    let (query, paths) = parse_attached_files("@file.txt");
    assert_eq!(query, "");
    assert_eq!(paths, vec!["file.txt"]);
}

#[test]
fn test_multiple_files() {
    let (query, paths) = parse_attached_files("@a.txt, @b.txt, @c.txt  query here");
    assert_eq!(query, "query here");
    assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt"]);
}

#[test]
fn test_multiple_files_no_query() {
    let (query, paths) = parse_attached_files("@a.txt, @b.txt");
    assert_eq!(query, "");
    assert_eq!(paths, vec!["a.txt", "b.txt"]);
}

#[test]
fn test_at_in_middle_ignored() {
    // Only the beginning is parsed; `@` in the middle is left alone.
    let (query, paths) = parse_attached_files("not a file @src/main.py");
    assert_eq!(query, "not a file @src/main.py");
    assert!(paths.is_empty());
}

#[test]
fn test_single_file_multiline_query() {
    let (query, paths) = parse_attached_files("@file.txt hello\nworld\nthird line");
    assert_eq!(query, "hello\nworld\nthird line");
    assert_eq!(paths, vec!["file.txt"]);
}

#[test]
fn test_spaces_around_commas() {
    let (query, paths) = parse_attached_files("@a.txt , @b.txt  , @c.txt   query");
    assert_eq!(query, "query");
    assert_eq!(paths, vec!["a.txt", "b.txt", "c.txt"]);
}
