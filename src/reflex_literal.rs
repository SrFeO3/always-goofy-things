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

/// Validates a safe url string
pub fn is_safe_url(url: &str) -> bool {
    const ALLOWED_PREFIXES: &[&str] = &[
        // Core / Async / Error handling
        "https://docs.rs/anyhow/",
        "https://docs.rs/arc-swap/",
        "https://docs.rs/async-trait/",
        "https://docs.rs/chrono/",
        "https://docs.rs/clap/",
        "https://docs.rs/directories/",
        "https://docs.rs/futures/",
        "https://docs.rs/futures-util/",
        "https://docs.rs/lazy_static/",
        "https://docs.rs/log/",
        "https://docs.rs/once_cell/",
        "https://docs.rs/pollster/",
        "https://docs.rs/regex/",
        "https://docs.rs/thiserror/",
        "https://docs.rs/tokio/",
        "https://docs.rs/tracing/",
        "https://docs.rs/tracing-subscriber/",
        // Networking / Web / Serialization
        "https://docs.rs/bytes/",
        "https://docs.rs/http/",
        "https://docs.rs/hyper/",
        "https://docs.rs/postcard/",
        "https://docs.rs/quinn/",
        "https://docs.rs/reqwest/",
        "https://docs.rs/serde/",
        "https://docs.rs/serde_json/",
        "https://docs.rs/serde_yaml/",
        "https://docs.rs/url/",
        // Security / TLS / Certificates
        "https://docs.rs/rustls/",
        "https://docs.rs/webpki-roots/",
        "https://docs.rs/x509-parser/",
        // Containers / Kubernetes
        "https://docs.rs/bollard/",
        "https://docs.rs/k8s-openapi/",
        "https://docs.rs/kube/",
        // GUI / Windowing / OS integration
        "https://docs.rs/arboard/",
        "https://docs.rs/cursor-icon/",
        "https://docs.rs/eframe/",
        "https://docs.rs/egui/",
        "https://docs.rs/muda/",
        "https://docs.rs/rfd/",
        "https://docs.rs/rustyline/",
        "https://docs.rs/winit/",
        // Graphics / WGPU
        "https://docs.rs/bytemuck/",
        "https://docs.rs/glyphon/",
        "https://docs.rs/wgpu/",
        // Text rendering / Manipulation
        "https://docs.rs/cosmic-text/",
        "https://docs.rs/ropey/",
        "https://docs.rs/unicode-segmentation/",
        "https://docs.rs/unicode-width/",
        // Data structures
        "https://docs.rs/fixedbitset/",
        "https://docs.rs/lru/",
        "https://docs.rs/petgraph/",
        "https://docs.rs/slotmap/",
    ];

    if url.contains("..") {
        return false;
    }

    ALLOWED_PREFIXES.iter().any(|prefix| {
        if url.starts_with(prefix) {
            let remaining = &url[prefix.len()..];
            remaining
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        } else {
            false
        }
    })
}

/// Validates a safe grep query string by restricting input to ASCII alphanumerics, `_`, `-`, ` `
pub fn is_safe_grep_query(query: &str) -> bool {
    if query.is_empty() {
        return false;
    }

    let mut query = query;

    if let Some(stripped) = query.strip_prefix('^') {
        query = stripped;
    }

    if let Some(stripped) = query.strip_suffix('$') {
        query = stripped;
    }

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

/// A single-path head/tail command (e.g., "head -n 10 path | tail -n 3", "head -10").
fn is_head_tail_command(input: &str) -> bool {
    // head+tail or head only
    if let Some(after_head) = input
        .strip_prefix("head -n ")
        .or_else(|| input.strip_prefix("head -"))
    {
        let Some((num_str, remaining)) = after_head.split_once(' ') else {
            return false;
        };
        if num_str.parse::<usize>().is_err() {
            return false;
        }

        if let Some((target_and_path, tail_num_str)) = remaining
            .rsplit_once(" | tail -n ")
            .or_else(|| remaining.rsplit_once(" | tail -"))
        {
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
    if let Some(after_tail) = input
        .strip_prefix("tail -n ")
        .or_else(|| input.strip_prefix("tail -"))
    {
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
