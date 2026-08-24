#![cfg(feature = "gui")]

//! GUI frontend (eframe / egui) for the LLM assistant.
//!
//! Provides an optional graphical interface as an alternative to the
//! terminal CLI. Shares the same core logic via feature-gated channels.

use std::collections::VecDeque;
use std::sync::Arc;

use eframe::egui;
use tokio::sync::oneshot;

use crate::attach;
use crate::cmd::{self, SlashCmdResult};
use crate::compat_provider::LlmProvider;
use crate::gui_pretty;
use crate::gui_pretty::{C_CYAN, C_GRAY, C_GREEN, C_MAGENTA, C_RED};
use crate::llm_stats::{Metrics, fmt_tokens};
use crate::model::{LLM_STREAM_BUF, Message, Session, Settings};
use crate::persistence;
use crate::reasoning::run_reasoning_loop;
use crate::startup::{self, Config};
use crate::todo;
use crate::tools::{TOOL_INTERACT_CH, ToolInteractMsg, ToolRunDecision, ToolRunDecisionKind};

type TurnResult = (Session, Settings, Metrics, bool, Option<String>);

struct GuiApp {
    config: Arc<Config>,
    provider: LlmProvider,
    session: Option<Session>,
    settings: Option<Settings>,
    metrics: Option<Metrics>,

    current_model: String,
    input_text: String,
    conversation: String,
    /// Display buffer holding the current turn's messages in pretty format
    /// (reasoning / content split, tool diff previews). Rebuilt from session
    /// on completion so the display stays consistent before and after the turn.
    stream_message_buffer: Vec<Message>,
    /// Number of `session.messages` committed before the current turn.
    /// Messages before this index are rendered from `session.messages`;
    /// messages from this index onward are rendered from `stream_message_buffer`.
    session_msg_base: usize,
    pending_confirms: VecDeque<PendingConfirm>,

    done_rx: Option<oneshot::Receiver<TurnResult>>,
    worker_handle: Option<tokio::task::JoinHandle<()>>,
    is_running: bool,
    focus_input: bool,
    todo_started: bool,
}

struct PendingConfirm {
    name: String,
    args: serde_json::Value,
    reply: oneshot::Sender<ToolRunDecision>,
}

impl GuiApp {
    fn new(config: Config, provider: LlmProvider) -> Self {
        let system_msg = startup::system_message(&config);
        let label = config.session_label.clone();
        let session = Session::new(label, system_msg);
        let settings = Settings::from_config(&config);
        let current_model = settings.llm_model.clone();
        let metrics = Metrics::default();

        if let Err(e) = persistence::init_session(&session.label) {
            eprintln!("[GUI] persistence init: {}", e);
        }
        if let Err(e) = persistence::save_message(&session.label, &session.messages[0]) {
            eprintln!("[GUI] persistence save: {}", e);
        }

        GuiApp {
            config: Arc::new(config),
            provider,
            session: Some(session),
            settings: Some(settings),
            metrics: Some(metrics),
            current_model,
            input_text: String::new(),
            conversation: String::new(),
            stream_message_buffer: Vec::new(),
            session_msg_base: 1, // system message at index 0, render from index 1 onward
            pending_confirms: VecDeque::new(),
            done_rx: None,
            worker_handle: None,
            is_running: false,
            focus_input: true,
            todo_started: false,
        }
    }

    fn sync_model(&mut self) {
        if let Some(ref s) = self.settings {
            self.current_model = s.llm_model.clone();
        }
    }

    /// Render a single `Message` with consistent formatting.
    /// Used for both completed session messages and the in-progress stream,
    /// ensuring identical display during and after a turn.
    fn render_message(ui: &mut egui::Ui, msg: &Message, turn: &mut u32) {
        match msg.role.as_str() {
            "user" => {
                *turn += 1;
                ui.colored_label(
                    C_GRAY,
                    format!("\u{2500}\u{2500} Turn {} \u{2500}\u{2500}", *turn),
                );
                ui.label(format!("User-{} > {}", *turn, msg.content));
                for f in &msg.attached_files {
                    ui.colored_label(C_GRAY, format!("[Attached] {}", f.path));
                }
            }
            "assistant" => {
                if let Some(ref rc) = msg.reasoning_content
                    && !rc.trim().is_empty()
                {
                    ui.colored_label(C_GREEN, "[Thinking]");
                    ui.colored_label(C_GREEN, rc);
                }
                if !msg.content.trim().is_empty() {
                    ui.label(format!("Assistant > {}", msg.content));
                } else if let Some(ref tc) = msg.tool_calls
                    && !tc.is_empty()
                {
                    let names: Vec<&str> = tc.iter().map(|c| c.function.name.as_str()).collect();
                    ui.label(format!("Assistant > [Tool Call: {}]", names.join(", ")));
                }
            }
            "tool" => {
                let name = msg.tool_name.as_deref().unwrap_or("?");
                match msg.tool_call_decision.as_ref().map(|d| &d.kind) {
                    Some(ToolRunDecisionKind::AutoConfirm) => {
                        ui.colored_label(C_MAGENTA, format!("[Tool: {}] (AutoConfirm)", name));
                    }
                    Some(ToolRunDecisionKind::UserConfirm) => {
                        ui.colored_label(C_GREEN, format!("[Tool: {}] (UserConfirm)", name));
                    }
                    _ => {
                        ui.label(format!("[Tool: {}]", name));
                    }
                }
                // Show pretty command preview when args are available (stream messages).
                if let Some(ref args) = msg.tool_args {
                    gui_pretty::gui_pretty_command(ui, name, args);
                }
                // Show pretty result when content is parseable JSON (completed messages).
                // Fall back to plain text for non-JSON content.
                if !msg.content.is_empty() {
                    if let Ok(result) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                        gui_pretty::gui_pretty_result(ui, name, &result, msg.tool_args.as_ref());
                    } else if msg.tool_args.is_none() {
                        // Only show raw content if we didn't already show a pretty preview.
                        ui.label(format!("  {}", msg.content));
                    }
                }
                if let Some(reason) = msg
                    .tool_call_decision
                    .as_ref()
                    .and_then(|d| d.reason.as_deref())
                {
                    ui.colored_label(C_MAGENTA, reason);
                }
            }
            "system" => {
                if msg.content.is_empty() {
                    // Blank line separator between todo-loop boundaries.
                    ui.label("");
                } else if msg.content.contains("--- [") || msg.content.starts_with("Executing todo")
                {
                    ui.colored_label(C_CYAN, &msg.content);
                } else {
                    ui.label(&msg.content);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Font setup
// ---------------------------------------------------------------------------

fn load_cjk_font(fonts: &mut egui::FontDefinitions) {
    let paths: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
            "/System/Library/Fonts/ヒラギノ角ゴシック W4.ttc",
            "/System/Library/Fonts/ヒラギノ丸ゴシック W4.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "C:/Windows/Fonts/msgothic.ttc",
            "C:/Windows/Fonts/meiryo.ttc",
            "C:/Windows/Fonts/yugothr.ttc",
            "C:/Windows/Fonts/malgun.ttf",
        ]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        ]
    };

    for path in paths {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert(
                "CJK".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(data)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "CJK".to_owned());
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// eframe entry-point
// ---------------------------------------------------------------------------

pub fn run(config: Config, provider: LlmProvider) {
    let cwd = match std::fs::canonicalize(&config.working_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[GUI] bad working dir '{}': {}", config.working_dir, e);
            return;
        }
    };
    if let Err(e) = std::env::set_current_dir(&cwd) {
        eprintln!("[GUI] chdir failed: {}", e);
        return;
    }

    // Register the workspace root and tool execution limits for path
    // validation and child-process hardening.
    crate::tools::set_workspace_root(cwd.clone());
    crate::tools_process::set_tool_limits(crate::tools_process::ToolLimits {
        max_output_bytes: config.max_tool_output_bytes,
        tool_timeout_secs: config.tool_timeout_secs,
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 600.0])
            .with_min_inner_size([400.0, 300.0]),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "always-goofy-things",
        options,
        Box::new(|cc| {
            let mut fonts = egui::FontDefinitions::default();
            load_cjk_font(&mut fonts);
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(GuiApp::new(config, provider)))
        }),
    );
}

// ---------------------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------------------

impl eframe::App for GuiApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain streaming text into the stream message buffer.
        let mut had_update = false;
        {
            let mut buf = LLM_STREAM_BUF.lock().unwrap();
            let (ref mut reasoning, ref mut content, ref mut system, ref mut user) = *buf;
            let has_stream = !reasoning.is_empty()
                || !content.is_empty()
                || !system.is_empty()
                || !user.is_empty();
            if has_stream {
                // System messages come first (task headers, etc.).
                if !system.is_empty() {
                    let text = std::mem::take(system);
                    // Split preserving blank lines (\n\n -> empty entry).
                    let lines: Vec<&str> = text.split('\n').collect();
                    let last_nonempty = lines.iter().rposition(|l| !l.is_empty()).unwrap_or(0);
                    for (i, line) in lines.iter().enumerate() {
                        if i > last_nonempty {
                            break;
                        }
                        self.stream_message_buffer.push(Message {
                            role: "system".to_string(),
                            content: line.to_string(),
                            ..Default::default()
                        });
                    }
                    had_update = true;
                }
                // User messages from the reasoning loop (turn-start markers).
                if !user.is_empty() {
                    let text = std::mem::take(user);
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            // If send_message pre-pushed a user message, update it
                            // instead of creating a duplicate.
                            if self
                                .stream_message_buffer
                                .last()
                                .is_some_and(|m| m.role == "user")
                            {
                                let last = self.stream_message_buffer.last_mut().unwrap();
                                last.content = trimmed.to_string();
                            } else {
                                self.stream_message_buffer.push(Message {
                                    role: "user".to_string(),
                                    content: trimmed.to_string(),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    had_update = true;
                }
                // Reasoning and content go to separate messages so they
                // display in strict chronological order without the visual
                // glitch of two independently-expanding sections.
                if !reasoning.is_empty() {
                    let text = std::mem::take(reasoning);
                    // Append to the last assistant that already has reasoning,
                    // otherwise push a fresh one.
                    let can_append = self
                        .stream_message_buffer
                        .last()
                        .is_some_and(|m| m.role == "assistant" && m.reasoning_content.is_some());
                    if !can_append {
                        self.stream_message_buffer.push(Message {
                            role: "assistant".to_string(),
                            ..Default::default()
                        });
                    }
                    self.stream_message_buffer
                        .last_mut()
                        .unwrap()
                        .reasoning_content
                        .get_or_insert_default()
                        .push_str(&text);
                    had_update = true;
                }
                if !content.is_empty() {
                    let text = std::mem::take(content);
                    // LLM errors are written to the content buffer with a
                    // marker; route to conversation so they survive stream
                    // cleanup on completion.
                    if text.starts_with("[LLM Error]") {
                        self.conversation.push_str(&text);
                    } else {
                        // Push a fresh assistant for content when the last
                        // message has reasoning (so the two stay separate).
                        let last_has_reasoning =
                            self.stream_message_buffer.last().is_some_and(|m| {
                                m.role == "assistant"
                                    && m.reasoning_content.as_ref().is_some_and(|r| !r.is_empty())
                            });
                        if last_has_reasoning
                            || self
                                .stream_message_buffer
                                .last()
                                .is_none_or(|m| m.role != "assistant")
                        {
                            self.stream_message_buffer.push(Message {
                                role: "assistant".to_string(),
                                ..Default::default()
                            });
                        }
                        self.stream_message_buffer
                            .last_mut()
                            .unwrap()
                            .content
                            .push_str(&text);
                    }
                    had_update = true;
                }
            }
        }

        // Drain tool interactions.
        while let Ok(msg) = TOOL_INTERACT_CH.1.lock().unwrap().try_recv() {
            match msg {
                ToolInteractMsg::Prompt { notice, reply } => {
                    self.pending_confirms.push_back(PendingConfirm {
                        name: notice.name,
                        args: notice.args,
                        reply,
                    });
                    had_update = true;
                }
                ToolInteractMsg::Notice(notice) => {
                    // Push as a "tool" message so it renders in chronological
                    // order between assistant messages.
                    let args_str = serde_json::to_string(&notice.args).unwrap_or_default();
                    self.stream_message_buffer.push(Message {
                        role: "tool".to_string(),
                        content: format!("Args: {}", args_str),
                        tool_name: Some(notice.name),
                        tool_args: Some(notice.args),
                        tool_call_decision: Some(ToolRunDecision {
                            proceed: true,
                            kind: ToolRunDecisionKind::AutoConfirm,
                            reason: notice.reason,
                        }),
                        ..Default::default()
                    });
                    had_update = true;
                }
            }
        }
        if had_update {
            ctx.request_repaint();
        }

        // Check worker completion.
        let mut had_completion = false;
        if let Some(ref mut rx) = self.done_rx
            && let Ok((mut session, settings, metrics, done, err_msg)) = rx.try_recv()
        {
            // Only advance turn when the reasoning loop completed successfully.
            // On interruption (Stop / error) the user message is reused on the
            // next send, matching CLI Ctrl+C behaviour.
            if done {
                session.turn += 1;
            }
            if let Some(msg) = err_msg {
                self.conversation
                    .push_str(&format!("\n[LLM Error] {}\n", msg));
            }
            self.session = Some(session);
            self.settings = Some(settings);
            self.metrics = Some(metrics);
            self.done_rx = None;
            self.worker_handle = None;
            self.is_running = false;
            self.sync_model();

            // Rebuild stream buffer from completed session (keeps display consistent).
            rebuild_stream_from_session(
                &mut self.stream_message_buffer,
                self.session.as_ref(),
                self.session_msg_base,
            );
            self.pending_confirms.clear();
            self.focus_input = true;
            had_completion = true;
        }
        if had_completion {
            ctx.request_repaint();
        }

        // eframe sleeps when idle; keep waking it every 16ms while a worker is running.
        if self.is_running {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }

        // Auto-start todo mode on first frame
        if self.config.todo_mode > 0 && !self.todo_started && self.session.is_some() {
            self.todo_started = true;
            self.input_text = String::new();
            self.send_message(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // -- top bar --
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("model: {}", self.current_model));
                // LLM resource usage summary (token totals).
                if let Some(m) = &self.metrics {
                    let t = &m.totals;
                    ui.label(format!(
                        "In {} | Cache {} | Out {}",
                        fmt_tokens(t.in_normal + t.in_cached + t.in_cache_write + t.in_audio),
                        fmt_tokens(t.in_cached),
                        fmt_tokens(t.out_normal),
                    ));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.is_running && ui.button("\u{23f9} Stop").clicked() {
                        // Abort the worker task.
                        if let Some(handle) = self.worker_handle.take() {
                            handle.abort();
                        }
                        // Recover session if the worker finished just before this click.
                        if let Some(mut rx) = self.done_rx.take() {
                            let recovered = rx.try_recv().ok();
                            match recovered {
                                Some((mut session, settings, metrics, done, err_msg)) => {
                                    if done {
                                        session.turn += 1;
                                    }
                                    if let Some(msg) = err_msg {
                                        self.conversation
                                            .push_str(&format!("\n[LLM Error] {}\n", msg));
                                    }
                                    self.session = Some(session);
                                    self.settings = Some(settings);
                                    self.metrics = Some(metrics);
                                    // Rebuild stream buffer from completed session messages.
                                    rebuild_stream_from_session(
                                        &mut self.stream_message_buffer,
                                        self.session.as_ref(),
                                        self.session_msg_base,
                                    );
                                }
                                None => {
                                    // Worker was aborted; the pre-pushed user
                                    // message stays visible (matching CLI Ctrl+C).
                                }
                            }
                        }
                        self.is_running = false;
                        self.pending_confirms.clear();
                        self.focus_input = true;
                    }
                });
            });
        });

        // -- bottom: input --
        egui::Panel::bottom("input_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut te = egui::TextEdit::multiline(&mut self.input_text)
                    .frame(egui::Frame::default())
                    .hint_text(
                        "Describe your task... (Enter to send, Shift+Enter for newline, @file for attachments)",
                    )
                    .desired_width(f32::INFINITY)
                    .desired_rows(3);
                if self.is_running {
                    te = te.interactive(false);
                }
                let resp = ui.add(te);

                if self.focus_input {
                    resp.request_focus();
                    self.focus_input = false;
                }

                // Enter sends, Shift+Enter inserts newline (the TextEdit already
                // inserted the newline; we detect plain Enter here).
                let enter_send = ui
                    .input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift)
                    && !self.input_text.trim().is_empty()
                    && !self.is_running;

                let send_clicked = ui
                    .add_enabled(
                        !self.is_running && !self.input_text.trim().is_empty(),
                        egui::Button::new("Send"),
                    )
                    .clicked();

                if send_clicked || enter_send {
                    let ctx = ui.ctx().clone();
                    self.send_message(&ctx);
                }
            });
        });

        // -- centre: conversation --
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);

                    let mut turn = 0u32;

                    // Render completed session messages from previous turns.
                    if let Some(ref s) = self.session {
                        let limit = self.session_msg_base.min(s.messages.len());
                        for msg in s.messages.iter().skip(1).take(limit.saturating_sub(1)) {
                            Self::render_message(ui, msg, &mut turn);
                        }
                    }

                    // Render current turn from stream buffer (always pretty format).
                    for msg in &self.stream_message_buffer {
                        Self::render_message(ui, msg, &mut turn);
                    }

                    // Error messages (set by send_message on early returns).
                    if !self.conversation.is_empty() {
                        ui.colored_label(C_RED, &self.conversation);
                    }

                    // Interruption indicator: unanswered user without an error message.
                    // (LLM errors are displayed separately via self.conversation.)
                    if !self.is_running
                        && self.stream_message_buffer.is_empty()
                        && self.conversation.is_empty()
                        && self
                            .session
                            .as_ref()
                            .is_some_and(|s| s.messages.last().is_some_and(|m| m.role == "user"))
                    {
                        ui.colored_label(C_RED, "[interrupted]");
                    }

                    // Status indicators.
                    let has_session_msgs =
                        self.session.as_ref().is_some_and(|s| s.messages.len() > 1);
                    let is_idle = !self.is_running
                        && self.stream_message_buffer.is_empty()
                        && self.conversation.is_empty()
                        && !has_session_msgs;
                    if is_idle {
                        ui.colored_label(C_GRAY, "Type a task and press Enter.");
                    }
                    if self.is_running
                        && self.stream_message_buffer.is_empty()
                        && self.conversation.is_empty()
                    {
                        ui.colored_label(C_GRAY, "Waiting for response...");
                    }
                });
        });

        // -- modal: tool confirm (oldest unhandled request only) --
        let mut confirm_response: Option<ToolRunDecision> = None;
        if let Some(pending) = self.pending_confirms.front() {
            egui::Window::new("Confirm Tool Execution")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("LLM wants to execute:");
                    ui.separator();
                    ui.monospace(format!("Tool: {}", pending.name));
                    ui.monospace(format!(
                        "Args: {}",
                        serde_json::to_string_pretty(&pending.args)
                            .unwrap_or_else(|_| "<unprintable>".to_string())
                    ));
                    ui.separator();
                    // Pretty command preview (diff, etc.)
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .show(ui, |ui| {
                            gui_pretty::gui_pretty_command(ui, &pending.name, &pending.args);
                        });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Deny").clicked() {
                            confirm_response = Some(ToolRunDecision {
                                proceed: false,
                                kind: ToolRunDecisionKind::UserCancel,
                                reason: None,
                            });
                        }
                        if ui.button("Allow").clicked() {
                            confirm_response = Some(ToolRunDecision {
                                proceed: true,
                                kind: ToolRunDecisionKind::UserConfirm,
                                reason: None,
                            });
                        }
                    });
                });
        }
        if let Some(resp) = confirm_response
            && let Some(pending) = self.pending_confirms.pop_front()
        {
            let _ = pending.reply.send(resp);
        }
    }
}

/// Rebuild `stream_message_buffer` from session messages starting at `base`,
/// converting to display format: reasoning split from content, tool / user as-is.
fn rebuild_stream_from_session(buf: &mut Vec<Message>, session: Option<&Session>, base: usize) {
    buf.clear();
    if let Some(s) = session {
        let start = base.min(s.messages.len());
        for msg in &s.messages[start..] {
            match msg.role.as_str() {
                "assistant" => {
                    if let Some(ref rc) = msg.reasoning_content
                        && !rc.trim().is_empty()
                    {
                        buf.push(Message {
                            role: "assistant".to_string(),
                            reasoning_content: Some(rc.clone()),
                            ..Default::default()
                        });
                    }
                    let has_body = !msg.content.trim().is_empty() || msg.tool_calls.is_some();
                    if has_body {
                        let mut m = msg.clone();
                        m.reasoning_content = None;
                        buf.push(m);
                    }
                }
                "user" | "tool" | "system" => {
                    buf.push(msg.clone());
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

impl GuiApp {
    fn send_message(&mut self, ctx: &egui::Context) {
        let mut input = std::mem::take(&mut self.input_text);
        // Strip the trailing newline that Enter-to-send inserts in multiline mode.
        if input.ends_with('\n') {
            input.pop();
        }
        if input.trim().is_empty() && self.config.todo_mode == 0 {
            return;
        }

        // Slash commands.
        {
            let session = match self.session.as_mut() {
                Some(s) => s,
                None => {
                    self.conversation
                        .push_str("\n[Error] session is busy or lost (try stopping).\n");
                    self.focus_input = true;
                    return;
                }
            };
            let settings = match self.settings.as_mut() {
                Some(s) => s,
                None => {
                    self.conversation
                        .push_str("\n[Error] settings are busy or lost (try stopping).\n");
                    self.focus_input = true;
                    return;
                }
            };
            if let Some(result) = cmd::try_handle_slash_command(
                &input,
                session,
                settings,
                self.metrics.as_ref().unwrap_or(&Metrics::default()),
            ) {
                match result {
                    SlashCmdResult::NoAdvance => {}
                    SlashCmdResult::RewoundTo(target) => session.turn = target + 1,
                    SlashCmdResult::RestoredTo {
                        turn: target,
                        label,
                    } => {
                        session.turn = target + 1;
                        session.label = label.clone();
                        // Rebuild resource stats for the restored label.
                        if let Some(m) = self.metrics.as_mut()
                            && let Ok(records) = persistence::load_stats(&label)
                        {
                            *m = Metrics::from_records(records);
                        }
                    }
                    SlashCmdResult::Exit => std::process::exit(0),
                }
                self.sync_model();
                self.focus_input = true;
                return;
            }
        }

        // @file attachments.
        let (query_text, specs, parse_mode) = attach::parse_attached_files(&input);
        let attached_files = if !specs.is_empty() {
            match attach::validate_files(&specs) {
                Ok(()) => match attach::read_attached_files(&specs, parse_mode) {
                    Ok(files) => files,
                    Err(e) => {
                        self.conversation.push_str(&format!("\n[Error] {}\n", e));
                        self.focus_input = true;
                        return;
                    }
                },
                Err(missing) => {
                    self.conversation
                        .push_str(&format!("\n[File not found] {}\n", missing.join(", ")));
                    self.focus_input = true;
                    return;
                }
            }
        } else {
            Vec::new()
        };

        let mut session = match self.session.clone() {
            Some(s) => s,
            None => {
                self.conversation
                    .push_str("\n[Error] session is busy or lost (try stopping).\n");
                self.focus_input = true;
                return;
            }
        };
        let mut settings = match self.settings.clone() {
            Some(s) => s,
            None => {
                self.conversation
                    .push_str("\n[Error] settings are busy or lost (try stopping).\n");
                self.session = Some(session); // restore session
                self.focus_input = true;
                return;
            }
        };
        let mut metrics = match self.metrics.clone() {
            Some(m) => m,
            None => {
                self.conversation
                    .push_str("\n[Error] metrics are busy or lost (try stopping).\n");
                self.session = Some(session);
                self.settings = Some(settings);
                self.focus_input = true;
                return;
            }
        };

        let config = Arc::clone(&self.config);
        let provider = self.provider;
        let (done_tx, done_rx) = oneshot::channel();

        self.conversation.clear();
        self.is_running = true;

        // Snapshot: session messages before this turn are rendered from session;
        // the current turn's messages come from stream_message_buffer.
        self.session_msg_base = self.session.as_ref().map_or(0, |s| s.messages.len());

        // Push / update user message in session for display.
        // If the previous turn left an unanswered user message (Stop / error),
        // update it in-place so the turn counter stays correct.
        if let Some(ref mut s) = self.session {
            if s.messages.last().is_some_and(|m| m.role == "user") {
                let last = s.messages.last_mut().unwrap();
                last.content = query_text.clone();
                last.attached_files = attached_files.clone();
            } else {
                let user_msg = Message {
                    role: "user".to_string(),
                    content: query_text.clone(),
                    attached_files: attached_files.clone(),
                    ..Default::default()
                };
                s.messages.push(user_msg.clone());
                // In todo mode the reasoning loop pushes the real user message
                // (task description) via LLM_STREAM_BUF.3, so skip the empty one.
                if self.config.todo_mode == 0 {
                    self.stream_message_buffer.push(user_msg);
                }
            }
        }
        self.pending_confirms.clear();

        // This runs in ui (after logic), so kick off the 16ms wakeup here too.
        ctx.request_repaint_after(std::time::Duration::from_millis(16));

        let handle = tokio::spawn(async move {
            let (done, err_msg) = if config.todo_mode > 0 {
                match todo::run_todo_loop(
                    &config,
                    provider,
                    &mut settings,
                    &mut metrics,
                    &mut session,
                    query_text,
                    attached_files,
                )
                .await
                {
                    Ok(summary) => {
                        session.messages.push(Message {
                            role: "assistant".to_string(),
                            content: summary,
                            ..Default::default()
                        });
                        (true, None)
                    }
                    Err(e) => (false, Some(e.to_string())),
                }
            } else {
                match run_reasoning_loop(
                    &config,
                    provider,
                    &mut session,
                    &mut settings,
                    &mut metrics,
                    "main",
                    query_text,
                    attached_files,
                )
                .await
                {
                    Ok(end_reason) => (end_reason.is_completed(), None),
                    Err(e) => (false, Some(e.to_string())),
                }
            };

            let _ = done_tx.send((session, settings, metrics, done, err_msg));
        });

        self.done_rx = Some(done_rx);
        self.worker_handle = Some(handle);
    }
}
