use eframe::egui;

use super::{OutputStyle, foreground_for_style, is_todo_mode, plain_enter_pressed};

#[test]
fn workspace_is_enabled_only_for_todo_modes() {
    assert!(!is_todo_mode(0));
    assert!(is_todo_mode(1));
    assert!(is_todo_mode(2));
}

// --- Multiline input: Enter sends, Ctrl+O inserts a newline ---

const NEWLINE_KEY: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::O);

fn multiline_edit(input: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::multiline(input).return_key(NEWLINE_KEY)
}

fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

/// Runs a focused input field over two frames: the first requests focus, the
/// second applies `events` and reports whether a plain Enter would send.
fn run_input_frames(events: Vec<egui::Event>) -> (String, bool) {
    let ctx = egui::Context::default();
    let mut input = String::from("ab");

    let _ = ctx.run_ui(
        egui::RawInput {
            focused: true,
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                multiline_edit(&mut input).show(ui).response.request_focus();
            });
        },
    );

    let mut send = false;
    let _ = ctx.run_ui(
        egui::RawInput {
            focused: true,
            events,
            ..Default::default()
        },
        |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = multiline_edit(&mut input).show(ui).response;
                send = response.has_focus() && ui.input(plain_enter_pressed);
            });
        },
    );

    (input, send)
}

#[test]
fn ctrl_o_inserts_a_newline_at_the_cursor() {
    let (input, _) = run_input_frames(vec![
        key(egui::Key::ArrowLeft, egui::Modifiers::NONE),
        key(egui::Key::O, egui::Modifiers::CTRL),
    ]);
    assert_eq!(input, "a\nb");
}

#[test]
fn plain_enter_sends_without_inserting_a_newline() {
    let (input, send) = run_input_frames(vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
    assert!(send);
    assert_eq!(input, "ab");
}

#[test]
fn modified_enter_does_not_send() {
    let (input, send) = run_input_frames(vec![key(egui::Key::Enter, egui::Modifiers::SHIFT)]);
    assert!(!send);
    assert_eq!(input, "ab");
}

#[test]
fn multiline_input_frames_round_trip_as_one_physical_line() {
    let framed = super::encode_gui_input("a\nb");
    assert!(!framed.contains(['\n', '\r']));
    assert_eq!(super::decode_gui_input(&framed), "a\nb");
    // Single-line input (e.g. y/n confirmations) is sent unframed.
    assert_eq!(super::encode_gui_input("y"), "y");
    assert_eq!(super::decode_gui_input("y"), "y");
}

#[test]
fn background_only_ansi_spans_use_white_foreground() {
    let red_background = egui::Color32::from_rgb(190, 85, 85);
    let green_background = egui::Color32::from_rgb(80, 150, 95);

    for background in [red_background, green_background] {
        let style = OutputStyle {
            background: Some(background),
            ..OutputStyle::default()
        };
        assert_eq!(foreground_for_style(style), Some(egui::Color32::WHITE));
    }
}

#[test]
fn explicit_ansi_foreground_wins_over_background_fallback() {
    let foreground = egui::Color32::from_gray(130);
    let style = OutputStyle {
        foreground: Some(foreground),
        background: Some(egui::Color32::from_rgb(190, 85, 85)),
        ..OutputStyle::default()
    };
    assert_eq!(foreground_for_style(style), Some(foreground));
}
