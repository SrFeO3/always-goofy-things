//! Pretty UI rendering for database query tools.
//!
//! Provides a human-readable CLI display of SQL queries and CSV results
//! during data tool execution.
//!
//! # Pretty-Printed Data Tools
//!
//! - `data_search`: Execute a read-only SQL query against a remote database.
//!     - Preview: Show the SQL query to be executed (1 line)
//!     - Success: Compact table with headers, column-aligned, up to 16 rows (multi-line)
//!     - Error: Error reason (1 line)
//! - `data_schema`: List tables or describe a specific table's columns.
//!     - Preview: Show the schema query to be executed (1 line)
//!     - Success: Compact table with headers (multi-line)
//!     - Error: Error reason (1 line)

use serde_json::Value;

use crate::startup::{BG_GRAY, C_GRAY, C_YELLOW, RESET};

/// Maximum column width in characters.
const MAX_COL_WIDTH: usize = 16;

/// Maximum columns displayed before truncating the rest.
const MAX_COLS: usize = 8;

/// Maximum table rows displayed before truncating the middle.
const MAX_ROWS: usize = 16;

/// How many rows to show at the top and bottom when truncating.
const HEAD_TAIL_ROWS: usize = 7;

/// Pretty-print the SQL query about to be executed.
pub(crate) fn pretty_print_data_command(_name: &str, args: &Value) {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return,
    };
    println!("-- Query: {}{}{}", C_YELLOW, query, RESET);
}

/// Parse a CSV line into fields, handling quoted fields with embedded
/// commas and escaped double-quotes ("").
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if in_quotes {
            if ch == '"' {
                if i + 1 < chars.len() && chars[i + 1] == '"' {
                    current.push('"');
                    i += 1;
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(ch);
            }
        } else if ch == '"' {
            in_quotes = true;
        } else if ch == ',' {
            fields.push(current);
            current = String::new();
        } else {
            current.push(ch);
        }
        i += 1;
    }
    fields.push(current);
    fields
}

/// Parse multi-line CSV body into rows, respecting quoted fields that
/// contain embedded newlines.
fn parse_csv_rows(body: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if in_quotes {
            if ch == '"' {
                if i + 1 < chars.len() && chars[i + 1] == '"' {
                    current.push('"');
                    i += 1;
                } else {
                    in_quotes = false;
                }
            }
            current.push(ch);
        } else if ch == '"' {
            in_quotes = true;
            current.push(ch);
        } else if ch == '\n' {
            rows.push(parse_csv_line(&current));
            current = String::new();
        } else {
            current.push(ch);
        }
        i += 1;
    }
    // Don't forget trailing row without newline
    if !current.is_empty() {
        rows.push(parse_csv_line(&current));
    }
    rows
}

/// Pretty-print a CSV result as a compact, column-aligned table.
pub(crate) fn pretty_print_data_result(result: &Value) {
    let obj = match result.as_object() {
        Some(o) => o,
        None => {
            println!("{}Result:{} {}", C_GRAY, RESET, result);
            return;
        }
    };

    let content = match obj.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            println!("{}Result:{} {}", C_GRAY, RESET, result);
            return;
        }
    };

    // Separate truncation notice from the body
    let (body, truncated_notice) = if let Some(pos) = content.find("[DB_TRUNCATED]") {
        (&content[..pos], Some(content[pos..].trim()))
    } else {
        (content, None)
    };

    let body = body.trim();
    if body.is_empty() {
        if let Some(notice) = truncated_notice {
            println!("{}{}{}", C_GRAY, notice, RESET);
        }
        return;
    }

    let mut rows_data = parse_csv_rows(body);
    if rows_data.is_empty() {
        return;
    }

    let headers = rows_data.remove(0);
    let rows = rows_data;

    if headers.is_empty() {
        println!("{}", body);
        if let Some(notice) = truncated_notice {
            println!("{}{}{}", C_GRAY, notice, RESET);
        }
        return;
    }

    // Compute column widths, capped at MAX_COL_WIDTH
    let col_count = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len().min(MAX_COL_WIDTH)).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i < col_count {
                widths[i] = widths[i].max(cell.len().min(MAX_COL_WIDTH));
            }
        }
    }

    // Limit visible columns
    let col_omitted = col_count.saturating_sub(MAX_COLS);
    let visible_headers: Vec<&str> = headers.iter().take(MAX_COLS).map(|s| s.as_str()).collect();
    let visible_widths: Vec<usize> = widths.iter().take(MAX_COLS).copied().collect();

    if col_omitted > 0 {
        println!(
            "{}{}  ... {} column{} omitted ...{}",
            BG_GRAY,
            C_GRAY,
            col_omitted,
            if col_omitted != 1 { "s" } else { "" },
            RESET
        );
    }

    let format_row = |cells: &[&str], widths_slice: &[usize]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let w = widths_slice.get(i).copied().unwrap_or(0);
                let truncated: String = c.chars().take(w).collect();
                format!("{:width$}", truncated, width = w)
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };

    let total = rows.len();

    println!(
        "{}{}{}",
        BG_GRAY,
        format_row(&visible_headers, &visible_widths),
        RESET
    );
    let sep: Vec<String> = visible_widths.iter().map(|&w| "-".repeat(w)).collect();
    println!("{}{}{}", BG_GRAY, sep.join("-+-"), RESET);

    if total <= MAX_ROWS {
        for row in &rows {
            let cells: Vec<&str> = row.iter().take(MAX_COLS).map(|s| s.as_str()).collect();
            println!(
                "{}{}{}",
                BG_GRAY,
                format_row(&cells, &visible_widths),
                RESET
            );
        }
    } else {
        for row in rows.iter().take(HEAD_TAIL_ROWS) {
            let cells: Vec<&str> = row.iter().take(MAX_COLS).map(|s| s.as_str()).collect();
            println!(
                "{}{}{}",
                BG_GRAY,
                format_row(&cells, &visible_widths),
                RESET
            );
        }
        let omitted = total.saturating_sub(HEAD_TAIL_ROWS * 2);
        println!(
            "{}{}  ... {} row{} omitted ...{}",
            BG_GRAY,
            C_GRAY,
            omitted,
            if omitted != 1 { "s" } else { "" },
            RESET
        );
        for row in rows.iter().skip(total.saturating_sub(HEAD_TAIL_ROWS)) {
            let cells: Vec<&str> = row.iter().take(MAX_COLS).map(|s| s.as_str()).collect();
            println!(
                "{}{}{}",
                BG_GRAY,
                format_row(&cells, &visible_widths),
                RESET
            );
        }
    }

    print!(
        "{}[{} col{} x {} row{}]{}",
        C_GRAY,
        col_count,
        if col_count != 1 { "s" } else { "" },
        total,
        if total != 1 { "s" } else { "" },
        RESET
    );
    if let Some(notice) = truncated_notice {
        print!(" -- {}", notice);
    }
    println!();
}
