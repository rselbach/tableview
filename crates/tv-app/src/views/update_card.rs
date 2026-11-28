use rune::{El, Tx, col, el, icon, row, spacer, text};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Spinner};
use tv_update::{UpdateFailure, UpdateStatus};

use crate::icons;
use crate::state::AppState;
use crate::theme::{self, UpdateCardPalette};

/// Builds the docked sidebar/rail update card when an update state needs user attention.
pub fn build(s: &AppState, k: &Kit) -> Option<El> {
    let c = &k.c;
    let pal = theme::update_card_colors(k.dark);

    match &s.update.snapshot.status {
        UpdateStatus::Ready {
            version,
            notes_url,
            highlights,
        } => {
            let has_notes = !highlights.is_empty() || notes_url.is_some();
            let mut head = row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(el().size(7.0, 7.0).radius(3.5).fill(pal.dot))
                .child(
                    text(format!("v{version} Ready"))
                        .font(k.ty.small_medium)
                        .color(pal.fg)
                        .grow(1.0)
                        .shrink()
                        .truncate(),
                );
            if has_notes {
                head = head.child(notes_toggle_button(s.update.notes_expanded, pal, k));
            }

            let mut card = card_shell(pal, k)
                .child(head)
                .child(text("Restart TableView to update").font(k.ty.small).color(c.text_muted).wrap());

            if s.has_uncommitted_staged_edits() {
                card = card.child(
                    text("Uncommitted staged edits stay local until committed.")
                        .font(k.ty.small)
                        .color(c.warning.fg)
                        .wrap(),
                );
            }

            if s.update.notes_expanded && has_notes {
                card = append_notes_section(card, highlights, notes_url.as_deref(), k);
            }

            card = card.child(accent_cta(
                "update-card-restart-btn",
                "Restart & Update",
                pal,
                k,
                |s, _| s.restart_to_update(),
            ));
            Some(card)
        }
        UpdateStatus::Installing { version } => Some(
            card_shell(pal, k)
                .child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(Spinner::new().size(11.0).color(pal.dot).build(k))
                        .child(
                            text(format!("Restarting to v{version}…"))
                                .font(k.ty.small_medium)
                                .color(pal.fg)
                                .truncate(),
                        ),
                )
                .child(text("Applying verified update…").font(k.ty.small).color(c.text_muted)),
        ),
        UpdateStatus::Downloading { version, percent } => {
            let fill_frac = (f32::from(*percent) / 100.0).clamp(0.02, 1.0);
            Some(
                card_shell(pal, k)
                    .child(
                        row()
                            .w_full()
                            .gap(6.0)
                            .center_y()
                            .child(Spinner::new().size(11.0).color(pal.dot).build(k))
                            .child(
                                text(format!("Downloading v{version}"))
                                    .font(k.ty.small_medium)
                                    .color(pal.fg)
                                    .grow(1.0)
                                    .shrink()
                                    .truncate(),
                            )
                            .child(text(format!("{percent}%")).font(k.ty.small).color(pal.fg)),
                    )
                    .child(
                        row()
                            .key("update-card-progress-track")
                            .w_full()
                            .h(4.0)
                            .radius(2.0)
                            .fill(c.layer_02)
                            .child(el().w_frac(fill_frac).h(4.0).radius(2.0).fill(pal.dot)),
                    ),
            )
        }
        UpdateStatus::Available {
            version,
            notes_url,
            highlights,
            ..
        } => {
            let has_notes = !highlights.is_empty() || notes_url.is_some();
            let mut head = row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(el().size(7.0, 7.0).radius(3.5).fill(pal.dot))
                .child(
                    text(format!("v{version} Available"))
                        .font(k.ty.small_medium)
                        .color(pal.fg)
                        .grow(1.0)
                        .shrink()
                        .truncate(),
                );
            if has_notes {
                head = head.child(notes_toggle_button(s.update.notes_expanded, pal, k));
            }

            let mut card = card_shell(pal, k)
                .child(head)
                .child(
                    text("This copy can't update in place.")
                        .font(k.ty.small)
                        .color(c.text_muted)
                        .wrap(),
                );

            if s.update.notes_expanded && has_notes {
                card = append_notes_section(card, highlights, notes_url.as_deref(), k);
            }

            card = card.child(accent_cta(
                "update-card-download-btn",
                "Download Update…",
                pal,
                k,
                |s, tx| s.open_update_download(tx),
            ));
            Some(card)
        }
        UpdateStatus::Failed(UpdateFailure::Relaunch { version }) => Some(
            col()
                .key("update-dock-card")
                .w_full()
                .padding(9.0)
                .gap(4.0)
                .radius(k.radius.md)
                .fill(c.warning.bg)
                .border(1.0, c.warning.border)
                .child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(icon(&icons::UPDATE_CIRCLE, 12.0).color(c.warning.fg))
                        .child(
                            text(format!("v{version} Installed"))
                                .font(k.ty.small_medium)
                                .color(c.warning.fg)
                                .truncate(),
                        ),
                )
                .child(
                    text("Quit and reopen TableView to finish updating.")
                        .font(k.ty.small)
                        .color(c.text_base)
                        .wrap(),
                ),
        ),
        UpdateStatus::Failed(UpdateFailure::RelaunchRecovery { version, .. }) => Some(
            col()
                .key("update-dock-card")
                .w_full()
                .padding(9.0)
                .gap(5.0)
                .radius(k.radius.md)
                .fill(c.warning.bg)
                .border(1.0, c.warning.border)
                .child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(icon(&icons::UPDATE_CIRCLE, 12.0).color(c.warning.fg))
                        .child(
                            text(format!("v{version} Update Incomplete"))
                                .font(k.ty.small_medium)
                                .color(c.warning.fg)
                                .truncate(),
                        ),
                )
                .child(
                    text("The last update didn't finish restarting. Check again to retry.")
                        .font(k.ty.small)
                        .color(c.text_base)
                        .wrap(),
                )
                .child(accent_cta(
                    "update-card-retry-btn",
                    "Check Again",
                    pal,
                    k,
                    |s, _| s.check_for_updates(),
                )),
        ),
        UpdateStatus::Failed(UpdateFailure::RequiresNewerOs { version, minimum_os })
            if !s.update.dismissed_os_warning =>
        {
            Some(
                col()
                    .key("update-dock-card")
                    .w_full()
                    .padding(9.0)
                    .gap(4.0)
                    .radius(k.radius.md)
                    .fill(c.warning.bg)
                    .border(1.0, c.warning.border)
                    .child(
                        row()
                            .w_full()
                            .gap(4.0)
                            .center_y()
                            .child(
                                text(format!("v{version} needs macOS {minimum_os}+"))
                                    .font(k.ty.small_medium)
                                    .color(c.warning.fg)
                                    .grow(1.0)
                                    .shrink()
                                    .truncate(),
                            )
                            .child(
                                IconButton::new(k.glyph.close)
                                    .variant(IconButtonVariant::GhostMuted)
                                    .control_size(ControlSize::Mini)
                                    .label("Dismiss update notice")
                                    .on_press(|s: &mut AppState| s.dismiss_update_os_warning())
                                    .build(k)
                                    .key("update-card-dismiss-btn"),
                            ),
                    ),
            )
        }
        _ => None,
    }
}

fn card_shell(pal: UpdateCardPalette, k: &Kit) -> El {
    col()
        .key("update-dock-card")
        .w_full()
        .padding(9.0)
        .gap(5.0)
        .radius(k.radius.md)
        .fill(pal.bg)
        .border(1.0, pal.border)
}

fn notes_toggle_button(expanded: bool, pal: UpdateCardPalette, k: &Kit) -> El {
    let label = if expanded { "Hide" } else { "Notes" };
    row()
        .key("update-card-notes-btn")
        .role("button")
        .label(label)
        .focusable(true)
        .px(4.0)
        .h(18.0)
        .center()
        .radius(k.radius.xs)
        .hover(k.c.hover)
        .press(k.c.pressed)
        .on_press(|s: &mut AppState| s.toggle_update_notes())
        .child(text(label).font(k.ty.small_medium).color(pal.fg))
}

fn append_notes_section(
    mut card: El,
    highlights: &[String],
    notes_url: Option<&str>,
    k: &Kit,
) -> El {
    let mut notes_col = col().key("update-card-notes").w_full().gap(2.0).py(2.0);
    for line in highlights.iter().take(4) {
        notes_col = notes_col.child(
            text(format!("• {line}"))
                .font(k.ty.small)
                .color(k.c.text_base)
                .w_full()
                .wrap(),
        );
    }
    if notes_url.is_some() {
        notes_col = notes_col.child(
            row()
                .w_full()
                .pt(2.0)
                .child(
                    Button::new("Release Notes")
                        .variant(ButtonVariant::GhostMuted)
                        .control_size(ControlSize::Mini)
                        .icon(&icons::ARROW_UP_RIGHT)
                        .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_update_notes(tx))
                        .build(k)
                        .key("update-card-open-notes-btn"),
                )
                .child(spacer()),
        );
    }
    card = card.child(notes_col);
    card
}

fn accent_cta(
    key: &'static str,
    label: &'static str,
    pal: UpdateCardPalette,
    k: &Kit,
    on_press: fn(&mut AppState, &mut Tx),
) -> El {
    row()
        .key(key)
        .role("button")
        .label(label)
        .focusable(true)
        .w_full()
        .h(26.0)
        .px(8.0)
        .gap(5.0)
        .center()
        .radius(k.radius.sm)
        .fill(pal.accent_bg)
        .hover(k.c.hover)
        .press(k.c.pressed)
        .on_press_tx(on_press)
        .child(icon(&icons::UPDATE_CIRCLE, 12.0).color(pal.accent_fg))
        .child(text(label).font(k.ty.small_medium).color(pal.accent_fg))
}
