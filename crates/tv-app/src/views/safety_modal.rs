use rune::{El, ScrollSpec, col, rich, row, spacer, text};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, Checkbox, ControlSize, Dialog, Kit};
use tv_core::{EnvTag, StatementRisk};

use crate::icons;
use crate::state::{AppState, OverlayState, SafetyConfirmModalState};
use crate::views::pill_badge;

/// Builds the confirmation modal surfaced before running mutating or destructive SQL on protected connections.
pub fn build_safety_modal(modal: &SafetyConfirmModalState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let is_prod = modal.env == EnvTag::Production;
    let needs_prod_ack = modal.is_destructive && is_prod;
    let can_execute = !needs_prod_ack || modal.destructive_ack;

    let banner_bg = if modal.is_destructive { c.danger.bg } else { c.warning.bg };
    let banner_fg = if modal.is_destructive { c.danger.fg } else { c.warning.fg };
    let banner_border = if modal.is_destructive {
        c.danger.border
    } else {
        c.warning.border
    };

    let banner = col()
        .w_full()
        .padding(12.0)
        .gap(6.0)
        .radius(k.radius.sm)
        .fill(banner_bg)
        .border(0.5, banner_border)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(rune::icon(&icons::SHIELD, 14.0).color(banner_fg))
                .child(pill_badge(
                    "safety/env-pill",
                    modal.env.banner_tag(),
                    c.layer_02,
                    banner_fg,
                    banner_border,
                    k,
                ))
                .child(pill_badge(
                    "safety/policy-pill",
                    modal.policy.label(),
                    c.layer_02,
                    c.text_base,
                    c.border_muted,
                    k,
                )),
        )
        .child(text(&modal.reason).font(k.ty.ui_compact).color(banner_fg).w_full().wrap());

    let syntax_theme = SyntaxTheme::from_kit(k);
    let mut stmt_list = col().w_full().gap(8.0);
    for (idx, stmt) in modal.statements.iter().enumerate() {
        let highlights = syntax::highlight(Some("sql"), &stmt.sql);
        let spans = syntax_theme.spans(&highlights, 0);
        let (risk_bg, risk_fg, risk_border) = match stmt.risk {
            StatementRisk::Destructive => (c.danger.bg, c.danger.fg, c.danger.border),
            StatementRisk::WriteDdl | StatementRisk::WriteDml => (c.warning.bg, c.warning.fg, c.warning.border),
            StatementRisk::ReadOnly => (c.layer_02, c.text_muted, c.border_muted),
        };
        stmt_list = stmt_list.child(
            col()
                .key(format!("safety/stmt/{idx}"))
                .w_full()
                .padding(10.0)
                .gap(6.0)
                .radius(k.radius.sm)
                .fill(c.layer_01)
                .border(0.5, c.border_muted)
                .child(
                    row()
                        .w_full()
                        .gap(6.0)
                        .center_y()
                        .child(pill_badge(
                            format!("safety/risk/{idx}"),
                            stmt.risk.badge_label(),
                            risk_bg,
                            risk_fg,
                            risk_border,
                            k,
                        ))
                        .child(
                            text(&stmt.summary)
                                .font(k.ty.small)
                                .color(c.text_muted)
                                .shrink()
                                .truncate(),
                        ),
                )
                .child(
                    rich(&stmt.sql, spans)
                        .font(k.ty.mono)
                        .color(c.text_base)
                        .w_full()
                        .wrap(),
                ),
        );
    }

    let stmts_scroll = k
        .scroll_bar(
            col()
                .key("safety-stmts-scroll")
                .w_full()
                .max_h(220.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(stmt_list),
        )
        .w_full()
        .max_h(220.0)
        .min_h(0.0);

    let mut body = col()
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(12.0)
        .child(banner)
        .child(stmts_scroll);

    if needs_prod_ack {
        body = body.child(
            Checkbox::new(modal.destructive_ack)
                .label("I understand this destructive operation targets Production")
                .on_toggle(|s: &mut AppState| {
                    if let OverlayState::SafetyConfirm(ref mut m) = s.overlay {
                        m.destructive_ack = !m.destructive_ack;
                    }
                })
                .build(k)
                .key("safety-prod-ack"),
        );
    }

    body = body.child(
        row()
            .w_full()
            .gap(8.0)
            .pt(4.0)
            .child(spacer())
            .child(
                Button::new("Cancel")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Regular)
                    .on_press(|s: &mut AppState| s.overlay = OverlayState::None)
                    .build(k)
                    .key("safety-cancel-btn"),
            )
            .child(
                Button::new("Execute SQL")
                    .variant(if modal.is_destructive {
                        ButtonVariant::Danger
                    } else {
                        ButtonVariant::Contrast
                    })
                    .control_size(ControlSize::Regular)
                    .icon(&icons::PLAY)
                    .disabled(!can_execute)
                    .on_press(|s: &mut AppState| s.confirm_safety_modal())
                    .build(k)
                    .key("safety-confirm-execute-btn"),
            ),
    );

    let title = if modal.is_destructive {
        "Confirm Destructive SQL"
    } else {
        "Confirm Write Query"
    };

    Dialog::new("safety-confirm-modal", title)
        .open(true)
        .width(540.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}
