#![cfg(feature = "gui")]

//! Minimal GUI process shell.
//!
//! The GUI does not link or call the CLI application layer. It starts the
//! same executable with `AGT_GUI_CHILD=1`, relays stdin/stdout/stderr, and
//! provides a read-only view of the workspace files used by todo mode.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui;
use regex::Regex;

use crate::startup::Config;

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const CHILD_ENV: &str = "AGT_GUI_CHILD";

static ANSI_RE: OnceLock<Regex> = OnceLock::new();

fn ansi_re() -> &'static Regex {
    ANSI_RE.get_or_init(|| Regex::new(r"\x1B\[[0-?]*[ -/]*[@-~]").expect("valid ANSI regex"))
}

fn strip_ansi(text: &str) -> String {
    ansi_re().replace_all(text, "").into_owned()
}

#[derive(Debug)]
enum ProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    StreamClosed(bool),
    ReadError(String),
}

struct ProcessReader {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<ProcessEvent>,
}

impl ProcessReader {
    fn start() -> Result<Self> {
        let exe = std::env::current_exe().context("failed to locate current executable")?;
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        let mut command = Command::new(exe);
        command
            .args(args)
            .env(CHILD_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .context("failed to start CLI child process")?;
        let stdin = child.stdin.take();
        let (tx, rx) = mpsc::channel();

        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, tx.clone(), true);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, tx, false);
        }

        Ok(Self {
            child,
            stdin,
            events: rx,
        })
    }

    fn send_input(&mut self, input: &str) -> std::io::Result<()> {
        if let Some(stdin) = self.stdin.as_mut() {
            stdin.write_all(input.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
        }
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(stdin) = self.stdin.take() {
            drop(stdin);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn poll(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

impl Drop for ProcessReader {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_reader<R>(mut reader: R, tx: Sender<ProcessEvent>, is_stdout: bool)
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let event = if is_stdout {
                        ProcessEvent::Stdout(buffer[..n].to_vec())
                    } else {
                        ProcessEvent::Stderr(buffer[..n].to_vec())
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(ProcessEvent::ReadError(e.to_string()));
                    break;
                }
            }
        }
        let _ = tx.send(ProcessEvent::StreamClosed(is_stdout));
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTab {
    Todo,
    NextTask,
    Handover,
    Artifacts,
}

#[derive(Debug, Default, Clone)]
struct TaskSummary {
    total: usize,
    completed: usize,
    pending: usize,
    next_task: Option<String>,
}

struct WorkspaceInspector {
    root: PathBuf,
    selected: WorkspaceTab,
    todo_md: String,
    next_task_md: String,
    handover_md: String,
    artifacts: Vec<PathBuf>,
    selected_artifact: Option<PathBuf>,
    task_summary: TaskSummary,
    last_refresh: Instant,
}

impl WorkspaceInspector {
    fn new(root: PathBuf) -> Self {
        let mut inspector = Self {
            root,
            selected: WorkspaceTab::Todo,
            todo_md: String::new(),
            next_task_md: String::new(),
            handover_md: String::new(),
            artifacts: Vec::new(),
            selected_artifact: None,
            task_summary: TaskSummary::default(),
            last_refresh: Instant::now() - REFRESH_INTERVAL,
        };
        inspector.refresh();
        inspector
    }

    fn refresh_if_due(&mut self) {
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        self.todo_md = read_text(&self.root.join("todo.md"));
        self.next_task_md = read_text(&self.root.join("next-task.md"));
        self.handover_md = read_text(&self.root.join("artifacts").join("handover.md"));
        self.task_summary = parse_task_summary(&self.todo_md);
        self.artifacts = list_files(&self.root.join("artifacts"));
        if self
            .selected_artifact
            .as_ref()
            .is_none_or(|path| !self.artifacts.contains(path))
        {
            self.selected_artifact = self.artifacts.first().cloned();
        }
    }

    fn selected_artifact_text(&self) -> String {
        self.selected_artifact
            .as_ref()
            .map(|path| read_text(path))
            .unwrap_or_else(|| "(no artifact selected)".to_string())
    }
}

fn read_text(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "(not found)".to_string(),
        Err(e) => format!("(read error: {e})"),
    }
}

fn list_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

fn parse_task_summary(todo_md: &str) -> TaskSummary {
    let mut summary = TaskSummary::default();
    let mut in_tasks = false;
    for line in todo_md.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## Tasks") {
            in_tasks = true;
            continue;
        }
        if in_tasks && trimmed.starts_with("##") {
            break;
        }
        if !in_tasks {
            continue;
        }
        if let Some(_description) = trimmed.strip_prefix("- [x]") {
            summary.total += 1;
            summary.completed += 1;
        } else if let Some(description) = trimmed.strip_prefix("- [ ]") {
            summary.total += 1;
            summary.pending += 1;
            if summary.next_task.is_none() {
                summary.next_task = Some(description.trim().to_string());
            }
        }
    }
    summary
}

struct GuiShell {
    process: ProcessReader,
    input: String,
    output: String,
    workspace: WorkspaceInspector,
    status: String,
    running: bool,
    focus_input: bool,
    output_requested_repaint: bool,
    stdout_pending: Vec<u8>,
    stderr_pending: Vec<u8>,
}

impl GuiShell {
    fn new(config: &Config) -> Result<Self> {
        let root = std::fs::canonicalize(&config.working_dir).unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(&config.working_dir)
        });
        let process = ProcessReader::start()?;
        Ok(Self {
            process,
            input: String::new(),
            output: String::new(),
            workspace: WorkspaceInspector::new(root),
            status: "running".to_string(),
            running: true,
            focus_input: true,
            output_requested_repaint: false,
            stdout_pending: Vec::new(),
            stderr_pending: Vec::new(),
        })
    }

    fn append_output_bytes(&mut self, bytes: &[u8], pending: &mut Vec<u8>) {
        pending.extend_from_slice(bytes);
        match String::from_utf8(pending.clone()) {
            Ok(text) => {
                pending.clear();
                self.append_output_text(&text);
            }
            Err(error) => {
                let valid_up_to = error.utf8_error().valid_up_to();
                if valid_up_to > 0 {
                    let text = String::from_utf8_lossy(&pending[..valid_up_to]).into_owned();
                    pending.drain(..valid_up_to);
                    self.append_output_text(&text);
                }
            }
        }
    }

    fn append_output_text(&mut self, text: &str) {
        let text = strip_ansi(text);
        self.output.push_str(&text);
        self.output_requested_repaint = true;
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.process.events.try_recv() {
            match event {
                ProcessEvent::Stdout(bytes) => {
                    let mut pending = std::mem::take(&mut self.stdout_pending);
                    self.append_output_bytes(&bytes, &mut pending);
                    self.stdout_pending = pending;
                }
                ProcessEvent::Stderr(bytes) => {
                    let mut pending = std::mem::take(&mut self.stderr_pending);
                    self.append_output_bytes(&bytes, &mut pending);
                    self.stderr_pending = pending;
                }
                ProcessEvent::StreamClosed(is_stdout) => {
                    if is_stdout && !self.stdout_pending.is_empty() {
                        let text = String::from_utf8_lossy(&self.stdout_pending).into_owned();
                        self.stdout_pending.clear();
                        self.append_output_text(&text);
                    } else if !is_stdout && !self.stderr_pending.is_empty() {
                        let text = String::from_utf8_lossy(&self.stderr_pending).into_owned();
                        self.stderr_pending.clear();
                        self.append_output_text(&text);
                    }
                }
                ProcessEvent::ReadError(error) => {
                    self.output
                        .push_str(&format!("\n[process read error] {error}\n"));
                    self.output_requested_repaint = true;
                }
            }
        }
        if self.output.len() > OUTPUT_LIMIT {
            let mut cut = self.output.len() - OUTPUT_LIMIT;
            while cut < self.output.len() && !self.output.is_char_boundary(cut) {
                cut += 1;
            }
            self.output.drain(..cut);
        }
    }

    fn send_input(&mut self) {
        if self.input.trim().is_empty() || !self.running {
            return;
        }
        let input = std::mem::take(&mut self.input);
        if let Err(e) = self.process.send_input(&input) {
            self.status = format!("input error: {e}");
        }
        self.focus_input = true;
    }

    fn stop(&mut self) {
        self.process.stop();
        self.running = false;
        self.status = "stopped".to_string();
    }

    fn update_process(&mut self) {
        if self.running
            && let Some(status) = self.process.poll()
        {
            self.running = false;
            self.status = format!("exited ({status})");
            self.output
                .push_str(&format!("\n[CLI process exited: {status}]\n"));
            self.output_requested_repaint = true;
        }
    }

    fn draw_workspace(&mut self, ui: &mut egui::Ui) {
        ui.heading("Workspace");
        ui.horizontal_wrapped(|ui| {
            for (tab, label) in [
                (WorkspaceTab::Todo, "Todo"),
                (WorkspaceTab::NextTask, "Next task"),
                (WorkspaceTab::Handover, "Handover"),
                (WorkspaceTab::Artifacts, "Artifacts"),
            ] {
                if ui
                    .selectable_label(self.workspace.selected == tab, label)
                    .clicked()
                {
                    self.workspace.selected = tab;
                }
            }
        });
        ui.separator();

        let summary = &self.workspace.task_summary;
        ui.label(format!(
            "Progress: {} / {} ({} pending)",
            summary.completed, summary.total, summary.pending
        ));
        if let Some(next) = &summary.next_task {
            ui.label(format!("Next: {next}"));
        }
        ui.separator();

        match self.workspace.selected {
            WorkspaceTab::Todo => text_view(ui, &self.workspace.todo_md),
            WorkspaceTab::NextTask => text_view(ui, &self.workspace.next_task_md),
            WorkspaceTab::Handover => text_view(ui, &self.workspace.handover_md),
            WorkspaceTab::Artifacts => {
                egui::ScrollArea::vertical()
                    .id_salt("artifact_list")
                    .max_height(100.0)
                    .show(ui, |ui| {
                        if self.workspace.artifacts.is_empty() {
                            ui.label("(no artifacts)");
                        }
                        for path in self.workspace.artifacts.clone() {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            let selected = self.workspace.selected_artifact.as_ref() == Some(&path);
                            if ui.selectable_label(selected, name).clicked() {
                                self.workspace.selected_artifact = Some(path.clone());
                            }
                        }
                    });
                ui.separator();
                text_view(ui, &self.workspace.selected_artifact_text());
            }
        }
    }

    fn draw_input(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .desired_width(f32::INFINITY)
                    .hint_text("Message, slash command, or y/n"),
            );
            if self.focus_input {
                response.request_focus();
                self.focus_input = false;
            }
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Send").clicked() || enter {
                self.send_input();
            }
            if ui.button("Stop").clicked() {
                self.stop();
            }
            if ui.button("Clear").clicked() {
                self.output.clear();
            }
        });
    }
}

fn text_view(ui: &mut egui::Ui, text: &str) {
    egui::ScrollArea::vertical()
        .id_salt("workspace_text")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.monospace(text);
        });
}

impl eframe::App for GuiShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.update_process();
        self.workspace.refresh_if_due();

        if self.running || self.output_requested_repaint {
            ctx.request_repaint_after(Duration::from_millis(50));
            self.output_requested_repaint = false;
        }
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("status").show(root_ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("status: {}", self.status));
                if self.running {
                    ui.label("(CLI child is running)");
                }
            });
        });

        egui::Panel::bottom("input").show(root_ui, |ui| self.draw_input(ui));

        egui::Panel::right("workspace")
            .default_size(360.0)
            .show(root_ui, |ui| self.draw_workspace(ui));

        egui::CentralPanel::default().show(root_ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("cli_output")
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.monospace(&self.output);
                });
        });
    }
}

/// Start the GUI shell. This function is compiled only with the `gui` feature.
pub fn run(config: Config) -> Result<()> {
    let app = GuiShell::new(&config)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([700.0, 450.0]),
        ..Default::default()
    };
    eframe::run_native(
        "always-goofy-things",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
}
