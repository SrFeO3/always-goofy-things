use super::*;
use std::fs;

// Helper to generate a unique temporary path for testing (relative path)
fn get_temp_path(name: &str) -> std::path::PathBuf {
    // Tests run with CWD = package root; register it as the workspace root
    // so path validation (validate_path) works (OnceLock: first call wins).
    crate::tools::set_workspace_root(
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    );
    fs::create_dir_all("./tmp").ok();
    let mut path = std::path::Path::new("./tmp").to_path_buf();
    path.push(format!(
        "agt_test_{}_{}",
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    path
}

#[test]
fn test_read_file() {
    let path = get_temp_path("read");
    fs::write(&path, "line1\nline2\nline3\nline4").unwrap();
    let path_str = path.to_str().unwrap();

    // Test full file read
    let args = json!({
        "path": path_str,
    });
    let val = execute_read_file(&args).unwrap();
    assert_eq!(val["total"], 4);
    assert_eq!(val["unit"], "lines");
    assert!(val["content"].as_str().unwrap().contains("line4"));

    // Test specific line range read (lines 2-3)
    let args = json!({
        "path": path_str,
        "start": Some(2),
        "end": Some(3)
    });
    let val = execute_read_file(&args).unwrap();
    assert_eq!(val["content"], "line2\nline3");
    assert_eq!(val["start"], 2);
    assert_eq!(val["end"], 3);
    assert!(val["truncated"].as_bool().unwrap());

    fs::remove_file(path).ok();
}

#[test]
fn test_read_file_text_range_errors() {
    let path = get_temp_path("read_errors");
    fs::write(&path, "line1\nline2\nline3\nline4").unwrap();
    let path_str = path.to_str().unwrap();

    // start=0 violates 1-based numbering.
    let args = json!({ "path": path_str, "start": 0, "end": 2 });
    let err = execute_read_file(&args).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("[INVALID_ARGUMENTS]"), "{}", msg);
    assert!(msg.contains("1 <= start <= end <= total"), "{}", msg);
    assert!(msg.contains("total lines: 4"), "{}", msg);

    // start > end (including end=0).
    let args = json!({ "path": path_str, "start": 3, "end": 2 });
    assert!(
        execute_read_file(&args)
            .unwrap_err()
            .to_string()
            .contains("[INVALID_ARGUMENTS]")
    );
    let args = json!({ "path": path_str, "start": 1, "end": 0 });
    assert!(
        execute_read_file(&args)
            .unwrap_err()
            .to_string()
            .contains("[INVALID_ARGUMENTS]")
    );

    // Out of bounds: never clamped, always an error.
    let args = json!({ "path": path_str, "start": 1, "end": 99 });
    let err = execute_read_file(&args).unwrap_err();
    assert!(err.to_string().contains("total lines: 4"), "{}", err);
    let args = json!({ "path": path_str, "start": 99, "end": 100 });
    assert!(
        execute_read_file(&args)
            .unwrap_err()
            .to_string()
            .contains("[INVALID_ARGUMENTS]")
    );

    fs::remove_file(path).ok();
}

#[test]
fn test_read_file_rejects_range_on_image() {
    // classify_file keys off the extension; a dummy file suffices.
    let path = get_temp_path("read_img");
    fs::write(&path, "not really a png").unwrap();
    let mut img_path = path.clone();
    img_path.set_extension("png");
    fs::rename(&path, &img_path).unwrap();
    let img_str = img_path.to_str().unwrap();

    // A range on an image is rejected with guidance, not silently ignored.
    let args = json!({ "path": img_str, "start": 1, "end": 2 });
    let err = execute_read_file(&args).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("[INVALID_ARGUMENTS]"), "{}", msg);
    assert!(msg.contains("only apply to text and PDF"), "{}", msg);

    // Without a range the image is read normally.
    let args = json!({ "path": img_str });
    assert!(execute_read_file(&args).is_ok());

    fs::remove_file(img_path).ok();
}

#[test]
fn test_read_file_pdf_pages() {
    use pdf_oxide::api::Pdf;

    let path = std::env::temp_dir().join(format!("read_file_pdf_{}.pdf", std::process::id()));
    let mut pdf =
        Pdf::from_markdown("# One\n\nBody one.\n\n## Two\n\nBody two.").expect("create pdf");
    pdf.save(&path).expect("save pdf");
    let path_str = path.to_string_lossy().to_string();

    // Full read: pages unit, review text as content.
    let args = json!({ "path": path_str });
    let val = execute_read_file(&args).unwrap();
    assert_eq!(val["unit"], "pages");
    assert!(val["total"].as_u64().unwrap() >= 1);
    assert!(val["content"].as_str().unwrap().contains("[pdf-review]"));
    assert!(val["content"].as_str().unwrap().contains("--- page 1 ---"));

    // Page range read.
    let args = json!({ "path": path_str, "start": 1, "end": 1 });
    let val = execute_read_file(&args).unwrap();
    assert_eq!(val["start"], 1);
    assert_eq!(val["end"], 1);
    assert!(val["content"].as_str().unwrap().contains("extracted p.1"));

    // Invalid page range.
    let args = json!({ "path": path_str, "start": 99, "end": 100 });
    let err = execute_read_file(&args).unwrap_err();
    assert!(err.to_string().contains("[INVALID_ARGUMENTS]"), "{}", err);

    fs::remove_file(path).ok();
}

#[test]
fn test_write_file() {
    let path = get_temp_path("write");
    let path_str = path.to_str().unwrap();
    let content = "test content for write_file";
    let args = json!({
        "path": path_str,
        "content": content
    });

    let val = execute_write_file(&args).unwrap();
    assert_eq!(val["path"], path_str);
    assert_eq!(val["bytes_written"], content.len() as u64);

    let actual_content = fs::read_to_string(path_str).unwrap();
    assert_eq!(actual_content, content);

    fs::remove_file(path).ok();
}

#[test]
fn test_str_replace_exact_and_fuzzy() {
    let path = get_temp_path("replace");
    fs::write(&path, "fn main() {\n    println!( \"hello\" );\n}").unwrap();
    let path_str = path.to_str().unwrap();

    // Match
    let _res1 = execute_str_replace(
        &json!({ "path": path_str, "old_string": "println!( \"hello\" );", "new_string": "println!(\"world\");" }),
    );

    let content = fs::read_to_string(path_str).unwrap();
    assert!(content.contains("println!(\"world\");"));

    // Fuzzy match
    let res2 = execute_str_replace(
        &json!({ "path": path_str, "old_string": "println! ( \"world\" ) ;", "new_string": "fixed();" }),
    );
    assert!(
        res2.as_ref().unwrap()["match_type"]
            .as_str()
            .unwrap()
            .contains("mismatch"),
        "Fuzzy match failed: {}",
        res2.as_ref().unwrap()
    );

    let content = fs::read_to_string(path_str).unwrap();
    assert!(content.contains("fixed();"));

    fs::remove_file(path).ok();
}

#[test]
fn test_four_stage_fuzzy_match() {
    let path = get_temp_path("four_stage");
    let path_str = path.to_str().unwrap();

    // Case 1: Exact match
    fs::write(&path, "fn hello() {\n    println!(\"hi\");\n}").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "println!(\"hi\");",
        "new_string": "println!(\"bye\");"
    }));
    assert_eq!(res.as_ref().unwrap()["match_type"], "Perfect match.");
    assert!(
        fs::read_to_string(path_str)
            .unwrap()
            .contains("println!(\"bye\");")
    );

    // Case 2: Space-fuzzy match (Space count difference - avoid substring match)
    fs::write(&path, "let x = 1  +  2;").unwrap(); // double spaces
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "let x = 1 + 2;", // single spaces
        "new_string": "let x = 3;"
    }));
    assert_eq!(
        res.as_ref().unwrap()["match_type"],
        "Space count mismatch: matched by allowing flexible space runs."
    );
    assert!(res.as_ref().unwrap().get("fuzzy_match_detail").is_some());

    // Case 3: Tab-fuzzy match (Tab vs Space)
    fs::write(&path, "fn hello() {\n\tprintln!(\"hi\");\n}").unwrap(); // tab
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "    println!(\"hi\");", // 4 spaces
        "new_string": "println!(\"bye\");"
    }));
    assert_eq!(
        res.as_ref().unwrap()["match_type"],
        "Tab/Space mismatch: matched by treating tabs and spaces as equivalent."
    );
    assert!(res.as_ref().unwrap().get("fuzzy_match_detail").is_some());

    // Case 4: Full-fuzzy match (Line break/Structure difference)
    fs::write(&path, "fn hello() {\n    println!(\"hi\");\n}").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn hello() { println!(\"hi\"); }", // flattened
        "new_string": "fn hello() { /* replaced */ }"
    }));
    assert_eq!(
        res.as_ref().unwrap()["match_type"],
        "Line break/Structure mismatch: matched by ignoring all whitespace and newlines."
    );
    assert!(res.as_ref().unwrap().get("fuzzy_match_detail").is_some());

    // Case 5: Regex meta-characters in full-fuzzy (Ensure they are escaped)
    fs::write(&path, "fn foo(x: i32) { x + 1 }").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn foo ( x : i32 ) { x + 1 }", // added spaces
        "new_string": "fn foo(x: i32) { x + 2 }"
    }));
    assert_eq!(
        res.as_ref().unwrap()["match_type"],
        "Tab/Space mismatch: matched by treating tabs and spaces as equivalent."
    );
    assert!(
        fs::read_to_string(path_str)
            .unwrap()
            .contains("fn foo(x: i32) { x + 2 }")
    );

    fs::remove_file(path).ok();
}

#[test]
fn test_str_replace_ambiguous_and_no_match() {
    let path = get_temp_path("ambiguous_no_match");
    let path_str = path.to_str().unwrap();

    // 1. Ambiguous match: multiple fuzzy matches
    // Content has double spaces, search string has single spaces.
    // This avoids the "Exact Match" shortcut (since count != 1) and
    // triggers the Space-Fuzzy stage which should find 2 matches.
    fs::write(
        &path,
        r"fn  foo() {}
fn  foo() {}
",
    )
    .unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn foo() {}",
        "new_string": "fixed();"
    }));

    assert!(res.is_err(), "Expected error for ambiguous match");
    assert!(res.unwrap_err().to_string().contains("AMBIGUOUS_MATCH"));

    // 2. No match at all
    fs::write(&path, "purely different content").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "not found",
        "new_string": "replacement"
    }));
    assert!(res.is_err(), "Expected error for no match");
    assert!(res.unwrap_err().to_string().contains("NO_MATCH"));

    fs::remove_file(path).ok();
}

#[test]
fn test_fuzzy_mismatch_report_all_patterns() {
    // 1. whitespace_only - leading indentation diff
    let report = build_fuzzy_mismatch_report("foo bar", "  foo bar");
    println!("\n=== whitespace_only: leading indent ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());

    // 2. whitespace_only - trailing whitespace diff
    let report = build_fuzzy_mismatch_report("foo bar", "foo bar    ");
    println!("\n=== whitespace_only: trailing ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());

    // 3. whitespace_only - internal spacing diff
    let report = build_fuzzy_mismatch_report("foo  bar", "foo   bar");
    println!("\n=== whitespace_only: internal gap ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());

    // 4. whitespace_only - different internal gap count (per-gap values differ)
    let report = build_fuzzy_mismatch_report("foo  bar  baz", "foo bar baz");
    println!("\n=== whitespace_only: per-gap diff ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());

    // 5. whitespace_only - tab vs space (unspecified)
    let report = build_fuzzy_mismatch_report("foo\tbar", "foo bar");
    println!("\n=== whitespace_only: tab vs space (unspecified) ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());

    // 6. extra_line - different line counts (same tokens)
    let report = build_fuzzy_mismatch_report("foo bar\nbaz", "foo\nbar\nbaz");
    println!("\n=== whitespace_only: extra line ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

#[test]
fn test_fuzzy_vs_report_gap_analysis() {
    // Simulate the fuzzy matching regex to find mismatches
    // between what fuzzy CAN match vs what report CAN detect

    // CASE A: provided has tab, actual has spaces
    // Fuzzy regex: "foo\tbar" -> regex::escape -> "foo\tbar" (tab stays literal, NOT \s*)
    // So fuzzy would FAIL to match "foo  bar" (no mismatch report generated!)
    // This is a GAP: NO_REPORT because fuzzy itself fails
    let re = Regex::new(r"foo\tbar").unwrap();
    assert!(
        !re.is_match("foo  bar"),
        "Expected fuzzy match to FAIL (tab in provided, spaces in file)"
    );
    println!("\n=== CASE A: tab vs spaces -> fuzzy FAILs, NO report generated ===");

    // CASE B: provided has space, actual has tab
    // Fuzzy regex: "foo bar" -> regex::escape -> "foo" + "\s*" + "bar"
    // So fuzzy WOULD match "foo\tbar" (mismatch report IS generated)
    let re = Regex::new(r"foo\s*bar").unwrap();
    assert!(
        re.is_match("foo\tbar"),
        "Expected fuzzy match to SUCCEED (space in provided, tab in file)"
    );
    // What does the report say?
    let report = build_fuzzy_mismatch_report("foo bar", "foo\tbar");
    println!("\n=== CASE B: space vs tab -> fuzzy SUCCEEDS, report says ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    // split_whitespace splits both on space and tab -> tokens are ["foo", "bar"] == ["foo", "bar"]
    // So report says "whitespace_only" with "unspecified whitespace difference" (tab vs space)

    // CASE C: provided has newline, actual has spaces
    // Fuzzy regex: "foo\nbar" -> regex::escape -> "foo\nbar" (newline stays literal, NOT \s*)
    // So fuzzy would FAIL to match "foo  bar" (no mismatch report generated!)
    let re = Regex::new(r"foo\nbar").unwrap();
    assert!(
        !re.is_match("foo  bar"),
        "Expected fuzzy match to FAIL (newline in provided, spaces in file)"
    );
    println!("\n=== CASE C: newline vs spaces -> fuzzy FAILs, NO report generated ===");

    // CASE D: provided has space, actual has newline (cross-line match!)
    // Fuzzy regex: "foo bar" -> regex::escape -> "foo\s*bar"
    // \s* matches newline -> fuzzy WOULD match "foo\nbar"
    let re = Regex::new(r"foo\s*bar").unwrap();
    assert!(
        re.is_match("foo\nbar"),
        "Expected fuzzy match to SUCCEED (space in provided, newline in file)"
    );
    let report = build_fuzzy_mismatch_report("foo bar", "foo\nbar");
    println!("\n=== CASE D: space vs newline -> fuzzy SUCCEEDS (cross-line!), report says ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    // split_whitespace: ["foo", "bar"] == ["foo", "bar"] -> whitespace_only, but
    // provided has 1 line, actual has 2 lines -> extra_line + unspecified diff

    // CASE E: provided has extra internal whitespace run (double space vs single space)
    // Fuzzy regex: "foo  bar" -> regex::escape -> "foo  bar" -> replace space -> "foo\s*\s*bar"
    // (each space independently becomes \s*)
    // This still matches "foo bar" (single space), because \s* matches 0
    let re = Regex::new(r"foo\s*\s*bar").unwrap();
    assert!(
        re.is_match("foo bar"),
        "Expected fuzzy match to SUCCEED (double space in provided, single in file)"
    );
    let report = build_fuzzy_mismatch_report("foo  bar", "foo bar");
    println!("\n=== CASE E: double space vs single -> fuzzy SUCCEEDS, report says ===");
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    // split_whitespace: ["foo", "bar"] == ["foo", "bar"] -> whitespace_only
    // leading=0,0  trailing=0,0  internal_ws: provided="  " (2), actual=" " (1)

    // CASE F: provided has trailing newline, actual does not
    // Fuzzy regex: "foo bar\n" -> escape -> "foo bar\n" -> replace space -> "foo\s*bar\n"
    // The trailing \n is literal -> requires actual to end with newline
    let re = Regex::new(r"foo\s*bar\n").unwrap();
    assert!(
        !re.is_match("foo bar"),
        "Expected fuzzy match to FAIL (trailing newline in provided, none in file)"
    );
    println!("\n=== CASE F: trailing newline -> fuzzy FAILs, NO report generated ===");
}

#[test]
fn test_list_directory() {
    // Test directory listing for the project root
    let res = execute_list_directory(&json!({ "path": "." })).unwrap();
    let entries: Vec<&str> = res["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(entries.contains(&"src"));
    assert!(entries.contains(&"Cargo.toml") || entries.contains(&"Cargo.lock"));
}

#[tokio::test]
async fn test_grep_search() {
    // Search for a function definition within the project source
    let res =
        execute_grep_search(&json!({ "query": "pub fn get_tool_definitions", "path": "src" }))
            .await
            .unwrap();
    let matches: Vec<&str> = res["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert!(matches.iter().any(|p| p.contains("tools.rs")));
    assert!(res["total_matches"].as_u64().unwrap() > 0);
    assert_eq!(res["truncated"], false);
    assert!(
        res.get("output_bytes_omitted").is_none(),
        "normal results must not carry the omitted field"
    );
}

#[tokio::test]
async fn test_grep_search_large_output_reports_truncated() {
    // Backed by more than the default output cap (1 MiB; the test binary
    // never overrides ToolLimits) so the tail is kept and the result must
    // say truncated = true instead of silently dropping the head.
    let path = get_temp_path("grep_trunc");
    fs::create_dir_all(&path).unwrap();
    let data_file = path.join("data.txt");
    let mut big = String::with_capacity(1_300_000);
    for i in 0..30_000 {
        big.push_str(&format!("match_line_{:06}\n", i));
    }
    fs::write(&data_file, &big).unwrap();

    // Search a directory so grep emits `path:linenum:text` lines that the
    // tool parses deterministically.
    let res =
        execute_grep_search(&json!({ "query": "match_line_", "path": path.to_str().unwrap() }))
            .await
            .unwrap();
    assert_eq!(res["truncated"], true);
    let omitted = res["output_bytes_omitted"].as_u64().unwrap();
    assert!(omitted > 0, "omitted byte count must be reported");
    let matches = res["matches"].as_array().unwrap();
    assert!(
        !matches.is_empty(),
        "tail must still yield complete match lines"
    );

    fs::remove_dir_all(path).ok();
}

#[tokio::test]
async fn test_execute_bash_security() {
    // Test an allowed command from the whitelist
    let res = execute_bash(&json!({ "command": "echo 'test execution'" }))
        .await
        .unwrap();
    assert_eq!(res["exit_code"], 0);
    assert!(res["stdout"].as_str().unwrap().contains("test execution"));

    // Test a command blocked by the security whitelist
    let res = execute_bash(&json!({ "command": "rm -rf /tmp/some_non_existent_file" })).await;
    assert!(res.is_err());
    assert!(
        res.unwrap_err()
            .to_string()
            .contains("BASH_NOT_WHITELISTED")
    );
}

#[tokio::test]
async fn test_fetch_web_validation() {
    // Test invalid URL scheme
    let res = execute_fetch_web(&json!({ "url": "ftp://example.com" })).await;
    assert!(res.is_err());

    // Test private network access rejection
    let res = execute_fetch_web(&json!({ "url": "http://127.0.0.1/admin" })).await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("forbidden"));

    // Stage 1 SSRF: bracketed IPv6 (previously bypassed the naive host parse)
    let res = execute_fetch_web(&json!({ "url": "http://[::1]:8080/admin" })).await;
    let err = res.unwrap_err().to_string();
    assert!(err.contains("forbidden"), "got: {}", err);

    // Stage 1 SSRF: userinfo trick (previously bypassed the naive host parse)
    let res = execute_fetch_web(&json!({ "url": "http://evil@127.0.0.1/admin" })).await;
    assert!(res.is_err());

    // Stage 1 SSRF: decimal / hex / short IP notations (previously unparseable)
    let res = execute_fetch_web(&json!({ "url": "http://2130706433/admin" })).await;
    assert!(res.is_err());
    let res = execute_fetch_web(&json!({ "url": "http://0x7f.0.0.1/admin" })).await;
    assert!(res.is_err());
    let res = execute_fetch_web(&json!({ "url": "http://127.1/admin" })).await;
    assert!(res.is_err());

    // Stage 1 SSRF: IPv4-mapped IPv6 (::ffff:127.0.0.1 == 127.0.0.1) must
    // be rejected as IPv4, not slip through the IPv6 checks.
    let res = execute_fetch_web(&json!({ "url": "http://[::ffff:127.0.0.1]/admin" })).await;
    let err = res.unwrap_err().to_string();
    assert!(err.contains("forbidden"), "got: {}", err);
    let res = execute_fetch_web(&json!({ "url": "http://[::ffff:0a00:0001]/admin" })).await;
    let err = res.unwrap_err().to_string();
    assert!(err.contains("forbidden"), "got: {}", err);
}

#[tokio::test]
async fn test_execute_bash_env_scrubbed() {
    // A secret set in the agent's own environment must NOT reach the child.
    unsafe {
        std::env::set_var("AGT_TEST_SECRET_KEY", "hunter2");
    }

    let res = execute_bash(&json!({ "command": "echo $AGT_TEST_SECRET_KEY" }))
        .await
        .unwrap();
    let stdout = res["stdout"].as_str().unwrap();
    assert!(
        !stdout.contains("hunter2"),
        "secret leaked into bash env: {}",
        stdout
    );

    // Control: PATH is still passed through, so ordinary commands work.
    let res = execute_bash(&json!({ "command": "echo ok" }))
        .await
        .unwrap();
    assert_eq!(res["exit_code"], 0);
    assert!(res["stdout"].as_str().unwrap().contains("ok"));
    assert_eq!(res["signal"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_execute_bash_output_capped() {
    // Generate a large output file in the workspace (tmp/), then cat it:
    // the tool result must be bounded and keep the tail.
    let path = get_temp_path("big_output");
    let big = "x".repeat(200_000);
    fs::write(&path, &big).unwrap();

    let res = execute_bash(&json!({ "command": format!("cat {}", path.display()) }))
        .await
        .unwrap();
    let stdout = res["stdout"].as_str().unwrap();
    assert!(
        stdout.contains("[... Output truncated ...]"),
        "expected truncation marker, got {} bytes",
        stdout.len()
    );
    assert!(stdout.ends_with("xxx"), "tail must be kept");
    assert!(stdout.len() <= 4096, "visible output must stay bounded");

    fs::remove_file(path).ok();
}

#[cfg(unix)]
#[test]
fn test_validate_path_symlink_escape() {
    use std::os::unix::fs::symlink;

    // Scratch dir under ./tmp; the workspace root is the package root (see
    // get_temp_path), so the outside-pointing symlink must be rejected while
    // inside-pointing ones stay allowed.
    let ws = get_temp_path("ws");
    fs::create_dir_all(&ws).unwrap();

    // A symlink inside the workspace pointing outside must be rejected.
    let link = ws.join("evil_link");
    symlink("/etc/passwd", &link).unwrap();
    let link_str = link.to_str().unwrap();
    assert!(validate_path(link_str).is_err());

    // A symlink pointing inside the workspace is allowed.
    let target = ws.join("real.txt");
    fs::write(&target, "data").unwrap();
    let inner_link = ws.join("inner_link");
    symlink(&target, &inner_link).unwrap();
    assert!(validate_path(inner_link.to_str().unwrap()).is_ok());

    // Regular paths: existing file and a not-yet-existing subpath both pass.
    assert!(validate_path(target.to_str().unwrap()).is_ok());
    assert!(validate_path(ws.join("new_dir/file.txt").to_str().unwrap()).is_ok());

    // The same escape through the dispatch path (execute_tool).
    let blocked = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(execute_tool(
            "read_file",
            &json!({ "path": link_str }),
            None,
            None,
            |_| true,
        ));
    assert!(blocked.is_err());
    assert!(
        blocked
            .unwrap_err()
            .to_string()
            .contains("SECURITY_VIOLATION")
    );

    fs::remove_file(&link).ok();
    fs::remove_file(&target).ok();
    fs::remove_file(&inner_link).ok();
    fs::remove_dir_all(&ws).ok();
}

#[test]
fn test_strip_html_tags() {
    let html =
        "<html><body><h1>Title</h1><p>Paragraph with <a href='#'>link</a>.</p></body></html>";
    let plain = strip_html_tags(html).replace("\n", ""); // remove newlines for testing
    // Verify links are converted to markdown
    assert!(plain.contains("[link](#)"));

    let complex = "<script>alert(1)</script>  <style>body{}</style>Text";
    assert_eq!(strip_html_tags(complex).trim(), "Text");
}

// -------------------------------------------------------------------------
// Stage 3.5 / 4.5: skip_blank integration tests
// -------------------------------------------------------------------------

#[test]
fn test_tab_skip_blank_replace_old_has_extra_blank() {
    // bug01.md scenario: old_string has an extra blank line that file doesn't have
    let path = get_temp_path("tab_skip_blank_extra");
    let path_str = path.to_str().unwrap();
    fs::write(&path, "fn hello() {\n    foo();\n}\n").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn hello() {\n    foo();\n\n}",  // extra blank line
        "new_string": "fn hello() {\n    bar();\n}"
    }));
    assert!(
        res.is_ok(),
        "Stage 3.5 should match despite extra blank: {:?}",
        res.err()
    );
    let match_type = res.unwrap()["match_type"].as_str().unwrap().to_string();
    assert!(
        match_type.contains("blank-line tolerant"),
        "match_type should indicate blank-line tolerance: {}",
        match_type
    );
    let content = fs::read_to_string(path_str).unwrap();
    assert!(
        content.contains("bar();"),
        "Replacement should have occurred"
    );
    fs::remove_file(path).ok();
}

#[test]
fn test_tab_skip_blank_replace_file_has_extra_blank() {
    // Reverse: file has blank line, old_string doesn't
    let path = get_temp_path("tab_skip_blank_file_extra");
    let path_str = path.to_str().unwrap();
    fs::write(&path, "fn hello() {\n    foo();\n\n}\n").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn hello() {\n    foo();\n}",  // no blank line
        "new_string": "fn hello() {\n    bar();\n}"
    }));
    assert!(
        res.is_ok(),
        "Stage 3.5 should match despite file having extra blank: {:?}",
        res.err()
    );
    let match_type = res.unwrap()["match_type"].as_str().unwrap().to_string();
    assert!(
        match_type.contains("blank-line tolerant"),
        "match_type should indicate blank-line tolerance: {}",
        match_type
    );
    let content = fs::read_to_string(path_str).unwrap();
    assert!(
        content.contains("bar();"),
        "Replacement should have occurred"
    );
    fs::remove_file(path).ok();
}

#[test]
fn test_tab_skip_blank_with_mixed_whitespace_indent() {
    // Tab-indented file, spaces in old_string, plus extra blank line
    let path = get_temp_path("tab_skip_blank_mixed");
    let path_str = path.to_str().unwrap();
    fs::write(&path, "fn hello() {\n\tfoo();\n}\n").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn hello() {\n    foo();\n\n}",  // space indent + extra blank
        "new_string": "fn hello() {\n\tbar();\n}"
    }));
    assert!(
        res.is_ok(),
        "Stage 3.5 should match with tab/space + blank: {:?}",
        res.err()
    );
    let match_type = res.unwrap()["match_type"].as_str().unwrap().to_string();
    assert!(
        match_type.contains("blank-line tolerant"),
        "match_type should indicate blank-line tolerance: {}",
        match_type
    );
    let content = fs::read_to_string(path_str).unwrap();
    assert!(
        content.contains("bar();"),
        "Replacement should have occurred"
    );
    fs::remove_file(path).ok();
}

#[test]
fn test_get_tool_definitions_filters_disabled() {
    let defs = get_tool_definitions(None, |n| n != "execute_bash" && n != "fetch_web");
    let names: Vec<&str> = defs
        .iter()
        .map(|d| d["function"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"write_file"));
    assert!(!names.contains(&"execute_bash"));
    assert!(!names.contains(&"fetch_web"));
}

#[test]
fn test_get_tool_definitions_only_data_tools_with_db_type() {
    let defs = get_tool_definitions(Some("greptimedb"), |n| {
        n == "data_search" || n == "data_schema"
    });
    let names: Vec<&str> = defs
        .iter()
        .map(|d| d["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["data_search", "data_schema"]);
}

#[tokio::test]
async fn test_execute_tool_rejects_disabled() {
    let res = execute_tool(
        "execute_bash",
        &json!({ "command": "ls" }),
        None,
        None,
        |_| false,
    )
    .await;
    let err = res.unwrap_err().to_string();
    assert!(
        err.contains("[TOOL_DISABLED]"),
        "expected [TOOL_DISABLED] error, got: {}",
        err
    );
}

#[tokio::test]
async fn test_confirm_execute_tool_rejects_disabled_without_prompt() {
    // Batch mode + disabled tool: must be rejected as a system error
    // before any stdin interaction or auto-confirm logic runs.
    let decision = confirm_execute_tool(
        "execute_bash",
        &json!({ "command": "ls" }),
        true, // unsafe_reflex would normally auto-confirm -- must NOT apply
        false,
        true, // batch
        |_| false,
    )
    .await;
    assert!(!decision.proceed);
    assert_eq!(decision.kind, ToolRunDecisionKind::SystemError);
    let reason = decision.reason.unwrap();
    assert!(
        reason.contains("[TOOL_DISABLED]"),
        "expected [TOOL_DISABLED] reason, got: {}",
        reason
    );
}

#[tokio::test]
async fn test_confirm_execute_tool_calc_follows_reflex_gate() {
    // calc must NEVER auto-run without a reflex flag (like every tool).
    // Batch mode (no stdin) denies instead of executing.
    let decision = confirm_execute_tool(
        "calc",
        &json!({ "expressions": ["1 + 1"] }),
        false, // unsafe_reflex off
        false, // db_unsafe_reflex off (does not apply to calc)
        true,  // batch
        |_| true,
    )
    .await;
    assert!(!decision.proceed, "calc must not auto-run without reflex");
    assert_eq!(decision.kind, ToolRunDecisionKind::SystemError);

    // --db-unsafe-reflex alone must NOT auto-confirm calc (db-only flag).
    let decision = confirm_execute_tool(
        "calc",
        &json!({ "expressions": ["1 + 1"] }),
        false,
        true,
        true,
        |_| true,
    )
    .await;
    assert!(!decision.proceed, "db_unsafe_reflex must not gate calc");

    // Under --unsafe-reflex calc auto-confirms (side-effect-free policy).
    let decision = confirm_execute_tool(
        "calc",
        &json!({ "expressions": ["1 + 1"] }),
        true,
        false,
        true,
        |_| true,
    )
    .await;
    assert!(decision.proceed);
    assert_eq!(decision.kind, ToolRunDecisionKind::AutoConfirm);
}

#[tokio::test]
async fn test_confirm_execute_tool_no_reflex_no_auto_run_for_any_tool() {
    // Reflex gates off: NO tool may auto-confirm. Batch mode must deny every
    // tool (SystemError) instead of running it without y/N.
    let cases: &[(&str, serde_json::Value)] = &[
        ("read_file", json!({ "path": "x" })),
        ("write_file", json!({ "path": "x", "content": "y" })),
        (
            "str_replace_editor",
            json!({ "path": "x", "old_string": "a", "new_string": "b" }),
        ),
        ("grep_search", json!({ "query": "q" })),
        ("list_directory", json!({ "path": "." })),
        ("execute_bash", json!({ "command": "ls" })),
        ("fetch_web", json!({ "url": "https://example.com" })),
        ("data_search", json!({ "query": "SELECT 1" })),
        ("data_schema", json!({})),
        ("calc", json!({ "expressions": ["1 + 1"] })),
    ];
    for (name, args) in cases {
        let decision = confirm_execute_tool(
            name,
            args,
            false, // unsafe_reflex off
            false, // db_unsafe_reflex off
            true,  // batch: no y/N available -> must deny, never execute
            |_| true,
        )
        .await;
        assert!(
            !decision.proceed,
            "tool '{}' must not auto-run without any reflex flag",
            name
        );
        assert_eq!(
            decision.kind,
            ToolRunDecisionKind::SystemError,
            "tool '{}'",
            name
        );
    }
}

#[test]
fn test_full_skip_blank_replace() {
    // Scenario where blank line tolerance rescues a match that
    // full_fuzzy (Stage 4) would fail on due to extra blank line.
    let path = get_temp_path("full_skip_blank_replace");
    let path_str = path.to_str().unwrap();
    fs::write(&path, "fn hello() {\n    foo();\n}\n").unwrap();
    // old has extra blank line (file doesn't) - Stage 4 (full_fuzzy) fails
    // because `\n\n` are literal in its pattern; Stage 3.5 or 4.5 should catch it.
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn hello() {\n    foo();\n\n}",
        "new_string": "fn hello() {\n    bar();\n}"
    }));
    assert!(res.is_ok(), "Skip-blank should match: {:?}", res.err());
    let match_type = res.as_ref().unwrap()["match_type"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        match_type.contains("blank-line tolerant"),
        "match_type should indicate blank-line tolerance: {}",
        match_type
    );
    let content = fs::read_to_string(path_str).unwrap();
    assert!(
        content.contains("bar();"),
        "Replacement should have occurred"
    );
    fs::remove_file(path).ok();
}

#[test]
fn test_skip_blank_ambiguous_match() {
    // Multiple matches with blank-line tolerance should still error
    let path = get_temp_path("skip_blank_ambiguous");
    let path_str = path.to_str().unwrap();
    fs::write(&path, "fn foo() {}\n\nfn foo() {}\n").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn foo() {}\n",
        "new_string": "fixed();"
    }));
    assert!(res.is_err(), "Expected error for ambiguous match");
    assert!(
        res.unwrap_err().to_string().contains("AMBIGUOUS_MATCH"),
        "Should report AMBIGUOUS_MATCH"
    );
    fs::remove_file(path).ok();
}

#[test]
fn test_skip_blank_no_match_on_different_content() {
    // Safety: differing non-blank content must NOT match
    let path = get_temp_path("skip_blank_no_match");
    let path_str = path.to_str().unwrap();
    fs::write(&path, "fn hello() {\n    foo();\n}\n").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "fn goodbye() {\n    foo();\n}",
        "new_string": "fn replaced() {}"
    }));
    assert!(res.is_err(), "Expected NO_MATCH for different content");
    assert!(
        res.unwrap_err().to_string().contains("NO_MATCH"),
        "Should report NO_MATCH"
    );
    fs::remove_file(path).ok();
}

#[test]
fn test_skip_blank_stage_order() {
    // Verify that skip_blank stages fire before/after parents correctly.
    // Stage 3.5 should be attempted BEFORE Stage 4 (full_fuzzy).
    // If Stage 3.5 is skipped (empty pattern), it should fall through.
    //
    // Case: no blank lines -> tab_skip_blank pattern is non-empty
    //       (just joins lines with the connector which tolerates blanks),
    //       but Stage 3 (tab) should match first for simpler content.
    let path = get_temp_path("skip_blank_order");
    let path_str = path.to_str().unwrap();
    // Simple content that Stage 3 (tab) should match perfectly
    fs::write(&path, "let x = 1;\n").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "let x = 1;",
        "new_string": "let y = 2;"
    }));
    assert!(res.is_ok());
    // Should be exact match, not skip_blank
    let match_type = res.unwrap()["match_type"].as_str().unwrap().to_string();
    assert_eq!(
        match_type, "Perfect match.",
        "Simple content should be exact match: {}",
        match_type
    );
    fs::remove_file(path).ok();
}

#[test]
fn test_skip_blank_empty_old_string() {
    // old_string with only blank lines -> pattern is empty -> not compiled
    // Should fall through to NO_MATCH
    let path = get_temp_path("skip_blank_empty");
    let path_str = path.to_str().unwrap();
    fs::write(&path, "some content\n").unwrap();
    let res = execute_str_replace(&json!({
        "path": path_str,
        "old_string": "\n  \n\t\n",
        "new_string": "replacement"
    }));
    assert!(res.is_err(), "Expected NO_MATCH for blank-only old_string");
    assert!(
        res.unwrap_err().to_string().contains("NO_MATCH"),
        "Should report NO_MATCH"
    );
    fs::remove_file(path).ok();
}
