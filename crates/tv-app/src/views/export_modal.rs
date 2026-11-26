use rune::{El, ScrollSpec, Tx, col, rich, row, spacer, text};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Field, Kit, SegmentedControl, TextInput};
use tv_core::{ExportFormat, export_rows};

use crate::icons;
use crate::state::{AppState, ExportModalState, OverlayState};

/// Builds the modal dialog for previewing and exporting tabular rows to CSV, JSON, or SQL INSERT.
pub fn build_export_modal(modal: &ExportModalState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;

    let format_switch = SegmentedControl::new(&["CSV", "JSON", "SQL INSERT"], Some(modal.format.index()))
        .label("Export format")
        .as_tabs()
        .fit()
        .on_pick(|s: &mut AppState, idx, _| {
            s.set_export_format(ExportFormat::from_index(idx));
        })
        .build(k)
        .key("export-format-switch");

    let preview_count = modal.rows.len().min(8);
    let preview_text = export_rows(
        modal.format,
        modal.schema.as_deref(),
        &modal.table_name.text,
        &modal.columns,
        &modal.rows[..preview_count],
    );

    let lang = match modal.format {
        ExportFormat::Json => Some("json"),
        ExportFormat::SqlInsert => Some("sql"),
        ExportFormat::Csv => None,
    };

    let preview_child = if let Some(lang) = lang {
        let highlights = syntax::highlight(Some(lang), &preview_text);
        let spans = SyntaxTheme::from_kit(k).spans(&highlights, 0);
        rich(&preview_text, spans)
            .font(k.ty.mono)
            .color(c.text_base)
            .w_full()
            .wrap()
    } else {
        text(&preview_text)
            .font(k.ty.mono)
            .color(c.text_base)
            .w_full()
            .wrap()
    };

    let preview_box = k
        .scroll_bar(
            col()
                .key("export-preview-scroll")
                .w_full()
                .h(184.0)
                .min_h(0.0)
                .padding(10.0)
                .scroll(ScrollSpec { stick_bottom: false })
                .child(preview_child),
        )
        .w_full()
        .h(184.0)
        .min_h(0.0)
        .radius(k.radius.sm)
        .fill(c.layer_01)
        .border(0.5, c.border_muted);

    let mut body = col()
        .w_full()
        .px(20.0)
        .pb(18.0)
        .gap(12.0)
        .child(
            row()
                .w_full()
                .center_y()
                .child(
                    text(format!(
                        "{} rows · {} columns from {}",
                        modal.rows.len(),
                        modal.columns.len(),
                        modal.source_label
                    ))
                    .font(k.ty.ui_compact)
                    .color(c.text_muted),
                )
                .child(spacer())
                .child(format_switch),
        );

    if modal.format == ExportFormat::SqlInsert {
        let target_input = TextInput::new(&modal.table_name)
            .label("Target table name")
            .placeholder("students")
            .width(None)
            .on_edit(AppState::export_table_name_mut)
            .build(k)
            .key("export-table-name");
        body = body.child(Field::new().label("Target Table").control(target_input).build(k));
    }

    body = body
        .child(
            col()
                .w_full()
                .gap(4.0)
                .child(
                    text(format!("Preview (first {preview_count} rows)"))
                        .font(k.ty.small_medium)
                        .color(c.text_muted),
                )
                .child(preview_box),
        )
        .child(
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
                        .key("export-cancel-btn"),
                )
                .child(
                    Button::new("Copy to Clipboard")
                        .variant(ButtonVariant::Outline)
                        .control_size(ControlSize::Regular)
                        .icon(&icons::COPY)
                        .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.copy_export_to_clipboard(tx))
                        .build(k)
                        .key("export-copy-btn"),
                )
                .child(
                    Button::new("Save to File…")
                        .variant(ButtonVariant::Contrast)
                        .control_size(ControlSize::Regular)
                        .icon(&icons::DOWNLOAD)
                        .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.save_export_to_file(tx))
                        .build(k)
                        .key("export-save-btn"),
                ),
        );

    Dialog::new("export-modal", "Export Data")
        .open(true)
        .width(580.0)
        .fit()
        .on_close(|s: &mut AppState| s.overlay = OverlayState::None)
        .child(body)
        .build(k, layer);
}
