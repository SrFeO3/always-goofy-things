use super::*;
use std::env;
use std::fs;

fn get_temp_path(name: &str) -> std::path::PathBuf {
    let mut path = env::temp_dir();
    path.push(format!(
        "agt_diff_test_{}_{}",
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    path
}

fn create_test_file(path: &std::path::Path) {
    let content = r#"#!/usr/bin/env python3

print("hello world");
print("how are you?");

for i in range(1, 3):
    print("hello times: ", i);
"#;
    fs::write(path, content).expect("Failed to create test file");
}

#[test]
fn test_show_diff_preview_hello_world() {
    let temp_path = get_temp_path("hello_world");
    create_test_file(&temp_path);

    let args = serde_json::json!({
        "new_string": "print(\"bonjour le monde\");",
        "old_string": "print(\"hello world\");",
        "path": temp_path.to_string_lossy()
    });

    if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
        show_diff_preview(&path, start_line, diff, Some(&match_type));
    }
    let _ = fs::remove_file(&temp_path);
}

#[test]
fn test_show_diff_preview_how_are_you() {
    let temp_path = get_temp_path("how_are_you");
    create_test_file(&temp_path);

    let args = serde_json::json!({
        "new_string": "print(\"comment allez-vous ?\");",
        "old_string": "print(\"how are you?\");",
        "path": temp_path.to_string_lossy()
    });

    if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
        show_diff_preview(&path, start_line, diff, Some(&match_type));
    }
    let _ = fs::remove_file(&temp_path);
}

#[test]
fn test_show_diff_preview_multiline() {
    let temp_path = get_temp_path("multiline");
    fs::write(
        &temp_path,
        "#!/usr/bin/env python3

print(\"hello world\");
print(\"how are you?\");
print(\"foo\");

for i in range(1, 3):
 print(\"hello times: \", i);
",
    )
    .expect("Failed to create test file");

    let args = serde_json::json!({
        "new_string": "print(\"bonjour\");\nprint(\"comment allez-vous ?\");\nprint(\"bar\");",
        "old_string": "print(\"hello world\");\nprint(\"how are you?\");\nprint(\"foo\");",
        "path": temp_path.to_string_lossy()
    });

    if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
        show_diff_preview(&path, start_line, diff, Some(&match_type));
    }
    let _ = fs::remove_file(&temp_path);
}

#[test]
fn test_show_diff_preview_two_blocks() {
    let temp_path = get_temp_path("two_blocks");
    fs::write(
        &temp_path,
        "abcdefg\nhijklmn\n12345\n67890\nopqrstu\nvwxyz\n",
    )
    .expect("Failed to create test file");

    let args = serde_json::json!({
        "new_string": "ABCDEFG\nHIJKLMN\n12345\n67890\nOPQRSTU\nVWXYZ",
        "old_string": "abcdefg\nhijklmn\n12345\n67890\nopqrstu\nvwxyz",
        "path": temp_path.to_string_lossy()
    });

    if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
        show_diff_preview(&path, start_line, diff, Some(&match_type));
    }
    let _ = fs::remove_file(&temp_path);
}

#[test]
fn test_show_diff_preview_not_found() {
    let temp_path = get_temp_path("not_found");
    create_test_file(&temp_path);

    let args = serde_json::json!({
        "new_string": "print(\"new stuff\");",
        "old_string": "print(\"this does not exist\");",
        "path": temp_path.to_string_lossy()
    });
    if let Some((path, start_line, diff, match_type)) = compute_str_replace_diff(&args) {
        show_diff_preview(&path, start_line, diff, Some(&match_type));
    }
    let _ = fs::remove_file(&temp_path);
}

#[test]
fn test_pretty_print_command_write_file() {
    let args = serde_json::json!({
        "path": "new_file.txt",
        "content": "hello\nworld\nfoo"
    });
    // Should not panic; renders all lines as Added (green) with line numbers starting at 1
    pretty_print_command("write_file", &args);
}

#[test]
fn test_pretty_print_command_write_file_empty() {
    let args = serde_json::json!({
        "path": "empty.txt",
        "content": ""
    });
    // Should not panic even with empty content
    pretty_print_command("write_file", &args);
}

#[test]
fn test_pretty_print_command_write_file_multiline() {
    let args = serde_json::json!({
        "path": "multi.py",
        "content": "#!/usr/bin/env python3\n\nprint(\"hello\")\nprint(\"world\")"
    });
    // Should render 4 lines (including blank line) as Added with line numbers 1–4
    pretty_print_command("write_file", &args);
}

#[test]
fn test_pretty_print_result_write_file_success() {
    let result = serde_json::json!({
        "success": true,
        "path": "output.txt",
        "bytes_written": 128
    });
    // Should print a success summary line without panicking
    pretty_print_result("write_file", &result, None);
}

#[test]
fn test_pretty_print_result_write_file_error() {
    let result = serde_json::json!({
        "success": false,
        "error": "Permission denied"
    });
    // Should print an error line without panicking
    pretty_print_result("write_file", &result, None);
}
