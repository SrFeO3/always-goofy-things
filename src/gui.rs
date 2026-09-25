#![cfg(feature = "gui")]

//! GUI frontend for the application.
//!
//! Provides a native egui interface that runs the CLI child process,
//! renders its ANSI output, and displays the todo workspace files.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui;

use crate::startup::Config;

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const CHILD_ENV: &str = "AGT_GUI_CHILD";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OutputStyle {
    foreground: Option<egui::Color32>,
    background: Option<egui::Color32>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

#[derive(Debug, Clone)]
struct OutputSpan {
    text: String,
    style: OutputStyle,
}

#[derive(Debug, Default, Clone)]
struct OutputLine {
    spans: Vec<OutputSpan>,
}

#[derive(Debug, Default)]
struct AnsiParser {
    style: OutputStyle,
    pending: String,
}

impl AnsiParser {
    fn feed(&mut self, text: &str, output: &mut Vec<OutputLine>, output_bytes: &mut usize) {
        self.pending.push_str(text);
        let input = std::mem::take(&mut self.pending);
        let chars: Vec<char> = input.chars().collect();
        let mut index = 0;
        ensure_output_line(output);

        while index < chars.len() {
            let ch = chars[index];
            if ch == '\u{1b}' {
                if index + 1 >= chars.len() {
                    self.pending = chars[index..].iter().collect();
                    break;
                }
                match chars[index + 1] {
                    '[' => {
                        let mut end = index + 2;
                        while end < chars.len() && !chars[end].is_ascii_alphabetic() {
                            if chars[end] == '\u{1b}' {
                                break;
                            }
                            end += 1;
                        }
                        if end >= chars.len() {
                            self.pending = chars[index..].iter().collect();
                            break;
                        }
                        let sequence: String = chars[index + 2..end].iter().collect();
                        if chars[end] == 'm' {
                            self.apply_sgr(&sequence);
                        }
                        index = end + 1;
                        continue;
                    }
                    ']' => {
                        let mut end = index + 2;
                        while end < chars.len() {
                            if chars[end] == '\u{7}' {
                                end += 1;
                                break;
                            }
                            if chars[end] == '\u{1b}'
                                && end + 1 < chars.len()
                                && chars[end + 1] == '\\'
                            {
                                end += 2;
                                break;
                            }
                            end += 1;
                        }
                        if end >= chars.len() {
                            self.pending = chars[index..].iter().collect();
                            break;
                        }
                        index = end;
                        continue;
                    }
                    _ => {
                        index += 2;
                        continue;
                    }
                }
            }
            index += 1;
            if ch == '\n' {
                output.push(OutputLine::default());
                continue;
            }
            if ch == '\r' {
                continue;
            }
            let line = output.last_mut().expect("output line exists");
            if let Some(span) = line.spans.last_mut()
                && span.style == self.style
            {
                span.text.push(ch);
            } else {
                line.spans.push(OutputSpan {
                    text: ch.to_string(),
                    style: self.style,
                });
            }
            *output_bytes += ch.len_utf8();
        }
    }

    fn finish(&mut self, output: &mut Vec<OutputLine>, output_bytes: &mut usize) {
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.feed(&pending, output, output_bytes);
        }
    }

    fn apply_sgr(&mut self, params: &str) {
        let values: Vec<u16> = if params.is_empty() {
            vec![0]
        } else {
            params
                .split(';')
                .map(|part| part.parse().unwrap_or(0))
                .collect()
        };
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => self.style = OutputStyle::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                30..=37 => self.style.foreground = ansi_color(values[index] - 30, false),
                39 => self.style.foreground = None,
                40..=47 => self.style.background = ansi_color(values[index] - 40, false),
                49 => self.style.background = None,
                90..=97 => self.style.foreground = ansi_color(values[index] - 90, true),
                100..=107 => self.style.background = ansi_color(values[index] - 100, true),
                38 | 48 => {
                    let is_foreground = values[index] == 38;
                    let Some((color, next_index)) = parse_extended_color(&values, index + 1) else {
                        break;
                    };
                    if is_foreground {
                        self.style.foreground = Some(color);
                    } else {
                        self.style.background = Some(color);
                    }
                    index = next_index;
                }
                _ => {}
            }
            index += 1;
        }
    }
}

fn ensure_output_line(output: &mut Vec<OutputLine>) {
    if output.is_empty() {
        output.push(OutputLine::default());
    }
}

fn parse_extended_color(values: &[u16], start: usize) -> Option<(egui::Color32, usize)> {
    match values.get(start).copied()? {
        5 => {
            let value = *values.get(start + 1)?;
            Some((ansi_256_color(value), start + 2))
        }
        2 => {
            let red = (*values.get(start + 1)?).min(255) as u8;
            let green = (*values.get(start + 2)?).min(255) as u8;
            let blue = (*values.get(start + 3)?).min(255) as u8;
            Some((egui::Color32::from_rgb(red, green, blue), start + 4))
        }
        _ => None,
    }
}

fn ansi_color(index: u16, bright: bool) -> Option<egui::Color32> {
    let colors = if bright {
        [
            egui::Color32::from_gray(130),
            egui::Color32::from_rgb(255, 92, 92),
            egui::Color32::from_rgb(92, 255, 92),
            egui::Color32::from_rgb(255, 255, 92),
            egui::Color32::from_rgb(92, 92, 255),
            egui::Color32::from_rgb(255, 92, 255),
            egui::Color32::from_rgb(92, 255, 255),
            egui::Color32::from_gray(230),
        ]
    } else {
        [
            egui::Color32::from_gray(60),
            egui::Color32::from_rgb(205, 65, 65),
            egui::Color32::from_rgb(65, 205, 65),
            egui::Color32::from_rgb(205, 205, 65),
            egui::Color32::from_rgb(65, 65, 205),
            egui::Color32::from_rgb(205, 65, 205),
            egui::Color32::from_rgb(65, 205, 205),
            egui::Color32::from_gray(180),
        ]
    };
    colors.get(index as usize).copied()
}

fn ansi_256_color(value: u16) -> egui::Color32 {
    match value {
        0..=7 => ansi_color(value, false).unwrap_or(egui::Color32::WHITE),
        8..=15 => ansi_color(value - 8, true).unwrap_or(egui::Color32::WHITE),
        16..=231 => {
            let value = value - 16;
            let red = (value / 36) % 6;
            let green = (value / 6) % 6;
            let blue = value % 6;
            let component = |value: u16| if value == 0 { 0 } else { 55 + value * 40 } as u8;
            egui::Color32::from_rgb(component(red), component(green), component(blue))
        }
        232..=255 => {
            let gray = (8 + (value - 232) * 10).min(255) as u8;
            egui::Color32::from_gray(gray)
        }
        _ => egui::Color32::WHITE,
    }
}

fn foreground_for_style(style: OutputStyle) -> Option<egui::Color32> {
    style
        .foreground
        .or_else(|| style.background.map(|_| egui::Color32::WHITE))
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
        if trimmed.starts_with("- [x]") {
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

fn is_todo_mode(todo_mode: u8) -> bool {
    matches!(todo_mode, 1 | 2)
}

fn install_fonts(ctx: &egui::Context) {
    let fonts = system_fonts::find_from_presets(
        [
            system_fonts::FontPreset::Japanese,
            system_fonts::FontPreset::Latin,
        ],
        system_fonts::FontStyle::Sans,
    );
    let mut definitions = egui::FontDefinitions::default();
    let mut font_keys = Vec::with_capacity(fonts.len());

    for font in fonts {
        let bytes = match font.source {
            system_fonts::FoundFontSource::Path(path) => match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            },
            system_fonts::FoundFontSource::Bytes(bytes) => bytes.to_vec(),
        };
        definitions.font_data.insert(
            font.key.clone(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        font_keys.push(font.key);
    }

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let family_fonts = definitions.families.entry(family).or_default();
        for key in font_keys.iter().rev() {
            if !family_fonts.contains(key) {
                family_fonts.insert(0, key.clone());
            }
        }
    }

    ctx.set_fonts(definitions);
}

struct GuiShell {
    process: ProcessReader,
    input: String,
    output: Vec<OutputLine>,
    output_bytes: usize,
    stdout_parser: AnsiParser,
    stderr_parser: AnsiParser,
    stdout_pending: Vec<u8>,
    stderr_pending: Vec<u8>,
    workspace: Option<WorkspaceInspector>,
    status: String,
    running: bool,
    focus_input: bool,
    output_requested_repaint: bool,
}

impl GuiShell {
    fn new(config: &Config) -> Result<Self> {
        let process = ProcessReader::start()?;
        let workspace = if is_todo_mode(config.todo_mode) {
            let root = std::fs::canonicalize(&config.working_dir).unwrap_or_else(|_| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(&config.working_dir)
            });
            Some(WorkspaceInspector::new(root))
        } else {
            None
        };
        Ok(Self {
            process,
            input: String::new(),
            output: Vec::new(),
            output_bytes: 0,
            stdout_parser: AnsiParser::default(),
            stderr_parser: AnsiParser::default(),
            stdout_pending: Vec::new(),
            stderr_pending: Vec::new(),
            workspace,
            status: "running".to_string(),
            running: true,
            focus_input: true,
            output_requested_repaint: false,
        })
    }

    fn append_output_bytes(
        output: &mut Vec<OutputLine>,
        output_bytes: &mut usize,
        bytes: &[u8],
        pending: &mut Vec<u8>,
        parser: &mut AnsiParser,
    ) {
        pending.extend_from_slice(bytes);
        match String::from_utf8(pending.clone()) {
            Ok(text) => {
                pending.clear();
                parser.feed(&text, output, output_bytes);
            }
            Err(error) => {
                let valid_up_to = error.utf8_error().valid_up_to();
                if valid_up_to > 0 {
                    let text = String::from_utf8_lossy(&pending[..valid_up_to]).into_owned();
                    pending.drain(..valid_up_to);
                    parser.feed(&text, output, output_bytes);
                }
                if error.utf8_error().error_len().is_some() {
                    pending.drain(..1);
                    parser.feed("\u{fffd}", output, output_bytes);
                }
            }
        }
    }

    fn append_internal_text(&mut self, text: &str) {
        self.stdout_parser
            .feed(text, &mut self.output, &mut self.output_bytes);
    }

    fn trim_output(&mut self) {
        while self.output_bytes > OUTPUT_LIMIT && !self.output.is_empty() {
            let mut remove = self.output_bytes - OUTPUT_LIMIT;
            let first = &mut self.output[0];
            while remove > 0 && !first.spans.is_empty() {
                let span = &mut first.spans[0];
                if span.text.len() <= remove {
                    remove -= span.text.len();
                    self.output_bytes -= span.text.len();
                    first.spans.remove(0);
                } else {
                    let mut cut = remove;
                    while cut > 0 && !span.text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    span.text.drain(..cut);
                    self.output_bytes -= remove;
                    remove = 0;
                }
            }
            if first.spans.is_empty() {
                self.output.remove(0);
            }
            if remove > 0 {
                break;
            }
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.process.events.try_recv() {
            match event {
                ProcessEvent::Stdout(bytes) => {
                    let mut pending = std::mem::take(&mut self.stdout_pending);
                    Self::append_output_bytes(
                        &mut self.output,
                        &mut self.output_bytes,
                        &bytes,
                        &mut pending,
                        &mut self.stdout_parser,
                    );
                    self.stdout_pending = pending;
                }
                ProcessEvent::Stderr(bytes) => {
                    let mut pending = std::mem::take(&mut self.stderr_pending);
                    Self::append_output_bytes(
                        &mut self.output,
                        &mut self.output_bytes,
                        &bytes,
                        &mut pending,
                        &mut self.stderr_parser,
                    );
                    self.stderr_pending = pending;
                }
                ProcessEvent::StreamClosed(is_stdout) => {
                    let (pending, parser) = if is_stdout {
                        (&mut self.stdout_pending, &mut self.stdout_parser)
                    } else {
                        (&mut self.stderr_pending, &mut self.stderr_parser)
                    };
                    if !pending.is_empty() {
                        let text = String::from_utf8_lossy(pending).into_owned();
                        pending.clear();
                        let (output, output_bytes) = (&mut self.output, &mut self.output_bytes);
                        parser.feed(&text, output, output_bytes);
                    }
                    let (output, output_bytes) = (&mut self.output, &mut self.output_bytes);
                    parser.finish(output, output_bytes);
                }
                ProcessEvent::ReadError(error) => {
                    self.append_internal_text(&format!("\n[process read error] {error}\n"));
                }
            }
        }
        self.trim_output();
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
            self.append_internal_text(&format!("\n[CLI process exited: {status}]\n"));
        }
    }

    fn draw_workspace(&mut self, ui: &mut egui::Ui) {
        let Some(workspace) = self.workspace.as_mut() else {
            return;
        };

        ui.heading("Workspace");
        ui.horizontal_wrapped(|ui| {
            for (tab, label) in [
                (WorkspaceTab::Todo, "Todo"),
                (WorkspaceTab::NextTask, "Next task"),
                (WorkspaceTab::Handover, "Handover"),
                (WorkspaceTab::Artifacts, "Artifacts"),
            ] {
                if ui
                    .selectable_label(workspace.selected == tab, label)
                    .clicked()
                {
                    workspace.selected = tab;
                }
            }
        });
        ui.separator();

        let summary = &workspace.task_summary;
        ui.label(format!(
            "Progress: {} / {} ({} pending)",
            summary.completed, summary.total, summary.pending
        ));
        if let Some(next) = &summary.next_task {
            ui.label(format!("Next: {next}"));
        }
        ui.separator();

        match workspace.selected {
            WorkspaceTab::Todo => text_view(ui, &workspace.todo_md),
            WorkspaceTab::NextTask => text_view(ui, &workspace.next_task_md),
            WorkspaceTab::Handover => text_view(ui, &workspace.handover_md),
            WorkspaceTab::Artifacts => {
                egui::ScrollArea::vertical()
                    .id_salt("artifact_list")
                    .max_height(100.0)
                    .show(ui, |ui| {
                        if workspace.artifacts.is_empty() {
                            ui.label("(no artifacts)");
                        }
                        for path in workspace.artifacts.clone() {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            let selected = workspace.selected_artifact.as_ref() == Some(&path);
                            if ui.selectable_label(selected, name).clicked() {
                                workspace.selected_artifact = Some(path.clone());
                            }
                        }
                    });
                ui.separator();
                text_view(ui, &workspace.selected_artifact_text());
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
                self.output_bytes = 0;
            }
        });
    }
}

fn draw_output(ui: &mut egui::Ui, lines: &[OutputLine]) {
    let previous_spacing = ui.spacing().item_spacing;
    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);

    for line in lines {
        let trailing_background = line.spans.last().and_then(|span| span.style.background);
        ui.horizontal_wrapped(|ui| {
            if line.spans.is_empty() {
                ui.label("");
                return;
            }

            let line_height = ui.text_style_height(&egui::TextStyle::Monospace);
            let mut parts = Vec::with_capacity(line.spans.len());
            for span in &line.spans {
                let mut text = egui::RichText::new(&span.text)
                    .monospace()
                    .line_height(Some(line_height));
                if let Some(color) = foreground_for_style(span.style) {
                    text = text.color(color);
                }
                if let Some(color) = span.style.background {
                    text = text.background_color(color);
                }
                if span.style.bold {
                    text = text.strong();
                }
                if span.style.dim {
                    text = text.color(egui::Color32::LIGHT_GRAY);
                }
                if span.style.italic {
                    text = text.italics();
                }
                if span.style.underline {
                    text = text.underline();
                }
                parts.push(text);
            }
            let mut job = egui::text::LayoutJob::default();
            for text in parts {
                text.append_to(
                    &mut job,
                    ui.style(),
                    egui::FontSelection::Default,
                    ui.text_valign(),
                );
            }
            let last_rect = ui.label(job).rect;

            if let Some(background) = trailing_background {
                let remaining_width = ui.available_width();
                if remaining_width > 0.0 {
                    let fill_rect = egui::Rect::from_min_size(
                        egui::pos2(last_rect.max.x, last_rect.min.y),
                        egui::vec2(remaining_width, last_rect.height()),
                    );
                    ui.painter().rect_filled(fill_rect, 0.0, background);
                }
            }
        });
    }

    ui.spacing_mut().item_spacing = previous_spacing;
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
        if let Some(workspace) = &mut self.workspace {
            workspace.refresh_if_due();
        }

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

        if self.workspace.is_some() {
            egui::Panel::right("workspace")
                .default_size(360.0)
                .show(root_ui, |ui| self.draw_workspace(ui));
        }

        egui::CentralPanel::default().show(root_ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("cli_output")
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| draw_output(ui, &self.output));
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
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
}

#[cfg(test)]
#[path = "tests/gui_test.rs"]
mod tests;
