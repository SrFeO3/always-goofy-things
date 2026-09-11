//! Pretty UI rendering for Knowledge Base tools.
//!
//! Provides a human-readable CLI display of the four KB tools (`data_kb_search` /
//! `data_kb_schema` / `data_kb_insert` / `data_kb_update`) and the `/kb`
//! command output. Mirrors the `pretty_data.rs` pattern: `pretty.rs` only
//! dispatches, this module owns the rendering.

use serde_json::Value;

use crate::startup::{C_GRAY, C_GREEN, C_YELLOW, RESET};

/// One-line preview before execution.
/// - search: the SQL query
/// - schema: the table (or "all tables")
/// - insert: target document + item count
/// - update: target_type + target_id
pub(crate) fn pretty_print_kb_command(name: &str, args: &Value) {
    match name {
        "data_kb_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            println!("-- KB Query: {}{}{}", C_YELLOW, query, RESET);
        }
        "data_kb_schema" => {
            let table = args
                .get("table")
                .and_then(|v| v.as_str())
                .unwrap_or("(all tables)");
            println!("-- KB Schema: {}{}{}", C_YELLOW, table, RESET);
        }
        "data_kb_insert" => {
            let doc = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let count = [
                "entities",
                "claims",
                "relations",
                "conditions",
                "events",
                "evidence",
            ]
            .iter()
            .map(|k| {
                args.get(*k)
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            })
            .sum::<usize>();
            println!(
                "-- KB Insert: document {}{}{} ({} item{})",
                C_YELLOW,
                doc,
                RESET,
                count,
                if count == 1 { "" } else { "s" }
            );
        }
        "data_kb_update" => {
            let tt = args
                .get("target_type")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let id = args
                .get("target_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("-- KB Update: {} {}", tt, id);
        }
        _ => {}
    }
}

/// Compact result rendering.
pub(crate) fn pretty_print_kb_result(result: &Value) {
    // data_kb_search: CSV content
    if let Some(content) = result.get("content").and_then(|v| v.as_str()) {
        let lines: Vec<&str> = content.lines().collect();
        let shown: Vec<&str> = lines.iter().take(5).map(|s| *s).collect();
        println!(
            "KB Result: [{} line{}]",
            lines.len(),
            if lines.len() == 1 { "" } else { "s" }
        );
        for line in shown {
            println!("{}", line);
        }
        if lines.len() > 5 {
            println!(
                "{}... ({} more line{}){}",
                C_GRAY,
                lines.len() - 5,
                if lines.len() - 5 == 1 { "" } else { "s" },
                RESET
            );
        }
        return;
    }
    // schema / insert / update: status + summary fields
    if let Some(status) = result.get("status").and_then(|v| v.as_str()) {
        print!("{}KB status: {}{}", C_GREEN, status, RESET);
        if let Some(n) = result.get("total").and_then(|v| v.as_u64()) {
            print!(" ({} items)", n);
        }
        if let Some(version) = result.get("version").and_then(|v| v.as_i64()) {
            print!(" (annotation version {})", version);
        }
        println!();
        if let Some(tables) = result.get("tables").and_then(|v| v.as_array()) {
            println!("  {} table(s) / view(s)", tables.len());
            for t in tables.iter().take(10) {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let typ = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let rows = t.get("approx_rows").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("  - {} ({}, ~{} rows)", name, typ, rows);
            }
        }
        if result.get("table").and_then(|v| v.as_str()).is_some() {
            if let Some(samples) = result.get("samples").and_then(|v| v.as_array()) {
                println!("  {} sample row(s)", samples.len());
            }
        }
        return;
    }
    println!("KB Result: {}", result);
}
