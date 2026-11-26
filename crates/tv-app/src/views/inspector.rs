use chrono::Local;
use rune::{El, ScrollSpec, Tx, col, rich, row, spacer, text};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, Divider, IconButton, IconButtonVariant, Kit, TextInput};
use tv_core::{CellValue, ColumnMeta, TablePage};

use crate::icons;
use crate::state::{AppState, SessionState, TableTabState, TableViewMode};
use crate::views::table_grid::format_bytes_preview;
use crate::views::{format_bytes, pill_badge};

/// Builds the right-hand inspector showing either typed row fields or table metadata details.
pub fn build(session: &SessionState, k: &Kit) -> El {
    let c = &k.c;
    let active_table = session.active_table_tab();

    let selected_row_info = active_table.and_then(|tab| {
        if tab.view_mode != TableViewMode::Data {
            return None;
        }
        let page = tab.page.as_ref()?;
        let idx = tab.selected_row?;
        let row_cells = page.rows.get(idx)?;
        Some((tab, page, idx, row_cells))
    });

    let mut header = row()
        .key("inspector-header")
        .w_full()
        .h(40.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(text("Details").font(k.ty.ui_medium).color(c.text_base));

    if let Some((tab, _, row_idx, _)) = selected_row_info {
        header = header
            .child(spacer())
            .child(pill_badge(
                "inspector/row-badge",
                format!("Row #{}", tab.offset + row_idx as i64 + 1),
                c.layer_02,
                c.text_accent,
                c.border_base,
                k,
            ))
            .child(
                IconButton::new(k.glyph.close)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Mini)
                    .label("Clear row selection")
                    .help("Back to table details")
                    .on_press(|s: &mut AppState| s.clear_active_tab_row_selection())
                    .build(k),
            );
    }

    let search_bar = active_table.map(|tab| {
        row()
            .key("inspector-search-row")
            .w_full()
            .px(10.0)
            .py(8.0)
            .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
            .child(
                TextInput::new(&tab.inspector_search)
                    .label("Search inspector fields")
                    .placeholder("Search for field…")
                    .leading_icon(k.glyph.search)
                    .clear_button()
                    .width(None)
                    .on_edit(AppState::active_inspector_search_mut)
                    .build(k),
            )
    });

    let body = if let Some((tab, page, _row_idx, row_cells)) = selected_row_info {
        build_row_fields_body(tab, page, row_cells, k)
    } else if let Some(tab) = active_table {
        build_table_stats_body(session, tab, k)
    } else {
        col()
            .key("inspector-empty")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .padding(20.0)
            .child(text("No table or row selected").font(k.ty.ui).color(c.text_muted))
    };

    let mut root = col()
        .key("inspector-pane-inner")
        .w_full()
        .h_full()
        .min_h(0.0)
        .fill(c.bg_base)
        .border_sides([0.0, 0.0, 0.0, 1.0], c.border_muted)
        .child(header);
    if let Some(sb) = search_bar {
        root = root.child(sb);
    }
    root.child(body)
}

fn build_row_fields_body(tab: &TableTabState, page: &TablePage, cells: &[CellValue], k: &Kit) -> El {
    let c = &k.c;
    let query = tab.inspector_search.text.trim().to_ascii_lowercase();
    let mut fields_col = col().key("inspector-row-fields").w_full().padding(12.0).gap(12.0);
    let mut shown = 0usize;

    for (idx, (col_meta, val)) in page.columns.iter().zip(cells.iter()).enumerate() {
        if !query.is_empty()
            && !col_meta.name.to_ascii_lowercase().contains(&query)
            && !col_meta.udt_name.to_ascii_lowercase().contains(&query)
        {
            continue;
        }
        shown += 1;
        fields_col = fields_col.child(build_typed_field(idx, col_meta, val, tab.inspector_json_pretty, k));
    }

    if shown == 0 {
        fields_col = fields_col.child(
            text("No matching columns")
                .font(k.ty.ui_compact)
                .color(c.text_muted),
        );
    }

    k.scroll_bar(
        col()
            .key("inspector-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec { stick_bottom: false })
            .child(fields_col),
    )
    .w_full()
    .grow(1.0)
    .min_h(0.0)
}

fn build_typed_field(
    idx: usize,
    col_meta: &ColumnMeta,
    val: &CellValue,
    json_pretty: bool,
    k: &Kit,
) -> El {
    let c = &k.c;
    let col_name = col_meta.name.clone();
    let copy_val = val.copy_text();

    let mut label_row = row()
        .w_full()
        .gap(6.0)
        .center_y()
        .child(
            text(&col_meta.name)
                .font(k.ty.small_medium)
                .color(c.text_base)
                .shrink()
                .truncate(),
        );
    if col_meta.is_primary_key {
        label_row = label_row.child(pill_badge(
            format!("inspector/pk/{idx}"),
            "PK",
            c.layer_02,
            c.warning.fg,
            c.border_muted,
            k,
        ));
    }
    label_row = label_row
        .child(spacer())
        .child(
            text(&col_meta.udt_name)
                .font(k.ty.small)
                .color(c.text_faint)
                .truncate(),
        );

    if matches!(val, CellValue::Json(_)) {
        label_row = label_row.child(
            Button::new(if json_pretty { "Raw" } else { "Pretty" })
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Mini)
                .on_press(|s: &mut AppState| {
                    if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                        t.inspector_json_pretty = !t.inspector_json_pretty;
                    }
                })
                .build(k),
        );
    }

    label_row = label_row.child(
        IconButton::new(&icons::COPY)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Mini)
            .label(format!("Copy {}", col_meta.name))
            .help("Copy value")
            .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                tx.copy(copy_val.clone());
                s.set_notice(format!("Copied {col_name}"), false);
            })
            .build(k),
    );

    let value_box = col()
        .key(format!("inspector/field/{idx}"))
        .w_full()
        .px(9.0)
        .py(7.0)
        .gap(4.0)
        .radius(k.radius.sm)
        .fill(c.layer_01)
        .border(0.5, c.border_muted);

    let populated = match val {
        CellValue::Null => value_box.child(row().child(pill_badge(
            format!("inspector/null/{idx}"),
            "NULL",
            c.layer_02,
            c.text_faint,
            c.border_muted,
            k,
        ))),
        CellValue::Json(raw) => {
            let text_to_show = if json_pretty {
                val.pretty_json().unwrap_or_else(|| raw.clone())
            } else {
                raw.clone()
            };
            let syntax_theme = SyntaxTheme::from_kit(k);
            let highlights = syntax::highlight(Some("json"), &text_to_show);
            let spans = syntax_theme.spans(&highlights, 0);
            value_box.child(
                rich(&text_to_show, spans)
                    .font(k.ty.mono)
                    .color(c.text_base)
                    .w_full()
                    .wrap(),
            )
        }
        CellValue::Timestamp { raw, utc } => {
            let mut b = value_box.child(
                text(raw)
                    .font(k.ty.mono)
                    .color(c.text_base)
                    .w_full()
                    .wrap(),
            );
            if let Some(dt_utc) = utc {
                let local_dt = dt_utc.with_timezone(&Local);
                b = b
                    .child(Divider::horizontal().build(k))
                    .child(
                        text(format!("UTC   {}", dt_utc.format("%Y-%m-%d %H:%M:%S%.3f UTC")))
                            .font(k.ty.small)
                            .color(c.text_muted)
                            .w_full()
                            .wrap(),
                    )
                    .child(
                        text(format!("Local {}", local_dt.format("%Y-%m-%d %H:%M:%S%.3f %Z")))
                            .font(k.ty.small)
                            .color(c.text_muted)
                            .w_full()
                            .wrap(),
                    );
            }
            b
        }
        CellValue::Bool(b) => value_box.child(
            text(if *b { "true" } else { "false" })
                .font(k.ty.mono)
                .color(if *b { c.success.fg } else { c.text_muted }),
        ),
        CellValue::Bytes(bytes) => value_box.child(
            text(format_bytes_preview(bytes, 64))
                .font(k.ty.mono)
                .color(c.text_base)
                .w_full()
                .wrap(),
        ),
        CellValue::Text(t) if t.is_empty() => value_box.child(
            text("(empty string)")
                .font(k.ty.small)
                .color(c.text_faint),
        ),
        other => value_box.child(
            text(other.copy_text())
                .font(if other.is_mono() { k.ty.mono } else { k.ty.ui_compact })
                .color(c.text_base)
                .w_full()
                .wrap(),
        ),
    };

    col().w_full().gap(4.0).child(label_row).child(populated)
}

fn build_table_stats_body(session: &SessionState, tab: &TableTabState, k: &Kit) -> El {
    let c = &k.c;
    let query = tab.inspector_search.text.trim().to_ascii_lowercase();

    let catalog_obj = session.status.as_connected().and_then(|d| {
        d.objects
            .iter()
            .find(|o| o.schema == tab.schema && o.name == tab.object_name)
    });

    let total_size = tab
        .structure
        .as_ref()
        .and_then(|st| st.stats.total_bytes)
        .or_else(|| catalog_obj.and_then(|o| o.total_bytes))
        .map(format_bytes);
    let data_size = tab
        .structure
        .as_ref()
        .and_then(|st| st.stats.data_bytes)
        .map(format_bytes);
    let index_size = tab
        .structure
        .as_ref()
        .and_then(|st| st.stats.index_bytes)
        .map(format_bytes);
    let estimated_rows = tab
        .structure
        .as_ref()
        .and_then(|st| st.stats.estimated_rows)
        .or_else(|| catalog_obj.and_then(|o| o.estimated_rows))
        .map(|n| n.to_string());
    let comment = tab.structure.as_ref().and_then(|st| st.stats.comment.clone());

    let col_count = tab
        .structure
        .as_ref()
        .map(|st| st.columns.len())
        .or_else(|| tab.page.as_ref().map(|p| p.columns.len()))
        .map(|n| n.to_string());
    let index_count = tab
        .structure
        .as_ref()
        .map(|st| st.indexes.len().to_string());

    let entries = [
        ("total_size", total_size),
        ("data_size", data_size),
        ("index_size", index_size),
        ("estimated_rows", estimated_rows),
        ("columns", col_count),
        ("indexes", index_count),
        ("comment", comment),
    ];

    let mut details = col().key("inspector-table-stats").w_full().padding(12.0).gap(10.0);
    for (key, val) in entries {
        if !query.is_empty() && !key.contains(&query) {
            continue;
        }
        details = details.child(inspector_meta_kv(key, val.as_deref(), k));
    }

    if tab.view_mode == TableViewMode::Data && tab.page.is_some() {
        details = details.child(Divider::horizontal().build(k)).child(
            text("Click any row in the grid to inspect typed columns, JSONB payloads, and UTC/Local timestamps.")
                .font(k.ty.small)
                .color(c.text_faint)
                .w_full()
                .wrap(),
        );
    }

    k.scroll_bar(
        col()
            .key("inspector-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec { stick_bottom: false })
            .child(details),
    )
    .w_full()
    .grow(1.0)
    .min_h(0.0)
}

fn inspector_meta_kv(key: &str, value: Option<&str>, k: &Kit) -> El {
    let c = &k.c;
    let value_box = row()
        .w_full()
        .min_h(28.0)
        .px(8.0)
        .py(5.0)
        .center_y()
        .radius(k.radius.sm)
        .fill(c.layer_01)
        .border(0.5, c.border_muted);

    let box_content = match value {
        Some(v) => value_box.child(text(v).font(k.ty.ui_compact).color(c.text_base).w_full().wrap()),
        None => value_box.child(pill_badge(
            format!("inspector/meta-null/{key}"),
            "NULL",
            c.layer_02,
            c.text_faint,
            c.border_muted,
            k,
        )),
    };

    col()
        .w_full()
        .gap(4.0)
        .child(text(key).font(k.ty.small).color(c.text_muted))
        .child(box_content)
}
