use std::borrow::Cow;

use rune::{Align, Color, Cx, El, ScrollSpec, TextStyle, Tx, col, el, icon, rich, row, spacer, text};
use rune_kit::shell::{TabItem, TabItemStatus, TabOrientation};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{
    Button, ButtonVariant, Cell, ControlSize, IconButton, IconButtonVariant, Kit, MenuItem, Placement,
    SegmentedControl, Select, SelectAppearance, SortDir, Spinner, TabStrip, Table, TableColumn, TextInput,
    context_menu,
};
use tv_core::engine::encode_hex;
use tv_core::{CellValue, ColumnMeta, DbObjectKind, TablePage, TableStructure};

use crate::icons;
use crate::state::{
    AppState, SessionState, TableTabState, TableViewMode, parse_tab_id,
};
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
        let tab_icon = match tab.object_kind() {
            Some(DbObjectKind::View) => &icons::VIEW_EYE,
            Some(DbObjectKind::MaterializedView) => &icons::MATERIALIZED_VIEW,
            _ => &icons::TABLE,
        };
        let status = if tab.is_loading() {
            Some(TabItemStatus::Busy)
        } else if tab.has_error() {
            Some(TabItemStatus::Failed)
        } else {
            None
        };
        entries.push((tab.id.to_string(), display_title, tab_icon, status));
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

    let strip = TabStrip::new(&strip_key, TabOrientation::Horizontal)
        .label("Open workspace tabs")
        .tabs(items)
        .active(active_id_str.as_deref())
        .viewport_width(cx.layout_size(&strip_key).map(|sz| sz[0]))
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

/// Builds the active table/view tab body (`Data | Structure | Indexes`) and its bottom bar.
pub fn build_table_tab(
    s: &AppState,
    session: &SessionState,
    tab: &TableTabState,
    k: &Kit,
    layer: &mut Vec<El>,
) -> El {
    let body = match tab.view_mode {
        TableViewMode::Data => build_data_view(s, tab, k, layer),
        TableViewMode::Structure => build_structure_view(s, tab, k),
        TableViewMode::Indexes => build_indexes_view(s, tab, k),
    };
    let footer = build_table_footer(session, tab, k, layer);

    col()
        .key("table-tab-root")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(body)
        .child(footer)
}

fn build_data_view(s: &AppState, tab: &TableTabState, k: &Kit, layer: &mut Vec<El>) -> El {
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
        build_data_grid(s, tab, page, k, layer)
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
    tab: &TableTabState,
    page: &TablePage,
    k: &Kit,
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
    let bool_true_fg = c.success.fg;
    let muted_fg = c.text_muted;
    let small_font = k.ty.small;
    let radius_xs = k.radius.xs;

    let mut table = Table::new(
        |s: &AppState| s.active_data_table(),
        |s: &mut AppState| s.active_data_table_mut(),
        |s: &AppState| {
            s.active_session()
                .and_then(SessionState::active_table_tab)
                .and_then(|t| t.page.as_ref())
                .map_or(0, |p| p.rows.len())
        },
        |_s: &AppState, i| Cow::Owned(format!("r-{i}")),
    )
    .column(
        TableColumn::new("#", "#")
            .width(52.0)
            .min(44.0)
            .max(84.0)
            .align(Align::End)
            .resizable(false),
    );

    for col_meta in &page.columns {
        let mut column = TableColumn::new(&col_meta.name, &col_meta.name)
            .width(initial_col_width(col_meta))
            .min(68.0)
            .max(640.0)
            .sortable()
            .resizable(true);
        if is_numeric_udt(&col_meta.udt_name) {
            column = column.align(Align::End);
        }
        table = table.column(column);
    }

    let built = table
        .cell(move |s: &AppState, row_idx, col_id| {
            let Some(tab) = s.active_session().and_then(SessionState::active_table_tab) else {
                return Cell::text("");
            };
            let Some(page) = &tab.page else {
                return Cell::text("");
            };
            if col_id == "#" {
                return Cell::text((tab.offset + row_idx as i64 + 1).to_string())
                    .mono()
                    .muted();
            }
            let Some(col_idx) = page.columns.iter().position(|c| c.name == col_id) else {
                return Cell::text("");
            };
            let Some(val) = page.rows.get(row_idx).and_then(|r| r.get(col_idx)) else {
                return Cell::text("");
            };
            match val {
                CellValue::Null => Cell::el(null_pill_el(null_bg, null_fg, null_border, small_font, radius_xs)),
                CellValue::Bool(b) => Cell::text(if *b { "true" } else { "false" })
                    .mono()
                    .color(if *b { bool_true_fg } else { muted_fg }),
                v if v.is_mono() => Cell::text(v.display_compact()).mono(),
                v => Cell::text(v.display_compact()),
            }
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
            }
        })
        .build(s, k)
        .w_full()
        .grow(1.0)
        .min_h(0.0);

    let grid_host = if page.rows.is_empty() {
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
    } else {
        grid_host
    }
}

fn build_structure_view(s: &AppState, tab: &TableTabState, k: &Kit) -> El {
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

fn build_indexes_view(s: &AppState, tab: &TableTabState, k: &Kit) -> El {
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

    Table::new(
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
    .min_h(0.0)
}

fn build_table_footer(_session: &SessionState, tab: &TableTabState, k: &Kit, layer: &mut Vec<El>) -> El {
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
        .gap(10.0)
        .center_y()
        .fill(c.layer_01)
        .border_sides([1.0, 0.0, 0.0, 0.0], c.border_muted)
        .child(view_switch)
        .child(spacer());

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

fn is_numeric_udt(udt: &str) -> bool {
    matches!(
        udt,
        "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "money"
    )
}

fn initial_col_width(col: &ColumnMeta) -> f32 {
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

fn null_pill_el(bg: Color, fg: Color, border: Color, font: TextStyle, radius: f32) -> El {
    row()
        .h(18.0)
        .px(6.0)
        .center()
        .radius(radius)
        .fill(bg)
        .border(0.5, border)
        .child(text("NULL").font(font).color(fg))
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
