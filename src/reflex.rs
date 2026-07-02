//! Unsafe reflex bypass for tool execution.
//!
//! Automatically determines if a tool call can bypass manual confirmation
//! based on predefined execution policies.

use crate::reflex_literal_filter::is_exact_matched_command;
use crate::reflex_literal_filter::is_safe_grep_query;
use crate::reflex_literal_filter::is_safe_subpath;
use crate::reflex_literal_filter::is_shallow_matched_command;

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

            if is_exact_matched_command(&command) {
                (
                    true,
                    Some(format!("A reasonably polite command: {}", command)),
                )
            } else if is_shallow_matched_command(&command) {
                (
                    true,
                    Some(format!("A reasonably familiar pattern: {}", command)),
                )
            } else {
                (false, None)
            }
        }
        "fetch_web" => (false, None),
        _ => (false, None),
    }
}
