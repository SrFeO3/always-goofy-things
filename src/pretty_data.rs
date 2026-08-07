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

/// Maximum columns displayed before truncating the rest.
const MAX_COLS: usize = 8;

/// Maximum table rows displayed before truncating the middle.
const MAX_ROWS: usize = 16;

/// How many rows to show at the top and bottom when truncating.
const HEAD_TAIL_ROWS: usize = 7;

/// Maximum per-column width when columns are few and terminal is wide.
const MAX_COL_WIDTH: usize = 50;

/// Minimum per-column width.
const MIN_COL_WIDTH: usize = 5;

/// Default terminal width when detection fails.
const DEFAULT_TERM_WIDTH: usize = 80;

/// Detect the current terminal width in columns.
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&w| w > 0)
        .unwrap_or(DEFAULT_TERM_WIDTH)
}

/// Pretty-print the SQL query about to be executed.
pub(crate) fn pretty_print_data_command(_name: &str, args: &Value) {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return,
    };
    println!("-- Query: {}{}{}", C_YELLOW, query, RESET);
}

/// Parse a CSV line into fields, handling quoted fields with embedded
/// commas and escaped double-quotes ("").  Strips stray `\r` characters
/// that may appear from Windows-style line endings.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        // silently skip stray carriage-return bytes
        if ch == '\r' {
            i += 1;
            continue;
        }
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
/// contain embedded newlines.  Treats both `\n` and `\r\n` as row
/// separators so that Windows-style CSV is handled correctly.
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
        } else if ch == '\r' {
            // \r (standalone or part of \r\n) ends the row when outside quotes.
            rows.push(parse_csv_line(&current));
            current = String::new();
            // If the next char is \n, skip it too.
            if i + 1 < chars.len() && chars[i + 1] == '\n' {
                i += 1;
            }
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

    // Compute column widths dynamically based on terminal width.
    let term_w = terminal_width();
    let col_count = headers.len();

    // Limit visible columns.
    let col_omitted = col_count.saturating_sub(MAX_COLS);
    let visible_cols = col_count.min(MAX_COLS);

    if col_omitted > 0 {
        println!(
            "{}  ... {} column{} omitted ...{}",
            C_GRAY,
            col_omitted,
            if col_omitted != 1 { "s" } else { "" },
            RESET
        );
    }

    // Border overhead: `| c1 | c2 |` = 3 * n + 1 chars for n columns.
    let overhead = 3 * visible_cols + 1;
    let available = term_w.saturating_sub(overhead);
    let equal_w = available
        .checked_div(visible_cols)
        .map(|w| w.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH))
        .unwrap_or(0);
    let mut widths: Vec<usize> = vec![equal_w; visible_cols];

    if col_omitted > 0 {
        // Allocate a small "..." column at the end.
        widths.push(3);
    }

    // Truncate a cell value to fit, appending `~` when truncated.
    let truncate_cell = |s: &str, w: usize| -> String {
        if s.chars().count() <= w {
            s.to_string()
        } else if w <= 1 {
            "~".to_string()
        } else {
            let mut out: String = s.chars().take(w - 1).collect();
            out.push('~');
            out
        }
    };

    // Build a bordered row string like `| c1  | c2  |` with BG_GRAY wrapped.
    let mk_line = |cells: &[String]| -> String {
        let mut s = String::from("|");
        for (i, cell) in cells.iter().enumerate() {
            s.push(' ');
            s.push_str(cell);
            // Pad to column width + 1 so the trailing space + `|` align.
            let visual = cell.chars().count();
            let target = widths.get(i).copied().unwrap_or(visual);
            for _ in visual..target {
                s.push(' ');
            }
            s.push(' ');
            s.push('|');
        }
        s
    };

    // Build visible cell slices for headers and rows.
    let build_cells = |fields: &[String]| -> Vec<String> {
        let mut cells: Vec<String> = fields
            .iter()
            .take(MAX_COLS)
            .enumerate()
            .map(|(i, f)| truncate_cell(f, widths[i]))
            .collect();
        if col_omitted > 0 {
            cells.push("...".to_string());
        }
        cells
    };

    let visible_headers = build_cells(&headers);

    println!("{}{}{}", BG_GRAY, mk_line(&visible_headers), RESET);

    // Separator line.
    let sep_cells: Vec<String> = widths.iter().map(|&w| "-".repeat(w)).collect();
    println!("{}{}{}", BG_GRAY, mk_line(&sep_cells), RESET);

    let total = rows.len();

    let print_row = |row: &[String]| {
        let cells = build_cells(row);
        println!("{}{}{}", BG_GRAY, mk_line(&cells), RESET);
    };

    if total <= MAX_ROWS {
        for row in &rows {
            print_row(row);
        }
    } else {
        for row in rows.iter().take(HEAD_TAIL_ROWS) {
            print_row(row);
        }
        let omitted = total.saturating_sub(HEAD_TAIL_ROWS * 2);
        println!(
            "{}  ... {} row{} omitted ...{}",
            C_GRAY,
            omitted,
            if omitted != 1 { "s" } else { "" },
            RESET
        );
        for row in rows.iter().skip(total.saturating_sub(HEAD_TAIL_ROWS)) {
            print_row(row);
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
