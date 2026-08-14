//! CLI entry point and main execution loop.
//!
//! Coordinates the application's lifecycle and runs the interactive
//! LLM conversation loop.
//!
//! # Safety Warning
//!
//! This application executes autonomous actions on your behalf, including
//! file system modifications, shell command execution, and internet access.
//! Review tool calls carefully before granting execution as these operations
//! may impact your local environment or interact with external servers.
//!
//! # Execution Flow
//!
//! 1. [User Input]     : Capture query from the terminal.
//! 2. [Reasoning Loop] : Recursive cycle for complex tasks.
//!    - LLM Call       : Process context and decide next action.
//!    - Tool Exec      : If requested, run local tool and get result.
//!    - Feedback       : Add result back to history and repeat.
//! 3. [Final Answer]   : Present the completed outcome to the user.

use std::io;
use std::io::Write;

use anyhow::{Result, anyhow};
use clap::Parser;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

mod attach;
mod cmd;
mod compat_provider;
mod compat_resilience;
mod file;
mod file_pdf;
#[cfg(feature = "gui")]
mod gui;
#[cfg(feature = "gui")]
mod gui_pretty;
mod model;
mod persistence;
mod pretty;
mod pretty_data;
mod reasoning;
mod reflex;
mod reflex_literal;
mod startup;
mod todo;
mod tools;
mod tools_data;
mod tools_fuzzy;

use attach::AttachedFile;
use compat_provider::LlmProvider;
use file::FileType;
use model::{Metrics, Session, Settings};
use reasoning::run_reasoning_loop;
use startup::{C_CYAN, RESET};

#[tokio::main]
#[cfg_attr(feature = "gui", allow(unreachable_code))]
async fn main() -> Result<()> {
    let config = startup::Config::parse();

    let provider: LlmProvider = config
        .provider
        .unwrap_or_else(|| compat_provider::detect_provider(&config.llm_url));

    #[cfg(feature = "gui")]
    {
        // eframe::run_native blocks the current thread.
        // block_in_place lets tokio move spawned tasks to other threads.
        tokio::task::block_in_place(|| {
            gui::run(config, provider);
        });
        return Ok(());
    }

    let is_batch = config.query.is_some() || config.todo_mode > 0;
    let start_time = std::time::Instant::now();

    // Set working directory and print banner (all modes)
    let _current_dir = startup::print_startup_info(&config, &provider)?;

    let mut query_reader = DefaultEditor::new()?;

    // Runtime settings, occasionally changed by `/model` / `/config`.
    let mut settings = Settings::from_config(&config);
    // Accumulated token metrics.
    let mut metrics = Metrics::default();

    if !is_batch {
        println!(
            "\n{}Describe your task and press Enter to start (or /help, /exit, ^D).{}",
            C_CYAN, RESET
        );
    }

    // Initial session: system message + label + turn 1.
    let mut session = Session::new(
        config.session_label.clone(),
        startup::system_message(&config),
    );
    // On startup: move meaningful last_session -> previous_session if it exists
    persistence::init_session(&session.label)?;
    // Save system message as the first line of the new session
    persistence::save_message(&session.label, &session.messages[0])?;

    // Main conversation loop
    let mut batch_input: Option<String> = if config.todo_mode > 0 {
        // In todo mode, -q is an additional instruction for every replan and
        // task session (appended to the user message); todo.md is the plan.
        Some(config.query.clone().unwrap_or_default())
    } else {
        config.query.clone()
    };
    loop {
        let input = if let Some(q) = batch_input.take() {
            // Batch: use the -q argument as the first (and only) user input
            q
        } else if is_batch {
            // Batch mode with no more input (should not reach here normally)
            break;
        } else {
            // Interactive: read from the user
            let query_prompt = format!("\nUser-{} > ", session.turn);
            let readline = query_reader.readline(&query_prompt);

            match readline {
                Ok(line) => {
                    // Add to CLI input history (allows using arrow keys to recall previous inputs)
                    query_reader.add_history_entry(line.as_str())?;
                    line
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl+C: Don't exit, show guidance instead
                    println!(
                        "\x1b[93mUse '/exit' or '/quit' to end the session, or press Ctrl+D.\x1b[0m"
                    );
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    // Ctrl+D on an empty line, exit
                    println!("Ctrl-D received. Exiting.");
                    break;
                }
                Err(err) => {
                    println!("Error reading line: {:?}", err);
                    break;
                }
            }
        };
        if input.trim().is_empty() && config.todo_mode == 0 {
            continue;
        }
        // Slash commands. cmd.rs mutates `session` / `settings` in place.
        if let Some(result) = cmd::try_handle_slash_command(&input, &mut session, &mut settings) {
            match result {
                cmd::SlashCmdResult::NoAdvance => continue,
                cmd::SlashCmdResult::RewoundTo(target) => {
                    // `last_sent_count` intentionally NOT reset (see Settings).
                    session.turn = target + 1;
                }
                cmd::SlashCmdResult::RestoredTo {
                    turn: target,
                    label,
                } => {
                    // `last_sent_count` intentionally NOT reset (see Settings).
                    session.turn = target + 1;
                    session.label = label;
                }
                cmd::SlashCmdResult::Exit => break,
            }
            continue;
        }

        // --- Parse @file references from the beginning of input ---
        let query_text;
        let attached_files: Vec<AttachedFile>;
        {
            let (clean, specs, parse_mode) = attach::parse_attached_files(&input);
            if !specs.is_empty() {
                match attach::validate_files(&specs) {
                    Ok(()) => {
                        // Check for oversized files (> 1 MiB)
                        let oversized =
                            attach::check_oversized_files(&specs, attach::OVERLOADED_BYTES);
                        if !oversized.is_empty() {
                            for (path, size) in &oversized {
                                let size_str = attach::format_file_size(*size);
                                if is_batch {
                                    eprintln!(
                                        "\x1b[93m[Warning] {} exceeds 1 MiB: {} (attaching anyway)\x1b[0m",
                                        path, size_str
                                    );
                                } else {
                                    println!(
                                        "{}[Warning] {} exceeds 1 MiB: {}{}",
                                        startup::C_YELLOW,
                                        path,
                                        size_str,
                                        startup::RESET
                                    );
                                }
                            }
                            if !is_batch {
                                print!("Attach these files anyway? (y/N) ");
                                let _ = io::stdout().flush();
                                let mut confirm = String::new();
                                if io::stdin().read_line(&mut confirm).is_err()
                                    || !confirm.trim().eq_ignore_ascii_case("y")
                                {
                                    // User cancelled or error - do not advance
                                    continue;
                                }
                            }
                        }

                        // All files exist - read them
                        match attach::read_attached_files(&specs, parse_mode) {
                            Ok(files) => {
                                for f in &files {
                                    let is_converted_pdf = f.path.to_lowercase().ends_with(".pdf")
                                        && matches!(f.attach_type, FileType::Text);
                                    let label = if is_converted_pdf {
                                        match f.page_range {
                                            Some((s, e)) => format!(
                                                "Markdown extracted from {} (p.{}-p.{})",
                                                f.path, s, e
                                            ),
                                            None => format!("Markdown extracted from {}", f.path),
                                        }
                                    } else {
                                        let size_str =
                                            attach::format_file_size(f.content.len() as u64);
                                        format!("{} ({})", f.path, size_str)
                                    };
                                    println!(
                                        "{}[Attached] {}{}",
                                        startup::C_DIM_GRAY,
                                        label,
                                        startup::RESET
                                    );
                                }
                                attached_files = files;
                                query_text = clean;
                            }
                            Err(e) => {
                                println!("{}[Error] {}{}", startup::C_RED, e, startup::RESET);
                                continue;
                            }
                        }
                    }
                    Err(missing) => {
                        for p in &missing {
                            println!(
                                "{}[File not found] {}{}",
                                startup::C_YELLOW,
                                p,
                                startup::RESET
                            );
                        }
                        // Do NOT advance turn / history
                        continue;
                    }
                }
            } else {
                query_text = input.to_string();
                attached_files = Vec::new();
            }
        }

        // --- Mode-aware execution ---
        let (done, final_answer) = match config.todo_mode {
            0 => {
                let d = run_reasoning_loop(
                    &config,
                    provider,
                    &mut session,
                    &mut settings,
                    &mut metrics,
                    query_text,
                    attached_files,
                )
                .await?;
                let answer = if d {
                    session.messages.last().unwrap().content.clone()
                } else {
                    String::new()
                };
                (d, answer)
            }
            1 | 2 => {
                let summary = todo::run_todo_loop(
                    &config,
                    provider,
                    &mut settings,
                    &mut metrics,
                    &mut session,
                    query_text,
                    attached_files,
                )
                .await?;
                (true, summary)
            }
            _ => unreachable!(),
        };

        if done {
            handle_turn_output(
                &final_answer,
                &config,
                session.turn,
                &session.label,
                is_batch,
                start_time,
            )?;
            if is_batch {
                return Ok(());
            }
            session.turn += 1;
        }
    }
    Ok(())
}

/// Write final answer to file (-o) and print batch summary.
fn handle_turn_output(
    final_answer: &str,
    config: &startup::Config,
    turn: i32,
    label: &str,
    is_batch: bool,
    start_time: std::time::Instant,
) -> Result<()> {
    // -o file output
    if let Some(output_path) = &config.output_file {
        let need_sep = std::fs::metadata(output_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        let content = if need_sep {
            format!(
                "\n\n<!-- always-goofy-things | turn {} | session: {} -->\n\n{}",
                turn, label, final_answer
            )
        } else {
            final_answer.to_string()
        };
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, content.as_bytes()))
            .map_err(|e| anyhow!("Failed to write output to '{}': {}", output_path, e))?;
    }

    if is_batch {
        // Batch: print to stdout if no -o was given, then summary & exit
        if config.output_file.is_none() {
            println!("{}", final_answer);
        }
        let elapsed = start_time.elapsed();
        let secs = elapsed.as_secs_f64();
        let time_str = if secs >= 60.0 {
            format!("{:.0}m {:.1}s", secs / 60.0, secs % 60.0)
        } else {
            format!("{:.1}s", secs)
        };
        let out_label = config.output_file.as_deref().unwrap_or("stdout");
        let q_preview: String = config
            .query
            .as_deref()
            .map(|q| {
                let one_line = q.replace('\n', "\\n").replace('\r', "");
                let chars: Vec<char> = one_line.chars().collect();
                if chars.len() > 10 {
                    format!("{}...", chars[..10].iter().collect::<String>())
                } else {
                    one_line
                }
            })
            .unwrap_or_default();
        eprintln!(
            "\n{}Batch completed in {}, output -> {}, query: \"{}\"{}.",
            C_CYAN, time_str, out_label, q_preview, RESET
        );
    }
    Ok(())
}
