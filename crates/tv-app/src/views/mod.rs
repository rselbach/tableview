pub mod connection_form;
pub mod explain_view;
pub mod export_modal;
pub mod inspector;
pub mod launcher;
pub mod pickers;
pub mod query_editor;
pub mod safety_modal;
pub mod settings_modal;
pub mod staged_diff;
pub mod table_grid;
pub mod update_card;
pub mod workspace;

use rune::{Cx, El, ColorScheme, col, el, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Toast, ToastRegion};

use crate::platform;
use crate::state::{AppState, OverlayState, Screen};
use crate::theme;

/// Root Rune view rendering either the Connection Launcher or active Workspace plus modal layers.
pub fn view(s: &AppState, cx: &mut Cx<AppState>) -> El {
    let dark = match s.appearance {
        ColorScheme::Dark => true,
        ColorScheme::Light => false,
        ColorScheme::System => cx.system_dark(),
    };
    let k = theme::kit(dark);
    let mut layer: Vec<El> = Vec::new();

    let screen_el = match s.screen {
        Screen::Launcher => launcher::build(s, &k, cx, &mut layer),
        Screen::Workspace(session_id) => {
            if let Some(session) = s.sessions.get(&session_id) {
                workspace::build(s, session, &k, cx, &mut layer)
            } else {
                launcher::build(s, &k, cx, &mut layer)
            }
        }
    };

    let base = col()
        .key("tv-app-base")
        .w_full()
        .h_full()
        .min_h(0.0)
        .on_key_preview(platform::handle_global_key)
        .child(screen_el);

    match &s.overlay {
        OverlayState::None => {}
        OverlayState::ConnectionForm(form) => {
            connection_form::build_form(s, form, &k, &mut layer);
        }
        OverlayState::ImportUrl(import) => {
            connection_form::build_import_url(import, &k, &mut layer);
        }
        OverlayState::Picker(picker) => {
            pickers::build(s, picker, &k, &mut layer);
        }
        OverlayState::PasswordPrompt(prompt) => {
            connection_form::build_password_prompt(prompt, &k, &mut layer);
        }
        OverlayState::SaveQuery(modal) => {
            query_editor::build_save_query_modal(modal, &k, &mut layer);
        }
        OverlayState::SafetyConfirm(modal) => {
            safety_modal::build_safety_modal(modal, &k, &mut layer);
        }
        OverlayState::Export(modal) => {
            export_modal::build_export_modal(modal, &k, &mut layer);
        }
        OverlayState::DeleteConfirm(id) => {
            connection_form::build_delete_confirm(s, *id, &k, &mut layer);
        }
        OverlayState::Settings => {
            settings_modal::build_settings_modal(s, &k, &mut layer);
        }
    }

    if let Some(notice) = &s.status_notice {
        let dot_color = if notice.is_error { k.c.danger.fg } else { k.c.success.fg };
        let toast = Toast::new("tv-notice", &notice.message)
            .lead(el().size(8.0, 8.0).radius(4.0).fill(dot_color))
            .on_close(AppState::dismiss_status_notice)
            .build(&k)
            .on_hover_change(AppState::hover_status_notice)
            .on_background_press_tx(AppState::pin_status_notice);
        layer.push(ToastRegion::new().toast(toast).build(&k));
    }

    rune_kit::root(base, layer)
}

/// Formats byte counts cleanly for database and table size readouts.
pub fn format_bytes(bytes: i64) -> String {
    if bytes < 0 {
        return "0 B".to_owned();
    }
    let b = bytes as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Renders a compact pill badge using custom background, ink, and border colors.
pub fn pill_badge(
    key: impl Into<String>,
    label: impl Into<String>,
    bg: rune::Color,
    fg: rune::Color,
    border: rune::Color,
    k: &Kit,
) -> El {
    let label = label.into();
    row()
        .key(key)
        .h(18.0)
        .px(6.0)
        .center()
        .radius(k.radius.xs)
        .fill(bg)
        .border(0.5, border)
        .child(text(label).font(k.ty.small_medium).color(fg))
}

/// Renders a compact action link button for empty states.
pub fn empty_cta(label: &str, k: &Kit, primary: bool, on_press: fn(&mut AppState, &mut rune::Tx)) -> El {
    Button::new(label)
        .variant(if primary {
            ButtonVariant::Contrast
        } else {
            ButtonVariant::Outline
        })
        .control_size(ControlSize::Regular)
        .on_press_tx(on_press)
        .build(k)
}
