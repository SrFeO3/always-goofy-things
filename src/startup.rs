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
#[derive(Parser, Debug)]
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

    /// Max retries when the LLM returns an empty response
    #[arg(short = 'E', long, env = "MAX_EMPTY_RETRY", default_value_t = 1)]
    pub max_empty_retry: u32,

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

    /// Write final LLM response to a file instead of stdout.
    /// Only meaningful when -q/--query is also specified.
    #[arg(short = 'o', long = "output", env = "OUTPUT_FILE")]
    pub output_file: Option<String>,

    /// Maximum reasoning turns per user message (tool-calling loop safety limit).
    /// In batch mode (-q), exceeding this exits with an error.
    /// In interactive mode, exceeding this returns control to the user.
    #[arg(long, env = "MAX_REASONING_TURNS", default_value_t = 30)]
    pub max_reasoning_turns: u32,
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
    println!("  max-empty-retry    : {}", config.max_empty_retry);
    println!("  max-reasoning-turns: {}", config.max_reasoning_turns);
    println!("  session-label      : {}", config.session_label);

    Ok(current_dir)
}
