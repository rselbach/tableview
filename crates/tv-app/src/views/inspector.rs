use chrono::Local;
use rune::{El, ScrollSpec, TextEdit, Tx, col, rich, row, spacer, text};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, ControlSize, Divider, IconButton, IconButtonVariant, Kit, TextInput};
use tv_core::{CellValue, ColumnMeta, ForeignKeyRef, RawCellValue, resolve_fk_lookup_components_with_raw};

use crate::icons;
use crate::state::{AppState, QueryTabState, RowKey, SessionState, TableTabState, TableViewMode};
use crate::views::table_grid::format_bytes_preview;
use crate::views::{format_bytes, pill_badge};

struct SelectedRowView {
    row_number: usize,
    row_badge: String,
    row_key: Option<RowKey>,
    editable: bool,
    is_draft: bool,
    is_deleted: bool,
    edited_cols: Vec<bool>,
    columns: Vec<ColumnMeta>,
    foreign_keys: Vec<ForeignKeyRef>,
    cells: Vec<CellValue>,
    raw_cells: Vec<Option<RawCellValue>>,
    search: TextEdit,
    json_pretty: bool,
    clear_help: &'static str,
}

/// Builds the right-hand inspector showing either typed row fields or table/query metadata details.
pub fn build(session: &SessionState, k: &Kit) -> El {
    let c = &k.c;
    let active_table = session.active_table_tab();
    let active_query = session.active_query_tab();

    let selected_row_info = if let Some(tab) = active_table {
        if tab.view_mode == TableViewMode::Data {
            tab.active_row_key().and_then(|rk| {
                let cols = tab.columns()?.to_vec();
                let foreign_keys = tab.all_foreign_keys();
                match rk {
                    RowKey::Draft(id) => {
                        tab.staged.insert_row(id)?;
                        let visual_idx = tab.row_key_to_visual_idx(rk).unwrap_or(0);
                        let cells: Vec<CellValue> = (0..cols.len())
                            .map(|c| tab.effective_cell(rk, c))
                            .collect();
                        let editable = tab.can_edit_table(session.profile.safety).is_ok();
                        let col_len = cols.len();
                        Some(SelectedRowView {
                            row_number: visual_idx + 1,
                            row_badge: format!("New Row #{}", id.get()),
                            row_key: Some(rk),
                            editable,
                            is_draft: true,
                            is_deleted: false,
                            edited_cols: vec![true; col_len],
                            columns: cols,
                            foreign_keys,
                            cells,
                            raw_cells: vec![None; col_len],
                            search: tab.inspector_search.clone(),
                            json_pretty: tab.inspector_json_pretty,
                            clear_help: "Back to table details",
                        })
                    }
                    RowKey::Existing(idx) => {
                        let page = tab.page.as_ref()?;
                        page.rows.get(idx)?;
                        let cells: Vec<CellValue> = (0..cols.len())
                            .map(|c| tab.effective_cell(rk, c))
                            .collect();
                        let raw_cells: Vec<Option<RawCellValue>> = (0..cols.len())
                            .map(|c| tab.effective_raw_cell(rk, c))
                            .collect();
                        let edited_cols: Vec<bool> = (0..cols.len())
                            .map(|c| tab.staged.effective_existing_cell(idx, c).is_some())
                            .collect();
                        let editable =
                            tab.can_edit_existing_row(session.profile.safety, idx).is_ok();
                        let row_num = (tab.offset + idx as i64 + 1).max(1) as usize;
                        Some(SelectedRowView {
                            row_number: row_num,
                            row_badge: format!("Row #{row_num}"),
                            row_key: Some(rk),
                            editable,
                            is_draft: false,
                            is_deleted: tab.staged.is_row_deleted(idx),
                            edited_cols,
                            columns: cols,
                            foreign_keys,
                            cells,
                            raw_cells,
                            search: tab.inspector_search.clone(),
                            json_pretty: tab.inspector_json_pretty,
                            clear_help: "Back to table details",
                        })
                    }
                }
            })
        } else {
            None
        }
    } else if let Some(q) = active_query {
        q.active_result_state().and_then(|res| {
            let idx = res.selected_row?;
            let cells = res.statement.rows.get(idx)?;
            let col_len = res.statement.columns.len();
            Some(SelectedRowView {
                row_number: idx + 1,
                row_badge: format!("Row #{}", idx + 1),
                row_key: None,
                editable: false,
                is_draft: false,
                is_deleted: false,
                edited_cols: vec![false; col_len],
                columns: res.statement.columns.clone(),
                foreign_keys: Vec::new(),
                cells: cells.clone(),
                raw_cells: vec![None; col_len],
                search: q.inspector_search.clone(),
                json_pretty: q.inspector_json_pretty,
                clear_help: "Back to query summary",
            })
        })
    } else {
        None
    };

    let mut header = row()
        .key("inspector-header")
        .w_full()
        .h(40.0)
        .px(12.0)
        .gap(6.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(text("Details").font(k.ty.ui_medium).color(c.text_base));

    if let Some(ref sel) = selected_row_info {
        let _ = sel.row_number;
        header = header.child(spacer());
        if sel.is_deleted {
            header = header.child(pill_badge(
                "inspector/row-status",
                "Deleted",
                c.danger.bg,
                c.danger.fg,
                c.danger.border,
                k,
            ));
        } else if sel.is_draft {
            header = header.child(pill_badge(
                "inspector/row-status",
                "Staged",
                c.success.bg,
                c.success.fg,
                c.success.border,
                k,
            ));
        }
        header = header
            .child(pill_badge(
                "inspector/row-badge",
                &sel.row_badge,
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
                    .help(sel.clear_help)
                    .on_press(|s: &mut AppState| s.clear_active_tab_row_selection())
                    .build(k),
            );
    }

    let search_edit = active_table
        .map(|t| &t.inspector_search)
        .or_else(|| active_query.map(|q| &q.inspector_search));

    let search_bar = search_edit.map(|edit| {
        row()
            .key("inspector-search-row")
            .w_full()
            .px(10.0)
            .py(8.0)
            .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
            .child(
                TextInput::new(edit)
                    .label("Search inspector fields")
                    .placeholder("Search for field…")
                    .leading_icon(k.glyph.search)
                    .clear_button()
                    .width(None)
                    .on_edit(AppState::active_inspector_search_mut)
                    .build(k),
            )
    });

    let body = if let Some(sel) = selected_row_info {
        build_row_fields_body(&sel, k)
    } else if let Some(tab) = active_table {
        build_table_stats_body(session, tab, k)
    } else if let Some(q) = active_query {
        build_query_overview_body(q, k)
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

fn build_row_fields_body(sel: &SelectedRowView, k: &Kit) -> El {
    let c = &k.c;
    let query = sel.search.text.trim().to_ascii_lowercase();
    let mut fields_col = col().key("inspector-row-fields").w_full().padding(12.0).gap(12.0);
    let mut shown = 0usize;

    for (idx, (col_meta, val)) in sel.columns.iter().zip(sel.cells.iter()).enumerate() {
        if !query.is_empty()
            && !col_meta.name.to_ascii_lowercase().contains(&query)
            && !col_meta.udt_name.to_ascii_lowercase().contains(&query)
        {
            continue;
        }
        shown += 1;
        let is_edited = sel.edited_cols.get(idx).copied().unwrap_or(false);
        fields_col = fields_col.child(build_typed_field(idx, col_meta, val, sel, is_edited, k));
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
    sel: &SelectedRowView,
    is_edited: bool,
    k: &Kit,
) -> El {
    let c = &k.c;
    let col_name = col_meta.name.clone();
    let copy_val = val.copy_text();

    let mut label_row = row()
        .w_full()
        .gap(5.0)
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
    let resolved_fks: Vec<(&ForeignKeyRef, Vec<tv_core::FkLookupComponent>)> =
        tv_core::engine::foreign_keys_for_column(&sel.foreign_keys, &sel.columns, &col_meta.name)
            .into_iter()
            .filter_map(|fk| {
                resolve_fk_lookup_components_with_raw(
                    fk,
                    &sel.columns,
                    &sel.cells,
                    Some(&sel.raw_cells),
                )
                .map(|comps| (fk, comps))
            })
            .collect();
    let fk_refs: Vec<&ForeignKeyRef> = resolved_fks.iter().map(|(fk, _)| *fk).collect();
    for (fk_idx, (fk, comps_jump)) in resolved_fks.iter().enumerate() {
        let fk_jump = (*fk).clone();
        let comps_jump = comps_jump.clone();
        let btn_label = crate::views::table_grid::format_fk_choice_label(fk, &fk_refs, true);
        let btn_key = if fk_idx == 0 {
            format!("inspector/fk/{idx}")
        } else {
            format!("inspector/fk/{idx}/{fk_idx}")
        };
        label_row = label_row.child(
            Button::new(btn_label)
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Mini)
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                    s.jump_to_foreign_key(fk_jump.clone(), comps_jump.clone(), tx);
                })
                .build(k)
                .key(btn_key),
        );
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
            Button::new(if sel.json_pretty { "Raw" } else { "Pretty" })
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Mini)
                .on_press(|s: &mut AppState| s.toggle_active_inspector_json_pretty())
                .build(k),
        );
    }

    if sel.editable && !sel.is_deleted && let Some(row_key) = sel.row_key {
        label_row = label_row.child(
            IconButton::new(&icons::EDIT_PENCIL)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Mini)
                .label(format!("Edit {}", col_meta.name))
                .help("Edit value in grid")
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                    s.begin_cell_edit(row_key, idx, tx);
                })
                .build(k)
                .key(format!("inspector/edit/{idx}")),
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

    let box_fill = if sel.is_deleted {
        c.danger.bg
    } else if sel.is_draft {
        c.success.bg
    } else if is_edited {
        c.warning.bg
    } else {
        c.layer_01
    };
    let box_border = if sel.is_deleted {
        c.danger.border
    } else if sel.is_draft {
        c.success.border
    } else if is_edited {
        c.warning.border
    } else {
        c.border_muted
    };

    let value_box = col()
        .key(format!("inspector/field/{idx}"))
        .w_full()
        .px(9.0)
        .py(7.0)
        .gap(4.0)
        .radius(k.radius.sm)
        .fill(box_fill)
        .border(0.5, box_border);

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
            let text_to_show = if sel.json_pretty {
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

fn build_query_overview_body(tab: &QueryTabState, k: &Kit) -> El {
    let c = &k.c;
    let filter = tab.inspector_search.text.trim().to_ascii_lowercase();
    let (line, col_num) = tab.cursor_line_col();
    let status = if tab.running {
        if tab.cancelling { "Cancelling" } else { "Running" }
    } else if tab.batch_cancelled {
        "Cancelled"
    } else if tab.saved_query_id.is_some() {
        if tab.is_dirty() { "Saved (modified)" } else { "Saved" }
    } else {
        "Scratchpad"
    };
    let last_duration = tab.total_duration_ms.map(|ms| format!("{ms} ms"));
    let result_count = (!tab.results.is_empty()).then(|| format!("{} statement(s)", tab.results.len()));

    let entries = [
        ("query", Some(tab.title.clone())),
        ("status", Some(status.to_owned())),
        ("cursor", Some(format!("Ln {line}, Col {col_num}"))),
        ("row_limit", Some(tab.row_limit_label().to_owned())),
        ("results", result_count),
        ("last_duration", last_duration),
    ];

    let mut details = col().key("inspector-query-overview").w_full().padding(12.0).gap(10.0);
    for (key, val) in entries {
        if !filter.is_empty() && !key.contains(&filter) {
            continue;
        }
        details = details.child(inspector_meta_kv(key, val.as_deref(), k));
    }
    details = details.child(Divider::horizontal().build(k)).child(
        text("Select any row in a tabular result below to inspect typed columns, JSONB payloads, and UTC/Local timestamps.")
            .font(k.ty.small)
            .color(c.text_faint)
            .w_full()
            .wrap(),
    );

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
