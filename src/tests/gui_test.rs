use eframe::egui;

use super::{OutputStyle, foreground_for_style, is_todo_mode};

#[test]
fn workspace_is_enabled_only_for_todo_modes() {
    assert!(!is_todo_mode(0));
    assert!(is_todo_mode(1));
    assert!(is_todo_mode(2));
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
