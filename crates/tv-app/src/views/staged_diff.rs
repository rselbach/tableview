use rune::{El, Tx, col, row, spacer, text};
use rune_kit::diff::{DiffMode, DiffView};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl, Spinner};
use tv_core::SafetyPolicy;

use crate::icons;
use crate::state::{AppState, SessionState, TableTabState};
use crate::views::pill_badge;

/// Builds the docked bottom drawer displaying row diffs, transactional SQL, and commit controls.
pub fn build_drawer(s: &AppState, session: &SessionState, tab: &TableTabState, k: &Kit) -> El {
    let c = &k.c;
    let is_split = tab.diff_state.mode() == DiffMode::Split;
    let tid = tab.tab_id.get();

    let mut header = row()
        .key("staged-diff-header")
        .w_full()
        .h(38.0)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([1.0, 0.0, 1.0, 0.0], c.border_base)
        .child(icon_el(&icons::DIFF_CHANGES, c.text_accent))
        .child(text("Staged Diff").font(k.ty.ui_medium).color(c.text_base));

    if tab.staged.insert_count() > 0 {
        header = header.child(pill_badge(
            "staged-diff/ins",
            format!("+{}", tab.staged.insert_count()),
            c.success.bg,
            c.success.fg,
            c.success.border,
            k,
        ));
    }
    if tab.staged.update_count() > 0 {
        header = header.child(pill_badge(
            "staged-diff/upd",
            format!("~{}", tab.staged.update_count()),
            c.warning.bg,
            c.warning.fg,
            c.warning.border,
            k,
        ));
    }
    if tab.staged.delete_count() > 0 {
        header = header.child(pill_badge(
            "staged-diff/del",
            format!("-{}", tab.staged.delete_count()),
            c.danger.bg,
            c.danger.fg,
            c.danger.border,
            k,
        ));
    }

    let mode_switch = SegmentedControl::new(&["Unified", "Split"], Some(usize::from(is_split)))
        .label("Diff layout mode")
        .as_tabs()
        .fit()
        .on_pick(|s: &mut AppState, idx, _| {
            s.set_staged_diff_mode(if idx == 1 { DiffMode::Split } else { DiffMode::Unified });
        })
        .build(k)
        .key("staged-diff-mode-switch");

    let sql_for_copy = tab
        .columns()
        .and_then(|cols| {
            tab.staged
                .build_transaction_sql_for_engine(session.profile.engine, &tab.schema, &tab.object_name, cols)
                .ok()
        })
        .unwrap_or_default();

    let can_commit = tab.staged.has_changes()
        && !tab.committing
        && tab.can_edit_table(session.profile.safety).is_ok()
        && session.profile.safety != SafetyPolicy::ReadOnly;

    header = header
        .child(spacer())
        .child(mode_switch)
        .child(
            Button::new("Copy SQL")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .icon(&icons::COPY)
                .disabled(sql_for_copy.is_empty())
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                    tx.copy(sql_for_copy.clone());
                    s.set_notice("Copied transaction SQL to clipboard", false);
                })
                .build(k)
                .key("staged-copy-sql-btn"),
        )
        .child(
            Button::new("Discard")
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .icon(&icons::UNDO)
                .disabled(!tab.staged.has_changes() || tab.committing)
                .on_press(|s: &mut AppState| s.discard_staged_changes())
                .build(k)
                .key("staged-discard-btn"),
        );

    if tab.committing {
        header = header.child(Spinner::new().size(13.0).color(c.text_accent).build(k));
    }

    header = header
        .child(
            Button::new(if tab.committing { "Committing…" } else { "Commit" })
                .variant(ButtonVariant::Contrast)
                .control_size(ControlSize::Small)
                .icon(&icons::CHECK)
                .disabled(!can_commit)
                .on_press(|s: &mut AppState| s.commit_staged_changes())
                .build(k)
                .key("staged-commit-btn"),
        )
        .child(
            IconButton::new(k.glyph.close)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Mini)
                .label("Close staged diff drawer")
                .on_press(|s: &mut AppState| s.close_staged_diff_drawer())
                .build(k)
                .key("staged-close-btn"),
        );

    let mut drawer = col()
        .key("staged-diff-drawer")
        .w_full()
        .h(340.0)
        .min_h(0.0)
        .fill(c.bg_base)
        .child(header);

    if let Some(err) = &tab.commit_error {
        drawer = drawer.child(
            row()
                .key("staged-commit-error")
                .w_full()
                .px(12.0)
                .py(7.0)
                .gap(8.0)
                .center_y()
                .fill(c.danger.bg)
                .border_sides([0.0, 0.0, 1.0, 0.0], c.danger.border)
                .child(text(err).font(k.ty.small).color(c.danger.fg).grow(1.0).wrap()),
        );
    }

    if tab.staged.is_empty() {
        drawer.child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .center()
                .child(text("No staged changes on this table.").font(k.ty.ui).color(c.text_muted)),
        )
    } else {
        let diff = DiffView::new(AppState::active_staged_diff, AppState::active_staged_diff_mut)
            .key(format!("staged-diff-view-{tid}"))
            .scroll_bar(true)
            .build(s, k)
            .w_full()
            .grow(1.0)
            .min_h(0.0);
        drawer.child(diff)
    }
}

fn icon_el(ic: &'static rune::Icon, color: rune::Color) -> El {
    rune::icon(ic, 14.0).color(color)
}
