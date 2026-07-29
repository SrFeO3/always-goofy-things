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
use crate::persistence;
use crate::startup::{self, Config};
use crate::tools::{
    TOOL_INTERACT_CH, ToolInteractMsg, ToolNotice, ToolRunDecision, ToolRunDecisionKind,
};
use crate::{LLM_STREAM_BUF, Message, Metrics, Session, Settings, run_reasoning_loop};

// egui color constants -- roughly match ANSI 32/35/90 as rendered in Alacritty's default theme.
const C_GREEN: egui::Color32 = egui::Color32::from_rgb(120, 170, 80);
const C_MAGENTA: egui::Color32 = egui::Color32::from_rgb(170, 120, 170);
const C_GRAY: egui::Color32 = egui::Color32::from_rgb(130, 130, 130);

// Unified live-event queue preserving arrival order during a turn.
enum LiveItem {
    Reasoning(String),
    Content(String),
    ToolNotice(ToolNotice),
}

struct GuiApp {
    config: Arc<Config>,
    provider: LlmProvider,
    session: Option<Session>,
    settings: Option<Settings>,
    metrics: Option<Metrics>,

    current_model: String,
    input_text: String,
    conversation: String,
    live_items: VecDeque<LiveItem>,
    pending_confirms: VecDeque<PendingConfirm>,

    done_rx: Option<oneshot::Receiver<(Session, Settings, Metrics, bool)>>,
    is_running: bool,
    focus_input: bool,
}

struct PendingConfirm {
    name: String,
    args: serde_json::Value,
    reply: oneshot::Sender<ToolRunDecision>,
}

impl GuiApp {
    fn new(config: Config, provider: LlmProvider) -> Self {
        let system_msg = startup::system_message();
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
            live_items: VecDeque::new(),
            pending_confirms: VecDeque::new(),
            done_rx: None,
            is_running: false,
            focus_input: true,
        }
    }

    fn sync_model(&mut self) {
        if let Some(ref s) = self.settings {
            self.current_model = s.llm_model.clone();
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
        // Drain streaming text into the unified live-items queue.
        let mut had_update = false;
        {
            let mut buf = LLM_STREAM_BUF.lock().unwrap();
            let (ref mut reasoning, ref mut content) = *buf;
            if !reasoning.is_empty() {
                let text = std::mem::take(reasoning);
                match self.live_items.back_mut() {
                    Some(LiveItem::Reasoning(existing)) => existing.push_str(&text),
                    _ => self.live_items.push_back(LiveItem::Reasoning(text)),
                }
                had_update = true;
            }
            if !content.is_empty() {
                let text = std::mem::take(content);
                match self.live_items.back_mut() {
                    Some(LiveItem::Content(existing)) => existing.push_str(&text),
                    _ => self.live_items.push_back(LiveItem::Content(text)),
                }
                had_update = true;
            }
        }

        // Drain tool interactions into the same queue to preserve arrival order.
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
                    self.live_items.push_back(LiveItem::ToolNotice(notice));
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
            && let Ok((mut session, settings, metrics, _done)) = rx.try_recv()
        {
            // Always advance turn; otherwise run_reasoning_loop will
            // overwrite the last user message on the next round.
            session.turn += 1;
            self.session = Some(session);
            self.settings = Some(settings);
            self.metrics = Some(metrics);
            self.done_rx = None;
            self.is_running = false;
            self.sync_model();
            self.live_items.clear();
            self.pending_confirms.clear();
            self.conversation.clear();
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
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // ── top bar ──
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("model: {}", self.current_model));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.is_running && ui.button("\u{23f9} Stop").clicked() {
                        // Recover session/settings/metrics if the worker finished before this click; otherwise leave them None.
                        if let Some(mut rx) = self.done_rx.take() {
                            let recovered = rx.try_recv().ok();
                            let (session, settings, metrics) = match recovered {
                                Some((session, settings, metrics, _done)) => {
                                    (Some(session), Some(settings), Some(metrics))
                                }
                                None => (None, None, None),
                            };
                            self.session = session.or_else(|| self.session.take());
                            self.settings = settings.or_else(|| self.settings.take());
                            self.metrics = metrics.or_else(|| self.metrics.take());
                        }
                        self.is_running = false;
                        self.live_items.clear();
                        self.pending_confirms.clear();
                        self.focus_input = true;
                    }
                });
            });
        });

        // ── bottom: input ──
        egui::Panel::bottom("input_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let mut te = egui::TextEdit::singleline(&mut self.input_text)
                    .hint_text("Describe your task... (@file for attachments)")
                    .desired_width(f32::INFINITY);
                if self.is_running {
                    te = te.interactive(false);
                }
                let resp = ui.add(te);

                if self.focus_input {
                    resp.request_focus();
                    self.focus_input = false;
                }

                let enter_sent = (resp.lost_focus()
                    || ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    && !self.input_text.trim().is_empty()
                    && !self.is_running;

                let send_clicked = ui
                    .add_enabled(
                        !self.is_running && !self.input_text.trim().is_empty(),
                        egui::Button::new("Send"),
                    )
                    .clicked();

                if send_clicked || enter_sent {
                    let ctx = ui.ctx().clone();
                    self.send_message(&ctx);
                }
            });
        });

        // ── centre: conversation ──
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    if let Some(ref s) = self.session {
                        let mut turn = 0u32;
                        for msg in s.messages.iter().skip(1) {
                            match msg.role.as_str() {
                                "user" => {
                                    turn += 1;
                                    ui.colored_label(
                                        C_GRAY,
                                        format!("\u{2500}\u{2500} Turn {} \u{2500}\u{2500}", turn),
                                    );
                                    ui.label(&msg.content);
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
                                        let names: Vec<&str> =
                                            tc.iter().map(|c| c.function.name.as_str()).collect();
                                        ui.label(format!(
                                            "Assistant > [Tool Call: {}]",
                                            names.join(", ")
                                        ));
                                    }
                                }
                                "tool" => {
                                    let name = msg.tool_name.as_deref().unwrap_or("?");
                                    match msg.tool_call_decision.as_ref().map(|d| &d.kind) {
                                        Some(ToolRunDecisionKind::AutoConfirm) => {
                                            ui.colored_label(
                                                C_MAGENTA,
                                                format!("[Tool: {}] (AutoConfirm)", name),
                                            );
                                        }
                                        Some(ToolRunDecisionKind::UserConfirm) => {
                                            ui.colored_label(
                                                C_GREEN,
                                                format!("[Tool: {}] (UserConfirm)", name),
                                            );
                                        }
                                        _ => {
                                            ui.label(format!("[Tool: {}]", name));
                                        }
                                    }
                                    ui.label(format!("  {}", msg.content));
                                }
                                _ => {}
                            }
                        }
                    }
                    if !self.conversation.is_empty() {
                        ui.label(&self.conversation);
                    }
                    for item in &self.live_items {
                        match item {
                            LiveItem::Reasoning(text) => {
                                ui.colored_label(C_GREEN, text);
                            }
                            LiveItem::Content(text) => {
                                ui.label(text);
                            }
                            LiveItem::ToolNotice(notice) => {
                                ui.separator();
                                ui.colored_label(
                                    C_MAGENTA,
                                    format!("Auto-confirmed: {}", notice.name),
                                );
                                ui.monospace(format!(
                                    "Args: {}",
                                    serde_json::to_string(&notice.args).unwrap_or_default()
                                ));
                                if let Some(ref r) = notice.reason {
                                    ui.colored_label(C_MAGENTA, r);
                                }
                                ui.add_space(2.0);
                            }
                        }
                    }
                    if self.is_running && self.live_items.is_empty() && self.conversation.is_empty()
                    {
                        ui.colored_label(C_GRAY, "Waiting for response...");
                    }
                    if self.conversation.is_empty()
                        && self.live_items.is_empty()
                        && !self.is_running
                    {
                        ui.colored_label(C_GRAY, "Type a task and press Enter or click Send.");
                    }
                });
        });

        // ── modal: tool confirm (oldest unhandled request only) ──
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

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

impl GuiApp {
    fn send_message(&mut self, ctx: &egui::Context) {
        let input = std::mem::take(&mut self.input_text);
        if input.trim().is_empty() {
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
            if let Some(result) = cmd::try_handle_slash_command(&input, session, settings) {
                match result {
                    SlashCmdResult::NoAdvance => {}
                    SlashCmdResult::RewoundTo(target) => session.turn = target + 1,
                    SlashCmdResult::RestoredTo {
                        turn: target,
                        label,
                    } => {
                        session.turn = target + 1;
                        session.label = label;
                    }
                    SlashCmdResult::Exit => std::process::exit(0),
                }
                self.sync_model();
                self.focus_input = true;
                return;
            }
        }

        // @file attachments.
        let (query_text, raw_paths, parse_mode) = attach::parse_attached_files(&input);
        let attached_files = if !raw_paths.is_empty() {
            match attach::validate_files(&raw_paths) {
                Ok(()) => match attach::read_attached_files(&raw_paths, parse_mode) {
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
        let mut settings = match self.settings.take() {
            Some(s) => s,
            None => {
                self.conversation
                    .push_str("\n[Error] settings are busy or lost (try stopping).\n");
                self.session = Some(session); // restore session
                self.focus_input = true;
                return;
            }
        };
        let mut metrics = match self.metrics.take() {
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

        self.is_running = true;
        self.live_items.clear();
        self.pending_confirms.clear();

        // Pre-push user message to canonical session for immediate display.
        if let Some(ref mut s) = self.session {
            s.messages.push(Message {
                role: "user".to_string(),
                content: query_text.clone(),
                attached_files: attached_files.clone(),
                ..Default::default()
            });
        }

        // This runs in ui (after logic), so kick off the 16ms wakeup here too.
        ctx.request_repaint_after(std::time::Duration::from_millis(16));

        tokio::spawn(async move {
            let done = run_reasoning_loop(
                &config,
                provider,
                &mut session,
                &mut settings,
                &mut metrics,
                query_text,
                attached_files,
            )
            .await
            .unwrap_or(false);

            let _ = done_tx.send((session, settings, metrics, done));
        });

        self.done_rx = Some(done_rx);
    }
}
