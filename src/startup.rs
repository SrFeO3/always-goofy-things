//! CLI initialization and runtime configuration.
//!
//! Parses command-line arguments, renders the startup banner,
//! and defines global configuration constants for the application.

use std::env;

use anyhow::{Result, anyhow};
use clap::Parser;

use crate::compat_provider::LlmProvider;
use crate::compat_resilience::ToolResultFormat;

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

    /// Batch query: run once non-interactively and print result to stdout.
    /// When set, the agent runs in batch mode and exits after completion.
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
    /// When the replan loop fails to decrease `- [ ]` count this many times, the agent stops.
    /// `0` = unlimited (never stop on replan stalls).
    #[arg(long, env = "MAX_REPLAN_ATTEMPTS", default_value_t = 3)]
    pub max_replan_attempts: u32,

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

/// Build the initial system message describing immutable workspace rules.
/// Used as `messages[0]` for every new session.
pub fn system_message() -> crate::model::Message {
    crate::model::Message {
        role: "system".to_string(),
        content: format!(
            "You are an expert software engineering assistant. Follow these immutable rules:\n\n\
            ## 0. Workspace Context\n\
            - Current Working Directory: Your root is ./ (the current directory).\n\
            - Relative Paths Only: You MUST use relative paths (e.g., file.txt, ./src/) for all operations.\n\
            - Prohibitions: NEVER use absolute paths starting with /. NEVER use ../ to escape the directory.\n\n\
            ## 1. Command Execution (bash)\n\
            - Allowed command patterns: [{}]\n\
            - Interactive commands (e.g., nano, vim, top, ssh) are strictly forbidden. Always check the whitelist.\n\n\
            ## 2. File Editing (str_replace_editor, write_file)\n\
            - str_replace_editor: Provide 'old_string' exactly as it appears in the file, including all whitespace and indentation.\n\
            - write_file: Use this to create new files or overwrite existing files entirely.\n\n\
            ## 3. Information Retrieval (fetch_web)\n\
            - Supports only http/https. Access to private or local networks is strictly prohibited.\n\n\
            ## 4. Response Style\n\
            - Briefly explain the purpose of a tool before calling it.\n\
            - Maintain system rules at the top of the context for inference efficiency.",
            crate::tools::ALLOW_COMMAND_LIST.join(", ")
        ),
        ..Default::default()
    }
}

/// Build a system message for todo-mode sessions.
/// Embeds the immutable workspace rules plus the full todo.md content.
pub fn system_message_with_todo(todo_md: &str) -> crate::model::Message {
    let base = system_message();
    crate::model::Message {
        role: "system".to_string(),
        content: format!(
            "{}\n\n\
            ## 5. Todo Context (Plan-Exec Mode)\n\
            - Your context just reset. Do ONLY the given task, then stop.\n\
            - If you need context from previous work, check `artifacts/`.\n\
            - Report what you did when finished. The agent marks the task as done.\n\
            - Save your outputs to `artifacts/`.\n\n\
            {}",
            base.content, todo_md
        ),
        ..Default::default()
    }
}

/// Build a system message for Mode 2 (Plan-Exec-Dynamic) sessions.
/// Instructs the LLM to update todo.md via write_file, including marking
/// tasks as done and writing the Conclusion when all tasks are complete.
pub fn system_message_with_todo_mode2(todo_md: &str) -> crate::model::Message {
    let base = system_message();
    crate::model::Message {
        role: "system".to_string(),
        content: format!(
            "{}\n\n\
            ## 5. Todo Context (Plan-Exec-Dynamic Mode)\n\
            - Your context just reset. Do ONLY the given task, then stop.\n\
            - If you need context from previous work, check `artifacts/`.\n\
            - After completing, use `write_file` to update `./todo.md`:\n\
              * Mark your task as `[x]`.\n\
              * If all tasks are done, update `## Conclusion` with `Status: Completed`.\n\
              * If you discovered new subtasks, add them to `## Tasks`.\n\
            - Save your outputs to `artifacts/`.\n\n\
            {}",
            base.content, todo_md
        ),
        ..Default::default()
    }
}

/// Print the startup banner and configuration summary.
/// Returns the canonical working directory.
pub fn print_startup_info(config: &Config, provider: &LlmProvider) -> Result<std::path::PathBuf> {
    let current_dir = std::fs::canonicalize(&config.working_dir)
        .map_err(|e| anyhow!("Invalid working directory '{}': {}", config.working_dir, e))?;
    env::set_current_dir(&current_dir)?;

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
    println!("  session-label      : {}", config.session_label);

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
