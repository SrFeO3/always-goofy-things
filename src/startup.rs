//! CLI initialization and runtime configuration.
//!
//! Parses command-line arguments, renders the startup banner,
//! and defines global configuration constants for the application.

use std::env;

use anyhow::{Result, anyhow};
use clap::Parser;

/// Max retries when the LLM returns an empty response
pub const MAX_EMPTY_RETRY: usize = 1;

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
      value_parser = clap::value_parser!(u8).range(0..=3),
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
}

/// Print the startup banner and configuration summary.
/// Returns the canonical working directory.
pub fn print_startup_info(config: &Config) -> Result<std::path::PathBuf> {
    let current_dir = std::fs::canonicalize(&config.working_dir)
        .map_err(|e| anyhow!("Invalid working directory '{}': {}", config.working_dir, e))?;
    env::set_current_dir(&current_dir)?;

    println!(
        "The {APP_NAME} v{}\nCopyright (C) 2026 SrFeO3. All rights reserved.\n{}\n",
        env!("CARGO_PKG_VERSION"),
        APP_DESCRIPTION
    );
    println!("CONFIGURATION:");
    println!("  working-dir    : {}", current_dir.display());
    println!("  unsafe-reflex  : {}", config.unsafe_reflex);
    println!("  llm-url        : {}", config.llm_url);
    println!("  llm-model      : {}", config.llm_model);
    println!(
        "  llm-api-key    : {}",
        config.llm_api_key.as_ref().map_or("(none)", |_| "(set)")
    );
    println!("  verbose-level  : {}", config.verbose_level);
    println!("  pretty-level   : {}", config.pretty_level);

    Ok(current_dir)
}
