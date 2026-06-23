//! Startup: CLI configuration parsing, verbosity/prettiness levels, and startup banner.

use std::env;
use std::fmt;

use anyhow::{Result, anyhow};
use clap::{Parser, ValueEnum};

/// Max retries when the LLM returns an empty response
pub const MAX_EMPTY_RETRY: usize = 3;

/// App description used in both help banner and startup output
pub const APP_DESCRIPTION: &str = "A mere LLM loop for software development tasks.";

/// UI decoration and friendliness level
#[derive(ValueEnum, Clone, Copy, Debug, Default)]
#[value(rename_all = "lower")]
pub enum PrettyLevel {
    #[default]
    Plain,
    Standard,
}

impl fmt::Display for PrettyLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrettyLevel::Plain => write!(f, "Plain"),
            PrettyLevel::Standard => write!(f, "Standard"),
        }
    }
}

/// Logging verbosity level
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq)]
#[value(rename_all = "lower")]
pub enum Verbosity {
    /// Silent (No request logs)
    Silent,
    /// Metadata only (Content-Length)
    Metadata,
    /// Incremental display (New messages only)
    Incremental,
    /// Full request display (Verbose)
    Full,
}

impl fmt::Display for Verbosity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verbosity::Silent => write!(f, "Silent"),
            Verbosity::Metadata => write!(f, "Metadata only"),
            Verbosity::Incremental => write!(f, "Incremental display"),
            Verbosity::Full => write!(f, "Full request display"),
        }
    }
}

/// The Always-Goofy-Things CLI configuration
#[derive(Parser, Debug)]
#[command(
    name = "The Always-Goofy-Things",
    bin_name = "always-goofy-things",
    version,
    about = APP_DESCRIPTION,
    disable_version_flag = true,
    help_template = "\
{before-help}{name} v{version}
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

    /// Set logging verbosity level
    #[arg(
        short = 'v',
        long,
        env = "VERBOSE_LEVEL",
        value_enum,
        default_value_t = Verbosity::Metadata
     )]
    pub verbose_level: Verbosity,

    /// Set UI decoration and friendliness level
    #[arg(
        short = 'p',
        long,
        env = "PRETTY_LEVEL",
        value_enum,
        default_value_t = PrettyLevel::Standard
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
        "The Always-Goofy-Things v{}\nCopyright (C) 2026 SrFeO3. All rights reserved.\n{}\n",
        env!("CARGO_PKG_VERSION"),
        APP_DESCRIPTION
    );
    println!("CONFIGURATION:");
    println!("  working-dir    : {}", current_dir.display());
    println!("  llm-url        : {}", config.llm_url);
    println!("  llm-model      : {}", config.llm_model);
    println!(
        "  llm-api-key    : {}",
        config.llm_api_key.as_ref().map_or("(none)", |_| "(set)")
    );
    println!(
        "  verbose-level: {} ({})",
        config.verbose_level as u8, config.verbose_level
    );
    println!(
        "  pretty-level : {} ({})",
        config.pretty_level as u8, config.pretty_level
    );

    Ok(current_dir)
}
