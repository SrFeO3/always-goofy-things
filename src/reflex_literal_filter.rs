//! Shallow literal-matching command filter.
//!
//! Provides an explainability-focused command filter based on literal string comparisons.
//! By restricting verification to exact string matching instead of full Abstract Syntax
//! Tree (AST) parsing, the logic guarantees predictable, human-auditable execution paths.
//!
//! # Limitations
//!
//! This filter serves as a preliminary heuristic check and does not constitute a robust,
//! standalone security boundary.

/// Validates a safe grep query string by restricting input to ASCII alphanumerics, `_`, `-`, ` `
pub fn is_safe_grep_query(query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    query
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ' ')
}

/// Validates a safe subpath by restricting input to ASCII alphanumerics, `_`, `-`, `/`, `.`, and an optional leading `./`.
/// This is a basic, restrictive heuristic to prevent directory traversal by explicitly disallowing `..` and certain segment patterns.
pub fn is_safe_subpath(mut path_str: &str) -> bool {
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

const REDIRECT_SUFFIXES: &[&str] = &[
    " > /dev/null 2>&1",
    " 2>&1",
    " > /dev/null",
    " 2> /dev/null",
    " >/dev/null",
];

/// Check if the input is an exactly-matched command allowed to bypass.
pub fn is_exact_matched_command(cmd: &str) -> bool {
    // Strip a safe redirect suffix from the end of the command if present.
    let mut input = cmd.trim_end();
    if let Some(stripped) = REDIRECT_SUFFIXES.iter().find_map(|s| input.strip_suffix(s)) {
        input = stripped;
    }

    const STRICT_COMMAND_LIST: &[&str] = &["cargo check", "cargo fmt", "cargo clippy"];

    STRICT_COMMAND_LIST.contains(&input)
}

/// Check if the input is a literal-matched command allowed to bypass.
pub fn is_shallow_matched_command(cmd: &str) -> bool {
    // Strip a safe redirect suffix from the end of the command if present.
    let mut input = cmd.trim_end();
    if let Some(stripped) = REDIRECT_SUFFIXES.iter().find_map(|s| input.strip_suffix(s)) {
        input = stripped;
    }

    // Strips allowed suffixes from the end of a command string
    const READ_CMD_SUFFIXES: &[&str] = &[
        " | cat -A",
        " | wc -l",
        " | wc -c",
        " | sort",
        " | sort -n",
        " | uniq",
    ];
    loop {
        input = input.trim_end();
        let next_query = READ_CMD_SUFFIXES
            .iter()
            .find_map(|suffix| input.strip_suffix(suffix));
        if let Some(stripped) = next_query {
            input = stripped;
        } else {
            break;
        }
    }

    is_basic_read_command(input) || is_head_tail_command(input)
}

/// A single-path read command (e.g., "cat path").
fn is_basic_read_command(input: &str) -> bool {
    const READ_CMD: &[&str] = &[
        "cat ",
        "nl ",
        "file ",
        "stat ",
        "md5sum ",
        "sha256sum ",
        "wc -l ",
        "wc -c ",
    ];

    for cmd in READ_CMD {
        if let Some(remaining) = input.strip_prefix(cmd) {
            let path = remaining.trim();
            if path.is_empty() {
                return false;
            }
            return is_safe_subpath(path);
        }
    }

    false
}

/// A single-path head/tail command (e.g., "head -n 10 path | tail -n 3").
fn is_head_tail_command(input: &str) -> bool {
    // head+tail or head only
    if let Some(after_head) = input.strip_prefix("head -n ") {
        let Some((num_str, remaining)) = after_head.split_once(' ') else {
            return false;
        };
        if num_str.parse::<usize>().is_err() {
            return false;
        }

        if let Some((target_and_path, tail_num_str)) = remaining.rsplit_once(" | tail -n ") {
            // head + tail
            if tail_num_str.parse::<usize>().is_err() {
                return false;
            }
            return is_safe_subpath(target_and_path.trim());
        } else {
            // head only
            return is_safe_subpath(remaining.trim());
        }
    }

    // tail only
    if let Some(after_tail) = input.strip_prefix("tail -n ") {
        let Some((num_str, remaining)) = after_tail.split_once(' ') else {
            return false;
        };
        if num_str.parse::<usize>().is_err() {
            return false;
        }
        return is_safe_subpath(remaining.trim());
    }

    false
}
