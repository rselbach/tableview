use std::borrow::Cow;

use rune::{Align, Color, Cx, El, ScrollSpec, TextStyle, Tx, col, el, icon, rich, row, spacer, text, text_input};
use rune_kit::shell::{TabItem, TabItemStatus, TabOrientation};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{
    Button, ButtonVariant, Cell, ControlSize, Divider, IconButton, IconButtonVariant, Kit, MenuItem, Placement,
    SegmentedControl, Select, SelectAppearance, SortDir, Spinner, TabStrip, Table, TableColumn, TextInput,
    context_menu, popover, scrim,
};
use tv_core::engine::encode_hex;
use tv_core::{
    CellValue, ColumnMeta, DbEngine, DbObjectKind, ExportFormat, SafetyPolicy, StagedCellValue, TablePage,
    TableStructure, build_fk_where_clause_for_engine, export_rows_for_engine,
};

use crate::icons;
use crate::platform;
use crate::state::{
    ActiveCellEdit, AppState, FkPeekState, RowKey, SessionState, TabContent, TableTabState, TableViewMode,
    parse_tab_id,
};
use crate::views::table_scroll::wrap_table_hscroll;
use crate::views::{empty_cta, format_bytes, pill_badge};

/// Builds the draggable horizontal tab strip across the top of the active session's main pane.
pub fn build_tab_bar(session: &SessionState, k: &Kit, cx: &Cx<AppState>) -> El {
    let c = &k.c;
    let sid = session.id;
    let strip_key = format!("session-tabs-{}", sid.get());
    let active_id_str = session.active_tab.map(|id| id.to_string());

    let mut entries: Vec<(String, String, &'static rune::Icon, Option<TabItemStatus>)> =
        Vec::with_capacity(session.tabs.len());
    for tab in &session.tabs {
        let base_title = tab.title();
        let duplicate_name = session
            .tabs
            .iter()
            .filter(|other| other.title() == base_title)
            .count()
            > 1;
        let display_title = if duplicate_name {
            match tab.schema_and_name() {
                Some((schema, name)) => format!("{schema}.{name}"),
                None => base_title.to_owned(),
            }
        } else {
            base_title.to_owned()
        };
        let tab_icon = match (&tab.content, tab.object_kind()) {
            (crate::state::TabContent::Query(_), _) => &icons::SQL_QUERY,
            (_, Some(DbObjectKind::View)) => &icons::VIEW_EYE,
            (_, Some(DbObjectKind::MaterializedView)) => &icons::MATERIALIZED_VIEW,
            _ => &icons::TABLE,
        };
        let dirty_title = match &tab.content {
            TabContent::Query(q) if q.is_dirty() => format!("{display_title} •"),
            TabContent::Table(t) if t.staged.has_changes() => {
                format!("{display_title} • {}", t.staged.change_count())
            }
            _ => display_title,
        };
        let status = if tab.is_loading() {
            Some(TabItemStatus::Busy)
        } else if tab.has_error() {
            Some(TabItemStatus::Failed)
        } else {
            None
        };
        entries.push((tab.id.to_string(), dirty_title, tab_icon, status));
    }

    let items: Vec<TabItem<'_, AppState>> = entries
        .iter()
        .map(|(id, title, tab_icon, status)| {
            TabItem::new(id.as_str(), title.as_str())
                .icon(tab_icon)
                .status(*status)
                .closable(true)
        })
        .collect();

    let new_query_btn = IconButton::new(&icons::PLUS)
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .label("New SQL query")
        .help("New SQL query")
        .shortcut("mod+t")
        .on_press_tx(|s: &mut AppState, tx: &mut Tx| s.open_new_query_tab(tx))
        .build(k)
        .key("tab-bar-new-query-btn");

    let strip = TabStrip::new(&strip_key, TabOrientation::Horizontal)
        .label("Open workspace tabs")
        .tabs(items)
        .active(active_id_str.as_deref())
        .viewport_width(cx.layout_size(&strip_key).map(|sz| sz[0]))
        .trailing_action(new_query_btn)
        .trailing_width(28.0)
        .drag_state(&session.tab_drag, move |s: &mut AppState| s.tab_drag_mut(sid))
        .fill(c.layer_01)
        .on_select(|s: &mut AppState, id, tx| {
            if let Some(tid) = parse_tab_id(id) {
                s.activate_tab(tid, tx);
            }
        })
        .on_close(|s: &mut AppState, id, tx| {
            if let Some(tid) = parse_tab_id(id) {
                s.close_tab(tid, tx);
            }
        })
        .on_reorder(|s: &mut AppState, reorder, _| {
            s.reorder_tabs(reorder.from, reorder.to);
        })
        .build(k);

    row()
        .key("workspace-tab-bar")
        .w_full()
        .h(36.0)
        .px(8.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(el().grow(1.0).min_w(0.0).child(strip))
}

/// Builds the active table/view tab body (`Data | Structure | Indexes`), optional staged banner, and bottom bar.
pub fn build_table_tab(
    s: &AppState,
    session: &SessionState,
    tab: &TableTabState,
    k: &Kit,
    cx: &Cx<AppState>,
    layer: &mut Vec<El>,
) -> El {
    let body = match tab.view_mode {
        TableViewMode::Data => build_data_view(s, session, tab, k, cx, layer),
        TableViewMode::Structure => build_structure_view(s, tab, k, cx),
        TableViewMode::Indexes => build_indexes_view(s, tab, k, cx),
    };
    let footer = build_table_footer(session, tab, k, layer);

    let mut root = col()
        .key("table-tab-root")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(body);

    if tab.view_mode == TableViewMode::Data && tab.staged.has_changes() {
        root = root.child(build_staged_banner(session, tab, k));
    }

    root.child(footer)
}

fn build_staged_banner(session: &SessionState, tab: &TableTabState, k: &Kit) -> El {
    let c = &k.c;
    let is_ro = session.profile.safety == SafetyPolicy::ReadOnly;
    let mut bar = row()
        .key("table-staged-banner")
        .w_full()
        .h(34.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .fill(c.warning.bg)
        .border_sides([1.0, 0.0, 0.0, 0.0], c.warning.border)
        .child(icon(&icons::DIFF_CHANGES, 13.0).color(c.warning.fg))
        .child(
            text(format!("{} staged row change(s)", tab.staged.change_count()))
                .font(k.ty.small_medium)
                .color(c.text_base),
        );

    if tab.staged.insert_count() > 0 {
        bar = bar.child(pill_badge(
            "staged-banner/ins",
            format!("+{}", tab.staged.insert_count()),
            c.success.bg,
            c.success.fg,
            c.success.border,
            k,
        ));
    }
    if tab.staged.update_count() > 0 {
        bar = bar.child(pill_badge(
            "staged-banner/upd",
            format!("~{}", tab.staged.update_count()),
            c.layer_02,
            c.warning.fg,
            c.warning.border,
            k,
        ));
    }
    if tab.staged.delete_count() > 0 {
        bar = bar.child(pill_badge(
            "staged-banner/del",
            format!("-{}", tab.staged.delete_count()),
            c.danger.bg,
            c.danger.fg,
            c.danger.border,
            k,
        ));
    }

    bar.child(spacer())
        .child(
            Button::new("Discard")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Mini)
                .disabled(tab.committing)
                .on_press(|s: &mut AppState| s.discard_staged_changes())
                .build(k)
                .key("staged-banner-discard-btn"),
        )
        .child(
            Button::new(if tab.diff_drawer_open { "Diff Open" } else { "Review & Commit" })
                .variant(ButtonVariant::Contrast)
                .control_size(ControlSize::Mini)
                .shortcut("mod+s")
                .disabled(is_ro || tab.committing)
                .on_press(|s: &mut AppState| s.open_staged_diff_drawer())
                .build(k)
                .key("staged-review-btn"),
        )
}

fn build_data_view(
    s: &AppState,
    session: &SessionState,
    tab: &TableTabState,
    k: &Kit,
    cx: &Cx<AppState>,
    layer: &mut Vec<El>,
) -> El {
    let c = &k.c;

    let filter_input = TextInput::new(&tab.filter_input)
        .label("SQL WHERE filter")
        .placeholder("SQL WHERE condition (e.g. gpa >= 3.5 AND enrolled = true)")
        .clear_button()
        .width(None)
        .on_edit(AppState::active_filter_input_mut)
        .on_submit(|s: &mut AppState, _| s.apply_active_tab_filter())
        .build(k)
        .key("table-filter-input");

    let mut filter_bar = row()
        .key("table-filter-bar")
        .w_full()
        .h(38.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .fill(c.bg_base)
        .border_sides([0.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(pill_badge(
            "filter/where-badge",
            "WHERE",
            c.layer_02,
            c.text_accent,
            c.border_muted,
            k,
        ))
        .child(el().grow(1.0).min_w(0.0).child(filter_input))
        .child(
            Button::new("Apply")
                .variant(ButtonVariant::Neutral)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut AppState| s.apply_active_tab_filter())
                .build(k)
                .key("filter-apply-btn"),
        );

    if !tab.filter_input.text.is_empty() || tab.applied_filter.is_some() {
        filter_bar = filter_bar.child(
            Button::new("Clear")
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut AppState| s.clear_active_tab_filter())
                .build(k)
                .key("filter-clear-btn"),
        );
    }

    if let Some(sort) = &tab.sort {
        let dir = if sort.ascending { "ASC" } else { "DESC" };
        filter_bar = filter_bar.child(
            row()
                .key("filter-sort-chip")
                .role("button")
                .label(format!("Clear sort {} {dir}", sort.column))
                .focusable(true)
                .h(24.0)
                .px(8.0)
                .gap(4.0)
                .center_y()
                .radius(k.radius.sm)
                .fill(c.layer_02)
                .border(0.5, c.border_base)
                .hover(c.hover)
                .on_press(|s: &mut AppState| s.clear_active_tab_sort())
                .child(
                    text(format!("ORDER BY {} {dir}", sort.column))
                        .font(k.ty.small)
                        .color(c.text_muted),
                )
                .child(icon(k.glyph.close, 11.0).color(c.icon_muted)),
        );
    }

    let mut stage = col()
        .key("data-view-root")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(filter_bar);

    if let Some(err) = &tab.filter_error {
        stage = stage.child(
            row()
                .key("table-filter-error")
                .w_full()
                .px(12.0)
                .py(6.0)
                .gap(8.0)
                .center_y()
                .fill(c.danger.bg)
                .border_sides([0.0, 0.0, 1.0, 0.0], c.danger.border)
                .child(text(err).font(k.ty.small).color(c.danger.fg).grow(1.0).wrap())
                .child(
                    IconButton::new(k.glyph.close)
                        .variant(IconButtonVariant::GhostMuted)
                        .control_size(ControlSize::Mini)
                        .label("Dismiss filter error")
                        .on_press(|s: &mut AppState| {
                            if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                                t.filter_error = None;
                            }
                        })
                        .build(k),
                ),
        );
    }

    let grid_area = if tab.page.is_none() && tab.page_loading {
        col()
            .key("data-loading")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .child(Spinner::new().size(18.0).color(c.text_accent).build(k))
            .child(
                text(format!("Loading {}.{}…", tab.schema, tab.object_name))
                    .font(k.ty.ui)
                    .color(c.text_muted),
            )
    } else if tab.page.is_none()
        && let Some(err) = &tab.page_error
    {
        col()
            .key("data-error")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .padding(24.0)
            .child(text("Query failed").font(k.ty.body_medium).color(c.text_base))
            .child(text(err).font(k.ty.ui_compact).color(c.danger.fg).wrap())
            .child(empty_cta("Retry", k, true, |s, _| s.reload_active_tab()))
    } else if let Some(page) = &tab.page {
        build_data_grid(s, session, tab, page, k, cx, layer)
    } else {
        col()
            .key("data-empty-init")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .child(text("Select Reload to load rows").font(k.ty.ui).color(c.text_muted))
    };

    stage.child(grid_area)
}

fn build_data_grid(
    s: &AppState,
    session: &SessionState,
    tab: &TableTabState,
    page: &TablePage,
    k: &Kit,
    cx: &Cx<AppState>,
    layer: &mut Vec<El>,
) -> El {
    let c = &k.c;
    if page.columns.is_empty() {
        return col()
            .key("data-no-columns")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .child(text("This relation defines zero columns.").font(k.ty.ui).color(c.text_muted));
    }

    let null_bg = c.layer_02;
    let null_fg = c.text_faint;
    let null_border = c.border_muted;
    let hover_bg = c.hover;
    let bool_true_fg = c.success.fg;
    let muted_fg = c.text_muted;
    let text_base = c.text_base;
    let text_accent = c.text_accent;
    let warn_bg = c.warning.bg;
    let warn_fg = c.warning.fg;
    let warn_border = c.warning.border;
    let success_bg = c.success.bg;
    let success_fg = c.success.fg;
    let danger_bg = c.danger.bg;
    let danger_fg = c.danger.fg;
    let ui_font = k.ty.ui_compact;
    let mono_font = k.ty.mono;
    let small_font = k.ty.small;
    let radius_xs = k.radius.xs;
    let anchor_rect = tab
        .editing_cell
        .as_ref()
        .and_then(|_| rune::memo::window_layout("inline-editor-anchor"));
    let editor_size = tab
        .editing_cell
        .as_ref()
        .and_then(|_| rune::memo::layout_size("inline-editor-wrap"));
    let win_size = tab
        .editing_cell
        .as_ref()
        .and_then(|_| rune::memo::layout_size("tv-app-base"));
    let grid_rect = tab
        .editing_cell
        .as_ref()
        .and_then(|_| rune::memo::layout("data-grid-clip"));
    let header_h = rune_kit::TableStyle::from_kit(k).header_height;
    let frozen_w = tab.data_table.column_width("#").unwrap_or(56.0);

    let mut table = Table::new(
        |s: &AppState| s.active_data_table(),
        |s: &mut AppState| s.active_data_table_mut(),
        |s: &AppState| {
            s.active_session()
                .and_then(SessionState::active_table_tab)
                .map_or(0, TableTabState::visual_row_count)
        },
        |s: &AppState, i| {
            let row_id = s
                .active_session()
                .and_then(SessionState::active_table_tab)
                .map_or_else(|| format!("r-{i}"), |t| t.row_id_string(i));
            Cow::Owned(row_id)
        },
    )
    .column(
        TableColumn::new("#", "#")
            .width(56.0)
            .min(44.0)
            .max(88.0)
            .align(Align::End)
            .resizable(false),
    );

    for col_meta in &page.columns {
        let extra = if col_meta.foreign_key.is_some() { 20.0 } else { 0.0 };
        let mut column = TableColumn::new(&col_meta.name, &col_meta.name)
            .width(initial_col_width(col_meta) + extra)
            .min(68.0)
            .max(640.0)
            .sortable()
            .resizable(true);
        if is_numeric_udt(&col_meta.udt_name) {
            column = column.align(Align::End);
        }
        table = table.column(column);
    }

    let data_hscroll = tab.data_hscroll.clone();
    let built = table
        .row(move |s: &AppState, visual_idx, r| {
            let r = data_hscroll.attach_row(r);
            let Some(tab) = s.active_session().and_then(SessionState::active_table_tab) else {
                return r;
            };
            let Some(row_key) = tab.visual_row_key(visual_idx) else {
                return r;
            };
            let row_id = tab.row_id_string(visual_idx);
            let is_sel = tab.data_table.is_selected(&row_id);
            let mut r = r.on_secondary_press(move |s: &mut AppState, press, _| {
                if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                    if !t.data_table.is_selected(&row_id) {
                        t.data_table.select(row_id.clone());
                        t.selected_row_key = Some(row_key);
                        t.selected_row = match row_key {
                            RowKey::Existing(idx) => Some(idx),
                            RowKey::Draft(_) => None,
                        };
                    }
                    t.row_menu = Some((row_key, press));
                    t.header_menu = None;
                }
            });
            if !is_sel {
                match row_key {
                    RowKey::Draft(_) => {
                        r = r.fill(success_bg);
                    }
                    RowKey::Existing(idx) if tab.staged.is_row_deleted(idx) => {
                        r = r.fill(danger_bg);
                    }
                    _ => {}
                }
            }
            r
        })
        .cell(move |s: &AppState, visual_idx, col_id| {
            let Some(tab) = s.active_session().and_then(SessionState::active_table_tab) else {
                return Cell::text("");
            };
            let Some(page) = &tab.page else {
                return Cell::text("");
            };
            let Some(row_key) = tab.visual_row_key(visual_idx) else {
                return Cell::text("");
            };
            if col_id == "#" {
                return match row_key {
                    RowKey::Draft(_) => Cell::text("+").mono().color(success_fg),
                    RowKey::Existing(idx) if tab.staged.is_row_deleted(idx) => {
                        Cell::text("−").mono().color(danger_fg)
                    }
                    RowKey::Existing(idx) if tab.staged.is_row_updated(idx) => {
                        Cell::text(format!("• {}", tab.offset + idx as i64 + 1))
                            .mono()
                            .color(warn_fg)
                    }
                    RowKey::Existing(idx) => Cell::text((tab.offset + idx as i64 + 1).to_string())
                        .mono()
                        .muted(),
                };
            }
            let Some(col_idx) = page.columns.iter().position(|c| c.name == col_id) else {
                return Cell::text("");
            };
            let col_meta = &page.columns[col_idx];
            let row_id_str = tab.row_id_string(visual_idx);

            if let Some(ref editing) = tab.editing_cell
                && editing.row_key == row_key
                && editing.col_idx == col_idx
            {
                let anchor = row()
                    .key("inline-editor-anchor")
                    .w_full()
                    .h(24.0)
                    .px(4.0)
                    .center_y()
                    .radius(radius_xs)
                    .fill(text_accent.alpha(0.10))
                    .border(1.0, text_accent.alpha(0.55));
                return Cell::el(anchor);
            }

            let is_draft_default = matches!(row_key, RowKey::Draft(id) if tab
                .staged
                .insert_row(id)
                .and_then(|d| d.values.get(col_idx))
                .is_some_and(|v| matches!(v, StagedCellValue::DefaultExpr)));
            let is_cell_edited =
                matches!(row_key, RowKey::Existing(idx) if tab.staged.effective_existing_cell(idx, col_idx).is_some());
            let val = tab.effective_cell(row_key, col_idx);

            let display_el = if is_draft_default {
                cell_pill_el("DEFAULT", null_bg, null_fg, null_border, small_font, radius_xs)
                    .key(format!("cell-default/{row_id_str}/{col_idx}"))
            } else {
                match &val {
                    CellValue::Null => cell_pill_el("NULL", null_bg, null_fg, null_border, small_font, radius_xs),
                    CellValue::Bool(b) => text(if *b { "true" } else { "false" })
                        .font(mono_font)
                        .color(if *b { bool_true_fg } else { muted_fg })
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                    v if v.is_mono() => text(v.display_compact())
                        .font(mono_font)
                        .color(text_base)
                        .align(if is_numeric_udt(&col_meta.udt_name) {
                            Align::End
                        } else {
                            Align::Start
                        })
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                    v => text(v.display_compact())
                        .font(ui_font)
                        .color(text_base)
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                }
            };

            let right_aligned = is_numeric_udt(&col_meta.udt_name);
            let mut inner = row().w_full().h(22.0).min_w(0.0).gap(4.0).center_y();
            if right_aligned {
                inner = inner.child(spacer()).child(display_el);
            } else {
                inner = inner.child(el().grow(1.0).min_w(0.0).child(display_el));
            }
            if is_cell_edited {
                inner = inner
                    .px(4.0)
                    .radius(radius_xs)
                    .fill(warn_bg)
                    .border(0.5, warn_border);
            }

            let fk_for_press = col_meta.foreign_key.clone();
            let val_for_press = val.clone();
            let col_for_press = col_meta.clone();

            inner = inner
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                    if platform::is_primary_mod(tx.mods())
                        && !val_for_press.is_null()
                        && let Some(ref fk) = fk_for_press
                    {
                        s.jump_to_foreign_key(fk.clone(), val_for_press.clone(), col_for_press.clone(), tx);
                    } else {
                        s.select_table_cell(row_key, col_idx);
                    }
                })
                .on_double_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                    s.begin_cell_edit(row_key, col_idx, tx);
                });

            if let Some(ref fk) = col_meta.foreign_key
                && !val.is_null()
            {
                let anchor_key = format!("fk-peek-{row_id_str}-{col_idx}");
                let anchor_for_click = anchor_key.clone();
                let fk_click = fk.clone();
                let val_click = val;
                let col_click = col_meta.clone();
                let fk_btn = row()
                    .key(anchor_key)
                    .role("button")
                    .label(format!("Inspect {} foreign key", fk.target_table))
                    .focusable(false)
                    .size(18.0, 18.0)
                    .center()
                    .radius(radius_xs)
                    .fill(null_bg)
                    .border(0.5, null_border)
                    .hover(hover_bg)
                    .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                        if platform::is_primary_mod(tx.mods()) {
                            s.jump_to_foreign_key(fk_click.clone(), val_click.clone(), col_click.clone(), tx);
                        } else {
                            s.open_fk_peek(
                                anchor_for_click.clone(),
                                fk_click.clone(),
                                val_click.clone(),
                                col_click.clone(),
                            );
                        }
                    })
                    .child(icon(&icons::ARROW_UP_RIGHT, 11.0).color(text_accent));
                inner = inner.child(fk_btn);
            }

            Cell::el(inner)
        })
        .zebra(true)
        .scroll_wide(true)
        .virtual_columns(true)
        .frozen_columns(1)
        .name(format!("{}.{} rows", tab.schema, tab.object_name))
        .on_sort(|s: &mut AppState, col_id, dir, _| {
            if col_id != "#" {
                s.sort_active_tab(col_id.to_owned(), dir == SortDir::Ascending);
            }
        })
        .on_select(|s: &mut AppState, row_id, _| s.select_active_tab_row(row_id))
        .on_reorder(|_, _, _, _| {})
        .on_header_secondary_press(|s: &mut AppState, col_id, press, _| {
            if col_id != "#"
                && let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut)
            {
                t.header_menu = Some((col_id.to_owned(), press));
                t.row_menu = None;
            }
        })
        .build(s, k)
        .w_full()
        .grow(1.0)
        .min_h(0.0);

    let grid_host = if tab.visual_row_count() == 0 {
        let filtered = tab.applied_filter.is_some();
        let mut msg = col()
            .key("data-empty-rows")
            .center()
            .gap(8.0)
            .pt(36.0)
            .child(
                text(if filtered {
                    "No rows match this WHERE filter"
                } else {
                    "This table has 0 rows"
                })
                .font(k.ty.ui)
                .color(c.text_muted),
            );
        if filtered {
            msg = msg.child(
                Button::new("Clear Filter")
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut AppState| s.clear_active_tab_filter())
                    .build(k),
            );
        }
        el().w_full()
            .grow(1.0)
            .min_h(0.0)
            .child(built.w_full().h_full())
            .child(
                el().pin(Some(0.0), Some(32.0), Some(0.0), Some(0.0))
                    .center()
                    .child(msg),
            )
    } else {
        built
    };
    let grid_host = wrap_table_hscroll(
        &format!("tv-data-{}", tab.tab_id.get()),
        &tab.data_hscroll,
        grid_host,
        tab.visual_row_count() > 0,
        cx,
        k,
    );

    let visible_anchor = anchor_rect.filter(|&[ax, ay, aw, ah]| {
        grid_rect.is_none_or(|[gx, gy, gw, gh]| {
            ax + aw > gx + frozen_w
                && ax < gx + gw
                && ay + ah > gy
                && ay < gy + gh
        })
    });
    let min_y = grid_rect.map_or(INLINE_EDITOR_WINDOW_MARGIN, |[_, gy, _, _]| {
        (gy + header_h).max(INLINE_EDITOR_WINDOW_MARGIN)
    });

    let mut grid_host = col()
        .key("data-grid-clip")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(grid_host);
    if tab.editing_cell.is_some() && visible_anchor.is_none() {
        grid_host = grid_host.on_any_press(|s: &mut AppState, _btn, _p, _| {
            s.cancel_inline_cell_edit();
        });
    }

    if let Some(ref editing) = tab.editing_cell
        && let Some(col_meta) = page.columns.get(editing.col_idx)
        && let Some(anchor) = visible_anchor
    {
        let bounds = InlineEditorBounds {
            anchor,
            min_y,
            editor_size,
            win_size,
        };
        mount_inline_cell_editor(editing, col_meta, bounds, k, layer);
    }

    if let Some(ref peek) = tab.fk_peek {
        mount_fk_peek_popover(session.profile.engine, peek, k, layer);
    }

    if let Some((col_id, press)) = &tab.header_menu {
        let sort_asc_col = col_id.clone();
        let sort_desc_col = col_id.clone();
        let hide_col = col_id.clone();
        let reset_col = col_id.clone();
        let all_cols: Vec<String> = page.columns.iter().map(|c| c.name.clone()).collect();
        let any_hidden = all_cols.iter().any(|name| tab.data_table.is_column_hidden(name));

        let mut menu = context_menu("table-col-ctx", grid_host, Some(*press))
            .item(MenuItem::new("Sort Ascending").on_select(move |s: &mut AppState| {
                s.sort_active_tab(sort_asc_col.clone(), true);
            }))
            .item(MenuItem::new("Sort Descending").on_select(move |s: &mut AppState| {
                s.sort_active_tab(sort_desc_col.clone(), false);
            }));

        if tab.sort.is_some() {
            menu = menu.item(MenuItem::new("Clear Sort").on_select(|s: &mut AppState| {
                s.clear_active_tab_sort();
            }));
        }

        menu = menu
            .separator()
            .item(MenuItem::new("Hide Column").on_select(move |s: &mut AppState| {
                if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                    t.data_table.hide_column(hide_col.clone());
                    t.header_menu = None;
                }
            }))
            .item(MenuItem::new("Reset Width").on_select(move |s: &mut AppState| {
                if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                    t.data_table.reset_column_width(&reset_col);
                    t.header_menu = None;
                }
            }));

        if any_hidden {
            menu = menu.item(MenuItem::new("Show All Hidden Columns").on_select(move |s: &mut AppState| {
                if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                    for name in &all_cols {
                        t.data_table.show_column(name);
                    }
                    t.header_menu = None;
                }
            }));
        }

        menu.on_close(|s: &mut AppState| {
            if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                t.header_menu = None;
            }
        })
        .build(k, layer)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
    } else if let Some((row_key, press)) = tab.row_menu {
        build_row_context_menu(session, tab, page, row_key, press, grid_host, k, layer)
    } else {
        grid_host
    }
}

#[allow(clippy::too_many_arguments)]
fn build_row_context_menu(
    session: &SessionState,
    tab: &TableTabState,
    page: &TablePage,
    row_key: RowKey,
    press: rune::PointerPress,
    grid_host: El,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    let editable = tab.can_edit_table(session.profile.safety).is_ok()
        && (matches!(row_key, RowKey::Draft(_)) || tab.has_primary_key());
    let target_col_idx = tab
        .selected_cell
        .filter(|(rk, _)| *rk == row_key)
        .map_or(0, |(_, ci)| ci)
        .min(page.columns.len().saturating_sub(1));

    let delete_label = match row_key {
        RowKey::Draft(_) => "Remove Draft Row",
        RowKey::Existing(idx) if tab.staged.is_row_deleted(idx) => "Undo Row Delete",
        RowKey::Existing(_) => "Delete Row",
    };

    let mut menu = context_menu("table-row-ctx", grid_host, Some(press))
        .item(
            MenuItem::new("Edit Cell")
                .disabled(!editable)
                .on_select_tx(move |s: &mut AppState, tx: &mut Tx| {
                    s.begin_cell_edit(row_key, target_col_idx, tx);
                }),
        );

    if page.columns.get(target_col_idx).is_some_and(|c| c.nullable) {
        menu = menu.item(
            MenuItem::new("Set Cell to NULL")
                .disabled(!editable)
                .on_select(move |s: &mut AppState| s.stage_cell_null(row_key, target_col_idx)),
        );
    }

    menu = menu
        .item(
            MenuItem::new("Insert New Row")
                .disabled(tab.can_edit_table(session.profile.safety).is_err())
                .on_select_tx(|s: &mut AppState, tx: &mut Tx| {
                    s.insert_staged_row(tx);
                }),
        )
        .item(
            MenuItem::new(delete_label)
                .disabled(!editable)
                .on_select(move |s: &mut AppState| s.toggle_delete_row(row_key)),
        );

    let eff_row: Vec<CellValue> = (0..page.columns.len())
        .map(|ci| tab.effective_cell(row_key, ci))
        .collect();
    let fk_links: Vec<_> = page
        .columns
        .iter()
        .enumerate()
        .filter_map(|(ci, col_meta)| {
            let fk = col_meta.foreign_key.as_ref()?;
            let val = eff_row.get(ci)?;
            if val.is_null() {
                return None;
            }
            Some((col_meta.clone(), fk.clone(), val.clone()))
        })
        .collect();
    if !fk_links.is_empty() {
        menu = menu.separator();
        for (col_meta, fk, val) in fk_links {
            let label = format!("Open FK → {}.{} ({})", fk.target_table, fk.target_column, val.display_compact());
            menu = menu.item(MenuItem::new(label).on_select_tx(move |s: &mut AppState, tx: &mut Tx| {
                s.jump_to_foreign_key(fk.clone(), val.clone(), col_meta.clone(), tx);
            }));
        }
    }

    let row_slice = vec![eff_row];
    let engine = session.profile.engine;
    let json_text = export_rows_for_engine(
        engine,
        ExportFormat::Json,
        Some(&tab.schema),
        &tab.object_name,
        &page.columns,
        &row_slice,
    );
    let insert_text = export_rows_for_engine(
        engine,
        ExportFormat::SqlInsert,
        Some(&tab.schema),
        &tab.object_name,
        &page.columns,
        &row_slice,
    );

    menu.separator()
        .item(MenuItem::new("Copy Row as JSON").on_select_tx(move |s: &mut AppState, tx: &mut Tx| {
            tx.copy(json_text.clone());
            s.set_notice("Copied row as JSON", false);
        }))
        .item(MenuItem::new("Copy Row as SQL INSERT").on_select_tx(move |s: &mut AppState, tx: &mut Tx| {
            tx.copy(insert_text.clone());
            s.set_notice("Copied row as SQL INSERT", false);
        }))
        .on_close(|s: &mut AppState| {
            if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                t.row_menu = None;
            }
        })
        .build(k, layer)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
}

fn mount_fk_peek_popover(engine: DbEngine, peek: &FkPeekState, k: &Kit, layer: &mut Vec<El>) {
    let c = &k.c;
    let fk_jump = peek.fk.clone();
    let val_jump = peek.value.clone();
    let col_jump = peek.source_col.clone();

    let mut card = col()
        .key("fk-peek-card")
        .role("dialog")
        .label(format!("Referenced {}.{} row", peek.fk.target_schema, peek.fk.target_table))
        .w(320.0)
        .padding(12.0)
        .gap(10.0)
        .child(
            row()
                .w_full()
                .gap(6.0)
                .center_y()
                .child(icon(&icons::ARROW_UP_RIGHT, 13.0).color(c.text_accent))
                .child(
                    text(format!("{}.{}", peek.fk.target_schema, peek.fk.target_table))
                        .font(k.ty.ui_medium)
                        .color(c.text_base)
                        .shrink()
                        .truncate(),
                )
                .child(spacer())
                .child(pill_badge(
                    "fk-peek/cond-badge",
                    format!("{} = {}", peek.fk.target_column, peek.value.display_compact()),
                    c.layer_02,
                    c.text_muted,
                    c.border_muted,
                    k,
                ))
                .child(
                    IconButton::new(k.glyph.close)
                        .variant(IconButtonVariant::GhostMuted)
                        .control_size(ControlSize::Mini)
                        .label("Close foreign key peek")
                        .on_press(|s: &mut AppState| s.close_fk_peek())
                        .build(k)
                        .key("fk-peek-close-btn"),
                ),
        )
        .child(Divider::horizontal().build(k));

    if peek.loading {
        card = card.child(
            row()
                .w_full()
                .py(18.0)
                .center()
                .gap(8.0)
                .child(Spinner::new().size(14.0).color(c.text_accent).build(k))
                .child(text("Fetching referenced row…").font(k.ty.ui_compact).color(c.text_muted)),
        );
    } else if let Some(err) = &peek.error {
        card = card.child(text(err).font(k.ty.ui_compact).color(c.danger.fg).w_full().wrap());
    } else if let Some(res) = &peek.result {
        if let Some(row_vals) = &res.row {
            let mut fields = col().w_full().gap(6.0);
            for (idx, (col_meta, cell)) in res.columns.iter().zip(row_vals.iter()).take(8).enumerate() {
                fields = fields.child(
                    row()
                        .key(format!("fk-peek-field-{idx}"))
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(
                            text(&col_meta.name)
                                .font(k.ty.small_medium)
                                .color(c.text_muted)
                                .w(104.0)
                                .truncate(),
                        )
                        .child(
                            text(cell.display_compact())
                                .font(k.ty.small.mono())
                                .color(if cell.is_null() { c.text_faint } else { c.text_base })
                                .grow(1.0)
                                .shrink()
                                .truncate(),
                        ),
                );
            }
            card = card.child(fields);
        } else {
            card = card.child(
                text("Referenced row was not found.")
                    .font(k.ty.ui_compact)
                    .color(c.text_muted),
            );
        }
    }

    if let Some(filter_sql) =
        build_fk_where_clause_for_engine(engine, &peek.fk.target_column, &peek.value, &peek.source_col)
    {
        let _ = filter_sql;
        card = card.child(
            row()
                .w_full()
                .pt(2.0)
                .gap(8.0)
                .child(spacer())
                .child(
                    Button::new("Open in Tab →")
                        .variant(ButtonVariant::Contrast)
                        .control_size(ControlSize::Small)
                        .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                            s.jump_to_foreign_key(fk_jump.clone(), val_jump.clone(), col_jump.clone(), tx);
                        })
                        .build(k)
                        .key("fk-peek-open-tab-btn"),
                ),
        );
    }

    layer.push(scrim("fk-peek-scrim", |s: &mut AppState| s.close_fk_peek()));
    let surface = popover::raised(k, card, k.radius.md);
    layer.push(popover::anchored(
        k,
        surface,
        &peek.anchor_key,
        Placement::BottomEnd,
        6.0,
    ));
}

const INLINE_EDITOR_WINDOW_MARGIN: f32 = 8.0;

fn inline_editor_layout(text: &str, nullable: bool, anchor_w: f32, win_w: f32) -> (f32, bool) {
    const CHAR_W: f32 = 8.0;
    const MIN_EDITOR_W: f32 = 160.0;
    const MAX_SINGLE_LINE_W: f32 = 460.0;
    const MULTILINE_MIN_W: f32 = 340.0;
    const MULTILINE_MAX_W: f32 = 520.0;

    let has_newline = text.contains('\n');
    let longest_line_chars = text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    let null_btn_reserve = if nullable { 52.0 } else { 0.0 };
    let single_line_needed_w = longest_line_chars as f32 * CHAR_W + 26.0 + null_btn_reserve;
    let max_allowed_w = (win_w - INLINE_EDITOR_WINDOW_MARGIN * 2.0).max(200.0);
    let single_cap = MAX_SINGLE_LINE_W.min(max_allowed_w);
    let is_multiline = has_newline || single_line_needed_w > single_cap;

    let box_w = if is_multiline {
        let multi_cap = MULTILINE_MAX_W.min(max_allowed_w);
        single_line_needed_w.max(anchor_w).max(MULTILINE_MIN_W).min(multi_cap)
    } else {
        single_line_needed_w.max(anchor_w).max(MIN_EDITOR_W).min(single_cap)
    };
    (box_w, is_multiline)
}

struct InlineEditorBounds {
    anchor: [f32; 4],
    min_y: f32,
    editor_size: Option<[f32; 2]>,
    win_size: Option<[f32; 2]>,
}

fn mount_inline_cell_editor(
    editing: &ActiveCellEdit,
    col_meta: &ColumnMeta,
    bounds: InlineEditorBounds,
    k: &Kit,
    layer: &mut Vec<El>,
) {
    let c = &k.c;
    let [ax, ay, aw, _ah] = bounds.anchor;
    let [win_w, win_h] = bounds.win_size.unwrap_or([1280.0, 800.0]);
    let min_y = bounds.min_y;
    let editor_size = bounds.editor_size;
    let (box_w, is_multiline) = inline_editor_layout(&editing.edit.text, col_meta.nullable, aw, win_w);

    let field = text_input(&editing.edit, k.ty.mono)
        .key("inline-cell-editor")
        .label(format!("Edit {}", col_meta.name))
        .w_full()
        .color(c.text_base)
        .caret(c.text_accent, c.text_accent.alpha(0.24))
        .on_edit(AppState::inline_cell_edit_mut)
        .on_submit(|s: &mut AppState, _| s.commit_inline_cell_edit());
    // Keep the slot path identical across single-line and multi-line states so typing past
    // the threshold never drops input focus.
    let field_slot = el()
        .key("inline-editor-field-slot")
        .grow(1.0)
        .min_w(0.0)
        .child(rune::auto_grow(field, 1, 8));

    let mut body = row()
        .key("inline-editor-body")
        .w_full()
        .px(7.0)
        .py(4.0)
        .gap(6.0)
        .child(field_slot);

    if col_meta.nullable {
        let row_key = editing.row_key;
        let col_idx = editing.col_idx;
        body = body.child(
            row()
                .key("inline-null-btn")
                .role("button")
                .label("Set NULL")
                .focusable(true)
                .mt(1.0)
                .h(16.0)
                .px(4.0)
                .center()
                .radius(k.radius.xs)
                .fill(c.layer_02)
                .border(0.5, c.border_muted)
                .hover(c.hover)
                .on_press(move |s: &mut AppState| s.stage_cell_null(row_key, col_idx))
                .child(text("NULL").font(k.ty.small).color(c.text_faint)),
        );
    }

    let mut card = col().w(box_w).child(body);
    if is_multiline {
        card = card.child(
            row()
                .key("inline-editor-footer")
                .w_full()
                .h(22.0)
                .px(8.0)
                .center_y()
                .border_sides([1.0, 0.0, 0.0, 0.0], c.border_muted)
                .child(
                    text("Enter to stage · Shift+Enter newline · Esc cancel")
                        .font(k.ty.small)
                        .color(c.text_faint)
                        .truncate()
                        .shrink(),
                ),
        );
    }

    let box_h_est = editor_size.map_or(if is_multiline { 68.0 } else { 28.0 }, |[_, h]| h);
    let x = ax.clamp(
        INLINE_EDITOR_WINDOW_MARGIN,
        (win_w - box_w - INLINE_EDITOR_WINDOW_MARGIN).max(INLINE_EDITOR_WINDOW_MARGIN),
    );
    let y = (ay - 2.0).clamp(
        min_y,
        (win_h - box_h_est - INLINE_EDITOR_WINDOW_MARGIN).max(min_y),
    );

    let card = k
        .elevate(
            card.radius(k.radius.sm)
                .fill(c.surface_raised)
                .border(1.5, c.text_accent),
            &k.elevation.md,
        )
        .key("inline-editor-wrap")
        .absolute(x, y)
        .preserve_focus_on_press(true)
        .on_key_preview(platform::handle_global_key)
        .on_any_press(move |s: &mut AppState, _btn, p, tx| {
            let h = editor_size.map_or(box_h_est, |[_, h]| h);
            if p.x < 0.0 || p.y < 0.0 || p.x > box_w || p.y > h {
                s.cancel_inline_cell_edit();
            } else {
                tx.live_only();
            }
        });
    layer.push(card);
}

fn build_structure_view(s: &AppState, tab: &TableTabState, k: &Kit, cx: &Cx<AppState>) -> El {
    let c = &k.c;
    if tab.structure.is_none() && tab.structure_loading {
        return col()
            .key("structure-loading")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .child(Spinner::new().size(18.0).color(c.text_accent).build(k))
            .child(text("Loading table structure…").font(k.ty.ui).color(c.text_muted));
    }

    if tab.structure.is_none()
        && let Some(err) = &tab.structure_error
    {
        return col()
            .key("structure-error")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .padding(24.0)
            .child(text("Could not load table structure").font(k.ty.body_medium).color(c.text_base))
            .child(text(err).font(k.ty.ui_compact).color(c.danger.fg).wrap())
            .child(empty_cta("Retry", k, true, |s, _| s.reload_active_tab()));
    }

    let Some(structure) = &tab.structure else {
        return col().w_full().grow(1.0).min_h(0.0);
    };

    let accent = c.text_accent;
    let warn_fg = c.warning.fg;
    let muted = c.text_muted;
    let faint = c.text_faint;

    let struct_hscroll = tab.structure_hscroll.clone();
    let columns_table = Table::new(
        |s: &AppState| s.active_structure_table(),
        |s: &mut AppState| s.active_structure_table_mut(),
        |s: &AppState| {
            s.active_session()
                .and_then(SessionState::active_table_tab)
                .and_then(|t| t.structure.as_ref())
                .map_or(0, |st| st.columns.len())
        },
        |_s: &AppState, i| Cow::Owned(format!("sc-{i}")),
    )
    .column(
        TableColumn::new("ord", "#")
            .width(40.0)
            .min(36.0)
            .max(60.0)
            .align(Align::End)
            .resizable(false),
    )
    .column(TableColumn::new("name", "Column").width(140.0).min(96.0).max(320.0))
    .column(TableColumn::new("type", "Type").width(136.0).min(100.0).max(300.0))
    .column(TableColumn::new("nullable", "Nullable").width(82.0).min(72.0).max(120.0))
    .column(TableColumn::new("default", "Default").width(148.0).min(96.0).max(320.0))
    .column(TableColumn::new("keys", "Constraints / FK").width(172.0).min(100.0).max(360.0))
    .column(TableColumn::new("comment", "Comment").flex(1.0).min(140.0))
    .row(move |_, _, r| struct_hscroll.attach_row(r))
    .cell(move |s: &AppState, row_idx, col_id| {
        let Some(col_meta) = s
            .active_session()
            .and_then(SessionState::active_table_tab)
            .and_then(|t| t.structure.as_ref())
            .and_then(|st| st.columns.get(row_idx))
        else {
            return Cell::text("");
        };
        match col_id {
            "ord" => Cell::text((row_idx + 1).to_string()).mono().muted(),
            "name" => Cell::text(&col_meta.name),
            "type" => Cell::text(&col_meta.data_type).mono().color(accent),
            "nullable" => Cell::text(if col_meta.nullable { "NULL" } else { "NOT NULL" })
                .mono()
                .color(if col_meta.nullable { faint } else { muted }),
            "default" => Cell::text(col_meta.default_expr.as_deref().unwrap_or("")).mono().muted(),
            "keys" => {
                let summary = format_constraints(col_meta);
                if col_meta.is_primary_key {
                    Cell::text(summary).mono().color(warn_fg)
                } else {
                    Cell::text(summary).mono().muted()
                }
            }
            _ => Cell::text(col_meta.comment.as_deref().unwrap_or("")).muted(),
        }
    })
    .zebra(true)
    .scroll_wide(true)
    .name("Table columns structure")
    .build(s, k)
    .w_full()
    .grow(1.0)
    .min_h(0.0);
    let columns_table = wrap_table_hscroll(
        &format!("tv-struct-{}", tab.tab_id.get()),
        &tab.structure_hscroll,
        columns_table,
        !structure.columns.is_empty(),
        cx,
        k,
    );

    let ddl_section = build_ddl_section(tab, structure, k);

    col()
        .key("structure-view-root")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(columns_table)
        .child(ddl_section)
}

fn build_ddl_section(tab: &TableTabState, structure: &TableStructure, k: &Kit) -> El {
    let c = &k.c;
    let ddl_for_copy = structure.stats.ddl.clone();

    let header = row()
        .key("structure-ddl-header")
        .w_full()
        .h(32.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([1.0, 0.0, 1.0, 0.0], c.border_muted)
        .child(
            row()
                .role("button")
                .label(if tab.ddl_expanded { "Collapse DDL" } else { "Expand DDL" })
                .focusable(true)
                .gap(6.0)
                .center_y()
                .px(4.0)
                .py(2.0)
                .radius(k.radius.xs)
                .hover(c.hover)
                .on_press(|s: &mut AppState| {
                    if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                        t.ddl_expanded = !t.ddl_expanded;
                    }
                })
                .child(
                    icon(
                        if tab.ddl_expanded {
                            k.glyph.triangle_down
                        } else {
                            k.glyph.triangle_right
                        },
                        12.0,
                    )
                    .color(c.icon_muted),
                )
                .child(icon(&icons::CODE_DDL, 13.0).color(c.icon_muted))
                .child(text("DDL").font(k.ty.small_medium).color(c.text_base)),
        )
        .child(spacer())
        .child(
            Button::new("Copy DDL")
                .variant(ButtonVariant::GhostMuted)
                .control_size(ControlSize::Mini)
                .icon(&icons::COPY)
                .on_press_tx(move |s: &mut AppState, tx: &mut Tx| {
                    tx.copy(ddl_for_copy.clone());
                    s.set_notice("Copied DDL to clipboard", false);
                })
                .build(k),
        );

    let mut section = col().key("structure-ddl-section").w_full().child(header);
    if tab.ddl_expanded {
        let syntax_theme = SyntaxTheme::from_kit(k);
        let highlights = syntax::highlight(Some("sql"), &structure.stats.ddl);
        let spans = syntax_theme.spans(&highlights, 0);
        let scroll_body = k
            .scroll_bar(
                col()
                    .key("structure-ddl-scroll")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .padding(12.0)
                    .scroll(ScrollSpec { stick_bottom: false })
                    .child(
                        rich(&structure.stats.ddl, spans)
                            .font(k.ty.mono)
                            .color(c.text_base)
                            .w_full()
                            .wrap(),
                    ),
            )
            .w_full()
            .grow(1.0)
            .min_h(0.0);
        section = section.child(
            col()
                .key("structure-ddl-body")
                .w_full()
                .h(168.0)
                .min_h(0.0)
                .fill(c.bg_base)
                .child(scroll_body),
        );
    }
    section
}

fn build_indexes_view(s: &AppState, tab: &TableTabState, k: &Kit, cx: &Cx<AppState>) -> El {
    let c = &k.c;
    if tab.structure.is_none() && tab.structure_loading {
        return col()
            .key("indexes-loading")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(10.0)
            .child(Spinner::new().size(18.0).color(c.text_accent).build(k))
            .child(text("Loading indexes…").font(k.ty.ui).color(c.text_muted));
    }

    let Some(structure) = &tab.structure else {
        if let Some(err) = &tab.structure_error {
            return col()
                .key("indexes-error")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .center()
                .gap(10.0)
                .padding(24.0)
                .child(text("Could not load indexes").font(k.ty.body_medium).color(c.text_base))
                .child(text(err).font(k.ty.ui_compact).color(c.danger.fg).wrap())
                .child(empty_cta("Retry", k, true, |s, _| s.reload_active_tab()));
        }
        return col().w_full().grow(1.0).min_h(0.0);
    };

    if structure.indexes.is_empty() {
        return col()
            .key("indexes-empty")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .center()
            .gap(8.0)
            .child(icon(&icons::INDEX_BOLT, 28.0).color(c.icon_faint))
            .child(
                text(format!("No indexes on {}.{}", tab.schema, tab.object_name))
                    .font(k.ty.ui)
                    .color(c.text_muted),
            );
    }

    let warn_fg = c.warning.fg;
    let accent = c.text_accent;
    let muted = c.text_muted;
    let idx_hscroll = tab.indexes_hscroll.clone();

    let built = Table::new(
        |s: &AppState| s.active_indexes_table(),
        |s: &mut AppState| s.active_indexes_table_mut(),
        |s: &AppState| {
            s.active_session()
                .and_then(SessionState::active_table_tab)
                .and_then(|t| t.structure.as_ref())
                .map_or(0, |st| st.indexes.len())
        },
        |_s: &AppState, i| Cow::Owned(format!("ix-{i}")),
    )
    .column(TableColumn::new("name", "Index Name").width(172.0).min(110.0).max(340.0))
    .column(TableColumn::new("flags", "Flags").width(90.0).min(76.0).max(140.0))
    .column(TableColumn::new("method", "Method").width(74.0).min(60.0).max(110.0))
    .column(TableColumn::new("columns", "Columns").width(132.0).min(90.0).max(300.0))
    .column(
        TableColumn::new("size", "Size")
            .width(76.0)
            .min(64.0)
            .max(120.0)
            .align(Align::End),
    )
    .column(TableColumn::new("definition", "Definition").flex(1.0).min(300.0))
    .row(move |_, _, r| idx_hscroll.attach_row(r))
    .cell(move |s: &AppState, row_idx, col_id| {
        let Some(idx) = s
            .active_session()
            .and_then(SessionState::active_table_tab)
            .and_then(|t| t.structure.as_ref())
            .and_then(|st| st.indexes.get(row_idx))
        else {
            return Cell::text("");
        };
        match col_id {
            "name" => Cell::text(&idx.name),
            "flags" => {
                if idx.is_primary {
                    Cell::text("PRIMARY").mono().color(warn_fg)
                } else if idx.is_unique {
                    Cell::text("UNIQUE").mono().color(accent)
                } else {
                    Cell::text("INDEX").mono().color(muted)
                }
            }
            "method" => Cell::text(&idx.method).mono().muted(),
            "columns" => Cell::text(idx.columns.join(", ")).mono(),
            "size" => Cell::text(idx.size_bytes.map_or_else(|| "—".to_owned(), format_bytes))
                .mono()
                .muted(),
            _ => Cell::text(&idx.definition).mono().muted(),
        }
    })
    .zebra(true)
    .scroll_wide(true)
    .name("Table indexes")
    .build(s, k)
    .w_full()
    .grow(1.0)
    .min_h(0.0);

    wrap_table_hscroll(
        &format!("tv-idx-{}", tab.tab_id.get()),
        &tab.indexes_hscroll,
        built,
        !structure.indexes.is_empty(),
        cx,
        k,
    )
}

fn build_table_footer(session: &SessionState, tab: &TableTabState, k: &Kit, layer: &mut Vec<El>) -> El {
    let c = &k.c;

    let view_switch = SegmentedControl::new(&["Data", "Structure", "Indexes"], Some(tab.view_mode.index()))
        .label("Table sub-view")
        .as_tabs()
        .fit()
        .on_pick(|s: &mut AppState, idx, _| {
            s.set_active_tab_view_mode(TableViewMode::from_index(idx));
        })
        .build(k)
        .key("table-view-switch");

    let readout = match tab.view_mode {
        TableViewMode::Data => format_page_readout(tab),
        TableViewMode::Structure => tab
            .structure
            .as_ref()
            .map_or_else(|| "Loading columns…".to_owned(), |st| format!("{} columns", st.columns.len())),
        TableViewMode::Indexes => tab
            .structure
            .as_ref()
            .map_or_else(|| "Loading indexes…".to_owned(), |st| format!("{} indexes", st.indexes.len())),
    };

    let mut bar = row()
        .key("table-footer")
        .w_full()
        .h(38.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([1.0, 0.0, 0.0, 0.0], c.border_muted)
        .child(view_switch);

    if tab.view_mode == TableViewMode::Data {
        let can_edit = tab.can_edit_table(session.profile.safety).is_ok() && tab.columns().is_some();
        let can_delete = can_edit
            && tab.active_row_key().is_some_and(|rk| match rk {
                RowKey::Draft(_) => true,
                RowKey::Existing(_) => tab.has_primary_key(),
            });
        let can_export = tab.page.as_ref().is_some_and(|p| !p.columns.is_empty());

        bar = bar
            .child(
                Button::new("Row")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Mini)
                    .icon(&icons::PLUS)
                    .disabled(!can_edit)
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                        s.insert_staged_row(tx);
                    })
                    .build(k)
                    .key("table-insert-row-btn"),
            )
            .child(
                IconButton::new(&icons::TRASH)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Mini)
                    .label("Delete selected row")
                    .help("Toggle delete on selected row")
                    .disabled(!can_delete)
                    .on_press(|s: &mut AppState| s.toggle_delete_selected_row())
                    .build(k)
                    .key("table-delete-row-btn"),
            )
            .child(
                IconButton::new(&icons::DOWNLOAD)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Mini)
                    .label("Export data")
                    .help("Export table page (⌘E)")
                    .shortcut("mod+e")
                    .disabled(!can_export)
                    .on_press_tx(|s: &mut AppState, tx: &mut Tx| {
                        s.open_export_modal_for_active_tab(tx);
                    })
                    .build(k)
                    .key("table-export-btn"),
            );
    }

    bar = bar.child(spacer());

    if (tab.view_mode == TableViewMode::Data && tab.page_loading)
        || (tab.view_mode != TableViewMode::Data && tab.structure_loading)
    {
        bar = bar.child(Spinner::new().size(12.0).color(c.icon_muted).build(k));
    }

    bar = bar.child(text(readout).font(k.ty.small).color(c.text_muted));

    if tab.view_mode == TableViewMode::Data {
        let size_options = ["100 / page", "300 / page", "1000 / page"];
        let current_size = match tab.page_size {
            100 => "100 / page",
            1000 => "1000 / page",
            _ => "300 / page",
        };
        let page_select = Select::new("table-page-size", &size_options)
            .label("Rows per page")
            .appearance(SelectAppearance::Inline)
            .placement(Placement::TopEnd)
            .current(Some(current_size))
            .open(tab.page_size_open)
            .on_toggle(|s: &mut AppState| {
                if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                    t.page_size_open = !t.page_size_open;
                }
            })
            .on_close(|s: &mut AppState| {
                if let Some(t) = s.active_session_mut().and_then(SessionState::active_table_tab_mut) {
                    t.page_size_open = false;
                }
            })
            .on_choose(|s: &mut AppState, val, _| {
                let next = if val.starts_with("1000") {
                    1000
                } else if val.starts_with("100") {
                    100
                } else {
                    300
                };
                s.set_active_tab_page_size(next);
            })
            .build(k, layer);

        let can_prev = tab.offset > 0 && !tab.page_loading;
        let can_next = tab.page.as_ref().is_some_and(|p| p.has_more) && !tab.page_loading;

        bar = bar
            .child(page_select)
            .child(
                IconButton::new(&icons::CHEVRON_LEFT)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .label("Previous page")
                    .help("Previous page")
                    .disabled(!can_prev)
                    .on_press(|s: &mut AppState| s.paginate_active_tab(false))
                    .build(k),
            )
            .child(
                IconButton::new(&icons::CHEVRON_RIGHT)
                    .variant(IconButtonVariant::GhostMuted)
                    .control_size(ControlSize::Small)
                    .label("Next page")
                    .help("Next page")
                    .disabled(!can_next)
                    .on_press(|s: &mut AppState| s.paginate_active_tab(true))
                    .build(k),
            );
    }

    bar.child(
        IconButton::new(&icons::REFRESH)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label("Reload tab")
            .help("Reload tab")
            .shortcut("mod+alt+r")
            .on_press(|s: &mut AppState| s.reload_active_tab())
            .build(k),
    )
}

fn format_page_readout(tab: &TableTabState) -> String {
    let Some(page) = &tab.page else {
        return if tab.page_loading {
            "Running query…".to_owned()
        } else {
            "—".to_owned()
        };
    };
    if page.rows.is_empty() {
        return format!("0 rows · {} ms", page.elapsed_ms);
    }
    let start = page.offset + 1;
    let end = page.offset + page.rows.len() as i64;
    let total_part = match (page.total_rows_estimate, page.total_is_exact) {
        (Some(total), true) => format!("{total} rows"),
        (Some(est), false) => format!("~{est} rows"),
        (None, _) => format!("{end}+ rows"),
    };
    format!("{start}–{end} of {total_part} · {} ms", page.elapsed_ms)
}

fn format_constraints(col: &ColumnMeta) -> String {
    let mut parts = Vec::new();
    if col.is_primary_key {
        parts.push("PK".to_owned());
    }
    if col.is_unique {
        parts.push("UNIQUE".to_owned());
    }
    if let Some(fk) = &col.foreign_key {
        parts.push(format!(
            "FK → {}.{}.{}",
            fk.target_schema, fk.target_table, fk.target_column
        ));
    }
    parts.join(" · ")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_cell_value(
    val: &CellValue,
    null_bg: Color,
    null_fg: Color,
    null_border: Color,
    bool_true_fg: Color,
    muted_fg: Color,
    small_font: TextStyle,
    radius_xs: f32,
) -> Cell {
    match val {
        CellValue::Null => Cell::el(cell_pill_el("NULL", null_bg, null_fg, null_border, small_font, radius_xs)),
        CellValue::Bool(b) => Cell::text(if *b { "true" } else { "false" })
            .mono()
            .color(if *b { bool_true_fg } else { muted_fg }),
        v if v.is_mono() => Cell::text(v.display_compact()).mono(),
        v => Cell::text(v.display_compact()),
    }
}

pub(crate) fn is_numeric_udt(udt: &str) -> bool {
    matches!(
        udt,
        "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "money"
    )
}

pub(crate) fn initial_col_width(col: &ColumnMeta) -> f32 {
    let base: f32 = match col.udt_name.as_str() {
        "uuid" => 248.0,
        "timestamptz" | "timestamp" => 214.0,
        "json" | "jsonb" => 240.0,
        "bool" => 84.0,
        "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "money" => 112.0,
        "date" | "time" | "timetz" => 126.0,
        "bytea" => 172.0,
        _ => 182.0,
    };
    let header_fit = (col.name.len() as f32 * 8.5 + 42.0).clamp(88.0, 280.0);
    base.max(header_fit)
}

fn cell_pill_el(label: &'static str, bg: Color, fg: Color, border: Color, font: TextStyle, radius: f32) -> El {
    row()
        .h(18.0)
        .px(6.0)
        .center()
        .radius(radius)
        .fill(bg)
        .border(0.5, border)
        .child(text(label).font(font).color(fg))
}

/// Formats a byte preview helper cleanly for external views (used in inspector as well).
pub fn format_bytes_preview(bytes: &[u8], max_bytes: usize) -> String {
    let shown = bytes.len().min(max_bytes);
    let hex = encode_hex(&bytes[..shown]);
    if bytes.len() > shown {
        format!("\\x{hex}… ({} bytes)", bytes.len())
    } else {
        format!("\\x{hex} ({} bytes)", bytes.len())
    }
}
