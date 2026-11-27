use rune::{ColorScheme, El, Tx, col, row, spacer, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Divider, Kit, SegmentedControl, Switch};
use tv_update::UpdateStatus;

use crate::platform::{self, Action};
use crate::state::{AppState, OverlayState};

/// Builds the application Settings modal dialog (`Appearance` and `Updates`).
pub fn build_settings_modal(s: &AppState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let scheme_idx = match s.appearance {
        ColorScheme::System => 0,
        ColorScheme::Light => 1,
        ColorScheme::Dark => 2,
    };

    let appearance_picker = SegmentedControl::new(&["System", "Light", "Dark"], Some(scheme_idx))
        .label("Theme")
        .fit()
        .on_pick(|s: &mut AppState, idx, tx| {
            let next = match idx {
                1 => ColorScheme::Light,
                2 => ColorScheme::Dark,
                _ => ColorScheme::System,
            };
            platform::dispatch_action(s, Action::SetColorScheme(next), tx);
        })
        .build(k)
        .key("settings-appearance");

    let appearance_card = col()
        .key("settings-appearance-card")
        .w_full()
        .padding(12.0)
        .gap(10.0)
        .radius(k.radius.md)
        .fill(c.layer_02)
        .border(0.5, c.border_muted)
        .child(
            row()
                .w_full()
                .gap(12.0)
                .center_y()
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(2.0)
                        .child(text("Theme").font(k.ty.ui_medium).color(c.text_base))
                        .child(
                            text("Match your system appearance or pick a fixed theme.")
                                .font(k.ty.caption())
                                .color(c.text_muted)
                                .wrap()
                                .w_full(),
                        ),
                )
                .child(appearance_picker),
        );

    let updates_disabled = matches!(s.update.snapshot.status, UpdateStatus::Disabled);
    let auto_switch = Switch::new(s.update.snapshot.automatic)
        .label("Automatic updates")
        .hide_label()
        .disabled(updates_disabled)
        .on_toggle(|s: &mut AppState| s.set_automatic_update_checks(!s.update.snapshot.automatic))
        .build(k)
        .key("settings-auto-updates-switch");

    let (update_summary, action_label, action_variant, action_disabled) = match &s.update.snapshot.status {
        UpdateStatus::Disabled => (
            "Available in packaged macOS builds.".to_owned(),
            "Check Now",
            ButtonVariant::Outline,
            false,
        ),
        UpdateStatus::Idle => (
            if s.update.snapshot.automatic {
                "Automatic background checks enabled.".to_owned()
            } else {
                "Automatic background checks paused.".to_owned()
            },
            "Check Now",
            ButtonVariant::Outline,
            false,
        ),
        UpdateStatus::Checking => (
            "Checking for updates…".to_owned(),
            "Checking…",
            ButtonVariant::Outline,
            true,
        ),
        UpdateStatus::UpToDate => (
            "You're on the latest version.".to_owned(),
            "Check Now",
            ButtonVariant::Outline,
            false,
        ),
        UpdateStatus::Downloading { version, percent } => (
            format!("Downloading v{version} ({percent}%)…"),
            "Downloading…",
            ButtonVariant::Outline,
            true,
        ),
        UpdateStatus::Ready { version, .. } => (
            format!("TableView {version} is ready to install."),
            "Restart & Update",
            ButtonVariant::Contrast,
            false,
        ),
        UpdateStatus::Installing { version } => (
            format!("Installing v{version}…"),
            "Updating…",
            ButtonVariant::Outline,
            true,
        ),
        UpdateStatus::Available { version, .. } => (
            format!("TableView {version} is available to download."),
            "Download…",
            ButtonVariant::Contrast,
            false,
        ),
        UpdateStatus::Failed(failure) => (
            UpdateStatus::failure_message(failure),
            "Check Now",
            ButtonVariant::Outline,
            false,
        ),
    };

    let updates_card = col()
        .key("settings-updates-card")
        .w_full()
        .padding(12.0)
        .gap(10.0)
        .radius(k.radius.md)
        .fill(c.layer_02)
        .border(0.5, c.border_muted)
        .child(
            row()
                .w_full()
                .gap(12.0)
                .center_y()
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(2.0)
                        .child(text("Automatic updates").font(k.ty.ui_medium).color(c.text_base))
                        .child(
                            text("Automatically check for and stage new releases in the background.")
                                .font(k.ty.caption())
                                .color(c.text_muted)
                                .wrap()
                                .w_full(),
                        ),
                )
                .child(auto_switch),
        )
        .child(Divider::horizontal().build(k))
        .child(
            row()
                .w_full()
                .gap(12.0)
                .center_y()
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            text(format!("TableView v{}", s.update.running_version))
                                .font(k.ty.ui_medium)
                                .color(c.text_base),
                        )
                        .child(
                            text(update_summary)
                                .font(k.ty.caption())
                                .color(c.text_muted)
                                .wrap()
                                .w_full(),
                        ),
                )
                .child(
                    Button::new(action_label)
                        .variant(action_variant)
                        .control_size(ControlSize::Regular)
                        .disabled(action_disabled)
                        .on_press_tx(perform_update_action)
                        .build(k)
                        .key("settings-update-action-btn"),
                ),
        );

    let body = col()
        .key("settings-body")
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(14.0)
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(
                    text("APPEARANCE")
                        .font(k.ty.small_medium)
                        .color(c.text_faint)
                        .px(2.0),
                )
                .child(appearance_card),
        )
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(
                    text("UPDATES")
                        .font(k.ty.small_medium)
                        .color(c.text_faint)
                        .px(2.0),
                )
                .child(updates_card),
        )
        .child(
            row()
                .w_full()
                .pt(2.0)
                .child(spacer())
                .child(
                    Button::new("Done")
                        .variant(ButtonVariant::Contrast)
                        .control_size(ControlSize::Regular)
                        .on_press(|s: &mut AppState| s.overlay = OverlayState::None)
                        .build(k)
                        .key("settings-done-btn"),
                ),
        );

    Dialog::new("settings-modal", "Settings")
        .open(true)
        .width(480.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}

fn perform_update_action(s: &mut AppState, tx: &mut Tx) {
    match &s.update.snapshot.status {
        UpdateStatus::Ready { .. } => s.restart_to_update(),
        UpdateStatus::Available { .. } => s.open_update_download(tx),
        UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Installing { .. } => {}
        UpdateStatus::Disabled | UpdateStatus::Idle | UpdateStatus::UpToDate | UpdateStatus::Failed(_) => {
            platform::dispatch_action(s, Action::CheckForUpdates, tx);
        }
    }
}
