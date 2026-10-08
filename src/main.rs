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
use rustyline::error::ReadlineError;
use rustyline::{Cmd, DefaultEditor, KeyEvent};

mod attach;
mod cmd;
mod compat_provider;
mod compat_resilience;
mod file;
mod file_pdf;
#[cfg(feature = "gui")]
mod gui;
mod job;
#[cfg(feature = "kb")]
mod kb;
#[cfg(feature = "kb")]
mod kb_analyze;
#[cfg(feature = "kb")]
mod kb_schema;
mod llm_stats;
mod model;
mod persistence;
mod pretty;
mod pretty_data;
#[cfg(feature = "kb")]
mod pretty_kb;
mod reasoning;
mod reflex;
mod reflex_literal;
mod session;
mod startup;
mod todo_guard;
mod todo_job;
mod tools;
mod tools_calc;
mod tools_data;
mod tools_fuzzy;
mod tools_process;
mod tools_pymini;

use attach::AttachedFile;
use compat_provider::LlmProvider;
use file::FileType;
use llm_stats::Metrics;
use model::{Session, Settings};
use reasoning::{LoopCtx, run_reasoning_loop};
use startup::{C_CYAN, RESET};

/// Third-party licensing notices embedded into the binary; differs by feature.
#[cfg(feature = "gui")]
const THIRD_PARTY_LICENSES: &str =
    include_str!("../third_party_licenses/THIRD_PARTY_LICENSES-gui.txt");
#[cfg(not(feature = "gui"))]
const THIRD_PARTY_LICENSES: &str =
    include_str!("../third_party_licenses/THIRD_PARTY_LICENSES-no-feature.txt");

#[tokio::main]
async fn main() -> Result<()> {
    let config = startup::Config::parse();

    // `license` subcommand: show the third-party notices and exit.
    if let Some(startup::Command::License) = config.command {
        print!("{}", THIRD_PARTY_LICENSES);
        return Ok(());
    }

    let provider: LlmProvider = config
        .provider
        .unwrap_or_else(|| compat_provider::detect_provider(&config.llm_url));

    // A GUI build is a GUI process by default. The only internal exception is
    // the CLI child started by the GUI process shell.
    #[cfg(feature = "gui")]
    if std::env::var_os(gui::GUI_CHILD_ENV).is_none() {
        tokio::task::block_in_place(|| gui::run(config))?;
        return Ok(());
    }

    if config.todo_mode > 0 {
        anyhow::bail!(
            "-t/--todo was removed; use /job run <todo.json> [--mode static|replan] instead (batch: -q \"/job run ...\")"
        );
    }

    let is_batch = config.query.is_some();
    let start_time = std::time::Instant::now();

    // Set working directory and print banner (all modes)
    let _current_dir = startup::print_startup_info(&config, &provider)?;

    // KB feature: resolve the directory once (--kb-dir / KB_DIR, or the
    // app-data default, after chdir) so the whole app sees one value -
    // including tool registration and the system prompt.
    #[cfg(feature = "kb")]
    let config = {
        let mut c = config;
        if c.kb_dir.is_none() {
            c.kb_dir = kb::default_kb_dir().map(|d| d.to_string_lossy().into_owned());
        }
        c
    };

    // KB feature: init once per process (run granularity = one process
    // start). Dir = --kb-dir / KB_DIR or the app-data default.
    #[cfg(feature = "kb")]
    let kb_ctx = {
        let store = kb::kb_context_from_config(&config)?;
        // Merged into the CONFIGURATION block: feature state + per-run id.
        // The dirs are already listed above (kb-dir / kb-db rows).
        if let Some(kctx) = &store {
            println!("  kb-feature         : enabled (run_id: {})", kctx.run_id);
        }
        store
    };
    #[cfg(not(feature = "kb"))]
    let kb_ctx: Option<()> = None;

    let mut query_reader = DefaultEditor::new()?;
    // Enter sends the line (rustyline default); Ctrl+O inserts a newline.
    query_reader.bind_sequence(KeyEvent::ctrl('O'), Cmd::Newline);

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
    // Append the system message as the first line of the new session
    persistence::append_message_to_session(&session.label, &session.messages[0])?;
    println!("\x1b[90mSession ID: {}\x1b[0m", session.id);

    // Main conversation loop
    let mut batch_input: Option<String> = config.query.clone();
    loop {
        let input = if let Some(q) = batch_input.take() {
            // Batch: use the -q argument as the first (and only) user input
            q
        } else if is_batch {
            // Batch mode with no more input (should not reach here normally)
            break;
        } else {
            // Interactive: read from the user
            let query_prompt = format!("\nUser-{} (Enter=send, Ctrl+O=newline) > ", session.turn);
            let readline = query_reader.readline(&query_prompt);

            match readline {
                Ok(line) => {
                    // The GUI child receives framed multiline messages from the GUI.
                    #[cfg(feature = "gui")]
                    let line = gui::decode_child_input(line);
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
        if input.trim().is_empty() {
            continue;
        }
        // Long jobs (/job, /kb extract, /kb analyze): parsed synchronously,
        // executed here. No turn advance, like other slash commands.
        if let Some(req) = cmd::parse_job_request(&input) {
            match req {
                Ok(req) => match execute_job_request(
                    req,
                    &config,
                    provider,
                    &mut settings,
                    &mut metrics,
                    kb_ctx.as_ref(),
                )
                .await
                {
                    Ok(summary) => {
                        handle_turn_output(
                            &summary,
                            &config,
                            session.turn,
                            &session.label,
                            false,
                            start_time,
                            &metrics,
                        )?;
                        println!("{}", summary);
                        if is_batch {
                            #[cfg(feature = "kb")]
                            if let Some(kctx) = &kb_ctx {
                                kb::kb_finish_run(kctx);
                            }
                            return Ok(());
                        }
                        continue;
                    }
                    Err(e) => {
                        if is_batch {
                            if let Some(output_path) = &config.output_file {
                                let note = format!("\n\n[job] Error: {}\n", e);
                                let written = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(output_path)
                                    .and_then(|mut f| f.write_all(note.as_bytes()));
                                if let Err(we) = written {
                                    eprintln!(
                                        "Failed to write completion-unconfirmed note to '{}': {}",
                                        output_path, we
                                    );
                                }
                            }
                            return Err(e);
                        }
                        eprintln!("\x1b[91mJob error: {}\x1b[0m", e);
                        continue;
                    }
                },
                Err(msg) => {
                    eprintln!("\x1b[91mSlash command error: {}\x1b[0m", msg);
                    continue;
                }
            }
        }
        // Slash commands. cmd.rs mutates `session` / `settings` in place.
        if let Some(result) = cmd::try_handle_slash_command(
            &input,
            &mut session,
            &mut settings,
            &metrics,
            kb_ctx.as_ref(),
        ) {
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
                    session.label = label.clone();
                    // Rebuild resource stats for the restored label.
                    if let Ok(records) = persistence::load_stats(&label) {
                        metrics = Metrics::from_records(records);
                    }
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
                                let read_ok = io::stdin().read_line(&mut confirm).is_ok();
                                #[cfg(feature = "gui")]
                                let confirm = gui::decode_child_input(confirm);
                                if !read_ok || !confirm.trim().eq_ignore_ascii_case("y") {
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
        let (done, final_answer) = {
            let mut ctx = LoopCtx {
                config: &config,
                provider,
                settings: &mut settings,
                metrics: &mut metrics,
                plan_guard: None,
                kb_ctx: kb_ctx.as_ref(),
                tool_policy: crate::session::ToolPolicy::Inherit,
            };
            let end_reason =
                run_reasoning_loop(&mut ctx, &mut session, "main", query_text, attached_files)
                    .await?;
            let done = end_reason.is_completed();
            let answer = if done {
                session.messages.last().unwrap().content.clone()
            } else {
                String::new()
            };
            (done, answer)
        };

        if done {
            handle_turn_output(
                &final_answer,
                &config,
                session.turn,
                &session.label,
                is_batch,
                start_time,
                &metrics,
            )?;
            if is_batch {
                #[cfg(feature = "kb")]
                if let Some(kctx) = &kb_ctx {
                    kb::kb_finish_run(kctx);
                }
                return Ok(());
            }
            session.turn += 1;
        }
    }
    // Best-effort: mark this process's KB analysis run as completed.
    #[cfg(feature = "kb")]
    if let Some(kctx) = &kb_ctx {
        kb::kb_finish_run(kctx);
    }
    Ok(())
}

/// Execute a parsed long-job request; returns the printable summary.
async fn execute_job_request(
    req: cmd::JobRequest,
    config: &startup::Config,
    provider: LlmProvider,
    settings: &mut Settings,
    metrics: &mut Metrics,
    kb_ctx: crate::tools::KbCtxOpt<'_>,
) -> Result<String> {
    use cmd::JobRequest as R;
    match req {
        R::TodoInit { path } => {
            todo_job::init_plan(std::path::Path::new(&path))?;
            Ok(format!(
                "Wrote scaffold {}. Fill goal/tasks, then /job run it.",
                path
            ))
        }
        R::TodoRun {
            plan,
            mode,
            dry_run,
            status,
            max_retries,
            note,
        } => {
            let path = std::path::Path::new(&plan);
            if dry_run {
                return todo_job::dry_run_todo(path);
            }
            if status {
                return todo_job::todo_status(path);
            }
            let mut ctx = LoopCtx {
                config,
                provider,
                settings,
                metrics,
                plan_guard: None,
                kb_ctx,
                tool_policy: crate::session::ToolPolicy::Inherit,
            };
            todo_job::run_todo(
                &mut ctx,
                path,
                &todo_job::TodoOptions {
                    mode,
                    max_retries,
                    max_stalls: 3,
                    note,
                    executor_policy: crate::session::ToolPolicy::Inherit,
                },
            )
            .await
        }
        R::TodoClean { all } => {
            let deleted = todo_job::clean_states(std::path::Path::new("."), all)?;
            Ok(format!("Removed {} stale job state(s).", deleted.len()))
        }
        #[cfg(feature = "kb")]
        R::KbExtract {
            source,
            chunk_bytes,
            dry_run,
            status,
            max_retries,
            redo,
            handover_auto,
        } => {
            let kb = kb_ctx.ok_or_else(|| {
                anyhow!("[KB_CONFIG_ERROR] The knowledge base is not initialized.")
            })?;
            if dry_run {
                let chunks = kb_analyze::enumerate_extract_chunks(
                    kb,
                    source.as_deref(),
                    chunk_bytes.unwrap_or(kb_analyze::DEFAULT_CHUNK_BYTES),
                )?;
                return Ok(kb_analyze::dry_run_report(&chunks));
            }
            if status {
                return kb_analyze::extract_status(kb, source.as_deref());
            }
            let mut ctx = LoopCtx {
                config,
                provider,
                settings,
                metrics,
                plan_guard: None,
                kb_ctx,
                tool_policy: crate::session::ToolPolicy::Inherit,
            };
            kb_analyze::run_extract(
                &mut ctx,
                source.as_deref(),
                &kb_analyze::ExtractOptions {
                    chunk_bytes: chunk_bytes.unwrap_or(kb_analyze::DEFAULT_CHUNK_BYTES),
                    max_retries,
                    redo: if redo {
                        kb_analyze::RedoMode::IncludeDone
                    } else {
                        kb_analyze::RedoMode::SkipDone
                    },
                    handover: if handover_auto {
                        kb_analyze::HandoverMode::Auto {
                            max_chars: kb_analyze::HANDOVER_AUTO_CHARS,
                        }
                    } else {
                        kb_analyze::HandoverMode::Off
                    },
                },
            )
            .await
        }
        #[cfg(not(feature = "kb"))]
        R::KbExtract { .. } => Err(anyhow!(
            "This binary was built without the 'kb' feature. Rebuild with --features kb."
        )),
        #[cfg(feature = "kb")]
        R::KbAnalyze {
            goal,
            sources,
            max_retries,
            note,
        } => {
            if kb_ctx.is_none() {
                anyhow::bail!("[KB_CONFIG_ERROR] The knowledge base is not initialized.");
            }
            let mut ctx = LoopCtx {
                config,
                provider,
                settings,
                metrics,
                plan_guard: None,
                kb_ctx,
                tool_policy: crate::session::ToolPolicy::Inherit,
            };
            kb_analyze::run_kb_analyze(
                &mut ctx,
                &goal,
                &sources,
                &kb_analyze::AnalyzeOptions { max_retries, note },
            )
            .await
        }
        #[cfg(not(feature = "kb"))]
        R::KbAnalyze { .. } => Err(anyhow!(
            "This binary was built without the 'kb' feature. Rebuild with --features kb."
        )),
    }
}

/// Write final answer to file (-o) and print batch summary.
fn handle_turn_output(
    final_answer: &str,
    config: &startup::Config,
    turn: i32,
    label: &str,
    is_batch: bool,
    start_time: std::time::Instant,
    metrics: &Metrics,
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
        // Token / LLM-time summary (resource accounting).
        let t = &metrics.totals;
        eprintln!(
            "Tokens: In {}, Cache {}, CacheW {}, Out {}, Reasoning {}",
            llm_stats::fmt_tokens(t.in_normal + t.in_cached + t.in_cache_write + t.in_audio),
            llm_stats::fmt_tokens(t.in_cached),
            llm_stats::fmt_tokens(t.in_cache_write),
            llm_stats::fmt_tokens(t.out_normal),
            llm_stats::fmt_tokens(t.out_reasoning),
        );
        let llm_secs = t.llm_ms_total as f64 / 1000.0;
        let wall_secs = secs;
        let pct = if wall_secs > 0.0 {
            100.0 * llm_secs / wall_secs
        } else {
            0.0
        };
        eprintln!("LLM time: {:.1}s ({:.1}% of wall)", llm_secs, pct);
    }
    Ok(())
}
