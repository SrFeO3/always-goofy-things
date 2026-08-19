//! CLI initialization and runtime configuration.
//!
//! Parses command-line arguments, renders the startup banner,
//! and defines global configuration constants for the application.

use std::env;

use anyhow::{Result, anyhow};
use clap::Parser;

use crate::compat_provider::LlmProvider;
use crate::compat_resilience::ToolResultFormat;
use crate::tools::ToolName;

/// The official name and description of this application
pub const APP_NAME: &str = "Always-Goofy-Things";
pub const APP_BIN_NAME: &str = "always-goofy-things";
pub const APP_DESCRIPTION: &str = "A lightweight LLM loop for software development tasks.";

// ANSI escape sequences for text styling.
pub const HDR_RED: &str = "\x1b[48;2;218;75;80m";
pub const HDR_GREEN: &str = "\x1b[48;2;45;180;103m";

pub const BG_RED: &str = "\x1b[48;2;190;85;85m";
pub const BG_GREEN: &str = "\x1b[48;2;80;150;95m";
pub const BG_GRAY: &str = "\x1b[48;2;85;85;90m";

pub const C_GRAY: &str = "\x1b[90m";
pub const C_RED: &str = "\x1b[31m";
pub const C_GREEN: &str = "\x1b[32m";
pub const C_CYAN: &str = "\x1b[36m";
pub const C_MAGENTA: &str = "\x1b[35m";
pub const C_YELLOW: &str = "\x1b[93m";

pub const C_DIM_GRAY: &str = "\x1b[90m";
pub const C_DIM_GREEN: &str = "\x1b[32m";

pub const RESET: &str = "\x1b[0m";
pub const ERASE_LINE: &str = "\x1b[K";
pub const EMPTY: &str = "";

/// UI decoration and friendliness level
pub type PrettyLevel = u8;

/// UI verbosity for LLM conversation display
pub type Verbosity = u8;

/// The Always-Goofy-Things CLI configuration
#[derive(Parser, Debug, Clone)]
#[command(
    name = APP_NAME,
    bin_name = APP_BIN_NAME,
    version,
    about = APP_DESCRIPTION,
    disable_version_flag = true,
    help_template = "\
{before-help}The {name} v{version}
{about-with-newline}
{usage-heading} {usage}

{all-args}

NOTE: Command-line options always override their corresponding environment variables.
{after-help}",
    help_expected = true
)]
pub struct Config {
    /// Directory where AI tools operate
    #[arg(short = 'w', long, env = "WORKING_DIR", default_value = ".")]
    pub working_dir: String,

    /// Todo execution mode
    #[arg(short = 't', long = "todo", env = "TODO_MODE", default_value_t = 0)]
    pub todo_mode: u8,

    /// Endpoint for the Chat API
    #[arg(
        short = 'u',
        long,
        env = "LLM_URL",
        default_value = "http://localhost:11434/api/chat"
    )]
    pub llm_url: String,

    /// The LLM model name to use
    #[arg(short = 'm', long, env = "LLM_MODEL", default_value = "gemma4:12b")]
    pub llm_model: String,

    /// API key for authentication
    #[arg(short = 'k', long, env = "LLM_API_KEY")]
    pub llm_api_key: Option<String>,

    /// Reflex mode
    #[arg(long, env = "UNSAFE_REFLEX_MODE", default_value_t = false)]
    pub unsafe_reflex: bool,

    /// UI verbosity for LLM conversation display
    #[arg(
      short = 'v',
      long,
      env = "VERBOSE_LEVEL",
      value_parser = clap::value_parser!(u8).range(0..=4),
      default_value_t = 1
    )]
    pub verbose_level: Verbosity,

    /// Set UI decoration and friendliness level
    #[arg(
      short = 'p',
      long,
      env = "PRETTY_LEVEL",
      value_parser = clap::value_parser!(u8).range(0..=1),
      default_value_t = 1
    )]
    pub pretty_level: PrettyLevel,

    /// Maximum requests per minute for the LLM API (0 = unlimited)
    #[arg(short = 'r', long, env = "LLM_RPM", default_value_t = 0)]
    pub llm_rpm: u32,

    /// Maximum output tokens per LLM request
    #[arg(short = 'T', long, env = "MAX_OUTPUT_TOKENS", default_value_t = 16384)]
    pub max_output_tokens: u32,

    /// Stop after N consecutive empty LLM responses in the reasoning loop (0 = unlimited)
    #[arg(
        short = 'E',
        long,
        env = "MAX_REASONING_EMPTY_RESPONSES",
        default_value_t = 2
    )]
    pub max_reasoning_empty_responses: u32,

    /// Session label for persistence (suffix for session files)
    #[arg(short = 's', long, env = "SESSION_LABEL", default_value = "default")]
    pub session_label: String,

    /// LLM API provider (auto-detected from URL if not specified)
    #[arg(short = 'P', long, env = "LLM_PROVIDER", value_enum)]
    pub provider: Option<LlmProvider>,

    /// How tool results are formatted when sent to the LLM
    #[arg(short = 'R', long, env = "TOOL_RESULT_FORMAT", value_enum, default_value_t = ToolResultFormat::JsonString)]
    pub tool_result_format: ToolResultFormat,

    /// Only these AI tools are enabled (repeatable / comma-separated).
    /// When unset, all tools are enabled. Disabled tools are hidden from the
    /// LLM and refuse to execute even if called.
    /// Note: todo modes (-t/--todo) require `read_file`, and mode 2 also
    /// requires `write_file`; disabling them breaks the todo workflow.
    #[arg(long = "only-tools", env = "ONLY_TOOLS", value_delimiter = ',')]
    pub only_tools: Vec<ToolName>,

    /// Batch query: run once non-interactively and print result to stdout.
    /// When set, the application runs in batch mode and exits after completion.
    #[arg(short = 'q', long)]
    pub query: Option<String>,

    /// Write each turn's final LLM response to a file.
    /// In batch mode (-q): single write (1 turn).
    /// In interactive mode: append each turn with a separator.
    #[arg(short = 'o', long = "output", env = "OUTPUT_FILE")]
    pub output_file: Option<String>,

    /// Maximum reasoning turns per user message (tool-calling loop safety limit) (0 = unlimited).
    /// In batch mode (-q), exceeding this exits with an error.
    /// In interactive mode, exceeding this returns control to the user.
    #[arg(long, env = "MAX_REASONING_TURNS", default_value_t = 30)]
    pub max_reasoning_turns: u32,

    /// Maximum consecutive replan attempts without reducing unchecked tasks (Mode 2).
    /// When the replan loop fails to decrease `- [ ]` count this many times, the application stops.
    /// `0` = unlimited (never stop on replan stalls).
    #[arg(long, env = "MAX_REPLAN_ATTEMPTS", default_value_t = 3)]
    pub max_replan_attempts: u32,

    /// Maximum bytes captured per output stream (stdout / stderr) for
    /// execute_bash / grep_search; excess output keeps the tail (0 = unlimited).
    #[arg(long, env = "MAX_TOOL_OUTPUT_BYTES", default_value_t = 1048576)]
    pub max_tool_output_bytes: usize,

    /// Wall-clock timeout in seconds for execute_bash / grep_search (0 = unlimited).
    #[arg(long, env = "TOOL_TIMEOUT_SECS", default_value_t = 30)]
    pub tool_timeout_secs: u64,

    /// Database type for data_search / data_schema tools.
    /// Supported: greptimedb, clickhouse, influxdb.
    /// When set, the data tools are enabled.
    #[arg(long, env = "DB_TYPE")]
    pub db_type: Option<String>,

    /// Database HTTP query endpoint URL (full path).
    /// Required when --db-type is set.
    #[arg(long, env = "DB_URL")]
    pub db_url: Option<String>,

    /// Authentication key/token for the database.
    /// GreptimeDB/ClickHouse: user:password (Basic auth).
    /// InfluxDB: API token (Bearer auth).
    #[arg(long, env = "DB_AUTH_KEY")]
    pub db_auth_key: Option<String>,

    /// Query timeout in seconds (default: 30).
    #[arg(long, env = "DB_TIMEOUT", default_value_t = 30)]
    pub db_timeout: u64,

    /// Maximum response size in bytes before truncation (default: 65536 = 64KB).
    #[arg(long, env = "DB_MAX_BYTES", default_value_t = 65536)]
    pub db_max_bytes: usize,

    /// Auto-confirm data_search / data_schema tools without user prompt.
    /// Data tools are read-only and safe to auto-execute.
    #[arg(long, env = "DB_UNSAFE_REFLEX", default_value_t = false)]
    pub db_unsafe_reflex: bool,
}

impl Config {
    /// Whether a tool (by its canonical name) is enabled.
    /// `--only-tools` unset means all tools are enabled (previous behavior).
    pub fn is_tool_enabled(&self, name: &str) -> bool {
        self.only_tools.is_empty() || self.only_tools.iter().any(|t| t.as_str() == name)
    }
}

/// Build the initial system message describing immutable workspace rules.
/// Used as `messages[0]` for every new session.
/// Sections that reference disabled tools are omitted, so the LLM only sees
/// guidance for tools it can actually call.
pub fn system_message(config: &Config) -> crate::model::Message {
    build_system_message(base_system_sections(|n| config.is_tool_enabled(n)))
}

/// Base sections shared by all system messages.
/// Section numbers are FIXED: disabling a tool only removes its own line,
/// leaving the other numbers unchanged. Missing numbers (gaps) are
/// intentional when tools are disabled.
/// - ## 1 / ## 3: always present
/// - ## 2: umbrella for tool sections, present only when at least one tool
///   is enabled; ## 2-1 / ## 2-2 / ## 2-3 are the fixed tool categories
/// - ## 4: Todo Context (appended by the todo-mode builders)
fn base_system_sections(is_enabled: impl Fn(&str) -> bool) -> Vec<String> {
    let mut sections = vec![
        "## 1. Workspace Context\n\
         - Current Working Directory: Your root is ./ (the current directory).\n\
         - Relative Paths Only: You MUST use relative paths (e.g., file.txt, ./src/) for all operations.\n\
         - Prohibitions: NEVER use absolute paths starting with /. NEVER use ../ to escape the directory."
            .to_string(),
    ];

    // Tool categories live under the fixed umbrella ## 2; each category and
    // each tool line keeps a fixed position, so toggling a tool never shifts
    // the numbers of the remaining lines (gaps are fine).
    let mut tool_sections: Vec<String> = Vec::new();

    if is_enabled("execute_bash") {
        tool_sections.push(format!(
            "## 2-1. Command Execution (execute_bash)\n\
             - Allowed command patterns: [{}]\n\
             - Interactive commands (e.g., nano, vim, top, ssh) are strictly forbidden. Always check the whitelist.",
            crate::tools::ALLOW_COMMAND_LIST.join(", ")
        ));
    }

    let mut file_names: Vec<&str> = Vec::new();
    let mut file_lines: Vec<&str> = Vec::new();
    if is_enabled("read_file") {
        file_names.push("read_file");
        file_lines.push(
            "- read_file: Read file contents; start/end form a 1-based inclusive range (lines for text files, pages for PDFs).",
        );
    }
    if is_enabled("str_replace_editor") {
        file_names.push("str_replace_editor");
        file_lines.push(
            "- str_replace_editor: Replace one exact string block; prefer it over write_file for partial edits. old_string must match the file exactly, including whitespace and indentation.",
        );
    }
    if is_enabled("write_file") {
        file_names.push("write_file");
        file_lines.push(
            "- write_file: Create a new file or fully replace an existing one; for new files and full rewrites only.",
        );
    }
    if !file_names.is_empty() {
        tool_sections.push(format!(
            "## 2-2. File Operations ({})\n{}",
            file_names.join(", "),
            file_lines.join("\n")
        ));
    }

    let mut retrieval_names: Vec<&str> = Vec::new();
    let mut retrieval_lines: Vec<&str> = Vec::new();
    if is_enabled("list_directory") {
        retrieval_names.push("list_directory");
        retrieval_lines.push(
            "- list_directory: List files and directories (non-recursive) to explore the project structure.",
        );
    }
    if is_enabled("grep_search") {
        retrieval_names.push("grep_search");
        retrieval_lines
            .push("- grep_search: Search for text patterns across workspace files to locate code.");
    }
    if is_enabled("fetch_web") {
        retrieval_names.push("fetch_web");
        retrieval_lines.push(
            "- fetch_web: Supports only http/https. Access to private or local networks is strictly prohibited.",
        );
    }
    if is_enabled("data_search") || is_enabled("data_schema") {
        retrieval_names.push("data_search, data_schema");
        retrieval_lines.push(
            "- data_search / data_schema: Query the configured database (requires --db-type).",
        );
    }
    if !retrieval_names.is_empty() {
        tool_sections.push(format!(
            "## 2-3. Information Retrieval ({})\n{}",
            retrieval_names.join(", "),
            retrieval_lines.join("\n")
        ));
    }

    if !tool_sections.is_empty() {
        sections.push(format!(
            "## 2. Tools (your interface to the workspace and the outside world)\n{}",
            tool_sections.join("\n\n")
        ));
    }

    sections.push(
        "## 3. Response Style\n\
         - Briefly explain the purpose of a tool before calling it.\n\
         - Maintain system rules at the top of the context for inference efficiency."
            .to_string(),
    );
    sections
}

/// Join the pre-numbered sections and wrap them in the standard preamble.
fn build_system_message(sections: Vec<String>) -> crate::model::Message {
    crate::model::Message {
        role: "system".to_string(),
        content: format!(
            "You are an expert software engineering assistant. Follow these immutable rules:\n\n{}",
            sections.join("\n\n")
        ),
        ..Default::default()
    }
}

/// Build a system message for Mode 1 (Plan-Exec-Static) task sessions.
/// The plan is read from ./todo.md with read_file; the task LLM executes the
/// single task named in the user message.
pub fn system_message_mode1_task_loop(config: &Config) -> crate::model::Message {
    let mut sections = base_system_sections(|n| config.is_tool_enabled(n));
    sections.push(format!(
        "## 4. Todo Context (Plan-Exec Task Loop)\n\
         - Read `./todo.md` and `artifacts/handover.md` FIRST with read_file; todo.md is the plan, handover.md holds the notes and reports from previous tasks.\n\
         - Execute ONLY the task in the user message; do NOT execute other tasks.\n\
         - Finish the task completely (create its outputs) before stopping.\n\
         - Check `artifacts/` for previous work; save your outputs there.\n\
         - Handover entries may be followed by an `outputs:` line listing the artifact paths the previous task created; read the listed files with read_file when your task needs them.\n\
         - Your final message must be a Handover Report in exactly this format:\n\
           - Status: done / blocked\n\
           - Output: <file paths created or updated, or \"none\">\n\
           - Findings: <facts you observed, in one or two sentences>\n\
           - Next: <what the next task should watch out for, or \"none\">\n\
         - Keep the entire report within {} characters.\n\
         Nothing else; do not add other sections. The application saves your report to `artifacts/handover.md`; do NOT edit `artifacts/handover.md` yourself.",
        crate::todo_guard::HANDOVER_REPORT_MAX_CHARS
    ));
    build_system_message(sections)
}

/// Build a system message for Mode 2 (Plan-Exec-Dynamic) replan sessions.
/// The replan session (planner role) only updates the plan; it never executes
/// the tasks.
pub fn system_message_mode2_replan(config: &Config) -> crate::model::Message {
    let mut sections = base_system_sections(|n| config.is_tool_enabled(n));
    sections.push(format!(
        "## 4. Todo Context (Plan-Exec-Dynamic Replan)\n\
         - You are the task planner. Do NOT execute the tasks; only update the plan.\n\
         - Read `./todo.md` and `artifacts/handover.md` FIRST with read_file; todo.md is the current plan, handover.md holds the task reports and planner notes from previous sessions (each task report is followed by an `outputs:` line listing the artifact paths that task declared).\n\
         - todo.md has a FIXED format of exactly three sections: `# <title>`, `## Goal`, `## Tasks`. NEVER add, remove, or rename sections; NEVER add prose outside the Tasks list; keep the `- [ ]` / `- [x]` bullet format.\n\
         - Mark completed tasks `[x]` (verify the files named in their `outputs:` lines exist in `artifacts/`), and add, remove, reorder, or split tasks. If ALL tasks are `[x]` but the Goal is not yet achieved, add the tasks needed to finish it.\n\
         - Write, in this order:\n\
           1. `./todo.md` - the updated plan; text alone does not update the plan.\n\
           2. `./next-task.md` (write_file; overwrite it every time) - the brief for the IMMEDIATELY NEXT task: its scope, the files it must read (mark each one must-read or optional), the previous task's `outputs:`, and warnings.\n\
         - Your final message must be your plan-update notes for the next planner session: anything it must know that does not fit in `./todo.md` or `./next-task.md`, in exactly this format:\n\
           - Status: <overall state of the job>\n\
           - Progress: <what the completed tasks achieved>\n\
           - Decisions: <what you changed in the plan and why>\n\
           - Next: <what happens next, or \"none\">\n\
         - Keep the entire note within {} characters.\n\
         Nothing else; do not add other sections. The application saves your note to `artifacts/handover.md`; do NOT edit `artifacts/handover.md` yourself.",
        crate::todo_guard::HANDOVER_REPORT_MAX_CHARS
    ));
    build_system_message(sections)
}

/// Build a system message for Mode 2 (Plan-Exec-Dynamic) task sessions.
/// The plan is read from ./todo.md with read_file; the task LLM executes the
/// single task named in the user message and updates the plan on completion.
pub fn system_message_mode2_task_loop(config: &Config) -> crate::model::Message {
    let mut sections = base_system_sections(|n| config.is_tool_enabled(n));
    sections.push(format!(
        "## 4. Todo Context (Plan-Exec-Dynamic Task Loop)\n\
         - Read `./todo.md` and `./next-task.md` FIRST with read_file; todo.md is the plan, next-task.md is the brief for your task: your scope, the files to read (marked must-read or optional), the previous task's `outputs:`, and warnings. If next-task.md is missing or the brief is insufficient, explore `artifacts/` with list_directory and read only the files your task needs.\n\
         - Execute ONLY the task in the user message; do NOT execute other tasks.\n\
         - Finish the task completely (create its outputs) before stopping.\n\
         - After completing, update `./todo.md` with write_file: mark ONLY your task `[x]`; you may add subtasks to `## Tasks` if needed.\n\
         - Check `artifacts/` for previous work; save your outputs there.\n\
         - Your final message must be a Handover Report in exactly this format:\n\
           - Status: done / blocked\n\
           - Output: <file paths created or updated, or \"none\">\n\
           - Findings: <facts you observed, in one or two sentences>\n\
           - Next: <what the next task should watch out for, or \"none\">\n\
         - Keep the entire report within {} characters.\n\
         Nothing else; do not add other sections. The application saves your report to `artifacts/handover.md`; do NOT edit `artifacts/handover.md` yourself.",
        crate::todo_guard::HANDOVER_REPORT_MAX_CHARS
    ));
    build_system_message(sections)
}

/// Print the startup banner and configuration summary.
/// Returns the canonical working directory.
pub fn print_startup_info(config: &Config, provider: &LlmProvider) -> Result<std::path::PathBuf> {
    let current_dir = std::fs::canonicalize(&config.working_dir)
        .map_err(|e| anyhow!("Invalid working directory '{}': {}", config.working_dir, e))?;
    env::set_current_dir(&current_dir)?;

    // Register the workspace root and tool execution limits for path
    // validation and child-process hardening.
    crate::tools::set_workspace_root(current_dir.clone());
    crate::tools_process::set_tool_limits(crate::tools_process::ToolLimits {
        max_output_bytes: config.max_tool_output_bytes,
        tool_timeout_secs: config.tool_timeout_secs,
    });

    println!(
        "The {APP_NAME} v{}\nCopyright (C) 2026 SrFeO3. All rights reserved.\n{}\n",
        env!("CARGO_PKG_VERSION"),
        APP_DESCRIPTION
    );
    println!("CONFIGURATION:");
    println!(
        "  mode               : {}",
        match config.todo_mode {
            1 => "Plan-Exec-Static",
            2 => "Plan-Exec-Dynamic",
            _ => "ReAct",
        }
    );
    println!(
        "  run                : {}",
        if config.query.is_some() || config.todo_mode > 0 {
            "batch"
        } else {
            "interactive"
        }
    );
    println!("  working-dir        : {}", current_dir.display());
    println!("  unsafe-reflex      : {}", config.unsafe_reflex);
    println!("  llm-url            : {}", config.llm_url);
    println!("  llm-provider       : {}", provider);
    println!("  llm-model          : {}", config.llm_model);
    println!(
        "  llm-api-key        : {}",
        config.llm_api_key.as_ref().map_or("(none)", |_| "(set)")
    );
    println!("  tool-result-format : {}", config.tool_result_format);
    println!("  llm-rpm            : {}", config.llm_rpm);
    println!("  verbose-level      : {}", config.verbose_level);
    println!("  pretty-level       : {}", config.pretty_level);
    println!("  max-output-tokens  : {}", config.max_output_tokens);
    println!(
        "  max-reasoning-empty-responses: {}",
        config.max_reasoning_empty_responses
    );
    println!("  max-reasoning-turns: {}", config.max_reasoning_turns);
    println!("  max-replan-attempts: {}", config.max_replan_attempts);
    println!("  max-tool-output-bytes: {}", config.max_tool_output_bytes);
    println!("  tool-timeout-secs   : {}", config.tool_timeout_secs);
    println!("  session-label      : {}", config.session_label);

    // --- Tool enablement ---
    if config.only_tools.is_empty() {
        println!("  only-tools         : all (default)");
    } else {
        println!(
            "  only-tools         : {}",
            config
                .only_tools
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if config
            .only_tools
            .iter()
            .any(|t| matches!(t, ToolName::DataSearch | ToolName::DataSchema))
            && config.db_type.is_none()
        {
            println!(
                "{C_YELLOW}[Warning] data_search/data_schema require --db-type; they stay disabled without it.{RESET}"
            );
        }
    }

    // --- Database configuration ---
    if let Some(db_type) = config.db_type.as_deref() {
        // Validate: db_type requires db_url
        let Some(db_url) = config.db_url.as_deref() else {
            anyhow::bail!(
                "[DB_CONFIG_ERROR] --db-url is required when --db-type is set. Provide the database HTTP endpoint URL."
            );
        };

        // Validate: db_type must be a supported value
        if !matches!(db_type, "greptimedb" | "clickhouse" | "influxdb") {
            anyhow::bail!(
                "[DB_UNKNOWN_TYPE] Unsupported database type '{}'. Supported types: greptimedb, clickhouse, influxdb.",
                db_type
            );
        }

        // Warning: Basic auth should use user:password format
        if matches!(db_type, "greptimedb" | "clickhouse")
            && config
                .db_auth_key
                .as_deref()
                .is_some_and(|k| !k.contains(':'))
        {
            println!(
                "{C_YELLOW}[Warning] --db-auth-key for {} should be in 'user:password' format (Basic auth).{RESET}",
                db_type
            );
        }

        println!("  db-type            : {}", db_type);
        println!("  db-url             : {}", db_url);
        println!(
            "  db-auth-key        : {}",
            config.db_auth_key.as_ref().map_or("(none)", |_| "(set)")
        );
        println!("  db-timeout         : {}s", config.db_timeout);
        println!("  db-max-bytes       : {}", config.db_max_bytes);
        println!("  db-unsafe-reflex   : {}", config.db_unsafe_reflex);
    }

    Ok(current_dir)
}

#[cfg(test)]
#[path = "tests/startup_test.rs"]
mod tests;
