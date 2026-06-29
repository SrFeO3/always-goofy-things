//! Unsafe reflex
//! Automatically determines if a tool call can bypass manual confirmation.

const AUTO_CONFIRM_STRICT_COMMAND_LIST: &[&str] = &["cargo check", "cargo check 2>&1", "cargo fmt"];

/// Automatically determines if a tool call can bypass manual confirmation.
///
/// Returns `true` if the tool is allowed and its path-based arguments pass
/// the subpath constraint checks. Returns `false` otherwise.
pub fn auto_confirm(name: &str, args: &serde_json::Value) -> (bool, Option<String>) {
    match name {
        "read_file" => {
            let obj = match args.as_object() {
                Some(o) => o,
                None => return (false, None),
            };
            let path = match obj.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return (false, None),
            };

            if is_safe_subpath(&path) {
                (true, Some(format!("A reasonably peaceful path: {}", path)))
            } else {
                (false, None)
            }
        }
        "write_file" => {
            let obj = match args.as_object() {
                Some(o) => o,
                None => return (false, None),
            };
            let path = match obj.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return (false, None),
            };

            if is_safe_subpath(&path) {
                (true, Some(format!("A reasonably peaceful path: {}", path)))
            } else {
                (false, None)
            }
        }
        "str_replace_editor" => {
            let obj = match args.as_object() {
                Some(o) => o,
                None => return (false, None),
            };
            let path = match obj.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return (false, None),
            };

            if is_safe_subpath(&path) {
                (true, Some(format!("A reasonably peaceful path: {}", path)))
            } else {
                (false, None)
            }
        }
        "grep_search" => {
            let obj = match args.as_object() {
                Some(o) => o,
                None => return (false, None),
            };

            let query = match obj.get("query").and_then(|v| v.as_str()) {
                Some(q) => q.to_string(),
                None => return (false, None),
            };

            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                if is_safe_grep_query(&query) && is_safe_subpath(&path) {
                    (
                        true,
                        Some(format!(
                            "A reasonably quiet query along a peaceful path (path: '{}', query: '{}')",
                            query, path
                        )),
                    )
                } else {
                    (false, None)
                }
            } else {
                if is_safe_grep_query(&query) {
                    (true, Some(format!("A reasonably quiet query: {}", query)))
                } else {
                    (false, None)
                }
            }
        }
        "list_directory" => {
            let obj = match args.as_object() {
                Some(o) => o,
                None => return (false, None),
            };
            let path = match obj.get("path").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => return (false, None),
            };

            if is_safe_subpath(&path) {
                (true, Some(format!("A reasonably peaceful path: {}", path)))
            } else {
                (false, None)
            }
        }
        "execute_bash" => {
            let obj = match args.as_object() {
                Some(o) => o,
                None => return (false, None),
            };
            let command = match obj.get("command").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => return (false, None),
            };

            if AUTO_CONFIRM_STRICT_COMMAND_LIST.contains(&command) {
                (
                    true,
                    Some(format!("A reasonably polite command: {}", command)),
                )
            } else {
                (false, None)
            }
        }
        "fetch_web" => (false, None),
        _ => (false, None),
    }
}

fn is_safe_grep_query(query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    query
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ' ')
}

/// Validates a safe subpath by restricting input to ASCII alphanumerics, `_`, `-`, `/`, `.`, and an optional leading `./`.
/// This is a basic, restrictive heuristic to prevent directory traversal by explicitly disallowing `..` and certain segment patterns.
fn is_safe_subpath(mut path_str: &str) -> bool {
    if path_str == "." || path_str == "./" {
        return true;
    }

    if path_str.starts_with("./") {
        path_str = &path_str[2..];
    }

    if path_str.ends_with("\\\\(") {
        path_str = &path_str[..3];
    }

    if path_str.is_empty()
        || path_str.starts_with('/')
        || path_str.ends_with('/')
        || path_str.contains("//")
        || path_str.contains("..")
    {
        return false;
    }

    path_str
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '.')
}
